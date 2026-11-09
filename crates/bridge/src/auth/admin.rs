//! The admin socket: how `passporter pair` and `passporter devices` reach
//! the running bridge.
//!
//! A Unix socket at `<state dir>/admin.sock` (socket `0600` from the
//! moment it appears, in the `0700` state dir, so only the bridge's user
//! can connect; a peer of another uid is refused too, `SO_PEERCRED`).
//! Each connection carries one JSON line ([`AdminRequest`]) and gets one
//! JSON line back ([`AdminResponse`]).

use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::{DeviceId, DeviceName, PairingCode};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use super::{AuthError, DeviceAuth, DeviceInfo};
use crate::shutdown::Shutdown;

/// The socket's file name inside the state dir.
pub const ADMIN_SOCKET: &str = "admin.sock";

/// Largest request line read.
const MAX_LINE: u64 = 4 * 1024;

/// Largest response line read by the client (a full device list).
const MAX_RESPONSE: u64 = 1024 * 1024;

/// How long one admin exchange may take.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);

/// What the CLI asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminRequest {
    /// Issue a pairing code.
    Pair {
        /// A label for the device; wins over the name the device reports.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<DeviceName>,
    },
    /// List paired devices.
    Devices {},
    /// Revoke a device.
    Revoke {
        /// The device.
        id: DeviceId,
    },
}

/// What the bridge answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum AdminResponse {
    /// A fresh pairing code.
    Paired {
        /// The code.
        code: PairingCode,
        /// Seconds it stays valid.
        expires_in_secs: u64,
    },
    /// The paired devices.
    Devices {
        /// Each device.
        devices: Vec<DeviceInfo>,
    },
    /// The device is revoked.
    Revoked {
        /// Sessions that ended.
        sessions: usize,
    },
    /// The request failed.
    Error {
        /// Why, for the operator.
        message: String,
    },
}

/// Why the admin socket could not be served or reached.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    /// Socket trouble.
    #[error("{action} {}: {source}", path.display())]
    Io {
        /// What we were doing.
        action: &'static str,
        /// The socket.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// Something other than a socket is in the way.
    #[error("{} exists and is not a socket", .0.display())]
    NotASocket(PathBuf),
    /// The bridge answered something unreadable.
    #[error("unexpected answer from the bridge: {0}")]
    Protocol(String),
    /// The bridge took too long.
    #[error("the bridge did not answer in time")]
    Timeout,
}

/// Binds `path` (replacing a stale socket) and serves admin requests until
/// `shutdown`, then removes the socket.
pub fn spawn(
    auth: Arc<DeviceAuth>,
    path: &Path,
    mut shutdown: Shutdown,
) -> Result<JoinHandle<()>, AdminError> {
    let io_err = |action| {
        let path = path.to_path_buf();
        move |source| AdminError::Io {
            action,
            path,
            source,
        }
    };
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(path).map_err(io_err("removing stale"))?;
        }
        Ok(_) => return Err(AdminError::NotASocket(path.to_path_buf())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_err("inspecting")(error)),
    }
    let listener = bind_private(path).map_err(io_err("binding"))?;
    let owner = std::fs::metadata(path).map_err(io_err("inspecting"))?.uid();
    let path = path.to_path_buf();
    Ok(tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let auth = Arc::clone(&auth);
                        tokio::spawn(async move {
                            if tokio::time::timeout(EXCHANGE_TIMEOUT, serve_one(&auth, stream, owner)).await.is_err() {
                                tracing::debug!("admin connection timed out");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "admin socket accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                () = shutdown.wait() => break,
            }
        }
        let _ = std::fs::remove_file(&path);
    }))
}

/// Binds a listener at `path` that is never reachable by anyone but its
/// owner: it is bound inside a fresh `0700` directory next to `path`,
/// restricted to `0600` there, and only then renamed into place. So there
/// is no moment when the socket sits at `path` with the umask's mode, and
/// no process-wide umask change.
fn bind_private(path: &Path) -> io::Result<UnixListener> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let mut dir_name = std::ffi::OsString::from(".");
    dir_name.push(name);
    dir_name.push(format!(".bind-{}", u64::from_le_bytes(nonce)));
    let dir = path.with_file_name(dir_name);
    // `create` fails rather than reuse anything already at `dir`.
    DirBuilder::new().mode(0o700).create(&dir)?;
    let staged = dir.join(name);
    let bound = UnixListener::bind(&staged).and_then(|listener| {
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&staged, path)?;
        Ok(listener)
    });
    let _ = std::fs::remove_file(&staged);
    let cleaned = std::fs::remove_dir(&dir);
    let listener = bound?;
    cleaned?;
    Ok(listener)
}

/// Answers one connection, if its peer is `owner` (the bridge's user):
/// the socket's mode already says so, and this holds even if an operator
/// loosens the state dir.
async fn serve_one(auth: &DeviceAuth, stream: UnixStream, owner: u32) {
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == owner => {}
        Ok(cred) => {
            tracing::warn!(
                uid = cred.uid(),
                "admin connection from another user refused"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "admin peer credentials unavailable");
            return;
        }
    }
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    if BufReader::new(read.take(MAX_LINE))
        .read_line(&mut line)
        .await
        .is_err()
    {
        return;
    }
    let response = match serde_json::from_str::<AdminRequest>(&line) {
        Ok(request) => handle(auth, request).await,
        Err(error) => AdminResponse::Error {
            message: format!("bad request: {error}"),
        },
    };
    if let Ok(mut json) = serde_json::to_vec(&response) {
        json.push(b'\n');
        let _ = write.write_all(&json).await;
        let _ = write.shutdown().await;
    }
}

