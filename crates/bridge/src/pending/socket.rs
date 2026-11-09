//! The hook socket: a unix socket the `passporter-hook` binary writes one
//! Claude Code hook payload to per connection, then closes (`docs/hooks.md`).
//!
//! - The socket is mode 600 in a directory that is mode 700 (created so if
//!   missing, refused if group- or world-accessible), and a connection
//!   from another uid is dropped (`SO_PEERCRED`).
//! - A payload is read up to [`MAX_PAYLOAD_BYTES`] (one byte more means
//!   oversized, and the payload is dropped unparsed), within
//!   [`READ_TIMEOUT`], with at most [`MAX_CONNECTIONS`] in flight; the
//!   bridge never answers, so a hook can never block Claude.
//! - A bridge holds an exclusive lock on `<socket>.lock` for as long as it
//!   listens, so two bridges never race over the socket: the second one
//!   fails to bind, and only the lock holder ever removes or replaces the
//!   socket file (a stale one left by a crash, or its own on shutdown).

use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use super::Pendings;
use super::hook::{self, MAX_PAYLOAD_BYTES};
use crate::shutdown::Shutdown;

/// How long a hook may take to send its payload.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Connections handled at once; more are closed unread.
const MAX_CONNECTIONS: usize = 16;

/// A bound hook socket. Dropping it removes the socket file.
#[derive(Debug)]
pub(crate) struct HookSocket {
    listener: UnixListener,
    path: PathBuf,
    /// The bridge's uid (the socket file's owner): the only peer accepted.
    owner: u32,
    /// Held until after the socket file is removed (fields drop after
    /// `Drop::drop`).
    _lock: File,
}

impl Drop for HookSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Binds the hook socket at `path` (see the module docs).
pub(crate) fn bind(path: &Path) -> io::Result<HookSocket> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    if !dir.exists() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let meta = fs::metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} is accessible by other users; chmod 700 it",
            dir.display()
        )));
    }
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(PathBuf::from(lock_path))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another bridge is listening on the hook socket",
            ));
        }
        Err(TryLockError::Error(error)) => return Err(error),
    }
    // We hold the lock: any socket file here was left by a bridge that died.
    match fs::symlink_metadata(path) {
        Ok(existing) if existing.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    // From here on the socket file is ours; `Drop` removes it.
    let mut socket = HookSocket {
        listener,
        path: path.to_owned(),
        owner: 0,
        _lock: lock,
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let owner = fs::metadata(path)?.uid();
    if meta.uid() != owner {
        return Err(io::Error::other(format!(
            "{} belongs to another user",
            dir.display()
        )));
    }
    socket.owner = owner;
    Ok(socket)
}

/// Accepts hook payloads until `shutdown`, recording them in `pendings`.
pub(crate) async fn serve(socket: HookSocket, pendings: Arc<Pendings>, mut shutdown: Shutdown) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = socket.listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                        tracing::warn!("hook socket busy; dropping a payload");
                        continue;
                    };
                    tasks.spawn(receive(stream, socket.owner, Arc::clone(&pendings), permit));
                }
                Err(error) => {
                    tracing::warn!(%error, "hook socket accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            () = shutdown.wait() => break,
        }
    }
}

/// Reads, checks and records one payload.
async fn receive(
    mut stream: UnixStream,
    owner: u32,
    pendings: Arc<Pendings>,
    _permit: OwnedSemaphorePermit,
) {
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == owner => {}
        Ok(cred) => {
            tracing::warn!(
                uid = cred.uid(),
                "hook connection from another user refused"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "hook peer credentials unavailable");
            return;
        }
    }
    let mut payload = Vec::new();
    let limit = u64::try_from(MAX_PAYLOAD_BYTES + 1).unwrap_or(u64::MAX);
    let read = tokio::time::timeout(
        READ_TIMEOUT,
        (&mut stream).take(limit).read_to_end(&mut payload),
    )
    .await;
    match read {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "hook payload read failed");
            return;
        }
        Err(_) => {
            tracing::warn!("hook payload not sent within {READ_TIMEOUT:?}");
            return;
        }
    }
    match hook::decode(&payload) {
        Ok(event) => pendings.record_hook(event),
        Err(error) => tracing::warn!(%error, "hook payload refused"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_socket_is_private_and_removed_on_drop() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("run").join("hooks.sock");
        let socket = bind(&path).expect("binds");
        let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("dir")), 0o700);
        let again = bind(&path).expect_err("a live socket is not replaced");
        assert_eq!(again.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists(), "the live socket is left alone");
        drop(socket);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("hooks.sock");
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
        drop(std::os::unix::net::UnixListener::bind(&path).expect("stale"));
        assert!(path.exists(), "std leaves the file behind");
        let _socket = bind(&path).expect("replaces it");
    }

    #[tokio::test]
    async fn an_open_directory_or_a_file_in_the_way_is_refused() {
        let tmp = tempfile::tempdir().expect("tmp");
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).expect("chmod");
        let error = bind(&tmp.path().join("hooks.sock")).expect_err("open dir");
        assert!(error.to_string().contains("chmod 700"), "{error}");

        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
        let file = tmp.path().join("hooks.sock");
        fs::write(&file, "").expect("file");
        let error = bind(&file).expect_err("not a socket");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(file.exists(), "a file that is not a socket is left alone");
    }
}
