//! The runner: `passporter-bridge runbook <spec>`, the one line the bridge
//! types into a runbook's herdr tab.
//!
//! The bridge writes a [`RunSpec`] (the runbook's `cwd`, `argv` and
//! timeout, all from the config) into a `0600` file in its state dir and
//! types ` '<this binary>' runbook '<spec file>'` into the tab's shell:
//! two bridge-made paths, shell-quoted, and nothing else. The runner then
//! executes `argv` **directly** (no `sh -c`, so no globbing, expansion or
//! pipes), its output going straight to the pane's terminal, and reports
//! to the bridge through a status file next to the spec ([`RunnerStatus`]).
//! After `timeout_secs` it kills the program (`SIGKILL`; programs it
//! started in turn are not followed).
//!
//! **At most once per spec.** Before anything runs, the runner *creates*
//! the status file (`O_EXCL`) with `running` and its own pid. If the file
//! exists, the run was started already, or the bridge gave up waiting
//! for it (the bridge claims the same file with `failed`), and the runner
//! does nothing. So the bridge may type the launch line again when a
//! shell seems to have dropped it, and a retry after a bridge restart
//! never runs twice.
//!
//! The run lives in the user's shell, outside the bridge's service
//! sandbox, so a runbook may restart the bridge itself (e.g. `deploy/
//! bridge/setup.sh`): the run carries on, its tab keeps the output, and
//! the restarted bridge picks the run up again from its files.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::{Duration, Instant};

use protocol::{Origin, RunState, Timestamp, command_line};
use serde::{Deserialize, Serialize};

/// Largest spec file the runner reads.
const MAX_SPEC_BYTES: u64 = 512 * 1024;

/// How often the runner checks on the program.
const POLL: Duration = Duration::from_millis(100);

/// Exit code for a program that could not be started (as shells use).
const NOT_STARTED: u8 = 127;
/// Exit code for a program killed at its timeout (as `timeout(1)` uses).
const TIMED_OUT: u8 = 124;

/// What to run, and the run's record: written by the bridge before it
/// types the launch line, read by the runner, and by a restarted bridge
/// to pick the run up again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RunSpec {
    /// The runbook's name, for the pane's header and footer lines.
    pub(crate) runbook: String,
    /// Where it runs.
    pub(crate) cwd: PathBuf,
    /// The program and its arguments.
    pub(crate) argv: Vec<String>,
    /// When to kill it.
    pub(crate) timeout_secs: u64,
    /// Where to report.
    pub(crate) status: PathBuf,
    /// When the bridge started the run.
    pub(crate) started_at: Timestamp,
    /// The herdr pane it runs in.
    pub(crate) pane: String,
    /// The device that asked for it.
    pub(crate) device: String,
    /// Where the request came from.
    pub(crate) origin: Origin,
}

/// What the runner (or the bridge, giving up) reports, in the status file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RunnerStatus {
    /// The runner has claimed the run; the program runs once `pid` is set.
    Running {
        /// The runner's own process id (the bridge checks it is alive).
        runner: u32,
        /// The program's process id, once started.
        #[serde(default)]
        pid: Option<u32>,
    },
    /// The program ended by itself.
    Exited {
        /// Its exit code; `None` when a signal ended it.
        code: Option<i32>,
    },
    /// The program was killed at the timeout.
    TimedOut,
    /// The program could not be started, the runner stopped without
    /// reporting, or the bridge gave up before the runner claimed the run.
    Failed {
        /// Why, for the user.
        message: String,
    },
}

impl RunnerStatus {
    /// A status file's content; `None` if it is not one.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }

    /// How it ended: the run's state and exit code; `None` while running.
    pub(crate) fn ending(&self) -> Option<(RunState, Option<i32>)> {
        match self {
            Self::Running { .. } => None,
            Self::Exited { code } => Some((RunState::Exited, *code)),
            Self::TimedOut => Some((RunState::TimedOut, None)),
            Self::Failed { .. } => Some((RunState::Failed, None)),
        }
    }

