//! Dictation on the bridge (design §6.3): resumable clip uploads, a
//! one-at-a-time transcription queue and retention.
//!
//! A clip's life: the phone declares it (`POST /v1/audio`: size and
//! length, both capped), sends its bytes in order (`PATCH`, each chunk at
//! the offset the bridge reports, so an interrupted upload resumes where it
//! stopped), and once the last byte lands the clip joins the queue. One
//! worker feeds clips to the [`Transcriber`] in order (a single GPU, so no
//! parallelism). The phone then polls `GET /v1/audio/:id/transcript`.
//!
//! A clip belongs to the device that declared it: every other device is
//! told it does not exist (and cannot declare its id).
//!
//! Everything is persisted in [`AudioSettings::dir`] (see `store`), so
//! uploads and queued clips survive a bridge restart. Writes run in their
//! own task, so a request that is dropped half-way (client gone, timeout)
//! still finishes its bookkeeping.
//!
//! The backend being briefly unreachable (whisper-server restarting or
//! loading its model) is retried with backoff
//! ([`AudioSettings::retry_delays`]). A clip that still fails can be
//! retried by declaring it again; its audio is kept for that.
//!
//! Retention: a transcribed clip's audio is deleted after
//! [`AudioSettings::audio_retention`] (kept for debugging; zero deletes it
//! as soon as it is transcribed), and its record and transcript after
//! [`AudioSettings::record_retention`]. A failed clip keeps its audio until
//! its record expires, so a retry never needs a re-upload. An upload
//! nobody touches for `record_retention` is abandoned and deleted; one
//! idle for [`AudioSettings::upload_idle`] makes way for a new clip when
//! [`AudioSettings::max_clips`] are in flight. Queued clips are never
//! swept.

mod store;
pub mod whisper;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use protocol::{AudioFailure, AudioId, ClipState, NewUpload, Transcript, UploadStatus, text};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use self::store::{Phase, Record, Store};
pub use self::whisper::{
    Recognized, TranscribeError, TranscribeFuture, Transcriber, WhisperClient,
};
use crate::config::AudioConfig;
use crate::server::detached;
use crate::shutdown::Shutdown;

/// Decoded clips may run this much past the duration cap (container
/// padding, the phone's timer vs the decoder) before they are refused.
const DURATION_SLACK: Duration = Duration::from_secs(2);

/// Dictation limits, retention and the backend.
#[derive(Debug, Clone)]
pub struct AudioSettings {
    /// Where clips and their records live (`<state dir>/audio`).
    pub dir: PathBuf,
    /// Largest clip, in bytes.
    pub max_upload_bytes: u64,
    /// Longest clip.
    pub max_duration: Duration,
    /// Largest single upload chunk, in bytes (the `PATCH` body limit).
    pub max_chunk_bytes: usize,
    /// How long one chunk upload may take (the `PATCH` request timeout,
    /// in place of the server-wide one): long enough for a full chunk over
    /// a slow mobile uplink.
    pub chunk_timeout: Duration,
    /// Most clips in flight (uploading, queued or transcribing) at once.
    /// Finished clips do not count. At the cap, a new clip evicts the
    /// upload idle the longest (see [`Self::upload_idle`]), or gets a 503.
    pub max_clips: usize,
    /// How long an upload must go without a chunk before a new clip may
    /// evict it to stay under [`Self::max_clips`].
    pub upload_idle: Duration,
    /// How long a finished clip's audio is kept (zero: delete it once
    /// transcribed).
    pub audio_retention: Duration,
    /// How long a finished clip's record and transcript are kept, and how
    /// long an idle upload waits before it is abandoned.
    pub record_retention: Duration,
    /// How often retention runs.
    pub sweep_every: Duration,
    /// Waits between retries while the backend is unreachable; the clip
    /// fails once they run out.
    pub retry_delays: Vec<Duration>,
    /// The speech-to-text backend.
    pub transcriber: Arc<dyn Transcriber>,
}

impl AudioSettings {
    /// Default caps for clips in `dir`: 25 MiB and 5 minutes per clip,
    /// 1 MiB chunks with two minutes each (64 kbit/s), 64 clips in flight
    /// (idle uploads make way after 10 minutes), 7 days of retention, swept
    /// every 10 minutes, and about four minutes of retries for an
    /// unreachable backend (enough for whisper-server to load its model
    /// after a boot).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>, transcriber: Arc<dyn Transcriber>) -> Self {
        const WEEK: Duration = Duration::from_hours(7 * 24);
        Self {
            dir: dir.into(),
            max_upload_bytes: 25 * 1024 * 1024,
            max_duration: Duration::from_secs(300),
            max_chunk_bytes: 1024 * 1024,
            chunk_timeout: Duration::from_mins(2),
            max_clips: 64,
            upload_idle: Duration::from_mins(10),
            audio_retention: WEEK,
            record_retention: WEEK,
            sweep_every: Duration::from_secs(600),
            retry_delays: [2, 5, 10, 20, 30, 60, 120]
                .map(Duration::from_secs)
                .to_vec(),
            transcriber,
        }
    }

    /// Settings from the `[audio]` config table: clips in
    /// `<state_dir>/audio`, transcribed by the `whisper-server` it names.
    #[must_use]
    pub fn from_config(config: &AudioConfig, state_dir: &Path) -> Self {
        let whisper = WhisperClient::new(config.whisper, whisper_timeout(config.max_duration));
        let mut settings = Self::new(state_dir.join(AUDIO_DIR), Arc::new(whisper));
        settings.max_upload_bytes = config.max_upload_bytes;
        settings.max_duration = config.max_duration;
        settings.audio_retention = config.retention;
        settings.record_retention = config.retention.max(MIN_RECORD_RETENTION);
        settings
    }
}

/// The audio directory's name inside the state dir.
pub const AUDIO_DIR: &str = "audio";

/// Longest wait for one transcription of a clip up to `max_duration`: a
/// minute plus twice real time. The GPU needs seconds even for long clips;
/// this leaves room for whisper-server falling back to the CPU.
fn whisper_timeout(max_duration: Duration) -> Duration {
    Duration::from_mins(1) + max_duration * 2
}

/// Transcripts outlive their audio by at least this, so a phone that comes
/// back online can still fetch one even with audio retention at zero.
const MIN_RECORD_RETENTION: Duration = Duration::from_hours(24);

/// The outcome of [`Audio::create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Created {
    /// A new upload.
    New(UploadStatus),
    /// The id was already declared with the same size and length (a clip
    /// that had failed transcription is queued again).
    Existing(UploadStatus),
}

/// What `GET /v1/audio/:id/transcript` has to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptView {
    /// Transcribed.
    Ready(Transcript),
    /// Not yet: still uploading, queued or transcribing.
    Pending(UploadStatus),
    /// Transcription failed.
    Failed(UploadStatus),
}

/// Why an upload request was refused.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// No clip with that id (never declared, or swept).
    #[error("no such clip")]
    NotFound,
    /// Over the size cap.
    #[error("a clip of {actual} bytes is over the {max}-byte cap")]
    TooLarge {
        /// The cap.
        max: u64,
        /// What was declared.
        actual: u64,
    },
    /// Over the duration cap.
    #[error("a clip of {actual_ms} ms is over the {max_ms} ms cap")]
    TooLong {
        /// The cap.
        max_ms: u64,
        /// What was declared.
        actual_ms: u64,
    },
    /// [`AudioSettings::max_clips`] are in flight already, none of them an
    /// idle upload that could make way.
    #[error("too many clips in flight; retry later")]
    Full,
    /// The id is being declared or deleted right now; retry shortly.
    #[error("the clip is busy; retry shortly")]
    Busy,
    /// The id names an upload with a different size or length.
    #[error("the clip id is in use by a different upload")]
    IdConflict(UploadStatus),
    /// The id cannot be used (files set aside under it); pick another.
    #[error("the clip id is unavailable; choose another")]
    IdUnavailable,
    /// The chunk does not start at the current offset, another chunk is
    /// being written, or the upload is complete.
    #[error("the chunk does not start at the upload's offset")]
    OffsetMismatch(UploadStatus),
    /// The chunk runs past the declared size.
    #[error("the chunk runs past the declared size")]
    PastEnd(UploadStatus),
    /// The disk failed.
    #[error("storing audio: {0}")]
    Io(#[from] io::Error),
}

