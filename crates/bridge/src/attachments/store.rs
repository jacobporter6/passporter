//! The attachments directory on disk: one image (`<id>.jpg` or `<id>.png`,
//! the re-encoded pixels) and one `<id>.json` [`Record`] per upload. The
//! directory is `0700` and every file `0600`.
//!
//! The image is written first (a fresh temp file, synced, then renamed)
//! and the record second, so a record always has its image. An image
//! without a record, or a temp file, is a leftover from a crash and is
//! deleted at startup.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use protocol::{AttachmentId, AttachmentReceipt, ImageType};
use serde::{Deserialize, Serialize};

const RECORD_EXT: &str = "json";
const TMP_SUFFIX: &str = ".tmp";

/// What is kept about an upload: never its pixels or the upload's
/// metadata, only what the bridge itself produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Record {
    /// The device that uploaded it, the only one that may use it.
    pub(crate) owner: String,
    /// The stored format.
    pub(crate) media_type: ImageType,
    /// The stored file's size in bytes.
    pub(crate) bytes: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// When it was stored (Unix milliseconds).
    pub(crate) created_ms: u64,
}

impl Record {
    /// The upload's answer.
    pub(crate) fn receipt(&self, id: &AttachmentId) -> AttachmentReceipt {
        AttachmentReceipt {
            attachment_id: id.clone(),
            media_type: self.media_type,
            bytes: self.bytes,
            width: self.width,
            height: self.height,
        }
    }
}

/// The attachments directory. Blocking: call it off the async threads.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    dir: PathBuf,
}

impl Store {
    /// Creates `dir` (`0700`) if needed and resolves it to an absolute
    /// path, the form agents are given.
    pub(crate) fn open(dir: &Path) -> io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        Ok(Self {
            dir: std::fs::canonicalize(dir)?,
        })
    }

    /// Where `id`'s image lives. `AttachmentId` is a validated UUID, so
    /// this never leaves the directory.
    pub(crate) fn image_path(&self, id: &AttachmentId, kind: ImageType) -> PathBuf {
        self.dir.join(format!("{id}.{}", kind.extension()))
    }

    fn record_path(&self, id: &AttachmentId) -> PathBuf {
        self.dir.join(format!("{id}.{RECORD_EXT}"))
    }

    /// Every stored upload. Deletes leftovers (temp files, images without
    /// a record) and records whose image is gone or that do not parse.
    pub(crate) fn load(&self) -> io::Result<Vec<(AttachmentId, Record)>> {
        let mut records = Vec::new();
        let mut images = Vec::new();
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
            let Ok(id) = AttachmentId::new(stem) else {
                continue;
            };
            if ext == RECORD_EXT {
                records.push(id);
            } else if [ImageType::Jpeg, ImageType::Png]
                .iter()
                .any(|kind| kind.extension() == ext)
            {
                images.push(path);
            }
        }
        let mut loaded = Vec::new();
        for id in records {
            let path = self.record_path(&id);
            let record = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok());
            match record {
                Some(record) if self.image_path(&id, record.media_type).is_file() => {
                    loaded.push((id, record));
                }
                _ => {
                    tracing::warn!(path = %path.display(), "dropping an attachment record without a readable image");
                    remove_quietly(&path);
                }
            }
        }
        for image in images {
            let kept = loaded
                .iter()
                .any(|(id, record)| self.image_path(id, record.media_type) == image);
            if !kept {
                remove_quietly(&image);
            }
        }
        Ok(loaded)
    }

    /// Stores `image` as `id`'s file and then its record, each `0600` and
    /// synced.
    pub(crate) fn save(&self, id: &AttachmentId, record: &Record, image: &[u8]) -> io::Result<()> {
        let path = self.image_path(id, record.media_type);
        let mut tmp_name = path.clone().into_os_string();
        tmp_name.push(TMP_SUFFIX);
        let tmp = PathBuf::from(tmp_name);
        remove_if_present(&tmp)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(image)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        let json = serde_json::to_vec(record).map_err(io::Error::other)?;
        crate::private::write_atomically(&self.record_path(id), &json)
    }

    /// Deletes `id`'s image, then its record.
    pub(crate) fn remove(&self, id: &AttachmentId, kind: ImageType) -> io::Result<()> {
        remove_if_present(&self.image_path(id, kind))?;
        remove_if_present(&self.record_path(id))
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn remove_quietly(path: &Path) {
    if let Err(error) = remove_if_present(path) {
        tracing::warn!(path = %path.display(), %error, "cannot delete a leftover attachment file");
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const ID: &str = "00000000-0000-4000-8000-0000000000a1";

    fn id() -> AttachmentId {
        AttachmentId::new(ID).expect("id")
    }

    fn record() -> Record {
        Record {
            owner: "dev-0000000000000001".to_owned(),
            media_type: ImageType::Jpeg,
            bytes: 3,
            width: 1,
            height: 1,
            created_ms: 5,
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("meta").permissions().mode() & 0o777
    }

    #[test]
    fn saves_privately_and_loads_again() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Store::open(&tmp.path().join("attachments")).expect("open");
        assert_eq!(store.load().expect("load").len(), 0);
        store.save(&id(), &record(), b"jpg").expect("save");
        let image = store.image_path(&id(), ImageType::Jpeg);
        assert!(image.is_absolute());
        assert_eq!(mode(&tmp.path().join("attachments")), 0o700);
        assert_eq!(mode(&image), 0o600);
        assert_eq!(mode(&store.record_path(&id())), 0o600);
        assert_eq!(std::fs::read(&image).expect("read"), b"jpg");
        assert_eq!(store.load().expect("load"), [(id(), record())]);
        store.remove(&id(), ImageType::Jpeg).expect("remove");
        store.remove(&id(), ImageType::Jpeg).expect("idempotent");
        assert_eq!(store.load().expect("load").len(), 0);
    }

    #[test]
    fn load_deletes_leftovers() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = Store::open(tmp.path()).expect("open");
        let orphan = "00000000-0000-4000-8000-0000000000b2";
        let lost = "00000000-0000-4000-8000-0000000000c3";
        std::fs::write(tmp.path().join(format!("{orphan}.jpg")), b"x").expect("orphan");
        std::fs::write(tmp.path().join(format!("{ID}.jpg.tmp")), b"x").expect("tmp");
        std::fs::write(
            tmp.path().join(format!("{lost}.json")),
            serde_json::to_vec(&record()).expect("json"),
        )
        .expect("record without image");
        std::fs::write(tmp.path().join(format!("{ID}.json")), b"{broken").expect("bad");
        std::fs::write(tmp.path().join("notes.txt"), b"keep").expect("other");
        assert_eq!(store.load().expect("load").len(), 0);
        let left: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("ls")
            .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
            .collect();
        assert_eq!(left, ["notes.txt"]);
    }
}
