//! Photo attachments (design §5, protocol `attachment`): `POST
//! /v1/attachments` stores one image for the device that sent it, and a
//! prompt naming it gets a line pointing the agent at the file.
//!
//! # Upload
//!
//! The body is the raw image, its id the `Idempotency-Key` header (a UUID
//! the phone picks). In order: the id (400) → the declared type must be
//! JPEG, PNG or WebP (415) → the idempotency check (the same device and
//! bytes replay `200` with the stored receipt; another device's id, or
//! other bytes under the same id, are `409`; an upload of the id still
//! under way is `429`) → the store cap (507) → the per-device rate limit
//! (429 with `Retry-After`; replays never count) → the bytes are sniffed
//! (415 unless they *are* a JPEG, PNG or WebP, whatever the header said)
//! and decoded under limits (422) and re-encoded ([`image::clean`]), so
//! EXIF, GPS and every other metadata block is gone before anything
//! reaches the disk → stored `0600` under a `0700` directory (503 if the
//! disk fails) → `201` with the [`AttachmentReceipt`]. The body limit
//! ([`AttachmentSettings::max_upload_bytes`]) is enforced before the
//! handler runs (413).
//!
//! The image is never served back: there is no `GET`. It is untrusted
//! input that only ever reaches the agent as a file path.
//!
//! # Prompts
//!
//! A prompt's `attachments` must all belong to the sending device and
//! still exist ([`Attachments::resolve`]; otherwise `unknown_attachment`).
//! Each adds one line after the text ([`prompt_text`]), naming the file by
//! its absolute path:
//!
//! - Claude Code: `Attached image: @/abs/path.jpg`. An `@` mention attaches
//!   the image to the message itself, with no tool call and no permission
//!   prompt, wherever the file lives (checked with Claude Code 2.1.317).
//! - Codex and anything else: `Attached image: /abs/path.jpg`. Codex
//!   (0.166) opens it with its `view_image` tool; its sandbox reads the
//!   whole disk.
//!
//! The files live in the bridge's state dir, which agents running as the
//! bridge's user can read.
//!
//! # Retention
//!
//! An attachment is deleted [`AttachmentSettings::retention`] after it was
//! stored (7 days by default); a prompt naming it after that is refused.

mod image;
mod store;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use protocol::{AttachmentId, AttachmentReceipt, IDEMPOTENCY_KEY_HEADER, ImageType};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use self::store::{Record, Store};
use crate::actions::{Budget, retry_after_secs};
use crate::auth::{RateLimit, RateLimiter};
use crate::server::{AppState, ClientAddr, Principal};
use crate::shutdown::Shutdown;

/// The attachments directory's name in the state dir.
pub const DIR: &str = "attachments";

/// Settings for photo attachments.
#[derive(Debug, Clone)]
pub struct AttachmentSettings {
    /// Where images and their records live (`<state dir>/attachments`).
    pub dir: PathBuf,
    /// Largest upload, in bytes (the route's body limit).
    pub max_upload_bytes: usize,
    /// How long one upload may take (the route's timeout).
    pub upload_timeout: Duration,
    /// Most attachments stored at once, across devices.
    pub max_stored: usize,
    /// Uploads each device may make (static-token clients: per address).
    pub rate: RateLimit,
    /// How long an attachment is kept.
    pub retention: Duration,
    /// How often retention runs.
    pub sweep_every: Duration,
}

impl AttachmentSettings {
    /// Defaults for `dir`: 10 MiB per upload with two minutes to send it,
    /// 512 stored, 30 uploads per device per 10 minutes, kept 7 days and
    /// swept every 10 minutes.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            max_upload_bytes: 10 * 1024 * 1024,
            upload_timeout: Duration::from_mins(2),
            max_stored: 512,
            rate: RateLimit {
                requests: 30,
                window: Duration::from_mins(10),
            },
            retention: Duration::from_hours(7 * 24),
            sweep_every: Duration::from_mins(10),
        }
    }
}

/// An attachment a prompt may carry, resolved for one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attached {
    pub(crate) id: AttachmentId,
    /// The image's absolute path.
    pub(crate) path: PathBuf,
    pub(crate) media_type: ImageType,
    pub(crate) bytes: u64,
}

