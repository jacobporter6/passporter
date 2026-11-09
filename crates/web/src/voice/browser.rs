//! The browser behind the dictation traits: the microphone through
//! `getUserMedia`, `MediaRecorder` and an `AnalyserNode` ([`MediaCapture`]),
//! clips in their own `IndexedDB` database ([`IdbClips`]), and `fetch`
//! with raw byte bodies ([`FetchAudio`]).
//!
//! Only called in the browser; everything they feed is tested on the host
//! through the traits.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use js_sys::{Array, Uint8Array};
use protocol::AudioId;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    AnalyserNode, AudioContext, Blob, BlobEvent, IdbDatabase, IdbObjectStore, IdbOpenDbRequest,
    IdbTransaction, IdbTransactionMode, MediaRecorder, MediaRecorderOptions, MediaStream,
    MediaStreamConstraints, RecordingState, VisibilityState,
};

use super::clip::{ClipMeta, ClipStore, ClipStoreError, PendingClip};
use super::level::rms_u8;
use super::recorder::{AudioCapture, CaptureError, CaptureEvent, CaptureSession, EventSink};
use super::upload::{AudioBody, AudioRequest, AudioTransport};
use crate::api::response::UNAUTHORIZED;
use crate::api::{ApiError, HttpResponse, Method, RawRequest, TokenSource, fetch_raw};
use crate::platform::{self, js_error_message};
use crate::store::{StoreError, committed, settle};

/// Encodings to ask `MediaRecorder` for, best first. The bridge's
/// whisper-server converts any of them; Opus is the smallest.
const MIME_PREFERENCES: [&str; 3] = [
    "audio/webm;codecs=opus",
    "audio/ogg;codecs=opus",
    "audio/mp4",
];
/// Longest wait for one chunk, in ms: a little over the bridge's own
/// two-minute chunk timeout, so a hung upload becomes a retry.
const CHUNK_TIMEOUT_MS: u32 = 150_000;
/// Longest wait for a declare or a transcript poll, in ms.
const REQUEST_TIMEOUT_MS: u32 = 30_000;
/// How often the meter samples the microphone, in ms.
const LEVEL_EVERY_MS: i32 = 50;
/// Analyser frame size: about 21 ms at 48 kHz.
const FFT_SIZE: u32 = 1024;

/// The clip database, apart from the app's JSON store (`crate::store`):
/// audio is stored as bytes by structured clone, not as JSON, and the
/// records have their own lifecycle.
const DB_NAME: &str = "passporter-voice";
/// The document event that reports the page being hidden.
const VISIBILITY_CHANGE: &str = "visibilitychange";
const DB_VERSION: u32 = 1;
const META: &str = "meta";
const AUDIO: &str = "audio";

/// Whether this browser can record here: a secure context (the microphone
/// is never offered outside one), `MediaRecorder` and `mediaDevices`.
#[must_use]
pub fn available() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    window.is_secure_context()
        && platform::has(&window, "MediaRecorder")
        && platform::has(&window.navigator(), "mediaDevices")
}

/// The device clock, in ms.
fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// The microphone.
#[derive(Debug, Clone, Default)]
pub struct MediaCapture {
    /// The meter's audio context: made by [`Self::prime`] inside a user
    /// gesture, then used (and finally closed) by the running capture.
    context: Rc<RefCell<Option<AudioContext>>>,
}

impl MediaCapture {
    /// Makes the meter's `AudioContext` if there is none, and asks it to
    /// run. Call it from every event that can carry user activation (the
    /// press and the release of the mic button): browsers (iOS Safari
    /// above all) only let audio start inside one, and a touch's
    /// `pointerdown` doesn't count, its `pointerup` does. A context that
    /// stays suspended leaves the meter flat and silence never detected.
    pub fn prime(&self) {
        let mut context = self.context.borrow_mut();
        if context.is_none() {
            *context = AudioContext::new().ok();
        }
        if let Some(context) = context.as_ref() {
            resume(context);
        }
    }

    async fn stream() -> Result<MediaStream, CaptureError> {
        if !available() {
            return Err(CaptureError::Unsupported);
        }
        let window = web_sys::window().ok_or(CaptureError::Unsupported)?;
        let devices = window
            .navigator()
            .media_devices()
            .map_err(|_| CaptureError::Unsupported)?;
        let constraints = MediaStreamConstraints::new();
        constraints.set_audio_bool(true);
        let request = devices
            .get_user_media_with_constraints(&constraints)
            .map_err(|e| capture_error(&e))?;
        Ok(JsFuture::from(request)
            .await
            .map_err(|e| capture_error(&e))?
            .unchecked_into())
    }
}

