//! The recorder (design §6.3): an [`AudioCapture`] (the microphone, or a
//! fake stream in tests) feeds [`CaptureEvent`]s to a pure [`Recorder`]
//! state machine, which keeps the live meter, stops on silence or at the
//! length cap, and hands back the finished [`Recording`]. [`Dictaphone`]
//! wires the two together; [`MicGesture`] turns presses on the mic button
//! into start and stop, for both tap-to-toggle and hold-to-talk.

use core::cell::{Cell, RefCell};
use core::future::Future;
use std::rc::Rc;

use super::level::{LevelMeter, SilenceConfig, SilenceGate};

/// What a capture reports, in order: `Started`, then levels (or ticks)
/// and data in any interleaving, then exactly one `Ended` or `Failed`.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureEvent {
    /// Capture began, at `at_ms` on the capture's clock.
    Started {
        /// Capture clock, in ms.
        at_ms: f64,
    },
    /// The loudness of the latest audio frame.
    Level {
        /// Capture clock, in ms.
        at_ms: f64,
        /// RMS loudness in `0.0..=1.0` (see [`super::level::rms_u8`]).
        rms: f32,
    },
    /// Time passing, from a capture that can't measure loudness (no
    /// audio graph): the length cap still applies.
    Tick {
        /// Capture clock, in ms.
        at_ms: f64,
    },
    /// Encoded audio, to append to the clip.
    Data(Vec<u8>),
    /// Capture stopped and all data has been delivered.
    Ended {
        /// Capture clock, in ms.
        at_ms: f64,
    },
    /// Capture broke (the device went away, the encoder failed).
    Failed(String),
    /// The page was hidden (the screen turned off, another app): the take
    /// stops and keeps what was recorded.
    Hidden,
}

/// Where a capture delivers its events.
pub type EventSink = Rc<dyn Fn(CaptureEvent)>;

/// A source of audio: the microphone in the browser, a scripted stream in
/// tests.
pub trait AudioCapture {
    /// A running capture.
    type Session: CaptureSession + Clone + 'static;

    /// Asks for the microphone and starts capturing; events go to `sink`
    /// until `Ended` or `Failed`.
    fn open(&self, sink: EventSink) -> impl Future<Output = Result<Self::Session, CaptureError>>;
}

/// A running capture.
pub trait CaptureSession {
    /// Asks the capture to finish: it delivers any remaining data, then
    /// `Ended`. Calling it again is harmless.
    fn stop(&self);
}

/// Why recording could not start or finish.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    /// The user (or a browser policy) refused the microphone.
    #[error("microphone access was refused; allow it in the browser's site settings")]
    Denied,
    /// There is no microphone.
    #[error("no microphone was found")]
    NoDevice,
    /// This browser can't record here (no secure context, no `MediaRecorder`).
    #[error("this browser can't record audio here")]
    Unsupported,
    /// The capture ended without any audio.
    #[error("nothing was recorded")]
    Empty,
    /// A recording is already running.
    #[error("already recording")]
    Busy,
    /// Anything else the browser reported.
    #[error("recording failed: {0}")]
    Failed(String),
}

/// Why a recording stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The user tapped stop or let go.
    User,
    /// Quiet for [`SilenceConfig::after_ms`] after speech.
    Silence,
    /// Reached [`RecorderConfig::max_ms`].
    TooLong,
    /// The capture ended by itself (the microphone went away).
    Interrupted,
    /// The page was hidden: the screen turned off, or another app.
    Hidden,
}

/// Recorder settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecorderConfig {
    /// Silence auto-stop; `None` records until stopped.
    pub silence: Option<SilenceConfig>,
    /// Longest recording, in ms. Kept under the bridge's default cap (five
    /// minutes), which also allows two seconds of slack.
    pub max_ms: f64,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            silence: Some(SilenceConfig::default()),
            max_ms: 295_000.0,
        }
    }
}

/// A finished recording, not yet stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    /// The encoded audio.
    pub bytes: Vec<u8>,
    /// Length in ms (at least 1).
    pub duration_ms: u64,
    /// What ended it.
    pub stopped_by: StopReason,
}

/// What the recorder wants after an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Keep going.
    Continue,
    /// Stop the capture (silence or the length cap).
    Stop(StopReason),
    /// Done: the recording.
    Finished(Recording),
    /// Done, without a recording.
    Failed(CaptureError),
}

