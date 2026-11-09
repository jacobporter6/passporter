//! Errors from talking to herdr.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// A herdr server error code (herdr 0.9.1, protocol 22). Codes Passporter acts
/// on are named; anything else is kept verbatim in [`ErrorCode::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// The agent is waiting at an approval or question dialog; nothing was sent.
    AgentBlocked,
    /// A prompt-with-wait was submitted but no `working`/`blocked` activity
    /// followed. Delivery is *not* disproven: never blindly resend.
    AgentPromptStalled,
    /// Found but not ready for input (e.g. blocked during startup).
    AgentNotReady,
    /// The agent is still launching.
    AgentLaunchPending,
    /// A server-side wait expired.
    Timeout,
    AgentNotFound,
    AgentPaneNotFound,
    PaneNotFound,
    TabNotFound,
    WorkspaceNotFound,
    /// `agent.start` with a name another live agent already has.
    AgentNameTaken,
    /// `agent.start` in a pane that isn't an idle shell.
    AgentPaneUnavailable,
    /// `agent.start` in a pane whose shell is not at its prompt yet (a fresh
    /// pane still running its rc files). Nothing was started: retry.
    AgentPaneBusy,
    AgentTargetAmbiguous,
    InvalidKey,
    EmptyAgentPrompt,
    /// Malformed request, unknown method or bad params.
    InvalidRequest,
    ServerUnavailable,
    InternalError,
    Other(String),
}

const NAMED: &[(&str, ErrorCode)] = &[
    ("agent_blocked", ErrorCode::AgentBlocked),
    ("agent_prompt_stalled", ErrorCode::AgentPromptStalled),
    ("agent_not_ready", ErrorCode::AgentNotReady),
    ("agent_launch_pending", ErrorCode::AgentLaunchPending),
    ("timeout", ErrorCode::Timeout),
    ("agent_not_found", ErrorCode::AgentNotFound),
    ("agent_pane_not_found", ErrorCode::AgentPaneNotFound),
    ("pane_not_found", ErrorCode::PaneNotFound),
    ("tab_not_found", ErrorCode::TabNotFound),
    ("workspace_not_found", ErrorCode::WorkspaceNotFound),
    ("agent_name_taken", ErrorCode::AgentNameTaken),
    ("agent_pane_unavailable", ErrorCode::AgentPaneUnavailable),
    ("agent_pane_busy", ErrorCode::AgentPaneBusy),
    ("agent_target_ambiguous", ErrorCode::AgentTargetAmbiguous),
    ("invalid_key", ErrorCode::InvalidKey),
    ("empty_agent_prompt", ErrorCode::EmptyAgentPrompt),
    ("invalid_request", ErrorCode::InvalidRequest),
    ("server_unavailable", ErrorCode::ServerUnavailable),
    ("internal_error", ErrorCode::InternalError),
];

impl ErrorCode {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Other(code) => code,
            named => NAMED
                .iter()
                .find(|(_, code)| code == named)
                .map_or_else(|| unreachable!("every named code is in NAMED"), |(s, _)| s),
        }
    }

    /// Any of herdr's "no such agent/pane/tab/workspace" codes.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::AgentNotFound
                | Self::AgentPaneNotFound
                | Self::PaneNotFound
                | Self::TabNotFound
                | Self::WorkspaceNotFound
        )
    }
}

impl From<&str> for ErrorCode {
    fn from(code: &str) -> Self {
        NAMED
            .iter()
            .find(|(s, _)| *s == code)
            .map_or_else(|| Self::Other(code.to_owned()), |(_, c)| c.clone())
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("connecting to herdr at {}: {source}", path.display())]
    Connect { path: PathBuf, source: io::Error },

    #[error("herdr socket I/O during {method}: {source}")]
    Io {
        method: &'static str,
        source: io::Error,
    },