/// What one retention sweep deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    /// Clips deleted entirely (record, transcript and any audio).
    pub clips: usize,
    /// Finished clips whose audio alone was deleted.
    pub audio: usize,
}

/// One clip in memory.
#[derive(Debug)]
struct Clip {
    record: Record,
    /// Bytes received (the audio file's length).
    offset: u64,
    /// Whether the audio file exists.
    has_audio: bool,
    /// Who, if anyone, is working on the clip's files.
    hold: Hold,
    /// Last upload activity, for abandoning idle uploads.
    touched: SystemTime,
    /// The final phase in memory has not reached the disk (which still
    /// says queued); retention keeps the audio until it does.
    unsaved: bool,
}

/// Work in progress on a clip's files, which excludes any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// Nobody.
    Free,
    /// Being declared: its record is not on disk yet, so it may still
    /// vanish; declaring it again meanwhile is refused as busy.
    Creating,
    /// A write (or retention deleting its audio); other writes get a
    /// conflict meanwhile.
    Busy,
    /// Retention is deleting all its files: it is gone to readers and its
    /// id cannot be declared until the entry is removed.
    Removing,
}

impl Clip {
    fn status(&self, id: &AudioId) -> UploadStatus {
        let (state, failure) = match &self.record.state {
            Phase::Uploading => (ClipState::Uploading, None),
            Phase::Queued => (ClipState::Queued, None),
            Phase::Transcribing => (ClipState::Transcribing, None),
            Phase::Done { .. } => (ClipState::Done, None),
            Phase::Failed { failure, .. } => (ClipState::Failed, Some(*failure)),
        };
        UploadStatus {
            audio_id: id.clone(),
            offset: self.offset,
            total_bytes: self.record.total_bytes,
            state,
            failure,
        }
    }

    /// Counts against [`AudioSettings::max_clips`].
    fn in_flight(&self) -> bool {
        self.record.state.finished_ms().is_none() && self.hold != Hold::Removing
    }

    /// An upload nobody has sent a chunk to for `idle` (as of `now`).
    fn idle_upload(&self, now: SystemTime, idle: Duration) -> bool {
        self.hold == Hold::Free
            && self.record.state == Phase::Uploading
            && now
                .duration_since(self.touched)
                .is_ok_and(|elapsed| elapsed >= idle)
    }
}

#[derive(Debug, Default)]
struct Inner {
    clips: HashMap<AudioId, Clip>,
    queue: VecDeque<AudioId>,
    /// Ids whose unreadable files are still on disk; never reused.
    reserved: HashSet<AudioId>,
}

impl Inner {
    /// The clip, unless retention is deleting it.
    fn live(&mut self, id: &AudioId) -> Result<&mut Clip, UploadError> {
        self.clips
            .get_mut(id)
            .filter(|clip| clip.hold != Hold::Removing)
            .ok_or(UploadError::NotFound)
    }

    /// The clip as `device` sees it: [`Self::live`], and not found unless
    /// `device` owns it.
    fn owned(&mut self, id: &AudioId, device: &str) -> Result<&mut Clip, UploadError> {
        self.live(id)
            .ok()
            .filter(|clip| clip.record.owned_by(device))
            .ok_or(UploadError::NotFound)
    }

    /// Room for one more clip in flight as of `now`: `None` under the cap.
    /// At the cap, the upload idle the longest (see
    /// [`AudioSettings::upload_idle`]) is held for removal and returned,
    /// for the caller to evict; with none, [`UploadError::Full`].
    fn admit(
        &mut self,
        settings: &AudioSettings,
        now: SystemTime,
    ) -> Result<Option<AudioId>, UploadError> {
        let in_flight = self.clips.values().filter(|c| c.in_flight()).count();
        if in_flight < settings.max_clips {
            return Ok(None);
        }
        let (victim, clip) = self
            .clips
            .iter_mut()
            .filter(|(_, clip)| clip.idle_upload(now, settings.upload_idle))
            .min_by_key(|(_, clip)| clip.touched)
            .ok_or(UploadError::Full)?;
        clip.hold = Hold::Removing;
        Ok(Some(victim.clone()))
    }
}

/// How a re-declared failed clip is retried.
enum Retry {
    /// Its audio is complete on disk: queue it again.
    Requeue,
    /// Its audio is gone (retention zero): take the upload again from 0.
    Reupload,
}

/// The dictation service: uploads, the queue and retention.
#[derive(Debug)]
pub struct Audio {
    settings: AudioSettings,
    store: Store,
    inner: Mutex<Inner>,
    /// Wakes the worker when a clip is queued.
    wake: Notify,
}

impl Audio {
    /// Opens (creating if needed) the audio directory and picks up where a
    /// previous run left off: uploads stay resumable and queued or
    /// interrupted transcriptions are queued again.
    pub async fn open(settings: AudioSettings) -> io::Result<Self> {
        let store = Store::new(settings.dir.clone());
        let reader = store.clone();
        let mut loaded = tokio::task::spawn_blocking(move || reader.load())
            .await
            .map_err(io::Error::other)??;
        loaded.clips.sort_by_key(|clip| clip.record.created_ms);
        let mut inner = Inner {
            reserved: loaded.reserved.into_iter().collect(),
            ..Inner::default()
        };
        for clip in loaded.clips {
            let mut record = clip.record;
            let offset = clip.audio_len.unwrap_or(0).min(record.total_bytes);
            // Fully uploaded but not yet recorded as queued (the bridge
            // stopped in between): queue it now.
            let complete = record.state == Phase::Uploading && offset == record.total_bytes;
            if record.state == Phase::Transcribing || complete {
                record.state = Phase::Queued;
            }
            if record.state == Phase::Queued {
                inner.queue.push_back(clip.id.clone());
            }
            // The idle clock keeps running across restarts.
            let touched = clip
                .audio_modified
                .unwrap_or(UNIX_EPOCH + Duration::from_millis(record.created_ms));
            inner.clips.insert(
                clip.id,
                Clip {
                    offset,
                    has_audio: clip.audio_len.is_some(),
                    hold: Hold::Free,
                    touched,
                    unsaved: false,
                    record,
                },
            );
        }
        Ok(Self {
            settings,
            store,
            inner: Mutex::new(inner),
            wake: Notify::new(),
        })
    }

