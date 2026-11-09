//! The bridge-facing view of an agent (`GET /v1/agents`). Outbound: tolerant.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, Seq};
use crate::status_line::StatusLine;
use crate::time::Timestamp;

/// Most characters in an agent's [`ask`](Agent::ask).
pub const MAX_ASK_CHARS: usize = 120;

/// One agent as shown on a triage card (design §6.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    /// Pane + agent session.
    pub id: AgentId,
    /// The agent's name, or the terminal title when it has none.
    pub title: String,
    /// Repository name (the main checkout's directory name).
    pub repo: String,
    /// Worktree name, when the agent runs in a linked worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// Current state.
    pub status: AgentStatus,
    /// herdr `state_change_seq`; echo it as an action's `expectedSeq`.
    pub seq: Seq,
    /// When the agent entered `status`. Time in state is computed by the
    /// client against its own clock, so a cached snapshot never goes stale.
    pub status_since: Timestamp,
    /// A completion the user has not yet seen (the unread dot).
    #[serde(default)]
    pub unread: bool,
    /// Which agent CLI runs in the pane.
    pub kind: AgentKind,
    /// One line of the latest question or result, for the card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    /// While it works: what it is doing now, from the turn in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_line: Option<StatusLine>,
    /// While it is blocked: what it is asking, e.g. ``Run `cargo test`?``,
    /// one line of plain text of at most [`MAX_ASK_CHARS`] characters, plus
    /// an ellipsis when cut (the prompt of its pending input). Untrusted
    /// agent output: display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<String>,
    /// An agent run by an orchestrator (another agent waits on it, not the
    /// user): its finished turns are not news, only its dialogs are. Sent
    /// only when true; absent (an older bridge) means false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet_done: bool,
}

/// An agent's state, as reported by herdr (`agent_status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Waiting for input.
    Idle,
    /// Running a turn.
    Working,
    /// Waiting on a dialog (permission, question, plan approval).
    Blocked,
    /// Finished its turn.
    Done,
    /// herdr could not tell, or a newer bridge sent a state this app predates.
    #[serde(other)]
    Unknown,
}

impl AgentStatus {
    /// Whether the agent belongs in the *Done* triage segment: herdr `idle`
    /// and `done` both count as ready (design §6.1).
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }
}

/// Which agent CLI runs in the pane. Transcript and hook features are
/// Claude-only; the rest works for every kind through herdr (design §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// Claude Code.
    Claude,
    /// The Codex CLI.
    Codex,
    /// opencode.
    Opencode,
    /// pi.
    Pi,
    /// Anything else.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_status_and_kind_fall_back() {
        assert_eq!(
            serde_json::from_str::<AgentStatus>("\"hibernating\"").unwrap(),
            AgentStatus::Unknown
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"aider\"").unwrap(),
            AgentKind::Unknown
        );
    }

    #[test]
    fn quiet_done_is_sent_only_when_true_and_defaults_to_false() {
        let json = serde_json::json!({
            "id": "w1:p1", "title": "impl-t1", "repo": "passporter", "status": "done",
            "seq": 3, "statusSince": 0, "kind": "claude",
        });
        let mut agent: Agent = serde_json::from_value(json.clone()).unwrap();
        assert!(!agent.quiet_done, "an older bridge: not quiet");
        assert_eq!(
            serde_json::to_value(&agent).unwrap()["quietDone"],
            serde_json::Value::Null
        );
        agent.quiet_done = true;
        let wire = serde_json::to_value(&agent).unwrap();
        assert_eq!(wire["quietDone"], true);
        assert_eq!(serde_json::from_value::<Agent>(wire).unwrap(), agent);
    }

    #[test]
    fn the_ask_is_sent_only_when_known_and_defaults_to_none() {
        let json = serde_json::json!({
            "id": "w1:p1", "title": "impl-t1", "repo": "passporter", "status": "blocked",
            "seq": 3, "statusSince": 0, "kind": "claude",
        });
        let mut agent: Agent = serde_json::from_value(json).unwrap();
        assert_eq!(agent.ask, None, "an older bridge: no ask");
        assert!(serde_json::to_value(&agent).unwrap().get("ask").is_none());
        agent.ask = Some("Run `cargo test`?".to_owned());
        let wire = serde_json::to_value(&agent).unwrap();
        assert_eq!(wire["ask"], "Run `cargo test`?");
        assert_eq!(serde_json::from_value::<Agent>(wire).unwrap(), agent);
    }

    #[test]
    fn ready_covers_idle_and_done() {
        assert!(AgentStatus::Idle.is_ready());
        assert!(AgentStatus::Done.is_ready());
        assert!(!AgentStatus::Blocked.is_ready());
        assert!(!AgentStatus::Working.is_ready());
        assert!(!AgentStatus::Unknown.is_ready());
    }
}
