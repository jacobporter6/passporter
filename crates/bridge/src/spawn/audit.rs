//! The audit lines of a spawn, in the action audit log (design §8): an
//! `execute` line written and synced **before** herdr is asked for
//! anything, and a `result` line once the request is decided. The first
//! prompt is truncated like an action's; project paths and herdr arguments
//! come from the config and are not repeated.

use protocol::{SpawnRequest, SpawnResult, Timestamp};
use serde::Serialize;

use crate::actions::audit::{Phase, excerpt};

/// One audit line for a spawn.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpawnEntry<'a> {
    at: Timestamp,
    phase: Phase,
    device: &'a str,
    /// Always `"spawn"`: tells these lines from actions'.
    kind: &'static str,
    idempotency_key: &'a str,
    project: &'a str,
    agent_kind: &'static str,
    created_at: Timestamp,
    /// Excerpt of the first prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    /// The new agent, for `result` lines of a spawn that started one.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    /// The result's status (and refusal reason), for `result` lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
}

impl<'a> SpawnEntry<'a> {
    /// A line for `request` by `device`; `result` only for [`Phase::Result`].
    pub(crate) fn new(
        phase: Phase,
        device: &'a str,
        request: &'a SpawnRequest,
        result: Option<&SpawnResult>,
    ) -> Self {
        Self {
            at: crate::registry::now(),
            phase,
            device,
            kind: "spawn",
            idempotency_key: request.idempotency_key.as_str(),
            project: request.project.as_str(),
            agent_kind: request.kind.as_str(),
            created_at: request.created_at,
            prompt: request.prompt.as_ref().map(|p| excerpt(p.as_str())),
            agent_id: match result {
                Some(SpawnResult::Spawned { agent_id, .. }) => Some(agent_id.to_string()),
                _ => None,
            },
            outcome: result.map(outcome),
        }
    }
}

fn outcome(result: &SpawnResult) -> String {
    match result {
        SpawnResult::Spawned {
            replayed,
            prompt_error,
            ..
        } => match (replayed, prompt_error) {
            (true, _) => "spawned (replayed)".to_owned(),
            (false, None) => "spawned".to_owned(),
            (false, Some(_)) => "spawned (prompt not sent)".to_owned(),
        },
        SpawnResult::Refused { reason, .. } => format!("refused: {}", reason.as_str()),
        SpawnResult::Failed { .. } => "failed".to_owned(),
        SpawnResult::Unknown => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use protocol::SpawnRefusal;

    use super::*;

    fn request() -> SpawnRequest {
        serde_json::from_value(serde_json::json!({
            "idempotencyKey": "00000000-0000-4000-8000-000000000001",
            "project": "web",
            "kind": "codex",
            "prompt": "y".repeat(300),
            "createdAt": 5,
        }))
        .expect("valid request")
    }

    #[test]
    fn lines_name_the_project_and_kind_and_cut_the_prompt() {
        let request = request();
        let line = serde_json::to_value(SpawnEntry::new(Phase::Execute, "dev", &request, None))
            .expect("json");
        assert_eq!(line["phase"], "execute");
        assert_eq!(line["kind"], "spawn");
        assert_eq!(line["project"], "web");
        assert_eq!(line["agentKind"], "codex");
        assert_eq!(line["createdAt"], 5);
        let prompt = line["prompt"].as_str().expect("prompt");
        assert!(prompt.chars().count() < 300 && prompt.ends_with('…'));
        assert!(line.get("outcome").is_none());

        let spawned = SpawnResult::Spawned {
            agent_id: "w2:p3".parse().expect("id"),
            replayed: false,
            prompt_error: Some("busy".into()),
        };
        let line = serde_json::to_value(SpawnEntry::new(
            Phase::Result,
            "dev",
            &request,
            Some(&spawned),
        ))
        .expect("json");
        assert_eq!(line["agentId"], "w2:p3");
        assert_eq!(line["outcome"], "spawned (prompt not sent)");

        let refused = SpawnResult::Refused {
            reason: SpawnRefusal::UnknownProject,
            message: String::new(),
        };
        let line = serde_json::to_value(SpawnEntry::new(
            Phase::Result,
            "dev",
            &request,
            Some(&refused),
        ))
        .expect("json");
        assert_eq!(line["outcome"], "refused: unknown_project");
    }
}
