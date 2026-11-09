//! Pure response interpretation: HTTP status + body → typed values.
//!
//! The rules for `POST /v1/actions` follow the outbox contract in
//! [`protocol::result`]: any 2xx or 409 is final for its idempotency key,
//! parseable or not, and "no response" (network error, a body lost
//! mid-read, a 5xx the bridge didn't answer) is the only thing worth
//! retrying with the same key.

use protocol::{ActionResult, CommandResult, RunbookResult, SpawnResult, Tldr};
use serde::de::DeserializeOwned;

/// A response as the transport saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// The body, or `None` if the connection dropped while reading it.
    pub body: Option<String>,
}

impl HttpResponse {
    /// A response whose body arrived in full.
    #[must_use]
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: Some(body.into()),
        }
    }

    const fn is_success(&self) -> bool {
        matches!(self.status, 200..=299)
    }

    /// 2xx or 409: the bridge has decided this action, whatever the body.
    const fn is_final_for_action(&self) -> bool {
        self.is_success() || self.status == CONFLICT
    }
}

/// HTTP 202: accepted, the result is still being produced.
pub const ACCEPTED: u16 = 202;
/// HTTP 401: missing, expired or revoked token.
pub const UNAUTHORIZED: u16 = 401;
/// HTTP 409: the action was stale.
pub const CONFLICT: u16 = 409;

/// What the transport reports when the connection dropped mid-body.
pub(crate) const BODY_LOST: &str = "connection lost while reading the response";

/// Why a request produced no usable value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The request got no complete HTTP response (offline, tunnel drop,
    /// body lost mid-read, DNS, CSP refusal).
    #[error("no response from the bridge: {0}")]
    Transport(String),
    /// The bridge rejected the token (401). The token source has been told.
    #[error("not authorized; pair this device again")]
    Unauthorized,
    /// Any other non-success status without a usable body.
    #[error("bridge answered HTTP {status}")]
    Status {
        /// The status code.
        status: u16,
    },
    /// A success status whose body did not match the protocol type.
    #[error("unexpected response body: {0}")]
    Decode(String),
    /// The request could not be built (a body failed to serialize).
    #[error("could not build request: {0}")]
    Encode(String),
}

impl ApiError {
    /// Whether retrying the identical request may succeed: no (complete)
    /// response, or a server-side (5xx) status. Everything else needs a
    /// change first (new token, new body, app update).
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Status { status } => *status >= 500,
            Self::Unauthorized | Self::Decode(_) | Self::Encode(_) => false,
        }
    }
}

/// Parses a read endpoint's response as `T`.
pub fn read_json<T: DeserializeOwned>(response: &HttpResponse) -> Result<T, ApiError> {
    match (response.status, &response.body) {
        (UNAUTHORIZED, _) => Err(ApiError::Unauthorized),
        (_, None) => Err(ApiError::Transport(BODY_LOST.to_owned())),
        (_, Some(body)) if response.is_success() => {
            serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))
        }
        (status, Some(_)) => Err(ApiError::Status { status }),
    }
}

/// What `GET /v1/agents/:id/tldr` said about a turn's TL;DR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TldrRead {
    /// The TL;DR (`200`).
    Ready(Tldr),
    /// Being generated (`202`); ask again shortly.
    Generating,
    /// There is none (`200 null`): no finished long turn, or not a Claude
    /// agent, or TL;DRs are off.
    None,
}

/// Interprets the response to `GET /v1/agents/:id/tldr`.
pub fn read_tldr(response: &HttpResponse) -> Result<TldrRead, ApiError> {
    if response.status == ACCEPTED && response.body.is_some() {
        return Ok(TldrRead::Generating);
    }
    let tldr: Option<Tldr> = read_json(response)?;
    Ok(tldr.map_or(TldrRead::None, TldrRead::Ready))
}

/// `Ok` for a 2xx, whatever the body (the push subscription endpoints
/// answer 201/204 with none); otherwise the matching [`ApiError`].
pub fn expect_success(response: &HttpResponse) -> Result<(), ApiError> {
    match response.status {
        UNAUTHORIZED => Err(ApiError::Unauthorized),
        _ if response.is_success() => Ok(()),
        status => Err(ApiError::Status { status }),
    }
}

/// What `GET /v1/agents/:id/suggestions` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionsRead {
    /// The suggestions (`200`), possibly none. Untrusted text: validate
    /// before showing.
    Ready(Vec<String>),
    /// Not ready yet (`202`): being generated, or held back after a
    /// failed try or the usage limit until a later ask retries. Ask again.
    Generating,
}

