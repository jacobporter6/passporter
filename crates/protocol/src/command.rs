//! Commands an agent asks the user to run (terminal mode):
//! `GET /v1/agents/:id/commands`, `POST /v1/agents/:id/commands/run`, and
//! the terminals they start (`GET /v1/terminals/:pane/screen`,
//! `POST /v1/terminals/:pane/keys`).
//!
//! The bridge extracts every command candidate from the agent's latest
//! message, deterministically (no model decides what runs), and lists them
//! all, in order: when the agent offers "run this, or this", both show and
//! the user picks. A run names one candidate by its [`id`](CommandCandidate::id)
//! and [`hash`](CommandCandidate::hash); the bridge extracts again when the
//! run arrives and runs nothing unless the same candidate is still there,
//! so a stale or forged command never runs. There is deliberately no field
//! for free-form command text.
//!
//! Wire shape of a run (inbound, strict):
//!
//! ```json
//! {
//!   "idempotencyKey": "9ca09978-ef89-46fd-9a85-118b84bee7b4",
//!   "origin": "app",
//!   "createdAt": 1762500000000,
//!   "candidate": 0,
//!   "hash": "0f1e2d3c4b5a69788796a5b4c3d2e1f0",
//!   "target": "here"
//! }
//! ```
//!
//! # Outbox contract
//!
//! As for runbooks ([`crate::runbook`]): no response means retry the same
//! key; any 2xx is final for the key; a refusal that
//! [`is_transient`](CommandRefusal::is_transient) may be retried with the
//! same key, any other is final.

use serde::{Deserialize, Serialize};

use crate::action::Origin;
use crate::error::ValidationError;
use crate::ids::{IdempotencyKey, PaneId};
use crate::keys::KeySequence;
use crate::macros::validated_string;
use crate::time::Timestamp;

/// Longest command candidate, in characters (a fenced block counts whole).
pub const MAX_COMMAND_CHARS: usize = 2000;

/// Most lines in a (fenced, multi-line) command candidate.
pub const MAX_COMMAND_LINES: usize = 40;

/// Most candidates listed for one message; the rest are counted in
/// [`CommandList::omitted`].
pub const MAX_CANDIDATES: usize = 12;

/// Length of a [`CommandHash`], in hex digits.
pub const COMMAND_HASH_LEN: usize = 32;

fn check_hash(value: &str) -> Result<(), ValidationError> {
    const FIELD: &str = "hash";
    if value.is_empty() {
        return Err(ValidationError::Empty { field: FIELD });
    }
    if value.len() != COMMAND_HASH_LEN {
        return Err(ValidationError::TooLong {
            field: FIELD,
            max: COMMAND_HASH_LEN,
            actual: value.len(),
        });
    }
    if value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(ValidationError::InvalidCharacter { field: FIELD })
    }
}

validated_string!(
    /// A candidate's content hash: [`COMMAND_HASH_LEN`] lowercase hex
    /// digits. The bridge hashes the whole message with the candidate's
    /// position and text, so any change to the message makes it stale.
    CommandHash,
    check_hash
);

/// Where the agent's wording says to run a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandIntent {
    /// In the agent's own session, so the output lands in its context: a
    /// `!` prefix, or "run this here" / "in this session".
    InSession,
    /// In a terminal of its own: "in another terminal", "separately".
    NewTerminal,
    /// The wording does not say.
    Unspecified,
}

impl CommandIntent {
    /// The intent's wire name, as serde writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InSession => "in_session",
            Self::NewTerminal => "new_terminal",
            Self::Unspecified => "unspecified",
        }
    }

    /// A wire name back to its intent; one this app predates is
    /// [`Self::Unspecified`].
    #[must_use]
    pub fn from_wire(name: &str) -> Self {
        match name {
            "in_session" => Self::InSession,
            "new_terminal" => Self::NewTerminal,
            _ => Self::Unspecified,
        }
    }
}

/// Where in the message a candidate was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandSource {
    /// A fenced code block (shown whole, possibly several lines).
    Block,
    /// An inline code span.
    Inline,
    /// A line of prose starting with `! `.
    BangLine,
    /// A source this app predates.
    Unknown,
}

impl CommandSource {
    /// The source's wire name, as serde writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Inline => "inline",
            Self::BangLine => "bang_line",
            Self::Unknown => "unknown",
        }
    }

    /// A wire name back to its source, [`Self::Unknown`] for one this app
    /// predates.
    #[must_use]
    pub fn from_wire(name: &str) -> Self {
        match name {
            "block" => Self::Block,
            "inline" => Self::Inline,
            "bang_line" => Self::BangLine,
            _ => Self::Unknown,
        }
    }
}

