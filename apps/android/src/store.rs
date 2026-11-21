//! Remembers the paired bridge's origin across launches, as one line of
//! text in the app's private data directory. It is not a secret.
//!
//! This file is also how native code finds the bridge: the device plugin's
//! Kotlin (`BridgeOrigin.saved`, in `plugins/device`) reads the same file
//! from `Context.getDataDir()` (Tauri's app data directory on Android), so
//! the webview and native sign-ins always talk to the same bridge, with or
//! without the app running. Its test pins [`FILE_NAME`].

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::origin::{BridgeOrigin, OriginError};

/// The file under the app data directory that holds the origin.
pub const FILE_NAME: &str = "bridge-origin";

/// The saved bridge origin, if any.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
}

/// Why the saved origin could not be read or written.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The file system refused.
    #[error("{action} {path}")]
    Io {
        /// What was being done.
        action: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// The file holds something that is not a bridge origin.
    #[error("{path} does not hold a bridge origin")]
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What was wrong with it.
        #[source]
        source: OriginError,
    },
}

impl Store {
    /// The store in `dir` (the app's data directory).
    #[must_use]
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    /// The saved origin, or `None` before the first pairing.
    pub fn load(&self) -> Result<Option<BridgeOrigin>, StoreError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error("reading", &self.path, source)),
        };
        BridgeOrigin::parse(&text)
            .map(Some)
            .map_err(|source| StoreError::Corrupt {
                path: self.path.clone(),
                source,
            })
    }

    /// Saves `origin`, replacing any earlier one. The write is atomic and
    /// durable: the new text reaches the disk before it replaces the old
    /// file, and the rename before this returns, so a crash or power loss
    /// leaves the old file or the new one, never half of either.
    pub fn save(&self, origin: &BridgeOrigin) -> Result<(), StoreError> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir).map_err(|source| io_error("creating", dir, source))?;
        let partial = self.path.with_extension("partial");
        write_synced(&partial, format!("{origin}\n").as_bytes())
            .map_err(|source| io_error("writing", &partial, source))?;
        fs::rename(&partial, &self.path)
            .map_err(|source| io_error("replacing", &self.path, source))?;
        File::open(dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|source| io_error("syncing", dir, source))
    }

    /// Deletes the saved origin, so the next launch opens the Connect page.
    /// Nothing saved is not an error.
    pub fn forget(&self) -> Result<(), StoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io_error("deleting", &self.path, source)),
        }
    }
}

/// Writes `bytes` to a new file at `path` and waits until they are on disk.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        action,
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(input: &str) -> BridgeOrigin {
        BridgeOrigin::parse(input).expect("valid origin")
    }

    #[test]
    fn nothing_is_saved_before_the_first_pairing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(Store::in_dir(dir.path()).load().expect("load").is_none());
    }

    #[test]
    fn saves_and_loads_the_origin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::in_dir(&dir.path().join("not-yet-created"));
        store
            .save(&origin("https://bridge.example.org:8443/agents"))
            .expect("save");
        store
            .save(&origin("https://other.example.org"))
            .expect("save");
        assert_eq!(
            store.load().expect("load"),
            Some(origin("https://other.example.org"))
        );
        let entries: Vec<_> = fs::read_dir(dir.path().join("not-yet-created"))
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(entries, [FILE_NAME], "no partial file is left behind");
    }

    #[test]
    fn forgetting_leaves_nothing_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::in_dir(dir.path());
        store.forget().expect("nothing to forget is fine");
        store
            .save(&origin("https://bridge.example.org"))
            .expect("save");
        store.forget().expect("forget");
        assert!(store.load().expect("load").is_none());
        store.forget().expect("forgetting twice is fine");
    }

    #[test]
    fn a_file_that_is_not_an_origin_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join(FILE_NAME), "http://bridge.example.org\n").expect("write");
        assert!(matches!(
            Store::in_dir(dir.path()).load(),
            Err(StoreError::Corrupt {
                source: OriginError::NotHttps,
                ..
            })
        ));
    }
}