/// The pure recording state machine.
#[derive(Debug, Clone)]
pub struct Recorder {
    config: RecorderConfig,
    meter: LevelMeter,
    silence: Option<SilenceGate>,
    started_at: Option<f64>,
    elapsed_ms: f64,
    bytes: Vec<u8>,
    stopping: Option<StopReason>,
    /// The mic button is held (hold-to-talk): silence doesn't stop it.
    held: bool,
    done: bool,
}

impl Recorder {
    /// A recorder that has not started.
    #[must_use]
    pub fn new(config: RecorderConfig) -> Self {
        Self {
            config,
            meter: LevelMeter::default(),
            silence: config.silence.map(SilenceGate::new),
            started_at: None,
            elapsed_ms: 0.0,
            bytes: Vec::new(),
            stopping: None,
            held: false,
            done: false,
        }
    }

    /// Handles one capture event.
    pub fn feed(&mut self, event: CaptureEvent) -> Step {
        if self.done {
            return Step::Continue;
        }
        match event {
            CaptureEvent::Started { at_ms } => {
                self.started_at.get_or_insert(at_ms);
                Step::Continue
            }
            CaptureEvent::Level { at_ms, rms } => {
                self.meter.push(rms);
                let quiet = self
                    .silence
                    .as_mut()
                    .is_some_and(|gate| gate.hear(at_ms, rms));
                self.advance(at_ms, quiet && !self.held)
            }
            CaptureEvent::Tick { at_ms } => self.advance(at_ms, false),
            CaptureEvent::Data(bytes) => {
                self.bytes.extend_from_slice(&bytes);
                Step::Continue
            }
            CaptureEvent::Ended { at_ms } => {
                self.done = true;
                if self.bytes.is_empty() {
                    return Step::Failed(CaptureError::Empty);
                }
                Step::Finished(Recording {
                    bytes: core::mem::take(&mut self.bytes),
                    // The bridge refuses a zero duration.
                    duration_ms: whole_ms(self.since_start(at_ms)).max(1),
                    stopped_by: self.stopping.unwrap_or(StopReason::Interrupted),
                })
            }
            CaptureEvent::Failed(message) => {
                self.done = true;
                Step::Failed(CaptureError::Failed(message))
            }
            CaptureEvent::Hidden => {
                if self.request_stop(StopReason::Hidden) {
                    Step::Stop(StopReason::Hidden)
                } else {
                    Step::Continue
                }
            }
        }
    }

    /// Moves the clock to `at_ms` and stops at the length cap, or on
    /// silence when `quiet`.
    fn advance(&mut self, at_ms: f64, quiet: bool) -> Step {
        self.elapsed_ms = self.since_start(at_ms);
        if self.stopping.is_some() {
            return Step::Continue;
        }
        let reason = if self.elapsed_ms >= self.config.max_ms {
            StopReason::TooLong
        } else if quiet {
            StopReason::Silence
        } else {
            return Step::Continue;
        };
        self.stopping = Some(reason);
        Step::Stop(reason)
    }

    /// Asks to stop for `reason`. False if it is already stopping or done
    /// (the first reason wins).
    pub fn request_stop(&mut self, reason: StopReason) -> bool {
        if self.done || self.stopping.is_some() {
            return false;
        }
        self.stopping = Some(reason);
        true
    }

    /// Whether the mic button is held: while it is, silence never stops
    /// the recording (letting go does).
    pub const fn set_held(&mut self, held: bool) {
        self.held = held;
    }

    /// Whether a stop was asked for.
    #[must_use]
    pub const fn is_stopping(&self) -> bool {
        self.stopping.is_some()
    }

    /// The live meter.
    #[must_use]
    pub fn bars(&self) -> Vec<f32> {
        self.meter.bars()
    }

    /// Recorded so far, in ms.
    #[must_use]
    pub const fn elapsed_ms(&self) -> f64 {
        self.elapsed_ms
    }

    fn since_start(&self, at_ms: f64) -> f64 {
        self.started_at
            .map_or(0.0, |start| (at_ms - start).max(0.0))
    }
}

/// `ms` rounded to whole milliseconds.
fn whole_ms(ms: f64) -> u64 {
    // Clamped to 0..=one day: in range for u64, and never negative.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole = ms.round().clamp(0.0, 86_400_000.0) as u64;
    whole
}

