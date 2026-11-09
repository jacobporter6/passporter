//! The audio directory on disk: one `<id>.audio` file (the bytes received
//! so far) and one `<id>.json` [`Record`] per clip. The directory is `0700`
//! and every file `0600`: dictated audio is as private as the prompts it
//! becomes.
//!
//! The upload offset is never stored: it is the audio file's length, so a
//! crash between writing a chunk and recording it cannot desynchronise them.
//! Chunks are written at their offset (the file is cut there first), never
//! appended, so stray bytes past it (a chunk that failed half-way, a file
//! left behind by a failed delete) are overwritten instead of corrupting
//! the clip.

use std::collections::HashSet;
use std::fs::DirBuilder;
use std::io::{self, ErrorKind};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use protocol::{AudioFailure, AudioId};
use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

const AUDIO_EXT: &str = "audio";
const RECORD_EXT: &str = "json";
const TMP_SUFFIX: &str = ".tmp";
/// Appended to the files of a clip whose record cannot be read: set aside
/// for a human (or a newer bridge), never deleted or overwritten.
const QUARANTINE_SUFFIX: &str = ".unreadable";
/// Numbered names tried for a set-aside file before giving up.
const QUARANTINE_SLOTS: u32 = 100;

/// Where a clip is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Tolerant of unknown fields, so a rollback can read a newer bridge's
// records.
#[serde(tag = "phase", rename_all = "camelCase")]
pub(super) enum Phase {
    /// Waiting for more bytes.
    Uploading,
    /// Complete, waiting for the worker.
    Queued,
    /// With the backend now. Never written to disk: a restart re-queues it.
    Transcribing,
    /// Transcribed.
    #[serde(rename_all = "camelCase")]
    Done {
        text: String,
        duration_ms: u64,
        finished_ms: u64,
    },
    /// Transcription failed.
    #[serde(rename_all = "camelCase")]
    Failed {
        failure: AudioFailure,
        finished_ms: u64,
    },
}

impl Phase {
    /// When the clip reached a final phase (Unix milliseconds).
    pub(super) fn finished_ms(&self) -> Option<u64> {
        match self {
            Self::Done { finished_ms, .. } | Self::Failed { finished_ms, .. } => Some(*finished_ms),
            Self::Uploading | Self::Queued | Self::Transcribing => None,
        }
    }
}

/// A clip's metadata, as stored in `<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Record {
    /// Declared size in bytes.
    pub(super) total_bytes: u64,
    /// Declared length in milliseconds.
    pub(super) duration_ms: u64,
    /// Upload start (Unix milliseconds).
    pub(super) created_ms: u64,
    /// The device that declared the clip (its
    /// [`Principal::device`](crate::server::Principal::device)), the only
    /// one that sees it. `None` for a record written before clips had
    /// owners: every device sees it, as before, so an upload or transcript
    /// in flight across the upgrade is not lost (retention deletes it in
    /// time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) owner: Option<String>,
    pub(super) state: Phase,
}

impl Record {
    /// Whether `device` may see the clip: it declared it, or the clip
    /// predates owners.
    pub(super) fn owned_by(&self, device: &str) -> bool {
        self.owner.as_deref().is_none_or(|owner| owner == device)
    }
}

/// A clip found on disk at startup.
#[derive(Debug)]
pub(super) struct Loaded {
    pub(super) id: AudioId,
    pub(super) record: Record,
    /// The audio file's length, if it exists.
    pub(super) audio_len: Option<u64>,
    /// When the audio file was last written, if it exists.
    pub(super) audio_modified: Option<SystemTime>,
}

/// What [`Store::load`] found.
#[derive(Debug, Default)]
pub(super) struct LoadedDir {
    /// Every readable clip.
    pub(super) clips: Vec<Loaded>,
    /// Ids with files set aside, or whose unreadable record could not even
    /// be set aside: they must not be reused while those files exist.
    pub(super) reserved: Vec<AudioId>,
}

/// The audio directory.
#[derive(Debug, Clone)]
pub(super) struct Store {
    dir: PathBuf,
}