/// Interprets the response to `GET /v1/agents/:id/suggestions`.
pub fn read_suggestions(response: &HttpResponse) -> Result<SuggestionsRead, ApiError> {
    if response.status == ACCEPTED && response.body.is_some() {
        return Ok(SuggestionsRead::Generating);
    }
    read_json(response).map(SuggestionsRead::Ready)
}

/// What `POST /v1/agents/:id/status` (a tap on the status line) said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusRefresh {
    /// A fresh line is being made (`202`); it arrives on the stream.
    Queued,
    /// The turn has not changed since the line shown (`200`): it is
    /// current.
    Current,
    /// No line can come (`204`): the agent is not a working Claude agent,
    /// or the bridge has no summarizer.
    Unavailable,
    /// Tapped too recently (`429`): try again shortly.
    TooSoon,
    /// The account is at its usage limit (`503`): try again much later.
    Paused,
}

/// Interprets the response to `POST /v1/agents/:id/status`.
pub fn read_status_refresh(response: &HttpResponse) -> Result<StatusRefresh, ApiError> {
    match response.status {
        UNAUTHORIZED => Err(ApiError::Unauthorized),
        ACCEPTED => Ok(StatusRefresh::Queued),
        200 => Ok(StatusRefresh::Current),
        204 => Ok(StatusRefresh::Unavailable),
        TOO_MANY_REQUESTS => Ok(StatusRefresh::TooSoon),
        SERVICE_UNAVAILABLE => Ok(StatusRefresh::Paused),
        status => Err(ApiError::Status { status }),
    }
}

/// HTTP 429: too many requests.
const TOO_MANY_REQUESTS: u16 = 429;
/// HTTP 503: the bridge cannot do it now.
const SERVICE_UNAVAILABLE: u16 = 503;

/// What came back from `POST /v1/actions`, when it is final for the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// The bridge's typed answer: sent, stale (409), refused or failed. See
    /// [`ActionResult`] for how each is handled.
    Result(ActionResult),
    /// A 2xx or 409 whose body did not parse (or was lost, or has a status
    /// this app predates). Still final for the key: the outcome is unknown,
    /// so surface it and never retry the key.
    Unreadable {
        /// The status code.
        status: u16,
    },
}

impl ActionOutcome {
    /// Whether the action was refused for a reason that allows retrying
    /// the *same* idempotency key ([`protocol::RefusalReason::is_transient`]).
    #[must_use]
    pub fn may_retry_same_key(&self) -> bool {
        matches!(self, Self::Result(ActionResult::Refused { reason, .. }) if reason.is_transient())
    }
}

/// Interprets the response to `POST /v1/actions`.
///
/// 1. 401 → [`ApiError::Unauthorized`].
/// 2. A body that parses as a *known* [`ActionResult`] variant (sent,
///    stale, refused, failed), at any status → that result. Only the bridge
///    produces these shapes.
/// 3. Otherwise a 2xx or 409 → [`ActionOutcome::Unreadable`]: final, because
///    the bridge has decided the key. This includes a lost body and
///    [`ActionResult::Unknown`].
/// 4. Otherwise no bridge answer can be identified: a lost body is
///    [`ApiError::Transport`], anything else (e.g. an edge error page, or
///    JSON like `{"status":"error"}` that only parses as `Unknown`) is
///    [`ApiError::Status`], retryable for 5xx. The bridge dedupes, so a
///    retry with the same key never sends twice.
pub fn action_outcome(response: &HttpResponse) -> Result<ActionOutcome, ApiError> {
    if response.status == UNAUTHORIZED {
        return Err(ApiError::Unauthorized);
    }
    let parsed = response
        .body
        .as_deref()
        .and_then(|body| serde_json::from_str::<ActionResult>(body).ok());
    match parsed {
        Some(result) if result != ActionResult::Unknown => Ok(ActionOutcome::Result(result)),
        _ if response.is_final_for_action() => Ok(ActionOutcome::Unreadable {
            status: response.status,
        }),
        _ if response.body.is_none() => Err(ApiError::Transport(BODY_LOST.to_owned())),
        _ => Err(ApiError::Status {
            status: response.status,
        }),
    }
}

/// Interprets the response to `POST /v1/agents`, by the same rules as
/// [`action_outcome`]: a known [`SpawnResult`] at any status is that
/// result, and a 2xx whose body this app cannot read is
/// [`SpawnResult::Unknown`] (final: the bridge decided the key, and the
/// agent may be running). Anything else is an error, which a lost body or
/// 5xx makes retryable with the same key (the bridge dedupes, so a retry
/// never starts a second agent).
pub fn spawn_outcome(response: &HttpResponse) -> Result<SpawnResult, ApiError> {
    decided_outcome(response, SpawnResult::Unknown)
}

