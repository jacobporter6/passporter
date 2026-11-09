//! A dictation transcript (`GET /v1/audio/:id/transcript`, design §6.3).
//! Outbound: tolerant.

use serde::{Deserialize, Serialize};

use crate::ids::AudioId;

/// The server-side transcription of one uploaded audio clip. The client shows
/// it as an editable card; nothing is sent until the user confirms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transcript {
    /// The clip this transcribes.
    pub audio_id: AudioId,
    /// The recognized text.
    pub text: String,
    /// Clip length in milliseconds.
    pub duration_ms: u64,
}
