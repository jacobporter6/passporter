//! What the reply surface sends (design §6.2), pure so it is tested on the
//! host: drafts, the actions built from them, refusal texts and the keypad.
//! Delivery (queueing, retries, stale results) is the outbox's job; see
//! [`crate::outbox`].

use protocol::{
    Action, ActionKind, AgentId, AgentStatus, AllowedKey, ChoosePayload, FeedbackText,
    IdempotencyKey, InterruptPayload, KeySequence, KeysPayload, OptionId, Origin, PendingId,
    PromptPayload, PromptText, RefusalReason, Seq, Timestamp, keys::Digit,
};

use super::choices::PendingRead;
use crate::outbox::Delivery;
use crate::voice::phase::append_transcript;

/// What the user decided to send, before it becomes an [`Action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Draft {
    /// A new prompt.
    Prompt(PromptText),
    /// Keys from the keypad.
    Keys(KeySequence),
    /// Interrupt the running turn (^C, after a confirm).
    Interrupt,
    /// An option of the dialog the user was shown.
    Choose {
        /// The shown [`protocol::PendingInput`]'s id.
        pending: PendingId,
        /// The chosen option.
        option: OptionId,
        /// Its label, for status lines.
        label: String,
        /// The typed reason, for options that take one.
        feedback: Option<FeedbackText>,
    },
}

impl Draft {
    /// One key.
    #[must_use]
    pub fn key(key: AllowedKey) -> Self {
        // A single allowed key is always a valid sequence.
        KeySequence::new(vec![key]).map_or_else(
            |_| unreachable!("one allowed key is a valid sequence"),
            Self::Keys,
        )
    }

    /// A short description for status lines: `Prompt`, `Key 1`, `Interrupt`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Prompt(_) => "Message".to_owned(),
            Self::Keys(keys) => {
                let names: Vec<&str> = keys.as_slice().iter().map(|k| key_label(*k)).collect();
                format!("Key {}", names.join(" "))
            }
            Self::Interrupt => "Interrupt".to_owned(),
            Self::Choose { label, .. } => format!("“{label}”"),
        }
    }

    /// The draft an outbox entry was made from; `label` is the chosen
    /// option's (the option id stands in when it is missing).
    #[must_use]
    pub fn from_action(action: &Action, label: Option<&str>) -> Self {
        match &action.kind {
            ActionKind::Prompt(payload) => Self::Prompt(payload.text.clone()),
            ActionKind::Keys(payload) => Self::Keys(payload.keys.clone()),
            ActionKind::Interrupt(_) => Self::Interrupt,
            ActionKind::Choose(payload) => Self::Choose {
                pending: payload.pending_id.clone(),
                option: payload.option_id.clone(),
                label: label.map_or_else(|| payload.option_id.to_string(), ToOwned::to_owned),
                feedback: payload.feedback_text.clone(),
            },
        }
    }

    /// The text to put back in the composer on "Edit", for prompts.
    #[must_use]
    pub fn editable_text(&self) -> Option<String> {
        match self {
            Self::Prompt(text) => Some(text.as_str().to_owned()),
            Self::Keys(_) | Self::Interrupt | Self::Choose { .. } => None,
        }
    }
}

/// Builds the action for `draft`, decided now against the agent's `seq`.
/// Keys carry `dialog`, the id of the pending input shown while the agent
/// is blocked, so the bridge refuses them (409) if the dialog changed.
#[must_use]
pub fn build_action(
    agent: AgentId,
    seq: Seq,
    draft: &Draft,
    dialog: Option<&PendingId>,
    key: IdempotencyKey,
    now: Timestamp,
) -> Action {
    let kind = match draft {
        Draft::Prompt(text) => ActionKind::Prompt(PromptPayload::text(text.clone())),
        Draft::Keys(keys) => ActionKind::Keys(KeysPayload {
            pending_id: dialog.cloned(),
            keys: keys.clone(),
        }),
        Draft::Interrupt => ActionKind::Interrupt(InterruptPayload {}),
        Draft::Choose {
            pending,
            option,
            feedback,
            ..
        } => ActionKind::Choose(ChoosePayload {
            pending_id: pending.clone(),
            option_id: option.clone(),
            feedback_text: feedback.clone(),
        }),
    };
    Action {
        idempotency_key: key,
        agent_id: agent,
        expected_seq: seq,
        created_at: now,
        kind,
        origin: Origin::App,
    }
}

