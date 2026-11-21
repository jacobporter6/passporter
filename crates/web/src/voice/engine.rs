//! The dictation WASM's side of dictation (design §6.3, §7): one recorder
//! per agent, and a queue of notes on their way to a transcript, kept for
//! the whole page session, so a recording that is uploading when the user
//! swipes to another agent is still there when they come back. A new take
//! can be recorded while earlier notes wait for the bridge.
//!
//! This runs in `voice_bg.wasm` (`src/bin/voice.rs`), loaded by the page
//! on demand ([`super::module`]). It draws the strip itself
//! ([`super::panel`]) into an element the page mounts, and tells the page,
//! through the [`Host`] it hands over, what the mic button can do and
//! what was said; it asks the page for the bridge token and whether to
//! review dictation before sending.
//!
//! A take sends itself: once its transcript is in, the page sends it to
//! the agent as a prompt, through the outbox ([`Arrival`]). With "Review
//! dictation before sending" on, for a clip left from an earlier visit
//! (the user wasn't there to see it go), or behind an older note waiting
//! for review, it shows as a card to edit and "Use" instead.

use core::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use protocol::{AgentId, AudioId, Timestamp};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::spawn_local;

use super::browser::{FetchAudio, IdbClips};
use super::capture::Capture;
use super::clip::{PendingClip, Stage};
use super::courier::{Courier, DeliverError, Delivery};
use super::native::{self, Backend};
use super::panel::{Act, Panel};
use super::phase::{
    Arrival, NOTHING_HEARD, Note, NoteState, RowKey, SCREEN_OFF, Take, keeps_awake,
    next_to_deliver, rows,
};
use super::recorder::{
    CaptureError, Dictaphone, Halt, MicGesture, MicIntent, RecorderConfig, Recording, StopReason,
    Update,
};
use super::upload::{CHUNK_BYTES, Progress, UploadError};
use super::wake;
use crate::api::{ApiError, BearerToken, TokenSource};
use crate::idgen::random_key;
use crate::pair::scanner::sleep;
use crate::platform;
use crate::ui::format::now;

/// Waits before retrying a delivery that found the bridge unreachable, in
/// ms; the last repeats while it stays unreachable.
const RETRY_DELAYS_MS: [i32; 5] = [2_000, 5_000, 10_000, 20_000, 30_000];
/// How long a clip may stay on the phone, in ms: ninety days. Until it is
/// delivered it is the only copy of what was said, so it outlives the
/// bridge's own retention (seven days by default): a bridge that has
/// forgotten the upload is sent it again.
const CLIP_RETENTION_MS: u64 = 90 * 24 * 60 * 60 * 1_000;
/// How long a delivery retries by itself without upload progress, in ms,
/// before it waits: for Retry, the phone coming back online, or the next
/// visit (see [`parks`]).
const AUTO_RETRY_MS: u64 = 12 * 60 * 1_000;
/// Added to the message of a delivery that stopped retrying by itself.
const RETRIES_LATER: &str =
    "It tries again when this phone is back online or the app reopens, or tap Retry.";
/// The browser event for the network coming back.
const ONLINE: &str = "online";
/// A waiting retry checks for "Retry now" this often, in ms.
const WAKE_CHECK_MS: i32 = 250;

#[wasm_bindgen]
extern "C" {
    /// The page, as `crate::voice::module` builds it: where phases, meter
    /// readings and notes go, and where the token comes from.
    #[derive(Debug, Clone)]
    pub type Host;

    /// `agent`'s mic button is now in this [`super::phase::Mic::code`] state.
    #[wasm_bindgen(method)]
    fn mic(this: &Host, agent: &str, code: u32);

    /// `text` was said to `agent`: with `send`, send it as a prompt
    /// (after anything typed); without, the user reviewed it and it goes
    /// in the composer. False if no screen took it.
    #[wasm_bindgen(method, js_name = takeText)]
    fn take_text(this: &Host, agent: &str, text: &str, send: bool) -> bool;

    /// Whether the user reviews dictation before it is sent.
    #[wasm_bindgen(method, js_name = reviewDictation)]
    fn review_dictation(this: &Host) -> bool;

    /// A note for the user.
    #[wasm_bindgen(method)]
    fn toast(this: &Host, text: &str);

    /// A clip was just saved on the device.
    #[wasm_bindgen(method)]
    fn saved(this: &Host);

    /// The bridge token to send now, if the device has one.
    #[wasm_bindgen(method)]
    fn token(this: &Host) -> Option<String>;

    /// The bridge answered 401 to a request sent with `token`.
    #[wasm_bindgen(method)]
    fn unauthorized(this: &Host, token: &str);

    /// Whether the page dropped a pasted token the bridge refused (it is
    /// not renewed, so retrying can't help).
    #[wasm_bindgen(method, js_name = tokenRejected)]
    fn token_rejected(this: &Host) -> bool;
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    static AGENTS: RefCell<HashMap<AgentId, Rc<Dictation>>> = RefCell::new(HashMap::new());
    /// Old clips are swept once per page session.
    static SWEPT: Cell<bool> = const { Cell::new(false) };
}

