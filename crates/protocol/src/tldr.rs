//! The per-turn summary (`GET /v1/agents/:id/tldr?turn=`, design §6.4).
//! Outbound: tolerant.
//!
//! Produced by a tool-less model from untrusted input: display only.

use serde::{Deserialize, Serialize};

/// A four-part summary of a long turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tldr {
    /// At most two short sentences: did it work?
    pub outcome: String,
    /// Up to 3 bullets: the main files/areas touched, with +/- lines.
    #[serde(default)]
    pub changed: Vec<String>,
    /// What the agent needs from the user, only when a decision is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decide: Option<String>,
    /// Up to 2 real problems flagged or left open (failing tests, skipped
    /// steps); usually empty.
    #[serde(default)]
    pub risks: Vec<String>,
}