/// A note to send as a prompt once the "No" it follows is through
/// (deny-with-feedback, design §6.2). Two actions, in order: the choice,
/// then the note, and the note only once the choice was *sent* and the
/// agent is seen back at its prompt *after* that: a pending-input read
/// started after the choice's answer finds no dialog, and the agent is
/// listed idle or done at a newer seq. A prompt sent while the dialog is
/// still up would be refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    /// The choice's idempotency key (a retry keeps it).
    after: IdempotencyKey,
    /// The seq the choice was decided against.
    decided_at: Seq,
    note: PromptText,
    /// Once the choice was sent: the pending generation a read must have
    /// started at (or after) to count as seen after it.
    chosen_at: Option<u64>,
}

/// What to do with a [`FollowUp`] now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowStep {
    /// Not yet.
    Wait,
    /// Send this prompt (a new decision: new key, current seq).
    Send(Draft),
    /// The choice will not go through: give the note back to the user.
    Abandon(PromptText),
}

impl FollowUp {
    /// `note`, to follow `choice` (the action choosing "No").
    #[must_use]
    pub fn new(choice: &Action, note: PromptText) -> Self {
        Self {
            after: choice.idempotency_key.clone(),
            decided_at: choice.expected_seq,
            note,
            chosen_at: None,
        }
    }

    /// Whether `action` is the choice this note follows.
    #[must_use]
    pub fn follows(&self, action: &Action) -> bool {
        action.idempotency_key == self.after
    }

    /// The choice's idempotency key.
    #[must_use]
    pub const fn key(&self) -> &IdempotencyKey {
        &self.after
    }

    /// Whether the choice was sent (the note now waits for the agent).
    #[must_use]
    pub const fn is_chosen(&self) -> bool {
        self.chosen_at.is_some()
    }

    /// The note.
    #[must_use]
    pub const fn note(&self) -> &PromptText {
        &self.note
    }

    /// The choice's outbox entry is now at `delivery` (`None`: gone before
    /// it was sent, e.g. cancelled), and the pending input is being read
    /// again at `generation`. Anything but sent or a first try in flight
    /// gives the note back: even while the outbox retries the same key,
    /// the user sees their note instead of it waiting out of sight (they
    /// can send it once the "No" is through).
    pub fn on_delivery(&mut self, delivery: Option<&Delivery>, generation: u64) -> FollowStep {
        if self.is_chosen() {
            return FollowStep::Wait;
        }
        match delivery {
            Some(Delivery::Sent) => {
                self.chosen_at = Some(generation);
                FollowStep::Wait
            }
            Some(Delivery::Queued { attempts: 0, .. } | Delivery::Sending { attempts: 1 }) => {
                FollowStep::Wait
            }
            _ => FollowStep::Abandon(self.note.clone()),
        }
    }

    /// The agent is listed `status` at `seq`, and `read` is the last
    /// pending-input read: the note goes once the choice was sent and the
    /// agent is seen to have left the dialog for its prompt since.
    #[must_use]
    pub fn poll(&self, status: Option<AgentStatus>, seq: Seq, read: &PendingRead) -> FollowStep {
        let at_prompt = matches!(status, Some(AgentStatus::Idle | AgentStatus::Done));
        let left_dialog = self.chosen_at.is_some_and(|chosen_at| {
            read.generation >= chosen_at && !read.failed && read.pending.is_none()
        });
        if left_dialog && at_prompt && seq > self.decided_at {
            FollowStep::Send(Draft::Prompt(self.note.clone()))
        } else {
            FollowStep::Wait
        }
    }
}

/// What dictated words do, with the composer holding `draft`. The
/// composer's text becomes [`Self::text`] in every case: dictation goes
/// after what was typed, and the two are sent together as one prompt
/// (the simplest rule that never drops what was typed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dictated {
    /// Reviewed by the user: only fills the composer.
    Fill(String),
    /// Send as one prompt; the composer clears once the outbox has it.
    Send(String, PromptText),
    /// Not sent: the agent waits at a dialog, and the bridge refuses a
    /// prompt then (as it would a typed one, whose words the refusal
    /// card drops). The words stay in the composer.
    AtDialog(String),
    /// Not sent: not a valid prompt (say, too long), and why.
    Invalid(String, String),
}

