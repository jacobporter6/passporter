//! The reply surface, pinned above the dock (design §6.2, §7): a composer
//! for prompts, option chips for a dialog the bridge understood, and a
//! keypad for any dialog, with the agent's outbox (design §6.5) shown
//! inline. Drafts and actions are in [`super::reply`], the chip rules in
//! [`super::choices`], delivery in [`crate::outbox`]. The dictation strip
//! comes from [`crate::voice`], and the dock's mic dictates to the agent
//! while the composer is up ([`crate::ui::MicFab`]): what was
//! dictated is sent as a prompt when the user stops, after anything typed
//! in the composer, through the outbox like a typed message (see
//! [`Sender::dictated`]). With "Review dictation before sending" on, the
//! reviewed transcript fills the composer instead. A tap on a suggested
//! reply sends it the same way, on its own ([`Sender::suggested`]).
//!
//! Safety rules enforced here:
//!
//! - keys and interrupts carry the seq of the screen the user is looking
//!   at, and the keypad is disabled until that screen is as new as the
//!   agent (so it never answers a dialog the user hasn't seen);
//! - chips name the pending input they show, and are disabled unless it is
//!   the latest for the agent's current seq; elevated options are never
//!   sent from here (they need the desktop until step-up exists);
//! - every decision goes through the outbox: stored, then retried with the
//!   same key until the bridge answers. Prompts queue behind each other;
//!   the keypad keys and chips wait while a key or choice for the agent is
//!   unanswered. Interrupt never waits;
//! - a short cooldown after a key or choice is sent swallows accidental
//!   double taps, and an answered dialog stays disabled until the next;
//! - dictation and suggested replies are never sent while the agent waits
//!   at a dialog (the bridge would refuse them): they go into the composer;
//!   only one suggestion per question is sent, so a double tap sends once;
//! - a note typed with a permission dialog's "No" is queued as a prompt
//!   only after the "No" was sent and the agent is back at its prompt
//!   ([`FollowUp`]); if the "No" does not go through, or the agent does not
//!   get there in time, the note is put in the composer instead.

use core::time::Duration;

use leptos::prelude::*;
use protocol::{
    Action, ActionKind, AgentId, AgentStatus, PendingId, PendingInput, PendingOption, PromptText,
    Seq,
};

use super::Panel;
use super::choices::{self, Chip, Gate, KeyDialog};
use super::pending::PendingFeed;
use super::reply::{Dictated, Draft, FollowStep, FollowUp, Pad, build_action, keypad};
use super::suggest::{self, Tap};
use crate::attach;
use crate::attach::state::{Gate as SendGate, Photo};
use crate::attach::view::{Attach, AttachButton};
use crate::idgen::random_key;
use crate::live::{Live, use_live};
use crate::outbox::{Delivery, Entry, OutboxService, OutboxTray, use_outbox};
use crate::platform::haptic_tick;
use crate::runbooks::use_runbooks;
use crate::store::OutboxKey;
use crate::ui::format::now;
use crate::ui::icons::{self, Icon};
use crate::ui::{AppState, use_app, use_dictation_target};
use crate::voice::phase::append_transcript;
use crate::voice::view::{Dictation, DictationStrip};

/// Taps on the keypad or chips within this long after a key or choice was
/// sent are ignored.
const KEY_COOLDOWN_MS: f64 = 400.0;
/// How long a note waits, after its "No" was sent, for the agent to get
/// back to its prompt.
const NOTE_WAIT: Duration = Duration::from_secs(15);
/// Why dictated or suggested words were put in the composer, not sent.
const HELD_AT_DIALOG: &str =
    "The agent is waiting at a dialog, so your words weren't sent. They're in the message box.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Compose,
    Keypad,
    Choices,
}

/// The mode that fits the agent: chips for a dialog with options, keys for
/// any other dialog (herdr refuses prompts while an agent is blocked),
/// words otherwise.
const fn fitting_mode(blocked: bool, has_choices: bool) -> Mode {
    if has_choices {
        Mode::Choices
    } else if blocked {
        Mode::Keypad
    } else {
        Mode::Compose
    }
}

/// Turns drafts for one agent into outbox entries.
#[derive(Debug, Clone)]
struct Sender {
    app: AppState,
    outbox: OutboxService,
    id: AgentId,
    /// Seq for prompts: the newest known.
    prompt_seq: Signal<Seq>,
    /// Seq for keys and interrupts: the displayed screen's, if it and the
    /// shown dialog (when blocked) are current.
    key_seq: Signal<Option<Seq>>,
    /// The dialog keys answer.
    key_dialog: Signal<KeyDialog>,
    /// Seq for choices: the shown pending input's, if current.
    choice_seq: Signal<Option<Seq>>,
    /// Whether a key or choice for the agent is unanswered.
    answering: Memo<bool>,
    pending: PendingFeed,
    text: RwSignal<String>,
    /// The agent's outbox entries, shared by the tray and the reactions.
    entries: Memo<Vec<(OutboxKey, Entry)>>,
    cooldown_until: StoredValue<f64>,
    /// A deny note waiting to follow its "No".
    note: RwSignal<Option<FollowUp>>,
    /// Saved drafts, for notes given back once this bar is gone.
    live: Live,
    /// The composer's photo.
    attach: Attach,
}