impl Store {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, id: &AudioId, ext: &str) -> PathBuf {
        // `AudioId` is a validated UUID, so this never leaves the directory.
        self.dir.join(format!("{id}.{ext}"))
    }

    /// Creates the directory (`0700`) if needed, deletes leftovers (temp
    /// files, audio without a record), sets aside clips whose record cannot
    /// be read (renamed with a `.unreadable` suffix, never deleted: it may
    /// be a newer bridge's format or a passing read error) and returns every
    /// clip, plus the ids that must stay unused (see
    /// [`LoadedDir::reserved`]). Blocking: run it off the async threads.
    pub(super) fn load(&self) -> io::Result<LoadedDir> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let mut records = Vec::new();
        let mut audio = HashSet::new();
        let mut reserved = HashSet::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(TMP_SUFFIX) {
                remove_quietly(&path);
                continue;
            }
            let Some((stem, ext)) = name.split_once('.') else {
                continue;
            };
            let Ok(id) = AudioId::new(stem) else {
                continue;
            };
            if ext.contains(QUARANTINE_SUFFIX) {
                reserved.insert(id);
                continue;
            }
            match ext {
                RECORD_EXT => records.push(id),
                AUDIO_EXT => {
                    audio.insert(id);
                }
                _ => {}
            }
        }
        let mut loaded = LoadedDir::default();
        for id in records {
            let record_path = self.path(&id, RECORD_EXT);
            let parsed = std::fs::read(&record_path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    serde_json::from_slice::<Record>(&bytes).map_err(|e| e.to_string())
                });
            let record = match parsed {
                Ok(record) => record,
                Err(error) => {
                    tracing::warn!(path = %record_path.display(), %error, "setting aside an unreadable audio record");
                    audio.remove(&id);
                    // Reserved either way: set aside, or still in place.
                    self.quarantine(&id);
                    reserved.insert(id);
                    continue;
                }
            };
            let meta = if audio.remove(&id) {
                let audio_path = self.path(&id, AUDIO_EXT);
                match std::fs::metadata(&audio_path) {
                    Ok(meta) => Some(meta),
                    Err(error) => {
                        // Treated as missing (and removed if possible): an
                        // upload restarts at 0, a queued clip fails and can
                        // be declared again.
                        tracing::warn!(path = %audio_path.display(), %error, "cannot inspect an audio file");
                        remove_quietly(&audio_path);
                        None
                    }
                }
            } else {
                None
            };
            loaded.clips.push(Loaded {
                id,
                record,
                audio_len: meta.as_ref().map(std::fs::Metadata::len),
                audio_modified: meta.and_then(|m| m.modified().ok()),
            });
        }
        for orphan in audio {
            remove_quietly(&self.path(&orphan, AUDIO_EXT));
        }
        loaded.reserved = reserved.into_iter().collect();
        Ok(loaded)
    }

    /// Renames `id`'s files out of the way, each to the first free name of
    /// `<id>.<ext>.unreadable`, `<id>.<ext>.unreadable.2`, ... so earlier
    /// set-aside files are never overwritten. Only this process writes the
    /// directory, so checking for a free name first is not racy.
    fn quarantine(&self, id: &AudioId) {
        for ext in [RECORD_EXT, AUDIO_EXT] {
            let path = self.path(id, ext);
            if !path.exists() {
                continue;
            }
            let free = (1..=QUARANTINE_SLOTS)
                .map(|n| {
                    let suffix = if n == 1 {
                        String::new()
                    } else {
                        format!(".{n}")
                    };
                    self.dir
                        .join(format!("{id}.{ext}{QUARANTINE_SUFFIX}{suffix}"))
                })
                .find(|aside| !aside.exists());
            let moved = match free {
                Some(aside) => std::fs::rename(&path, &aside),
                None => Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    "every set-aside name is taken",
                )),
            };
            if let Err(error) = moved {
                tracing::warn!(path = %path.display(), %error, "cannot set aside an audio file");
            }
        }
    }

    /// Writes `<id>.json` durably: temp file, fsync, rename, then fsync the
    /// directory so the rename itself survives a power loss (otherwise a
    /// new clip's audio could outlive its record and be deleted as an
    /// orphan on the next start).
    pub(super) async fn save(&self, id: &AudioId, record: &Record) -> io::Result<()> {
        let path = self.path(id, RECORD_EXT);
        let tmp = path.with_extension(format!("{RECORD_EXT}{TMP_SUFFIX}"));
        let json = serde_json::to_vec(record).map_err(io::Error::other)?;
        let mut file = private_file(&tmp).await?;
        file.set_len(0).await?;
        file.write_all(&json).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, &path).await?;
        tokio::fs::File::open(&self.dir).await?.sync_all().await
    }

    /// Writes `chunk` at byte `offset` of `<id>.audio` (creating it),
    /// cutting the file at `offset` first, and syncs it to disk. Whatever
    /// lay past `offset` is discarded; an `offset` past the end of the file
    /// is refused, as it would leave a hole.
    pub(super) async fn write_at(&self, id: &AudioId, offset: u64, chunk: &[u8]) -> io::Result<()> {
        let mut file = private_file(&self.path(id, AUDIO_EXT)).await?;
        let len = file.metadata().await?.len();
        if offset > len {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("a chunk at {offset} would leave a hole after byte {len}"),
            ));
        }
        file.set_len(offset).await?;
        file.seek(io::SeekFrom::Start(offset)).await?;
        file.write_all(chunk).await?;
        file.flush().await?;
        file.sync_data().await
    }

    /// The whole of `<id>.audio`.
    pub(super) async fn read_audio(&self, id: &AudioId) -> io::Result<Vec<u8>> {
        tokio::fs::read(self.path(id, AUDIO_EXT)).await
    }

    /// Deletes `<id>.audio`, keeping the record (and its transcript).
    pub(super) async fn remove_audio(&self, id: &AudioId) -> io::Result<()> {
        remove_if_present(&self.path(id, AUDIO_EXT)).await
    }

    /// Deletes `<id>.json`. Delete the audio first: a record without audio
    /// is a valid state, audio without a record is not.
    pub(super) async fn remove_record(&self, id: &AudioId) -> io::Result<()> {
        remove_if_present(&self.path(id, RECORD_EXT)).await
    }
}