impl AudioCapture for MediaCapture {
    type Session = MicSession;

    async fn open(&self, sink: EventSink) -> Result<MicSession, CaptureError> {
        // Unprimed: try anyway; a suspended context only costs the meter
        // until the next prime resumes it.
        self.prime();
        let meter = self.context.borrow().clone();
        let stream = match Self::stream().await {
            Ok(stream) => stream,
            Err(error) => {
                self.release_context();
                return Err(error);
            }
        };
        Mic::start(&stream, meter, self.clone(), &sink)
            .map(MicSession)
            .map_err(|error| {
                platform::stop_tracks(&stream);
                capture_error(&error)
            })
    }
}

impl MediaCapture {
    /// Asks the meter's context to run, if there is one. For a release of
    /// the mic button: it never makes a context of its own.
    pub fn resume_meter(&self) {
        if let Some(context) = self.context.borrow().as_ref() {
            resume(context);
        }
    }

    /// Closes the meter's context: the capture that used it is over.
    fn release_context(&self) {
        if let Some(context) = self.context.borrow_mut().take() {
            close(&context);
        }
    }

    /// Closes `meter` and forgets it, unless a newer one replaced it.
    fn retire(&self, meter: &AudioContext) {
        close(meter);
        let mut context = self.context.borrow_mut();
        if context.as_ref() == Some(meter) {
            *context = None;
        }
    }
}

/// Asks an audio context to run (best effort).
fn resume(context: &AudioContext) {
    if let Ok(resume) = context.resume() {
        spawn_local(async move {
            let _ = JsFuture::from(resume).await;
        });
    }
}

/// Closes an audio context (best effort).
fn close(context: &AudioContext) {
    if let Ok(closed) = context.close() {
        spawn_local(async move {
            let _ = JsFuture::from(closed).await;
        });
    }
}

/// Maps a `getUserMedia` or `MediaRecorder` failure.
fn capture_error(error: &JsValue) -> CaptureError {
    let name = error
        .dyn_ref::<web_sys::DomException>()
        .map(web_sys::DomException::name);
    match name.as_deref() {
        Some("NotAllowedError" | "SecurityError") => CaptureError::Denied,
        Some("NotFoundError" | "OverconstrainedError") => CaptureError::NoDevice,
        Some("NotSupportedError") => CaptureError::Unsupported,
        _ => CaptureError::Failed(js_error_message(error)),
    }
}

/// A running microphone capture.
#[derive(Clone)]
pub struct MicSession(Rc<Mic>);

impl core::fmt::Debug for MicSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MicSession").finish_non_exhaustive()
    }
}

impl CaptureSession for MicSession {
    fn stop(&self) {
        let recorder = &self.0.recorder;
        if recorder.state() != RecordingState::Inactive {
            // Fires the last `dataavailable`, then `stop` (see `Mic::start`).
            let _ = recorder.stop();
        }
    }
}

/// The browser objects behind one capture. Event handlers live here and
/// are dropped in [`Mic::teardown`], once the recorder has stopped (or
/// failed to start).
struct Mic {
    recorder: MediaRecorder,
    stream: MediaStream,
    meter: Option<AudioContext>,
    /// Where `meter` came from, to retire it there.
    capture: MediaCapture,
    timer: Cell<Option<i32>>,
    parts: RefCell<Vec<Blob>>,
    handlers: RefCell<Vec<Handler>>,
    /// The document's `visibilitychange` listener, removed in teardown.
    on_hidden: RefCell<Option<Handler>>,
}

/// A DOM event handler kept alive in Rust.
type Handler = Closure<dyn FnMut(JsValue)>;