impl Sender {
    /// Whether the keypad and chips must wait (re-checked on tap: the
    /// disabled attribute can lag).
    fn busy(&self) -> bool {
        self.answering.get_untracked()
    }

    /// The user decided `draft`: a new decision with a fresh idempotency
    /// key and the right current seq for its kind, into the outbox. The
    /// action, if the outbox took it as new.
    fn submit(&self, draft: &Draft) -> Option<Action> {
        self.submit_with(draft, None)
    }

    /// [`Self::submit`], a prompt carrying `photo` (once it is on the
    /// bridge); the photo leaves the composer with the text.
    fn submit_with(&self, draft: &Draft, photo: Option<&Photo>) -> Option<Action> {
        // Interrupts never wait: they are the emergency control.
        let answers = matches!(draft, Draft::Keys(_) | Draft::Choose { .. });
        if answers && self.busy() {
            return None;
        }
        let seq = match draft {
            Draft::Prompt(_) => self.prompt_seq.get_untracked(),
            // The keypad and chips are disabled meanwhile; never guess a seq.
            Draft::Keys(_) | Draft::Interrupt => self.key_seq.get_untracked()?,
            Draft::Choose { .. } => self.choice_seq.get_untracked()?,
        };
        let dialog = match draft {
            Draft::Keys(_) => {
                let dialog = self.key_dialog.get_untracked();
                dialog.pending_id()?.cloned()
            }
            Draft::Prompt(_) | Draft::Interrupt | Draft::Choose { .. } => None,
        };
        let Some(key) = random_key() else {
            self.app
                .toast("This browser can't generate secure random ids.");
            return None;
        };
        let action = attach::state::attach(
            build_action(self.id.clone(), seq, draft, dialog.as_ref(), key, now()),
            photo,
        );
        let label = match draft {
            Draft::Choose { label, .. } => Some(label.clone()),
            Draft::Prompt(_) | Draft::Keys(_) | Draft::Interrupt => None,
        };
        haptic_tick();
        // The composer keeps the text until the prompt is stored; a second
        // tap meanwhile is the same message, which the outbox turns away.
        let sent = match draft {
            Draft::Prompt(text) => Some(text.as_str().to_owned()),
            _ => None,
        };
        let sender = self.clone();
        let decided = action.clone();
        let photo = photo.map(|photo| photo.id.clone());
        let queued = Box::new(move || {
            if let Some(photo) = &photo {
                sender.attach.sent(photo);
            }
            // Safe in the outbox now. Only clear what was sent, not what
            // was typed since.
            let text = sender.text;
            if let Some(sent) = sent
                && text.try_with_untracked(|t| t.trim_end() == sent) == Some(true)
            {
                let _ = text.try_set(String::new());
            }
            // A waiting note belongs to its own "No" only: any other
            // decision gives it back (after the clear above).
            let other = sender
                .note
                .try_with_untracked(|n| n.as_ref().is_some_and(|n| !n.follows(&decided)));
            if other == Some(true)
                && let Some(Some(note)) = sender.note.try_get_untracked()
            {
                let _ = sender.note.try_set(None);
                sender.give_back(note.note());
            }
        });
        self.outbox
            .submit(action.clone(), label, queued)
            .then_some(action)
    }

    /// The user chose "No" (`draft`) with an optional `note` to queue as a
    /// prompt once it is through.
    fn submit_with_note(&self, draft: &Draft, note: Option<PromptText>) {
        match (self.submit(draft), note) {
            (Some(action), Some(note)) => {
                if let Some(old) = self.note.get_untracked() {
                    self.give_back(old.note());
                }
                self.note.set(Some(FollowUp::new(&action, note)));
            }
            // Not taken (busy, stale chips): the note goes back now.
            (None, Some(note)) => self.give_back(&note),
            (_, None) => {}
        }
    }

    /// Gives a note that will not be sent back to the user, in the
    /// composer, or in the agent's saved draft once the reply bar is gone
    /// (a timer after a swipe), so it is never lost.
    fn give_back(&self, note: &PromptText) {
        let given = self
            .text
            .try_update(|text| *text = append_transcript(text, note.as_str()));
        if given.is_none() {
            let draft = self.live.draft(&self.id);
            self.live
                .set_draft(&self.id, append_transcript(&draft, note.as_str()));
        }
    }

    /// The waiting note's "No" is now at `delivery` (`None`: gone before
    /// it went): gives the note back if the "No" will not go through, and
    /// once it was sent, gives the agent [`NOTE_WAIT`] to get back to its
    /// prompt.
    fn follow_delivery(&self, delivery: Option<&Delivery>) {
        let generation = self.pending.generation.get_untracked();
        let Some(step) = self
            .note
            .try_update(|n| n.as_mut().map(|n| n.on_delivery(delivery, generation)))
            .flatten()
        else {
            return;
        };
        match step {
            FollowStep::Abandon(note) => {
                self.note.set(None);
                self.give_back(&note);
            }
            FollowStep::Wait if delivery == Some(&Delivery::Sent) => {
                let sender = self.clone();
                let waiting = self.note.get_untracked();
                set_timeout(
                    move || {
                        if sender.note.try_get_untracked().flatten() == waiting
                            && let Some(waiting) = waiting
                        {
                            let _ = sender.note.try_set(None);
                            sender.give_back(waiting.note());
                            sender.app.toast(
                                "“No” was sent, but the agent didn't get back to its prompt in \
                                 time, so your note wasn't sent. It's in the message box.",
                            );
                        }
                    },
                    NOTE_WAIT,
                );
            }
            FollowStep::Wait | FollowStep::Send(_) => {}
        }
    }