    /// Writes it atomically over an existing status (a temporary file
    /// renamed over `path`), so the bridge never reads half a status.
    pub(crate) fn write(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(self).map_err(io::Error::other)?)?;
        fs::rename(&tmp, path)
    }

    /// Creates the status file with this status, only if there is none:
    /// `Ok(false)` if one exists (someone claimed the run first).
    pub(crate) fn claim(&self, path: &Path) -> io::Result<bool> {
        let created = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path);
        match created {
            Ok(mut file) => {
                file.write_all(&serde_json::to_vec(self).map_err(io::Error::other)?)?;
                file.sync_all()?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// `passporter-bridge runbook <spec>`: runs the spec once, mirroring the
/// program's exit code (127 if it could not or may not start, 124 at the
/// timeout).
#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the runner's output is the pane the user watches"
)]
#[must_use]
pub fn main(spec: &Path) -> ExitCode {
    let spec = match read_spec(spec) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!(
                "passporter: cannot read runbook spec {}: {error}",
                spec.display()
            );
            return ExitCode::from(NOT_STARTED);
        }
    };
    let claim = RunnerStatus::Running {
        runner: std::process::id(),
        pid: None,
    };
    match claim.claim(&spec.status) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "passporter: {} was started already, or the bridge stopped waiting for it; \
                 nothing ran this time.",
                spec.runbook
            );
            return ExitCode::from(NOT_STARTED);
        }
        Err(error) => {
            eprintln!("passporter: cannot report to the bridge ({error}); nothing ran.");
            return ExitCode::from(NOT_STARTED);
        }
    }
    println!(
        "▶ {}: {} (in {})",
        spec.runbook,
        command_line(&spec.argv),
        spec.cwd.display()
    );
    let started = Instant::now();
    let (status, code) = run(&spec);
    let took = started.elapsed().as_secs();
    let end = match &status {
        RunnerStatus::Failed { message } => message.clone(),
        other => other
            .ending()
            .map(|(state, code)| protocol::runbook::outcome(state, code))
            .unwrap_or_default(),
    };
    println!("■ {}: {end} ({took}s)", spec.runbook);
    if let Err(error) = status.write(&spec.status) {
        report_error(&error);
    }
    ExitCode::from(code)
}

fn read_spec(path: &Path) -> io::Result<RunSpec> {
    let meta = fs::metadata(path)?;
    if !meta.is_file() || meta.len() > MAX_SPEC_BYTES {
        return Err(io::Error::other("not a runbook spec file"));
    }
    let text = fs::read(path)?;
    let spec: RunSpec = serde_json::from_slice(&text).map_err(io::Error::other)?;
    if spec.argv.is_empty() {
        return Err(io::Error::other("empty argv"));
    }
    Ok(spec)
}

/// Runs the program to its end or timeout; the final status and the exit
/// code to mirror.
fn run(spec: &RunSpec) -> (RunnerStatus, u8) {
    let Some((program, args)) = spec.argv.split_first() else {
        return failed("empty argv".to_owned());
    };
    let mut child = match Command::new(program_path(program, &spec.cwd))
        .args(args)
        .current_dir(&spec.cwd)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return failed(format!("cannot start {program}: {error}")),
    };
    let running = RunnerStatus::Running {
        runner: std::process::id(),
        pid: Some(child.id()),
    };
    if let Err(error) = running.write(&spec.status) {
        // The claim already says it runs; the run goes on regardless.
        report_error(&error);
    }
    let deadline = Instant::now() + Duration::from_secs(spec.timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return exited(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return (RunnerStatus::TimedOut, TIMED_OUT);
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(error) => {
                let _ = child.kill();
                return failed(format!("lost the program: {error}"));
            }
        }
    }
}

#[expect(
    clippy::print_stderr,
    reason = "the runner's output is the pane the user watches"
)]
fn report_error(error: &io::Error) {
    eprintln!("passporter: cannot report to the bridge: {error}");
}

fn failed(message: String) -> (RunnerStatus, u8) {
    (RunnerStatus::Failed { message }, NOT_STARTED)
}

fn exited(status: ExitStatus) -> (RunnerStatus, u8) {
    let code = status.code();
    let mirrored = code.map_or(1, |c| u8::try_from(c & 0xff).unwrap_or(1));
    (RunnerStatus::Exited { code }, mirrored)
}

