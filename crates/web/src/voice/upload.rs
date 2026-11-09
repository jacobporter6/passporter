//! The phone side of the bridge's dictation endpoints (design §5, §6.3;
//! the bridge side is `crates/bridge/src/server/audio.rs`):
//!
//! - `POST /v1/audio` declares the clip under the phone's own id; declaring
//!   it again answers with the upload's current offset, so a reload or a
//!   lost reply resumes instead of starting over;
//! - `PATCH /v1/audio/:id` with `Upload-Offset` sends one chunk at the
//!   offset the bridge last reported. A `409` carries the bridge's offset,
//!   which is where the next chunk goes;
//! - `GET /v1/audio/:id/transcript` is the transcript (`200`), the status
//!   while pending (`202`) or once failed (`422`).
//!
//! Requests and responses are pure values; an [`AudioTransport`] sends
//! them (raw bytes, unlike the JSON-only [`crate::api::Transport`]).

use core::future::Future;

use protocol::{AudioId, ClipState, NewUpload, Transcript, UploadStatus};
use serde::de::DeserializeOwned;

use super::clip::PendingClip;
use crate::api::response::BODY_LOST;
use crate::api::{ApiError, HttpResponse, Method};

/// The declare endpoint, and the prefix of every clip's path.
pub const AUDIO_PATH: &str = "/v1/audio";

/// Bytes per `PATCH`: well under the bridge's 1 MiB default, so a chunk
/// finishes in seconds on a weak mobile uplink and a failure costs little.
pub const CHUNK_BYTES: usize = 256 * 1024;

/// `409`s in a row that don't move the offset before giving up for now
/// (another tab or a retried request is writing the same clip).
const MAX_STALLED_CONFLICTS: u32 = 3;

/// A request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioBody<'a> {
    /// None.
    Empty,
    /// JSON text.
    Json(String),
    /// Raw audio bytes, borrowed from the clip.
    Audio(&'a [u8]),
}

/// One request to the audio endpoints, relative to the app's origin. The
/// transport adds the bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRequest<'a> {
    /// The method.
    pub method: Method,
    /// Absolute path.
    pub path: String,
    /// The `Upload-Offset` header, for chunks.
    pub upload_offset: Option<u64>,
    /// The body.
    pub body: AudioBody<'a>,
}

/// Sends one audio request. Resolves to `Err` only when there was no HTTP
/// response at all; any status is an `Ok`.
pub trait AudioTransport {
    /// Sends `request`.
    fn send(
        &self,
        request: AudioRequest<'_>,
    ) -> impl Future<Output = Result<HttpResponse, ApiError>>;
}

/// `POST /v1/audio`.
pub fn declare_request(upload: &NewUpload) -> Result<AudioRequest<'static>, serde_json::Error> {
    Ok(AudioRequest {
        method: Method::Post,
        path: AUDIO_PATH.to_owned(),
        upload_offset: None,
        body: AudioBody::Json(serde_json::to_string(upload)?),
    })
}

/// `PATCH /v1/audio/:id` with `chunk` at `offset`. [`AudioId`] is a UUID,
/// so it goes into the path unescaped.
#[must_use]
pub fn append_request<'a>(id: &AudioId, offset: u64, chunk: &'a [u8]) -> AudioRequest<'a> {
    AudioRequest {
        method: Method::Patch,
        path: format!("{AUDIO_PATH}/{id}"),
        upload_offset: Some(offset),
        body: AudioBody::Audio(chunk),
    }
}

/// `GET /v1/audio/:id/transcript`.
#[must_use]
pub fn transcript_request(id: &AudioId) -> AudioRequest<'static> {
    AudioRequest {
        method: Method::Get,
        path: format!("{AUDIO_PATH}/{id}/transcript"),
        upload_offset: None,
        body: AudioBody::Empty,
    }
}

