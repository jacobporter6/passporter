//! Dictation uploads (design §5, §6.3): `POST /v1/audio` declares a clip
//! ([`NewUpload`], inbound: strict), and every upload request answers with
//! the clip's [`UploadStatus`] (outbound: tolerant).
//!
//! ```json
//! { "audioId": "9ca09978-ef89-46fd-9a85-118b84bee7b4", "totalBytes": 48213, "durationMs": 4100 }
//! { "audioId": "9ca09978-ef89-46fd-9a85-118b84bee7b4", "offset": 32768, "totalBytes": 48213, "state": "uploading" }
//! ```
//!
//! Size and length caps are the bridge's configuration, so this type only
//! rejects an empty clip; the bridge refuses one over its caps.

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::ids::AudioId;

/// `POST /v1/audio`: declares a clip before its bytes are sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "RawNewUpload")]
pub struct NewUpload {
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_id: Option<AudioId>,
    total_bytes: u64,
    duration_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawNewUpload {
    #[serde(default)]
    audio_id: Option<AudioId>,
    total_bytes: u64,
    duration_ms: u64,
}

impl TryFrom<RawNewUpload> for NewUpload {
    type Error = ValidationError;

    fn try_from(raw: RawNewUpload) -> Result<Self, Self::Error> {
        Self::new(raw.audio_id, raw.total_bytes, raw.duration_ms)
    }
}

impl NewUpload {
    /// A clip of `total_bytes` bytes and `duration_ms` milliseconds, both
    /// non-zero. `audio_id` is the phone's own id for it (a UUID made at
    /// record time): declaring it again returns the upload's current state,
    /// so a phone that lost the reply can resume. `None` lets the bridge
    /// pick one.
    pub fn new(
        audio_id: Option<AudioId>,
        total_bytes: u64,
        duration_ms: u64,
    ) -> Result<Self, ValidationError> {
        if total_bytes == 0 {
            return Err(ValidationError::Empty {
                field: "totalBytes",
            });
        }
        if duration_ms == 0 {
            return Err(ValidationError::Empty {
                field: "durationMs",
            });
        }
        Ok(Self {
            audio_id,
            total_bytes,
            duration_ms,
        })
    }

    /// The phone's id for the clip, if it chose one.
    #[must_use]
    pub fn audio_id(&self) -> Option<&AudioId> {
        self.audio_id.as_ref()
    }

    /// Exact size of the clip in bytes.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Length of the recording in milliseconds.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
}

/// Where a clip is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipState {
    /// Waiting for more bytes.
    Uploading,
    /// Complete, waiting for transcription.
    Queued,
    /// Being transcribed.
    Transcribing,
    /// Transcript ready (`GET /v1/audio/:id/transcript`).
    Done,
    /// Transcription failed; see [`UploadStatus::failure`].
    Failed,
    /// A state this app predates.
    #[serde(other)]
    Unknown,
}

/// Why a clip's transcription failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFailure {
    /// The backend failed or could not decode the clip. Declaring the
    /// same clip again retries it.
    Transcription,
    /// The decoded clip is longer than the bridge's duration cap.
    TooLong,
    /// A reason this app predates.
    #[serde(other)]
    Unknown,
}

/// A clip's upload progress: the reply to every upload request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadStatus {
    /// The clip.
    pub audio_id: AudioId,
    /// Bytes received; the next chunk must start here.
    pub offset: u64,
    /// The declared size.
    pub total_bytes: u64,
    /// Where it is.
    pub state: ClipState,
    /// Why it failed, for [`ClipState::Failed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<AudioFailure>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";

    #[test]
    fn new_upload_round_trips() {
        let json = format!(r#"{{"audioId":"{ID}","totalBytes":10,"durationMs":900}}"#);
        let upload: NewUpload = serde_json::from_str(&json).expect("valid");
        assert_eq!(upload.audio_id().map(AudioId::as_str), Some(ID));
        assert_eq!((upload.total_bytes(), upload.duration_ms()), (10, 900));
        assert_eq!(serde_json::to_string(&upload).expect("serialize"), json);
        let anonymous: NewUpload =
            serde_json::from_str(r#"{"totalBytes":1,"durationMs":1}"#).expect("valid");
        assert_eq!(anonymous.audio_id(), None);
    }

    #[test]
    fn new_upload_is_strict() {
        for bad in [
            r#"{"totalBytes":0,"durationMs":1}"#,
            r#"{"totalBytes":1,"durationMs":0}"#,
            r#"{"totalBytes":1,"durationMs":1,"codec":"opus"}"#,
            r#"{"audioId":"NOT-A-UUID","totalBytes":1,"durationMs":1}"#,
            r#"{"totalBytes":1}"#,
        ] {
            assert!(serde_json::from_str::<NewUpload>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn status_is_tolerant() {
        let json = format!(
            r#"{{"audioId":"{ID}","offset":3,"totalBytes":9,"state":"paused","failure":"cosmic_rays","extra":1}}"#
        );
        let status: UploadStatus = serde_json::from_str(&json).expect("tolerant");
        assert_eq!(status.state, ClipState::Unknown);
        assert_eq!(status.failure, Some(AudioFailure::Unknown));
        let failed = UploadStatus {
            state: ClipState::Failed,
            failure: Some(AudioFailure::TooLong),
            ..status
        };
        let text = serde_json::to_string(&failed).expect("serialize");
        assert!(
            text.contains(r#""state":"failed","failure":"too_long""#),
            "{text}"
        );
    }
}