    /// The limits in force.
    #[must_use]
    pub fn settings(&self) -> &AudioSettings {
        &self.settings
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Declares a clip for `device`, or finds the one it declared before
    /// (and queues it again if its transcription had failed). An id that
    /// another device declared is [`UploadError::IdUnavailable`].
    pub async fn create(
        self: &Arc<Self>,
        device: &str,
        upload: NewUpload,
    ) -> Result<Created, UploadError> {
        let this = Arc::clone(self);
        let device = device.to_owned();
        detached(async move { this.create_now(device, upload).await })
            .await
            .map_err(task_failed)?
    }

    async fn create_now(&self, device: String, upload: NewUpload) -> Result<Created, UploadError> {
        self.check_caps(&upload)?;
        let id = match upload.audio_id() {
            Some(id) => id.clone(),
            None => random_audio_id()?,
        };
        let record = Record {
            total_bytes: upload.total_bytes(),
            duration_ms: upload.duration_ms(),
            created_ms: unix_ms(SystemTime::now()),
            owner: Some(device.clone()),
            state: Phase::Uploading,
        };
        let now = SystemTime::now();
        let (step, victim) = {
            let mut inner = self.lock();
            if inner.reserved.contains(&id) {
                return Err(UploadError::IdUnavailable);
            }
            let step = match inner.clips.get(&id) {
                // Before anything else, so another device's clip shows
                // nothing (not even that it is busy).
                Some(clip) if !clip.record.owned_by(&device) => {
                    return Err(UploadError::IdUnavailable);
                }
                Some(clip) if matches!(clip.hold, Hold::Removing | Hold::Creating) => {
                    return Err(UploadError::Busy);
                }
                Some(clip) => {
                    let status = clip.status(&id);
                    if clip.record.total_bytes != record.total_bytes
                        || clip.record.duration_ms != record.duration_ms
                    {
                        return Err(UploadError::IdConflict(status));
                    }
                    let retriable = matches!(
                        clip.record.state,
                        Phase::Failed {
                            failure: AudioFailure::Transcription,
                            ..
                        }
                    );
                    if !retriable || clip.hold != Hold::Free {
                        return Ok(Created::Existing(status));
                    }
                    if clip.has_audio && clip.offset == clip.record.total_bytes {
                        Some(Retry::Requeue)
                    } else {
                        Some(Retry::Reupload)
                    }
                }
                None => None,
            };
            // A retried clip is back in flight too.
            let victim = inner.admit(&self.settings, now)?;
            match inner.clips.get_mut(&id) {
                Some(clip) => clip.hold = Hold::Busy,
                None => {
                    inner.clips.insert(
                        id.clone(),
                        Clip {
                            record: record.clone(),
                            offset: 0,
                            has_audio: false,
                            hold: Hold::Creating,
                            touched: now,
                            unsaved: false,
                        },
                    );
                }
            }
            (step, victim)
        };
        if let Some(victim) = victim {
            tracing::info!(audio_id = %victim, "evicting an idle upload to make room");
            self.remove_clip(&victim).await;
        }
        if let Some(how) = step {
            return self.retry(&id, how).await.map(Created::Existing);
        }
        let saved = self.store.save(&id, &record).await;
        let mut inner = self.lock();
        if let Err(error) = saved {
            inner.clips.remove(&id);
            return Err(error.into());
        }
        let clip = inner.live(&id)?;
        clip.hold = Hold::Free;
        Ok(Created::New(clip.status(&id)))
    }

    fn check_caps(&self, upload: &NewUpload) -> Result<(), UploadError> {
        if upload.total_bytes() > self.settings.max_upload_bytes {
            return Err(UploadError::TooLarge {
                max: self.settings.max_upload_bytes,
                actual: upload.total_bytes(),
            });
        }
        let max_ms = millis(self.settings.max_duration);
        if upload.duration_ms() > max_ms {
            return Err(UploadError::TooLong {
                max_ms,
                actual_ms: upload.duration_ms(),
            });
        }
        Ok(())
    }

    /// Gives a failed clip (held busy by the caller) another go.
    async fn retry(&self, id: &AudioId, how: Retry) -> Result<UploadStatus, UploadError> {
        let record = {
            let mut inner = self.lock();
            let clip = inner.live(id)?;
            let state = match how {
                Retry::Requeue => Phase::Queued,
                Retry::Reupload => Phase::Uploading,
            };
            Record {
                state,
                ..clip.record.clone()
            }
        };
        let removed = match how {
            Retry::Requeue => Ok(()),
            Retry::Reupload => self.store.remove_audio(id).await,
        };
        let saved = match &removed {
            Ok(()) => self.store.save(id, &record).await,
            Err(_) => Ok(()),
        };
        let mut inner = self.lock();
        let clip = inner.live(id)?;
        clip.hold = Hold::Free;
        if matches!(how, Retry::Reupload) && removed.is_ok() {
            clip.offset = 0;
            clip.has_audio = false;
        }
        removed?;
        saved?;
        clip.record = record;
        clip.touched = SystemTime::now();
        let status = clip.status(id);
        if matches!(how, Retry::Requeue) {
            inner.queue.push_back(id.clone());
            drop(inner);
            self.wake.notify_one();
        }
        Ok(status)
    }

    /// Appends `chunk` at `offset` to `device`'s clip `id`. The offset must
    /// be the clip's current one. The last byte queues the clip for
    /// transcription.
    pub async fn append(
        self: &Arc<Self>,
        device: &str,
        id: &AudioId,
        offset: u64,
        chunk: Bytes,
    ) -> Result<UploadStatus, UploadError> {
        let this = Arc::clone(self);
        let device = device.to_owned();
        let id = id.clone();
        detached(async move { this.append_now(&device, &id, offset, chunk).await })
            .await
            .map_err(task_failed)?
    }

    async fn append_now(
        &self,
        device: &str,
        id: &AudioId,
        offset: u64,
        chunk: Bytes,
    ) -> Result<UploadStatus, UploadError> {
        let len = chunk.len() as u64;
        let total = {
            let mut inner = self.lock();
            let clip = inner.owned(id, device)?;
            if clip.hold != Hold::Free
                || clip.record.state != Phase::Uploading
                || offset != clip.offset
            {
                return Err(UploadError::OffsetMismatch(clip.status(id)));
            }
            let total = clip.record.total_bytes;
            if offset.saturating_add(len) > total {
                return Err(UploadError::PastEnd(clip.status(id)));
            }
            clip.touched = SystemTime::now();
            if len == 0 {
                return Ok(clip.status(id));
            }
            clip.hold = Hold::Busy;
            total
        };
        if let Err(error) = self.store.write_at(id, offset, &chunk).await {
            // Whatever part of the chunk landed is cut off by the retry,
            // which writes at the same offset.
            self.release(id);
            return Err(error.into());
        }
        let end = offset + len;
        if end == total {
            return self.complete(id).await;
        }
        let mut inner = self.lock();
        let clip = inner.live(id)?;
        clip.offset = end;
        clip.has_audio = true;
        clip.hold = Hold::Free;
        Ok(clip.status(id))
    }

    /// Frees a clip held busy, leaving its state as it was.
    fn release(&self, id: &AudioId) {
        if let Ok(clip) = self.lock().live(id) {
            clip.hold = Hold::Free;
        }
    }

    /// The last chunk is in: record the clip as queued and wake the worker.
    /// If that cannot be recorded the offset stays at the chunk's start, so
    /// the phone retries the chunk (rewriting it in place).
    async fn complete(&self, id: &AudioId) -> Result<UploadStatus, UploadError> {
        let record = {
            let mut inner = self.lock();
            let clip = inner.live(id)?;
            Record {
                state: Phase::Queued,
                ..clip.record.clone()
            }
        };
        if let Err(error) = self.store.save(id, &record).await {
            self.release(id);
            return Err(error.into());
        }
        let mut inner = self.lock();
        let clip = inner.live(id)?;
        clip.offset = record.total_bytes;
        clip.has_audio = true;
        clip.hold = Hold::Free;
        clip.record = record;
        let status = clip.status(id);
        inner.queue.push_back(id.clone());
        drop(inner);
        self.wake.notify_one();
        Ok(status)
    }

    /// The upload progress of `device`'s clip `id`.
    pub fn status(&self, device: &str, id: &AudioId) -> Result<UploadStatus, UploadError> {
        let mut inner = self.lock();
        Ok(inner.owned(id, device)?.status(id))
    }

    /// The transcript of `device`'s clip `id`, or why there is none (yet).
    pub fn transcript(&self, device: &str, id: &AudioId) -> Result<TranscriptView, UploadError> {
        let mut inner = self.lock();
        let clip = inner.owned(id, device)?;
        Ok(match &clip.record.state {
            Phase::Done {
                text, duration_ms, ..
            } => TranscriptView::Ready(Transcript {
                audio_id: id.clone(),
                text: text.clone(),
                duration_ms: *duration_ms,
            }),
            Phase::Failed { .. } => TranscriptView::Failed(clip.status(id)),
            Phase::Uploading | Phase::Queued | Phase::Transcribing => {
                TranscriptView::Pending(clip.status(id))
            }
        })
    }

    /// Starts the transcription worker: one clip at a time, oldest first,
    /// until `shutdown`. A clip interrupted by shutdown is queued again on
    /// the next start.
    pub fn spawn_worker(self: &Arc<Self>, mut shutdown: Shutdown) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let woken = this.wake.notified();
                let Some(id) = this.next_job() else {
                    tokio::select! {
                        () = woken => continue,
                        () = shutdown.wait() => break,
                    }
                };
                tokio::select! {
                    () = this.transcribe(&id) => {}
                    () = shutdown.wait() => break,
                }
            }
        })
    }

    /// Takes the oldest queued clip and marks it transcribing.
    fn next_job(&self) -> Option<AudioId> {
        let mut inner = self.lock();
        while let Some(id) = inner.queue.pop_front() {
            if let Ok(clip) = inner.live(&id)
                && clip.record.state == Phase::Queued
            {
                clip.record.state = Phase::Transcribing;
                return Some(id);
            }
        }
        None
    }

    async fn transcribe(&self, id: &AudioId) {
        let declared = {
            let mut inner = self.lock();
            let Ok(clip) = inner.live(id) else {
                return;
            };
            Duration::from_millis(clip.record.duration_ms)
        };
        let audio = match self.store.read_audio(id).await {
            Ok(audio) => Bytes::from(audio),
            Err(error) => {
                tracing::warn!(audio_id = %id, %error, "cannot read a queued clip");
                self.finish(id, Err(AudioFailure::Transcription)).await;
                return;
            }
        };
        let started = std::time::Instant::now();
        let outcome = match self.transcribe_with_retries(id, audio).await {
            Ok(recognized) => {
                let duration = recognized.duration.unwrap_or(declared);
                tracing::info!(
                    audio_id = %id,
                    audio_ms = millis(duration),
                    took_ms = millis(started.elapsed()),
                    "transcribed a clip"
                );
                if duration > self.settings.max_duration + DURATION_SLACK {
                    Err(AudioFailure::TooLong)
                } else {
                    Ok((clean_text(&recognized.text), millis(duration)))
                }
            }
            Err(error) => {
                tracing::warn!(audio_id = %id, %error, "transcription failed");
                Err(AudioFailure::Transcription)
            }
        };
        self.finish(id, outcome).await;
    }

    /// Calls the backend, waiting out [`AudioSettings::retry_delays`] while
    /// it is unreachable.
    async fn transcribe_with_retries(
        &self,
        id: &AudioId,
        audio: Bytes,
    ) -> Result<Recognized, TranscribeError> {
        let mut delays = self.settings.retry_delays.iter();
        loop {
            match self.settings.transcriber.transcribe(audio.clone()).await {
                Err(error) if error.is_transient() => {
                    let Some(delay) = delays.next() else {
                        return Err(error);
                    };
                    tracing::warn!(audio_id = %id, %error, ?delay, "whisper-server unavailable; retrying");
                    tokio::time::sleep(*delay).await;
                }
                outcome => return outcome,
            }
        }
    }

    /// Records a clip's final phase, deleting its audio at once if it was
    /// transcribed, retention is zero and the result is safely on disk. A
    /// failed clip keeps its audio for a retry.
    async fn finish(&self, id: &AudioId, outcome: Result<(String, u64), AudioFailure>) {
        let finished_ms = unix_ms(SystemTime::now());
        let transcribed = outcome.is_ok();
        let state = match outcome {
            Ok((text, duration_ms)) => Phase::Done {
                text,
                duration_ms,
                finished_ms,
            },
            Err(failure) => Phase::Failed {
                failure,
                finished_ms,
            },
        };
        let record = {
            let mut inner = self.lock();
            let Ok(clip) = inner.live(id) else {
                return;
            };
            Record {
                state,
                ..clip.record.clone()
            }
        };
        let saved = self.store.save(id, &record).await;
        if let Err(error) = &saved {
            // The outcome holds in memory and retention saves it again
            // later; until then the disk says "queued", so a restart
            // transcribes the clip again, and its audio is kept for that.
            tracing::warn!(audio_id = %id, %error, "cannot record a transcription result");
        }
        let mut drop_audio =
            transcribed && saved.is_ok() && self.settings.audio_retention.is_zero();
        if drop_audio && let Err(error) = self.store.remove_audio(id).await {
            tracing::warn!(audio_id = %id, %error, "cannot delete transcribed audio");
            drop_audio = false;
        }
        let mut inner = self.lock();
        if let Ok(clip) = inner.live(id) {
            clip.record = record;
            clip.has_audio &= !drop_audio;
            clip.unsaved = saved.is_err();
        }
    }

    /// Starts retention: a sweep now, then every
    /// [`AudioSettings::sweep_every`], until `shutdown`.
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
                let swept = this.sweep(SystemTime::now()).await;
                if swept != Swept::default() {
                    tracing::info!(clips = swept.clips, audio = swept.audio, "audio retention");
                }
            }
        })
    }

    /// Applies retention as of `now` (see the module docs).
    ///
    /// Every clip the sweep works on is reserved first, as a write would
    /// be: held busy while its audio is deleted or its unsaved result is
    /// written again, held for removal (gone to readers, its id not
    /// declarable) until a fully expired clip's files are deleted. So no request can
    /// write files that the sweep then deletes.
    pub async fn sweep(&self, now: SystemTime) -> Swept {
        let settings = &self.settings;
        let mut whole = Vec::new();
        let mut audio = Vec::new();
        let mut resave = Vec::new();
        {
            let mut inner = self.lock();
            for (id, clip) in &mut inner.clips {
                if clip.hold != Hold::Free {
                    continue;
                }
                if let Some(finished_ms) = clip.record.state.finished_ms() {
                    let finished = UNIX_EPOCH + Duration::from_millis(finished_ms);
                    if now >= finished + settings.record_retention {
                        clip.hold = Hold::Removing;
                        whole.push(id.clone());
                    } else if clip.unsaved {
                        clip.hold = Hold::Busy;
                        resave.push((id.clone(), clip.record.clone()));
                    } else if clip.has_audio
                        && matches!(clip.record.state, Phase::Done { .. })
                        && now >= finished + settings.audio_retention
                    {
                        clip.hold = Hold::Busy;
                        audio.push(id.clone());
                    }
                } else if clip.record.state == Phase::Uploading
                    && now >= clip.touched + settings.record_retention
                {
                    clip.hold = Hold::Removing;
                    whole.push(id.clone());
                }
            }
        }
        for (id, record) in &resave {
            let saved = self.store.save(id, record).await;
            if let Err(error) = &saved {
                tracing::warn!(audio_id = %id, %error, "still cannot record a transcription result");
            }
            let mut inner = self.lock();
            if let Ok(clip) = inner.live(id) {
                clip.hold = Hold::Free;
                clip.unsaved = saved.is_err();
            }
        }
        let mut audio_deleted = 0;
        for id in &audio {
            let removed = self.store.remove_audio(id).await;
            if let Err(error) = &removed {
                tracing::warn!(audio_id = %id, %error, "cannot delete expired audio");
            } else {
                audio_deleted += 1;
            }
            let mut inner = self.lock();
            if let Ok(clip) = inner.live(id) {
                clip.hold = Hold::Free;
                clip.has_audio &= removed.is_err();
            }
        }
        let mut clips_deleted = 0;
        for id in &whole {
            if self.remove_clip(id).await {
                clips_deleted += 1;
            }
        }
        Swept {
            clips: clips_deleted,
            audio: audio_deleted,
        }
    }

    /// Deletes a clip held for removal: its files, then its entry. If a
    /// file cannot be deleted the clip stays, released, with what is left
    /// of it (the next sweep tries again); whether it is gone.
    async fn remove_clip(&self, id: &AudioId) -> bool {
        // The audio goes first: a record without audio is a valid clip.
        let removed = match self.store.remove_audio(id).await {
            Ok(()) => self.store.remove_record(id).await.map_err(|e| (e, true)),
            Err(error) => Err((error, false)),
        };
        let mut inner = self.lock();
        match removed {
            Ok(()) => {
                inner.clips.remove(id);
                true
            }
            Err((error, audio_gone)) => {
                tracing::warn!(audio_id = %id, %error, "cannot delete a clip; keeping it");
                if let Some(clip) = inner.clips.get_mut(id) {
                    clip.hold = Hold::Free;
                    if audio_gone {
                        clip.has_audio = false;
                        if clip.record.state == Phase::Uploading {
                            clip.offset = 0;
                        }
                    }
                }
                false
            }
        }
    }
}