/// One stored attachment, in memory.
#[derive(Debug)]
struct Held {
    record: Record,
    /// SHA-256 of the upload as received, to tell a replay from another
    /// image under the same id. Memory only: after a restart a replay by
    /// the owner is answered from the record.
    upload: Option<[u8; 32]>,
}

#[derive(Debug, Default)]
struct Inner {
    stored: HashMap<AttachmentId, Held>,
    uploading: HashSet<AttachmentId>,
}

/// The attachments store, its index and the upload rate limit.
#[derive(Debug)]
pub struct Attachments {
    settings: AttachmentSettings,
    store: Store,
    inner: Mutex<Inner>,
    limit: RateLimiter<Budget>,
}

/// What an upload came to.
#[derive(Debug, PartialEq, Eq)]
enum Uploaded {
    New(AttachmentReceipt),
    Replay(AttachmentReceipt),
}

/// Why an upload was refused.
#[derive(Debug, thiserror::Error)]
enum UploadError {
    #[error("the Idempotency-Key header must be a lowercase UUID")]
    BadId,
    #[error("only JPEG, PNG and WebP images can be attached")]
    NotAnImage,
    #[error("this id belongs to another upload; choose another")]
    Conflict,
    #[error("this upload is still being stored; retry shortly")]
    InFlight,
    #[error("too many attachments are stored; try again later")]
    Full,
    #[error("too many uploads from this device; retry shortly")]
    RateLimited(Duration),
    #[error("the image does not decode")]
    Undecodable,
    #[error("the attachment could not be stored")]
    Io(#[source] io::Error),
}

impl UploadError {
    fn status(&self) -> StatusCode {
        match self {
            Self::BadId => StatusCode::BAD_REQUEST,
            Self::NotAnImage => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Conflict => StatusCode::CONFLICT,
            Self::InFlight | Self::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Full => StatusCode::INSUFFICIENT_STORAGE,
            Self::Undecodable => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Io(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        let mut response = (self.status(), self.to_string()).into_response();
        if let Self::RateLimited(wait) = self {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from(retry_after_secs(wait)),
            );
        }
        response
    }
}

/// Takes `id` out of the upload set when dropped, however the upload ends.
struct Uploading<'a> {
    attachments: &'a Attachments,
    id: AttachmentId,
}

impl Drop for Uploading<'_> {
    fn drop(&mut self) {
        self.attachments.lock().uploading.remove(&self.id);
    }
}

impl Attachments {
    /// Opens the directory (creating it `0700`) and indexes what is
    /// stored, deleting leftovers. Blocking.
    pub fn open(settings: AttachmentSettings) -> io::Result<Self> {
        let store = Store::open(&settings.dir)?;
        let stored = store
            .load()?
            .into_iter()
            .map(|(id, record)| {
                (
                    id,
                    Held {
                        record,
                        upload: None,
                    },
                )
            })
            .collect();
        Ok(Self {
            limit: RateLimiter::new(settings.rate),
            settings,
            store,
            inner: Mutex::new(Inner {
                stored,
                uploading: HashSet::new(),
            }),
        })
    }

