//! A run's files in `<state dir>/runbooks/`, named after the request's
//! idempotency key: `<key>.spec.json` (what runs, written once by the
//! bridge), `<key>.status.json` (claimed by the runner, or by the bridge
//! giving up; see [`super::runner`]) and `<key>.audited` (the audit log
//! has the run's `finish` line, so a bridge that missed the end writes it
//! exactly once). They outlive the bridge, so a
//! restarted bridge finds runs still going and never runs a key twice.
//! Files of finished runs are pruned after [`KEEP_FINISHED`].

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use protocol::IdempotencyKey;

use super::runner::{RunSpec, RunnerStatus};

/// How long the files of a finished run are kept: replays of its key are
/// recognised for this long, even across restarts.
const KEEP_FINISHED: Duration = Duration::from_hours(7 * 24);

const SPEC_SUFFIX: &str = ".spec.json";
const STATUS_SUFFIX: &str = ".status.json";
const AUDITED_SUFFIX: &str = ".audited";

/// One run's spec and status files.
#[derive(Debug, Clone)]
pub(crate) struct RunFiles {
    dir: PathBuf,
    /// The idempotency key the files are named after.
    key: String,
    pub(crate) spec: PathBuf,
    pub(crate) status: PathBuf,
    audited: PathBuf,
}

impl RunFiles {
    /// The files of the run for `key` (lowercase hex and dashes: always a
    /// safe file name).
    pub(crate) fn new(dir: &Path, key: &IdempotencyKey) -> Self {
        Self::named(dir, key.as_str())
    }

    fn named(dir: &Path, stem: &str) -> Self {
        Self {
            dir: dir.to_path_buf(),
            key: stem.to_owned(),
            spec: dir.join(format!("{stem}{SPEC_SUFFIX}")),
            status: dir.join(format!("{stem}{STATUS_SUFFIX}")),
            audited: dir.join(format!("{stem}{AUDITED_SUFFIX}")),
        }
    }

    /// The idempotency key of the run.
    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    /// The spec, if this key ever started a run.
    pub(crate) async fn read_spec(&self) -> Option<RunSpec> {
        serde_json::from_slice(&tokio::fs::read(&self.spec).await.ok()?).ok()
    }

    /// The status, once someone claimed the run.
    pub(crate) async fn status(&self) -> Option<RunnerStatus> {
        RunnerStatus::parse(&tokio::fs::read(&self.status).await.ok()?)
    }

    /// Writes `spec` (`0600`, in a `0700` dir), never over an existing one:
    /// `AlreadyExists` means this key started a run before. Prunes old
    /// finished runs on the way.
    pub(crate) async fn write_spec(&self, spec: &RunSpec) -> io::Result<()> {
        let dir = self.dir.clone();
        let path = self.spec.clone();
        let json = serde_json::to_vec(spec).map_err(io::Error::other)?;
        tokio::task::spawn_blocking(move || {
            DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
            prune(&dir, KEEP_FINISHED);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(&json)?;
            file.sync_all()
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Claims the run for the bridge with `status` (it gave up): true if
    /// the runner had not claimed it yet, so it never will.
    pub(crate) async fn give_up(&self, status: RunnerStatus) -> bool {
        let path = self.status.clone();
        tokio::task::spawn_blocking(move || status.claim(&path))
            .await
            .map_err(io::Error::other)
            .and_then(|claimed| claimed)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot claim a runbook status file");
                false
            })
    }

    /// Records how a run ended over the runner's `running` (it stopped
    /// without saying).
    pub(crate) async fn overwrite(&self, status: RunnerStatus) {
        let path = self.status.clone();
        let written = tokio::task::spawn_blocking(move || status.write(&path))
            .await
            .map_err(io::Error::other)
            .and_then(|written| written);
        if let Err(error) = written {
            tracing::warn!(%error, "cannot record how a runbook run ended");
        }
    }

    /// Whether the run's `finish` line is in the audit log.
    pub(crate) async fn is_audited(&self) -> bool {
        tokio::fs::try_exists(&self.audited).await.unwrap_or(false)
    }

    /// Notes that the run's `finish` line is in the audit log.
    pub(crate) async fn mark_audited(&self) {
        if let Err(error) = tokio::fs::write(&self.audited, b"").await {
            tracing::warn!(%error, "cannot note a runbook run as audited");
        }
    }

    /// Whether the runner with process id `runner` is alive and running
    /// this spec (its command line names the spec file, so a recycled pid
    /// never counts).
    pub(crate) fn runner_alive(&self, runner: u32) -> bool {
        let Ok(cmdline) = fs::read(format!("/proc/{runner}/cmdline")) else {
            return false;
        };
        let spec = self.spec.as_os_str().as_encoded_bytes();
        cmdline.split(|b| *b == 0).any(|arg| arg == spec)
    }
}

/// Every run with files in `dir`, oldest first by start time.
pub(crate) async fn all(dir: &Path) -> Vec<(RunFiles, RunSpec)> {
    let mut runs = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return runs;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(SPEC_SUFFIX)) else {
            continue;
        };
        let files = RunFiles::named(dir, stem);
        if let Some(spec) = files.read_spec().await {
            runs.push((files, spec));
        }
    }
    runs.sort_by_key(|(_, spec)| spec.started_at);
    runs
}

