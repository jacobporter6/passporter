//! Starting a new agent from the phone: `GET /v1/projects` (what may be
//! started) and `POST /v1/agents` (start one).
//!
//! The phone never names a path, a command or a flag. It picks a project
//! by [`ProjectName`] from the bridge's allowlist (`[spawn] projects` in the
//! bridge config) and a [`SpawnKind`]; the bridge maps both to fixed herdr
//! arguments from its config. Wire shape of the request (inbound, strict):
//!
//! ```json
//! {
//!   "idempotencyKey": "9ca09978-ef89-46fd-9a85-118b84bee7b4",
//!   "project": "passporter",
//!   "kind": "claude",
//!   "prompt": "Fix the flaky stream test",
//!   "createdAt": 1762500000000
//! }
//! ```
//!
//! # Outbox contract
//!
//! As for actions ([`crate::result`]): no response means retry the same
//! key (the bridge dedupes, so a retry never starts a second agent); any
//! 2xx is final for the key; a refusal that
//! [`is_transient`](SpawnRefusal::is_transient) may be retried with the
//! same key, any other is final.

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::ids::{AgentId, IdempotencyKey};
use crate::macros::validated_string;
use crate::text::PromptText;
use crate::time::Timestamp;

/// Longest [`ProjectName`], in bytes. Short enough that the bridge can
/// build a herdr agent name (at most 32 characters) from it.
pub const MAX_PROJECT_NAME_LEN: usize = 20;

fn check_project_name(value: &str) -> Result<(), ValidationError> {
    const FIELD: &str = "project";
    if value.is_empty() {
        return Err(ValidationError::Empty { field: FIELD });
    }
    if value.len() > MAX_PROJECT_NAME_LEN {
        return Err(ValidationError::TooLong {
            field: FIELD,
            max: MAX_PROJECT_NAME_LEN,
            actual: value.len(),
        });
    }
    let mut bytes = value.bytes();
    let first_ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase());
    let rest_ok = bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if first_ok && rest_ok {
        Ok(())
    } else {
        Err(ValidationError::InvalidCharacter { field: FIELD })
    }
}

validated_string!(
    /// The name of a project in the bridge's spawn allowlist, e.g.
    /// `passporter`: a lowercase ASCII letter, then lowercase letters,
    /// digits and `-`, at most [`MAX_PROJECT_NAME_LEN`] bytes. Only the name
    /// crosses the wire; its path stays in the bridge config.
    ProjectName,
    check_project_name
);

/// Which agent to start (herdr `agent start --kind`). A closed set: the
/// bridge refuses anything else, and may allow fewer (`[spawn] kinds`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnKind {
    /// Claude Code, in its normal permission mode.
    Claude,
    /// The Codex CLI.
    Codex,
}

impl SpawnKind {
    /// Every kind the protocol knows, in display order.
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    /// herdr's name for the kind (`agent start --kind <name>`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The kind as people say it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }
}

/// The body of `POST /v1/agents`. Inbound: strict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SpawnRequest {
    /// Generated on the device when the user taps Start; a retry reuses it.
    pub idempotency_key: IdempotencyKey,
    /// The project to start in, from [`SpawnOptions::projects`].
    pub project: ProjectName,
    /// The agent to start.
    pub kind: SpawnKind,
    /// The first prompt, sent once the agent is ready for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptText>,
    /// When the user tapped Start (device clock).
    pub created_at: Timestamp,
}

/// The response of `GET /v1/projects`: what this device may start.
/// Outbound: tolerant (names and kinds this app cannot parse are skipped).
///
/// Parsed through a flat list of strings rather than the generic
/// [`Lenient`](crate::Lenient) wrapper: the same tolerance, at a fraction
/// of the WASM size (the app's budget).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "SpawnOptionsWire")]
pub struct SpawnOptions {
    /// The allowlisted projects, in config order. Empty when the bridge has
    /// no `[spawn]` table: starting agents is off.
    pub projects: Vec<ProjectName>,
    /// The kinds the bridge allows, in display order.
    pub kinds: Vec<SpawnKind>,
}

#[derive(Deserialize)]
struct SpawnOptionsWire {
    #[serde(default)]
    projects: Vec<String>,
    #[serde(default)]
    kinds: Vec<String>,
}

impl From<SpawnOptionsWire> for SpawnOptions {
    fn from(wire: SpawnOptionsWire) -> Self {
        Self {
            projects: wire
                .projects
                .into_iter()
                .filter_map(|name| ProjectName::new(name).ok())
                .collect(),
            kinds: wire
                .kinds
                .iter()
                .filter_map(|kind| SpawnKind::ALL.into_iter().find(|k| k.as_str() == kind))
                .collect(),
        }
    }
}