/// One command the agent's latest message suggests running. Outbound:
/// tolerant (see [`CommandList`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandCandidate {
    /// Its position among the message's candidates, from 0.
    pub id: u16,
    /// Binds a run to this exact candidate of this exact message.
    pub hash: CommandHash,
    /// The exact text that runs (without a `!` prefix), cleaned of
    /// control and hidden characters. Untrusted: show as plain text.
    pub text: String,
    /// What the wording around it says.
    pub intent: CommandIntent,
    /// Where it was found.
    pub source: CommandSource,
}

impl CommandCandidate {
    /// Whether it spans several lines (only a fenced block can): such a
    /// command runs only in a new terminal.
    #[must_use]
    pub fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }
}

/// `GET /v1/agents/:id/commands`. Outbound: tolerant: a candidate whose
/// hash this app cannot parse is skipped, unknown enum values fall back.
/// Decoded through flat wire structs, like [`crate::runbook`], for the
/// app's WASM budget.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ListWire")]
pub struct CommandList {
    /// Every candidate of the agent's latest message, in message order;
    /// empty when there are none.
    pub candidates: Vec<CommandCandidate>,
    /// Candidates past [`MAX_CANDIDATES`], not listed.
    pub omitted: u32,
}

#[derive(Deserialize)]
struct ListWire {
    #[serde(default)]
    candidates: Vec<CandidateWire>,
    #[serde(default)]
    omitted: u32,
}

#[derive(Deserialize)]
struct CandidateWire {
    #[serde(default)]
    id: u16,
    #[serde(default)]
    hash: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    intent: String,
    #[serde(default)]
    source: String,
}

impl From<ListWire> for CommandList {
    fn from(wire: ListWire) -> Self {
        let candidates = wire
            .candidates
            .into_iter()
            .filter_map(|c| {
                Some(CommandCandidate {
                    id: c.id,
                    hash: CommandHash::new(c.hash).ok()?,
                    text: c.text,
                    intent: CommandIntent::from_wire(&c.intent),
                    source: CommandSource::from_wire(&c.source),
                })
            })
            .filter(|c| !c.text.trim().is_empty())
            .collect();
        Self {
            candidates,
            omitted: wire.omitted,
        }
    }
}

/// Where a confirmed command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTarget {
    /// Typed as `!<command>` into the agent's own prompt: Claude Code and
    /// Codex run it as a shell command in the session, and the output lands
    /// in the agent's context. Single-line commands only.
    Here,
    /// Typed into the shell of a new tab in the spawn workspace, in the
    /// agent's working directory.
    NewTerminal,
}

/// The body of `POST /v1/agents/:id/commands/run`. Inbound: strict. The
/// command is named, never sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandRunRequest {
    /// Generated on the device when the user confirms; a retry reuses it.
    pub idempotency_key: IdempotencyKey,
    /// Where the request came from. The bridge refuses `car` outright.
    pub origin: Origin,
    /// When the user confirmed (device clock).
    pub created_at: Timestamp,
    /// The candidate's [`id`](CommandCandidate::id).
    pub candidate: u16,
    /// The candidate's [`hash`](CommandCandidate::hash).
    pub hash: CommandHash,
    /// Where to run it.
    pub target: RunTarget,
}

/// The body of `POST /v1/terminals/:pane/keys`: allowlisted keys for a
/// terminal a command run opened. Inbound: strict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalKeysRequest {
    /// Generated on the device per tap; a retry reuses it.
    pub idempotency_key: IdempotencyKey,
    /// Where the request came from. The bridge refuses `car` outright.
    pub origin: Origin,
    /// When the user tapped (device clock).
    pub created_at: Timestamp,
    /// The keys, in order.
    pub keys: KeySequence,
}

/// A terminal a command run opened. Outbound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Terminal {
    /// Its herdr pane: what `GET /v1/terminals/:pane/screen` takes.
    pub pane: PaneId,
    /// Its tab's label in herdr, e.g. `run: api-fix`.
    pub label: String,
}

/// `GET /v1/terminals/:pane/screen`: the terminal's last lines. Outbound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalScreen {
    /// The last lines, ANSI stripped, at most
    /// [`MAX_SCREEN_LINES`](crate::MAX_SCREEN_LINES). Untrusted: display as
    /// plain text.
    pub text: String,
    /// herdr's pane revision for this read; changes with the screen.
    pub revision: u64,
}