impl Dictated {
    /// What `said` does: `send` unless the user reviewed it; `at_dialog`
    /// while the agent waits at a dialog.
    #[must_use]
    pub fn of(draft: &str, said: &str, send: bool, at_dialog: bool) -> Self {
        let text = append_transcript(draft, said);
        if !send {
            return Self::Fill(text);
        }
        if at_dialog {
            return Self::AtDialog(text);
        }
        match PromptText::new(text.trim_end()) {
            Ok(prompt) => Self::Send(text, prompt),
            Err(e) => Self::Invalid(text, format!("Can't send this: {e}")),
        }
    }

    /// The composer's text from now on.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Fill(text)
            | Self::Send(text, _)
            | Self::AtDialog(text)
            | Self::Invalid(text, _) => text,
        }
    }
}

/// Plain-language text for a refusal of `draft`. The MVP bridge uses
/// step-up for what the phone can't do yet (elevated prompts, prompts while
/// a dialog is up), and there is no step-up flow, so those are explained
/// rather than echoed.
#[must_use]
pub fn refusal_message(draft: &Draft, reason: RefusalReason, message: &str) -> String {
    if reason == RefusalReason::StepUpRequired {
        let text = match draft {
            Draft::Prompt(text) if text.is_elevated() => {
                "Slash, shell and memory commands need the desktop for now."
            }
            Draft::Prompt(_) => {
                "The agent is waiting at a dialog. Answer it with the options or the keypad."
            }
            Draft::Choose {
                feedback: Some(text),
                ..
            } if text.is_elevated() => "Messages starting with !, / or # need the desktop for now.",
            Draft::Choose { .. } => "Needs confirmation on the desktop for now.",
            Draft::Keys(_) | Draft::Interrupt => refusal_text(reason),
        };
        return text.to_owned();
    }
    if message.trim().is_empty() {
        refusal_text(reason).to_owned()
    } else {
        message.to_owned()
    }
}

/// Plain-language text for a refusal that came without a message.
#[must_use]
pub const fn refusal_text(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::CarPolicy => "Not allowed from the car. Confirm on the phone.",
        RefusalReason::StepUpRequired => "This needs extra confirmation the app can't do yet.",
        RefusalReason::UnknownAgent => "That agent is gone.",
        RefusalReason::UnknownOption | RefusalReason::NoPendingInput => {
            "The agent isn't asking that any more."
        }
        RefusalReason::FeedbackNotAccepted => "That option doesn't take a message.",
        RefusalReason::RateLimited => "Too many actions at once. Try again in a moment.",
        RefusalReason::IdempotencyConflict => "This decision was already used for something else.",
        RefusalReason::InvalidAction => "The bridge couldn't accept that. Try rewording it.",
        RefusalReason::UnknownAttachment => {
            "The photo is no longer on the bridge. Attach it again."
        }
        RefusalReason::Other => "The bridge refused this.",
    }
}

/// One keypad button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pad {
    /// An allowed key.
    Key(AllowedKey),
    /// ^C: interrupt, after a confirm.
    Interrupt,
    /// Switch to the text composer.
    Compose,
}

impl Pad {
    /// Visible label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Key(key) => key_label(key),
            Self::Interrupt => "^C",
            Self::Compose => "Aa",
        }
    }

    /// Accessible name.
    #[must_use]
    pub const fn aria(self) -> &'static str {
        match self {
            Self::Key(AllowedKey::Up) => "Arrow up",
            Self::Key(AllowedKey::Down) => "Arrow down",
            Self::Key(AllowedKey::Left) => "Arrow left",
            Self::Key(AllowedKey::Right) => "Arrow right",
            Self::Key(AllowedKey::Enter) => "Enter",
            Self::Key(AllowedKey::Esc) => "Escape",
            Self::Key(AllowedKey::Tab) => "Tab",
            Self::Key(AllowedKey::CtrlC) | Self::Interrupt => "Interrupt (Control C)",
            Self::Key(AllowedKey::Digit(_)) => "Digit",
            Self::Compose => "Type a message",
        }
    }
}

/// The keypad layout, row by row (four columns): digits on the left, the
/// navigation keys on the right, the escape hatches and the switch to the
/// composer at the bottom.
#[must_use]
pub fn keypad() -> [[Pad; 4]; 4] {
    let d = |n: u8| {
        Pad::Key(AllowedKey::Digit(
            Digit::new(n).unwrap_or_else(|| unreachable!("keypad digits are 1..=9")),
        ))
    };
    [
        [d(1), d(2), d(3), Pad::Key(AllowedKey::Up)],
        [d(4), d(5), d(6), Pad::Key(AllowedKey::Down)],
        [d(7), d(8), d(9), Pad::Key(AllowedKey::Enter)],
        [
            Pad::Key(AllowedKey::Esc),
            Pad::Key(AllowedKey::Tab),
            Pad::Interrupt,
            Pad::Compose,
        ],
    ]
}