/// Removes the files of runs that ended (or never started) more than
/// `keep` ago. A run still claimed `running` is kept whatever its age.
fn prune(dir: &Path, keep: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(SPEC_SUFFIX)) else {
            continue;
        };
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|at| now.duration_since(at).unwrap_or_default() > keep);
        if !old {
            continue;
        }
        let files = RunFiles::named(dir, stem);
        let running = fs::read(&files.status)
            .ok()
            .and_then(|bytes| RunnerStatus::parse(&bytes))
            .is_some_and(|status| status.ending().is_none());
        if !running {
            let _ = fs::remove_file(&files.spec);
            let _ = fs::remove_file(&files.status);
            let _ = fs::remove_file(&files.audited);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use protocol::{Origin, Timestamp};

    use super::*;

    fn spec(files: &RunFiles, started_at: u64) -> RunSpec {
        RunSpec {
            runbook: "R".into(),
            cwd: PathBuf::from("/"),
            argv: vec!["true".into()],
            timeout_secs: 1,
            status: files.status.clone(),
            started_at: Timestamp::from_unix_millis(started_at),
            pane: "w1:p1".into(),
            device: "dev".into(),
            origin: Origin::App,
        }
    }

    fn key(n: u8) -> IdempotencyKey {
        IdempotencyKey::new(format!("00000000-0000-4000-8000-0000000000{n:02x}")).expect("key")
    }

    #[tokio::test]
    async fn a_spec_is_written_once_and_found_again() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("runbooks");
        let first = RunFiles::new(&dir, &key(1));
        first.write_spec(&spec(&first, 5)).await.expect("write");
        let again = first.write_spec(&spec(&first, 6)).await;
        assert_eq!(
            again.map_err(|e| e.kind()),
            Err(io::ErrorKind::AlreadyExists)
        );
        assert_eq!(first.read_spec().await, Some(spec(&first, 5)));
        let second = RunFiles::new(&dir, &key(2));
        second.write_spec(&spec(&second, 3)).await.expect("write");
        let found: Vec<u64> = all(&dir)
            .await
            .iter()
            .map(|(_, s)| s.started_at.unix_millis())
            .collect();
        assert_eq!(found, [3, 5], "oldest first");
        let mode = fs::metadata(&first.spec)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[tokio::test]
    async fn pruning_keeps_running_and_recent_runs() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().to_path_buf();
        let done = RunFiles::new(&dir, &key(1));
        done.write_spec(&spec(&done, 1)).await.expect("write");
        RunnerStatus::Exited { code: Some(0) }
            .write(&done.status)
            .expect("status");
        let running = RunFiles::new(&dir, &key(2));
        running.write_spec(&spec(&running, 2)).await.expect("write");
        assert!(
            RunnerStatus::Running {
                runner: 1,
                pid: None
            }
            .claim(&running.status)
            .expect("claim")
        );
        prune(&dir, Duration::from_secs(3600));
        assert!(done.spec.exists(), "recent: kept");
        prune(&dir, Duration::ZERO);
        assert!(
            !done.spec.exists() && !done.status.exists(),
            "old and done: gone"
        );
        assert!(running.spec.exists(), "still running: kept");
    }

    #[tokio::test]
    async fn the_bridge_gives_up_only_before_the_runner_claims() {
        let tmp = tempfile::tempdir().expect("tmp");
        let files = RunFiles::new(tmp.path(), &key(1));
        let failed = || RunnerStatus::Failed {
            message: "gave up".into(),
        };
        assert!(files.give_up(failed()).await);
        assert!(!files.give_up(failed()).await, "claimed already");
    }

    #[test]
    fn a_runner_is_alive_only_while_running_this_spec() {
        let files = RunFiles::new(Path::new("/nowhere"), &key(1));
        // This test process is alive but runs no spec; pid 0 is no process.
        assert!(!files.runner_alive(std::process::id()));
        assert!(!files.runner_alive(0));
    }
}
