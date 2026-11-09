//! Pure request building: which method, path, headers and body each bridge
//! endpoint takes (design §5). Nothing here touches the network, so it is
//! tested on the host.

use protocol::{
    Action, AgentId, CommandRunRequest, PaneId, RunbookName, RunbookRunRequest, SpawnRequest,
    TerminalKeysRequest, ThreadCursor, TurnIndex,
};

use super::auth::BearerToken;

/// An HTTP method the bridge API uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Reads.
    Get,
    /// `POST`: actions, pairing, push registration, declaring a dictation clip.
    Post,
    /// `PATCH /v1/audio/:id`: one chunk of a dictation clip.
    Patch,
    /// Push subscription removal.
    Delete,
}

impl Method {
    /// The method name as `fetch` expects it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// A fully built request, relative to the app's own origin (the bridge
/// serves the app, and the CSP allows `connect-src 'self'` only).
///
/// The bearer token stays a typed, `Debug`-redacted [`BearerToken`] until
/// the transport turns it into an `Authorization` header, so logging a
/// request can never leak it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// The method.
    pub method: Method,
    /// Absolute path plus query, e.g. `/v1/agents/w1:p1/thread?since=4`.
    pub path: String,
    /// Fixed header names and values (never secrets).
    pub headers: Vec<(&'static str, &'static str)>,
    /// The token to send as `Authorization: Bearer`, if any.
    pub auth: Option<BearerToken>,
    /// JSON body, for `POST` and `DELETE`.
    pub body: Option<String>,
}

/// A bridge read endpoint. Each variant knows its path; the response type is
/// fixed by the matching [`ApiClient`](super::ApiClient) method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// `GET /v1/agents`.
    Agents,
    /// `GET /v1/agents/:id/thread`, optionally only items after `since`,
    /// at most `limit` per page.
    Thread {
        /// The agent.
        agent: AgentId,
        /// The resume cursor of the last page or event.
        since: Option<ThreadCursor>,
        /// Page size; the bridge picks one when absent.
        limit: Option<u32>,
    },
    /// `GET /v1/agents/:id/screen`.
    Screen(AgentId),
    /// `GET /v1/agents/:id/pending`.
    Pending(AgentId),
    /// `GET /v1/agents/:id/tldr`, optionally for a given turn.
    Tldr {
        /// The agent.
        agent: AgentId,
        /// The turn; the bridge defaults to the latest.
        turn: Option<TurnIndex>,
    },
    /// `GET /v1/push/key`: the bridge's VAPID public key.
    PushKey,
    /// `GET /v1/push/subscriptions`: this device's subscriptions.
    PushSubscriptions,
    /// `GET /v1/agents/:id/suggestions`.
    Suggestions(AgentId),
    /// `GET /v1/version`: the bridge's build (unauthenticated).
    Version,
    /// `GET /v1/projects`: what this device may start.
    Projects,
    /// `GET /v1/runbooks`: what this device may run.
    Runbooks,
    /// `GET /v1/agents/:id/commands`: the commands in the agent's latest
    /// message.
    Commands(AgentId),
    /// `GET /v1/terminals/:pane/screen`: a command terminal's last lines.
    TerminalScreen(PaneId),
}

impl Endpoint {
    /// The path and query. [`AgentId`]'s charset is URL-path safe, so ids go
    /// in unescaped.
    #[must_use]
    pub fn path(&self) -> String {
        match self {
            Self::Agents => "/v1/agents".to_owned(),
            Self::Thread {
                agent,
                since,
                limit,
            } => {
                let mut query = Vec::new();
                if let Some(cursor) = since {
                    query.push(format!("since={}", cursor.0));
                }
                if let Some(limit) = limit {
                    query.push(format!("limit={limit}"));
                }
                if query.is_empty() {
                    format!("/v1/agents/{agent}/thread")
                } else {
                    format!("/v1/agents/{agent}/thread?{}", query.join("&"))
                }
            }
            Self::Screen(agent) => format!("/v1/agents/{agent}/screen"),
            Self::Pending(agent) => format!("/v1/agents/{agent}/pending"),
            Self::Suggestions(agent) => format!("/v1/agents/{agent}/suggestions"),
            Self::Tldr { agent, turn } => match turn {
                Some(turn) => format!("/v1/agents/{agent}/tldr?turn={}", turn.0),
                None => format!("/v1/agents/{agent}/tldr"),
            },
            Self::PushKey => "/v1/push/key".to_owned(),
            Self::PushSubscriptions => PUSH_SUBSCRIPTIONS_PATH.to_owned(),
            Self::Version => "/v1/version".to_owned(),
            Self::Projects => "/v1/projects".to_owned(),
            Self::Runbooks => "/v1/runbooks".to_owned(),
            Self::Commands(agent) => format!("/v1/agents/{agent}/commands"),
            Self::TerminalScreen(pane) => format!("/v1/terminals/{pane}/screen"),
        }
    }
}