    /// The settings it was opened with.
    #[must_use]
    pub fn settings(&self) -> &AttachmentSettings {
        &self.settings
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores `body` as `id` for `device` (see the module docs).
    async fn upload(
        self: &Arc<Self>,
        device: &str,
        budget: &Budget,
        id: AttachmentId,
        body: Bytes,
    ) -> Result<Uploaded, UploadError> {
        let digest: [u8; 32] = Sha256::digest(&body).into();
        let guard = {
            let mut inner = self.lock();
            if let Some(held) = inner.stored.get(&id) {
                let same = held.record.owner == device
                    && held.upload.is_none_or(|upload| upload == digest);
                return if same {
                    Ok(Uploaded::Replay(held.record.receipt(&id)))
                } else {
                    Err(UploadError::Conflict)
                };
            }
            if inner.uploading.contains(&id) {
                return Err(UploadError::InFlight);
            }
            if inner.stored.len() + inner.uploading.len() >= self.settings.max_stored {
                return Err(UploadError::Full);
            }
            inner.uploading.insert(id.clone());
            Uploading {
                attachments: self,
                id: id.clone(),
            }
        };
        self.limit.check(budget).map_err(UploadError::RateLimited)?;
        let this = Arc::clone(self);
        let owner = device.to_owned();
        let stored_id = id.clone();
        let record =
            tokio::task::spawn_blocking(move || this.clean_and_save(&stored_id, owner, &body))
                .await
                .map_err(|error| UploadError::Io(io::Error::other(error)))??;
        let receipt = record.receipt(&id);
        self.lock().stored.insert(
            id,
            Held {
                record,
                upload: Some(digest),
            },
        );
        drop(guard);
        Ok(Uploaded::New(receipt))
    }

    /// Makes `upload` safe ([`image::clean`]) and stores it. Blocking.
    fn clean_and_save(
        &self,
        id: &AttachmentId,
        owner: String,
        upload: &[u8],
    ) -> Result<Record, UploadError> {
        let clean = image::clean(upload).map_err(|error| match error {
            image::CleanError::NotAnImage => UploadError::NotAnImage,
            image::CleanError::Undecodable(_) => UploadError::Undecodable,
            image::CleanError::Encode(error) => UploadError::Io(io::Error::other(error)),
        })?;
        let record = Record {
            owner,
            media_type: clean.kind,
            bytes: clean.bytes.len() as u64,
            width: clean.width,
            height: clean.height,
            created_ms: unix_ms(SystemTime::now()),
        };
        self.store
            .save(id, &record, &clean.bytes)
            .map_err(UploadError::Io)?;
        Ok(record)
    }

    /// The attachments `ids` names, if every one is `device`'s and still
    /// stored; otherwise the first that is not (another device's upload is
    /// as unknown as one that never existed).
    pub(crate) fn resolve(
        &self,
        device: &str,
        ids: &[AttachmentId],
    ) -> Result<Vec<Attached>, AttachmentId> {
        let inner = self.lock();
        ids.iter()
            .map(|id| match inner.stored.get(id) {
                Some(held) if held.record.owner == device => Ok(Attached {
                    id: id.clone(),
                    path: self.store.image_path(id, held.record.media_type),
                    media_type: held.record.media_type,
                    bytes: held.record.bytes,
                }),
                _ => Err(id.clone()),
            })
            .collect()
    }

    /// Runs [`Self::sweep`] every [`AttachmentSettings::sweep_every`] until
    /// `shutdown`.
    pub fn spawn_sweeper(self: &Arc<Self>, mut shutdown: Shutdown) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(this.settings.sweep_every);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    () = shutdown.wait() => break,
                }
                let this = Arc::clone(&this);
                let swept = tokio::task::spawn_blocking(move || this.sweep(SystemTime::now()))
                    .await
                    .unwrap_or_default();
                if swept > 0 {
                    tracing::info!(attachments = swept, "attachment retention");
                }
            }
        })
    }

    /// Deletes every attachment stored [`AttachmentSettings::retention`]
    /// or longer before `now`; returns how many. Blocking.
    pub fn sweep(&self, now: SystemTime) -> usize {
        let cutoff = unix_ms(now)
            .saturating_sub(u64::try_from(self.settings.retention.as_millis()).unwrap_or(u64::MAX));
        let expired: Vec<(AttachmentId, ImageType)> = {
            let mut inner = self.lock();
            let ids: Vec<AttachmentId> = inner
                .stored
                .iter()
                .filter(|(_, held)| held.record.created_ms <= cutoff)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    let held = inner.stored.remove(&id)?;
                    Some((id, held.record.media_type))
                })
                .collect()
        };
        for (id, kind) in &expired {
            if let Err(error) = self.store.remove(id, *kind) {
                tracing::warn!(%id, %error, "cannot delete an expired attachment");
            }
        }
        expired.len()
    }
}

