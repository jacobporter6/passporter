//! The audit lines of terminal mode, in the action audit log (design §8).
//!
//! A command run writes an `execute` line, synced **before** anything is
//! typed, with the **exact** command (never cut: it is what runs, at most
//! [`protocol::MAX_COMMAND_CHARS`]), the agent, the candidate's id and hash
//! and where it runs; then a `result` line once decided. Keys sent to a
//! terminal get the same pair, with the pane and the keys.

use protocol::{
    AgentId, CommandResult, CommandRunRequest, Origin, RunTarget, TerminalKeysRequest, Timestamp,
};
use serde::Serialize;

use crate::actions::audit::Phase;

/// One audit line for a command run.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandEntry<'a> {
    at: Timestamp,
    phase: Phase,
    device: &'a str,
    /// Always `"command"`: tells these lines from actions'.
    kind: &'static str,
    idempotency_key: &'a str,
    origin: Origin,
    agent_id: &'a AgentId,
    candidate: u16,
    hash: &'a str,
    target: RunTarget,
    created_at: Timestamp,
    /// The exact command, for `execute` lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a str>,
    /// The terminal's pane, for `result` lines of a run that opened one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pane: Option<&'a str>,
    /// The result's status (and refusal reason), for `result` lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
}

impl<'a> CommandEntry<'a> {
    fn new(
        phase: Phase,
        device: &'a str,
        agent: &'a AgentId,
        request: &'a CommandRunRequest,
    ) -> Self {
        Self {
            at: crate::registry::now(),
            phase,
            device,
            kind: "command",
            idempotency_key: request.idempotency_key.as_str(),
            origin: request.origin,
            agent_id: agent,
            candidate: request.candidate,
            hash: request.hash.as_str(),
            target: request.target,
            created_at: request.created_at,
            command: None,
            pane: None,
            outcome: None,
        }
    }

    /// The line written before `command` runs.
    pub(crate) fn execute(
        device: &'a str,
        agent: &'a AgentId,
        request: &'a CommandRunRequest,
        command: &'a str,
    ) -> Self {
        Self {
            command: Some(command),
            ..Self::new(Phase::Execute, device, agent, request)
        }
    }

    /// The line for how `request` was decided.
    pub(crate) fn result(
        device: &'a str,
        agent: &'a AgentId,
        request: &'a CommandRunRequest,
        result: &'a CommandResult,
    ) -> Self {
        Self {
            pane: match result {
                CommandResult::Started { terminal, .. } => Some(terminal.pane.as_str()),
                _ => None,
            },
            outcome: Some(outcome(result)),
            ..Self::new(Phase::Result, device, agent, request)
        }
    }
}

/// One audit line for keys sent to a terminal.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeysEntry<'a> {
    at: Timestamp,
    phase: Phase,
    device: &'a str,
    /// Always `"terminal_keys"`.
    kind: &'static str,
    idempotency_key: &'a str,
    origin: Origin,
    pane: &'a str,
    keys: Vec<&'static str>,
    created_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
}

impl<'a> KeysEntry<'a> {
    /// A line for `request` to `pane`; `result` only for [`Phase::Result`].
    pub(crate) fn new(
        phase: Phase,
        device: &'a str,
        pane: &'a herdr::PaneId,
        request: &'a TerminalKeysRequest,
        result: Option<&CommandResult>,
    ) -> Self {
        Self {
            at: crate::registry::now(),
            phase,
            device,
            kind: "terminal_keys",
            idempotency_key: request.idempotency_key.as_str(),
            origin: request.origin,
            pane: pane.as_str(),
            keys: request.keys.as_slice().iter().map(|k| k.as_str()).collect(),
            created_at: request.created_at,
            outcome: result.map(outcome),
        }
    }
}

fn outcome(result: &CommandResult) -> String {
    match result {
        CommandResult::Sent { replayed: false } => "sent".to_owned(),
        CommandResult::Started {
            replayed: false, ..
        } => "started".to_owned(),
        CommandResult::Sent { replayed: true } | CommandResult::Started { replayed: true, .. } => {
            "replayed".to_owned()
        }
        CommandResult::Refused { reason, .. } => format!("refused: {}", reason.as_str()),
        CommandResult::Failed { .. } => "failed".to_owned(),
        CommandResult::Unknown => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use protocol::CommandRefusal;

    use super::*;

    fn request() -> CommandRunRequest {
        serde_json::from_value(serde_json::json!({
            "idempotencyKey": "00000000-0000-4000-8000-000000000001",
            "origin": "app",
            "createdAt": 5,
            "candidate": 1,
            "hash": "0f1e2d3c4b5a69788796a5b4c3d2e1f0",
            "target": "here",
        }))
        .expect("valid request")
    }

    #[test]
    fn execute_lines_hold_the_exact_command() {
        let request = request();
        let agent: AgentId = "w1:p2".parse().expect("id");
        let long = format!("echo {}", "x".repeat(1500));
        let line = serde_json::to_value(CommandEntry::execute("dev", &agent, &request, &long))
            .expect("json");
        assert_eq!(line["phase"], "execute");
        assert_eq!(line["kind"], "command");
        assert_eq!(line["agentId"], "w1:p2");
        assert_eq!(line["candidate"], 1);
        assert_eq!(line["target"], "here");
        assert_eq!(line["command"], long.as_str());
        assert!(line.get("outcome").is_none());

        let refused = CommandResult::Refused {
            reason: CommandRefusal::Stale,
            message: String::new(),
        };
        let line = serde_json::to_value(CommandEntry::result("dev", &agent, &request, &refused))
            .expect("json");
        assert_eq!(line["outcome"], "refused: stale");
        assert!(line.get("command").is_none());
    }
}
