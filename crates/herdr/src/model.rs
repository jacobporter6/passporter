//! Typed views of what herdr returns.
//!
//! Only the fields Passporter uses are modeled; unknown fields are ignored so
//! newer herdr versions keep working. Field names follow herdr's wire format.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{AgentName, PaneId, TabId, TerminalId, WorkspaceId};

/// herdr's lifecycle classification of the agent in a pane.
///
/// `Idle` and `Done` both mean "ready for input"; `Done` marks a completion
/// not yet seen. `Unknown` means herdr can't classify it confidently, and also
/// absorbs any status a future herdr adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    #[serde(other)]
    Unknown,
}

impl AgentStatus {
    /// Ready for a new prompt (`idle` or `done`).
    #[must_use]
    pub fn is_ready(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }
}

/// The agent's own session handle, e.g. a Claude Code session ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSession {
    /// Which integration reported it, e.g. `herdr:claude`.
    pub source: String,
    /// Agent kind, e.g. `claude`.
    pub agent: String,
    /// What `value` is, e.g. `id`.
    pub kind: String,
    /// For Claude: the session ID, which names the transcript file.
    pub value: String,
}

/// An agent occupying a pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub terminal_id: TerminalId,
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub workspace_id: WorkspaceId,
    /// The live name, if one was assigned (`agent start` or `agent rename`).
    #[serde(default)]
    pub name: Option<AgentName>,
    /// Detected agent kind, e.g. `claude`; `None` until herdr recognizes it.
    #[serde(default, rename = "agent")]
    pub kind: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
    /// Bumped on every status change. The bridge's stale guard compares it.
    #[serde(default)]
    pub state_change_seq: u64,
    pub revision: u64,
    /// The title the agent reported (e.g. Claude's session summary).
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub foreground_cwd: Option<PathBuf>,
    pub focused: bool,
    #[serde(default)]
    pub interactive_ready: bool,
}

/// A pane, whether or not an agent is in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub tab_id: TabId,
    pub workspace_id: WorkspaceId,
    #[serde(rename = "agent", default)]
    pub agent_kind: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    pub focused: bool,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub tab_id: TabId,
    pub workspace_id: WorkspaceId,
    pub label: String,
    pub number: u32,
    pub focused: bool,
    pub pane_count: u32,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub workspace_id: WorkspaceId,
    pub label: String,
    pub number: u32,
    pub focused: bool,
    pub pane_count: u32,
    pub tab_count: u32,
    pub active_tab_id: TabId,
    pub agent_status: AgentStatus,
}

/// A newly created workspace with its first tab and pane.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreatedWorkspace {
    pub workspace: Workspace,
    pub tab: Tab,
    pub root_pane: Pane,
}

/// A newly created tab with its first pane.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreatedTab {
    pub tab: Tab,
    pub root_pane: Pane,
}

/// Which snapshot of a pane's output to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadSource {
    /// The currently rendered viewport.
    Visible,
    /// Recent output, including soft wraps.
    Recent,
    /// Recent output with soft wraps joined: best for logs and transcripts.
    RecentUnwrapped,
    /// The bottom-buffer snapshot herdr uses for agent detection.
    Detection,
}

/// Text read from a pane (ANSI stripped).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Screen {
    pub pane_id: PaneId,
    pub source: ReadSource,
    pub text: String,
    pub revision: u64,
    /// More output existed than was returned.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

/// What `ping` reports about the server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub protocol: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from herdr 0.9.1 (`agent.get`), session ID replaced.
    const AGENT_INFO: &str = r#"{
        "terminal_id": "term_65fb3aadc4ab011", "name": "circus", "agent": "claude",
        "terminal_title": "◐ Passporter", "terminal_title_stripped": "Passporter",
        "agent_status": "working",
        "agent_session": {"source": "herdr:claude", "agent": "claude", "kind": "id",
                          "value": "00000000-0000-4000-8000-000000000000"},
        "workspace_id": "w7", "tab_id": "w7:t1", "pane_id": "w7:p1", "focused": true,
        "interactive_ready": true, "state_change_seq": 151,
        "cwd": "/home/dev/git/passporter", "foreground_cwd": "/home/dev/git/passporter",
        "revision": 4, "tokens": {}, "state_labels": {}, "some_future_field": [1, 2]
    }"#;

    #[test]
    fn decodes_real_agent_info() {
        let agent: Agent = serde_json::from_str(AGENT_INFO).expect("decodes");
        assert_eq!(agent.name.as_ref().map(AgentName::as_str), Some("circus"));
        assert_eq!(agent.kind.as_deref(), Some("claude"));
        assert_eq!(agent.agent_status, AgentStatus::Working);
        assert_eq!(agent.state_change_seq, 151);
        assert_eq!(agent.pane_id.as_str(), "w7:p1");
        let session = agent.agent_session.expect("has session");
        assert_eq!(session.value, "00000000-0000-4000-8000-000000000000");
    }

    #[test]
    fn decodes_unrecognized_agent_with_nulls() {
        let json = r#"{"terminal_id":"t","name":null,"agent":null,"agent_status":"idle",
            "workspace_id":"w6","tab_id":"w6:t1","pane_id":"w6:p2","focused":false,"revision":0}"#;
        let agent: Agent = serde_json::from_str(json).expect("decodes");
        assert_eq!(agent.name, None);
        assert_eq!(agent.kind, None);
        assert_eq!(agent.state_change_seq, 0, "schema default");
    }

    #[test]
    fn future_statuses_decode_as_unknown() {
        let status: AgentStatus = serde_json::from_str("\"thinking_hard\"").expect("decodes");
        assert_eq!(status, AgentStatus::Unknown);
        let status: AgentStatus = serde_json::from_str("\"unknown\"").expect("decodes");
        assert_eq!(status, AgentStatus::Unknown);
    }

    #[test]
    fn ready_means_idle_or_done() {
        assert!(AgentStatus::Idle.is_ready());
        assert!(AgentStatus::Done.is_ready());
        assert!(!AgentStatus::Blocked.is_ready());
        assert!(!AgentStatus::Working.is_ready());
        assert!(!AgentStatus::Unknown.is_ready());
    }

    #[test]
    fn read_source_wire_names() {
        let json = serde_json::to_string(&ReadSource::RecentUnwrapped).expect("ser");
        assert_eq!(json, "\"recent_unwrapped\"");
    }
}