/// Answers one request.
pub async fn handle(auth: &DeviceAuth, request: AdminRequest) -> AdminResponse {
    let failed = |error: AuthError| AdminResponse::Error {
        message: error.to_string(),
    };
    match request {
        AdminRequest::Pair { name } => match auth.start_pairing(name) {
            Ok(issued) => {
                tracing::info!("pairing code issued");
                AdminResponse::Paired {
                    code: issued.code,
                    expires_in_secs: issued.valid_for.as_secs(),
                }
            }
            Err(error) => failed(error),
        },
        AdminRequest::Devices {} => AdminResponse::Devices {
            devices: auth.devices(),
        },
        AdminRequest::Revoke { id } => match auth.revoke(&id).await {
            Ok(sessions) => AdminResponse::Revoked { sessions },
            Err(error) => failed(error),
        },
    }
}

/// Sends `request` to the bridge listening on `path`.
pub async fn send(path: &Path, request: &AdminRequest) -> Result<AdminResponse, AdminError> {
    let exchange = async {
        let io_err = |action| {
            let path = path.to_path_buf();
            move |source| AdminError::Io {
                action,
                path,
                source,
            }
        };
        let stream = UnixStream::connect(path)
            .await
            .map_err(io_err("connecting to the bridge at"))?;
        let (read, mut write) = stream.into_split();
        let mut json =
            serde_json::to_vec(request).map_err(|e| AdminError::Protocol(e.to_string()))?;
        json.push(b'\n');
        write.write_all(&json).await.map_err(io_err("writing to"))?;
        let mut line = String::new();
        BufReader::new(read.take(MAX_RESPONSE))
            .read_line(&mut line)
            .await
            .map_err(io_err("reading from"))?;
        serde_json::from_str(&line).map_err(|e| AdminError::Protocol(e.to_string()))
    };
    tokio::time::timeout(EXCHANGE_TIMEOUT, exchange)
        .await
        .map_err(|_| AdminError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthSettings;
    use crate::shutdown;

    #[tokio::test]
    async fn pairs_lists_and_revokes_over_the_socket() {
        let tmp = tempfile::tempdir().expect("tmp");
        let auth =
            Arc::new(DeviceAuth::open(tmp.path(), AuthSettings::default(), None).expect("auth"));
        let path = tmp.path().join(ADMIN_SOCKET);
        // A stale socket from a crashed run is replaced.
        drop(std::os::unix::net::UnixListener::bind(&path).expect("stale"));
        let (trigger, shutdown) = shutdown::channel();
        let task = spawn(Arc::clone(&auth), &path, shutdown).expect("spawn");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let left: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("ls")
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.contains(".bind-"))
            .collect();
        assert!(left.is_empty(), "staging dir left behind: {left:?}");

        let paired = send(&path, &AdminRequest::Pair { name: None })
            .await
            .expect("pair");
        assert!(
            matches!(
                paired,
                AdminResponse::Paired {
                    expires_in_secs: 300,
                    ..
                }
            ),
            "{paired:?}"
        );
        let listed = send(&path, &AdminRequest::Devices {}).await.expect("list");
        assert!(matches!(listed, AdminResponse::Devices { ref devices } if devices.is_empty()));
        let id = DeviceId::new("dev-0000000000000001").expect("id");
        let revoked = send(&path, &AdminRequest::Revoke { id })
            .await
            .expect("revoke");
        assert!(
            matches!(revoked, AdminResponse::Error { ref message } if message.contains("no paired device")),
            "{revoked:?}"
        );

        trigger.trigger();
        task.await.expect("stops");
        assert!(!path.exists(), "socket removed on shutdown");
    }

    #[tokio::test]
    async fn refuses_a_peer_of_another_user() {
        let tmp = tempfile::tempdir().expect("tmp");
        let auth =
            Arc::new(DeviceAuth::open(tmp.path(), AuthSettings::default(), None).expect("auth"));
        let me = std::fs::metadata(tmp.path()).expect("meta").uid();
        for (owner, answered) in [(me, true), (me.wrapping_add(1), false)] {
            let (mut client, bridge_end) = UnixStream::pair().expect("pair");
            let auth = Arc::clone(&auth);
            let served = tokio::spawn(async move { serve_one(&auth, bridge_end, owner).await });
            client
                .write_all(b"{\"op\":\"pair\"}\n")
                .await
                .expect("write");
            let mut answer = String::new();
            // A refused peer's unread request makes the close a reset.
            if let Err(error) = client.read_to_string(&mut answer).await {
                assert!(!answered, "owner {owner}: {error}");
            }
            served.await.expect("served");
            assert_eq!(
                answer.contains("\"paired\""),
                answered,
                "owner {owner}: {answer:?}"
            );
            if !answered {
                assert!(answer.is_empty(), "{answer:?}");
            }
        }
    }

    #[tokio::test]
    async fn refuses_to_replace_a_regular_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let auth =
            Arc::new(DeviceAuth::open(tmp.path(), AuthSettings::default(), None).expect("auth"));
        let path = tmp.path().join(ADMIN_SOCKET);
        std::fs::write(&path, "x").expect("write");
        let (_trigger, shutdown) = shutdown::channel();
        assert!(matches!(
            spawn(auth, &path, shutdown),
            Err(AdminError::NotASocket(_))
        ));
    }

    #[test]
    fn requests_are_strict() {
        assert!(serde_json::from_str::<AdminRequest>(r#"{"op":"devices"}"#).is_ok());
        assert!(serde_json::from_str::<AdminRequest>(r#"{"op":"devices","x":1}"#).is_err());
        assert!(serde_json::from_str::<AdminRequest>(r#"{"op":"revoke","id":"nope"}"#).is_err());
    }
}
