//! Paired devices, persisted as `<state dir>/devices.json` (file `0600`,
//! directory `0700`, tightened at startup if it is wider). Only public keys are stored: nothing in the file is a
//! credential. Sessions, challenges and pairing codes live in memory, so a
//! restart signs every device out (each signs back in with its key).
//!
//! Saves take turns ([`DeviceStore::save_in_turn`]): each writes the list as
//! it is when its turn comes, on the blocking pool, so concurrent changes
//! all reach the disk and the async threads never wait on an fsync.

use std::fs::{self, DirBuilder, File};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use p256::ecdsa::VerifyingKey;
use protocol::{DeviceId, DeviceName, PublicKey, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::secret::unhex;
use crate::private::write_atomically;

/// The file name inside the state dir.
pub const DEVICES_FILE: &str = "devices.json";

/// Larger device files are refused rather than read.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// One paired device, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceRecord {
    /// The device's id.
    pub id: DeviceId,
    /// Its label.
    pub name: DeviceName,
    /// Its P-256 public key.
    pub public_key: PublicKey,
    /// When it paired.
    pub paired_at: Timestamp,
}

impl DeviceRecord {
    /// The key that verifies this device's signatures, or `None` if the
    /// stored key is not a point on P-256.
    pub(crate) fn verifying_key(&self) -> Option<VerifyingKey> {
        verifying_key(&self.public_key)
    }
}

/// Parses a SEC1 public key.
pub(crate) fn verifying_key(key: &PublicKey) -> Option<VerifyingKey> {
    VerifyingKey::from_sec1_bytes(&unhex(key.as_str())?).ok()
}

/// The file's shape.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DevicesFile {
    devices: Vec<DeviceRecord>,
}

/// Why the device file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// File system trouble.
    #[error("{action} {}: {source}", path.display())]
    Io {
        /// What we were doing.
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The file is not a valid device list.
    #[error("{} is not a valid device list: {message}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What was wrong.
        message: String,
    },
}

/// The device file.
#[derive(Debug)]
pub(crate) struct DeviceStore {
    path: PathBuf,
    /// Held from a save's snapshot until its write is done, so saves run
    /// one at a time, in order (they share a temp file, too).
    turn: Arc<Mutex<()>>,
}