    /// Follows the agent's pending input and status for this component's
    /// lifetime (one effect for both jobs):
    ///
    /// - once the agent shows another dialog (or none), the one answered
    ///   from here is over: it is forgotten, so an id a restarted bridge
    ///   reuses is not mistaken for answered;
    /// - a waiting note is queued once the agent (`status`, at the newest
    ///   seq) is back at its prompt after its "No". Once queued it is an
    ///   outbox entry like any prompt: if it doesn't go through, its card
    ///   keeps the text.
    fn follow_pending(&self, status: Signal<Option<AgentStatus>>) {
        let sender = self.clone();
        Effect::new(move |_| {
            let seq = sender.prompt_seq.get();
            let status = status.get();
            let step = sender.pending.read.with(|read| {
                if !read.failed {
                    let shown = read.pending.as_ref().map(|p| &p.id);
                    sender.outbox.dialog_shown(&sender.id, shown);
                }
                sender
                    .note
                    .with(|n| n.as_ref().map(|n| n.poll(status, seq, read)))
            });
            if let Some(FollowStep::Send(draft @ Draft::Prompt(_))) = step {
                let note = sender.note.get_untracked();
                sender.note.set(None);
                if sender.submit(&draft).is_none()
                    && let Some(note) = note
                {
                    sender.give_back(note.note());
                }
            }
        });
    }

    /// `said` was dictated ([`Dictated`]): sent with the composer's draft,
    /// or kept in the composer with a toast saying why not. False if this
    /// bar is gone (the user left meanwhile), so the dictation keeps the
    /// text for their return.
    fn dictated(&self, said: &str, send: bool, at_dialog: bool, mode: RwSignal<Mode>) -> bool {
        let Some(dictated) = self
            .text
            .try_with_untracked(|t| Dictated::of(t, said, send, at_dialog))
        else {
            return false;
        };
        self.text.set(dictated.text().to_owned());
        match dictated {
            Dictated::Fill(_) => mode.set(Mode::Compose),
            Dictated::Send(_, prompt) => {
                self.submit(&Draft::Prompt(prompt));
            }
            Dictated::AtDialog(_) => self.app.toast(HELD_AT_DIALOG),
            Dictated::Invalid(_, why) => self.app.toast(why),
        }
        true
    }

    /// A suggested reply was tapped ([`Tap`]): sent as a prompt on its
    /// own, leaving any draft in the composer, or put in the composer with
    /// a toast while the agent waits at a dialog. Whether it was sent
    /// (which spends the shown set).
    fn suggested(&self, suggestion: &PromptText, at_dialog: bool, spent: bool) -> bool {
        let tap = self
            .text
            .with_untracked(|t| Tap::of(t, suggestion, at_dialog, spent));
        match tap {
            Tap::Send(prompt) => self.submit(&Draft::Prompt(prompt)).is_some(),
            Tap::AtDialog(text) => {
                self.text.set(text);
                self.app.toast(HELD_AT_DIALOG);
                false
            }
            Tap::Spent => false,
        }
    }

    /// When the reply bar goes (a swipe to another agent), a note still
    /// waiting for its "No" is not sent: it goes into the agent's saved
    /// draft, so it is in the composer on return.
    fn give_back_on_leave(&self) {
        let (id, note, live) = (self.id.clone(), self.note, self.live);
        on_cleanup(move || {
            if let Some(Some(waiting)) = note.try_get_untracked() {
                let _ = note.try_set(None);
                live.set_draft(
                    &id,
                    append_transcript(&live.draft(&id), waiting.note().as_str()),
                );
            }
        });
    }

    /// Reacts to one of the agent's entries getting its answer.
    fn on_settled(&self, entry: &Entry) {
        // Swallow double taps right after a key or choice went through.
        // (A delivered choice's dialog is recorded by the outbox.)
        if entry.delivery == Delivery::Sent && !matches!(entry.action.kind, ActionKind::Prompt(_)) {
            self.cooldown_until
                .set_value(js_sys::Date::now() + KEY_COOLDOWN_MS);
        }
        if matches!(entry.delivery, Delivery::Stale { .. }) {
            haptic_tick();
        }
        // Whatever happened to a choice, the dialog it answered may be
        // gone: read it again (chips wait meanwhile). Likewise for keys
        // that came back stale or refused.
        let dialog_gone = matches!(
            entry.delivery,
            Delivery::Stale { .. } | Delivery::Refused { .. }
        );
        match &entry.action.kind {
            ActionKind::Choose(_) => self.pending.refresh(),
            ActionKind::Keys(_) if dialog_gone => self.pending.refresh(),
            _ => {}
        }
    }
}

