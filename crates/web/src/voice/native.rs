//! Dictation through the Android app's native recorder (design §6.3).
//!
//! In a webview, `MediaRecorder` lives and dies with the page, and the PWA
//! stops a take when the page is hidden ([`super::browser`]). In the APK
//! the `recorder` plugin (`apps/android/plugins/recorder`) records instead:
//! in a foreground service, to app-private files written frame by frame,
//! so a take goes on with the screen off and survives the webview
//! reloading or the app being killed.
//!
//! The plugin only records; everything after is the PWA's, unchanged: the
//! finished audio (AAC) is read over IPC into the same [`Recording`] a
//! browser take makes, stored in `IndexedDB` under the plugin's id for it,
//! and only then released on the native side, so a take is always in at
//! least one durable place. It is then uploaded and transcribed by the
//! [`super::courier`], resumable and offline-tolerant as ever.
//!
//! - [`NativeCapture`] is the [`AudioCapture`] for one agent: it starts a
//!   take, polls its level for the meter, and turns the finished take into
//!   `Data` and `Ended`.
//! - [`adopt`] stores takes no page is waiting for (one finished while the
//!   page was gone, or recovered after the app was killed), so the agent's
//!   screen shows them on a card, never sent unseen.
//! - [`backend`] says whether the plugin is here at all; without it
//!   (Chrome, an old `WebView`) dictation uses `MediaRecorder`, unchanged.
//!
//! The plugin's answers are read tolerantly: the PWA updates with the
//! bridge, the plugin with the APK, so either may be newer. Calls go
//! through `window.__TAURI__.core.invoke` as in [`crate::pair::native`],
//! which keeps its own copy of these few lines: sharing them costs the
//! app's WASM, which sits at its size budget, about 600 bytes.

use core::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use js_sys::{Function, JSON, Object, Promise, Reflect};
use protocol::{AgentId, AudioId, Timestamp};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use super::clip::ClipStore;
use super::courier::Courier;
use super::recorder::{
    AudioCapture, CaptureError, CaptureEvent, CaptureSession, EventSink, Recording, StopReason,
};
use super::upload::AudioTransport;
use crate::platform::js_error_message;

/// The plugin's name.
const PLUGIN: &str = "recorder";
/// How often a running take is asked for its level, in ms.
const POLL_MS: i32 = 100;
/// Bytes asked for per `read` (the plugin answers at most 512 KiB).
const READ_BYTES: u64 = 256 * 1024;
/// The largest take read back: well over the bridge's 25 MiB clip cap, so
/// only a broken answer is refused.
const MAX_TAKE_BYTES: u64 = 64 * 1024 * 1024;
/// The plugin reports a frame's peak; speech's RMS sits about 10 dB under
/// its peak, which is what the meter and silence detection expect.
const PEAK_TO_RMS: f32 = 0.3;

/// How a take ended, as the plugin says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndedBy {
    /// Stopped: by the PWA, or from the notification.
    User,
    /// The length cap the PWA asked for.
    Cap,
    /// A call or another app took the audio.
    Focus,
    /// The microphone or the encoder failed.
    Error,
    /// The app was killed while recording; recovered at its next start.
    Interrupted,
    /// A reason this page predates.
    #[serde(other)]
    Unknown,
}

/// A finished take, as the plugin answers it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTake {
    /// The plugin's id for it, which the clip keeps.
    pub id: AudioId,
    /// What the PWA attached at start: the agent's id.
    pub tag: String,
    /// Length, from its audio frames.
    pub duration_ms: u64,
    /// Size of its audio.
    pub bytes: u64,
    /// Its media type (AAC).
    pub mime: String,
    /// How it ended.
    pub ended_by: EndedBy,
    /// When it ended, in ms since the epoch.
    pub ended_at: u64,
}

/// The running take.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTake {
    /// The plugin's id for it.
    pub id: AudioId,
    /// What the PWA attached at start.
    pub tag: String,
    /// Recorded so far.
    pub elapsed_ms: u64,
    /// The loudest sample since the last poll, `0.0..=1.0`.
    pub peak: f32,
}