/// The path of the action endpoint.
pub const ACTIONS_PATH: &str = "/v1/actions";

/// Builds a `GET` for `endpoint`.
#[must_use]
pub fn get(endpoint: &Endpoint, auth: Option<BearerToken>) -> HttpRequest {
    HttpRequest {
        method: Method::Get,
        path: endpoint.path(),
        headers: vec![ACCEPT_JSON],
        auth,
        body: None,
    }
}

/// Builds the `POST /v1/actions` for `action`.
pub fn post_action(
    action: &Action,
    auth: Option<BearerToken>,
) -> Result<HttpRequest, serde_json::Error> {
    Ok(HttpRequest {
        method: Method::Post,
        path: ACTIONS_PATH.to_owned(),
        headers: vec![ACCEPT_JSON, CONTENT_JSON],
        auth,
        body: Some(serde_json::to_string(action)?),
    })
}

/// The path that starts a new agent.
pub const SPAWN_PATH: &str = "/v1/agents";

/// Builds the `POST /v1/agents` that starts the agent `request` asks for.
pub fn post_spawn(
    request: &SpawnRequest,
    auth: Option<BearerToken>,
) -> Result<HttpRequest, serde_json::Error> {
    Ok(HttpRequest {
        method: Method::Post,
        path: SPAWN_PATH.to_owned(),
        headers: vec![ACCEPT_JSON, CONTENT_JSON],
        auth,
        body: Some(serde_json::to_string(request)?),
    })
}

/// Builds the `POST /v1/runbooks/:name/run` that runs `name`. The body
/// carries only the key, origin and time: there is nowhere to put an
/// argument.
pub fn post_runbook_run(
    name: &RunbookName,
    request: &RunbookRunRequest,
    auth: Option<BearerToken>,
) -> Result<HttpRequest, serde_json::Error> {
    Ok(HttpRequest {
        method: Method::Post,
        path: format!("/v1/runbooks/{}/run", path_segment(name.as_str())),
        headers: vec![ACCEPT_JSON, CONTENT_JSON],
        auth,
        body: Some(serde_json::to_string(request)?),
    })
}

/// Builds the `POST /v1/agents/:id/commands/run` that runs one of the
/// agent's listed commands. The body names the command (id and hash); it
/// never carries command text.
pub fn post_command_run(
    agent: &AgentId,
    request: &CommandRunRequest,
    auth: Option<BearerToken>,
) -> Result<HttpRequest, serde_json::Error> {
    Ok(HttpRequest {
        method: Method::Post,
        path: format!("/v1/agents/{agent}/commands/run"),
        headers: vec![ACCEPT_JSON, CONTENT_JSON],
        auth,
        body: Some(serde_json::to_string(request)?),
    })
}

/// Builds the `POST /v1/terminals/:pane/keys` that sends allowlisted keys
/// to a command terminal.
pub fn post_terminal_keys(
    pane: &PaneId,
    request: &TerminalKeysRequest,
    auth: Option<BearerToken>,
) -> Result<HttpRequest, serde_json::Error> {
    Ok(HttpRequest {
        method: Method::Post,
        path: format!("/v1/terminals/{pane}/keys"),
        headers: vec![ACCEPT_JSON, CONTENT_JSON],
        auth,
        body: Some(serde_json::to_string(request)?),
    })
}

/// `text` percent-encoded as one URL path segment: everything but ASCII
/// letters, digits and `-._~` is escaped, byte by byte.
#[must_use]
pub fn path_segment(text: &str) -> String {
    use core::fmt::Write;
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Builds the `POST /v1/agents/:id/status` that asks for a fresh status
/// line (a tap on it). No body.
#[must_use]
pub fn post_status_refresh(agent: &AgentId, auth: Option<BearerToken>) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        path: format!("/v1/agents/{agent}/status"),
        headers: Vec::new(),
        auth,
        body: None,
    }
}

/// The path of this device's push subscriptions.
pub const PUSH_SUBSCRIPTIONS_PATH: &str = "/v1/push/subscriptions";