impl DeviceStore {
    /// The store in `state_dir`, created `0700` if missing. An existing
    /// dir that group or other users can reach (it also holds the admin
    /// socket) is tightened to `0700`; one this user cannot `chmod` (not
    /// its owner) is refused.
    pub(crate) fn open(state_dir: &Path) -> Result<Self, StoreError> {
        let io_err = |action| {
            move |source| StoreError::Io {
                action,
                path: state_dir.to_path_buf(),
                source,
            }
        };
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state_dir)
            .map_err(io_err("creating"))?;
        let mode = fs::metadata(state_dir)
            .map_err(io_err("inspecting"))?
            .permissions()
            .mode()
            & 0o7777;
        if mode & 0o077 != 0 {
            fs::set_permissions(state_dir, fs::Permissions::from_mode(0o700))
                .map_err(io_err("restricting to 0700 (other users can reach it)"))?;
            tracing::warn!(
                dir = %state_dir.display(),
                was = format_args!("{mode:04o}"),
                "state dir was accessible by other users; restricted it to 0700"
            );
        }
        Ok(Self {
            path: state_dir.join(DEVICES_FILE),
            turn: Arc::new(Mutex::new(())),
        })
    }

    /// Every stored device; none if the file does not exist yet. A record
    /// whose key is not on the curve makes the whole file invalid.
    pub(crate) fn load(&self) -> Result<Vec<DeviceRecord>, StoreError> {
        let io_err = |source| StoreError::Io {
            action: "reading",
            path: self.path.clone(),
            source,
        };
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_err(error)),
        };
        let mut text = String::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(io_err)?;
        let invalid = |message: String| StoreError::Invalid {
            path: self.path.clone(),
            message,
        };
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid(format!("larger than {MAX_FILE_BYTES} bytes")));
        }
        let parsed: DevicesFile =
            serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?;
        if let Some(bad) = parsed.devices.iter().find(|d| d.verifying_key().is_none()) {
            return Err(invalid(format!("{} has an invalid public key", bad.id)));
        }
        Ok(parsed.devices)
    }

    /// Waits for this save's turn, then writes the list `snapshot` returns
    /// on the blocking pool. The snapshot is taken in turn, so it holds
    /// every change made before any earlier save's snapshot: whatever order
    /// changes and saves interleave in, the last write has them all. A
    /// caller that stops waiting leaves an already started write to finish,
    /// still in turn.
    pub(crate) async fn save_in_turn(
        self: &Arc<Self>,
        snapshot: impl FnOnce() -> Vec<DeviceRecord>,
    ) -> Result<(), StoreError> {
        let turn = Arc::clone(&self.turn).lock_owned().await;
        let devices = snapshot();
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            store.save(&devices)
        })
        .await
        .map_err(|error| self.interrupted(&error))?
    }

    /// The error for a save whose task died (panicked or was aborted).
    pub(crate) fn interrupted(&self, error: &tokio::task::JoinError) -> StoreError {
        StoreError::Io {
            action: "saving",
            path: self.path.clone(),
            source: io::Error::other(error.to_string()),
        }
    }

    /// Holds the save turn: saves wait until the guard drops.
    #[cfg(test)]
    pub(crate) async fn hold_turn(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.turn).lock_owned().await
    }

    /// Replaces the file with `devices`, atomically and durably (write,
    /// sync, rename, sync the directory). Blocking; callers other than
    /// tests go through [`Self::save_in_turn`].
    fn save(&self, devices: &[DeviceRecord]) -> Result<(), StoreError> {
        let io_err = |action, path: &Path| {
            let path = path.to_path_buf();
            move |source| StoreError::Io {
                action,
                path,
                source,
            }
        };
        let json = serde_json::to_vec_pretty(&DevicesFile {
            devices: devices.to_vec(),
        })
        .map_err(|e| StoreError::Invalid {
            path: self.path.clone(),
            message: e.to_string(),
        })?;
        // Syncs the directory too, or a crash could bring back the old list
        // (and with it a revoked device).
        write_atomically(&self.path, &json).map_err(io_err("writing", &self.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The P-256 generator point: a valid public key.
    pub(crate) const KEY: &str = "046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296\
                                  4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5";

    fn record(id: &str) -> DeviceRecord {
        DeviceRecord {
            id: DeviceId::new(id).expect("id"),
            name: DeviceName::new("Pixel").expect("name"),
            public_key: PublicKey::new(KEY).expect("key"),
            paired_at: Timestamp::from_unix_millis(1),
        }
    }

    #[test]
    fn round_trips_privately() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("state");
        let store = DeviceStore::open(&dir).expect("open");
        assert_eq!(store.load().expect("empty"), Vec::new());
        let devices = vec![
            record("dev-0000000000000001"),
            record("dev-0000000000000002"),
        ];
        store.save(&devices).expect("save");
        assert_eq!(store.load().expect("load"), devices);
        let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(DEVICES_FILE)), 0o600);
    }

    #[test]
    fn a_leftover_world_readable_tmp_file_is_not_reused() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("state");
        let store = DeviceStore::open(&dir).expect("open");
        let leftover = dir.join(format!("{DEVICES_FILE}.tmp"));
        fs::write(&leftover, "stale").expect("plant");
        fs::set_permissions(&leftover, fs::Permissions::from_mode(0o644)).expect("chmod");
        store.save(&[record("dev-0000000000000001")]).expect("save");
        let mode = fs::metadata(dir.join(DEVICES_FILE))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn tightens_an_open_state_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("state");
        fs::create_dir(&dir).expect("mkdir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        DeviceStore::open(&dir).expect("open");
        let mode = fs::metadata(&dir).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn refuses_a_key_off_the_curve() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = DeviceStore::open(tmp.path()).expect("open");
        let mut bad = record("dev-0000000000000001");
        bad.public_key = PublicKey::new(format!("04{}", "00".repeat(64))).expect("shape");
        store.save(&[bad]).expect("save");
        assert!(matches!(store.load(), Err(StoreError::Invalid { .. })));
    }

    #[test]
    fn refuses_junk() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = DeviceStore::open(tmp.path()).expect("open");
        fs::write(tmp.path().join(DEVICES_FILE), "{\"devices\":[],\"x\":1}").expect("write");
        assert!(matches!(store.load(), Err(StoreError::Invalid { .. })));
    }
}