/// "Edit" on a stale prompt: its text into the composer. Never loses a
/// draft in progress: the stale text goes after it.
fn edit_into(text: RwSignal<String>, mode: RwSignal<Mode>) -> Callback<String> {
    Callback::new(move |body: String| {
        text.update(|t| {
            *t = if t.trim().is_empty() {
                body
            } else {
                format!("{}\n\n{body}", t.trim_end())
            };
        });
        mode.set(Mode::Compose);
    })
}

/// One agent's view of the outbox: whether a key or choice is unanswered,
/// and its entries.
fn agent_outbox(
    outbox: &OutboxService,
    id: &AgentId,
) -> (Memo<bool>, Memo<Vec<(OutboxKey, Entry)>>) {
    let (outbox, a, e) = (*outbox, id.clone(), id.clone());
    (
        Memo::new(move |_| outbox.answers_pending(&a)),
        Memo::new(move |_| outbox.entries(&e)),
    )
}

/// The reply mode, following the agent (a dialog wants chips or keys,
/// anything else wants words) but never under the user's thumb or while
/// they type; the keypad brings the screen up.
fn reply_mode(
    app: AppState,
    blocked: Memo<bool>,
    has_choices: Memo<bool>,
    panel: RwSignal<Panel>,
    pointer_down: StoredValue<bool>,
) -> RwSignal<Mode> {
    let mode = RwSignal::new(fitting_mode(
        blocked.get_untracked(),
        has_choices.get_untracked(),
    ));
    Effect::new(move |previous: Option<Mode>| {
        let fitting = fitting_mode(blocked.get(), has_choices.get());
        let interacting = app.composing.get_untracked() || pointer_down.get_value();
        if previous.is_some_and(|was| was != fitting) && !interacting {
            mode.set(fitting);
        }
        fitting
    });
    // Keys answer the screen, so the keypad brings the screen up.
    Effect::new(move |_| {
        if mode.get() == Mode::Keypad {
            panel.set(Panel::Screen);
        }
    });
    mode
}

/// Reacts to answers that arrive while this agent is shown: cooldowns and
/// re-reads. Only entries seen waiting here count, so ones settled earlier,
/// elsewhere, or restored from the store never fire.
fn watch_settled(sender: Sender) {
    let shared = sender.entries;
    // A Vec: a handful of keys, and a set would cost more WASM.
    Effect::new(move |waiting: Option<Vec<OutboxKey>>| {
        let entries = shared.get();
        let waiting = waiting.unwrap_or_default();
        for (_, entry) in entries
            .iter()
            .filter(|(k, e)| !e.delivery.is_unsettled() && waiting.contains(k))
        {
            sender.on_settled(entry);
        }
        // A deny note follows its "No" (after any re-read started above).
        let followed = sender.note.with_untracked(|n| {
            n.as_ref().map(|n| {
                let found = entries.iter().find(|(_, e)| n.follows(&e.action));
                let seen = waiting.iter().any(|k| k.idempotency_key == *n.key());
                (found.map(|(_, e)| e.delivery.clone()), seen)
            })
        });
        match followed {
            Some((Some(delivery), _)) => sender.follow_delivery(Some(&delivery)),
            // Gone after it was seen waiting here (cancelled, or another
            // tab took it): not sent from here.
            Some((None, true)) => sender.follow_delivery(None),
            Some((None, false)) | None => {}
        }
        entries
            .into_iter()
            .filter(|(_, e)| e.delivery.is_unsettled())
            .map(|(k, _)| k)
            .collect()
    });
}

