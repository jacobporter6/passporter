//! Which microphone a dictation records from: the app's native recorder
//! when the page runs in the APK and the plugin answers
//! ([`super::native`]), else the browser's `MediaRecorder`
//! ([`super::browser`]), unchanged. The choice is made once per page
//! session ([`native::backend`]), on the first take at the latest.

use protocol::AudioId;

use super::browser::{MediaCapture, MicSession};
use super::native::{self, Backend, NativeCapture, NativeSession};
use super::recorder::{AudioCapture, CaptureError, CaptureSession, EventSink};

/// One agent's microphone, either backend.
#[derive(Debug, Clone)]
pub struct Capture {
    media: MediaCapture,
    native: NativeCapture,
}

impl Capture {
    /// The microphone for `agent`'s takes, each at most `max_ms` long.
    #[must_use]
    pub fn new(agent: String, max_ms: u64) -> Self {
        Self {
            media: MediaCapture::default(),
            native: NativeCapture::new(agent, max_ms),
        }
    }

    /// Readies the browser's meter inside a user gesture
    /// ([`MediaCapture::prime`]), unless the native recorder is known to be
    /// in use (it needs no audio context).
    pub fn prime(&self) {
        if native::known_backend() != Some(Backend::Native) {
            self.media.prime();
        }
    }

    /// [`MediaCapture::resume_meter`]: harmless with the native recorder,
    /// where there is no meter context to resume.
    pub fn resume_meter(&self) {
        self.media.resume_meter();
    }
}

/// A running take from either backend.
#[derive(Debug, Clone)]
pub enum Session {
    /// The browser's.
    Media(MicSession),
    /// The app's.
    Native(NativeSession),
}

impl CaptureSession for Session {
    fn stop(&self) {
        match self {
            Self::Media(session) => session.stop(),
            Self::Native(session) => session.stop(),
        }
    }

    fn cancel(&self) {
        match self {
            Self::Media(session) => session.cancel(),
            Self::Native(session) => session.cancel(),
        }
    }

    fn take_id(&self) -> Option<AudioId> {
        match self {
            Self::Media(session) => session.take_id(),
            Self::Native(session) => session.take_id(),
        }
    }
}

impl AudioCapture for Capture {
    type Session = Session;

    async fn open(&self, sink: EventSink) -> Result<Session, CaptureError> {
        match native::backend().await {
            Backend::Native => self.native.open(sink).await.map(Session::Native),
            Backend::Media => self.media.open(sink).await.map(Session::Media),
        }
    }
}
