//! The dictation WASM's side of dictation (design §6.3, §7): one recorder
//! and one clip on its way to a transcript per agent, kept for the whole
//! page session, so a recording that is uploading when the user swipes to
//! another agent is still there when they come back.
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
//! dictation before sending" on, or for a clip left from an earlier visit
//! (the user wasn't there to see it go), it shows as a card to edit and
//! "Use" instead.

use core::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use protocol::{AgentId, AudioId, Timestamp};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::spawn_local;

use super::browser::{FetchAudio, IdbClips, MediaCapture};
use super::clip::{PendingClip, Stage};
use super::courier::{Courier, DeliverError, Delivery};
use super::panel::{Act, Panel};
use super::phase::{Arrival, NOTHING_HEARD, Phase, SCREEN_OFF, Strip};
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
/// How long a clip may stay on the phone, in ms: the bridge's default
/// record retention (seven days), after which its transcript is gone too.
const CLIP_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
/// Unattended retries in a row that make no upload progress, before a
/// delivery waits for the user.
const MAX_AUTO_RETRIES: u32 = 12;
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

/// Hands over the page's [`Host`]. Called once, right after loading.
pub fn init(host: Host) {
    HOST.set(Some(host));
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
                    phase: RefCell::new(Phase::Idle),
                    phone: Dictaphone::new(MediaCapture::default(), RecorderConfig::default()),
                    gesture: Cell::new(MicGesture::default()),
                    delivering: Cell::new(None),
                    generation: Cell::new(0),
                    wake: Cell::new(false),
                    takes: Cell::new(0),
                    send: Cell::new(true),
                    awake: Cell::new(false),
                })
            })
            .clone()
    })
}

/// One agent's dictation: the recorder and the clip on its way to a
/// transcript.
pub struct Dictation {
    /// Itself, for the strip's buttons.
    this: Weak<Self>,
    /// The strip, while a screen shows it.
    panel: RefCell<Option<Panel>>,
    agent: AgentId,
    /// `agent`, as the page knows it.
    key: String,
    phase: RefCell<Phase>,
    phone: Dictaphone<MediaCapture>,
    gesture: Cell<MicGesture>,
    /// The generation of the running delivery loop, if any.
    delivering: Cell<Option<u32>>,
    /// Bumped to abandon a delivery loop (its clip was discarded).
    generation: Cell<u32>,
    /// "Retry now": cut the current wait short.
    wake: Cell<bool>,
    /// Counts recordings started, to tell a late answer from an old take.
    takes: Cell<u32>,
    /// Whether the current take sends its transcript without review.
    send: Cell<bool>,
    /// Whether this agent's take is keeping the screen on ([`wake`]).
    awake: Cell<bool>,
}

impl core::fmt::Debug for Dictation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dictation")
            .field("agent", &self.agent)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

impl Dictation {
    fn phase(&self) -> Phase {
        self.phase.borrow().clone()
    }

    fn phase_is(&self, check: impl FnOnce(&Phase) -> bool) -> bool {
        check(&self.phase.borrow())
    }

    /// Every phase change goes through here, so the screen is kept on for
    /// exactly as long as a take is under way, whichever way it ends.
    fn set_phase(&self, phase: Phase) {
        let awake = phase.keeps_awake();
        if self.awake.replace(awake) != awake {
            wake::keep_on(awake);
        }
        if let Some(host) = host() {
            host.mic(&self.key, phase.mic().code());
        }
        if let Some(panel) = self.panel.borrow_mut().as_mut() {
            panel.draw(&Strip::of(&phase, self.send.get()));
        }
        *self.phase.borrow_mut() = phase;
    }

    /// Shows the strip in `root` (a screen mounting), in place of any
    /// other element.
    pub fn mount(&self, root: web_sys::Element) {
        let mut panel = Panel::new(root, self.this.clone());
        if let Some(panel) = &mut panel {
            panel.draw(&Strip::of(&self.phase.borrow(), self.send.get()));
        }
        *self.panel.borrow_mut() = panel;
        if let Some(host) = host() {
            host.mic(&self.key, self.phase.borrow().mic().code());
        }
    }

    /// Stops drawing into `root` (its screen went away). A newer screen's
    /// element stays.
    pub fn unmount(&self, root: &web_sys::Element) {
        let mut panel = self.panel.borrow_mut();
        if panel.as_ref().is_some_and(|p| p.is_in(root)) {
            *panel = None;
        }
    }