/// Hands over the page's [`Host`]. Called once, right after loading. Also
/// finds out which recorder this page has ([`native::backend`]), so the
/// first tap knows.
pub fn init(host: Host) {
    HOST.set(Some(host));
    listen_online();
    spawn_local(async {
        native::backend().await;
    });
}

/// Retries parked deliveries whenever the browser reports the network
/// back, for the page's lifetime.
fn listen_online() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let on_online = Closure::<dyn Fn()>::new(|| {
        let all: Vec<_> = AGENTS.with_borrow(|all| all.values().cloned().collect());
        for dictation in all {
            dictation.retry_parked();
        }
    })
    .into_js_value();
    let _ = window.add_event_listener_with_callback(ONLINE, on_online.unchecked_ref());
}

fn host() -> Option<Host> {
    HOST.with_borrow(Clone::clone)
}

/// The token, from the page.
#[derive(Debug, Clone, Copy)]
struct HostToken;

impl TokenSource for HostToken {
    fn current(&self) -> Option<BearerToken> {
        host()?
            .token()
            .and_then(|token| BearerToken::new(token).ok())
    }

    fn unauthorized(&self, rejected: &BearerToken) {
        if let Some(host) = host() {
            host.unauthorized(rejected.expose());
        }
    }
}

type BrowserCourier = Courier<IdbClips, FetchAudio<HostToken>>;

fn courier() -> BrowserCourier {
    Courier::new(
        IdbClips,
        FetchAudio::new(HostToken),
        CHUNK_BYTES,
        new_audio_id,
    )
}

fn new_audio_id() -> Option<AudioId> {
    AudioId::new(random_key()?.to_string()).ok()
}

fn wait(ms: u32) -> impl Future<Output = ()> {
    sleep(i32::try_from(ms).unwrap_or(i32::MAX))
}

/// `agent`'s dictation, made on first use.
#[must_use]
pub fn dictation(agent: &AgentId) -> Rc<Dictation> {
    AGENTS.with_borrow_mut(|all| {
        all.entry(agent.clone())
            .or_insert_with(|| {
                Rc::new_cyclic(|this| Dictation {
                    this: this.clone(),
                    panel: RefCell::new(None),
                    agent: agent.clone(),
                    key: agent.to_string(),
                    take: RefCell::new(Take::Idle),
                    notes: RefCell::new(Vec::new()),
                    next_key: Cell::new(0),
                    phone: phone(agent),
                    gesture: Cell::new(MicGesture::default()),
                    delivering: Cell::new(None),
                    wake: Cell::new(false),
                    takes: Cell::new(0),
                    send: Cell::new(true),
                    awake: Cell::new(false),
                    loaded: Cell::new(false),
                })
            })
            .clone()
    })
}

/// `agent`'s recorder, on whichever microphone this page has.
fn phone(agent: &AgentId) -> Dictaphone<Capture> {
    let config = RecorderConfig::default();
    // The cap is about an hour: exact in a u64.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let max_ms = config.max_ms as u64;
    Dictaphone::new(Capture::new(agent.to_string(), max_ms), config)
}

/// One agent's dictation: the recorder, and the notes on their way to a
/// transcript (see [`super::phase`]).
pub struct Dictation {
    /// Itself, for the strip's buttons.
    this: Weak<Self>,
    /// The strip, while a screen shows it.
    panel: RefCell<Option<Panel>>,
    agent: AgentId,
    /// `agent`, as the page knows it.
    key: String,
    /// The take being recorded.
    take: RefCell<Take>,
    /// The stored notes, oldest first.
    notes: RefCell<Vec<Note>>,
    /// The next [`Note::key`].
    next_key: Cell<u32>,
    phone: Dictaphone<Capture>,
    gesture: Cell<MicGesture>,
    /// The note being delivered, if any: one at a time.
    delivering: Cell<Option<u32>>,
    /// "Retry now": cut the current wait short.
    wake: Cell<bool>,
    /// Counts recordings started, to tell a late answer from an old take.
    takes: Cell<u32>,
    /// Whether the current take sends its transcript without review.
    send: Cell<bool>,
    /// Whether this agent is keeping the screen on ([`wake`]).
    awake: Cell<bool>,
    /// Whether the notes stored by earlier visits have been loaded.
    loaded: Cell<bool>,
}

impl core::fmt::Debug for Dictation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dictation")
            .field("agent", &self.agent)
            .field("take", &self.take)
            .field("notes", &self.notes)
            .finish_non_exhaustive()
    }
}