/// What `status` answers.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStatus {
    /// The running take, if any.
    #[serde(default)]
    pub active: Option<ActiveTake>,
    /// Finished takes not yet released, oldest first.
    #[serde(default)]
    pub takes: Vec<NativeTake>,
}

#[derive(Deserialize)]
struct Started {
    id: AudioId,
}

#[derive(Deserialize)]
struct Chunk {
    data: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartArgs<'a> {
    tag: &'a str,
    max_ms: u64,
}

#[derive(Serialize)]
struct IdArgs<'a> {
    id: &'a AudioId,
}

#[derive(Serialize)]
struct ReadArgs<'a> {
    id: &'a AudioId,
    offset: u64,
    length: u64,
}

/// Which recorder dictation uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The browser's `MediaRecorder`.
    Media,
    /// The app's native recorder.
    Native,
}

/// The backend for a page `in_app` whose plugin answered `status` so: the
/// native recorder when it answers, else `MediaRecorder` (Chrome; an app
/// whose shell granted no plugins; a plugin that cannot answer).
#[must_use]
pub fn backend_for(in_app: bool, status: &Result<RecorderStatus, Failure>) -> Backend {
    match status {
        Ok(_) if in_app => Backend::Native,
        Ok(_) | Err(_) => Backend::Media,
    }
}

thread_local! {
    /// The backend, once asked.
    static BACKEND: Cell<Option<Backend>> = const { Cell::new(None) };
    /// Takes this page is recording (one, nearly always): [`adopt`] leaves
    /// them alone.
    static LIVE: RefCell<Vec<AudioId>> = const { RefCell::new(Vec::new()) };
    /// Starts on their way: the take they start is not known yet.
    static STARTING: Cell<u32> = const { Cell::new(0) };
}

/// The backend, if [`backend`] has already found out.
#[must_use]
pub fn known_backend() -> Option<Backend> {
    BACKEND.get()
}

/// Which recorder this page uses, asked once per page session.
pub async fn backend() -> Backend {
    if let Some(known) = BACKEND.get() {
        return known;
    }
    let in_app = invoke_fn().is_some();
    let chosen = if in_app {
        let status = status().await;
        if let Err(failure) = &status {
            web_sys::console::warn_1(&format!("native recorder: {}", failure.message).into());
        }
        backend_for(in_app, &status)
    } else {
        Backend::Media
    };
    BACKEND.set(Some(chosen));
    chosen
}

/// A refused call: `{message, code}` from the plugin, or a bare message
/// from Tauri (an ACL refusal: no plugin granted here).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Failure {
    /// What went wrong.
    pub message: String,
    /// The plugin's stable code (`RecorderException.code` in Kotlin);
    /// Tauri's own refusals have none.
    #[serde(default)]
    pub code: Option<String>,
}

impl Failure {
    fn broken(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: Some("badResponse".to_owned()),
        }
    }
}

/// `window.__TAURI__.core.invoke`, if this page runs in the app.
fn invoke_fn() -> Option<Function> {
    let tauri = Reflect::get(&js_sys::global(), &"__TAURI__".into()).ok()?;
    let core = Reflect::get(&tauri, &"core".into()).ok()?;
    Reflect::get(&core, &"invoke".into()).ok()?.dyn_into().ok()
}

/// Calls the plugin's `command` with `args` and reads its answer as `T`.
async fn call<T: DeserializeOwned>(command: &str, args: &Object) -> Result<T, Failure> {
    let invoke = invoke_fn().ok_or_else(|| Failure {
        message: "the app's native layer is gone".to_owned(),
        code: None,
    })?;
    let promise: Promise = invoke
        .call2(
            &JsValue::NULL,
            &format!("plugin:{PLUGIN}|{command}").into(),
            args,
        )
        .and_then(JsCast::dyn_into)
        .map_err(|e| failure(&e))?;
    let value = JsFuture::from(promise).await.map_err(|e| failure(&e))?;
    decode(&value).map_err(Failure::broken)
}

