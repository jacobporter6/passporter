//! The visible terminal tail of an agent, and stream authentication.
//!
//! `AgentScreen` backs the keypad fallback (design §6.2): when no structured
//! pending input exists, the phone shows the last lines of the agent's screen
//! and lets the user answer with allowed keys. The text is untrusted agent
//! output: render it as plain text, never as markup.

use serde::{Deserialize, Serialize};

use crate::agent::AgentStatus;
use crate::ids::Seq;

/// Maximum lines the bridge sends in an [`AgentScreen`].
pub const MAX_SCREEN_LINES: usize = 60;

/// `GET /v1/agents/:id/screen`: the agent's visible terminal tail.
///
/// Outbound (bridge → app), so tolerant of unknown fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentScreen {
    /// The last lines of the screen, ANSI stripped, at most
    /// [`MAX_SCREEN_LINES`] lines. Untrusted: display as plain text.
    pub text: String,
    /// herdr's pane revision for this read; changes when the screen changes.
    pub revision: u64,
    /// The agent's status when the screen was read.
    pub status: AgentStatus,
    /// The agent's `state_change_seq` at read time, for `expectedSeq`.
    pub seq: Seq,
}

/// The first message a client sends on `WS /v1/stream`.
///
/// Browsers can't set headers on a WebSocket, so the bearer token travels in
/// this message instead; the bridge closes the socket if it doesn't arrive
/// (and verify) promptly. Inbound, so strict.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamAuth {
    /// The bearer token.
    pub token: String,
}

impl std::fmt::Debug for StreamAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamAuth")
            .field("token", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_round_trips_camel_case() {
        let screen = AgentScreen {
            text: "Run tests?\n1. Yes\n2. No".to_owned(),
            revision: 7,
            status: AgentStatus::Blocked,
            seq: Seq(12),
        };
        let json = serde_json::to_value(&screen).expect("ser");
        assert_eq!(json["revision"], 7);
        assert_eq!(json["seq"], 12);
        assert_eq!(json["status"], "blocked");
        let back: AgentScreen = serde_json::from_value(json).expect("de");
        assert_eq!(back, screen);
    }

    #[test]
    fn stream_auth_is_strict_and_redacted() {
        let auth: StreamAuth = serde_json::from_str(r#"{"token":"abc"}"#).expect("de");
        assert_eq!(auth.token, "abc");
        assert!(serde_json::from_str::<StreamAuth>(r#"{"token":"abc","x":1}"#).is_err());
        assert!(!format!("{auth:?}").contains("abc"));
    }
}