/// What happened to a [`SpawnRequest`], tagged by `"status"`. Outbound:
/// tolerant: a status this app predates, or a `spawned` whose agent id it
/// cannot parse, is [`SpawnResult::Unknown`]; a missing reason is
/// [`SpawnRefusal::Other`].
///
/// Parsed through a flat struct rather than serde's internally tagged
/// enum: the same wire shape, at a fraction of the WASM size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    from = "SpawnResultWire"
)]
pub enum SpawnResult {
    /// The agent is running; open it.
    Spawned {
        /// The new agent.
        agent_id: AgentId,
        /// True when this is the stored result of an earlier identical
        /// request: nothing was started this time.
        #[serde(default)]
        replayed: bool,
        /// Set when a first prompt was given but not sent (the agent asked
        /// something first, or was not ready in time): why, for the user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_error: Option<String>,
    },
    /// The bridge refused. Nothing was started.
    Refused {
        /// Machine-readable reason.
        reason: SpawnRefusal,
        /// Human-readable text. Empty if a bridge omits it.
        #[serde(default)]
        message: String,
    },
    /// herdr failed. Final for this key: whether a tab or agent was left
    /// behind is uncertain, so look in herdr before trying again.
    Failed {
        /// Human-readable error. Empty if a bridge omits it.
        #[serde(default)]
        message: String,
    },
    /// A status this app predates; treat as failed.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpawnResultWire {
    status: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    replayed: bool,
    #[serde(default)]
    prompt_error: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl From<SpawnResultWire> for SpawnResult {
    fn from(wire: SpawnResultWire) -> Self {
        let message = wire.message.unwrap_or_default();
        match wire.status.as_str() {
            "spawned" => match wire.agent_id.and_then(|id| id.parse().ok()) {
                Some(agent_id) => Self::Spawned {
                    agent_id,
                    replayed: wire.replayed,
                    prompt_error: wire.prompt_error,
                },
                None => Self::Unknown,
            },
            "refused" => Self::Refused {
                reason: wire
                    .reason
                    .as_deref()
                    .map_or(SpawnRefusal::Other, SpawnRefusal::from_wire),
                message,
            },
            "failed" => Self::Failed { message },
            _ => Self::Unknown,
        }
    }
}

/// Why a spawn was [`SpawnResult::Refused`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnRefusal {
    /// The bridge has no `[spawn]` projects: starting agents is off.
    Disabled,
    /// The project is not in the allowlist. Permanent.
    UnknownProject,
    /// The bridge does not allow this kind. Permanent.
    UnknownKind,
    /// The first prompt needs step-up (starts with `!`, `/` or `#`).
    /// Transient: retry the same key after step-up.
    StepUpRequired,
    /// Per-device rate limit hit. Transient: retry the same key later.
    RateLimited,
    /// The idempotency key was already used for a different request.
    IdempotencyConflict,
    /// The body failed validation. Permanent.
    InvalidRequest,
    /// A reason this app predates. Treated as permanent.
    #[serde(other)]
    Other,
}

impl SpawnRefusal {
    /// Every known reason.
    pub const ALL: [Self; 7] = [
        Self::Disabled,
        Self::UnknownProject,
        Self::UnknownKind,
        Self::StepUpRequired,
        Self::RateLimited,
        Self::IdempotencyConflict,
        Self::InvalidRequest,
    ];