/// What a running recording tells the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// The meter moved.
    Level {
        /// Bar heights, oldest first.
        bars: Vec<u8>,
        /// Recorded so far, in ms.
        elapsed_ms: u64,
    },
    /// Stopping (the UI shows it until `Finished`).
    Stopping(StopReason),
    /// The recording, ready to store.
    Finished(Recording),
    /// It ended without a recording.
    Failed(CaptureError),
}

/// Bar heights as percentages, so updates compare cheaply and exactly.
fn percent(bars: &[f32]) -> Vec<u8> {
    // Clamped to 0..=100 first.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    bars.iter()
        .map(|bar| (bar.clamp(0.0, 1.0) * 100.0).round() as u8)
        .collect()
}

/// One recording in progress.
struct Take<S> {
    recorder: Recorder,
    session: Option<S>,
    observer: Rc<dyn Fn(Update)>,
    /// Shared with [`Dictaphone::hold`].
    held: Rc<Cell<bool>>,
    /// Cancelled: finish quietly, report nothing.
    discard: bool,
}

type Slot<S> = Rc<RefCell<Option<Rc<RefCell<Take<S>>>>>>;

/// Records from an [`AudioCapture`], one take at a time, and reports to an
/// observer. Cheap to share behind an `Rc`.
pub struct Dictaphone<C: AudioCapture> {
    capture: C,
    config: RecorderConfig,
    current: Slot<C::Session>,
    held: Rc<Cell<bool>>,
    /// Whether takes stop by themselves on silence (when the config has
    /// it); see [`Self::auto_stop`].
    auto_stop: Cell<bool>,
}

impl<C: AudioCapture> core::fmt::Debug for Dictaphone<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dictaphone")
            .field("config", &self.config)
            .field("recording", &self.is_recording())
            .finish_non_exhaustive()
    }
}

impl<C: AudioCapture> Dictaphone<C> {
    /// A dictaphone over `capture`.
    pub fn new(capture: C, config: RecorderConfig) -> Self {
        Self {
            capture,
            config,
            current: Rc::new(RefCell::new(None)),
            held: Rc::new(Cell::new(false)),
            auto_stop: Cell::new(true),
        }
    }

    /// Whether takes started from now on stop by themselves on silence
    /// (if the config has silence auto-stop). Off, a take runs until it is
    /// stopped or reaches the length cap.
    pub fn auto_stop(&self, on: bool) {
        self.auto_stop.set(on);
    }

    /// The capture it records from.
    pub const fn capture(&self) -> &C {
        &self.capture
    }

    /// Whether a take is running (including while the microphone opens).
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.current.borrow().is_some()
    }

    /// Whether the current take is still waiting for the microphone (the
    /// permission prompt, the first time): nothing is being recorded yet.
    #[must_use]
    pub fn is_opening(&self) -> bool {
        self.current
            .borrow()
            .as_ref()
            .is_some_and(|take| take.borrow().session.is_none())
    }

    /// Opens the microphone and records until stopped, reporting to
    /// `observer`. The take is current as soon as this is called, before
    /// the future is first polled, so a second press right after is read
    /// as a stop. The future resolves once capture is running (or failed
    /// to start).
    pub fn start<F: Fn(Update) + 'static>(
        &self,
        observer: F,
    ) -> impl Future<Output = Result<(), CaptureError>> + use<C, F>
    where
        C: Clone,
    {
        let busy = self.is_recording();
        let take = Rc::new(RefCell::new(Take {
            recorder: Recorder::new(RecorderConfig {
                silence: self.config.silence.filter(|_| self.auto_stop.get()),
                ..self.config
            }),
            session: None,
            observer: Rc::new(observer),
            held: self.held.clone(),
            discard: false,
        }));
        if !busy {
            *self.current.borrow_mut() = Some(take.clone());
        }
        let capture = self.capture.clone();
        let slot = self.current.clone();
        async move {
            if busy {
                return Err(CaptureError::Busy);
            }
            let sink: EventSink = {
                let take = take.clone();
                let slot = slot.clone();
                Rc::new(move |event| on_event(&take, &slot, event))
            };
            match capture.open(sink).await {
                Ok(session) => {
                    // A stop asked for while the microphone was opening.
                    let stop_now = {
                        let mut take = take.borrow_mut();
                        take.session = Some(session.clone());
                        take.recorder.is_stopping()
                    };
                    if stop_now {
                        session.stop();
                    }
                    Ok(())
                }
                Err(error) => {
                    release(&slot, &take);
                    Err(error)
                }
            }
        }
    }

    /// Whether the mic button is held down. While it is, silence does not
    /// end the take (hold-to-talk ends when the user lets go).
    pub fn hold(&self, held: bool) {
        self.held.set(held);
    }

    /// Stops the take. Once the microphone is open, the recording arrives
    /// as [`Update::Finished`] after [`Update::Stopping`]; a stop while it
    /// is still opening (the permission prompt) cancels the take instead,
    /// since nothing has been recorded.
    pub fn stop(&self) -> Halt {
        let Some(take) = self.current.borrow().clone() else {
            return Halt::Idle;
        };
        let (session, observer) = {
            let mut t = take.borrow_mut();
            if !t.recorder.request_stop(StopReason::User) {
                return Halt::Idle;
            }
            let Some(session) = t.session.clone() else {
                // `start` closes the microphone if it opens later; the take
                // stops being current now, so a new one can start even if
                // the permission prompt never settles.
                t.discard = true;
                drop(t);
                release(&self.current, &take);
                return Halt::Cancelled;
            };
            (session, t.observer.clone())
        };
        observer(Update::Stopping(StopReason::User));
        session.stop();
        Halt::Stopping
    }
}

