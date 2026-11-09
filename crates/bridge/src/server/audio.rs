//! The dictation endpoints (design §5, §6.3), over [`crate::audio::Audio`]:
//!
//! - `POST /v1/audio` with a JSON [`NewUpload`](protocol::NewUpload) declares a clip: `201` and
//!   its [`UploadStatus`](protocol::UploadStatus) (`200` if the phone's own `audioId` was declared
//!   before, so a lost reply is harmless; a clip whose transcription
//!   failed is queued again). Over a cap: `413` (size) or `422` (length);
//!   `503` when too many clips are in flight or the id is busy for a moment
//!   (retry later); `409` when the id is in use by a different clip (body:
//!   its status) or unavailable (plain text: choose another id).
//! - `PATCH /v1/audio/:id` with `Upload-Offset: <n>` and raw bytes appends
//!   a chunk at `n`: `200` and the new status. A chunk at any other offset
//!   (a retry, a gap, a write already in progress) is a `409` carrying the
//!   status, whose `offset` is where to resume. The last byte queues the
//!   clip for transcription. This route has its own, longer timeout
//!   ([`AudioSettings::chunk_timeout`](crate::audio::AudioSettings::chunk_timeout)).
//! - `GET /v1/audio/:id` is the status, for resuming after a restart.
//! - `GET /v1/audio/:id/transcript` is the [`Transcript`](protocol::Transcript)
//!   (`200`), or the status while it is pending (`202`) or once it failed
//!   (`422`).
//!
//! A clip belongs to the device that declared it ([`Principal::device`]):
//! to every other device its id is a `404` (`409`, choose another, when
//! declared).

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use protocol::{AudioId, NewUpload};

use super::{AppState, Principal};
use crate::audio::{Created, TranscriptView, UploadError};

/// The chunk offset header (as in tus).
pub(crate) const UPLOAD_OFFSET: &str = "upload-offset";

/// `POST /v1/audio`.
pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    body: Bytes,
) -> Response {
    let Ok(upload) = serde_json::from_slice::<NewUpload>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match state.audio.create(&principal.device, upload).await {
        Ok(Created::New(status)) => (StatusCode::CREATED, Json(status)).into_response(),
        Ok(Created::Existing(status)) => Json(status).into_response(),
        Err(error) => refuse(error),
    }
}

/// `GET /v1/audio/:id`.
pub(crate) async fn status(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(raw): Path<String>,
) -> Response {
    let Ok(id) = AudioId::new(raw) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match state.audio.status(&principal.device, &id) {
        Ok(status) => Json(status).into_response(),
        Err(error) => refuse(error),
    }
}

/// `PATCH /v1/audio/:id`.
pub(crate) async fn append(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(id) = AudioId::new(raw) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let offset = headers
        .get(UPLOAD_OFFSET)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let Some(offset) = offset else {
        return (
            StatusCode::BAD_REQUEST,
            "missing or malformed Upload-Offset",
        )
            .into_response();
    };
    match state
        .audio
        .append(&principal.device, &id, offset, body)
        .await
    {
        Ok(status) => Json(status).into_response(),
        Err(error) => refuse(error),
    }
}

/// `GET /v1/audio/:id/transcript`.
pub(crate) async fn transcript(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(raw): Path<String>,
) -> Response {
    let Ok(id) = AudioId::new(raw) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match state.audio.transcript(&principal.device, &id) {
        Ok(TranscriptView::Ready(transcript)) => Json(transcript).into_response(),
        Ok(TranscriptView::Pending(status)) => (StatusCode::ACCEPTED, Json(status)).into_response(),
        Ok(TranscriptView::Failed(status)) => {
            (StatusCode::UNPROCESSABLE_ENTITY, Json(status)).into_response()
        }
        Err(error) => refuse(error),
    }
}

fn refuse(error: UploadError) -> Response {
    let message = error.to_string();
    match error {
        UploadError::NotFound => StatusCode::NOT_FOUND.into_response(),
        UploadError::TooLarge { .. } => (StatusCode::PAYLOAD_TOO_LARGE, message).into_response(),
        UploadError::TooLong { .. } => (StatusCode::UNPROCESSABLE_ENTITY, message).into_response(),
        UploadError::Full | UploadError::Busy => {
            (StatusCode::SERVICE_UNAVAILABLE, message).into_response()
        }
        UploadError::IdUnavailable => (StatusCode::CONFLICT, message).into_response(),
        UploadError::IdConflict(status) | UploadError::OffsetMismatch(status) => {
            (StatusCode::CONFLICT, Json(status)).into_response()
        }
        UploadError::PastEnd(status) => (StatusCode::BAD_REQUEST, Json(status)).into_response(),
        UploadError::Io(error) => {
            tracing::error!(%error, "audio storage failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
