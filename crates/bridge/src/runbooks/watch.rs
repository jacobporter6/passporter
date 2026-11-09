//! Following one run to its end: typing the launch line once the tab's
//! shell looks ready, then polling the status file and the pane, and
//! publishing each change as `runbook_updated`.
//!
//! - **Launch.** A fresh tab's shell may still be running its rc files,
//!   and some shells drop what was typed meanwhile. The watcher waits
//!   (boundedly) for the pane to show a settled prompt, then types the
//!   line **once**. It never types it again: a second copy still buffered
//!   in a slow shell's input would be read by the runbook's program.
//! - **Never started.** After [`RunbookSettings::start_wait`] the bridge
//!   claims the status file itself (`failed`). Only if that claim wins is
//!   the run over: a runner that claimed first is followed as usual, and
//!   one that starts later finds the claim and runs nothing. A claim file
//!   that is still unreadable by then (a runner that died mid-write) is
//!   overwritten as failed too.
//! - **Interrupted.** While the runner's claim says `running`, the watcher
//!   checks that the runner process is still alive (and still on this
//!   spec). A runner killed without reporting (Ctrl-C, its tab closed)
//!   ends the run as failed at the next poll, and the status file says so.
//!   The runbook stays claimed (busy) for as long as its runner lives.
//!
//! [`RunbookSettings::start_wait`]: super::RunbookSettings::start_wait

use std::sync::Arc;
use std::time::Duration;

use herdr::PaneId;
use protocol::{MAX_RUN_LINES, Origin, RunState, RunbookRun};
use tokio::time::Instant;

use super::Active;
use super::audit::Asked;
use super::files::RunFiles;
use super::runner::RunnerStatus;
use crate::spawn::SpawnControl;
use crate::spawn::tabs::wait_for_shell;

/// Longest output line a run carries, in characters.
const MAX_LINE_CHARS: usize = 200;

/// How many lines of the pane to read for the last lines (more than are
/// kept: the tail of a pane is often blank).
const READ_LINES: u32 = 40;

pub(super) const NEVER_STARTED: &str =
    "The runbook did not start in its herdr tab; nothing ran. Look there.";
const INTERRUPTED: &str =
    "The run stopped without reporting how it ended (interrupted, or its tab was closed).";

/// One run being followed.
pub(super) struct Watch {
    /// The runbook's claim, released when the run ends.
    pub(super) active: Active,
    pub(super) pane: PaneId,
    pub(super) files: RunFiles,

    /// The run as last published.
    pub(super) run: RunbookRun,
    pub(super) timeout: Duration,
    /// The launch line to type first: for a fresh run only (a run picked up
    /// after a restart was claimed already, or is given up on).
    pub(super) launch: Option<String>,
    pub(super) device: String,
    pub(super) key: String,
    pub(super) origin: Origin,
}

impl Watch {
    /// Launches (if asked), then follows the run until it ends; audits the
    /// end and releases the runbook.
    pub(super) async fn follow(mut self) {
        let runbooks = Arc::clone(&self.active.runbooks);
        let settings = &runbooks.settings;
        let control = runbooks.control.as_ref();
        if let Some(line) = &self.launch {
            wait_for_shell(control, &self.pane, settings.shell_wait).await;
            if let Err(error) = control.type_line(&self.pane, line).await {
                // The start deadline takes it from here.
                tracing::warn!(%error, pane = %self.pane, "typing a runbook's launch line failed");
            }
        }
        let start_by = Instant::now() + settings.start_wait;
        loop {
            tokio::time::sleep(settings.poll).await;
            let now = Instant::now();
            let ended = match self.files.status().await {
                None if now >= start_by => self.give_up().await,
                None => None,
                Some(RunnerStatus::Running { runner, .. }) if !self.files.runner_alive(runner) => {
                    // It may have reported its end just before exiting.
                    let reported = self.files.status().await;
                    if let Some(ending) =
                        reported.as_ref().and_then(|s| Ending::of(s, self.timeout))
                    {
                        Some(ending)
                    } else {
                        self.files
                            .overwrite(RunnerStatus::Failed {
                                message: INTERRUPTED.to_owned(),
                            })
                            .await;
                        Some(Ending::failed(INTERRUPTED))
                    }
                }
                Some(status) => Ending::of(&status, self.timeout),
            };
            let mut next = self.run.clone();
            if let Some(lines) = last_lines(control, &self.pane).await {
                next.last_lines = lines;
            }
            if let Some(ending) = ended {
                next.state = ending.state;
                next.exit_code = ending.exit_code;
                next.message = ending.message;
                next.finished_at = Some(crate::registry::now());
            }
            if next != self.run {
                self.run = next;
                runbooks.record(&self.run);
            }
            if self.run.is_finished() {
                break;
            }
        }
        tracing::info!(runbook = %self.run.runbook, state = ?self.run.state, exit_code = ?self.run.exit_code, "runbook run ended");
        let asked = Asked {
            device: &self.device,
            key: &self.key,
            origin: self.origin,
            runbook: &self.run.runbook,
        };
        runbooks.audit_finish(&self.files, asked, &self.run).await;
    }

