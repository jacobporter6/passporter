//! Event subscriptions.
//!
//! herdr streams events on a dedicated connection after `events.subscribe`.
//! Note that agent status changes are subscribed **per pane**: to follow every
//! agent, also subscribe to [`Subscription::PaneCreated`] and
//! [`Subscription::AgentDetected`] and add a per-pane subscription as panes appear.

use serde::{Deserialize, Serialize};

use crate::ids::{PaneId, TabId, WorkspaceId};
use crate::model::{AgentStatus, Pane};

/// One thing to subscribe to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "type")]
pub enum Subscription {
    #[serde(rename = "pane.agent_status_changed")]
    AgentStatusChanged {
        pane_id: PaneId,
        /// Only report transitions into this status.
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_status: Option<AgentStatus>,
    },
    #[serde(rename = "pane.agent_detected")]
    AgentDetected,
    #[serde(rename = "pane.created")]
    PaneCreated,
    #[serde(rename = "pane.closed")]
    PaneClosed,
    #[serde(rename = "pane.exited")]
    PaneExited,
    #[serde(rename = "tab.closed")]
    TabClosed,
    #[serde(rename = "workspace.closed")]
    WorkspaceClosed,
}

impl Subscription {
    /// Every status change of the agent in `pane`.
    #[must_use]
    pub fn agent_status(pane: PaneId) -> Self {
        Self::AgentStatusChanged {
            pane_id: pane,
            agent_status: None,
        }
    }
}

/// An event from herdr. Kinds Passporter doesn't use arrive as [`Event::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    PaneAgentStatusChanged {
        pane_id: PaneId,
        workspace_id: WorkspaceId,
        agent_status: AgentStatus,
        #[serde(default)]
        agent: Option<String>,
        #[serde(default)]
        title: Option<String>,
    },
    /// An agent appeared in a pane, or (with `released: true`) left it.
    PaneAgentDetected {
        pane_id: PaneId,
        workspace_id: WorkspaceId,
        #[serde(default)]
        agent: Option<String>,
        /// The agent exited or was released: the pane no longer hosts it.
        #[serde(default)]
        released: bool,
        /// The agent's last status, when it was released.
        #[serde(default)]
        final_status: Option<AgentStatus>,
    },
    PaneCreated {
        pane: Pane,
    },
    PaneClosed {
        pane_id: PaneId,
        workspace_id: WorkspaceId,
    },
    PaneExited {
        pane_id: PaneId,
        workspace_id: WorkspaceId,
    },
    TabClosed {
        tab_id: TabId,
        workspace_id: WorkspaceId,
    },
    WorkspaceClosed {
        workspace_id: WorkspaceId,
    },
    #[serde(other)]
    Other,
}

/// The wire envelope. herdr uses two shapes on a subscription connection:
///
/// - subscription events (status changes, output matches, scroll):
///   `{"event": "pane.agent_status_changed", "data": {"pane_id": ..., ...}}`,
///   a dotted kind and **no** `type` in `data`;
/// - lifecycle events (pane closed, ...):
///   `{"event": "pane_closed", "data": {"type": "pane_closed", ...}}`.
///
/// Both are normalized to the underscore kind and decoded as one [`Event`].
#[derive(Debug, Deserialize)]
pub(crate) struct Envelope {
    event: String,
    data: serde_json::Value,
}

impl Envelope {
    pub(crate) fn into_event(self) -> Result<Event, serde_json::Error> {
        let mut data = self.data;
        if let Some(fields) = data.as_object_mut() {
            fields
                .entry("type")
                .or_insert_with(|| serde_json::Value::String(self.event.replace('.', "_")));
        }
        serde_json::from_value(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_serialize_to_herdr_shape() {
        let subs = vec![
            Subscription::agent_status(PaneId::new("w7:p1")),
            Subscription::AgentStatusChanged {
                pane_id: PaneId::new("w7:p2"),
                agent_status: Some(AgentStatus::Blocked),
            },
            Subscription::PaneCreated,
        ];
        let json = serde_json::to_value(&subs).expect("ser");
        assert_eq!(
            json,
            serde_json::json!([
                {"type": "pane.agent_status_changed", "pane_id": "w7:p1"},
                {"type": "pane.agent_status_changed", "pane_id": "w7:p2", "agent_status": "blocked"},
                {"type": "pane.created"},
            ])
        );
    }

    fn decode(line: &str) -> Event {
        let env: Envelope = serde_json::from_str(line).expect("envelope");
        env.into_event().expect("event")
    }

    /// The subscription-event shape herdr 0.9.1 actually sends: dotted kind, untagged data.
    #[test]
    fn decodes_live_subscription_status_change() {
        let line = r#"{"data":{"pane_id":"w7:p1","workspace_id":"w7","agent_status":"blocked",
            "agent":"claude","display_agent":"Claude","title":"Run tests?","state_labels":{}},
            "event":"pane.agent_status_changed"}"#;
        assert_eq!(
            decode(line),
            Event::PaneAgentStatusChanged {
                pane_id: PaneId::new("w7:p1"),
                workspace_id: WorkspaceId::new("w7"),
                agent_status: AgentStatus::Blocked,
                agent: Some("claude".to_owned()),
                title: Some("Run tests?".to_owned()),
            }
        );
    }

    #[test]
    fn decodes_tagged_lifecycle_event() {
        let line = r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1:p3","workspace_id":"w1"}}"#;
        assert_eq!(
            decode(line),
            Event::PaneClosed {
                pane_id: PaneId::new("w1:p3"),
                workspace_id: WorkspaceId::new("w1")
            }
        );
    }

    #[test]
    fn unknown_event_kinds_are_other() {
        let tagged = r#"{"event":"layout_updated","data":{"type":"layout_updated","layout":{}}}"#;
        assert_eq!(decode(tagged), Event::Other);
        let untagged = r#"{"event":"pane.output_matched","data":{"pane_id":"w7:p1","matched_line":"x","read":{}}}"#;
        assert_eq!(decode(untagged), Event::Other);
    }
}