/// `value` as a call's arguments object, through its JSON text.
fn args<T: Serialize>(value: &T) -> Result<Object, Failure> {
    let text = serde_json::to_string(value).map_err(|e| Failure::broken(e.to_string()))?;
    JSON::parse(&text)
        .map_err(|e| Failure::broken(js_error_message(&e)))?
        .dyn_into()
        .map_err(|_| Failure::broken("arguments must be an object"))
}

/// `value` (a plugin answer) as `T`, through its JSON text.
fn decode<T: DeserializeOwned>(value: &JsValue) -> Result<T, String> {
    let text = if value.is_undefined() {
        "null".to_owned()
    } else {
        JSON::stringify(value)
            .map(String::from)
            .map_err(|e| js_error_message(&e))?
    };
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// A rejection as it came.
fn failure(error: &JsValue) -> Failure {
    decode::<Failure>(error).unwrap_or_else(|_| Failure {
        message: js_error_message(error),
        code: None,
    })
}

async fn status() -> Result<RecorderStatus, Failure> {
    call("status", &Object::new()).await
}

async fn stop(id: &AudioId) -> Result<NativeTake, Failure> {
    call("stop", &args(&IdArgs { id })?).await
}

/// Deletes the native copy of take `id`, once its clip is stored. Best
/// effort: a take left behind is stored again (under the same id) by the
/// next [`adopt`], and swept by the plugin after seven days.
pub async fn release(id: &AudioId) {
    let released = match args(&IdArgs { id }) {
        Ok(args) => call::<()>("release", &args).await,
        Err(failure) => Err(failure),
    };
    if let Err(failure) = released {
        web_sys::console::warn_1(&format!("native recorder: release: {}", failure.message).into());
    }
}

/// The whole audio of `take`, read in chunks.
async fn read_all(take: &NativeTake) -> Result<Vec<u8>, Failure> {
    let broken = Failure::broken;
    if take.bytes == 0 || take.bytes > MAX_TAKE_BYTES {
        return Err(broken("a take of an impossible size"));
    }
    let mut audio = Vec::with_capacity(usize::try_from(take.bytes).unwrap_or(0));
    let mut offset = 0;
    while offset < take.bytes {
        let length = READ_BYTES.min(take.bytes - offset);
        let args = args(&ReadArgs {
            id: &take.id,
            offset,
            length,
        })?;
        let chunk: Chunk = call("read", &args).await?;
        let bytes =
            decode_base64(&chunk.data).ok_or_else(|| broken("a chunk that is not base64"))?;
        if bytes.is_empty() || bytes.len() as u64 > length {
            return Err(broken("a chunk of the wrong size"));
        }
        offset += bytes.len() as u64;
        audio.extend_from_slice(&bytes);
    }
    Ok(audio)
}

/// The [`CaptureError`] for a plugin's refusal.
#[must_use]
pub fn capture_error(failure: &Failure) -> CaptureError {
    match failure.code.as_deref() {
        Some("micDenied") => CaptureError::AppDenied,
        Some("busy") => CaptureError::Busy,
        Some("micBusy") => {
            CaptureError::Failed("the microphone is in use by a call or another app".to_owned())
        }
        Some("empty") => CaptureError::Empty,
        _ => CaptureError::Failed(failure.message.clone()),
    }
}

/// What a poll of `status` says about the take `id`.
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    /// Still recording.
    Running(ActiveTake),
    /// Ended by itself (the cap, a call, the notification's Stop).
    Ended(NativeTake),
    /// Gone without a take: nothing was recorded.
    Gone,
}

/// What `status` says about take `id`.
#[must_use]
pub fn seen(status: &RecorderStatus, id: &AudioId) -> Seen {
    if let Some(active) = status.active.as_ref().filter(|a| &a.id == id) {
        return Seen::Running(active.clone());
    }
    match status.takes.iter().find(|take| &take.id == id) {
        Some(take) => Seen::Ended(take.clone()),
        None => Seen::Gone,
    }
}