/// What [`Dictaphone::stop`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Halt {
    /// Nothing was recording, or it was already stopping.
    Idle,
    /// Stopping; the recording follows.
    Stopping,
    /// The microphone was still opening: the take is cancelled.
    Cancelled,
}

/// Ends `take` as the current one (if it still is).
fn release<S>(slot: &Slot<S>, take: &Rc<RefCell<Take<S>>>) {
    let mut current = slot.borrow_mut();
    if current.as_ref().is_some_and(|c| Rc::ptr_eq(c, take)) {
        *current = None;
    }
}

/// Feeds one event to the take's recorder and acts on the outcome. No
/// borrow is held while calling out, since a capture may answer `stop`
/// with events synchronously.
fn on_event<S: CaptureSession + Clone>(
    take: &Rc<RefCell<Take<S>>>,
    slot: &Slot<S>,
    event: CaptureEvent,
) {
    let is_level = matches!(
        event,
        CaptureEvent::Level { .. } | CaptureEvent::Tick { .. }
    );
    let (step, observer, session, discard, level) = {
        let mut t = take.borrow_mut();
        let held = t.held.get();
        t.recorder.set_held(held);
        let step = t.recorder.feed(event);
        let level = is_level.then(|| Update::Level {
            bars: percent(&t.recorder.bars()),
            elapsed_ms: whole_ms(t.recorder.elapsed_ms()),
        });
        (
            step,
            t.observer.clone(),
            t.session.clone(),
            t.discard,
            level,
        )
    };
    let report = |update: Update| {
        if !discard {
            observer(update);
        }
    };
    if let Some(level) = level {
        report(level);
    }
    match step {
        Step::Continue => {}
        Step::Stop(reason) => {
            report(Update::Stopping(reason));
            if let Some(session) = session {
                session.stop();
            }
        }
        Step::Finished(recording) => {
            release(slot, take);
            report(Update::Finished(recording));
        }
        Step::Failed(error) => {
            release(slot, take);
            if let Some(session) = session {
                session.stop();
            }
            report(Update::Failed(error));
        }
    }
}

/// How long a press must last to count as hold-to-talk, in ms. Shorter is
/// a tap, which toggles.
pub const HOLD_MS: f64 = 350.0;

/// What a press on the mic button means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicIntent {
    /// Start recording.
    Start,
    /// Stop recording.
    Stop,
}

/// Tap-to-toggle and hold-to-talk on one button: a press starts; letting
/// go after [`HOLD_MS`] stops (hold), letting go sooner keeps recording
/// until the next tap (toggle).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MicGesture {
    pressed_at: Option<f64>,
}

impl MicGesture {
    /// The button went down at `at_ms`.
    pub const fn press(&mut self, at_ms: f64, recording: bool) -> MicIntent {
        if recording {
            self.pressed_at = None;
            MicIntent::Stop
        } else {
            self.pressed_at = Some(at_ms);
            MicIntent::Start
        }
    }