impl Mic {
    /// Starts recording `stream`. Audio arrives as `Data` once the recorder
    /// stops (one blob per `dataavailable`), followed by `Ended`, whether
    /// it was asked to stop, the microphone went away or the encoder
    /// failed: what was captured is always kept.
    fn start(
        stream: &MediaStream,
        meter: Option<AudioContext>,
        capture: MediaCapture,
        sink: &EventSink,
    ) -> Result<Rc<Self>, JsValue> {
        let options = MediaRecorderOptions::new();
        let mime = MIME_PREFERENCES
            .into_iter()
            .find(|mime| MediaRecorder::is_type_supported(mime));
        if let Some(mime) = mime {
            options.set_mime_type(mime);
        }
        let recorder =
            match MediaRecorder::new_with_media_stream_and_media_recorder_options(stream, &options)
            {
                Ok(recorder) => recorder,
                Err(error) => {
                    if let Some(meter) = &meter {
                        capture.retire(meter);
                    }
                    return Err(error);
                }
            };
        // Without a meter the recording still works; it just can't show
        // levels or stop on silence.
        let mic = Rc::new(Self {
            recorder,
            stream: stream.clone(),
            meter,
            capture,
            timer: Cell::new(None),
            parts: RefCell::new(Vec::new()),
            handlers: RefCell::new(Vec::new()),
            on_hidden: RefCell::new(None),
        });

        let on_data = {
            let mic = Rc::downgrade(&mic);
            Handler::new(move |event: JsValue| {
                let blob = event.dyn_into::<BlobEvent>().ok().and_then(|e| e.data());
                if let (Some(mic), Some(blob)) = (mic.upgrade(), blob)
                    && blob.size() > 0.0
                {
                    mic.parts.borrow_mut().push(blob);
                }
            })
        };
        // An encoder error is followed by the last `dataavailable` and
        // `stop`, which keep what was recorded; only log it here.
        let on_error = Handler::new(|event: JsValue| {
            platform::log_error("the recorder stopped with an error", &event);
        });
        // When the recorder stops: hand over the audio, then let go of
        // everything. Teardown drops this very handler, so it runs after
        // the handler has returned.
        let on_stop = {
            let mic = Rc::downgrade(&mic);
            let sink = sink.clone();
            Handler::new(move |_: JsValue| {
                let Some(mic) = mic.upgrade() else {
                    return;
                };
                let at_ms = now_ms();
                let parts = mic.parts.take();
                let sink = sink.clone();
                spawn_local(async move {
                    mic.teardown();
                    deliver_parts(parts, &sink, at_ms).await;
                });
            })
        };
        // A hidden page may be frozen, or its renderer killed: the clock
        // stops while the recorder runs on (past the length cap the bridge
        // enforces), and audio held only in memory can be lost. So the
        // take stops and what was recorded is saved; the wake lock
        // (`super::wake`) keeps the screen from turning off meanwhile.
        let on_hidden = {
            let sink = sink.clone();
            Handler::new(move |_: JsValue| {
                let hidden = web_sys::window()
                    .and_then(|w| w.document())
                    .is_some_and(|d| d.visibility_state() == VisibilityState::Hidden);
                if hidden {
                    sink(CaptureEvent::Hidden);
                }
            })
        };
        if let Some(document) = web_sys::window().and_then(|w| w.document()) {
            let _ = document.add_event_listener_with_callback(
                VISIBILITY_CHANGE,
                on_hidden.as_ref().unchecked_ref(),
            );
        }
        mic.recorder
            .set_ondataavailable(Some(on_data.as_ref().unchecked_ref()));
        mic.recorder
            .set_onerror(Some(on_error.as_ref().unchecked_ref()));
        mic.recorder
            .set_onstop(Some(on_stop.as_ref().unchecked_ref()));
        *mic.on_hidden.borrow_mut() = Some(on_hidden);
        mic.handlers
            .borrow_mut()
            .extend([on_data, on_error, on_stop]);

        if let Err(error) = mic
            .start_clock(sink.clone())
            .and_then(|()| mic.recorder.start())
        {
            mic.teardown();
            return Err(error);
        }
        sink(CaptureEvent::Started { at_ms: now_ms() });
        Ok(mic)
    }