/// The meter reading for a poll of the take that started at `start_ms`.
#[must_use]
pub fn level(active: &ActiveTake, start_ms: f64) -> CaptureEvent {
    #[allow(clippy::cast_precision_loss)] // ms of a take: far below 2^52
    let at_ms = start_ms + active.elapsed_ms as f64;
    CaptureEvent::Level {
        at_ms,
        rms: (active.peak * PEAK_TO_RMS).clamp(0.0, 1.0),
    }
}

/// The events that end a take that started at `start_ms` once its audio
/// has been delivered: `Ended` at its real length, after a `Tick` at the
/// cap when the cap ended it, so the recorder reports the length cap
/// rather than an interruption.
#[must_use]
pub fn closing(take: &NativeTake, start_ms: f64, max_ms: u64) -> Vec<CaptureEvent> {
    #[allow(clippy::cast_precision_loss)] // ms of a take: far below 2^52
    let at = |ms: u64| start_ms + ms as f64;
    let mut events = Vec::with_capacity(2);
    if take.ended_by == EndedBy::Cap {
        events.push(CaptureEvent::Tick {
            at_ms: at(max_ms.max(take.duration_ms)),
        });
    }
    events.push(CaptureEvent::Ended {
        at_ms: at(take.duration_ms),
    });
    events
}

/// What [`adopt`] does with the plugin's takes.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// A running take no page here started (the page that did is gone):
    /// stop it, and store it with the rest.
    pub stop: Option<AudioId>,
    /// Finished takes no page here is waiting for.
    pub adopt: Vec<NativeTake>,
}

/// The [`Plan`] for `status`, leaving alone the takes in `live` and,
/// while a start is on its way (`starting`), a running take that may be
/// its.
#[must_use]
pub fn plan(status: &RecorderStatus, live: &[AudioId], starting: bool) -> Plan {
    let stop = status
        .active
        .as_ref()
        .filter(|active| !starting && !live.contains(&active.id))
        .map(|active| active.id.clone());
    let adopt = status
        .takes
        .iter()
        .filter(|take| !live.contains(&take.id))
        .cloned()
        .collect();
    Plan { stop, adopt }
}

/// `take` as a clip to store: its agent, the recording and when it was
/// recorded. `None` when its tag is not an agent id (it can't be shown).
#[must_use]
pub fn adopted(take: &NativeTake, audio: Vec<u8>) -> Option<(AgentId, Recording, Timestamp)> {
    let agent = take.tag.parse::<AgentId>().ok()?;
    let recording = Recording {
        bytes: audio,
        duration_ms: take.duration_ms.max(1),
        // Nobody was there when it ended: it waits for review.
        stopped_by: StopReason::Interrupted,
        id: Some(take.id.clone()),
    };
    Some((agent, recording, Timestamp::from_unix_millis(take.ended_at)))
}

/// Stores the takes no page here is waiting for, then releases them: a
/// take that finished while no page was there (the notification's Stop,
/// the cap), one recovered after the app was killed, and one still
/// running from a page that is gone (it is stopped first). Each is stored
/// as a clip for its agent, so that agent's screen shows it on a card.
/// Takes are stored under their own ids, so running this twice, or on two
/// screens at once, stores each take once.
pub async fn adopt<S: ClipStore, T: AudioTransport>(courier: &Courier<S, T>) {
    let Ok(status) = status().await else {
        return;
    };
    let live = LIVE.with_borrow(Clone::clone);
    let mut plan = plan(&status, &live, STARTING.get() > 0);
    if let Some(id) = plan.stop.take() {
        match stop(&id).await {
            Ok(take) => plan.adopt.push(take),
            Err(failure) => warn(&failure.message),
        }
    }
    for take in plan.adopt {
        let audio = match read_all(&take).await {
            Ok(audio) => audio,
            Err(failure) => {
                warn(&failure.message);
                continue;
            }
        };
        let Some((agent, recording, recorded_at)) = adopted(&take, audio) else {
            warn("a recording for no agent was dropped");
            release(&take.id).await;
            continue;
        };
        match courier
            .keep(agent, take.id.clone(), recording, recorded_at)
            .await
        {
            Ok(_) => release(&take.id).await,
            Err(error) => warn(&error.to_string()),
        }
    }
}