/// The reply surface.
#[component]
pub fn ReplyBar(
    id: AgentId,
    status: Signal<Option<AgentStatus>>,
    /// Newest known seq (prompts).
    seq: Signal<Seq>,
    /// The displayed screen's seq while it is current (keys).
    key_seq: Signal<Option<Seq>>,
    /// The agent's listed seq (what a shown dialog must be current for).
    listed_seq: Signal<Option<Seq>>,
    /// The agent's pending input.
    pending: PendingFeed,
    panel: RwSignal<Panel>,
    title: Signal<String>,
) -> impl IntoView {
    let app = use_app();
    let live = use_live();
    let outbox = use_outbox();
    let blocked = Memo::new(move |_| status.get() == Some(AgentStatus::Blocked));
    let has_choices = Memo::new(move |_| {
        pending
            .read
            .with(|r| !choices::chips(r.pending.as_ref()).is_empty())
    });
    let answered = {
        let id = id.clone();
        Memo::new(move |_| outbox.answered(&id))
    };
    let gate = Memo::new(move |_| {
        let generation = pending.generation.get();
        let listed = listed_seq.get();
        answered.with(|answered| {
            pending
                .read
                .with(|r| choices::gate(r, listed, generation, answered.as_ref()))
        })
    });
    let choice_seq = Signal::derive(move || match gate.get() {
        Gate::Ready(seq) => Some(seq),
        Gate::Updating | Gate::Answered => None,
    });
    // While blocked, keys answer the shown dialog: they name it, and wait
    // while it is being re-read.
    let key_dialog = Signal::derive(move || {
        let gate = gate.get();
        pending
            .read
            .with(|r| choices::key_dialog(blocked.get(), gate, r))
    });
    let key_seq = Signal::derive(move || {
        key_seq
            .get()
            .filter(|_| key_dialog.with(|d| d.pending_id().is_some()))
    });
    let pointer_down = StoredValue::new(false);
    let mode = reply_mode(app, blocked, has_choices, panel, pointer_down);
    // The draft survives swipes between agents.
    let text = RwSignal::new(live.draft(&id));
    {
        let id = id.clone();
        Effect::new(move |_| live.set_draft(&id, text.get()));
    }
    let (answering, entries) = agent_outbox(&outbox, &id);
    let suggestions = suggest::suggestions_feed(app, id.clone(), status, listed_seq).into();
    let sender = Sender {
        app,
        outbox,
        id,
        prompt_seq: seq,
        key_seq,
        key_dialog,
        choice_seq,
        answering,
        pending,
        text,
        entries,
        cooldown_until: StoredValue::new(0.0),
        note: RwSignal::new(None),
        live,
        attach: Attach::new(app),
    };
    let at_dialog = Signal::derive(move || {
        blocked.get()
            || pending
                .read
                .with(|r| r.pending.as_ref().is_some_and(PendingInput::is_dialog))
    });
    let dictation = dictation(&sender, mode, at_dialog);
    sender.follow_pending(status);
    sender.give_back_on_leave();
    watch_settled(sender.clone());

    view! {
        <div
            class="reply"
            class:reply-blocked=move || blocked.get()
            on:pointerdown=move |_| pointer_down.set_value(true)
            on:pointerup=move |_| pointer_down.set_value(false)
            on:pointercancel=move |_| pointer_down.set_value(false)
        >
            <OutboxTray entries=entries seq=seq on_edit=edit_into(text, mode) />
            <DictationStrip dictation=dictation.clone() />
            <div class="attach-slot" node_ref=sender.attach.slot></div>
            <Show when=move || blocked.get() && mode.get() != Mode::Choices>
                <p class="reply-hint">
                    <Icon paths=icons::HAND />
                    "Waiting at a dialog — answer with keys"
                </p>
            </Show>
            {
                let sender = sender.clone();
                move || match mode.get() {
                    Mode::Compose => view! { <Composer sender=sender.clone() mode=mode title=title suggestions=suggestions at_dialog=at_dialog /> }.into_any(),
                    Mode::Keypad => view! { <Keypad sender=sender.clone() mode=mode panel=panel has_choices=has_choices /> }.into_any(),
                    Mode::Choices => view! { <Choices sender=sender.clone() mode=mode gate=gate /> }.into_any(),
                }
            }
        </div>
    }
}

/// The agent's dictation, picked up where it was left (a clip uploading or
/// transcribed), and what its text does ([`Sender::dictated`]). A
/// recording stops (and is kept) with the screen.
fn dictation(sender: &Sender, mode: RwSignal<Mode>, at_dialog: Signal<bool>) -> Dictation {
    let dictation = Dictation::for_agent(sender.app, &sender.id);
    let taker = sender.clone();
    // Words that name a runbook exactly offer it (behind a confirm)
    // instead of reaching the agent; anything else is dictated text.
    let runbooks = use_runbooks();
    runbooks.ensure_loaded(sender.app);
    // Words offered as a runbook but not run land in the composer.
    let text = sender.text;
    let give_back = Callback::new(move |said: String| {
        text.update(|t| *t = append_transcript(t, &said));
        mode.set(Mode::Compose);
    });
    dictation.on_take(Callback::new(move |(said, send): (String, bool)| {
        if runbooks.offer_spoken(&said, give_back) {
            return true;
        }
        let at_dialog = at_dialog.try_get_untracked().unwrap_or(true);
        taker.dictated(&said, send, at_dialog, mode)
    }));
    dictation.resume();
    {
        let dictation = dictation.clone();
        on_cleanup(move || dictation.leave());
    }
    dictation
}

