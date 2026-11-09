//! The audit lines of a runbook run, in the action audit log (design §8):
//! an `execute` line written and synced **before** herdr is asked for
//! anything, a `result` line once the request is decided, and a `finish`
//! line when the run ends. Each line names the runbook and the exact
//! `argv` and `cwd` it runs (from the config), so the log alone says what
//! was executed.

use std::path::Path;

use protocol::{Origin, RunbookName, RunbookResult, RunbookRun, Timestamp};
use serde::Serialize;

use crate::actions::audit::Phase;

/// One audit line for a runbook run.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunbookEntry<'a> {
    at: Timestamp,
    phase: Phase,
    device: &'a str,
    /// Always `"runbook"`: tells these lines from actions' and spawns'.
    kind: &'static str,
    idempotency_key: &'a str,
    origin: Origin,
    runbook: &'a str,
    /// What runs, for `execute` lines (from the config).
    #[serde(skip_serializing_if = "Option::is_none")]
    argv: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a Path>,
    /// The result's status (and refusal reason), or how the run ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
}

/// Who asked for a run, and how.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Asked<'a> {
    pub(crate) device: &'a str,
    pub(crate) key: &'a str,
    pub(crate) origin: Origin,
    pub(crate) runbook: &'a RunbookName,
}

impl<'a> RunbookEntry<'a> {
    fn new(phase: Phase, asked: Asked<'a>) -> Self {
        Self {
            at: crate::registry::now(),
            phase,
            device: asked.device,
            kind: "runbook",
            idempotency_key: asked.key,
            origin: asked.origin,
            runbook: asked.runbook.as_str(),
            argv: None,
            cwd: None,
            outcome: None,
        }
    }

    /// The `execute` line: what is about to run.
    pub(crate) fn execute(asked: Asked<'a>, argv: &'a [String], cwd: &'a Path) -> Self {
        Self {
            argv: Some(argv),
            cwd: Some(cwd),
            ..Self::new(Phase::Execute, asked)
        }
    }

    /// The `result` line: what the client was told.
    pub(crate) fn result(asked: Asked<'a>, result: &RunbookResult) -> Self {
        Self {
            outcome: Some(outcome(result)),
            ..Self::new(Phase::Result, asked)
        }
    }

    /// The `finish` line: how the run ended.
    pub(crate) fn finish(asked: Asked<'a>, run: &RunbookRun) -> Self {
        Self {
            outcome: Some(run.outcome()),
            ..Self::new(Phase::Finish, asked)
        }
    }
}

fn outcome(result: &RunbookResult) -> String {
    match result {
        RunbookResult::Started { replayed: true, .. } => "started (replayed)".to_owned(),
        RunbookResult::Started { .. } => "started".to_owned(),
        RunbookResult::Refused { reason, .. } => format!("refused: {}", reason.as_str()),
        RunbookResult::Failed { .. } => "failed".to_owned(),
        RunbookResult::Unknown => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use protocol::{RunState, RunbookRefusal};

    use super::*;

    #[test]
    fn lines_say_what_ran_and_how_it_ended() {
        let name = RunbookName::new("Deploy bridge").expect("name");
        let asked = Asked {
            device: "dev",
            key: "00000000-0000-4000-8000-000000000001",
            origin: Origin::App,
            runbook: &name,
        };
        let argv = vec!["deploy/bridge/setup.sh".to_owned()];
        let line = serde_json::to_value(RunbookEntry::execute(asked, &argv, Path::new("/repo")))
            .expect("json");
        assert_eq!(line["phase"], "execute");
        assert_eq!(line["kind"], "runbook");
        assert_eq!(line["runbook"], "Deploy bridge");
        assert_eq!(line["argv"][0], "deploy/bridge/setup.sh");
        assert_eq!(line["cwd"], "/repo");
        assert_eq!(line["origin"], "app");
        assert!(line.get("outcome").is_none());

        let refused = RunbookResult::Refused {
            reason: RunbookRefusal::Busy,
            message: String::new(),
        };
        let line = serde_json::to_value(RunbookEntry::result(asked, &refused)).expect("json");
        assert_eq!(line["outcome"], "refused: busy");
        assert!(line.get("argv").is_none());

        let run = RunbookRun {
            runbook: name.clone(),
            state: RunState::Exited,
            exit_code: Some(2),
            last_lines: Vec::new(),
            message: None,
            started_at: Timestamp::from_unix_millis(1),
            finished_at: None,
        };
        let line = serde_json::to_value(RunbookEntry::finish(asked, &run)).expect("json");
        assert_eq!(line["phase"], "finish");
        assert_eq!(line["outcome"], "exit 2");
    }
}