fn warn(message: &str) {
    web_sys::console::warn_1(&format!("native recorder: {message}").into());
}

/// The native recorder, for one agent's takes.
#[derive(Debug, Clone)]
pub struct NativeCapture {
    tag: String,
    max_ms: u64,
}

impl NativeCapture {
    /// Records takes tagged `tag` (the agent's id), each ending by itself
    /// after `max_ms`, even with the page hidden or gone.
    #[must_use]
    pub const fn new(tag: String, max_ms: u64) -> Self {
        Self { tag, max_ms }
    }
}

/// Counts a start on its way while it lives.
struct Starting;

impl Starting {
    fn new() -> Self {
        STARTING.set(STARTING.get() + 1);
        Self
    }
}

impl Drop for Starting {
    fn drop(&mut self) {
        STARTING.set(STARTING.get().saturating_sub(1));
    }
}

impl AudioCapture for NativeCapture {
    type Session = NativeSession;

    async fn open(&self, sink: EventSink) -> Result<NativeSession, CaptureError> {
        let starting = Starting::new();
        let args = args(&StartArgs {
            tag: &self.tag,
            max_ms: self.max_ms,
        })
        .map_err(|failure| capture_error(&failure))?;
        let started: Started = call("start", &args)
            .await
            .map_err(|failure| capture_error(&failure))?;
        LIVE.with_borrow_mut(|live| live.push(started.id.clone()));
        drop(starting);
        let start_ms = js_sys::Date::now();
        sink(CaptureEvent::Started { at_ms: start_ms });
        let session = Rc::new(Take {
            id: started.id,
            start_ms,
            max_ms: self.max_ms,
            sink,
            state: Cell::new(State::Running),
            polling: Cell::new(false),
            timer: Cell::new(None),
            tick: RefCell::new(None),
        });
        Take::poll_every(&session);
        Ok(NativeSession(session))
    }
}

/// Where a native take is, as this page sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Recording; polled for its level.
    Running,
    /// Ended; its audio is being read.
    Closing,
    /// Delivered, failed or cancelled.
    Done,
}

/// One native take this page started.
struct Take {
    id: AudioId,
    /// When it started, on the page's clock.
    start_ms: f64,
    max_ms: u64,
    sink: EventSink,
    state: Cell<State>,
    /// A poll is on its way.
    polling: Cell<bool>,
    timer: Cell<Option<i32>>,
    tick: RefCell<Option<Closure<dyn FnMut()>>>,
}

impl Take {
    /// Polls `status` every [`POLL_MS`] while the take runs.
    fn poll_every(this: &Rc<Self>) {
        let weak = Rc::downgrade(this);
        let tick = Closure::<dyn FnMut()>::new(move || Self::poll(&weak));
        let timer = web_sys::window().and_then(|window| {
            window
                .set_interval_with_callback_and_timeout_and_arguments_0(
                    tick.as_ref().unchecked_ref(),
                    POLL_MS,
                )
                .ok()
        });
        this.timer.set(timer);
        *this.tick.borrow_mut() = Some(tick);
    }

    fn poll(weak: &Weak<Self>) {
        let Some(this) = weak.upgrade() else {
            return;
        };
        if this.state.get() != State::Running || this.polling.replace(true) {
            return;
        }
        spawn_local(async move {
            let answer = status().await;
            this.polling.set(false);
            if this.state.get() != State::Running {
                return;
            }
            match answer.map(|status| seen(&status, &this.id)) {
                Ok(Seen::Running(active)) => (this.sink)(level(&active, this.start_ms)),
                Ok(Seen::Ended(take)) => this.close(Ok(Some(take))).await,
                Ok(Seen::Gone) => this.close(Ok(None)).await,
                // A busy moment: the next poll asks again.
                Err(failure) => warn(&failure.message),
            }
        });
    }