/// Why an upload or poll did not get through.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    /// No (usable) response, a 5xx, or the token was rejected.
    #[error(transparent)]
    Api(#[from] ApiError),
    /// The bridge doesn't know the clip (swept, or never declared): declare
    /// it again.
    #[error("the bridge no longer has this clip")]
    Gone,
    /// The id is taken by a different clip, or unavailable: pick another
    /// and declare again.
    #[error("the clip id is taken")]
    IdTaken,
    /// The caller gave up on the clip (it was discarded).
    #[error("the upload was abandoned")]
    Cancelled,
    /// The bridge refused the clip for good (over its size or length cap).
    #[error("{0}")]
    Refused(String),
    /// Another writer keeps moving the same upload; try again shortly.
    #[error("the bridge is busy with this clip; try again")]
    Busy,
    /// The bridge's idea of the upload doesn't fit the recording.
    #[error("the bridge's copy of this clip doesn't match the recording")]
    Mismatch,
}

impl UploadError {
    /// Whether trying again (re-declaring, then resuming) may succeed
    /// without anything changing first.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Api(error) => error.is_retryable(),
            Self::Gone | Self::Busy => true,
            Self::IdTaken | Self::Cancelled | Self::Refused(_) | Self::Mismatch => false,
        }
    }
}

/// Upload progress, for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes the bridge has.
    pub sent: u64,
    /// The clip's size.
    pub total: u64,
}

/// What a transcript poll found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptPoll {
    /// Transcribed.
    Ready(Transcript),
    /// Still uploading, queued or transcribing.
    Pending(UploadStatus),
    /// Transcription failed (see [`UploadStatus::failure`]).
    Failed(UploadStatus),
}

/// What one chunk did.
enum Chunk {
    /// Appended: the new status.
    Accepted(UploadStatus),
    /// Not at the bridge's offset (a retry, a gap, a concurrent write): the
    /// bridge's status, with the offset to resume from.
    Moved(UploadStatus),
}

/// Sends `clip`'s audio, resuming wherever the bridge got to, and resolves
/// with the status once every byte has landed (the clip is then queued for
/// transcription). Safe to call again after any failure: nothing is sent
/// twice past the bridge's offset.
pub async fn upload<T: AudioTransport>(
    transport: &T,
    clip: &PendingClip,
    chunk_bytes: usize,
    mut progress: impl FnMut(Progress),
    cancelled: impl Fn() -> bool,
) -> Result<UploadStatus, UploadError> {
    let id = &clip.meta.audio_id;
    let audio = clip.audio.as_slice();
    let total = u64::try_from(audio.len()).map_err(|_| UploadError::Mismatch)?;
    let declared = NewUpload::new(Some(id.clone()), total, clip.meta.duration_ms)
        .map_err(|_| UploadError::Mismatch)?;
    let mut status = declare(transport, &declared).await?;
    if status.total_bytes != total {
        return Err(UploadError::Mismatch);
    }
    let mut stalled = 0;
    while status.state == ClipState::Uploading {
        if cancelled() {
            return Err(UploadError::Cancelled);
        }
        progress(Progress {
            sent: status.offset,
            total,
        });
        let start = usize::try_from(status.offset)
            .ok()
            .filter(|&start| start < audio.len())
            .ok_or(UploadError::Mismatch)?;
        let end = start.saturating_add(chunk_bytes.max(1)).min(audio.len());
        let request = append_request(id, status.offset, &audio[start..end]);
        match append(transport, request).await? {
            Chunk::Accepted(next) => {
                stalled = 0;
                status = next;
            }
            Chunk::Moved(next) => {
                stalled = if next.offset == status.offset {
                    stalled + 1
                } else {
                    0
                };
                if stalled >= MAX_STALLED_CONFLICTS {
                    return Err(UploadError::Busy);
                }
                status = next;
            }
        }
    }
    progress(Progress { sent: total, total });
    Ok(status)
}

/// Asks for `id`'s transcript.
pub async fn poll_transcript<T: AudioTransport>(
    transport: &T,
    id: &AudioId,
) -> Result<TranscriptPoll, UploadError> {
    let response = transport.send(transcript_request(id)).await?;
    let (status, body) = usable(&response)?;
    match status {
        200 => json(body).map(TranscriptPoll::Ready),
        202 => json(body).map(TranscriptPoll::Pending),
        422 => json(body).map(TranscriptPoll::Failed),
        status => Err(ApiError::Status { status }.into()),
    }
}