/// Builds the `POST /v1/push/subscriptions` that registers `subscription`,
/// the browser's `PushSubscription` as JSON (`toJSON()`), passed through
/// unchanged: the bridge validates it.
#[must_use]
pub fn post_push_subscription(subscription: String, auth: Option<BearerToken>) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        path: PUSH_SUBSCRIPTIONS_PATH.to_owned(),
        headers: vec![CONTENT_JSON],
        auth,
        body: Some(subscription),
    }
}

/// Builds the `DELETE /v1/push/subscriptions` that drops the subscription
/// at `endpoint`.
#[must_use]
pub fn delete_push_subscription(endpoint: &str, auth: Option<BearerToken>) -> HttpRequest {
    HttpRequest {
        method: Method::Delete,
        path: PUSH_SUBSCRIPTIONS_PATH.to_owned(),
        headers: vec![CONTENT_JSON],
        auth,
        // By hand rather than `json!`, which would link `serde_json::Value`
        // into the app for this one body (the WASM size budget).
        body: Some(format!(
            "{{\"endpoint\":{}}}",
            serde_json::to_string(endpoint).unwrap_or_default()
        )),
    }
}

const ACCEPT_JSON: (&str, &str) = ("Accept", "application/json");
const CONTENT_JSON: (&str, &str) = ("Content-Type", "application/json");

#[cfg(test)]
mod tests {
    use protocol::{ActionKind, IdempotencyKey, Origin, PromptPayload, PromptText, Seq, Timestamp};

    use super::*;

    fn agent() -> AgentId {
        "w6:p1~583dc90c".parse().unwrap()
    }

    #[test]
    fn runbook_names_are_one_escaped_path_segment() {
        assert_eq!(path_segment("Deploy bridge"), "Deploy%20bridge");
        assert_eq!(path_segment("a/../b?c#d%"), "a%2F..%2Fb%3Fc%23d%25");
        assert_eq!(path_segment("Ünï"), "%C3%9Cn%C3%AF");
        let request = RunbookRunRequest {
            idempotency_key: IdempotencyKey::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap(),
            origin: Origin::App,
            created_at: Timestamp::from_unix_millis(5),
        };
        let name = RunbookName::new("Deploy bridge").unwrap();
        let built = post_runbook_run(&name, &request, None).unwrap();
        assert_eq!(built.method, Method::Post);
        assert_eq!(built.path, "/v1/runbooks/Deploy%20bridge/run");
        assert_eq!(
            built.body.as_deref(),
            Some(
                r#"{"idempotencyKey":"9ca09978-ef89-46fd-9a85-118b84bee7b4","origin":"app","createdAt":5}"#
            )
        );
    }

    #[test]
    fn command_runs_name_the_candidate_and_never_carry_its_text() {
        let request = CommandRunRequest {
            idempotency_key: IdempotencyKey::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap(),
            origin: Origin::App,
            created_at: Timestamp::from_unix_millis(5),
            candidate: 1,
            hash: protocol::CommandHash::new("0f1e2d3c4b5a69788796a5b4c3d2e1f0").unwrap(),
            target: protocol::RunTarget::Here,
        };
        let built = post_command_run(&agent(), &request, None).unwrap();
        assert_eq!(built.method, Method::Post);
        assert_eq!(built.path, "/v1/agents/w6:p1~583dc90c/commands/run");
        assert_eq!(
            built.body.as_deref(),
            Some(
                r#"{"idempotencyKey":"9ca09978-ef89-46fd-9a85-118b84bee7b4","origin":"app","createdAt":5,"candidate":1,"hash":"0f1e2d3c4b5a69788796a5b4c3d2e1f0","target":"here"}"#
            )
        );
        let keys = TerminalKeysRequest {
            idempotency_key: request.idempotency_key,
            origin: Origin::App,
            created_at: Timestamp::from_unix_millis(5),
            keys: serde_json::from_str(r#"["ctrl+c"]"#).unwrap(),
        };
        let pane = PaneId::new("w9:p2").unwrap();
        let built = post_terminal_keys(&pane, &keys, None).unwrap();
        assert_eq!(built.path, "/v1/terminals/w9:p2/keys");
    }

    #[test]
    fn endpoint_paths() {
        let cases = [
            (Endpoint::Agents, "/v1/agents"),
            (
                Endpoint::Thread {
                    agent: agent(),
                    since: None,
                    limit: None,
                },
                "/v1/agents/w6:p1~583dc90c/thread",
            ),
            (
                Endpoint::Thread {
                    agent: agent(),
                    since: Some(ThreadCursor(42)),
                    limit: None,
                },
                "/v1/agents/w6:p1~583dc90c/thread?since=42",
            ),
            (
                Endpoint::Thread {
                    agent: agent(),
                    since: Some(ThreadCursor(0)),
                    limit: Some(200),
                },
                "/v1/agents/w6:p1~583dc90c/thread?since=0&limit=200",
            ),
            (
                Endpoint::Screen(agent()),
                "/v1/agents/w6:p1~583dc90c/screen",
            ),
            (
                Endpoint::Pending(agent()),
                "/v1/agents/w6:p1~583dc90c/pending",
            ),
            (
                Endpoint::Tldr {
                    agent: agent(),
                    turn: Some(TurnIndex(3)),
                },
                "/v1/agents/w6:p1~583dc90c/tldr?turn=3",
            ),
            (
                Endpoint::Tldr {
                    agent: agent(),
                    turn: None,
                },
                "/v1/agents/w6:p1~583dc90c/tldr",
            ),
            (Endpoint::PushKey, "/v1/push/key"),
            (Endpoint::PushSubscriptions, "/v1/push/subscriptions"),
            (Endpoint::Version, "/v1/version"),
            (
                Endpoint::Suggestions(agent()),
                "/v1/agents/w6:p1~583dc90c/suggestions",
            ),
            (
                Endpoint::Commands(agent()),
                "/v1/agents/w6:p1~583dc90c/commands",
            ),
            (
                Endpoint::TerminalScreen(PaneId::new("w9:p2").unwrap()),
                "/v1/terminals/w9:p2/screen",
            ),
        ];
        for (endpoint, want) in cases {
            assert_eq!(endpoint.path(), want);
        }
    }

    #[test]
    fn get_without_token_has_no_authorization() {
        let req = get(&Endpoint::Agents, None);
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.headers, vec![("Accept", "application/json")]);
        assert_eq!(req.auth, None);
        assert_eq!(req.body, None);
    }