impl Dictation {
    /// Every change goes through here, so the screen is kept on for
    /// exactly as long as something is under way, whichever way it ends.
    fn redraw(&self) {
        let (awake, mic, rows) = {
            let take = self.take.borrow();
            let notes = self.notes.borrow();
            (
                keeps_awake(&take, &notes),
                take.mic(),
                rows(&take, &notes, self.send.get()),
            )
        };
        if self.awake.replace(awake) != awake {
            wake::keep_on(awake);
        }
        if let Some(host) = host() {
            host.mic(&self.key, mic.code());
        }
        if let Some(panel) = self.panel.borrow_mut().as_mut() {
            panel.draw(&rows);
        }
    }

    fn set_take(&self, take: Take) {
        *self.take.borrow_mut() = take;
        self.redraw();
    }

    fn take_is(&self, check: impl FnOnce(&Take) -> bool) -> bool {
        check(&self.take.borrow())
    }

    /// Note `key`, if it is still on the strip.
    fn note(&self, key: u32) -> Option<Note> {
        self.notes
            .borrow()
            .iter()
            .find(|note| note.key == key)
            .cloned()
    }

    /// Whether note `key` is still on the strip (not sent or discarded).
    fn has(&self, key: u32) -> bool {
        self.notes.borrow().iter().any(|note| note.key == key)
    }

    /// Changes note `key`, if it is still on the strip.
    fn update(&self, key: u32, change: impl FnOnce(&mut Note)) {
        let found = {
            let mut notes = self.notes.borrow_mut();
            notes
                .iter_mut()
                .find(|note| note.key == key)
                .map(change)
                .is_some()
        };
        if found {
            self.redraw();
        }
    }

    fn set_state(&self, key: u32, state: NoteState) {
        self.update(key, |note| note.state = state);
    }

    /// Adds `note` under a fresh key, at the end (`newest`) or the start.
    fn add_note(&self, mut note: Note, newest: bool) {
        let key = self.next_key.get();
        self.next_key.set(key.wrapping_add(1));
        note.key = key;
        {
            let mut notes = self.notes.borrow_mut();
            if newest {
                notes.push(note);
            } else {
                notes.insert(0, note);
            }
        }
        self.redraw();
    }

    /// Shows the strip in `root` (a screen mounting), in place of any
    /// other element.
    pub fn mount(&self, root: web_sys::Element) {
        let panel = Panel::new(root, self.this.clone());
        *self.panel.borrow_mut() = panel;
        self.redraw();
    }

    /// Stops drawing into `root` (its screen went away). A newer screen's
    /// element stays.
    pub fn unmount(&self, root: &web_sys::Element) {
        let mut panel = self.panel.borrow_mut();
        if panel.as_ref().is_some_and(|p| p.is_in(root)) {
            *panel = None;
        }
    }

    /// A button on row `row` of the strip.
    pub fn act(self: &Rc<Self>, act: Act, row: RowKey) {
        match (act, row) {
            (Act::Stop, _) => self.stop(),
            (Act::Dismiss, _) => self.dismiss(),
            (Act::Retry, RowKey::Note(key)) => self.retry(key),
            (Act::Cancel | Act::Discard, RowKey::Note(key)) => self.finish(key),
            (Act::Use, RowKey::Note(key)) => {
                let text = self
                    .panel
                    .borrow()
                    .as_ref()
                    .and_then(|panel| panel.transcript(key))
                    .unwrap_or_default();
                let text = text.trim();
                if text.is_empty() {
                    return;
                }
                if let Some(host) = host() {
                    host.take_text(&self.key, text, false);
                }
                self.finish(key);
            }
            (Act::Rerecord, RowKey::Note(key)) => {
                self.finish(key);
                self.start();
            }
            (Act::Retry | Act::Cancel | Act::Discard | Act::Use | Act::Rerecord, RowKey::Take) => {}
        }
    }

    /// The mic button went down at `at_ms`. A take that is still opening
    /// counts as recording: a quick second tap stops it.
    pub fn press(self: &Rc<Self>, at_ms: f64) {
        let mut gesture = self.gesture.get();
        let intent = gesture.press(at_ms, self.phone.is_recording());
        self.gesture.set(gesture);
        match intent {
            MicIntent::Start => {
                self.phone.hold(true);
                self.start();
            }
            MicIntent::Stop => self.stop(),
        }
    }

    /// The mic button came up at `at_ms`: a hold ends the recording, unless
    /// the microphone is still opening (see [`MicGesture::release`]). The
    /// release can carry user activation where the press couldn't (a
    /// touch), so the meter's audio is resumed here too.
    pub fn release(&self, at_ms: f64) {
        self.phone.capture().resume_meter();
        let mut gesture = self.gesture.get();
        let intent = gesture.release(at_ms, self.phone.is_opening());
        self.gesture.set(gesture);
        self.phone.hold(false);
        if intent == Some(MicIntent::Stop) {
            self.stop();
        }
    }

