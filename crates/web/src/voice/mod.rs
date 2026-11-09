//! Dictation (design §6.3), the phone side of the bridge's `/v1/audio`.
//!
//! Record first, transcribe later: the mic button on the composer records
//! (tap to toggle, hold to talk) with a live meter (and, only when
//! reviewing, silence auto-stop: a take that sends runs until tapped);
//! the clip is written to `IndexedDB` before anything touches the network;
//! it is then uploaded in resumable chunks, and the bridge's transcript
//! is sent to the agent as a prompt through the outbox, after anything
//! typed in the composer. Tap to start, tap again (or the strip's Send,
//! or let go of a hold) to send; Cancel while it transcribes discards it;
//! nothing heard sends nothing. With "Review dictation before sending"
//! on (Settings), the transcript comes back as a card whose "Use" fills
//! the composer instead.
//!
//! - [`level`]: loudness, the meter and silence detection.
//! - [`recorder`]: the [`recorder::AudioCapture`] trait (a fake stream in
//!   tests), the pure recording state machine and the tap/hold gesture.
//! - [`clip`]: the stored clip and the [`clip::ClipStore`] trait.
//! - [`upload`]: requests, responses and the resumable upload.
//! - [`courier`]: store, upload, transcript, in that order, resumable.
//! - [`browser`]: `MediaRecorder`, `IndexedDB` and `fetch` behind those
//!   traits.
//! - [`engine`]: each agent's dictation, driving all of the above. It runs
//!   in its own WASM (`src/bin/voice.rs`), loaded on demand by [`module`],
//!   which keeps it out of the app's size budget.
//! - [`phase`]: where a dictation is, what its strip shows and what the
//!   mic button can do. [`panel`]: the strip, drawn by the dictation WASM.
//! - [`view`]: the mic button, and the element the strip is drawn into,
//!   in the app.
//! - [`wake`]: keeps the screen on while a take is under way.
//!
//! The microphone needs a secure context; outside one the mic button is
//! not shown at all ([`browser::available`]).

pub mod browser;
pub mod clip;
pub mod courier;
pub mod engine;
pub mod level;
pub mod module;
pub mod panel;
pub mod phase;
pub mod recorder;
pub mod upload;
pub mod view;
pub mod wake;

/// Runs a future whose fakes are all ready, for host tests.
#[cfg(test)]
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("a fake future was pending"),
    }
}