/// What happened to a [`CommandRunRequest`] or a [`TerminalKeysRequest`],
/// tagged by `"status"`. Outbound: tolerant, through a flat wire struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    from = "ResultWire"
)]
pub enum CommandResult {
    /// Typed into the agent's prompt (or the keys into the terminal).
    Sent {
        /// True when this is the stored result of an earlier identical
        /// request: nothing was sent this time.
        replayed: bool,
    },
    /// A new terminal opened and the command was typed into its shell.
    Started {
        /// The terminal, to watch.
        terminal: Terminal,
        /// As for [`Self::Sent`].
        replayed: bool,
    },
    /// The bridge refused. Nothing ran.
    Refused {
        /// Machine-readable reason.
        reason: CommandRefusal,
        /// Human-readable text.
        message: String,
    },
    /// herdr failed after the bridge decided to run it. Final for this
    /// key: look in herdr before trying again.
    Failed {
        /// Human-readable error.
        message: String,
    },
    /// A status this app predates; treat as failed.
    Unknown,
}

#[derive(Deserialize)]
struct ResultWire {
    #[serde(default)]
    status: String,
    #[serde(default)]
    replayed: bool,
    #[serde(default)]
    terminal: Option<Terminal>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl From<ResultWire> for CommandResult {
    fn from(wire: ResultWire) -> Self {
        let message = wire.message.unwrap_or_default();
        match (wire.status.as_str(), wire.terminal) {
            ("sent", _) => Self::Sent {
                replayed: wire.replayed,
            },
            ("started", Some(terminal)) => Self::Started {
                terminal,
                replayed: wire.replayed,
            },
            ("refused", _) => Self::Refused {
                reason: wire
                    .reason
                    .as_deref()
                    .map_or(CommandRefusal::Other, CommandRefusal::from_wire),
                message,
            },
            ("failed", _) => Self::Failed { message },
            _ => Self::Unknown,
        }
    }
}

/// Why a run was [`CommandResult::Refused`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandRefusal {
    /// The candidate is no longer in the agent's latest message (the
    /// message changed, or the id and hash never matched). Permanent:
    /// fetch the list again.
    Stale,
    /// The agent is not at its prompt (working, or waiting at a dialog), so
    /// nothing can be typed into it. Permanent for this key.
    AgentBusy,
    /// That candidate cannot run there: a multi-line command never runs in
    /// the session. Permanent.
    NotHere,
    /// No such agent, or no such terminal. Permanent.
    NotFound,
    /// Commands never run from the car. Permanent.
    CarForbidden,
    /// Per-device rate limit hit. Transient: retry the same key later.
    RateLimited,
    /// The idempotency key was already used for a different request.
    IdempotencyConflict,
    /// The body failed validation. Permanent.
    InvalidRequest,
    /// A reason this app predates. Treated as permanent.
    Other,
}