#[component]
fn Composer(
    sender: Sender,
    mode: RwSignal<Mode>,
    title: Signal<String>,
    suggestions: Signal<Vec<PromptText>>,
    at_dialog: Signal<bool>,
) -> impl IntoView {
    let app = sender.app;
    let text = sender.text;
    let attach = sender.attach;
    let invalid = RwSignal::new(None::<String>);
    let gate = move || text.with(|t| attach.photo.with(|p| attach::state::gate(t, p.as_ref())));
    let submit = {
        let sender = sender.clone();
        move || {
            if gate() != SendGate::Ready {
                return;
            }
            let photo = attach.photo.get_untracked();
            match attach::state::prompt_text(&text.get_untracked(), photo.is_some()) {
                Ok(prompt) => {
                    invalid.set(None);
                    sender.submit_with(&Draft::Prompt(prompt), photo.as_ref());
                }
                Err(e) => invalid.set(Some(format!("Can't send this: {e}"))),
            }
        }
    };
    let on_key = {
        let submit = submit.clone();
        move |ev: leptos::ev::KeyboardEvent| {
            // Enter adds a line on the phone; Ctrl/⌘+Enter sends on a keyboard.
            if ev.key() == "Enter" && (ev.ctrl_key() || ev.meta_key()) {
                ev.prevent_default();
                submit();
            }
        }
    };
    let placeholder = move || {
        let title = title.get();
        if title.is_empty() {
            "Message the agent".to_owned()
        } else {
            format!("Message {title}")
        }
    };
    // While typing, the dock hides so the composer stays above the keyboard.
    on_cleanup(move || {
        let _ = app.composing.try_set(false);
    });
    // While this composer is up, the dock's mic dictates to its agent.
    use_dictation_target().set(sender.id.clone());
    view! {
        <Suggestions sender=sender.clone() suggestions=suggestions at_dialog=at_dialog />
        <div class="composer">
            <button type="button" class="icon-button" aria-label="Show keypad" on:click=move |_| mode.set(Mode::Keypad)>
                <Icon paths=icons::KEYPAD />
            </button>
            <AttachButton attach=attach />
            <label class="visually-hidden" for="reply-text">"Message"</label>
            <textarea
                id="reply-text"
                class="composer-input"
                rows="1"
                maxlength="16000"
                enterkeyhint="enter"
                placeholder=placeholder
                aria-invalid=move || invalid.get().is_some().to_string()
                aria-describedby="reply-error"
                prop:value=move || text.get()
                on:input=move |ev| {
                    text.set(event_target_value(&ev));
                    invalid.set(None);
                }
                on:focus=move |_| app.composing.set(true)
                on:blur=move |_| app.composing.set(false)
                on:keydown=on_key
            ></textarea>
            <button
                type="button"
                class="send-button"
                aria-label="Send"
                class:send-waiting=move || gate() == SendGate::Waiting
                disabled=move || gate() != SendGate::Ready
                on:click=move |_| submit()
            >
                <Icon paths=icons::SEND />
            </button>
        </div>
        <p id="reply-error" class="field-error composer-error" role="alert">{move || invalid.get()}</p>
    }
}

/// Suggested replies as chips: a tap sends one at once
/// ([`Sender::suggested`]); it then shows as sent, and the others wait for
/// the next set (the agent's next question).
#[component]
fn Suggestions(
    sender: Sender,
    suggestions: Signal<Vec<PromptText>>,
    at_dialog: Signal<bool>,
) -> impl IntoView {
    move || {
        let shown = suggestions.get();
        // The chip sent from this set, if any; a new set starts afresh.
        let sent = RwSignal::new(None::<usize>);
        (!shown.is_empty()).then(|| {
            view! {
                <div class="chips suggestions" role="group" aria-label="Suggested replies">
                    {shown.into_iter().enumerate().map(|(at, suggestion)| {
                        let label = suggestion.as_str().to_owned();
                        let sender = sender.clone();
                        view! {
                            <button
                                type="button"
                                class="chip chip-suggestion"
                                class:chip-sent=move || sent.get() == Some(at)
                                disabled=move || sent.with(Option::is_some)
                                on:click=move |_| {
                                    // Re-checked here: the disabled attribute can lag a tap.
                                    let spent = sent.with_untracked(Option::is_some);
                                    let at_dialog = at_dialog.get_untracked();
                                    if sender.suggested(&suggestion, at_dialog, spent) {
                                        sent.set(Some(at));
                                    }
                                }
                            >
                                <span class="chip-label">{label}</span>
                            </button>
                        }
                    }).collect_view()}
                </div>
            }
        })
    }
}