    /// A button on the strip.
    pub fn act(self: &Rc<Self>, act: Act) {
        let id = match &*self.phase.borrow() {
            Phase::Waiting { id, .. } | Phase::Ready { id, .. } => Some(id.clone()),
            Phase::Failed { id, .. } => id.clone(),
            _ => None,
        };
        match (act, id) {
            (Act::Stop, _) => self.stop(),
            (Act::Cancel, _) => self.cancel(),
            (Act::Retry, _) => self.retry(),
            (Act::Dismiss, _) => self.dismiss(),
            (Act::Discard, Some(id)) => self.finish(id),
            (Act::Use, Some(id)) => {
                let text = self
                    .panel
                    .borrow()
                    .as_ref()
                    .and_then(Panel::transcript)
                    .unwrap_or_default();
                let text = text.trim();
                if text.is_empty() {
                    return;
                }
                if let Some(host) = host() {
                    host.take_text(&self.key, text, false);
                }
                self.finish(id);
            }
            (Act::Rerecord, Some(id)) => {
                self.finish(id);
                self.start();
            }
            (Act::Discard | Act::Use | Act::Rerecord, None) => {}
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

    /// Starts recording, if the strip is free. Call it from a user gesture:
    /// the meter's audio can only start inside one.
    pub fn start(self: &Rc<Self>) {
        if !self.phase_is(Phase::mic_enabled) || self.phone.is_recording() {
            return;
        }
        self.phone.capture().prime();
        platform::haptic_tick();
        // Sending on stop means tap to send: a pause mid-thought must not
        // end the take. Reviewing keeps silence auto-stop.
        self.send.set(sends());
        self.phone.auto_stop(!self.send.get());
        self.set_phase(Phase::Opening);
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
            let opening = this.takes.get() == take && this.phase_is(|p| *p == Phase::Opening);
            match result {
                Ok(()) if opening => this.set_phase(Phase::Recording),
                Err(error) if opening => this.set_phase(Phase::Failed {
                    id: None,
                    message: capture_message(&error),
                }),
                Ok(()) | Err(_) => {}
            }
        });
    }