    /// Reports every [`LEVEL_EVERY_MS`]: the loudness when the meter
    /// works, otherwise just the time, so the length cap always applies.
    fn start_clock(&self, sink: EventSink) -> Result<(), JsValue> {
        let analyser =
            self.meter
                .as_ref()
                .and_then(|context| match analyser(context, &self.stream) {
                    Ok(analyser) => Some(analyser),
                    Err(error) => {
                        platform::log_error("level meter unavailable", &error);
                        None
                    }
                });
        let mut frame = vec![128_u8; FFT_SIZE as usize];
        let tick = Handler::new(move |_: JsValue| {
            let at_ms = now_ms();
            sink(match &analyser {
                Some(analyser) => {
                    analyser.get_byte_time_domain_data(&mut frame);
                    CaptureEvent::Level {
                        at_ms,
                        rms: rms_u8(&frame),
                    }
                }
                None => CaptureEvent::Tick { at_ms },
            });
        });
        let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
        let timer = window.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            LEVEL_EVERY_MS,
        )?;
        self.timer.set(Some(timer));
        self.handlers.borrow_mut().push(tick);
        Ok(())
    }

    /// Releases the microphone, the audio graph and every handler. Never
    /// called from inside one of the handlers it drops.
    fn teardown(&self) {
        if let (Some(timer), Some(window)) = (self.timer.take(), web_sys::window()) {
            window.clear_interval_with_handle(timer);
        }
        self.recorder.set_ondataavailable(None);
        self.recorder.set_onerror(None);
        self.recorder.set_onstop(None);
        platform::stop_tracks(&self.stream);
        if let Some(context) = &self.meter {
            self.capture.retire(context);
        }
        if let (Some(listener), Some(document)) = (
            self.on_hidden.take(),
            web_sys::window().and_then(|w| w.document()),
        ) {
            let _ = document.remove_event_listener_with_callback(
                VISIBILITY_CHANGE,
                listener.as_ref().unchecked_ref(),
            );
        }
        self.handlers.borrow_mut().clear();
    }
}

/// An analyser on `stream`, for the meter.
fn analyser(context: &AudioContext, stream: &MediaStream) -> Result<AnalyserNode, JsValue> {
    let source = context.create_media_stream_source(stream)?;
    let analyser = context.create_analyser()?;
    analyser.set_fft_size(FFT_SIZE);
    source.connect_with_audio_node(&analyser)?;
    // Started from a tap, but after the permission prompt: make sure the
    // context runs.
    resume(context);
    Ok(analyser)
}

/// Reads the recorded blobs in order into `Data` events, then `Ended`. A
/// blob that can't be read ends the clip there: what came before is kept
/// (a gap in the middle would not decode).
async fn deliver_parts(parts: Vec<Blob>, sink: &EventSink, at_ms: f64) {
    for part in parts {
        match JsFuture::from(part.array_buffer()).await {
            Ok(buffer) => sink(CaptureEvent::Data(Uint8Array::new(&buffer).to_vec())),
            Err(error) => {
                platform::log_error("a recorded chunk could not be read", &error);
                break;
            }
        }
    }
    sink(CaptureEvent::Ended { at_ms });
}

/// Clips in `IndexedDB`, keyed by clip id in two object stores: the
/// [`ClipMeta`] as JSON in `meta`, the audio as a `Uint8Array` in `audio`.
/// Listing reads `meta` alone, so it never loads anyone's audio.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdbClips;

impl ClipStore for IdbClips {
    async fn put(&self, clip: &PendingClip) -> Result<(), ClipStoreError> {
        let meta = serde_json::to_string(&clip.meta).map_err(|e| ClipStoreError(e.to_string()))?;
        let key = JsValue::from_str(clip.meta.audio_id.as_str());
        let audio = Uint8Array::from(clip.audio.as_slice());
        write(|tx| {
            object_store(tx, META)?
                .put_with_key(&JsValue::from_str(&meta), &key)
                .map_err(|e| js_store_error(&e))?;
            object_store(tx, AUDIO)?
                .put_with_key(&audio, &key)
                .map_err(|e| js_store_error(&e))?;
            Ok(())
        })
        .await
    }

    async fn list(&self) -> Result<Vec<ClipMeta>, ClipStoreError> {
        let tx = transaction(IdbTransactionMode::Readonly).await?;
        let request = object_store(&tx, META)?
            .get_all()
            .map_err(|e| js_store_error(&e))?;
        let records: Array = settle(&request)
            .await
            .map_err(|e| store_error(&e))?
            .unchecked_into();
        Ok(records
            .iter()
            .filter_map(|meta| serde_json::from_str(&meta.as_string()?).ok())
            .collect())
    }

    async fn get(&self, id: &AudioId) -> Result<Option<PendingClip>, ClipStoreError> {
        let key = JsValue::from_str(id.as_str());
        let tx = transaction(IdbTransactionMode::Readonly).await?;
        // Both requests go out, with their handlers attached, before
        // either is awaited: the transaction stays active for both, and
        // no success event fires before its handler is there.
        let meta = settle(
            &object_store(&tx, META)?
                .get(&key)
                .map_err(|e| js_store_error(&e))?,
        );
        let audio = settle(
            &object_store(&tx, AUDIO)?
                .get(&key)
                .map_err(|e| js_store_error(&e))?,
        );
        let meta = meta.await.map_err(|e| store_error(&e))?;
        let audio = audio.await.map_err(|e| store_error(&e))?;
        let meta = meta
            .as_string()
            .and_then(|meta| serde_json::from_str::<ClipMeta>(&meta).ok());
        let audio = audio.dyn_into::<Uint8Array>().ok().map(|a| a.to_vec());
        Ok(meta.map(|meta| PendingClip {
            meta,
            audio: audio.unwrap_or_default(),
        }))
    }