fn unix_ms(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The text typed into the agent for a prompt of `text` with `attached`:
/// the text, a blank line, then one line per image (see the module docs
/// for why Claude gets an `@` mention). `kind` is herdr's agent kind.
pub(crate) fn prompt_text(text: &str, kind: Option<&str>, attached: &[Attached]) -> String {
    let mut out = text.to_owned();
    if attached.is_empty() {
        return out;
    }
    out.push('\n');
    for attachment in attached {
        let path = attachment.path.display().to_string();
        // An `@` mention ends at whitespace, so a path with any falls back
        // to the plain form (which Claude reads with its Read tool).
        let mention = kind == Some("claude") && !path.contains(char::is_whitespace);
        let at = if mention { "@" } else { "" };
        let _ = write!(out, "\nAttached image: {at}{path}");
    }
    out
}

/// `POST /v1/attachments`.
pub(crate) async fn post(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(client): Extension<ClientAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let id = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| AttachmentId::new(v).ok());
    let Some(id) = id else {
        return UploadError::BadId.into_response();
    };
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !image::declared_ok(declared) {
        return UploadError::NotAnImage.into_response();
    }
    let budget = Budget::of(&principal, client);
    let attachments = Arc::clone(&state.attachments);
    // In its own task, so a client hanging up cannot leave a half-stored
    // upload behind.
    let work = tokio::spawn(async move {
        attachments
            .upload(&principal.device, &budget, id, body)
            .await
    });
    match work.await {
        Ok(Ok(Uploaded::New(receipt))) => {
            tracing::info!(
                id = %receipt.attachment_id,
                bytes = receipt.bytes,
                media_type = receipt.media_type.media_type(),
                "attachment stored"
            );
            (StatusCode::CREATED, Json(receipt)).into_response()
        }
        Ok(Ok(Uploaded::Replay(receipt))) => Json(receipt).into_response(),
        Ok(Err(error)) => {
            if let UploadError::Io(source) = &error {
                tracing::error!(error = %source, "storing an attachment failed");
            }
            error.into_response()
        }
        Err(error) => {
            tracing::error!(%error, "attachment task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::image::fixtures;
    use super::*;

    const A: &str = "00000000-0000-4000-8000-0000000000a1";
    const B: &str = "00000000-0000-4000-8000-0000000000b2";
    const PHONE: &str = "dev-00000000000000aa";
    const TABLET: &str = "dev-00000000000000bb";

    fn id(s: &str) -> AttachmentId {
        AttachmentId::new(s).expect("id")
    }

    fn budget(device: &str) -> Budget {
        Budget::Device(device.to_owned())
    }

    fn open(
        dir: &std::path::Path,
        adjust: impl FnOnce(&mut AttachmentSettings),
    ) -> Arc<Attachments> {
        let mut settings = AttachmentSettings::new(dir.join("attachments"));
        adjust(&mut settings);
        Arc::new(Attachments::open(settings).expect("open"))
    }

    fn photo() -> Bytes {
        Bytes::from(fixtures::with_exif(&fixtures::jpeg(32, 24), 1))
    }

    async fn upload(
        attachments: &Arc<Attachments>,
        device: &str,
        key: &str,
        body: Bytes,
    ) -> Result<Uploaded, UploadError> {
        attachments
            .upload(device, &budget(device), id(key), body)
            .await
    }

    #[tokio::test]
    async fn uploads_replay_and_conflict() {
        let tmp = tempfile::tempdir().expect("tmp");
        let attachments = open(tmp.path(), |_| {});
        let Uploaded::New(receipt) = upload(&attachments, PHONE, A, photo()).await.expect("new")
        else {
            panic!("a new upload");
        };
        assert_eq!(receipt.media_type, ImageType::Jpeg);
        assert_eq!((receipt.width, receipt.height), (32, 24));
        let stored = std::fs::read(tmp.path().join(format!("attachments/{A}.jpg"))).expect("file");
        assert_eq!(stored.len() as u64, receipt.bytes);
        assert!(!fixtures::contains(&stored, fixtures::PLANTED));

        let replay = upload(&attachments, PHONE, A, photo())
            .await
            .expect("replay");
        assert_eq!(replay, Uploaded::Replay(receipt.clone()));
        let other_bytes = Bytes::from(fixtures::jpeg(8, 8));
        assert!(matches!(
            upload(&attachments, PHONE, A, other_bytes).await,
            Err(UploadError::Conflict)
        ));
        assert!(matches!(
            upload(&attachments, TABLET, A, photo()).await,
            Err(UploadError::Conflict)
        ));

        // After a restart the owner's replay is still answered.
        let reopened = open(tmp.path(), |_| {});
        assert_eq!(
            upload(&reopened, PHONE, A, photo()).await.expect("replay"),
            Uploaded::Replay(receipt)
        );
        assert!(matches!(
            upload(&reopened, TABLET, A, photo()).await,
            Err(UploadError::Conflict)
        ));
    }

    #[tokio::test]
    async fn refused_uploads_store_nothing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let attachments = open(tmp.path(), |_| {});
        for (body, want) in [
            (
                &b"<html>not a picture</html>"[..],
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (b"\xFF\xD8\xFF\xE0broken", StatusCode::UNPROCESSABLE_ENTITY),
        ] {
            let error = upload(&attachments, PHONE, A, Bytes::from_static(body))
                .await
                .expect_err("refused");
            assert_eq!(error.status(), want, "{error}");
        }
        let dir: Vec<_> = std::fs::read_dir(tmp.path().join("attachments"))
            .expect("ls")
            .collect();
        assert!(dir.is_empty(), "{dir:?}");
        // The id was not spent.
        assert!(matches!(
            upload(&attachments, PHONE, A, photo()).await,
            Ok(Uploaded::New(_))
        ));
    }

    #[tokio::test]
    async fn uploads_are_rate_limited_per_device_but_replays_are_free() {
        let tmp = tempfile::tempdir().expect("tmp");
        let attachments = open(tmp.path(), |s| s.rate = RateLimit::per_minute(1));
        upload(&attachments, PHONE, A, photo())
            .await
            .expect("first");
        upload(&attachments, PHONE, A, photo())
            .await
            .expect("a replay is free");
        let limited = upload(&attachments, PHONE, B, photo()).await;
        assert!(
            matches!(limited, Err(UploadError::RateLimited(_))),
            "{limited:?}"
        );
        upload(&attachments, TABLET, B, photo())
            .await
            .expect("another device has its own budget");
    }

    #[tokio::test]
    async fn the_store_is_capped() {
        let tmp = tempfile::tempdir().expect("tmp");
        let attachments = open(tmp.path(), |s| s.max_stored = 1);
        upload(&attachments, PHONE, A, photo())
            .await
            .expect("first");
        let full = upload(&attachments, PHONE, B, photo()).await;
        assert!(matches!(full, Err(UploadError::Full)), "{full:?}");
    }

    #[tokio::test]
    async fn only_the_owner_resolves_an_attachment() {
        let tmp = tempfile::tempdir().expect("tmp");
        let attachments = open(tmp.path(), |_| {});
        upload(&attachments, PHONE, A, photo())
            .await
            .expect("stored");
        let attached = attachments.resolve(PHONE, &[id(A)]).expect("resolves");
        assert_eq!(attached.len(), 1);
        assert!(attached[0].path.is_absolute());
        assert!(attached[0].path.ends_with(format!("{A}.jpg")));
        assert_eq!(attachments.resolve(TABLET, &[id(A)]), Err(id(A)));
        assert_eq!(attachments.resolve(PHONE, &[id(A), id(B)]), Err(id(B)));
    }

    #[tokio::test]
    async fn retention_deletes_old_attachments() {
        let tmp = tempfile::tempdir().expect("tmp");
        let attachments = open(tmp.path(), |_| {});
        upload(&attachments, PHONE, A, photo())
            .await
            .expect("stored");
        let now = SystemTime::now();
        assert_eq!(attachments.sweep(now), 0, "fresh ones stay");
        let later = now + attachments.settings().retention + Duration::from_secs(1);
        assert_eq!(attachments.sweep(later), 1);
        assert_eq!(attachments.resolve(PHONE, &[id(A)]), Err(id(A)));
        let left = std::fs::read_dir(tmp.path().join("attachments"))
            .expect("ls")
            .count();
        assert_eq!(left, 0, "image and record are gone");
    }

    #[test]
    fn prompts_name_each_file_the_way_the_agent_reads_it() {
        let attached = |path: &str| Attached {
            id: id(A),
            path: PathBuf::from(path),
            media_type: ImageType::Jpeg,
            bytes: 1,
        };
        let one = [attached("/state/attachments/a.jpg")];
        assert_eq!(
            prompt_text("What is this?", Some("claude"), &one),
            "What is this?\n\nAttached image: @/state/attachments/a.jpg"
        );
        assert_eq!(
            prompt_text("What is this?", Some("codex"), &one),
            "What is this?\n\nAttached image: /state/attachments/a.jpg"
        );
        assert_eq!(prompt_text("plain", Some("claude"), &[]), "plain");
        let two = [attached("/s/a.jpg"), attached("/my state/b.png")];
        assert_eq!(
            prompt_text("Compare", Some("claude"), &two),
            "Compare\n\nAttached image: @/s/a.jpg\nAttached image: /my state/b.png"
        );
        assert_eq!(
            prompt_text("x", None, &one),
            "x\n\nAttached image: /state/attachments/a.jpg"
        );
    }
}
