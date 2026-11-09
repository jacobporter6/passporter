//! Helpers shared by unit tests.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Sets `path`'s permission bits (`0o500` makes a dir unwritable).
pub(crate) fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Waits until `done` holds, in real time (work on the blocking pool does
/// not advance a paused tokio clock), letting other tasks run meanwhile.
pub(crate) async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "{what}: timed out");
        std::thread::sleep(Duration::from_millis(2));
        tokio::task::yield_now().await;
    }
}