    #[error("herdr closed the connection during {method}")]
    Closed { method: &'static str },

    /// The client gave up waiting. `sent` says whether the request had been
    /// written before the deadline hit.
    #[error("herdr {method}: no reply within {after:?}")]
    TimedOut {
        method: &'static str,
        after: Duration,
        sent: bool,
    },

    #[error("decoding herdr reply to {method}: {source}")]
    Decode {
        method: &'static str,
        source: serde_json::Error,
    },

    #[error("herdr replied to {method} with `{got}`, expected `{expected}`")]
    UnexpectedReply {
        method: &'static str,
        expected: &'static str,
        got: String,
    },

    #[error("herdr {method}: path is not valid UTF-8: {}", path.display())]
    NonUtf8Path { method: &'static str, path: PathBuf },

    #[error("herdr {method} failed: {code}: {message}")]
    Server {
        method: &'static str,
        code: ErrorCode,
        message: String,
    },
}

impl Error {
    /// The server error code, if herdr rejected the request.
    #[must_use]
    pub fn code(&self) -> Option<&ErrorCode> {
        match self {
            Self::Server { code, .. } => Some(code),
            _ => None,
        }
    }

    /// True when the request may have taken effect despite the error.
    ///
    /// herdr documents that a timeout or stalled prompt does not prove
    /// non-delivery; a connection lost after sending is equally ambiguous; and
    /// a reply we couldn't decode means herdr *did* process the request.
    /// Callers must inspect state before retrying, never blindly resend.
    #[must_use]
    pub fn may_have_taken_effect(&self) -> bool {
        match self {
            Self::Server { code, .. } => {
                matches!(code, ErrorCode::AgentPromptStalled | ErrorCode::Timeout)
            }
            Self::TimedOut { sent, .. } => *sent,
            Self::Io { .. }
            | Self::Closed { .. }
            | Self::Decode { .. }
            | Self::UnexpectedReply { .. } => true,
            Self::Connect { .. } | Self::NonUtf8Path { .. } => false,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for (s, code) in NAMED {
            assert_eq!(ErrorCode::from(*s), *code);
            assert_eq!(code.as_str(), *s);
        }
        let other = ErrorCode::from("workspace_group_close_required");
        assert_eq!(
            other,
            ErrorCode::Other("workspace_group_close_required".to_owned())
        );
        assert_eq!(other.as_str(), "workspace_group_close_required");
    }

    #[test]
    fn not_found_family() {
        for s in [
            "agent_not_found",
            "pane_not_found",
            "tab_not_found",
            "workspace_not_found",
        ] {
            assert!(ErrorCode::from(s).is_not_found(), "{s}");
        }
        assert!(!ErrorCode::AgentBlocked.is_not_found());
    }

    #[test]
    fn may_have_taken_effect() {
        let server = |code: &str| Error::Server {
            method: "agent.prompt",
            code: code.into(),
            message: String::new(),
        };
        assert!(server("timeout").may_have_taken_effect());
        assert!(server("agent_prompt_stalled").may_have_taken_effect());
        assert!(!server("agent_blocked").may_have_taken_effect());
        assert!(!server("invalid_request").may_have_taken_effect());
        assert!(
            Error::Closed {
                method: "agent.prompt"
            }
            .may_have_taken_effect()
        );
        let decode = serde_json::from_str::<u8>("x").expect_err("bad json");
        assert!(
            Error::Decode {
                method: "agent.prompt",
                source: decode
            }
            .may_have_taken_effect()
        );
        let after = Duration::from_secs(1);
        assert!(
            Error::TimedOut {
                method: "ping",
                after,
                sent: true
            }
            .may_have_taken_effect()
        );
        assert!(
            !Error::TimedOut {
                method: "ping",
                after,
                sent: false
            }
            .may_have_taken_effect()
        );
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert!(
            !Error::Connect {
                path: "/s".into(),
                source: refused
            }
            .may_have_taken_effect()
        );
    }
}