impl CommandRefusal {
    /// The reason's wire name, as serde writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stale => "stale",
            Self::AgentBusy => "agent_busy",
            Self::NotHere => "not_here",
            Self::NotFound => "not_found",
            Self::CarForbidden => "car_forbidden",
            Self::RateLimited => "rate_limited",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::InvalidRequest => "invalid_request",
            Self::Other => "other",
        }
    }

    /// A wire name back to its reason, [`Self::Other`] for one this app
    /// predates.
    #[must_use]
    pub fn from_wire(name: &str) -> Self {
        [
            Self::Stale,
            Self::AgentBusy,
            Self::NotHere,
            Self::NotFound,
            Self::CarForbidden,
            Self::RateLimited,
            Self::IdempotencyConflict,
            Self::InvalidRequest,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == name)
        .unwrap_or(Self::Other)
    }

    /// Whether the same idempotency key may be retried: the bridge does not
    /// record transient refusals against the key.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::RateLimited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";
    const HASH: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    #[test]
    fn hashes_are_fixed_length_lowercase_hex() {
        assert!(CommandHash::new(HASH).is_ok());
        for bad in [
            "",
            "0F1E2D3C4B5A69788796A5B4C3D2E1F0",
            "abc",
            &"g".repeat(32),
        ] {
            assert!(CommandHash::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn run_requests_are_strict_and_carry_no_command_text() {
        let ok = serde_json::json!({
            "idempotencyKey": KEY, "origin": "app", "createdAt": 1,
            "candidate": 2, "hash": HASH, "target": "new_terminal",
        });
        let request: CommandRunRequest = serde_json::from_value(ok.clone()).expect("valid");
        assert_eq!(request.candidate, 2);
        assert_eq!(request.target, RunTarget::NewTerminal);
        for (field, value) in [
            ("text", serde_json::json!("rm -rf ~")),
            ("command", serde_json::json!("ls")),
            ("cwd", serde_json::json!("/")),
        ] {
            let mut extra = ok.clone();
            extra[field] = value;
            assert!(
                serde_json::from_value::<CommandRunRequest>(extra).is_err(),
                "{field}"
            );
        }
        let mut target = ok;
        target["target"] = serde_json::json!("elsewhere");
        assert!(serde_json::from_value::<CommandRunRequest>(target).is_err());
    }

    #[test]
    fn lists_decode_tolerantly() {
        let list: CommandList = serde_json::from_value(serde_json::json!({
            "candidates": [
                {"id": 0, "hash": HASH, "text": "gcloud auth login", "intent": "in_session", "source": "inline"},
                {"id": 1, "hash": "nope", "text": "skipped"},
                {"id": 2, "hash": HASH, "text": "npm run dev", "intent": "teleport", "source": "hologram", "extra": 1},
            ],
            "omitted": 3,
        }))
        .expect("list");
        assert_eq!(list.candidates.len(), 2);
        assert_eq!(list.candidates[0].intent, CommandIntent::InSession);
        assert_eq!(list.candidates[1].intent, CommandIntent::Unspecified);
        assert_eq!(list.candidates[1].source, CommandSource::Unknown);
        assert_eq!(list.omitted, 3);
        let back: CommandList =
            serde_json::from_value(serde_json::to_value(&list).expect("ser")).expect("de");
        assert_eq!(back, list);
    }

    #[test]
    fn results_round_trip_and_tolerate_the_unknown() {
        let started = CommandResult::Started {
            terminal: Terminal {
                pane: PaneId::new("w9:p1").expect("pane"),
                label: "run: api".into(),
            },
            replayed: false,
        };
        for result in [
            CommandResult::Sent { replayed: true },
            started,
            CommandResult::Refused {
                reason: CommandRefusal::Stale,
                message: "gone".into(),
            },
            CommandResult::Failed {
                message: "herdr".into(),
            },
        ] {
            let json = serde_json::to_value(&result).expect("ser");
            assert_eq!(
                serde_json::from_value::<CommandResult>(json).expect("de"),
                result
            );
        }
        let odd: CommandResult =
            serde_json::from_str(r#"{"status":"refused","reason":"why_not","message":"m"}"#)
                .expect("de");
        assert!(matches!(
            odd,
            CommandResult::Refused {
                reason: CommandRefusal::Other,
                ..
            }
        ));
        let future: CommandResult = serde_json::from_str(r#"{"status":"queued"}"#).expect("de");
        assert_eq!(future, CommandResult::Unknown);
        assert!(CommandRefusal::RateLimited.is_transient());
        assert!(!CommandRefusal::Stale.is_transient());
        for reason in [
            CommandRefusal::Stale,
            CommandRefusal::AgentBusy,
            CommandRefusal::NotHere,
            CommandRefusal::CarForbidden,
        ] {
            assert_eq!(CommandRefusal::from_wire(reason.as_str()), reason);
        }
    }

    #[test]
    fn multiline_is_a_property_of_the_text() {
        let candidate = |text: &str| CommandCandidate {
            id: 0,
            hash: CommandHash::new(HASH).expect("hash"),
            text: text.into(),
            intent: CommandIntent::Unspecified,
            source: CommandSource::Block,
        };
        assert!(!candidate("make").is_multiline());
        assert!(candidate("npm ci\nnpm test").is_multiline());
    }

    #[test]
    fn terminal_keys_are_allowlisted() {
        let ok = serde_json::json!({
            "idempotencyKey": KEY, "origin": "app", "createdAt": 1, "keys": ["ctrl+c"],
        });
        assert!(serde_json::from_value::<TerminalKeysRequest>(ok).is_ok());
        let typed = serde_json::json!({
            "idempotencyKey": KEY, "origin": "app", "createdAt": 1, "keys": ["rm -rf ~"],
        });
        assert!(serde_json::from_value::<TerminalKeysRequest>(typed).is_err());
    }
}