/// Interprets the response to `POST /v1/runbooks/:name/run`, by the same
/// rules as [`spawn_outcome`] (a retry with the same key never runs it
/// twice).
pub fn runbook_outcome(response: &HttpResponse) -> Result<RunbookResult, ApiError> {
    decided_outcome(response, RunbookResult::Unknown)
}

/// Interprets the response to `POST /v1/agents/:id/commands/run` or
/// `POST /v1/terminals/:pane/keys`, by the same rules as
/// [`spawn_outcome`] (a retry with the same key never runs it twice).
pub fn command_outcome(response: &HttpResponse) -> Result<CommandResult, ApiError> {
    decided_outcome(response, CommandResult::Unknown)
}

/// A known result at any status, `unknown` for an unreadable 2xx, else an
/// error.
fn decided_outcome<T>(response: &HttpResponse, unknown: T) -> Result<T, ApiError>
where
    T: DeserializeOwned + PartialEq,
{
    if response.status == UNAUTHORIZED {
        return Err(ApiError::Unauthorized);
    }
    let parsed = response
        .body
        .as_deref()
        .and_then(|body| serde_json::from_str::<T>(body).ok());
    match parsed {
        Some(result) if result != unknown => Ok(result),
        _ if response.is_success() => Ok(unknown),
        _ if response.body.is_none() => Err(ApiError::Transport(BODY_LOST.to_owned())),
        _ => Err(ApiError::Status {
            status: response.status,
        }),
    }
}

#[cfg(test)]
mod tests {
    use protocol::{Agent, RefusalReason, SpawnRefusal};

    use super::*;

    fn resp(status: u16, body: &str) -> HttpResponse {
        HttpResponse::new(status, body)
    }

    fn lost(status: u16) -> HttpResponse {
        HttpResponse { status, body: None }
    }

    #[test]
    fn status_refreshes_map_their_statuses() {
        let read = |status| read_status_refresh(&resp(status, ""));
        assert_eq!(read(202), Ok(StatusRefresh::Queued));
        assert_eq!(read(200), Ok(StatusRefresh::Current));
        assert_eq!(read(204), Ok(StatusRefresh::Unavailable));
        assert_eq!(read(429), Ok(StatusRefresh::TooSoon));
        assert_eq!(read(503), Ok(StatusRefresh::Paused));
        assert_eq!(read(401), Err(ApiError::Unauthorized));
        assert_eq!(read(404), Err(ApiError::Status { status: 404 }));
    }

    #[test]
    fn tldr_reads_tell_generating_from_none() {
        let body = r#"{"outcome":"Tests pass.","changed":["a.rs"],"risks":[]}"#;
        let Ok(TldrRead::Ready(tldr)) = read_tldr(&resp(200, body)) else {
            panic!("a TL;DR");
        };
        assert_eq!(tldr.outcome, "Tests pass.");
        assert_eq!(read_tldr(&resp(202, "null")), Ok(TldrRead::Generating));
        assert_eq!(read_tldr(&resp(200, "null")), Ok(TldrRead::None));
        assert_eq!(read_tldr(&resp(401, "")), Err(ApiError::Unauthorized));
        assert_eq!(
            read_tldr(&resp(404, "")),
            Err(ApiError::Status { status: 404 })
        );
        assert!(matches!(read_tldr(&lost(202)), Err(ApiError::Transport(_))));
        assert!(matches!(
            read_tldr(&resp(200, "{}")),
            Err(ApiError::Decode(_))
        ));
    }

