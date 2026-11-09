//! `passporter-hook forward`: stdin to the hook socket, best effort.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bridge::pending::hook::{MAX_PAYLOAD_BYTES, default_socket_path};

/// The most a hook run may spend on the bridge: connecting (which blocks
/// when a wedged bridge's backlog is full) and writing.
const DEADLINE: Duration = Duration::from_secs(1);

/// Forwards the payload on stdin. Every failure (no socket, no bridge, a
/// wedged bridge, an oversized payload) ends quietly: the bridge falls back
/// to the screen.
pub(crate) fn run() {
    let mut payload = Vec::new();
    let limit = u64::try_from(MAX_PAYLOAD_BYTES + 1).unwrap_or(u64::MAX);
    if io::stdin()
        .lock()
        .take(limit)
        .read_to_end(&mut payload)
        .is_err()
        || payload.len() > MAX_PAYLOAD_BYTES
    {
        return;
    }
    let Some(path) = default_socket_path() else {
        return;
    };
    // `UnixStream::connect` has no timeout: connect and write on a thread
    // and stop waiting at the deadline. Returning ends the process, and the
    // thread with it.
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        send(path, &payload);
        let _ = done.send(());
    });
    let _ = finished.recv_timeout(DEADLINE);
}

fn send(path: PathBuf, payload: &[u8]) {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(DEADLINE));
    if stream.write_all(payload).is_ok() {
        let _ = stream.shutdown(Shutdown::Write);
    }
}