    /// Stops recording; the clip is then saved and delivered. While the
    /// microphone is still opening there is nothing to keep: it cancels.
    pub fn stop(&self) {
        self.gesture.set(MicGesture::default());
        self.phone.hold(false);
        if self.phone.stop() == Halt::Cancelled {
            self.set_phase(Phase::Idle);
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
                if self.phase_is(|p| *p == Phase::Opening) {
                    self.set_phase(Phase::Recording);
                }
                if let Some(panel) = self.panel.borrow().as_ref() {
                    panel.meter(&bars, elapsed_ms);
                }
            }
            Update::Stopping(_) => {
                self.release_mic();
                platform::haptic_tick();
                self.set_phase(Phase::Saving);
            }
            Update::Finished(recording) => {
                self.release_mic();
                platform::haptic_tick();
                self.set_phase(Phase::Saving);
                let send = sends_after(recording.stopped_by, self.send.get());
                let note = card_note(recording.stopped_by);
                self.send.set(send);
                if let (Some(note), Some(host)) = (stop_note(recording.stopped_by, send), host()) {
                    host.toast(note);
                }
                let this = self.clone();
                spawn_local(async move { this.keep(recording, note).await });
            }
            Update::Failed(error) => {
                self.release_mic();
                self.set_phase(Phase::Failed {
                    id: None,
                    message: capture_message(&error),
                });
            }
        }
    }

    /// The take is over: forget any press and hold. The button may have
    /// been disabled under a held finger, so its `pointerup` can be lost.
    fn release_mic(&self) {
        self.gesture.set(MicGesture::default());
        self.phone.hold(false);
    }

    /// Stores the recording, then delivers it; a review card shows `note`.
    async fn keep(self: &Rc<Self>, recording: Recording, note: Option<&'static str>) {
        let Some(id) = new_audio_id() else {
            self.set_phase(Phase::Failed {
                id: None,
                message: "This browser can't make a recording id.".to_owned(),
            });
            return;
        };
        match courier()
            .keep(self.agent.clone(), id, recording, now())
            .await
        {
            Ok(clip) => {
                if let Some(host) = host() {
                    host.saved();
                }
                self.deliver(clip, self.send.get(), note).await;
            }
            Err(error) => self.set_phase(Phase::Failed {
                id: None,
                message: format!("Couldn't save the recording on this phone: {}.", error.0),
            }),
        }
    }

    /// Whether a delivery loop for the current clip is running. A loop
    /// left behind by a discard belongs to an older generation and doesn't
    /// count: it stops at its next check.
    fn is_delivering(&self) -> bool {
        self.delivering.get() == Some(self.generation.get())
    }

    /// Whether the clip that `generation` was delivering is still wanted.
    fn is_current(&self, generation: u32) -> bool {
        self.generation.get() == generation
    }

    fn is_free(&self) -> bool {
        self.phase_is(|p| *p == Phase::Idle) && !self.is_delivering() && !self.phone.is_recording()
    }

    /// Picks up where a previous screen or page load left off: the oldest
    /// stored clip for this agent, if the strip is free. The first call in
    /// a page session also deletes clips (of any agent) past the bridge's
    /// retention.
    pub fn resume(self: &Rc<Self>) {
        if !self.is_free() {
            return;
        }
        let sweep_before = (!SWEPT.replace(true)).then(|| {
            Timestamp::from_unix_millis(now().unix_millis().saturating_sub(CLIP_RETENTION_MS))
        });
        let this = self.clone();
        spawn_local(async move {
            let courier = courier();
            let waiting = match courier.waiting(&this.agent, sweep_before).await {
                Ok(waiting) => waiting,
                Err(error) => {
                    warn(&error.to_string());
                    return;
                }
            };
            let Some(meta) = waiting.into_iter().next() else {
                return;
            };
            if !this.is_free() {
                return;
            }
            // A transcript needs no audio and no network; a clip that
            // failed for good waits for the user's Retry or Discard.
            match (meta.transcript, meta.failure) {
                (Some(text), _) => this.set_phase(Phase::Ready {
                    id: meta.audio_id,
                    text,
                    note: None,
                }),
                (None, Some(message)) => this.set_phase(Phase::Failed {
                    id: Some(meta.audio_id),
                    message,
                }),
                (None, None) => {
                    let Ok(Some(clip)) = courier.load(&meta.audio_id).await else {
                        return;
                    };
                    // Something may have started meanwhile. The user
                    // wasn't there when it was said: they review it.
                    if this.is_free() {
                        this.deliver(clip, false, None).await;
                    }
                }
            }
        });
    }

    /// Delivers `clip`, retrying by itself while the bridge is unreachable;
    /// its transcript is then sent if `send`, else shown for review (with
    /// `note`). Discarding the clip (a new generation) abandons the loop.
    async fn deliver(
        self: &Rc<Self>,
        mut clip: PendingClip,
        send: bool,
        note: Option<&'static str>,
    ) {
        if self.is_delivering() {
            return;
        }
        let generation = self.generation.get();
        self.delivering.set(Some(generation));
        self.wake.set(false);
        // Busy before the first request: the mic stays off until this
        // clip is through, so a new recording can't land on top of it.
        self.set_phase(Phase::Uploading(Progress {
            sent: 0,
            total: u64::try_from(clip.audio.len()).unwrap_or(u64::MAX),
        }));
        let courier = courier();
        let mut attempt: u32 = 0;
        // The furthest offset the bridge has confirmed: an attempt that
        // moved it was worth making, so it doesn't count against retries.
        let furthest = Cell::new(0);
        let outcome = loop {
            let before = furthest.get();
            let result = courier
                .deliver(
                    &mut clip,
                    |delivery| {
                        if !self.is_current(generation) {
                            return;
                        }
                        match delivery {
                            Delivery::Uploading(progress) => {
                                furthest.set(furthest.get().max(progress.sent));
                                self.set_phase(Phase::Uploading(progress));
                            }
                            Delivery::Transcribing => self.set_phase(Phase::Transcribing),
                        }
                    },
                    wait,
                    || !self.is_current(generation),
                )
                .await;
            // `deliver` moves `clip` to a fresh id if the bridge refused
            // its id, so this is always the id it is stored under.
            let id = clip.meta.audio_id.clone();
            match result {
                Ok(done) => {
                    let text = match done.stage() {
                        Stage::Transcribed(text) => text.to_owned(),
                        Stage::Recorded => String::new(),
                    };
                    break Phase::Ready { id, text, note };
                }
                Err(_) if !self.is_current(generation) => break Phase::Idle,
                Err(error)
                    if retries_by_itself(&error, host().is_some_and(|h| h.token_rejected()))
                        && (attempt < MAX_AUTO_RETRIES || furthest.get() > before) =>
                {
                    if furthest.get() > before {
                        attempt = 0;
                    }
                    self.set_phase(Phase::Waiting {
                        id,
                        message: waiting_message(&error),
                    });
                    let delay = RETRY_DELAYS_MS[(attempt as usize).min(RETRY_DELAYS_MS.len() - 1)];
                    attempt += 1;
                    if !self.pause(delay, generation).await {
                        break Phase::Idle;
                    }
                }
                Err(error) => {
                    let message = deliver_message(&error);
                    // Remembered, so the next visit doesn't send it again
                    // by itself.
                    if let Err(error) = courier.record_failure(&clip, message.clone()).await {
                        warn(&error.to_string());
                    }
                    break Phase::Failed {
                        id: Some(id),
                        message,
                    };
                }
            }
        };
        if self.delivering.get() == Some(generation) {
            self.delivering.set(None);
        }
        if !self.is_current(generation) {
            // Cancelled or discarded: forgotten under the id it ended up
            // with (an upload can move it to a fresh one).
            if let Err(error) = courier.forget(&clip.meta.audio_id).await {
                warn(&error.to_string());
            }
            return;
        }
        match outcome {
            Phase::Ready { id, text, note } => self.arrive(Arrival::of(id, text, send, note)),
            outcome => self.set_phase(outcome),
        }
    }

    /// A transcript is in: sent (the clip is then forgotten), or shown for
    /// review. One no screen took (the user left meanwhile) waits as a
    /// card for their return.
    fn arrive(self: &Rc<Self>, arrival: Arrival) {
        match arrival {
            Arrival::Review(phase) => self.set_phase(phase),
            Arrival::Empty { id } => {
                if let Some(host) = host() {
                    host.toast(NOTHING_HEARD);
                }
                self.finish(id);
            }
            Arrival::Send { id, text } => {
                if host().is_some_and(|host| host.take_text(&self.key, &text, true)) {
                    self.finish(id);
                } else {
                    self.set_phase(Phase::Ready {
                        id,
                        text,
                        note: None,
                    });
                }
            }
        }
    }

    /// Waits `ms`, or until "Retry now" (also one tapped during the attempt
    /// that just failed). False if the clip was discarded meanwhile.
    async fn pause(&self, ms: i32, generation: u32) -> bool {
        let mut left = ms;
        while left > 0 && !self.wake.replace(false) {
            if !self.is_current(generation) {
                return false;
            }
            sleep(WAKE_CHECK_MS.min(left)).await;
            left -= WAKE_CHECK_MS;
        }
        self.is_current(generation)
    }

    /// "Retry": try the stored clip again now. Its stored failure goes
    /// first, so a reload while the retry waits keeps delivering it.
    pub fn retry(self: &Rc<Self>) {
        if self.is_delivering() {
            self.wake.set(true);
            return;
        }
        let Phase::Failed { id: Some(id), .. } = self.phase() else {
            return;
        };
        // A Discard while the clip loads wins: nothing is sent.
        let generation = self.generation.get();
        let this = self.clone();
        spawn_local(async move {
            let courier = courier();
            let loaded = courier.load(&id).await;
            if !this.is_current(generation) {
                return;
            }
            let mut clip = match loaded {
                Ok(Some(clip)) => clip,
                Ok(None) => return this.set_phase(Phase::Idle),
                Err(error) => {
                    return this.set_phase(Phase::Failed {
                        id: Some(id),
                        message: deliver_message(&DeliverError::Store(error)),
                    });
                }
            };
            if let Err(error) = courier.clear_failure(&mut clip).await {
                warn(&error.to_string());
            }
            if this.is_current(generation) {
                this.deliver(clip, sends(), None).await;
            }
        });
    }

    /// The transcript was used or sent, or the clip discarded: abandon any delivery
    /// of it, forget it, then move on to the next stored clip, if any.
    pub fn finish(self: &Rc<Self>, id: AudioId) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.set_phase(Phase::Idle);
        let this = self.clone();
        spawn_local(async move {
            if let Err(error) = courier().forget(&id).await {
                warn(&error.to_string());
            }
            this.resume();
        });
    }

    /// "Cancel" while the clip uploads or transcribes: it is discarded, and
    /// nothing is sent. The delivery loop forgets it at its next check.
    pub fn cancel(&self) {
        if self.is_delivering() && self.phase_is(Phase::can_cancel) {
            self.generation.set(self.generation.get().wrapping_add(1));
            self.set_phase(Phase::Idle);
        }
    }

    /// Clears a failure that left nothing stored.
    pub fn dismiss(&self) {
        if self.phase_is(|p| matches!(p, Phase::Failed { id: None, .. })) {
            self.set_phase(Phase::Idle);
        }
    }
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
        (StopReason::Interrupted, _) => Some("Recording stopped: the microphone went away."),
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
}