    /// The button came up (or the pointer left or was cancelled) at `at_ms`.
    /// While the microphone is still `opening`, nothing has been recorded
    /// yet, and the permission prompt itself cancels the pointer: the take
    /// carries on as a tap (the next tap, or silence, stops it) rather
    /// than ending before it began.
    pub fn release(&mut self, at_ms: f64, opening: bool) -> Option<MicIntent> {
        let pressed_at = self.pressed_at.take()?;
        (!opening && at_ms - pressed_at >= HOLD_MS).then_some(MicIntent::Stop)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::block_on;
    use super::*;

    /// A scripted microphone: tests push events through [`Self::emit`];
    /// `stop` flushes `tail` and ends, as `MediaRecorder` does.
    pub(crate) struct FakeMic {
        pub(crate) sink: RefCell<Option<EventSink>>,
        pub(crate) refuse: Cell<bool>,
        pub(crate) stops: Rc<Cell<u32>>,
        pub(crate) clock: Rc<Cell<f64>>,
        pub(crate) tail: Rc<RefCell<Vec<u8>>>,
        /// While false, `open` stays pending (the permission prompt).
        pub(crate) granted: Rc<Cell<bool>>,
    }

    impl Default for FakeMic {
        fn default() -> Self {
            Self {
                sink: RefCell::default(),
                refuse: Cell::default(),
                stops: Rc::default(),
                clock: Rc::default(),
                tail: Rc::default(),
                granted: Rc::new(Cell::new(true)),
            }
        }
    }

    /// Resolves once the fake permission prompt is answered.
    pub(crate) struct Prompt {
        granted: Rc<Cell<bool>>,
        result: Option<Result<FakeSession, CaptureError>>,
    }

    impl Future for Prompt {
        type Output = Result<FakeSession, CaptureError>;

        fn poll(
            mut self: core::pin::Pin<&mut Self>,
            _: &mut core::task::Context<'_>,
        ) -> core::task::Poll<Self::Output> {
            if self.granted.get() {
                core::task::Poll::Ready(self.result.take().expect("polled once ready"))
            } else {
                core::task::Poll::Pending
            }
        }
    }

    #[derive(Clone)]
    pub(crate) struct FakeSession {
        sink: EventSink,
        stops: Rc<Cell<u32>>,
        clock: Rc<Cell<f64>>,
        tail: Rc<RefCell<Vec<u8>>>,
    }

    impl CaptureSession for FakeSession {
        fn stop(&self) {
            self.stops.set(self.stops.get() + 1);
            if self.stops.get() > 1 {
                return;
            }
            let tail = core::mem::take(&mut *self.tail.borrow_mut());
            if !tail.is_empty() {
                (self.sink)(CaptureEvent::Data(tail));
            }
            (self.sink)(CaptureEvent::Ended {
                at_ms: self.clock.get(),
            });
        }
    }

    impl AudioCapture for &FakeMic {
        type Session = FakeSession;

        fn open(&self, sink: EventSink) -> impl Future<Output = Result<FakeSession, CaptureError>> {
            let result = if self.refuse.get() {
                Err(CaptureError::Denied)
            } else {
                *self.sink.borrow_mut() = Some(sink.clone());
                sink(CaptureEvent::Started {
                    at_ms: self.clock.get(),
                });
                Ok(FakeSession {
                    sink,
                    stops: self.stops.clone(),
                    clock: self.clock.clone(),
                    tail: self.tail.clone(),
                })
            };
            Prompt {
                granted: self.granted.clone(),
                result: Some(result),
            }
        }
    }

    impl FakeMic {
        pub(crate) fn emit(&self, event: CaptureEvent) {
            let sink = self.sink.borrow().clone().expect("mic is open");
            sink(event);
        }

        /// Advances the clock by `ms` and reports a frame at `rms`.
        pub(crate) fn frame(&self, ms: f64, rms: f32) {
            self.clock.set(self.clock.get() + ms);
            self.emit(CaptureEvent::Level {
                at_ms: self.clock.get(),
                rms,
            });
        }
    }

    type Log = Rc<RefCell<Vec<Update>>>;

    fn record(mic: &FakeMic, config: RecorderConfig) -> (Dictaphone<&FakeMic>, Log) {
        let phone = Dictaphone::new(mic, config);
        let log: Log = Rc::default();
        let sink = log.clone();
        block_on(phone.start(move |u| sink.borrow_mut().push(u))).expect("starts");
        (phone, log)
    }

    fn finished(log: &Log) -> Option<Recording> {
        log.borrow().iter().find_map(|u| match u {
            Update::Finished(r) => Some(r.clone()),
            _ => None,
        })
    }

    #[test]
    fn a_tap_records_until_stopped() {
        let mic = FakeMic::default();
        let (phone, log) = record(&mic, RecorderConfig::default());
        assert!(phone.is_recording());
        mic.frame(100.0, 0.2);
        mic.emit(CaptureEvent::Data(vec![1, 2]));
        mic.frame(1_100.0, 0.2);
        mic.tail.borrow_mut().extend([3, 4]);
        assert_eq!(phone.stop(), Halt::Stopping);
        assert_eq!(phone.stop(), Halt::Idle, "already stopped");
        assert!(!phone.is_recording());
        let recording = finished(&log).expect("finished");
        assert_eq!(recording.bytes, [1, 2, 3, 4]);
        assert_eq!(recording.duration_ms, 1_200);
        assert_eq!(recording.stopped_by, StopReason::User);
        // The meter followed the stream.
        let levels = log
            .borrow()
            .iter()
            .filter(|u| matches!(u, Update::Level { .. }))
            .count();
        assert_eq!(levels, 2);
        let Some(Update::Level { bars, elapsed_ms }) = log.borrow().get(1).cloned() else {
            panic!("no level")
        };
        assert_eq!(elapsed_ms, 1_200);
        assert!(*bars.last().unwrap() > 50, "{bars:?}");
    }

    #[test]
    fn silence_after_speech_stops_by_itself() {
        let mic = FakeMic::default();
        let (phone, log) = record(&mic, RecorderConfig::default());
        mic.emit(CaptureEvent::Data(vec![9]));
        // Quiet start: never stops.
        for _ in 0..100 {
            mic.frame(50.0, 0.0);
        }
        assert!(phone.is_recording());
        mic.frame(50.0, 0.3);
        for _ in 0..50 {
            mic.frame(50.0, 0.001);
        }
        assert!(phone.is_recording(), "2.45 s of quiet");
        mic.frame(50.0, 0.001);
        assert!(!phone.is_recording());
        assert!(
            log.borrow()
                .contains(&Update::Stopping(StopReason::Silence))
        );
        assert_eq!(
            finished(&log).map(|r| r.stopped_by),
            Some(StopReason::Silence)
        );
        assert_eq!(mic.stops.get(), 1);
    }

    #[test]
    fn silence_never_cuts_a_hold_short() {
        let mic = FakeMic::default();
        let (phone, log) = record(&mic, RecorderConfig::default());
        phone.hold(true);
        mic.emit(CaptureEvent::Data(vec![9]));
        mic.frame(50.0, 0.3);
        mic.frame(50.0, 0.0);
        mic.frame(10_000.0, 0.0);
        assert!(phone.is_recording(), "a pause while holding");
        // Let go without stopping (say the release was lost): silence
        // takes over again.
        phone.hold(false);
        mic.frame(50.0, 0.0);
        assert!(!phone.is_recording());
        assert_eq!(
            finished(&log).map(|r| r.stopped_by),
            Some(StopReason::Silence)
        );
    }

    #[test]
    fn a_take_without_auto_stop_runs_through_pauses_until_the_cap() {
        let mic = FakeMic::default();
        let phone = Dictaphone::new(
            &mic,
            RecorderConfig {
                silence: Some(SilenceConfig::default()),
                max_ms: 10_000.0,
            },
        );
        phone.auto_stop(false);
        let log = Rc::new(RefCell::new(Vec::new()));
        let sink = log.clone();
        block_on(phone.start(move |u| sink.borrow_mut().push(u))).expect("starts");
        mic.emit(CaptureEvent::Data(vec![9]));
        mic.frame(100.0, 0.3);
        for _ in 0..80 {
            mic.frame(50.0, 0.0);
        }
        assert!(phone.is_recording(), "a 4 s pause mid-thought");
        mic.frame(6_000.0, 0.0);
        assert!(!phone.is_recording());
        assert_eq!(
            finished(&log).map(|r| r.stopped_by),
            Some(StopReason::TooLong)
        );
    }

    #[test]
    fn auto_stop_can_be_turned_off_and_length_is_capped() {
        let mic = FakeMic::default();
        let config = RecorderConfig {
            silence: None,
            max_ms: 10_000.0,
        };
        let (phone, log) = record(&mic, config);
        mic.emit(CaptureEvent::Data(vec![9]));
        mic.frame(100.0, 0.3);
        mic.frame(5_000.0, 0.0);
        assert!(phone.is_recording(), "silence ignored");
        mic.frame(5_000.0, 0.0);
        assert!(!phone.is_recording());
        assert_eq!(
            finished(&log).map(|r| r.stopped_by),
            Some(StopReason::TooLong)
        );
    }

    #[test]
    fn without_a_meter_the_length_cap_still_applies() {
        let mic = FakeMic::default();
        let config = RecorderConfig {
            silence: Some(SilenceConfig::default()),
            max_ms: 10_000.0,
        };
        let (phone, log) = record(&mic, config);
        mic.emit(CaptureEvent::Data(vec![9]));
        mic.clock.set(9_000.0);
        mic.emit(CaptureEvent::Tick { at_ms: 9_000.0 });
        assert!(phone.is_recording());
        assert!(log.borrow().contains(&Update::Level {
            bars: vec![0; super::super::level::METER_BARS],
            elapsed_ms: 9_000
        }));
        mic.clock.set(10_000.0);
        mic.emit(CaptureEvent::Tick { at_ms: 10_000.0 });
        assert!(!phone.is_recording());
        assert_eq!(
            finished(&log).map(|r| r.stopped_by),
            Some(StopReason::TooLong)
        );
    }

    #[test]
    fn a_refused_microphone_leaves_it_idle() {
        let mic = FakeMic::default();
        mic.refuse.set(true);
        let phone = Dictaphone::new(&mic, RecorderConfig::default());
        assert_eq!(block_on(phone.start(|_| {})), Err(CaptureError::Denied));
        assert!(!phone.is_recording());
        mic.refuse.set(false);
        assert_eq!(block_on(phone.start(|_| {})), Ok(()));
        assert_eq!(block_on(phone.start(|_| {})), Err(CaptureError::Busy));
    }

    #[test]
    fn nothing_recorded_and_broken_captures_fail() {
        let mic = FakeMic::default();
        let (phone, log) = record(&mic, RecorderConfig::default());
        phone.stop();
        assert_eq!(
            log.borrow().last(),
            Some(&Update::Failed(CaptureError::Empty))
        );
        let (phone, log) = record(&mic, RecorderConfig::default());
        mic.emit(CaptureEvent::Failed("encoder".into()));
        assert!(!phone.is_recording());
        assert_eq!(
            log.borrow().last(),
            Some(&Update::Failed(CaptureError::Failed("encoder".into())))
        );
    }

    #[test]
    fn a_user_stop_says_so_before_the_recording_arrives() {
        let mic = FakeMic::default();
        let (phone, log) = record(&mic, RecorderConfig::default());
        mic.emit(CaptureEvent::Data(vec![1]));
        phone.stop();
        let log = log.borrow();
        assert_eq!(log[0], Update::Stopping(StopReason::User));
        assert!(matches!(log[1], Update::Finished(_)));
    }

    #[test]
    fn a_stop_during_the_permission_prompt_cancels_quietly() {
        use core::pin::pin;
        use core::task::{Context, Poll, Waker};
        let mic = FakeMic::default();
        mic.granted.set(false);
        let phone = Dictaphone::new(&mic, RecorderConfig::default());
        let log: Log = Rc::default();
        let sink = log.clone();
        let mut start = pin!(phone.start(move |u| sink.borrow_mut().push(u)));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(start.as_mut().poll(&mut cx).is_pending());
        assert!(phone.is_recording(), "opening");
        assert_eq!(phone.stop(), Halt::Cancelled);
        mic.granted.set(true);
        assert!(!phone.is_recording(), "released at once");
        assert_eq!(start.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        // The microphone opened and was closed straight away, silently.
        assert!(mic.stops.get() >= 1);
        assert!(!phone.is_recording());
        assert!(log.borrow().is_empty(), "{:?}", log.borrow());
    }

    #[test]
    fn a_device_that_goes_away_keeps_what_was_recorded() {
        let mic = FakeMic::default();
        let (_phone, log) = record(&mic, RecorderConfig::default());
        mic.emit(CaptureEvent::Data(vec![1]));
        mic.emit(CaptureEvent::Ended { at_ms: 800.0 });
        let recording = finished(&log).expect("kept");
        assert_eq!(recording.stopped_by, StopReason::Interrupted);
        assert_eq!(recording.duration_ms, 800);
    }

    #[test]
    fn a_hidden_page_stops_the_take_and_keeps_what_was_recorded() {
        let mic = FakeMic::default();
        let (phone, log) = record(&mic, RecorderConfig::default());
        mic.frame(100.0, 0.2);
        mic.emit(CaptureEvent::Data(vec![1, 2]));
        mic.frame(900.0, 0.2);
        mic.tail.borrow_mut().extend([3]);
        mic.emit(CaptureEvent::Hidden);
        assert_eq!(mic.stops.get(), 1, "the capture was stopped");
        assert!(log.borrow().contains(&Update::Stopping(StopReason::Hidden)));
        let recording = finished(&log).expect("kept");
        assert_eq!(recording.bytes, [1, 2, 3]);
        assert_eq!(recording.duration_ms, 1_000);
        assert_eq!(recording.stopped_by, StopReason::Hidden);
        assert!(!phone.is_recording());
    }

    #[test]
    fn a_hidden_page_does_not_change_why_a_stopping_take_stopped() {
        let mut recorder = Recorder::new(RecorderConfig::default());
        recorder.feed(CaptureEvent::Started { at_ms: 0.0 });
        assert!(recorder.request_stop(StopReason::User));
        assert_eq!(recorder.feed(CaptureEvent::Hidden), Step::Continue);
        recorder.feed(CaptureEvent::Data(vec![1]));
        let Step::Finished(recording) = recorder.feed(CaptureEvent::Ended { at_ms: 10.0 }) else {
            panic!("finished");
        };
        assert_eq!(recording.stopped_by, StopReason::User);
    }

    #[test]
    fn taps_toggle_and_holds_talk() {
        let mut gesture = MicGesture::default();
        // Tap: start, keep recording on release, the next tap stops.
        assert_eq!(gesture.press(0.0, false), MicIntent::Start);
        assert_eq!(gesture.release(120.0, false), None);
        assert_eq!(gesture.press(3_000.0, true), MicIntent::Stop);
        assert_eq!(gesture.release(3_050.0, false), None);
        // Hold: letting go stops.
        assert_eq!(gesture.press(5_000.0, false), MicIntent::Start);
        assert_eq!(
            gesture.release(5_000.0 + HOLD_MS, false),
            Some(MicIntent::Stop)
        );
        assert_eq!(
            gesture.release(9_000.0, false),
            None,
            "one release per press"
        );
    }

    #[test]
    fn letting_go_during_the_permission_prompt_keeps_the_take() {
        use core::pin::pin;
        use core::task::{Context, Waker};
        let mic = FakeMic::default();
        mic.granted.set(false);
        let phone = Dictaphone::new(&mic, RecorderConfig::default());
        let log: Log = Rc::default();
        let sink = log.clone();
        let mut gesture = MicGesture::default();
        assert_eq!(gesture.press(0.0, phone.is_recording()), MicIntent::Start);
        phone.hold(true);
        let mut start = pin!(phone.start(move |u| sink.borrow_mut().push(u)));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(start.as_mut().poll(&mut cx).is_pending());
        assert!(phone.is_opening());
        // The prompt cancels the pointer well after the hold threshold.
        assert_eq!(gesture.release(2_000.0, phone.is_opening()), None);
        phone.hold(false);
        mic.granted.set(true);
        assert!(start.as_mut().poll(&mut cx).is_ready());
        assert!(!phone.is_opening());
        assert!(phone.is_recording(), "recording once allowed");
        mic.emit(CaptureEvent::Data(vec![5]));
        // The next tap stops it, and keeps what was said.
        assert_eq!(
            gesture.press(4_000.0, phone.is_recording()),
            MicIntent::Stop
        );
        assert_eq!(phone.stop(), Halt::Stopping);
        assert_eq!(finished(&log).map(|r| r.bytes), Some(vec![5]));
    }

    #[test]
    fn a_second_tap_right_after_the_first_is_a_stop() {
        let mic = FakeMic::default();
        let phone = Dictaphone::new(&mic, RecorderConfig::default());
        let mut gesture = MicGesture::default();
        assert_eq!(gesture.press(0.0, phone.is_recording()), MicIntent::Start);
        // Not yet polled (the task that opens the microphone hasn't run).
        let pending = phone.start(|_| {});
        assert!(phone.is_recording(), "current before the first poll");
        assert_eq!(gesture.release(80.0, phone.is_opening()), None);
        assert_eq!(gesture.press(150.0, phone.is_recording()), MicIntent::Stop);
        assert_eq!(phone.stop(), Halt::Cancelled);
        assert_eq!(block_on(pending), Ok(()));
        assert!(!phone.is_recording());
    }
}