#[component]
fn Keypad(
    sender: Sender,
    mode: RwSignal<Mode>,
    panel: RwSignal<Panel>,
    has_choices: Memo<bool>,
) -> impl IntoView {
    let answering = sender.answering;
    let key_seq = sender.key_seq;
    let confirm = RwSignal::new(false);
    let on_screen = move || panel.get() == Panel::Screen;
    let ready = move || on_screen() && key_seq.with(Option::is_some);
    // Keys wait for an unanswered key or choice; the interrupt never does.
    let busy = move |pad: Pad| !ready() || (matches!(pad, Pad::Key(_)) && answering.get());
    let cooldown = sender.cooldown_until;
    let rows = keypad();
    view! {
        <Show when=move || has_choices.get()>
            <button type="button" class="button button-secondary button-small keypad-back" on:click=move |_| mode.set(Mode::Choices)>
                "Back to options"
            </button>
        </Show>
        {move || {
            if !on_screen() {
                view! {
                    <p class="keypad-hint">
                        "Keys answer what the screen shows."
                        <button type="button" class="button button-secondary button-small" on:click=move |_| panel.set(Panel::Screen)>"Show screen"</button>
                    </p>
                }.into_any()
            } else if key_seq.with(Option::is_none) {
                view! { <p class="keypad-hint keypad-hint-quiet" role="status">"Updating the screen…"</p> }.into_any()
            } else {
                ().into_any()
            }
        }}
        <div class="keypad" role="group" aria-label="Keys">
            {rows.into_iter().flatten().map(|pad| {
                let sender = sender.clone();
                let on_click = move |_| {
                    if pad == Pad::Compose {
                        mode.set(Mode::Compose);
                        return;
                    }
                    // Re-checked here: the disabled attribute can lag a tap.
                    if busy(pad) || js_sys::Date::now() < cooldown.get_value() {
                        return;
                    }
                    match pad {
                        Pad::Key(key) => {
                            sender.submit(&Draft::key(key));
                        }
                        Pad::Interrupt => confirm.set(true),
                        Pad::Compose => {}
                    }
                };
                let class = match pad {
                    Pad::Key(protocol::AllowedKey::Digit(_)) => "key key-digit",
                    Pad::Key(protocol::AllowedKey::Esc | protocol::AllowedKey::Tab) => "key key-nav key-word",
                    Pad::Key(_) => "key key-nav",
                    Pad::Interrupt => "key key-danger",
                    Pad::Compose => "key key-mode",
                };
                let is_digit = matches!(pad, Pad::Key(protocol::AllowedKey::Digit(_)));
                let aria = if is_digit { None } else { Some(pad.aria()) };
                let gate = pad != Pad::Compose;
                view! {
                    <button
                        type="button"
                        class=class
                        aria-label=aria
                        disabled=move || gate && busy(pad)
                        on:click=on_click
                    >
                        {if pad == Pad::Compose {
                            view! { <Icon paths=icons::KEYBOARD /> }.into_any()
                        } else {
                            pad.label().into_any()
                        }}
                    </button>
                }
            }).collect_view()}
        </div>
        <Show when=move || confirm.get()>
            <div class="confirm" role="alertdialog" aria-label="Interrupt the agent?">
                <p class="confirm-text">"Interrupt the agent? It stops what it's doing."</p>
                <div class="confirm-actions">
                    <button type="button" class="button button-secondary" on:click=move |_| confirm.set(false)>"Cancel"</button>
                    {
                        let sender = sender.clone();
                        view! {
                            <button type="button" class="button button-danger" on:click=move |_| {
                                confirm.set(false);
                                sender.submit(&Draft::Interrupt);
                            }>"Interrupt"</button>
                        }
                    }
                </div>
            </div>
        </Show>
    }
}

/// The dialog's prompt, its detail, and its options as chips.
#[component]
fn Choices(sender: Sender, mode: RwSignal<Mode>, gate: Memo<Gate>) -> impl IntoView {
    let read = sender.pending.read;
    let shown = Memo::new(move |_| read.with(|r| r.pending.clone()));
    // An elevated chip tapped (its explanation shows) and the option whose
    // reason is being typed; both reset when the dialog changes.
    let explain = RwSignal::new(None::<String>);
    // Tied to the dialog it was opened for: a new dialog never inherits it.
    let feedback_for = RwSignal::new(None::<(PendingId, PendingOption)>);
    Effect::new(move |previous: Option<Option<PendingId>>| {
        let id = shown.with(|p| p.as_ref().map(|p| p.id.clone()));
        if previous.is_some_and(|was| was != id) {
            explain.set(None);
            feedback_for.set(None);
        }
        id
    });
    move || {
        let Some(pending) = shown.get() else {
            return ().into_any();
        };
        let sender = sender.clone();
        view! {
            <div class="choices">
                <p class="choices-prompt" id="choices-prompt">
                    <Icon paths=icons::HAND />
                    <span>{if pending.prompt.trim().is_empty() { "The agent is asking:".to_owned() } else { pending.prompt.clone() }}</span>
                </p>
                {pending.detail.clone().filter(|d| !d.trim().is_empty()).map(|detail| view! {
                    <details class="choices-detail">
                        <summary class="choices-detail-summary">"Details"</summary>
                        <pre class="code choices-detail-body" tabindex="0">{detail}</pre>
                    </details>
                })}
                {choices::from_screen(&pending).then(|| view! {
                    <p class="choices-note">"Read from the terminal: check the screen if unsure."</p>
                })}
                {move || match feedback_for.get().filter(|(id, _)| *id == pending.id) {
                    Some((_, option)) => view! {
                        <Feedback sender=sender.clone() pending=pending.clone() option=option feedback_for=feedback_for gate=gate />
                    }.into_any(),
                    None => view! {
                        <Chips sender=sender.clone() pending=pending.clone() gate=gate explain=explain feedback_for=feedback_for />
                    }.into_any(),
                }}
                <p class="choices-status" role="status">
                    {move || match gate.get() {
                        Gate::Ready(_) => "",
                        Gate::Updating => "Checking the dialog…",
                        Gate::Answered => "Answered — waiting for the agent.",
                    }}
                </p>
                <div class="choices-foot">
                    <button type="button" class="button button-secondary button-small" on:click=move |_| mode.set(Mode::Keypad)>
                        <Icon paths=icons::KEYPAD />
                        "Use keys"
                    </button>
                </div>
            </div>
        }
        .into_any()
    }
}

/// Whether a chip tap may send now (re-checked on tap: the disabled
/// attribute can lag).
fn chips_ready(sender: &Sender, gate: Memo<Gate>) -> bool {
    matches!(gate.get_untracked(), Gate::Ready(_))
        && !sender.busy()
        && js_sys::Date::now() >= sender.cooldown_until.get_value()
}