    /// Starts recording, unless a take is already recording or saving.
    /// Notes on their way don't stop it. Call it from a user gesture: the
    /// meter's audio can only start inside one.
    pub fn start(self: &Rc<Self>) {
        if !self.take_is(Take::can_start) || self.phone.is_recording() {
            return;
        }
        self.phone.capture().prime();
        platform::haptic_tick();
        // Sending on stop means tap to send: a pause mid-thought must not
        // end the take. Reviewing keeps silence auto-stop.
        self.send.set(sends());
        self.phone.auto_stop(!self.send.get());
        self.set_take(Take::Opening);
        // Which take this is: a cancelled one that opens (or fails) late
        // must not touch the strip of the one after it.
        let take = self.takes.get().wrapping_add(1);
        self.takes.set(take);
        let observer = {
            let this = self.clone();
            move |update| this.on_update(update)
        };
        // Current from here on, before the task below first runs.
        let opened = self.phone.start(observer);
        let this = self.clone();
        spawn_local(async move {
            let result = opened.await;
            let opening = this.takes.get() == take && this.take_is(|t| *t == Take::Opening);
            match result {
                Ok(()) if opening => this.set_take(Take::Recording),
                Err(error) if opening => this.set_take(Take::Failed(capture_message(&error))),
                Ok(()) | Err(_) => {}
            }
        });
    }

    /// Stops recording; the clip is then saved and queued. While the
    /// microphone is still opening there is nothing to keep: it cancels.
    pub fn stop(&self) {
        self.gesture.set(MicGesture::default());
        self.phone.hold(false);
        if self.phone.stop() == Halt::Cancelled {
            self.set_take(Take::Idle);
        }
    }

    /// The screen is going away: a recording stops (and is kept).
    pub fn leave(&self) {
        if self.phone.is_recording() {
            self.stop();
        }
    }

    fn on_update(self: &Rc<Self>, update: Update) {
        match update {
            Update::Level { bars, elapsed_ms } => {
                if self.take_is(|t| *t == Take::Opening) {
                    self.set_take(Take::Recording);
                }
                if let Some(panel) = self.panel.borrow().as_ref() {
                    panel.meter(&bars, elapsed_ms);
                }
            }
            Update::Stopping(_) => {
                self.release_mic();
                platform::haptic_tick();
                self.set_take(Take::Saving);
            }
            Update::Finished(recording) => {
                self.release_mic();
                platform::haptic_tick();
                self.set_take(Take::Saving);
                let send = sends_after(recording.stopped_by, self.send.get());
                let stopped = card_note(recording.stopped_by);
                if let (Some(note), Some(host)) = (stop_note(recording.stopped_by, send), host()) {
                    host.toast(note);
                }
                let this = self.clone();
                spawn_local(async move { this.keep(recording, send, stopped).await });
            }
            Update::Failed(error) => {
                self.release_mic();
                self.set_take(Take::Failed(capture_message(&error)));
            }
        }
    }

    /// The take is over: forget any press and hold. The button may have
    /// been disabled under a held finger, so its `pointerup` can be lost.
    fn release_mic(&self) {
        self.gesture.set(MicGesture::default());
        self.phone.hold(false);
    }

    /// Stores the recording, queues it as a note (sent without review if
    /// `send`; a review card shows `stopped`), and frees the mic. A take the
    /// native recorder made is stored under its id, and only then released
    /// there.
    async fn keep(
        self: &Rc<Self>,
        recording: Recording,
        send: bool,
        stopped: Option<&'static str>,
    ) {
        let native = recording.id.is_some();
        let Some(id) = recording.id.clone().or_else(new_audio_id) else {
            self.set_take(Take::Failed(
                "This browser can't make a recording id.".to_owned(),
            ));
            return;
        };
        let duration_ms = recording.duration_ms;
        match courier()
            .keep(self.agent.clone(), id.clone(), recording, now())
            .await
        {
            Ok(_) => {
                if let Some(host) = host() {
                    host.saved();
                }
                if native {
                    let id = id.clone();
                    spawn_local(async move { native::settle(&id, true).await });
                }
                *self.take.borrow_mut() = Take::Idle;
                self.add_note(
                    Note {
                        key: 0,
                        id,
                        duration_ms,
                        send,
                        stopped,
                        state: NoteState::Queued,
                    },
                    true,
                );
                self.pump();
            }
            Err(error) => {
                if native {
                    // Still safe natively: a later visit stores it.
                    native::settle(&id, false).await;
                }
                self.set_take(Take::Failed(format!(
                    "Couldn't save the recording on this phone: {}.",
                    error.0
                )));
            }
        }
    }