    async fn put_meta(&self, meta: &ClipMeta) -> Result<(), ClipStoreError> {
        let json = serde_json::to_string(meta).map_err(|e| ClipStoreError(e.to_string()))?;
        let key = JsValue::from_str(meta.audio_id.as_str());
        let tx = transaction(IdbTransactionMode::Readwrite).await?;
        let done = committed(&tx);
        // Read and write in one transaction, so a discard can't land
        // between them: a removed clip is not brought back as metadata
        // without its audio.
        let written = async {
            let metas = object_store(&tx, META)?;
            let request = metas.get(&key).map_err(|e| js_store_error(&e))?;
            let stored = settle(&request).await.map_err(|e| store_error(&e))?;
            if !stored.is_undefined() {
                metas
                    .put_with_key(&JsValue::from_str(&json), &key)
                    .map_err(|e| js_store_error(&e))?;
            }
            Ok(())
        }
        .await;
        finish(&tx, done, written).await
    }

    async fn rename(&self, old: &AudioId, clip: &PendingClip) -> Result<(), ClipStoreError> {
        let meta = serde_json::to_string(&clip.meta).map_err(|e| ClipStoreError(e.to_string()))?;
        let key = JsValue::from_str(clip.meta.audio_id.as_str());
        let old = JsValue::from_str(old.as_str());
        let audio = Uint8Array::from(clip.audio.as_slice());
        write(|tx| {
            let (metas, audios) = (object_store(tx, META)?, object_store(tx, AUDIO)?);
            metas
                .put_with_key(&JsValue::from_str(&meta), &key)
                .map_err(|e| js_store_error(&e))?;
            audios
                .put_with_key(&audio, &key)
                .map_err(|e| js_store_error(&e))?;
            for store in [&metas, &audios] {
                store.delete(&old).map_err(|e| js_store_error(&e))?;
            }
            Ok(())
        })
        .await
    }

    async fn remove(&self, id: &AudioId) -> Result<(), ClipStoreError> {
        let key = JsValue::from_str(id.as_str());
        write(|tx| {
            for name in [META, AUDIO] {
                object_store(tx, name)?
                    .delete(&key)
                    .map_err(|e| js_store_error(&e))?;
            }
            Ok(())
        })
        .await
    }
}

/// Runs `requests` in one read-write transaction over both stores and
/// waits for the commit. If issuing a request fails, the transaction is
/// aborted, so a clip is never stored half (its metadata without its
/// audio).
async fn write(
    requests: impl FnOnce(&IdbTransaction) -> Result<(), ClipStoreError>,
) -> Result<(), ClipStoreError> {
    let tx = transaction(IdbTransactionMode::Readwrite).await?;
    let done = committed(&tx);
    let issued = requests(&tx);
    finish(&tx, done, issued).await
}

/// Waits for `tx` to commit, or aborts it if issuing its requests failed.
async fn finish(
    tx: &IdbTransaction,
    done: impl Future<Output = Result<(), StoreError>>,
    issued: Result<(), ClipStoreError>,
) -> Result<(), ClipStoreError> {
    if let Err(error) = issued {
        let _ = tx.abort();
        // Settles the commit promise (as an abort) before returning.
        let _ = done.await;
        return Err(error);
    }
    done.await.map_err(|e| store_error(&e))
}

fn js_store_error(error: &JsValue) -> ClipStoreError {
    ClipStoreError(js_error_message(error))
}

fn store_error(error: &StoreError) -> ClipStoreError {
    ClipStoreError(error.to_string())
}

fn object_store(tx: &IdbTransaction, name: &str) -> Result<IdbObjectStore, ClipStoreError> {
    tx.object_store(name).map_err(|e| js_store_error(&e))
}

thread_local! {
    /// The clip database's connection, opened on first use. Dropped when
    /// another tab upgrades the schema (`versionchange`) or the browser
    /// closes it, and reopened on the next use.
    static DATABASE: RefCell<Option<IdbDatabase>> = const { RefCell::new(None) };
}