    #[test]
    fn suggestion_reads_tell_generating_from_ready() {
        assert_eq!(
            read_suggestions(&resp(200, r#"["Yes","No"]"#)),
            Ok(SuggestionsRead::Ready(vec!["Yes".into(), "No".into()]))
        );
        assert_eq!(
            read_suggestions(&resp(200, "[]")),
            Ok(SuggestionsRead::Ready(vec![]))
        );
        assert_eq!(
            read_suggestions(&resp(202, "[]")),
            Ok(SuggestionsRead::Generating)
        );
        assert!(matches!(
            read_suggestions(&lost(202)),
            Err(ApiError::Transport(_))
        ));
        assert!(matches!(
            read_suggestions(&resp(200, "{}")),
            Err(ApiError::Decode(_))
        ));
    }

    #[test]
    fn sent() {
        assert_eq!(
            action_outcome(&resp(200, r#"{"status":"sent"}"#)),
            Ok(ActionOutcome::Result(ActionResult::Sent {
                replayed: false
            }))
        );
    }

    #[test]
    fn stale_409_is_typed() {
        let outcome = action_outcome(&resp(409, r#"{"status":"stale"}"#)).unwrap();
        assert_eq!(
            outcome,
            ActionOutcome::Result(ActionResult::Stale {
                current: None,
                pending: None
            })
        );
        assert!(!outcome.may_retry_same_key());
    }

    #[test]
    fn unparseable_or_lost_409_and_2xx_are_final_not_errors() {
        for response in [resp(409, "<html>"), resp(202, ""), lost(200), lost(409)] {
            assert_eq!(
                action_outcome(&response),
                Ok(ActionOutcome::Unreadable {
                    status: response.status
                })
            );
        }
        // A status this app predates, on a 2xx: decided, but unknown how.
        assert_eq!(
            action_outcome(&resp(200, r#"{"status":"queued"}"#)),
            Ok(ActionOutcome::Unreadable { status: 200 })
        );
    }

    #[test]
    fn refusals_are_typed_and_know_if_retryable() {
        let step_up = action_outcome(&resp(
            403,
            r#"{"status":"refused","reason":"step_up_required","message":"Confirm"}"#,
        ))
        .unwrap();
        assert_eq!(
            step_up,
            ActionOutcome::Result(ActionResult::Refused {
                reason: RefusalReason::StepUpRequired,
                message: "Confirm".into()
            })
        );
        assert!(step_up.may_retry_same_key());

        let car =
            action_outcome(&resp(403, r#"{"status":"refused","reason":"car_policy"}"#)).unwrap();
        assert!(!car.may_retry_same_key());
    }

    #[test]
    fn unknown_status_on_a_5xx_is_retryable_not_final() {
        // An edge/proxy error body that happens to have a `status` string.
        let err = action_outcome(&resp(
            502,
            r#"{"status":"error","message":"upstream unavailable"}"#,
        ))
        .unwrap_err();
        assert_eq!(err, ApiError::Status { status: 502 });
        assert!(err.is_retryable());
    }

    #[test]
    fn lost_body_on_other_statuses_is_retryable() {
        let err = action_outcome(&lost(429)).unwrap_err();
        assert!(matches!(err, ApiError::Transport(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn unauthorized_and_edge_errors() {
        assert_eq!(action_outcome(&resp(401, "")), Err(ApiError::Unauthorized));
        let edge = action_outcome(&resp(502, "Bad Gateway")).unwrap_err();
        assert_eq!(edge, ApiError::Status { status: 502 });
        assert!(edge.is_retryable());
        assert!(!ApiError::Status { status: 400 }.is_retryable());
        assert!(ApiError::Transport("offline".into()).is_retryable());
        assert!(!ApiError::Unauthorized.is_retryable());
    }

    #[test]
    fn read_json_maps_statuses() {
        let agents: Vec<Agent> = read_json(&resp(200, "[]")).unwrap();
        assert_eq!(agents, Vec::new());
        assert_eq!(
            read_json::<Vec<Agent>>(&resp(401, "")),
            Err(ApiError::Unauthorized)
        );
        assert_eq!(
            read_json::<Vec<Agent>>(&resp(503, "")),
            Err(ApiError::Status { status: 503 })
        );
        assert!(matches!(
            read_json::<Vec<Agent>>(&resp(200, "{}")),
            Err(ApiError::Decode(_))
        ));
    }

    #[test]
    fn read_json_lost_body_is_retryable() {
        let err = read_json::<Vec<Agent>>(&lost(200)).unwrap_err();
        assert!(err.is_retryable(), "{err:?}");
    }

    #[test]
    fn spawn_outcomes_follow_the_outbox_rules() {
        let spawned = r#"{"status":"spawned","agentId":"w2:p1"}"#;
        assert!(matches!(
            spawn_outcome(&resp(200, spawned)),
            Ok(SpawnResult::Spawned { .. })
        ));
        let limited = r#"{"status":"refused","reason":"rate_limited","message":"later"}"#;
        assert_eq!(
            spawn_outcome(&resp(429, limited)),
            Ok(SpawnResult::Refused {
                reason: SpawnRefusal::RateLimited,
                message: "later".into()
            })
        );
        assert_eq!(
            spawn_outcome(&resp(200, "<html>")),
            Ok(SpawnResult::Unknown)
        );
        assert_eq!(spawn_outcome(&resp(401, "")), Err(ApiError::Unauthorized));
        let edge = spawn_outcome(&resp(502, "<html>")).unwrap_err();
        assert!(edge.is_retryable());
        assert!(spawn_outcome(&lost(503)).unwrap_err().is_retryable());
    }
}
