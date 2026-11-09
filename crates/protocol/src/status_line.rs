//! Live status lines of working agents (design §6.1, §6.4). Outbound:
//! tolerant.
//!
//! Produced by a tool-less model from the tail of the turn in progress, so
//! a long-running agent that rarely finishes a turn still says what it is
//! doing. Untrusted input: display only.

use serde::{Deserialize, Serialize};

use crate::time::Timestamp;

/// Most characters in a status line.
pub const MAX_STATUS_LINE_CHARS: usize = 80;

/// What a working agent is doing now, and what it is waiting on if
/// anything: one line of at most [`MAX_STATUS_LINE_CHARS`] characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusLine {
    /// The line.
    pub text: String,
    /// When it was made; the client shows its age ("2 min ago") against
    /// its own clock.
    pub at: Timestamp,
}