#[component]
fn Chips(
    sender: Sender,
    pending: PendingInput,
    gate: Memo<Gate>,
    explain: RwSignal<Option<String>>,
    feedback_for: RwSignal<Option<(PendingId, PendingOption)>>,
) -> impl IntoView {
    let answering = sender.answering;
    let enabled = move || matches!(gate.get(), Gate::Ready(_)) && !answering.get();
    let options = choices::chips(Some(&pending)).to_vec();
    view! {
        <div class="chips" role="group" aria-labelledby="choices-prompt">
            {options.into_iter().map(|option| {
                let kind = choices::chip(&pending, &option);
                let label = option.label.clone();
                match kind {
                    Chip::Desktop => {
                        let id = option.id.to_string();
                        let shown_for = id.clone();
                        view! {
                            <button
                                type="button"
                                class="chip chip-elevated"
                                aria-disabled="true"
                                aria-describedby="choices-why"
                                on:click=move |_| explain.set(Some(id.clone()))
                            >
                                <Icon paths=icons::LOCK />
                                <span class="chip-label">{label}</span>
                                <span class="chip-hint">"Desktop"</span>
                            </button>
                            <Show when={
                                let shown_for = shown_for.clone();
                                move || explain.with(|e| e.as_deref() == Some(shown_for.as_str()))
                            }>
                                <p class="chips-why" id="choices-why" role="status">{choices::DESKTOP_REASON}</p>
                            </Show>
                        }.into_any()
                    }
                    Chip::Feedback | Chip::Note => view! {
                        <button
                            type="button"
                            class="chip chip-feedback"
                            disabled=move || !enabled()
                            on:click={
                                let id = pending.id.clone();
                                move |_| feedback_for.set(Some((id.clone(), option.clone())))
                            }
                        >
                            <span class="chip-label">{label}</span>
                        </button>
                    }.into_any(),
                    Chip::Choose => {
                        let sender = sender.clone();
                        let pending = pending.clone();
                        view! {
                            <button
                                type="button"
                                class="chip"
                                disabled=move || !enabled()
                                on:click=move |_| {
                                    if !chips_ready(&sender, gate) {
                                        return;
                                    }
                                    // Low risk, from the shown input: always Ok.
                                    if let Ok(draft) = choices::choose(&pending, &option.id, None) {
                                        sender.submit(&draft);
                                    }
                                }
                            >
                                <span class="chip-label">{label}</span>
                            </button>
                        }.into_any()
                    }
                }
            }).collect_view()}
        </div>
    }
}

/// The reason box for an option that takes feedback ("No, and tell Claude
/// what to do differently"), or for a plain "No" whose note follows it as
/// a prompt.
#[component]
fn Feedback(
    sender: Sender,
    pending: PendingInput,
    option: PendingOption,
    feedback_for: RwSignal<Option<(PendingId, PendingOption)>>,
    gate: Memo<Gate>,
) -> impl IntoView {
    let app = sender.app;
    let answering = sender.answering;
    let text = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let input = NodeRef::<leptos::html::Textarea>::new();
    Effect::new(move |_| {
        if let Some(el) = input.get() {
            let _ = el.focus();
        }
    });
    on_cleanup(move || {
        let _ = app.composing.try_set(false);
    });
    let empty = move || text.with(|t| t.trim().is_empty());
    let enabled = move || matches!(gate.get(), Gate::Ready(_)) && !answering.get();
    let label = option.label.clone();
    // A "No" that takes no feedback itself: the note goes as a prompt after.
    let as_note = choices::chip(&pending, &option) == Chip::Note;
    let send = move || {
        if !chips_ready(&sender, gate) {
            return;
        }
        let typed = text.get_untracked();
        let decided = if as_note {
            choices::deny_with_note(&pending, &option.id, &typed)
        } else {
            choices::choose(&pending, &option.id, Some(&typed)).map(|draft| (draft, None))
        };
        match decided {
            Ok((draft, note)) => {
                error.set(None);
                sender.submit_with_note(&draft, note);
                feedback_for.set(None);
            }
            Err(e) => error.set(Some(e.to_string())),
        }
    };
    view! {
        <div class="feedback">
            <label class="feedback-label" for="feedback-text">
                {format!("{label}: tell the agent what to do instead (optional)")}
            </label>
            <textarea
                id="feedback-text"
                class="composer-input feedback-input"
                rows="2"
                maxlength="4096"
                enterkeyhint="send"
                node_ref=input
                aria-invalid=move || error.get().is_some().to_string()
                aria-describedby="feedback-error"
                prop:value=move || text.get()
                on:input=move |ev| {
                    text.set(event_target_value(&ev));
                    error.set(None);
                }
                on:focus=move |_| app.composing.set(true)
                on:blur=move |_| app.composing.set(false)
            ></textarea>
            <p id="feedback-error" class="field-error" role="alert">{move || error.get()}</p>
            <div class="confirm-actions">
                <button type="button" class="button button-secondary" on:click=move |_| feedback_for.set(None)>"Back"</button>
                <button type="button" class="button button-primary" disabled=move || !enabled() on:click=move |_| send()>
                    {move || if empty() { format!("Choose “{}”", option.label) } else { "Send".to_owned() }}
                </button>
            </div>
        </div>
    }
}