    /// Picks up the notes earlier visits left on the phone, once per page
    /// session for each agent, ahead of any recorded since. The first call
    /// in a page session also deletes clips (of any agent) past
    /// [`CLIP_RETENTION_MS`]. In the app, takes the native recorder kept
    /// for no page (the app was killed, or the take ended while the page was
    /// gone) are stored first ([`native::adopt`]), so they show here too.
    pub fn resume(self: &Rc<Self>) {
        if self.loaded.replace(true) {
            return;
        }
        let sweep_before = (!SWEPT.replace(true)).then(|| {
            Timestamp::from_unix_millis(now().unix_millis().saturating_sub(CLIP_RETENTION_MS))
        });
        let this = self.clone();
        spawn_local(async move {
            let courier = courier();
            if native::backend().await == Backend::Native {
                native::adopt(&courier).await;
            }
            let stored = match courier.waiting(&this.agent, sweep_before).await {
                Ok(stored) => stored,
                Err(error) => {
                    warn(&error.to_string());
                    return;
                }
            };
            // Oldest last, so each goes in ahead of the one after it.
            for meta in stored.into_iter().rev() {
                let known = this
                    .notes
                    .borrow()
                    .iter()
                    .any(|note| note.id == meta.audio_id);
                if known {
                    continue;
                }
                // A transcript needs no audio and no network; a clip that
                // failed for good waits for the user's Retry or Discard.
                // The user wasn't there when the rest was said: they
                // review it.
                let state = match (meta.transcript, meta.failure) {
                    (Some(text), _) => NoteState::Ready { text, note: None },
                    (None, Some(message)) => NoteState::Failed {
                        message,
                        parked: false,
                    },
                    (None, None) => NoteState::Queued,
                };
                this.add_note(
                    Note {
                        key: 0,
                        id: meta.audio_id,
                        duration_ms: meta.duration_ms,
                        send: false,
                        stopped: None,
                        state,
                    },
                    false,
                );
            }
            this.pump();
        });
    }

    /// Starts delivering the next note, unless one is on its way already.
    fn pump(self: &Rc<Self>) {
        if self.delivering.get().is_some() {
            return;
        }
        let Some(key) = next_to_deliver(&self.notes.borrow()) else {
            return;
        };
        let Some(id) = self.note(key).map(|note| note.id) else {
            return;
        };
        self.delivering.set(Some(key));
        self.wake.set(false);
        let this = self.clone();
        spawn_local(async move {
            match courier().load(&id).await {
                Ok(Some(clip)) if this.has(key) => this.deliver(key, clip).await,
                Ok(Some(_)) => {}
                // Gone from the phone (another tab, or cleared storage).
                Ok(None) => this.drop_note(key),
                Err(error) => this.set_state(
                    key,
                    NoteState::Failed {
                        message: deliver_message(&DeliverError::Store(error)),
                        parked: false,
                    },
                ),
            }
            this.delivering.set(None);
            this.pump();
        });
    }

    /// Delivers note `key`'s `clip`, retrying by itself while the bridge
    /// is unreachable; its transcript then [arrives](Self::arrive).
    /// Discarding the note abandons the loop.
    async fn deliver(self: &Rc<Self>, key: u32, mut clip: PendingClip) {
        self.set_state(
            key,
            NoteState::Uploading(Progress {
                sent: 0,
                total: u64::try_from(clip.audio.len()).unwrap_or(u64::MAX),
            }),
        );
        let courier = courier();
        let mut attempt: u32 = 0;
        // The furthest offset the bridge has confirmed: an attempt that
        // moved it was worth making, so it restarts the retry clock.
        let furthest = Cell::new(0);
        let mut stalled_since = now().unix_millis();
        loop {
            let before = furthest.get();
            let result = courier
                .deliver(
                    &mut clip,
                    |delivery| match delivery {
                        Delivery::Uploading(progress) => {
                            furthest.set(furthest.get().max(progress.sent));
                            self.set_state(key, NoteState::Uploading(progress));
                        }
                        Delivery::Transcribing => self.set_state(key, NoteState::Transcribing),
                    },
                    wait,
                    || !self.has(key),
                )
                .await;
            // `deliver` moves `clip` to a fresh id if the bridge refused
            // its id: the note follows it.
            let id = clip.meta.audio_id.clone();
            self.update(key, |note| note.id = id);
            if !self.has(key) {
                // Discarded: forgotten under the id it ended up with.
                if let Err(error) = courier.forget(&clip.meta.audio_id).await {
                    warn(&error.to_string());
                }
                return;
            }
            let error = match result {
                Ok(done) => {
                    let text = match done.stage() {
                        Stage::Transcribed(text) => text.to_owned(),
                        Stage::Recorded => String::new(),
                    };
                    return self.arrive(key, text);
                }
                Err(error) => error,
            };
            let at = now().unix_millis();
            if furthest.get() > before {
                attempt = 0;
                stalled_since = at;
            }
            let token_rejected = host().is_some_and(|h| h.token_rejected());
            if !retries_by_itself(&error, token_rejected)
                || at.saturating_sub(stalled_since) >= AUTO_RETRY_MS
            {
                let state = give_up(&courier, &clip, &error).await;
                return self.set_state(key, state);
            }
            self.set_state(
                key,
                NoteState::Waiting {
                    message: waiting_message(&error),
                },
            );
            let delay = RETRY_DELAYS_MS[(attempt as usize).min(RETRY_DELAYS_MS.len() - 1)];
            attempt += 1;
            if !self.pause(delay, key).await {
                return;
            }
        }
    }