/// Visible label of a key.
#[must_use]
pub const fn key_label(key: AllowedKey) -> &'static str {
    match key {
        AllowedKey::Digit(_) => key.as_str(),
        AllowedKey::Up => "↑",
        AllowedKey::Down => "↓",
        AllowedKey::Left => "←",
        AllowedKey::Right => "→",
        AllowedKey::Enter => "⏎",
        AllowedKey::Esc => "Esc",
        AllowedKey::Tab => "Tab",
        AllowedKey::CtrlC => "^C",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";

    fn key(k: &str) -> IdempotencyKey {
        IdempotencyKey::new(k).unwrap()
    }

    fn prompt(text: &str) -> Draft {
        Draft::Prompt(PromptText::new(text).unwrap())
    }

    #[test]
    fn dictation_is_sent_after_what_was_typed() {
        let sent = |draft, said| match Dictated::of(draft, said, true, false) {
            Dictated::Send(text, prompt) => {
                assert_eq!(text, prompt.as_str());
                text
            }
            other => panic!("not sent: {other:?}"),
        };
        assert_eq!(sent("", " run the tests "), "run the tests");
        assert_eq!(sent("Then", "run them"), "Then run them");
        assert_eq!(
            sent("Fix the parser.\n", "And add a test."),
            "Fix the parser.\nAnd add a test."
        );
    }

    #[test]
    fn dictation_waits_in_the_composer_at_a_dialog() {
        assert_eq!(
            Dictated::of("Then", "run them", true, true),
            Dictated::AtDialog("Then run them".into())
        );
    }

    #[test]
    fn reviewed_dictation_only_fills_the_composer() {
        let filled = Dictated::of("Then", "run them", false, true);
        assert_eq!(filled, Dictated::Fill("Then run them".into()));
        assert_eq!(filled.text(), "Then run them");
    }

    #[test]
    fn dictation_too_long_to_send_stays_in_the_composer() {
        let long = "word ".repeat(10_000);
        let Dictated::Invalid(text, why) = Dictated::of(&long, "more", true, false) else {
            panic!("sent")
        };
        assert!(text.ends_with("more"));
        assert!(why.starts_with("Can't send this"));
    }

    fn action(draft: &Draft, seq: u64, k: &str) -> Action {
        build_action(
            "w1:p1".parse().unwrap(),
            Seq(seq),
            draft,
            None,
            key(k),
            Timestamp::from_unix_millis(1),
        )
    }

    #[test]
    fn builds_each_verb() {
        let a = action(&prompt("go"), 4, K1);
        assert_eq!(a.expected_seq, Seq(4));
        assert_eq!(a.origin, Origin::App);
        assert!(matches!(a.kind, ActionKind::Prompt(_)));
        let k = action(&Draft::key(AllowedKey::Enter), 4, K1);
        let ActionKind::Keys(payload) = &k.kind else {
            panic!("not keys")
        };
        assert_eq!(payload.keys.as_slice(), [AllowedKey::Enter]);
        assert_eq!(payload.pending_id, None);
        let i = action(&Draft::Interrupt, 4, K1);
        assert_eq!(i.kind, ActionKind::Interrupt(InterruptPayload {}));
        // Serializes as the protocol expects.
        let json = serde_json::to_value(&k).unwrap();
        assert_eq!(json["kind"], "keys");
        assert_eq!(json["payload"]["keys"][0], "enter");
    }

    #[test]
    fn keypad_maps_to_allowed_keys() {
        let wire: Vec<Vec<&str>> = keypad()
            .iter()
            .map(|row| {
                row.iter()
                    .map(|pad| match pad {
                        Pad::Key(k) => k.as_str(),
                        Pad::Interrupt => "interrupt",
                        Pad::Compose => "compose",
                    })
                    .collect()
            })
            .collect();
        assert_eq!(
            wire,
            [
                ["1", "2", "3", "up"],
                ["4", "5", "6", "down"],
                ["7", "8", "9", "enter"],
                ["esc", "tab", "interrupt", "compose"],
            ]
        );
        assert_eq!(Pad::Key(AllowedKey::Enter).label(), "⏎");
        assert_eq!(Pad::Interrupt.label(), "^C");
        assert_eq!(Draft::key(AllowedKey::Up).describe(), "Key ↑");
    }

    fn choice(option: &str, feedback: Option<&str>) -> Draft {
        Draft::Choose {
            pending: PendingId::new("perm-7").unwrap(),
            option: OptionId::new(option).unwrap(),
            label: format!("Label {option}"),
            feedback: feedback.map(|f| FeedbackText::new(f).unwrap()),
        }
    }

    #[test]
    fn choose_carries_the_shown_pending_id_option_and_feedback() {
        let a = action(&choice("allow_once", None), 9, K1);
        assert_eq!(a.expected_seq, Seq(9));
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["kind"], "choose");
        assert_eq!(json["payload"]["pendingId"], "perm-7");
        assert_eq!(json["payload"]["optionId"], "allow_once");
        assert!(json["payload"].get("feedbackText").is_none());
        let deny = action(&choice("deny", Some("use pnpm")), 9, K1);
        let json = serde_json::to_value(&deny).unwrap();
        assert_eq!(json["payload"]["feedbackText"], "use pnpm");
        // The bridge's stale guard sees the pending id.
        assert_eq!(
            deny.pending_id().map(ToString::to_string).as_deref(),
            Some("perm-7")
        );
        assert_eq!(choice("deny", None).describe(), "“Label deny”");
    }

    #[test]
    fn keys_name_the_shown_dialog_when_there_is_one() {
        let dialog = PendingId::new("screen-12-0123456789ab").unwrap();
        let draft = Draft::key(AllowedKey::Enter);
        let a = build_action(
            "w1:p1".parse().unwrap(),
            Seq(12),
            &draft,
            Some(&dialog),
            key(K1),
            Timestamp::from_unix_millis(1),
        );
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["payload"]["pendingId"], "screen-12-0123456789ab");
        let plain = action(&draft, 12, K1);
        assert!(
            serde_json::to_value(&plain).unwrap()["payload"]
                .get("pendingId")
                .is_none()
        );
        // Prompts and interrupts never carry it.
        let p = build_action(
            "w1:p1".parse().unwrap(),
            Seq(12),
            &prompt("hi"),
            Some(&dialog),
            key(K1),
            Timestamp::from_unix_millis(1),
        );
        assert_eq!(p.pending_id(), None);
    }

    #[test]
    fn drafts_come_back_from_their_actions() {
        for draft in [
            prompt("go"),
            Draft::key(AllowedKey::Esc),
            Draft::Interrupt,
            choice("deny", Some("use pnpm")),
        ] {
            let a = action(&draft, 1, K1);
            let label = match &draft {
                Draft::Choose { label, .. } => Some(label.as_str()),
                _ => None,
            };
            assert_eq!(Draft::from_action(&a, label), draft);
        }
        // Without its label, a choice names the option id.
        let a = action(&choice("deny", None), 1, K1);
        assert_eq!(Draft::from_action(&a, None).describe(), "“deny”");
    }

    const K2: &str = "0ca09978-ef89-46fd-9a85-118b84bee7b4";

    fn plain_no() -> protocol::PendingInput {
        serde_json::from_str(
            r#"{"id":"perm-7","kind":"permission","prompt":"Run `pnpm test`?","source":"hook",
                "options":[{"id":"yes","label":"Yes","risk":"low"},
                           {"id":"yes_always","label":"Yes, don't ask again","risk":"elevated"},
                           {"id":"no","label":"No","risk":"low"}]}"#,
        )
        .unwrap()
    }

    fn read(generation: u64, dialog: bool) -> PendingRead {
        PendingRead {
            pending: dialog.then(plain_no),
            seq: Some(Seq(6)),
            generation,
            failed: false,
        }
    }

    #[test]
    fn deny_then_feedback_sends_two_actions_in_order() {
        use super::super::choices;

        let pending = plain_no();
        let no = OptionId::new("no").unwrap();
        let (deny, note) = choices::deny_with_note(&pending, &no, "use pnpm instead").unwrap();
        let mut sent = vec![action(&deny, 5, K1)];
        let mut follow = FollowUp::new(&sent[0], note.unwrap());
        assert!(follow.follows(&sent[0]));
        // Nothing before the choice's answer, and not while it is queued
        // or on its first try.
        assert_eq!(
            follow.poll(Some(AgentStatus::Idle), Seq(6), &read(3, false)),
            FollowStep::Wait
        );
        let queued = Delivery::Queued {
            attempts: 0,
            retry_at: Timestamp::from_unix_millis(1),
        };
        assert_eq!(follow.on_delivery(Some(&queued), 2), FollowStep::Wait);
        let sending = Delivery::Sending { attempts: 1 };
        assert_eq!(follow.on_delivery(Some(&sending), 2), FollowStep::Wait);
        assert_eq!(
            follow.on_delivery(Some(&Delivery::Sent), 3),
            FollowStep::Wait
        );
        assert!(follow.is_chosen());
        // Its tick going away later changes nothing.
        assert_eq!(follow.on_delivery(None, 9), FollowStep::Wait);
        // Listed idle at a newer seq, but not seen to leave the dialog
        // since the choice: a read from before it, a read still showing
        // the dialog, or a failed read.
        for early in [
            read(2, false),
            read(3, true),
            PendingRead {
                failed: true,
                ..read(3, false)
            },
        ] {
            assert_eq!(
                follow.poll(Some(AgentStatus::Idle), Seq(7), &early),
                FollowStep::Wait
            );
        }
        // Still at the dialog, or not past the choice's seq: wait.
        assert_eq!(
            follow.poll(Some(AgentStatus::Blocked), Seq(7), &read(3, false)),
            FollowStep::Wait
        );
        assert_eq!(
            follow.poll(Some(AgentStatus::Idle), Seq(5), &read(3, false)),
            FollowStep::Wait
        );
        let FollowStep::Send(draft) = follow.poll(Some(AgentStatus::Idle), Seq(7), &read(4, false))
        else {
            panic!("the note goes once the agent is back at its prompt")
        };
        sent.push(action(&draft, 7, K2));
        let kinds: Vec<serde_json::Value> = sent
            .iter()
            .map(|a| serde_json::to_value(a).unwrap())
            .collect();
        assert_eq!(kinds[0]["kind"], "choose");
        assert_eq!(kinds[0]["payload"]["optionId"], "no");
        assert!(kinds[0]["payload"].get("feedbackText").is_none());
        assert_eq!(kinds[1]["kind"], "prompt");
        assert_eq!(kinds[1]["payload"]["text"], "use pnpm instead");
        assert_eq!(kinds[1]["expectedSeq"], 7);
        assert_ne!(sent[0].idempotency_key, sent[1].idempotency_key);
    }

    #[test]
    fn a_note_is_given_back_when_the_no_does_not_go_through() {
        use super::super::choices;

        let pending = plain_no();
        let no = OptionId::new("no").unwrap();
        let (deny, note) = choices::deny_with_note(&pending, &no, "use pnpm").unwrap();
        let note = note.unwrap();
        let a = action(&deny, 5, K1);
        let back = FollowStep::Abandon(note.clone());
        for delivery in [
            // No answer: the outbox retries the same key, but the note
            // comes back into view rather than waiting unseen.
            Some(Delivery::Queued {
                attempts: 1,
                retry_at: Timestamp::from_unix_millis(1),
            }),
            Some(Delivery::Sending { attempts: 2 }),
            Some(Delivery::Refused {
                reason: RefusalReason::NoPendingInput,
                message: String::new(),
            }),
            // The dialog changed: nothing was chosen.
            Some(Delivery::Stale { current: None }),
            Some(Delivery::Failed {
                message: "x".into(),
            }),
            // Cancelled before it went.
            None,
        ] {
            let mut follow = FollowUp::new(&a, note.clone());
            assert_eq!(
                follow.on_delivery(delivery.as_ref(), 1),
                back,
                "{delivery:?}"
            );
            assert!(!follow.is_chosen());
            assert_eq!(
                follow.poll(Some(AgentStatus::Idle), Seq(9), &read(5, false)),
                FollowStep::Wait
            );
        }
    }

    #[test]
    fn step_up_refusals_are_explained() {
        for (draft, want) in [
            (prompt("/clear"), "desktop"),
            (prompt("yes please"), "keypad"),
            (choice("always", None), "desktop"),
            (choice("deny", Some("/clear")), "!, / or #"),
        ] {
            let text = refusal_message(&draft, RefusalReason::StepUpRequired, "step up");
            assert!(text.contains(want), "{text}");
        }
        assert_eq!(
            refusal_message(&prompt("x"), RefusalReason::InvalidAction, "too long"),
            "too long"
        );
        assert_eq!(
            refusal_message(&prompt("x"), RefusalReason::NoPendingInput, " "),
            refusal_text(RefusalReason::NoPendingInput)
        );
    }
}