    /// The reason's wire name, as serde writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::UnknownProject => "unknown_project",
            Self::UnknownKind => "unknown_kind",
            Self::StepUpRequired => "step_up_required",
            Self::RateLimited => "rate_limited",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::InvalidRequest => "invalid_request",
            Self::Other => "other",
        }
    }

    /// A wire name back to its reason, [`Self::Other`] for one this app
    /// predates. [`SpawnResult`] parses reasons with this rather than
    /// serde's enum visitor (the WASM size budget).
    #[must_use]
    pub fn from_wire(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == name)
            .unwrap_or(Self::Other)
    }

    /// Whether the same idempotency key may be retried: the bridge does not
    /// record transient refusals against the key.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::StepUpRequired | Self::RateLimited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";

    fn request(extra: &str) -> Result<SpawnRequest, serde_json::Error> {
        serde_json::from_str(&format!(
            r#"{{"idempotencyKey":"{KEY}","project":"passporter","kind":"claude",
                "createdAt":1762500000000{extra}}}"#
        ))
    }

    #[test]
    fn project_names_are_short_lowercase_slugs() {
        for ok in ["p", "passporter", "web-2", "a2345678901234567890"] {
            assert!(ProjectName::new(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "2fast",
            "-x",
            "Passporter",
            "../etc",
            "a b",
            "a_b",
            "a/b",
            "a23456789012345678901",
        ] {
            assert!(ProjectName::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn requests_are_strict() {
        let parsed = request(r#","prompt":"go""#).unwrap();
        assert_eq!(parsed.kind, SpawnKind::Claude);
        assert_eq!(parsed.prompt.map(String::from), Some("go".into()));
        assert!(request("").unwrap().prompt.is_none());
        // No paths, flags or other kinds from the client.
        assert!(request(r#","path":"/etc""#).is_err());
        assert!(request(r#","args":["--dangerously-skip-permissions"]"#).is_err());
        let pi = format!(
            r#"{{"idempotencyKey":"{KEY}","project":"passporter","kind":"pi","createdAt":0}}"#
        );
        assert!(serde_json::from_str::<SpawnRequest>(&pi).is_err());
        let path = format!(
            r#"{{"idempotencyKey":"{KEY}","project":"/home","kind":"claude","createdAt":0}}"#
        );
        assert!(serde_json::from_str::<SpawnRequest>(&path).is_err());
        // A blank prompt is no prompt, not an empty one.
        assert!(request(r#","prompt":"  ""#).is_err());
    }

    #[test]
    fn options_skip_what_this_app_cannot_parse() {
        let options: SpawnOptions =
            serde_json::from_str(r#"{"projects":["web","Bad Name"],"kinds":["codex","pi"]}"#)
                .unwrap();
        assert_eq!(options.projects, [ProjectName::new("web").unwrap()]);
        assert_eq!(options.kinds, [SpawnKind::Codex]);
        assert_eq!(
            serde_json::from_str::<SpawnOptions>("{}").unwrap(),
            SpawnOptions::default()
        );
    }

    #[test]
    fn results_are_tolerant() {
        assert_eq!(
            serde_json::from_str::<SpawnResult>(r#"{"status":"queued"}"#).unwrap(),
            SpawnResult::Unknown
        );
        assert_eq!(
            serde_json::from_str::<SpawnResult>(r#"{"status":"refused","reason":"moon"}"#).unwrap(),
            SpawnResult::Refused {
                reason: SpawnRefusal::Other,
                message: String::new()
            }
        );
        let spawned: SpawnResult =
            serde_json::from_str(r#"{"status":"spawned","agentId":"w3:p4"}"#).unwrap();
        assert_eq!(
            spawned,
            SpawnResult::Spawned {
                agent_id: "w3:p4".parse().unwrap(),
                replayed: false,
                prompt_error: None
            }
        );
    }

    #[test]
    fn unparseable_agent_ids_and_missing_reasons_stay_readable() {
        assert_eq!(
            serde_json::from_str::<SpawnResult>(r#"{"status":"spawned","agentId":"w1/p1"}"#)
                .unwrap(),
            SpawnResult::Unknown
        );
        assert_eq!(
            serde_json::from_str::<SpawnResult>(r#"{"status":"refused"}"#).unwrap(),
            SpawnResult::Refused {
                reason: SpawnRefusal::Other,
                message: String::new()
            }
        );
        assert!(serde_json::from_str::<SpawnResult>(r#"{"agentId":"w1:p1"}"#).is_err());
    }

    #[test]
    fn wire_names_match_serde() {
        for reason in SpawnRefusal::ALL.into_iter().chain([SpawnRefusal::Other]) {
            let serde = serde_json::to_value(reason).unwrap();
            assert_eq!(serde, reason.as_str(), "{reason:?}");
            assert_eq!(SpawnRefusal::from_wire(reason.as_str()), reason);
        }
        assert_eq!(SpawnRefusal::from_wire("moon"), SpawnRefusal::Other);
    }

    #[test]
    fn transient_refusals() {
        assert!(SpawnRefusal::RateLimited.is_transient());
        assert!(SpawnRefusal::StepUpRequired.is_transient());
        for permanent in [
            SpawnRefusal::Disabled,
            SpawnRefusal::UnknownProject,
            SpawnRefusal::UnknownKind,
            SpawnRefusal::IdempotencyConflict,
            SpawnRefusal::InvalidRequest,
            SpawnRefusal::Other,
        ] {
            assert!(!permanent.is_transient(), "{permanent:?}");
        }
    }
}