    /// The runner never claimed the run in time: claims it as failed, so it
    /// can never start. If the runner won after all, says how its claim
    /// stands; a claim still unreadable (the runner died writing it) is
    /// overwritten as failed.
    async fn give_up(&self) -> Option<Ending> {
        let never = || RunnerStatus::Failed {
            message: NEVER_STARTED.to_owned(),
        };
        if self.files.give_up(never()).await {
            return Some(Ending::failed(NEVER_STARTED));
        }
        if let Some(status) = self.files.status().await {
            // Claimed meanwhile: running is followed by the next poll.
            return Ending::of(&status, self.timeout);
        }
        self.files.overwrite(never()).await;
        Some(Ending::failed(NEVER_STARTED))
    }
}

/// How a run ended.
pub(super) struct Ending {
    pub(super) state: RunState,
    pub(super) exit_code: Option<i32>,
    pub(super) message: Option<String>,
}

impl Ending {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            state: RunState::Failed,
            exit_code: None,
            message: Some(message.into()),
        }
    }

    /// How `status` says the run ended; `None` while it runs.
    pub(super) fn of(status: &RunnerStatus, timeout: Duration) -> Option<Self> {
        let (state, exit_code) = status.ending()?;
        let message = match status {
            RunnerStatus::Failed { message } => Some(message.clone()),
            RunnerStatus::TimedOut => Some(format!(
                "Killed after {}s (the runbook's timeout).",
                timeout.as_secs()
            )),
            _ => None,
        };
        Some(Self {
            state,
            exit_code,
            message,
        })
    }
}

/// The pane's last non-blank lines (at most [`MAX_RUN_LINES`], each cut to
/// [`MAX_LINE_CHARS`]); `None` if herdr could not read it.
async fn last_lines(control: &dyn SpawnControl, pane: &PaneId) -> Option<Vec<String>> {
    match control.read_recent(pane, READ_LINES).await {
        Ok(text) => Some(tail(&text)),
        Err(error) => {
            tracing::debug!(%error, %pane, "cannot read a runbook pane");
            None
        }
    }
}

fn tail(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(MAX_RUN_LINES)..]
        .iter()
        .map(|line| line.chars().take(MAX_LINE_CHARS).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_keeps_the_last_non_blank_lines() {
        let text = (1..=12)
            .map(|n| format!("line {n}   "))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n\n   \n";
        let lines = tail(&text);
        assert_eq!(lines.len(), MAX_RUN_LINES);
        assert_eq!(lines.first().map(String::as_str), Some("line 5"));
        assert_eq!(lines.last().map(String::as_str), Some("line 12"));
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        assert_eq!(tail(&long)[0].chars().count(), MAX_LINE_CHARS);
        assert_eq!(tail(""), Vec::<String>::new());
    }

    #[test]
    fn endings_carry_why() {
        let timeout = Duration::from_secs(7);
        let running = RunnerStatus::Running {
            runner: 1,
            pid: Some(2),
        };
        assert!(Ending::of(&running, timeout).is_none());
        let timed_out = Ending::of(&RunnerStatus::TimedOut, timeout).expect("ended");
        assert_eq!(timed_out.state, RunState::TimedOut);
        assert!(timed_out.message.is_some_and(|m| m.contains("7s")));
        let exited = Ending::of(&RunnerStatus::Exited { code: Some(4) }, timeout).expect("ended");
        assert_eq!(
            (exited.state, exited.exit_code),
            (RunState::Exited, Some(4))
        );
    }
}