/// The program to execute: a relative path with a `/` (`deploy/x.sh`) is
/// taken relative to `cwd`, so it never depends on where the runner was
/// started; a bare name is looked up on `PATH`.
fn program_path(program: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(program);
    if path.is_relative() && program.contains('/') {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl RunnerStatus {
        fn read(path: &Path) -> Option<Self> {
            Self::parse(&fs::read(path).ok()?)
        }
    }

    fn spec(dir: &Path, argv: &[&str], timeout_secs: u64) -> RunSpec {
        RunSpec {
            runbook: "Test".to_owned(),
            cwd: dir.to_path_buf(),
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            timeout_secs,
            status: dir.join("run.status"),
            started_at: Timestamp::from_unix_millis(1),
            pane: "w1:p1".to_owned(),
            device: "dev".to_owned(),
            origin: Origin::App,
        }
    }

    #[test]
    fn relative_programs_resolve_against_cwd() {
        let cwd = Path::new("/repo");
        assert_eq!(
            program_path("deploy/x.sh", cwd),
            Path::new("/repo/deploy/x.sh")
        );
        assert_eq!(program_path("uptime", cwd), Path::new("uptime"));
        assert_eq!(program_path("/bin/true", cwd), Path::new("/bin/true"));
    }

    #[test]
    fn runs_argv_directly_and_reports_the_exit_code() {
        let tmp = tempfile::tempdir().expect("tmp");
        // A shell metacharacter in an argument stays literal: no shell.
        let ok = spec(tmp.path(), &["test", "$(false)", "=", "$(false)"], 10);
        assert_eq!(run(&ok), (RunnerStatus::Exited { code: Some(0) }, 0));
        let fails = spec(tmp.path(), &["false"], 10);
        assert_eq!(run(&fails), (RunnerStatus::Exited { code: Some(1) }, 1));
        let missing = spec(tmp.path(), &["./no-such-program"], 10);
        assert!(matches!(
            run(&missing),
            (RunnerStatus::Failed { .. }, NOT_STARTED)
        ));
    }

    #[test]
    fn kills_at_the_timeout() {
        let tmp = tempfile::tempdir().expect("tmp");
        let slow = spec(tmp.path(), &["sleep", "30"], 1);
        let started = Instant::now();
        assert_eq!(run(&slow), (RunnerStatus::TimedOut, TIMED_OUT));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn statuses_round_trip_through_the_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("s.status");
        assert_eq!(RunnerStatus::read(&path), None);
        let status = RunnerStatus::Exited { code: Some(3) };
        status.write(&path).expect("write");
        assert_eq!(RunnerStatus::read(&path), Some(status));
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn only_the_first_claim_wins() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("s.status");
        let gave_up = RunnerStatus::Failed {
            message: "gave up".into(),
        };
        assert!(gave_up.claim(&path).expect("claim"));
        let runner = RunnerStatus::Running {
            runner: 1,
            pid: None,
        };
        assert!(!runner.claim(&path).expect("claim"));
        assert_eq!(RunnerStatus::read(&path), Some(gave_up));
    }

    #[test]
    fn main_runs_a_spec_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let marker = tmp.path().join("ran");
        let marker_arg = marker.to_string_lossy().into_owned();
        let spec = spec(tmp.path(), &["touch", &marker_arg], 10);
        let file = tmp.path().join("run.spec.json");
        fs::write(&file, serde_json::to_vec(&spec).expect("json")).expect("write");
        assert_eq!(main(&file), ExitCode::SUCCESS);
        assert!(marker.exists());
        assert_eq!(
            RunnerStatus::read(&spec.status),
            Some(RunnerStatus::Exited { code: Some(0) })
        );
        // The launch line typed again (or a replay): nothing runs.
        fs::remove_file(&marker).expect("rm");
        assert_eq!(main(&file), ExitCode::from(NOT_STARTED));
        assert!(!marker.exists(), "ran twice");
        assert_eq!(
            main(&tmp.path().join("missing")),
            ExitCode::from(NOT_STARTED)
        );
    }
}
