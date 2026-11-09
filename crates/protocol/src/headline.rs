//! One-line headlines of turns (design §6.4). Outbound: tolerant.
//!
//! Produced by a tool-less model from untrusted input: display only.

use serde::{Deserialize, Serialize};

use crate::thread::TurnIndex;

/// Most characters in a headline.
pub const MAX_HEADLINE_CHARS: usize = 100;

/// The headline of one turn: one sentence of at most
/// [`MAX_HEADLINE_CHARS`] characters saying what happened or what the agent
/// needs. Every finished turn gets one, long or short; a turn still in
/// progress gets a provisional one once it has grown enough to say
/// something, replaced as it goes on and by the final one when it
/// finishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnHeadline {
    /// The turn; matches [`ThreadItem::turn`](crate::ThreadItem::turn).
    pub turn: TurnIndex,
    /// The headline text.
    pub text: String,
    /// Made from the turn so far, while it was still in progress: show it
    /// as such ("so far"). Absent on the wire when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub provisional: bool,
}

/// Whether a flag is off, so it can be left off the wire.
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "the signature `skip_serializing_if` calls"
)]
pub(crate) const fn is_false(flag: &bool) -> bool {
    !*flag
}