/// Starts a transaction over both stores, on the shared connection.
async fn transaction(mode: IdbTransactionMode) -> Result<IdbTransaction, ClipStoreError> {
    let stores = Array::of2(&JsValue::from_str(META), &JsValue::from_str(AUDIO));
    if let Some(db) = DATABASE.with_borrow(Clone::clone) {
        match db.transaction_with_str_sequence_and_mode(&stores, mode) {
            Ok(tx) => return Ok(tx),
            // Closed under us: reopen below.
            Err(_) => DATABASE.set(None),
        }
    }
    let db = open_database().await?;
    let tx = db
        .transaction_with_str_sequence_and_mode(&stores, mode)
        .map_err(|e| js_store_error(&e))?;
    DATABASE.set(Some(db));
    Ok(tx)
}

/// Opens the database, creating its stores on first use. The connection
/// closes itself when another tab needs a newer schema, so it never blocks
/// an upgrade; with a single schema version so far, this open is never
/// blocked either.
async fn open_database() -> Result<IdbDatabase, ClipStoreError> {
    let factory = web_sys::window()
        .ok_or_else(|| ClipStoreError("no window".into()))?
        .indexed_db()
        .map_err(|e| js_store_error(&e))?
        .ok_or_else(|| ClipStoreError("IndexedDB is not supported".into()))?;
    let open: IdbOpenDbRequest = factory
        .open_with_u32(DB_NAME, DB_VERSION)
        .map_err(|e| js_store_error(&e))?;
    let upgrading = open.clone();
    // Runs inside the version-change transaction: if a store can't be
    // made, abort it, so the open fails (and is tried again next time)
    // instead of committing a version without its stores.
    let on_upgrade = Closure::<dyn FnMut()>::new(move || {
        let Ok(db) = upgrading.result().and_then(JsCast::dyn_into::<IdbDatabase>) else {
            return;
        };
        for name in [META, AUDIO] {
            if !db.object_store_names().contains(name) && db.create_object_store(name).is_err() {
                if let Some(tx) = upgrading.transaction() {
                    let _ = tx.abort();
                }
                return;
            }
        }
    });
    open.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));
    let db = settle(&open).await;
    open.set_onupgradeneeded(None);
    let db: IdbDatabase = db
        .map_err(|e| store_error(&e))?
        .dyn_into()
        .map_err(|e| js_store_error(&e))?;
    let closing = db.clone();
    let on_version_change = Closure::once_into_js(move || {
        closing.close();
        DATABASE.set(None);
    });
    db.set_onversionchange(Some(on_version_change.unchecked_ref()));
    Ok(db)
}

/// Sends audio requests through [`crate::api::fetch_raw`], so they follow
/// the app's one set of `fetch` rules (same origin, no cookies, redirects
/// are errors, no HTTP cache). A 401 is reported to the token source.
#[derive(Debug, Clone)]
pub struct FetchAudio<A> {
    auth: A,
}

impl<A: TokenSource> FetchAudio<A> {
    /// A transport that authenticates with `auth`.
    pub const fn new(auth: A) -> Self {
        Self { auth }
    }
}

impl<A: TokenSource> AudioTransport for FetchAudio<A> {
    async fn send(&self, request: AudioRequest<'_>) -> Result<HttpResponse, ApiError> {
        let token = self.auth.current();
        let offset = request.upload_offset.map(|offset| offset.to_string());
        let mut headers = vec![("Accept", "application/json")];
        let body = match &request.body {
            AudioBody::Empty => None,
            AudioBody::Json(json) => {
                headers.push(("Content-Type", "application/json"));
                Some(JsValue::from_str(json))
            }
            AudioBody::Audio(bytes) => {
                headers.push(("Content-Type", "application/octet-stream"));
                Some(Uint8Array::from(*bytes).into())
            }
        };
        if let Some(offset) = &offset {
            headers.push(("Upload-Offset", offset));
        }
        let response = fetch_raw(RawRequest {
            method: request.method.as_str(),
            path: &request.path,
            headers: &headers,
            auth: token.as_ref(),
            body: body.as_ref(),
            timeout_ms: Some(match request.method {
                Method::Patch => CHUNK_TIMEOUT_MS,
                Method::Get | Method::Post | Method::Delete => REQUEST_TIMEOUT_MS,
            }),
        })
        .await
        .map_err(|e| ApiError::Transport(js_error_message(&e)))?;
        if response.status == UNAUTHORIZED
            && let Some(token) = &token
        {
            self.auth.unauthorized(token);
        }
        Ok(response)
    }
}