/// Opens (creating) `path` for writing as a `0600` file, contents intact.
/// An existing file is made private, or refused if it is a link or not
/// ours ([`crate::private::tighten`]).
async fn private_file(path: &Path) -> io::Result<tokio::fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .await?
        .into_std()
        .await;
    let path = path.to_owned();
    let file =
        tokio::task::spawn_blocking(move || crate::private::tighten(&file, &path).map(|()| file))
            .await
            .map_err(io::Error::other)??;
    Ok(tokio::fs::File::from_std(file))
}

async fn remove_if_present(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn remove_quietly(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "cannot delete a leftover audio file");
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const ID: &str = "00000000-0000-4000-8000-0000000000a1";

    fn id() -> AudioId {
        AudioId::new(ID).expect("id")
    }

    fn record() -> Record {
        Record {
            total_bytes: 4,
            duration_ms: 1000,
            created_ms: 1,
            owner: Some("dev-0000000000000001".to_owned()),
            state: Phase::Uploading,
        }
    }

    #[tokio::test]
    async fn round_trips_and_keeps_files_private() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Store::new(tmp.path().join("audio"));
        assert!(store.load().expect("load").clips.is_empty());
        store.save(&id(), &record()).await.expect("save");
        store.write_at(&id(), 0, b"ab").await.expect("write");

        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&tmp.path().join("audio")), 0o700);
        assert_eq!(mode(&store.path(&id(), AUDIO_EXT)), 0o600);
        assert_eq!(mode(&store.path(&id(), RECORD_EXT)), 0o600);

        let loaded = store.load().expect("load").clips;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].record, record());
        assert_eq!(loaded[0].audio_len, Some(2));
    }

    #[tokio::test]
    async fn an_existing_open_clip_is_made_private() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Store::new(tmp.path().join("audio"));
        store.load().expect("load");
        let audio = store.path(&id(), AUDIO_EXT);
        std::fs::write(&audio, b"").expect("plant");
        std::fs::set_permissions(&audio, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        store.write_at(&id(), 0, b"ab").await.expect("write");
        let mode = std::fs::metadata(&audio)
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn load_deletes_leftovers_and_sets_aside_unreadable_records() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Store::new(tmp.path().to_path_buf());
        let orphan = "00000000-0000-4000-8000-0000000000b2";
        std::fs::write(tmp.path().join(format!("{orphan}.audio")), b"x").expect("orphan");
        std::fs::write(tmp.path().join(format!("{ID}.json")), b"{not json").expect("bad");
        std::fs::write(tmp.path().join(format!("{ID}.audio")), b"kept").expect("audio");
        std::fs::write(tmp.path().join(format!("{ID}.json.tmp")), b"{}").expect("tmp");
        std::fs::write(tmp.path().join("unrelated.txt"), b"keep").expect("other");

        let loaded = store.load().expect("load");
        assert!(loaded.clips.is_empty());
        assert_eq!(loaded.reserved, [id()]);
        let mut left: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("ls")
            .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
            .collect();
        left.sort();
        let audio = format!("{ID}.audio.unreadable");
        let record = format!("{ID}.json.unreadable");
        assert_eq!(left, [audio.as_str(), record.as_str(), "unrelated.txt"]);
        // A second start leaves the set-aside files alone.
        assert!(store.load().expect("reload").clips.is_empty());
        assert_eq!(std::fs::read_dir(tmp.path()).expect("ls").count(), 3);
    }

    #[test]
    fn records_tolerate_unknown_fields() {
        let json = r#"{"totalBytes":4,"durationMs":1000,"createdMs":1,"state":{"phase":"queued","priority":2},"device":"x"}"#;
        let record: Record = serde_json::from_str(json).expect("tolerant");
        assert_eq!(record.state, Phase::Queued);
        // Written before clips had owners: everybody's, as before.
        assert_eq!(record.owner, None);
        assert!(record.owned_by("x"));
        let owned = Record {
            owner: Some("a".to_owned()),
            ..record
        };
        assert!(owned.owned_by("a"));
        assert!(!owned.owned_by("x"));
    }

    #[tokio::test]
    async fn writes_land_at_their_offset_and_cut_what_lies_past_it() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Store::new(tmp.path().to_path_buf());
        store.save(&id(), &record()).await.expect("save");
        store.write_at(&id(), 0, b"abcd").await.expect("write");
        store.write_at(&id(), 1, b"X").await.expect("overwrite");
        assert_eq!(store.read_audio(&id()).await.expect("read"), b"aX");
        // A stale file (say, a delete that failed) never prefixes a new upload.
        store.write_at(&id(), 0, b"z").await.expect("restart");
        let gap = store.write_at(&id(), 5, b"!").await.expect_err("a hole");
        assert_eq!(gap.kind(), ErrorKind::InvalidInput);
        assert_eq!(store.read_audio(&id()).await.expect("read"), b"z");
        store.remove_audio(&id()).await.expect("rm audio");
        store.remove_audio(&id()).await.expect("idempotent");
        assert_eq!(store.load().expect("load").clips[0].audio_len, None);
        store.remove_record(&id()).await.expect("rm record");
        store.remove_record(&id()).await.expect("idempotent");
        assert!(store.load().expect("load").clips.is_empty());
    }

    #[test]
    fn setting_aside_never_overwrites_and_reserves_the_id() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Store::new(tmp.path().to_path_buf());
        let earlier = tmp.path().join(format!("{ID}.json.unreadable"));
        std::fs::write(&earlier, b"first").expect("earlier");
        std::fs::write(tmp.path().join(format!("{ID}.json")), b"{second").expect("bad");

        let loaded = store.load().expect("load");
        assert!(loaded.clips.is_empty());
        assert_eq!(loaded.reserved, [id()]);
        assert_eq!(std::fs::read(&earlier).expect("kept"), b"first");
        let second = tmp.path().join(format!("{ID}.json.unreadable.2"));
        assert_eq!(std::fs::read(&second).expect("set aside"), b"{second");
        // Still reserved on the next start, while the files exist.
        assert_eq!(store.load().expect("reload").reserved, [id()]);
    }
}