async fn declare<T: AudioTransport>(
    transport: &T,
    upload: &NewUpload,
) -> Result<UploadStatus, UploadError> {
    let request = declare_request(upload).map_err(|e| ApiError::Encode(e.to_string()))?;
    let response = transport.send(request).await?;
    let (status, body) = usable(&response)?;
    match status {
        200 | 201 => json(body),
        409 => Err(UploadError::IdTaken),
        413 | 422 => Err(UploadError::Refused(refusal(status, body))),
        status => Err(ApiError::Status { status }.into()),
    }
}

async fn append<T: AudioTransport>(
    transport: &T,
    request: AudioRequest<'_>,
) -> Result<Chunk, UploadError> {
    let response = transport.send(request).await?;
    let (status, body) = usable(&response)?;
    match status {
        200 => json(body).map(Chunk::Accepted),
        409 => serde_json::from_str(body)
            .map(Chunk::Moved)
            .map_err(|_| ApiError::Status { status }.into()),
        400 => Err(UploadError::Mismatch),
        status => Err(ApiError::Status { status }.into()),
    }
}

/// The status and body of a response worth reading: 401, a lost body, a
/// 5xx and a 404 are errors here.
fn usable(response: &HttpResponse) -> Result<(u16, &str), UploadError> {
    let status = response.status;
    if status == crate::api::response::UNAUTHORIZED {
        return Err(ApiError::Unauthorized.into());
    }
    let Some(body) = response.body.as_deref() else {
        return Err(ApiError::Transport(BODY_LOST.to_owned()).into());
    };
    match status {
        500.. => Err(ApiError::Status { status }.into()),
        404 => Err(UploadError::Gone),
        _ => Ok((status, body)),
    }
}

fn json<T: DeserializeOwned>(body: &str) -> Result<T, UploadError> {
    serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()).into())
}