    /// Ends the take: delivers its audio and closing events (none for a
    /// take with nothing in it), or the failure. A take whose audio can't
    /// be read here stays native, for [`adopt`].
    async fn close(&self, ended: Result<Option<NativeTake>, Failure>) {
        if self.state.replace(State::Closing) != State::Running {
            return;
        }
        self.stop_polling();
        let events = match ended {
            Ok(Some(take)) => match read_all(&take).await {
                Ok(audio) => {
                    let mut events = vec![CaptureEvent::Data(audio)];
                    events.extend(closing(&take, self.start_ms, self.max_ms));
                    events
                }
                Err(failure) => vec![CaptureEvent::Failed(failure.message)],
            },
            // Nothing was recorded: the recorder reports it as empty.
            Ok(None) => vec![CaptureEvent::Ended {
                at_ms: js_sys::Date::now(),
            }],
            Err(failure) => vec![CaptureEvent::Failed(failure.message)],
        };
        self.state.set(State::Done);
        LIVE.with_borrow_mut(|live| live.retain(|id| id != &self.id));
        for event in events {
            (self.sink)(event);
        }
    }

    fn stop_polling(&self) {
        if let (Some(timer), Some(window)) = (self.timer.take(), web_sys::window()) {
            window.clear_interval_with_handle(timer);
        }
        self.tick.borrow_mut().take();
    }
}

/// A running native take.
#[derive(Clone)]
pub struct NativeSession(Rc<Take>);

impl core::fmt::Debug for NativeSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeSession")
            .field("id", &self.0.id)
            .finish_non_exhaustive()
    }
}

impl CaptureSession for NativeSession {
    fn stop(&self) {
        let take = self.0.clone();
        if take.state.get() != State::Running {
            return;
        }
        spawn_local(async move {
            let stopped = match stop(&take.id).await {
                Ok(ended) => Ok(Some(ended)),
                Err(failure) if matches!(failure.code.as_deref(), Some("empty" | "notFound")) => {
                    Ok(None)
                }
                Err(failure) => Err(failure),
            };
            take.close(stopped).await;
        });
    }

    fn cancel(&self) {
        let take = self.0.clone();
        if take.state.replace(State::Done) == State::Done {
            return;
        }
        take.stop_polling();
        LIVE.with_borrow_mut(|live| live.retain(|id| id != &take.id));
        spawn_local(async move {
            let cancelled = match args(&IdArgs { id: &take.id }) {
                Ok(args) => call::<()>("cancel", &args).await,
                Err(failure) => Err(failure),
            };
            if let Err(failure) = cancelled {
                warn(&failure.message);
            }
        });
    }

    fn take_id(&self) -> Option<AudioId> {
        Some(self.0.id.clone())
    }
}