    /// Note `key`'s transcript is in: sent (the note is then forgotten),
    /// or shown for review. It is sent only if its take sends and no older
    /// note waits for review ahead of it, so prompts keep their order. One
    /// no screen took (the user left meanwhile) waits as a card for their
    /// return.
    fn arrive(self: &Rc<Self>, key: u32, text: String) {
        let Some(note) = self.note(key) else {
            return;
        };
        let first = self.notes.borrow().first().map(|n| n.key) == Some(key);
        match Arrival::of(text, note.send && first, note.stopped) {
            Arrival::Review(state) => self.set_state(key, state),
            Arrival::Empty => {
                if let Some(host) = host() {
                    host.toast(NOTHING_HEARD);
                }
                self.finish(key);
            }
            Arrival::Send(text) => {
                if host().is_some_and(|host| host.take_text(&self.key, &text, true)) {
                    self.finish(key);
                } else {
                    self.set_state(key, NoteState::Ready { text, note: None });
                }
            }
        }
    }

    /// Waits `ms`, or until "Retry now" (also one tapped during the attempt
    /// that just failed). False if note `key` was discarded meanwhile.
    async fn pause(&self, ms: i32, key: u32) -> bool {
        let mut left = ms;
        while left > 0 && !self.wake.replace(false) {
            if !self.has(key) {
                return false;
            }
            sleep(WAKE_CHECK_MS.min(left)).await;
            left -= WAKE_CHECK_MS;
        }
        self.has(key)
    }

    /// Retries every parked note (the network is back).
    fn retry_parked(self: &Rc<Self>) {
        let parked: Vec<u32> = self
            .notes
            .borrow()
            .iter()
            .filter(|note| matches!(note.state, NoteState::Failed { parked: true, .. }))
            .map(|note| note.key)
            .collect();
        for key in parked {
            self.retry(key);
        }
    }

    /// "Retry" on note `key`: cut its wait short if it is retrying by
    /// itself, or queue it again if it failed. Its stored failure goes
    /// first, so a reload meanwhile delivers it too.
    pub fn retry(self: &Rc<Self>, key: u32) {
        if self.delivering.get() == Some(key) {
            self.wake.set(true);
            return;
        }
        let Some(Note {
            id,
            state: NoteState::Failed { .. },
            ..
        }) = self.note(key)
        else {
            return;
        };
        self.set_state(key, NoteState::Queued);
        let this = self.clone();
        spawn_local(async move {
            let courier = courier();
            match courier.load(&id).await {
                Ok(Some(mut clip)) => {
                    if let Err(error) = courier.clear_failure(&mut clip).await {
                        warn(&error.to_string());
                    }
                }
                Ok(None) => return this.drop_note(key),
                Err(error) => warn(&error.to_string()),
            }
            this.pump();
        });
    }

    /// Takes note `key` off the strip without touching storage.
    fn drop_note(&self, key: u32) {
        self.notes.borrow_mut().retain(|note| note.key != key);
        self.redraw();
    }

    /// Note `key`'s transcript was used or sent, or the note discarded:
    /// abandon any delivery of it, forget it, then deliver the next one.
    pub fn finish(self: &Rc<Self>, key: u32) {
        let Some(note) = self.note(key) else {
            return;
        };
        self.drop_note(key);
        let this = self.clone();
        spawn_local(async move {
            if let Err(error) = courier().forget(&note.id).await {
                warn(&error.to_string());
            }
            this.pump();
        });
    }

    /// Clears a failure that left nothing stored.
    pub fn dismiss(&self) {
        if self.take_is(|t| matches!(t, Take::Failed(_))) {
            self.set_take(Take::Idle);
        }
    }
}

/// The failure a delivery ends in once it stops retrying after `error`.
/// A parked one (see [`parks`]) is not stored as a failure, so the next
/// visit (or the network coming back) delivers the clip again; any other
/// is remembered, so the next visit doesn't send it again by itself.
async fn give_up(courier: &BrowserCourier, clip: &PendingClip, error: &DeliverError) -> NoteState {
    let parked = parks(error);
    let message = if parked {
        parked_message(error)
    } else {
        let message = deliver_message(error);
        if let Err(error) = courier.record_failure(clip, message.clone()).await {
            warn(&error.to_string());
        }
        message
    };
    NoteState::Failed { message, parked }
}