    #[test]
    fn get_with_token_sends_bearer() {
        let token = BearerToken::new("tok").unwrap();
        let req = get(&Endpoint::Agents, Some(token.clone()));
        assert_eq!(req.auth, Some(token));
    }

    #[test]
    fn debug_never_prints_the_token() {
        let req = get(
            &Endpoint::Agents,
            Some(BearerToken::new("s3cret-t0ken").unwrap()),
        );
        let debug = format!("{req:?}");
        assert!(!debug.contains("s3cret"), "{debug}");
        assert!(debug.contains("redacted"), "{debug}");
    }

    #[test]
    fn status_refresh_is_a_bodyless_post() {
        let token = BearerToken::new("tok").unwrap();
        let agent: AgentId = "w1:p1~abc".parse().unwrap();
        let req = post_status_refresh(&agent, Some(token.clone()));
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.path, "/v1/agents/w1:p1~abc/status");
        assert_eq!(req.body, None);
        assert_eq!(req.auth, Some(token));
    }

    #[test]
    fn push_subscription_requests() {
        let token = BearerToken::new("tok").unwrap();
        let post = post_push_subscription("{\"endpoint\":\"e\"}".into(), Some(token.clone()));
        assert_eq!(post.method, Method::Post);
        assert_eq!(post.path, "/v1/push/subscriptions");
        assert_eq!(post.body.as_deref(), Some("{\"endpoint\":\"e\"}"));
        assert_eq!(post.auth, Some(token));

        let delete = delete_push_subscription("https://push.example/a\"b", None);
        assert_eq!(delete.method.as_str(), "DELETE");
        assert_eq!(delete.path, "/v1/push/subscriptions");
        let body: serde_json::Value =
            serde_json::from_str(delete.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "endpoint": "https://push.example/a\"b" })
        );
        assert!(
            delete
                .headers
                .contains(&("Content-Type", "application/json"))
        );
    }

    #[test]
    fn post_action_serializes_the_protocol_type() {
        let action = Action {
            idempotency_key: IdempotencyKey::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap(),
            agent_id: agent(),
            expected_seq: Seq(7),
            created_at: Timestamp::from_unix_millis(1),
            kind: ActionKind::Prompt(PromptPayload::text(PromptText::new("go").unwrap())),
            origin: Origin::App,
        };
        let req = post_action(&action, None).unwrap();
        assert_eq!(req.method.as_str(), "POST");
        assert_eq!(req.path, ACTIONS_PATH);
        assert!(req.headers.contains(&("Content-Type", "application/json")));
        let round_trip: Action = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(round_trip, action);
    }
}