/// A people-facing reason for a refused clip: the bridge's own text, which
/// names its cap, or a fallback.
fn refusal(status: u16, body: &str) -> String {
    let text = body.trim();
    if !text.is_empty() && text.len() <= 200 && !text.starts_with('{') {
        return format!("The bridge refused this recording: {text}.");
    }
    if status == 413 {
        "This recording is too big for the bridge.".to_owned()
    } else {
        "This recording is too long for the bridge.".to_owned()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use core::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use protocol::{AgentId, Timestamp};

    use super::super::block_on;
    use super::super::clip::ClipMeta;
    use super::*;

    pub(crate) const ID: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";

    /// How the next chunks fail: after `after` good ones, one chunk fails;
    /// `lands` says whether the bridge kept it before the reply was lost.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct ChunkFailure {
        pub(crate) after: u32,
        pub(crate) lands: bool,
    }

    /// One request the fake bridge saw.
    #[derive(Debug, Clone)]
    pub(crate) struct Logged {
        pub(crate) method: Method,
        pub(crate) offset: Option<u64>,
        pub(crate) body_len: usize,
    }

    #[derive(Debug, Clone)]
    struct FakeClip {
        total: u64,
        received: Vec<u8>,
        duration_ms: u64,
    }

    /// An in-memory bridge, following the real one's rules for offsets,
    /// re-declaration and transcripts.
    #[derive(Default)]
    pub(crate) struct FakeBridge {
        clips: RefCell<HashMap<String, FakeClip>>,
        /// Every request.
        pub(crate) log: RefCell<Vec<Logged>>,
        pub(crate) offline: Cell<bool>,
        pub(crate) chunk_failure: Cell<Option<ChunkFailure>>,
        /// Transcript polls answered `202` before the transcript is ready.
        pub(crate) busy_polls: Cell<u32>,
        /// The transcription fails.
        pub(crate) fail_transcription: Cell<bool>,
    }

    impl FakeBridge {
        pub(crate) fn received(&self, id: &str) -> Option<Vec<u8>> {
            self.clips.borrow().get(id).map(|c| c.received.clone())
        }

        pub(crate) fn forget(&self, id: &str) {
            self.clips.borrow_mut().remove(id);
        }

        fn status(id: &str, clip: &FakeClip, state: ClipState) -> String {
            serde_json::to_string(&UploadStatus {
                audio_id: AudioId::new(id).unwrap(),
                offset: clip.received.len() as u64,
                total_bytes: clip.total,
                state,
                failure: None,
            })
            .unwrap()
        }

        fn state(clip: &FakeClip) -> ClipState {
            if (clip.received.len() as u64) < clip.total {
                ClipState::Uploading
            } else {
                ClipState::Queued
            }
        }

        fn answer(&self, request: AudioRequest<'_>) -> Result<HttpResponse, ApiError> {
            let body_len = match &request.body {
                AudioBody::Empty => 0,
                AudioBody::Json(json) => json.len(),
                AudioBody::Audio(bytes) => bytes.len(),
            };
            self.log.borrow_mut().push(Logged {
                method: request.method,
                offset: request.upload_offset,
                body_len,
            });
            if self.offline.get() {
                return Err(ApiError::Transport("offline".into()));
            }
            let tail = request.path.strip_prefix("/v1/audio").unwrap();
            let id = tail.trim_start_matches('/').trim_end_matches("/transcript");
            match (request.method, request.body) {
                (Method::Post, AudioBody::Json(json)) => {
                    Ok(self.declare(&serde_json::from_str(&json).unwrap()))
                }
                (Method::Patch, AudioBody::Audio(bytes)) => {
                    self.append(id, request.upload_offset.unwrap(), bytes)
                }
                (Method::Get, AudioBody::Empty) => Ok(self.poll(id)),
                other => panic!("unexpected request {other:?}"),
            }
        }

        fn declare(&self, upload: &NewUpload) -> HttpResponse {
            let mut clips = self.clips.borrow_mut();
            let id = upload.audio_id().unwrap().to_string();
            if let Some(clip) = clips.get(&id) {
                if clip.total != upload.total_bytes() {
                    return HttpResponse::new(409, "{}");
                }
                return HttpResponse::new(200, Self::status(&id, clip, Self::state(clip)));
            }
            let clip = FakeClip {
                total: upload.total_bytes(),
                received: Vec::new(),
                duration_ms: upload.duration_ms(),
            };
            let body = Self::status(&id, &clip, ClipState::Uploading);
            clips.insert(id, clip);
            HttpResponse::new(201, body)
        }

        fn append(&self, id: &str, offset: u64, bytes: &[u8]) -> Result<HttpResponse, ApiError> {
            let mut clips = self.clips.borrow_mut();
            let Some(clip) = clips.get_mut(id) else {
                return Ok(HttpResponse::new(404, ""));
            };
            if offset != clip.received.len() as u64 || Self::state(clip) != ClipState::Uploading {
                let body = Self::status(id, clip, Self::state(clip));
                return Ok(HttpResponse::new(409, body));
            }
            if let Some(failure) = self.chunk_failure.get() {
                if failure.after == 0 {
                    self.chunk_failure.set(None);
                    if failure.lands {
                        clip.received.extend_from_slice(bytes);
                    }
                    return Err(ApiError::Transport("tunnel dropped".into()));
                }
                self.chunk_failure.set(Some(ChunkFailure {
                    after: failure.after - 1,
                    ..failure
                }));
            }
            clip.received.extend_from_slice(bytes);
            Ok(HttpResponse::new(
                200,
                Self::status(id, clip, Self::state(clip)),
            ))
        }

        fn poll(&self, id: &str) -> HttpResponse {
            let clips = self.clips.borrow();
            let Some(clip) = clips.get(id) else {
                return HttpResponse::new(404, "");
            };
            if Self::state(clip) == ClipState::Uploading || self.busy_polls.get() > 0 {
                self.busy_polls.set(self.busy_polls.get().saturating_sub(1));
                return HttpResponse::new(202, Self::status(id, clip, Self::state(clip)));
            }
            if self.fail_transcription.get() {
                let body = Self::status(id, clip, ClipState::Failed)
                    .replace('}', r#","failure":"transcription"}"#);
                return HttpResponse::new(422, body);
            }
            let transcript = Transcript {
                audio_id: AudioId::new(id).unwrap(),
                text: format!("{} bytes heard", clip.received.len()),
                duration_ms: clip.duration_ms,
            };
            HttpResponse::new(200, serde_json::to_string(&transcript).unwrap())
        }
    }

    impl AudioTransport for &FakeBridge {
        fn send(
            &self,
            request: AudioRequest<'_>,
        ) -> impl Future<Output = Result<HttpResponse, ApiError>> {
            core::future::ready(self.answer(request))
        }
    }

    pub(crate) fn clip(audio: Vec<u8>) -> PendingClip {
        PendingClip {
            meta: ClipMeta {
                audio_id: AudioId::new(ID).unwrap(),
                agent: "w1:p1".parse::<AgentId>().unwrap(),
                duration_ms: 1_500,
                recorded_at: Timestamp::from_unix_millis(1),
                transcript: None,
                failure: None,
            },
            audio,
        }
    }

    fn audio(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    pub(crate) fn never() -> bool {
        false
    }

    fn patches(bridge: &FakeBridge) -> Vec<(Option<u64>, usize)> {
        bridge
            .log
            .borrow()
            .iter()
            .filter(|logged| logged.method == Method::Patch)
            .map(|logged| (logged.offset, logged.body_len))
            .collect()
    }

    #[test]
    fn requests_match_the_bridge_routes() {
        let id = AudioId::new(ID).unwrap();
        let declare = declare_request(&NewUpload::new(Some(id.clone()), 10, 900).unwrap()).unwrap();
        assert_eq!(declare.method.as_str(), "POST");
        assert_eq!(declare.path, "/v1/audio");
        assert_eq!(
            declare.body,
            AudioBody::Json(format!(
                r#"{{"audioId":"{ID}","totalBytes":10,"durationMs":900}}"#
            ))
        );
        let chunk = append_request(&id, 4, &[1, 2]);
        assert_eq!(chunk.method.as_str(), "PATCH");
        assert_eq!(chunk.path, format!("/v1/audio/{ID}"));
        assert_eq!(chunk.upload_offset, Some(4));
        assert_eq!(chunk.body, AudioBody::Audio(&[1, 2]));
        let poll = transcript_request(&id);
        assert_eq!(
            (poll.method.as_str(), poll.path.as_str()),
            ("GET", format!("/v1/audio/{ID}/transcript").as_str())
        );
    }

    #[test]
    fn uploads_in_chunks_and_reports_progress() {
        let bridge = FakeBridge::default();
        let clip = clip(audio(10));
        let mut seen = Vec::new();
        let status = block_on(upload(&&bridge, &clip, 4, |p| seen.push(p.sent), never)).unwrap();
        assert_eq!(status.state, ClipState::Queued);
        assert_eq!(bridge.received(ID), Some(clip.audio.clone()));
        assert_eq!(patches(&bridge), [(Some(0), 4), (Some(4), 4), (Some(8), 2)]);
        assert_eq!(seen, [0, 4, 8, 10]);
    }

    #[test]
    fn a_failed_chunk_resumes_where_the_bridge_stopped() {
        for lands in [false, true] {
            let bridge = FakeBridge::default();
            bridge
                .chunk_failure
                .set(Some(ChunkFailure { after: 1, lands }));
            let clip = clip(audio(10));
            let error = block_on(upload(&&bridge, &clip, 4, |_| {}, never)).unwrap_err();
            assert!(error.is_retryable(), "{error:?}");
            assert_eq!(
                bridge.received(ID).map(|r| r.len()),
                Some(if lands { 8 } else { 4 })
            );
            // Try again: re-declare, then continue from the bridge's offset.
            bridge.log.borrow_mut().clear();
            let status = block_on(upload(&&bridge, &clip, 4, |_| {}, never)).unwrap();
            assert_eq!(status.state, ClipState::Queued);
            assert_eq!(
                bridge.received(ID),
                Some(clip.audio.clone()),
                "lands={lands}"
            );
            let resumed = if lands {
                vec![(Some(8), 2)]
            } else {
                vec![(Some(4), 4), (Some(8), 2)]
            };
            assert_eq!(patches(&bridge), resumed);
        }
    }

    #[test]
    fn a_cancelled_upload_sends_no_further_chunks() {
        let bridge = FakeBridge::default();
        let clip = clip(audio(10));
        let chunks = Cell::new(0);
        let error = block_on(upload(
            &&bridge,
            &clip,
            4,
            |_| chunks.set(chunks.get() + 1),
            // Discarded once the first chunk is on its way.
            || chunks.get() >= 1,
        ))
        .unwrap_err();
        assert_eq!(error, UploadError::Cancelled);
        assert!(!error.is_retryable());
        assert_eq!(patches(&bridge), [(Some(0), 4)]);
    }

    #[test]
    fn a_conflict_moves_to_the_bridges_offset() {
        let bridge = FakeBridge::default();
        let clip = clip(audio(10));
        // Another writer (a second tab) already sent the first six bytes.
        let id = &clip.meta.audio_id;
        let declare = NewUpload::new(Some(id.clone()), 10, 1_500).unwrap();
        bridge.answer(declare_request(&declare).unwrap()).unwrap();
        bridge
            .answer(append_request(id, 0, &clip.audio[..6]))
            .unwrap();
        bridge.log.borrow_mut().clear();
        let status = block_on(upload(&&bridge, &clip, 4, |_| {}, never)).unwrap();
        assert_eq!(status.state, ClipState::Queued);
        assert_eq!(bridge.received(ID), Some(clip.audio.clone()));
        assert_eq!(patches(&bridge), [(Some(6), 4)]);
    }

    /// Answers every request with the same response.
    struct Canned(u16, Option<&'static str>);
    impl AudioTransport for Canned {
        fn send(&self, _: AudioRequest) -> impl Future<Output = Result<HttpResponse, ApiError>> {
            core::future::ready(Ok(HttpResponse {
                status: self.0,
                body: self.1.map(str::to_owned),
            }))
        }
    }

    #[test]
    fn errors_are_classified() {
        let id = AudioId::new(ID).unwrap();
        let poll = |status, body| block_on(poll_transcript(&Canned(status, body), &id));
        assert_eq!(
            poll(401, Some("")),
            Err(UploadError::Api(ApiError::Unauthorized))
        );
        assert_eq!(poll(404, Some("")), Err(UploadError::Gone));
        assert!(poll(503, Some("full")).unwrap_err().is_retryable());
        assert!(poll(200, None).unwrap_err().is_retryable(), "body lost");
        let small = clip(audio(3));
        let send = |status, body| block_on(upload(&Canned(status, body), &small, 4, |_| {}, never));
        assert_eq!(send(409, Some("choose another")), Err(UploadError::IdTaken));
        let Err(UploadError::Refused(why)) =
            send(413, Some("a clip of 3 bytes is over the 2-byte cap"))
        else {
            panic!("not refused")
        };
        assert!(why.contains("2-byte cap"), "{why}");
        assert!(!send(422, Some("")).unwrap_err().is_retryable());
        assert_eq!(
            block_on(upload(
                &Canned(200, Some("{}")),
                &clip(Vec::new()),
                4,
                |_| {},
                never
            )),
            Err(UploadError::Mismatch),
            "an empty clip is never declared"
        );
    }

    #[test]
    fn polls_until_the_transcript_is_ready() {
        let bridge = FakeBridge::default();
        let clip = clip(audio(5));
        let id = clip.meta.audio_id.clone();
        block_on(upload(&&bridge, &clip, 4, |_| {}, never)).unwrap();
        bridge.busy_polls.set(1);
        assert!(matches!(
            block_on(poll_transcript(&&bridge, &id)),
            Ok(TranscriptPoll::Pending(_))
        ));
        let Ok(TranscriptPoll::Ready(transcript)) = block_on(poll_transcript(&&bridge, &id)) else {
            panic!("not ready")
        };
        assert_eq!(transcript.text, "5 bytes heard");
        bridge.fail_transcription.set(true);
        let Ok(TranscriptPoll::Failed(status)) = block_on(poll_transcript(&&bridge, &id)) else {
            panic!("not failed")
        };
        assert_eq!(status.failure, Some(protocol::AudioFailure::Transcription));
    }
}