/// Whether a take started (or retried by the user) now sends its
/// transcript without review.
fn sends() -> bool {
    !host().is_some_and(|host| host.review_dictation())
}

fn warn(message: &str) {
    web_sys::console::warn_1(&message.into());
}

/// People-facing text for a recording that could not start or finish.
#[must_use]
pub fn capture_message(error: &CaptureError) -> String {
    match error {
        CaptureError::Denied => {
            "The microphone is blocked. Allow it in the browser's site settings.".to_owned()
        }
        CaptureError::AppDenied => {
            "The microphone is blocked. Allow it for Passporter in Android's settings.".to_owned()
        }
        CaptureError::NoDevice => "No microphone found.".to_owned(),
        CaptureError::Unsupported => "This browser can't record audio here.".to_owned(),
        CaptureError::Empty => "Nothing was recorded. Try again.".to_owned(),
        CaptureError::Busy => "Already recording.".to_owned(),
        CaptureError::Failed(why) => format!("Recording failed: {why}."),
    }
}

/// Whether a take that would `send` still sends after stopping for
/// `reason`. The length cap sends what was said (the user is still
/// talking to the agent); an interruption (the screen turned off, the app
/// went to the background, the microphone went away) may have cut a
/// thought short, so it waits for review instead.
#[must_use]
pub const fn sends_after(reason: StopReason, send: bool) -> bool {
    send && !matches!(reason, StopReason::Interrupted | StopReason::Hidden)
}

/// The note on the transcript card of a take that stopped for `reason`.
/// A hidden page gets one there rather than a toast: the toast would come
/// and go while the screen is off, and the card waits for the user.
#[must_use]
pub const fn card_note(reason: StopReason) -> Option<&'static str> {
    match reason {
        StopReason::Hidden => Some(SCREEN_OFF),
        StopReason::User | StopReason::Silence | StopReason::TooLong | StopReason::Interrupted => {
            None
        }
    }
}

/// What to tell the user about a recording that stopped by itself for a
/// reason they may not expect; `send` when it is being sent. A pause or
/// their own stop needs no note.
#[must_use]
pub const fn stop_note(reason: StopReason, send: bool) -> Option<&'static str> {
    match (reason, send) {
        (StopReason::TooLong, true) => {
            Some("Recording stopped at the length limit. Sending what you said.")
        }
        (StopReason::TooLong, false) => Some("Recording stopped at the length limit."),
        (StopReason::Interrupted, _) => {
            Some("Recording stopped: the microphone went away, or a call took it.")
        }
        // Said on the transcript card (`card_note`).
        (StopReason::User | StopReason::Silence | StopReason::Hidden, _) => None,
    }
}

/// People-facing text for a delivery that stopped short. The clip is
/// always still on the phone when this shows.
#[must_use]
pub fn deliver_message(error: &DeliverError) -> String {
    match error {
        DeliverError::Upload(UploadError::Api(ApiError::Unauthorized)) => {
            "The bridge didn't accept this device. Reconnect in Settings, then retry.".to_owned()
        }
        DeliverError::Upload(UploadError::Refused(why)) => why.clone(),
        DeliverError::Upload(UploadError::IdTaken | UploadError::Mismatch) => {
            "The bridge couldn't take this recording.".to_owned()
        }
        DeliverError::Upload(error) if error.is_retryable() => {
            "Can't reach the bridge. The recording is saved on this phone.".to_owned()
        }
        DeliverError::Upload(error) => format!("Upload failed: {error}."),
        DeliverError::Store(error) => format!("Couldn't save on this phone: {}.", error.0),
        DeliverError::Transcription => "The bridge couldn't transcribe this recording.".to_owned(),
        DeliverError::TooLong => "This recording is longer than the bridge allows.".to_owned(),
        DeliverError::Slow => "The bridge is still transcribing.".to_owned(),
        DeliverError::Cancelled => "The recording was discarded.".to_owned(),
    }
}

/// Whether a delivery that ran out of time retrying by itself after
/// `error` waits to be tried again (the bridge was unreachable or still
/// transcribing), rather than failing until the user acts.
const fn parks(error: &DeliverError) -> bool {
    match error {
        DeliverError::Slow => true,
        DeliverError::Upload(UploadError::Api(ApiError::Unauthorized))
        | DeliverError::Store(_)
        | DeliverError::Transcription
        | DeliverError::TooLong
        | DeliverError::Cancelled => false,
        DeliverError::Upload(error) => error.is_retryable(),
    }
}

