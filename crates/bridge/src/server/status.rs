//! `POST /v1/agents/:id/status`: remake a working agent's status line now
//! (a tap on it; design §6.1, §6.4). The line itself is served on the agent
//! (`status_line`) and announced as `status_line_ready`, followed by an
//! `agent_updated`.
//!
//! - `202` when a new line is being made: an event follows (unless the
//!   generation fails);
//! - `200` (no body) when the turn has not changed since the last line:
//!   the line shown is current;
//! - `204` when there is nothing to do: the agent is not a working Claude
//!   agent, or status lines are disabled;
//! - `429` (with `Retry-After`) when the agent was tapped too recently;
//! - `503` (with `Retry-After`) while the account is at its usage limit;
//! - `400` for a malformed id, `404` if the agent is gone, `502` if herdr
//!   fails.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::views::current_agent;
use crate::actions::retry_after_secs;
use crate::tldr::StatusRefresh;

pub(crate) async fn refresh(State(state): State<AppState>, Path(raw): Path<String>) -> Response {
    let (id, agent) = match current_agent(&state, &raw).await {
        Ok(found) => found,
        Err(status) => return status.into_response(),
    };
    match state.tldrs.status_lines().refresh(&id, &agent).await {
        StatusRefresh::Queued => StatusCode::ACCEPTED.into_response(),
        StatusRefresh::Unchanged => StatusCode::OK.into_response(),
        StatusRefresh::Idle => StatusCode::NO_CONTENT.into_response(),
        StatusRefresh::TooSoon(wait) => retry_after(StatusCode::TOO_MANY_REQUESTS, wait),
        StatusRefresh::Paused(wait) => retry_after(StatusCode::SERVICE_UNAVAILABLE, wait),
    }
}

fn retry_after(status: StatusCode, wait: Duration) -> Response {
    let secs = HeaderValue::from(retry_after_secs(wait));
    (status, [(header::RETRY_AFTER, secs)]).into_response()
}