/// The error for upload work whose task died. Detached work (see
/// [`detached`]) is never cancelled by a dropped request, so a clip it
/// holds is always released and memory always matches the disk.
fn task_failed(error: tokio::task::JoinError) -> UploadError {
    UploadError::Io(io::Error::other(error))
}

/// The backend's text without control or hidden characters, as one line of
/// prose. whisper.cpp ends every segment with a newline, and a segment can
/// end mid-word ("dict" then "ation") or before punctuation: a segment that
/// starts a new word begins with its own space, one that continues a word
/// does not. So newlines are dropped, not turned into spaces, and only then
/// do runs of spaces become one.
fn clean_text(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|&c| !text::is_disallowed(c, false))
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `N` bytes from the kernel's random source.
fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// A fresh random (version 4) clip id.
fn random_audio_id() -> io::Result<AudioId> {
    let mut bytes: [u8; 16] = random_bytes()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut id = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            id.push('-');
        }
        let _ = write!(id, "{byte:02x}");
    }
    AudioId::new(id).map_err(io::Error::other)
}

fn unix_ms(at: SystemTime) -> u64 {
    millis(at.duration_since(UNIX_EPOCH).unwrap_or_default())
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Semaphore;

    use super::*;
    use crate::testing::set_mode;

    const DAY: Duration = Duration::from_hours(24);

    /// The device the tests upload as.
    const OWNER: &str = "dev-0000000000000001";

    /// Answers every clip with its own bytes as text (reporting `reported`
    /// as the decoded length). The first `unreachable` calls fail as if
    /// whisper-server were down; `fail` fails every call for good. A gated
    /// one holds each call until [`Echo::release`].
    #[derive(Debug)]
    struct Echo {
        fail: bool,
        unreachable: AtomicUsize,
        reported: Option<Duration>,
        calls: AtomicUsize,
        gate: Semaphore,
    }

    impl Default for Echo {
        fn default() -> Self {
            Self {
                fail: false,
                unreachable: AtomicUsize::new(0),
                reported: None,
                calls: AtomicUsize::new(0),
                gate: Semaphore::new(Semaphore::MAX_PERMITS),
            }
        }
    }

    impl Echo {
        fn gated() -> Self {
            Self {
                gate: Semaphore::new(0),
                ..Self::default()
            }
        }

        fn release(&self) {
            self.gate.add_permits(1);
        }
    }

    impl Transcriber for Echo {
        fn transcribe(&self, audio: Bytes) -> TranscribeFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                self.gate.acquire().await.expect("gate").forget();
                let down = self
                    .unreachable
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok();
                if down {
                    return Err(TranscribeError::Connect {
                        addr: "127.0.0.1:8178".parse().expect("addr"),
                        source: io::ErrorKind::ConnectionRefused.into(),
                    });
                }
                if self.fail {
                    return Err(TranscribeError::Server("no".to_owned()));
                }
                Ok(Recognized {
                    text: format!("  {}\u{200b}\u{7} ", String::from_utf8_lossy(&audio)),
                    duration: self.reported,
                })
            })
        }
    }

    struct Fixture {
        audio: Arc<Audio>,
        tmp: tempfile::TempDir,
        _trigger: crate::shutdown::ShutdownTrigger,
    }

    async fn open(
        dir: &Path,
        echo: Arc<Echo>,
        adjust: impl FnOnce(&mut AudioSettings),
    ) -> Arc<Audio> {
        let mut settings = AudioSettings::new(dir, echo);
        settings.retry_delays = vec![Duration::from_millis(1); 3];
        adjust(&mut settings);
        Arc::new(Audio::open(settings).await.expect("open"))
    }

    async fn fixture_with(echo: Arc<Echo>, adjust: impl FnOnce(&mut AudioSettings)) -> Fixture {
        let tmp = tempfile::tempdir().expect("tmp");
        let audio = open(&tmp.path().join("audio"), echo, adjust).await;
        let (trigger, shutdown) = crate::shutdown::channel();
        audio.spawn_worker(shutdown);
        Fixture {
            audio,
            tmp,
            _trigger: trigger,
        }
    }

    async fn fixture(adjust: impl FnOnce(&mut AudioSettings)) -> Fixture {
        fixture_with(Arc::new(Echo::default()), adjust).await
    }

    fn upload(id: Option<&AudioId>, total_bytes: u64, duration_ms: u64) -> NewUpload {
        NewUpload::new(id.cloned(), total_bytes, duration_ms).expect("valid")
    }

    async fn new_clip(audio: &Arc<Audio>, total: u64) -> AudioId {
        match audio
            .create(OWNER, upload(None, total, 1500))
            .await
            .expect("create")
        {
            Created::New(status) => status.audio_id,
            Created::Existing(_) => panic!("expected a new clip"),
        }
    }

    async fn upload_all(audio: &Arc<Audio>, bytes: &'static [u8]) -> AudioId {
        let id = new_clip(audio, bytes.len() as u64).await;
        audio
            .append(OWNER, &id, 0, Bytes::from_static(bytes))
            .await
            .expect("whole clip");
        id
    }

    async fn transcript(audio: &Audio, id: &AudioId) -> TranscriptView {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match audio.transcript(OWNER, id).expect("known") {
                    TranscriptView::Pending(_) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    view => return view,
                }
            }
        })
        .await
        .expect("transcribed in time")
    }

    #[tokio::test]
    async fn caps_are_enforced_at_declaration() {
        let f = fixture(|s| {
            s.max_upload_bytes = 10;
            s.max_duration = Duration::from_secs(1);
            s.max_clips = 1;
        })
        .await;
        assert!(matches!(
            f.audio.create(OWNER, upload(None, 11, 1)).await,
            Err(UploadError::TooLarge {
                max: 10,
                actual: 11
            })
        ));
        assert!(matches!(
            f.audio.create(OWNER, upload(None, 5, 1001)).await,
            Err(UploadError::TooLong { max_ms: 1000, .. })
        ));
        f.audio
            .create(OWNER, upload(None, 5, 1000))
            .await
            .expect("at the caps");
        assert!(matches!(
            f.audio.create(OWNER, upload(None, 5, 1)).await,
            Err(UploadError::Full)
        ));
    }

    #[tokio::test]
    async fn only_clips_in_flight_count_against_the_cap() {
        let f = fixture(|s| s.max_clips = 1).await;
        let first = upload_all(&f.audio, b"one").await;
        assert!(matches!(
            transcript(&f.audio, &first).await,
            TranscriptView::Ready(_)
        ));
        // The finished clip is still held (for its transcript), yet a new
        // upload is welcome.
        let second = upload_all(&f.audio, b"two").await;
        assert!(f.audio.status(OWNER, &first).is_ok());
        assert!(matches!(
            transcript(&f.audio, &second).await,
            TranscriptView::Ready(_)
        ));
    }

    #[tokio::test]
    async fn chunks_must_arrive_in_order_and_resume_at_the_offset() {
        let f = fixture(|_| {}).await;
        let id = new_clip(&f.audio, 6).await;
        let status = f
            .audio
            .append(OWNER, &id, 0, Bytes::from_static(b"he"))
            .await
            .expect("first");
        assert_eq!((status.offset, status.state), (2, ClipState::Uploading));

        // A retried or skipped chunk is refused with the real offset.
        for wrong in [0, 3] {
            match f
                .audio
                .append(OWNER, &id, wrong, Bytes::from_static(b"ll"))
                .await
            {
                Err(UploadError::OffsetMismatch(status)) => assert_eq!(status.offset, 2),
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(
            f.audio
                .append(OWNER, &id, 2, Bytes::from_static(b"llo!!"))
                .await,
            Err(UploadError::PastEnd(_))
        ));
        assert_eq!(f.audio.status(OWNER, &id).expect("status").offset, 2);
        let done = f
            .audio
            .append(OWNER, &id, 2, Bytes::from_static(b"llo!"))
            .await
            .expect("last");
        assert_eq!(done.offset, 6);
        assert_ne!(done.state, ClipState::Uploading);
        assert!(matches!(
            f.audio.append(OWNER, &id, 6, Bytes::new()).await,
            Err(UploadError::OffsetMismatch(_))
        ));

        let TranscriptView::Ready(transcript) = transcript(&f.audio, &id).await else {
            panic!("expected a transcript");
        };
        assert_eq!(transcript.text, "hello!");
        assert_eq!(transcript.duration_ms, 1500);
    }

    #[tokio::test]
    async fn a_dropped_request_still_finishes_its_write() {
        let f = fixture(|_| {}).await;
        let id = new_clip(&f.audio, 4).await;
        // Polled once, then dropped mid-write (as when the client goes away
        // or the request timeout fires).
        let _ = tokio::time::timeout(
            Duration::ZERO,
            f.audio.append(OWNER, &id, 0, Bytes::from_static(b"ab")),
        )
        .await;
        let resumed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match f
                    .audio
                    .append(OWNER, &id, 2, Bytes::from_static(b"cd"))
                    .await
                {
                    Ok(status) => return status,
                    Err(UploadError::OffsetMismatch(_)) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(other) => panic!("{other:?}"),
                }
            }
        })
        .await
        .expect("the first chunk landed and the clip is not stuck busy");
        assert_eq!(resumed.offset, 4);
    }

    #[tokio::test]
    async fn redeclaring_an_id_resumes_and_a_different_clip_conflicts() {
        let f = fixture(|_| {}).await;
        let id = AudioId::new("00000000-0000-4000-8000-0000000000c3").expect("id");
        assert!(matches!(
            f.audio.create(OWNER, upload(Some(&id), 4, 1500)).await,
            Ok(Created::New(_))
        ));
        f.audio
            .append(OWNER, &id, 0, Bytes::from_static(b"ab"))
            .await
            .expect("chunk");
        match f.audio.create(OWNER, upload(Some(&id), 4, 1500)).await {
            Ok(Created::Existing(status)) => assert_eq!(status.offset, 2),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            f.audio.create(OWNER, upload(Some(&id), 5, 1500)).await,
            Err(UploadError::IdConflict(_))
        ));
    }

    #[tokio::test]
    async fn uploads_and_the_queue_survive_a_restart() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("audio");
        let echo = Arc::new(Echo::default());
        let (partial, queued) = {
            // No worker: the completed clip stays queued.
            let audio = open(&dir, echo.clone(), |_| {}).await;
            let partial = new_clip(&audio, 4).await;
            audio
                .append(OWNER, &partial, 0, Bytes::from_static(b"ab"))
                .await
                .expect("chunk");
            let queued = upload_all(&audio, b"ok").await;
            (partial, queued)
        };
        let audio = open(&dir, echo.clone(), |_| {}).await;
        assert_eq!(audio.status(OWNER, &partial).expect("partial").offset, 2);
        audio
            .append(OWNER, &partial, 2, Bytes::from_static(b"cd"))
            .await
            .expect("resumed");
        let (_trigger, shutdown) = crate::shutdown::channel();
        audio.spawn_worker(shutdown);
        for (id, text) in [(&queued, "ok"), (&partial, "abcd")] {
            match transcript(&audio, id).await {
                TranscriptView::Ready(t) => assert_eq!(t.text, text),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(echo.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_unreachable_backend_is_retried() {
        let echo = Arc::new(Echo {
            unreachable: AtomicUsize::new(2),
            ..Echo::default()
        });
        let f = fixture_with(echo.clone(), |_| {}).await;
        let id = upload_all(&f.audio, b"hi").await;
        assert!(matches!(
            transcript(&f.audio, &id).await,
            TranscriptView::Ready(_)
        ));
        assert_eq!(echo.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_clip_that_failed_is_retried_when_declared_again() {
        let echo = Arc::new(Echo {
            unreachable: AtomicUsize::new(10),
            ..Echo::default()
        });
        let f = fixture_with(echo.clone(), |s| s.retry_delays.clear()).await;
        let id = AudioId::new("00000000-0000-4000-8000-0000000000e5").expect("id");
        f.audio
            .create(OWNER, upload(Some(&id), 2, 1500))
            .await
            .expect("declare");
        f.audio
            .append(OWNER, &id, 0, Bytes::from_static(b"hi"))
            .await
            .expect("all");
        match transcript(&f.audio, &id).await {
            TranscriptView::Failed(status) => {
                assert_eq!(status.failure, Some(AudioFailure::Transcription));
            }
            other => panic!("{other:?}"),
        }
        echo.unreachable.store(0, Ordering::SeqCst);
        match f.audio.create(OWNER, upload(Some(&id), 2, 1500)).await {
            Ok(Created::Existing(status)) => assert_eq!(status.state, ClipState::Queued),
            other => panic!("{other:?}"),
        }
        match transcript(&f.audio, &id).await {
            TranscriptView::Ready(t) => assert_eq!(t.text, "hi"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failed_clip_without_audio_is_uploaded_again() {
        let tmp = tempfile::tempdir().expect("tmp");
        let id = AudioId::new("00000000-0000-4000-8000-0000000000f6").expect("id");
        {
            // No worker: the clip stays queued, then its audio goes missing.
            let audio = open(tmp.path(), Arc::new(Echo::default()), |_| {}).await;
            audio
                .create(OWNER, upload(Some(&id), 2, 1500))
                .await
                .expect("declare");
            audio
                .append(OWNER, &id, 0, Bytes::from_static(b"hi"))
                .await
                .expect("all");
        }
        std::fs::remove_file(tmp.path().join(format!("{id}.audio"))).expect("lose audio");
        let audio = open(tmp.path(), Arc::new(Echo::default()), |_| {}).await;
        let (_trigger, shutdown) = crate::shutdown::channel();
        audio.spawn_worker(shutdown);
        assert!(matches!(
            transcript(&audio, &id).await,
            TranscriptView::Failed(_)
        ));
        match audio.create(OWNER, upload(Some(&id), 2, 1500)).await {
            Ok(Created::Existing(status)) => {
                assert_eq!((status.state, status.offset), (ClipState::Uploading, 0));
            }
            other => panic!("{other:?}"),
        }
    }

    async fn failed_with(transcriber: Echo, max_duration: Duration) -> UploadStatus {
        let f = fixture_with(Arc::new(transcriber), |s| s.max_duration = max_duration).await;
        let id = upload_all(&f.audio, b"x").await;
        match transcript(&f.audio, &id).await {
            TranscriptView::Failed(status) => status,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_backend_failure_fails_the_clip() {
        let failing = Echo {
            fail: true,
            ..Echo::default()
        };
        let status = failed_with(failing, Duration::from_secs(60)).await;
        assert_eq!(status.state, ClipState::Failed);
        assert_eq!(status.failure, Some(AudioFailure::Transcription));
    }

    #[tokio::test]
    async fn a_clip_that_decodes_past_the_cap_fails() {
        // The phone declared 1.5 s (within the cap); the decoder says 10 s.
        let lying = Echo {
            reported: Some(Duration::from_secs(10)),
            ..Echo::default()
        };
        let status = failed_with(lying, Duration::from_secs(2)).await;
        assert_eq!(status.failure, Some(AudioFailure::TooLong));
    }

    #[tokio::test]
    async fn retention_deletes_audio_then_records_and_abandoned_uploads() {
        let f = fixture(|s| {
            s.audio_retention = DAY;
            s.record_retention = 7 * DAY;
        })
        .await;
        let done = upload_all(&f.audio, b"hi").await;
        let abandoned = new_clip(&f.audio, 9).await;
        f.audio
            .append(OWNER, &abandoned, 0, Bytes::from_static(b"x"))
            .await
            .expect("part");
        assert!(matches!(
            transcript(&f.audio, &done).await,
            TranscriptView::Ready(_)
        ));
        let audio_file = f.audio.settings().dir.join(format!("{done}.audio"));
        let record_file = f.audio.settings().dir.join(format!("{done}.json"));
        assert!(audio_file.exists());

        let now = SystemTime::now();
        assert_eq!(f.audio.sweep(now).await, Swept::default());
        assert_eq!(
            f.audio.sweep(now + DAY + Duration::from_secs(1)).await,
            Swept { clips: 0, audio: 1 }
        );
        assert!(!audio_file.exists());
        assert!(record_file.exists());
        assert!(matches!(
            f.audio.transcript(OWNER, &done),
            Ok(TranscriptView::Ready(_))
        ));

        assert_eq!(
            f.audio.sweep(now + 7 * DAY + Duration::from_secs(1)).await,
            Swept { clips: 2, audio: 0 }
        );
        assert!(!record_file.exists());
        assert!(matches!(
            f.audio.transcript(OWNER, &done),
            Err(UploadError::NotFound)
        ));
        assert!(matches!(
            f.audio.status(OWNER, &abandoned),
            Err(UploadError::NotFound)
        ));
        let left = std::fs::read_dir(&f.audio.settings().dir)
            .expect("ls")
            .count();
        assert_eq!(left, 0);
        // The id is free again once the sweep is done.
        f.audio
            .create(OWNER, upload(Some(&done), 2, 1500))
            .await
            .expect("declared afresh");
    }

    #[tokio::test]
    async fn zero_retention_deletes_audio_once_transcribed() {
        let f = fixture(|s| s.audio_retention = Duration::ZERO).await;
        let id = upload_all(&f.audio, b"hi").await;
        assert!(matches!(
            transcript(&f.audio, &id).await,
            TranscriptView::Ready(_)
        ));
        assert!(!f.audio.settings().dir.join(format!("{id}.audio")).exists());
        assert!(f.audio.settings().dir.join(format!("{id}.json")).exists());
    }

    #[tokio::test]
    async fn audio_is_kept_when_the_result_cannot_be_recorded() {
        let echo = Arc::new(Echo::gated());
        let f = fixture_with(echo.clone(), |s| s.audio_retention = Duration::ZERO).await;
        let id = upload_all(&f.audio, b"hi").await;
        let dir = f.audio.settings().dir.clone();
        set_mode(&dir, 0o500);
        echo.release();
        let view = transcript(&f.audio, &id).await;
        set_mode(&dir, 0o700);
        assert!(matches!(view, TranscriptView::Ready(_)));
        assert!(dir.join(format!("{id}.audio")).exists());

        // The disk still says queued: a restart would transcribe it again.
        let again = open(&dir, Arc::new(Echo::default()), |_| {}).await;
        assert_eq!(
            again.status(OWNER, &id).expect("kept").state,
            ClipState::Queued
        );

        // Retention writes the result again before it deletes any audio.
        let later = SystemTime::now() + DAY;
        assert_eq!(f.audio.sweep(later).await, Swept::default());
        assert!(dir.join(format!("{id}.audio")).exists());
        let reopened = open(&dir, Arc::new(Echo::default()), |_| {}).await;
        assert_eq!(
            reopened.status(OWNER, &id).expect("kept").state,
            ClipState::Done
        );
        assert_eq!(f.audio.sweep(later).await, Swept { clips: 0, audio: 1 });
        drop(f.tmp);
    }

    #[tokio::test]
    async fn a_failed_first_chunk_can_be_retried() {
        let f = fixture(|_| {}).await;
        let id = new_clip(&f.audio, 2).await;
        let dir = f.audio.settings().dir.clone();
        set_mode(&dir, 0o500);
        let failed = f
            .audio
            .append(OWNER, &id, 0, Bytes::from_static(b"hi"))
            .await;
        set_mode(&dir, 0o700);
        assert!(matches!(failed, Err(UploadError::Io(_))), "{failed:?}");
        let retried = f
            .audio
            .append(OWNER, &id, 0, Bytes::from_static(b"hi"))
            .await
            .expect("the clip is still there");
        assert_eq!(retried.offset, 2);
    }

    #[tokio::test]
    async fn a_last_chunk_that_cannot_be_recorded_is_undone() {
        let f = fixture(|_| {}).await;
        let id = new_clip(&f.audio, 4).await;
        f.audio
            .append(OWNER, &id, 0, Bytes::from_static(b"ab"))
            .await
            .expect("first");
        let dir = f.audio.settings().dir.clone();
        // The audio file is writable, but the queued record cannot be saved.
        set_mode(&dir, 0o500);
        let failed = f
            .audio
            .append(OWNER, &id, 2, Bytes::from_static(b"cd"))
            .await;
        set_mode(&dir, 0o700);
        assert!(matches!(failed, Err(UploadError::Io(_))), "{failed:?}");
        let status = f.audio.status(OWNER, &id).expect("kept");
        assert_eq!((status.offset, status.state), (2, ClipState::Uploading));
        let retried = f
            .audio
            .append(OWNER, &id, 2, Bytes::from_static(b"cd"))
            .await
            .expect("the same chunk again");
        assert_ne!(retried.state, ClipState::Uploading);
        match transcript(&f.audio, &id).await {
            TranscriptView::Ready(t) => assert_eq!(t.text, "abcd"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_complete_upload_left_uploading_is_queued_on_restart() {
        let tmp = tempfile::tempdir().expect("tmp");
        let echo = Arc::new(Echo::default());
        let audio = open(tmp.path(), echo.clone(), |_| {}).await;
        let id = upload_all(&audio, b"ok").await;
        drop(audio);
        let record = tmp.path().join(format!("{id}.json"));
        let text = std::fs::read_to_string(&record).expect("record");
        std::fs::write(&record, text.replace("queued", "uploading")).expect("rewrite");
        let audio = open(tmp.path(), echo, |_| {}).await;
        assert_eq!(
            audio.status(OWNER, &id).expect("kept").state,
            ClipState::Queued
        );
    }

    #[tokio::test]
    async fn the_idle_clock_survives_a_restart() {
        let tmp = tempfile::tempdir().expect("tmp");
        let echo = Arc::new(Echo::default());
        let week = |s: &mut AudioSettings| s.record_retention = 7 * DAY;
        let audio = open(tmp.path(), echo.clone(), week).await;
        let id = new_clip(&audio, 4).await;
        audio
            .append(OWNER, &id, 0, Bytes::from_static(b"ab"))
            .await
            .expect("part");
        drop(audio);
        let file = std::fs::File::options()
            .write(true)
            .open(tmp.path().join(format!("{id}.audio")))
            .expect("audio");
        file.set_modified(SystemTime::now() - 8 * DAY)
            .expect("backdate");
        let audio = open(tmp.path(), echo, week).await;
        assert_eq!(
            audio.sweep(SystemTime::now()).await,
            Swept { clips: 1, audio: 0 }
        );
    }

    #[tokio::test]
    async fn retrying_a_failed_clip_respects_the_cap() {
        let echo = Arc::new(Echo {
            fail: true,
            ..Echo::default()
        });
        let f = fixture_with(echo, |s| s.max_clips = 1).await;
        let id = AudioId::new("00000000-0000-4000-8000-0000000000a7").expect("id");
        f.audio
            .create(OWNER, upload(Some(&id), 2, 1500))
            .await
            .expect("declare");
        f.audio
            .append(OWNER, &id, 0, Bytes::from_static(b"hi"))
            .await
            .expect("all");
        assert!(matches!(
            transcript(&f.audio, &id).await,
            TranscriptView::Failed(_)
        ));
        new_clip(&f.audio, 9).await;
        assert!(matches!(
            f.audio.create(OWNER, upload(Some(&id), 2, 1500)).await,
            Err(UploadError::Full)
        ));
    }

    #[tokio::test]
    async fn a_failed_clip_keeps_its_audio_for_a_retry_even_at_zero_retention() {
        let echo = Arc::new(Echo {
            unreachable: AtomicUsize::new(10),
            ..Echo::default()
        });
        let f = fixture_with(echo.clone(), |s| {
            s.retry_delays.clear();
            s.audio_retention = Duration::ZERO;
            s.record_retention = 7 * DAY;
        })
        .await;
        let id = AudioId::new("00000000-0000-4000-8000-0000000000a8").expect("id");
        f.audio
            .create(OWNER, upload(Some(&id), 2, 1500))
            .await
            .expect("declare");
        f.audio
            .append(OWNER, &id, 0, Bytes::from_static(b"hi"))
            .await
            .expect("all");
        assert!(matches!(
            transcript(&f.audio, &id).await,
            TranscriptView::Failed(_)
        ));
        let audio_file = f.audio.settings().dir.join(format!("{id}.audio"));
        assert!(audio_file.exists());
        let later = SystemTime::now() + DAY;
        assert_eq!(f.audio.sweep(later).await, Swept::default());
        assert!(audio_file.exists());

        // whisper-server is back: declaring the clip again re-queues it,
        // no upload needed; once transcribed, its audio goes at once.
        echo.unreachable.store(0, Ordering::SeqCst);
        match f.audio.create(OWNER, upload(Some(&id), 2, 1500)).await {
            Ok(Created::Existing(status)) => assert_eq!(status.state, ClipState::Queued),
            other => panic!("{other:?}"),
        }
        match transcript(&f.audio, &id).await {
            TranscriptView::Ready(t) => assert_eq!(t.text, "hi"),
            other => panic!("{other:?}"),
        }
        assert!(!audio_file.exists());
    }

    #[tokio::test]
    async fn stale_audio_never_prefixes_a_new_upload() {
        let f = fixture(|_| {}).await;
        let id = AudioId::new("00000000-0000-4000-8000-0000000000a9").expect("id");
        f.audio
            .create(OWNER, upload(Some(&id), 2, 1500))
            .await
            .expect("declare");
        // Left behind by a delete that failed, or a chunk cut short.
        std::fs::write(f.audio.settings().dir.join(format!("{id}.audio")), b"stale")
            .expect("stale");
        f.audio
            .append(OWNER, &id, 0, Bytes::from_static(b"hi"))
            .await
            .expect("all");
        match transcript(&f.audio, &id).await {
            TranscriptView::Ready(t) => assert_eq!(t.text, "hi"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_clip_whose_files_cannot_be_deleted_stays_until_they_are() {
        let f = fixture(|s| s.record_retention = DAY).await;
        let id = AudioId::new("00000000-0000-4000-8000-0000000000aa").expect("id");
        f.audio
            .create(OWNER, upload(Some(&id), 2, 1500))
            .await
            .expect("declare");
        f.audio
            .append(OWNER, &id, 0, Bytes::from_static(b"hi"))
            .await
            .expect("all");
        assert!(matches!(
            transcript(&f.audio, &id).await,
            TranscriptView::Ready(_)
        ));
        let dir = f.audio.settings().dir.clone();
        let later = SystemTime::now() + 2 * DAY;
        set_mode(&dir, 0o500);
        let swept = f.audio.sweep(later).await;
        set_mode(&dir, 0o700);
        assert_eq!(swept, Swept::default());
        // Still known, so its id cannot be declared afresh over its files.
        assert!(matches!(
            f.audio.transcript(OWNER, &id),
            Ok(TranscriptView::Ready(_))
        ));
        assert!(matches!(
            f.audio.create(OWNER, upload(Some(&id), 2, 1500)).await,
            Ok(Created::Existing(_))
        ));
        // The next sweep tries again.
        assert_eq!(f.audio.sweep(later).await, Swept { clips: 1, audio: 0 });
        assert_eq!(std::fs::read_dir(&dir).expect("ls").count(), 0);
    }

    #[tokio::test]
    async fn an_idle_upload_makes_way_for_a_new_clip() {
        let f = fixture(|s| {
            s.max_clips = 1;
            s.upload_idle = Duration::ZERO;
        })
        .await;
        let idle = new_clip(&f.audio, 4).await;
        f.audio
            .append(OWNER, &idle, 0, Bytes::from_static(b"ab"))
            .await
            .expect("part");
        let fresh = new_clip(&f.audio, 2).await;
        assert!(matches!(
            f.audio.status(OWNER, &idle),
            Err(UploadError::NotFound)
        ));
        assert!(
            !f.audio
                .settings()
                .dir
                .join(format!("{idle}.audio"))
                .exists()
        );
        assert!(f.audio.status(OWNER, &fresh).is_ok());
    }

    #[tokio::test]
    async fn an_active_upload_is_not_evicted() {
        let f = fixture(|s| s.max_clips = 1).await;
        let active = new_clip(&f.audio, 4).await;
        assert!(matches!(
            f.audio.create(OWNER, upload(None, 2, 1500)).await,
            Err(UploadError::Full)
        ));
        assert!(f.audio.status(OWNER, &active).is_ok());
    }

    #[tokio::test]
    async fn redeclaring_while_the_first_declaration_saves_is_busy() {
        let f = fixture(|_| {}).await;
        let id = AudioId::new("00000000-0000-4000-8000-0000000000ab").expect("id");
        // As if the first create were still saving its record.
        f.audio.lock().clips.insert(
            id.clone(),
            Clip {
                record: Record {
                    total_bytes: 2,
                    duration_ms: 1500,
                    created_ms: 0,
                    owner: Some(OWNER.to_owned()),
                    state: Phase::Uploading,
                },
                offset: 0,
                has_audio: false,
                hold: Hold::Creating,
                touched: SystemTime::now(),
                unsaved: false,
            },
        );
        assert!(matches!(
            f.audio.create(OWNER, upload(Some(&id), 2, 1500)).await,
            Err(UploadError::Busy)
        ));
    }

    #[tokio::test]
    async fn an_id_with_files_set_aside_is_unavailable() {
        let tmp = tempfile::tempdir().expect("tmp");
        let id = AudioId::new("00000000-0000-4000-8000-0000000000ac").expect("id");
        std::fs::write(tmp.path().join(format!("{id}.json")), b"{not json").expect("bad");
        let audio = open(tmp.path(), Arc::new(Echo::default()), |_| {}).await;
        assert!(matches!(
            audio.create(OWNER, upload(Some(&id), 2, 1500)).await,
            Err(UploadError::IdUnavailable)
        ));
        // Other ids are fine.
        new_clip(&audio, 2).await;
    }

    #[tokio::test]
    async fn queued_clips_are_never_swept() {
        let tmp = tempfile::tempdir().expect("tmp");
        let audio = open(tmp.path(), Arc::new(Echo::default()), |_| {}).await;
        let id = upload_all(&audio, b"x").await;
        let later = SystemTime::now() + 365 * DAY;
        assert_eq!(audio.sweep(later).await, Swept::default());
        assert_eq!(
            audio.status(OWNER, &id).expect("kept").state,
            ClipState::Queued
        );
    }

    fn not_found<T>(result: &Result<T, UploadError>) -> bool {
        matches!(result, Err(UploadError::NotFound))
    }

    #[tokio::test]
    async fn another_device_never_sees_a_clip() {
        const OTHER: &str = "dev-0000000000000002";
        let tmp = tempfile::tempdir().expect("tmp");
        let audio = open(tmp.path(), Arc::new(Echo::default()), |_| {}).await;
        let id = AudioId::new("00000000-0000-4000-8000-0000000000c1").expect("id");
        audio
            .create(OWNER, upload(Some(&id), 4, 1500))
            .await
            .expect("create");
        assert!(not_found(&audio.status(OTHER, &id)));
        assert!(not_found(&audio.transcript(OTHER, &id)));
        assert!(not_found(
            &audio.append(OTHER, &id, 0, Bytes::from_static(b"ab")).await
        ));
        assert!(matches!(
            audio.create(OTHER, upload(Some(&id), 4, 1500)).await,
            Err(UploadError::IdUnavailable)
        ));
        assert_eq!(audio.status(OWNER, &id).expect("own").offset, 0);
        // Ownership is stored with the clip.
        drop(audio);
        let reopened = open(tmp.path(), Arc::new(Echo::default()), |_| {}).await;
        assert!(not_found(&reopened.status(OTHER, &id)));
        assert!(reopened.status(OWNER, &id).is_ok());
    }

    #[test]
    fn the_timeout_scales_with_the_duration_cap() {
        assert_eq!(
            whisper_timeout(Duration::from_mins(5)),
            Duration::from_mins(11)
        );
        assert_eq!(
            whisper_timeout(Duration::from_hours(1)),
            Duration::from_mins(121)
        );
    }

    #[test]
    fn random_ids_are_valid_v4_uuids() {
        let a = random_audio_id().expect("id");
        let b = random_audio_id().expect("id");
        assert_ne!(a, b);
        assert_eq!(&a.as_str()[14..15], "4");
    }

    #[test]
    fn cleaning_drops_controls_and_hidden_characters() {
        assert_eq!(
            clean_text("  yes\u{200b} please\u{1b}[31m\n "),
            "yes please[31m"
        );
    }

    /// whisper.cpp's `/inference` reply as the live server sends it: every
    /// segment's text carries its own leading space when it starts a word,
    /// and ends with a newline. A boundary can fall inside a word or before
    /// punctuation, where the next segment starts with no space.
    #[test]
    fn whisper_segments_join_by_their_own_spaces() {
        // The real reply for samples/jfk.wav with `max_len=6`, which makes
        // short segments and so shows boundaries before punctuation.
        assert_eq!(
            clean_text(
                " And\n so my\n fellow\n Americans\n, ask\n not\n what\n your\n country\n can\n do\n for\n you,\n ask\n what\n you\n can\n do\n for\n your\n country\n.\n"
            ),
            "And so my fellow Americans, ask not what your country can do for you, ask what you can do for your country."
        );
        // Mid-word boundaries in the same shape, as dictation produced them.
        assert_eq!(
            clean_text(" for a long dict\nation, if the screen\n times out, it trunc\nates it.\n"),
            "for a long dictation, if the screen times out, it truncates it."
        );
        assert_eq!(
            clean_text(" I\n'm dictating, the\nn it stops.\n"),
            "I'm dictating, then it stops."
        );
        // A segment that starts a word keeps its space; blank segments and
        // runs of spaces collapse.
        assert_eq!(
            clean_text(
                " So, the microphone opened, and\n it asked me for permissions.\n\n So I'm dictating.\n"
            ),
            "So, the microphone opened, and it asked me for permissions. So I'm dictating."
        );
        assert_eq!(clean_text("a\t\n  b"), "a b");
    }
}