/// What the strip says once a delivery stops retrying by itself after
/// `error` (see [`parks`]): the clip is kept and will be tried again.
#[must_use]
pub fn parked_message(error: &DeliverError) -> String {
    format!("{} {RETRIES_LATER}", deliver_message(error))
}

/// What the strip says while it retries by itself after `error`.
#[must_use]
pub fn waiting_message(error: &DeliverError) -> String {
    match error {
        // The page renews its token on a 401 (a restarted bridge forgets
        // them); a pasted token that was revoked ends in
        // `deliver_message`'s advice once the retries run out.
        DeliverError::Upload(UploadError::Api(ApiError::Unauthorized)) => {
            "Signing in to the bridge again…".to_owned()
        }
        error => deliver_message(error),
    }
}

/// Whether a failed delivery should be retried without asking: when the
/// bridge could not be reached, is still working, or rejected a token the
/// page is renewing. `token_rejected` is the page's verdict on a 401: a
/// pasted token that was refused is dropped, not renewed, so retrying
/// can't help.
const fn retries_by_itself(error: &DeliverError, token_rejected: bool) -> bool {
    match error {
        DeliverError::Upload(UploadError::Api(ApiError::Unauthorized)) => !token_rejected,
        DeliverError::Slow => true,
        DeliverError::Upload(error) => error.is_retryable(),
        DeliverError::Store(_)
        | DeliverError::Transcription
        | DeliverError::TooLong
        | DeliverError::Cancelled => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unexpected_stops_get_a_note() {
        for send in [true, false] {
            assert!(stop_note(StopReason::TooLong, send).is_some());
            assert!(stop_note(StopReason::Interrupted, send).is_some());
            assert_eq!(stop_note(StopReason::User, send), None);
            assert_eq!(stop_note(StopReason::Silence, send), None);
            assert_eq!(stop_note(StopReason::Hidden, send), None, "on the card");
        }
        assert!(stop_note(StopReason::TooLong, true).is_some_and(|n| n.contains("Sending")));
    }

    #[test]
    fn the_length_cap_sends_but_an_interruption_waits_for_review() {
        assert!(sends_after(StopReason::User, true));
        assert!(sends_after(StopReason::TooLong, true));
        assert!(!sends_after(StopReason::Interrupted, true));
        assert!(!sends_after(StopReason::Hidden, true));
        assert!(!sends_after(StopReason::User, false), "review stays review");
    }

    #[test]
    fn a_take_cut_short_by_the_screen_turning_off_says_so_on_its_card() {
        assert_eq!(card_note(StopReason::Hidden), Some(SCREEN_OFF));
        assert!(SCREEN_OFF.contains("screen turned off"));
        for reason in [
            StopReason::User,
            StopReason::Silence,
            StopReason::TooLong,
            StopReason::Interrupted,
        ] {
            assert_eq!(card_note(reason), None);
        }
    }

    #[test]
    fn messages_say_the_recording_is_safe_when_offline() {
        let offline = DeliverError::Upload(UploadError::Api(ApiError::Transport("x".into())));
        assert!(deliver_message(&offline).contains("saved on this phone"));
        assert!(retries_by_itself(&offline, false));
        assert!(!retries_by_itself(&DeliverError::Transcription, false));
        let refused = DeliverError::Upload(UploadError::Refused("Too big.".into()));
        assert_eq!(deliver_message(&refused), "Too big.");
        assert!(!retries_by_itself(&refused, false));
        let unauthorized = DeliverError::Upload(UploadError::Api(ApiError::Unauthorized));
        assert!(
            retries_by_itself(&unauthorized, false),
            "the page renews its token"
        );
        assert!(
            !retries_by_itself(&unauthorized, true),
            "a refused pasted token is not renewed"
        );
        assert!(waiting_message(&unauthorized).contains("Signing in"));
        assert!(deliver_message(&unauthorized).contains("Settings"));
        assert!(!retries_by_itself(&DeliverError::Cancelled, false));
        assert!(capture_message(&CaptureError::Denied).contains("site settings"));
    }

    #[test]
    fn only_an_unreachable_or_busy_bridge_parks_a_clip() {
        let offline = DeliverError::Upload(UploadError::Api(ApiError::Transport("x".into())));
        assert!(parks(&offline));
        assert!(parks(&DeliverError::Slow));
        assert!(parks(&DeliverError::Upload(UploadError::Gone)));
        let message = parked_message(&offline);
        assert!(message.contains("saved on this phone"), "{message}");
        assert!(message.contains("back online"), "{message}");
        for stays_failed in [
            DeliverError::Upload(UploadError::Api(ApiError::Unauthorized)),
            DeliverError::Upload(UploadError::Refused("Too big.".into())),
            DeliverError::Transcription,
            DeliverError::TooLong,
            DeliverError::Cancelled,
        ] {
            assert!(!parks(&stays_failed), "{stays_failed:?}");
        }
    }
}