/// Standard base64 with padding (how the plugin sends audio), or `None`.
#[must_use]
pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        Some(u32::from(value))
    }
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let (quads, _) = bytes.as_chunks::<4>();
    let last = quads.len().saturating_sub(1);
    for (index, quad) in quads.iter().enumerate() {
        let padding = quad.iter().rev().take_while(|&&b| b == b'=').count();
        if padding > 2 || (padding > 0 && index != last) {
            return None;
        }
        let mut word = 0;
        for &byte in &quad[..4 - padding] {
            word = word << 6 | sextet(byte)?;
        }
        word <<= 6 * padding;
        let [_, a, b, c] = word.to_be_bytes();
        out.extend_from_slice(&[a, b, c][..3 - padding]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";
    const OTHER: &str = "0f6b1a52-3c1e-4d7a-8f2b-5e9c7d4a1b30";

    /// The status the plugin's Kotlin test checks it writes byte for byte.
    const GOLDEN: &str = include_str!(
        "../../../../apps/android/plugins/recorder/android/src/test/resources/recorder-status-golden.json"
    );

    fn id(raw: &str) -> AudioId {
        AudioId::new(raw).unwrap()
    }

    fn take(raw: &str, ended_by: EndedBy) -> NativeTake {
        NativeTake {
            id: id(raw),
            tag: "w1:p1".to_owned(),
            duration_ms: 61_440,
            bytes: 122_880,
            mime: "audio/aac".to_owned(),
            ended_by,
            ended_at: 1_767_225_600_000,
        }
    }

    fn active(raw: &str) -> ActiveTake {
        ActiveTake {
            id: id(raw),
            tag: "w1:p1".to_owned(),
            elapsed_ms: 4_200,
            peak: 0.5,
        }
    }

    #[test]
    fn the_plugins_status_decodes() {
        let status: RecorderStatus = serde_json::from_str(GOLDEN).unwrap();
        assert_eq!(
            status,
            RecorderStatus {
                active: Some(active(OTHER)),
                takes: vec![take(ID, EndedBy::Interrupted)],
            }
        );
        let idle: RecorderStatus = serde_json::from_str(r#"{"takes":[]}"#).unwrap();
        assert_eq!(idle.active, None);
    }

    #[test]
    fn a_newer_plugin_still_decodes() {
        let json = format!(
            r#"{{"takes":[{{"id":"{ID}","tag":"w1:p1","durationMs":1,"bytes":1,"mime":"audio/aac","endedBy":"cosmic_rays","endedAt":1,"codec":"opus"}}],"paused":false}}"#
        );
        let status: RecorderStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status.takes[0].ended_by, EndedBy::Unknown);
        let bad = r#"{"takes":[{"id":"../x","tag":"","durationMs":1,"bytes":1,"mime":"","endedBy":"user","endedAt":1}]}"#;
        assert!(
            serde_json::from_str::<RecorderStatus>(bad).is_err(),
            "ids stay strict"
        );
    }

    #[test]
    fn the_native_recorder_is_used_only_when_it_answers() {
        let answered = Ok(RecorderStatus {
            active: None,
            takes: Vec::new(),
        });
        let refused = |code: Option<&str>| {
            Err(Failure {
                message: "no".to_owned(),
                code: code.map(str::to_owned),
            })
        };
        assert_eq!(backend_for(true, &answered), Backend::Native);
        assert_eq!(backend_for(false, &answered), Backend::Media, "Chrome");
        assert_eq!(
            backend_for(true, &refused(None)),
            Backend::Media,
            "the shell granted no plugins (an old WebView)"
        );
        assert_eq!(
            backend_for(true, &refused(Some("storage"))),
            Backend::Media,
            "a plugin that cannot answer"
        );
    }

    #[test]
    fn refusals_map_to_capture_errors() {
        let failure = |code: &str| Failure {
            message: "why".to_owned(),
            code: Some(code.to_owned()),
        };
        assert_eq!(
            capture_error(&failure("micDenied")),
            CaptureError::AppDenied
        );
        assert_eq!(capture_error(&failure("busy")), CaptureError::Busy);
        assert_eq!(capture_error(&failure("empty")), CaptureError::Empty);
        assert!(
            matches!(capture_error(&failure("micBusy")), CaptureError::Failed(m) if m.contains("call"))
        );
        assert_eq!(
            capture_error(&failure("storage")),
            CaptureError::Failed("why".to_owned())
        );
    }

    #[test]
    fn a_poll_follows_its_own_take() {
        let mut status = RecorderStatus {
            active: Some(active(ID)),
            takes: vec![take(OTHER, EndedBy::User)],
        };
        assert_eq!(seen(&status, &id(ID)), Seen::Running(active(ID)));
        status.active = None;
        assert_eq!(seen(&status, &id(ID)), Seen::Gone);
        status.takes.push(take(ID, EndedBy::Cap));
        assert_eq!(seen(&status, &id(ID)), Seen::Ended(take(ID, EndedBy::Cap)));
        status.active = Some(active(OTHER));
        assert_eq!(
            seen(&status, &id(ID)),
            Seen::Ended(take(ID, EndedBy::Cap)),
            "another take running is not this one"
        );
    }

    #[test]
    fn levels_run_on_the_takes_own_clock() {
        let CaptureEvent::Level { at_ms, rms } = level(&active(ID), 1_000.0) else {
            panic!("a level")
        };
        assert!((at_ms - 5_200.0).abs() < f64::EPSILON);
        assert!((rms - 0.15).abs() < 1e-6);
        let loud = ActiveTake {
            peak: 9.0,
            ..active(ID)
        };
        assert!(
            matches!(level(&loud, 0.0), CaptureEvent::Level { rms, .. } if (rms - 1.0).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn a_take_ends_at_its_real_length_and_the_cap_says_so() {
        let user = closing(&take(ID, EndedBy::User), 1_000.0, 295_000);
        assert_eq!(user, vec![CaptureEvent::Ended { at_ms: 62_440.0 }]);
        let capped = NativeTake {
            duration_ms: 294_976,
            ..take(ID, EndedBy::Cap)
        };
        assert_eq!(
            closing(&capped, 0.0, 295_000),
            vec![
                CaptureEvent::Tick { at_ms: 295_000.0 },
                CaptureEvent::Ended { at_ms: 294_976.0 }
            ]
        );
    }

    #[test]
    fn the_recorder_reports_a_capped_take_as_capped() {
        use super::super::recorder::{Recorder, RecorderConfig, Step};
        let mut recorder = Recorder::new(RecorderConfig {
            silence: None,
            max_ms: 295_000.0,
        });
        let capped = NativeTake {
            duration_ms: 294_976,
            ..take(ID, EndedBy::Cap)
        };
        recorder.feed(CaptureEvent::Started { at_ms: 0.0 });
        recorder.feed(CaptureEvent::Data(vec![1]));
        let mut steps = closing(&capped, 0.0, 295_000)
            .into_iter()
            .map(|event| recorder.feed(event));
        assert_eq!(steps.next(), Some(Step::Stop(StopReason::TooLong)));
        let Some(Step::Finished(recording)) = steps.next() else {
            panic!("finished")
        };
        assert_eq!(recording.stopped_by, StopReason::TooLong);
        assert_eq!(recording.duration_ms, 294_976);
    }

    #[test]
    fn adoption_leaves_this_pages_takes_alone() {
        let status = RecorderStatus {
            active: Some(active(OTHER)),
            takes: vec![take(ID, EndedBy::Interrupted)],
        };
        let none = [];
        assert_eq!(
            plan(&status, &none, false),
            Plan {
                stop: Some(id(OTHER)),
                adopt: vec![take(ID, EndedBy::Interrupted)],
            },
            "a page that is gone left both"
        );
        assert_eq!(
            plan(&status, &none, true).stop,
            None,
            "the running take may be the one a start here is waiting for"
        );
        let live = [id(OTHER), id(ID)];
        assert_eq!(
            plan(&status, &live, false),
            Plan {
                stop: None,
                adopt: Vec::new(),
            }
        );
    }

    #[test]
    fn an_adopted_take_waits_for_review_under_its_own_id() {
        let (agent, recording, at) = adopted(&take(ID, EndedBy::User), vec![1, 2]).unwrap();
        assert_eq!(agent.to_string(), "w1:p1");
        assert_eq!(recording.id, Some(id(ID)));
        assert_eq!(recording.stopped_by, StopReason::Interrupted);
        assert_eq!(recording.duration_ms, 61_440);
        assert_eq!(at, Timestamp::from_unix_millis(1_767_225_600_000));
        let stray = NativeTake {
            tag: "not an agent".to_owned(),
            ..take(ID, EndedBy::User)
        };
        assert_eq!(adopted(&stray, vec![1]), None);
    }

    #[test]
    fn base64_decodes_as_java_encodes_it() {
        assert_eq!(decode_base64(""), Some(Vec::new()));
        assert_eq!(decode_base64("Zg=="), Some(b"f".to_vec()));
        assert_eq!(decode_base64("Zm8="), Some(b"fo".to_vec()));
        assert_eq!(decode_base64("Zm9v"), Some(b"foo".to_vec()));
        assert_eq!(decode_base64("Zm9vYmFy"), Some(b"foobar".to_vec()));
        assert_eq!(
            decode_base64("//79AA=="),
            Some(vec![0xFF, 0xFE, 0xFD, 0x00])
        );
        for bad in ["Zg", "Zg=a", "Z===", "Zg==Zm9v", "Zm9v!A==", "Zm-_"] {
            assert_eq!(decode_base64(bad), None, "{bad}");
        }
    }
}
