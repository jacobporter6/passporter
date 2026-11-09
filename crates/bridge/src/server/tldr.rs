//! `GET /v1/agents/:id/tldr[?turn=<n>]` (design §6.4).
//!
//! Without `turn`, the latest finished turn's TL;DR, which only a long
//! turn has. With it, turn `n`'s on request (the app's "Summarise"),
//! whatever its length, finished or in progress; one of the turn in
//! progress is provisional (marked so on its `tldr_ready`).
//!
//! - `200` with the [`protocol::Tldr`] when it is cached;
//! - `202` with `null` while it is generated; a `tldr_ready` event follows;
//! - `200` with `null` when there is none: no finished turn (no such
//!   turn), a short one (without `turn`), an agent without a Claude
//!   transcript, a recent failure or the usage limit, or TL;DRs disabled;
//! - `429` (with `Retry-After`) for a turn asked for by number while
//!   another such turn is being generated (the latest finished turn never
//!   is), or when the device has started too many lately;
//! - `400` for a malformed id or query, `404` if the agent is gone, `502`
//!   if herdr fails.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use protocol::{Tldr, TurnIndex};

use super::views::current_agent;
use super::{AppState, ClientAddr, Principal, query};
use crate::actions::{Budget, retry_after_secs};
use crate::tldr::TldrState;

/// `Retry-After` for a busy answer: about one generation.
const BUSY_RETRY_AFTER_SECS: u64 = 30;

/// `?turn=<n>`, optional; anything else is a 400.
fn turn_query(query: Option<&str>) -> Result<Option<TurnIndex>, StatusCode> {
    let mut turn = None;
    for pair in query::pairs(query) {
        match pair? {
            ("turn", value) => turn = Some(TurnIndex(query::number(value)?)),
            _ => return Err(StatusCode::BAD_REQUEST),
        }
    }
    Ok(turn)
}

pub(crate) async fn tldr(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    RawQuery(query): RawQuery,
    Extension(principal): Extension<Principal>,
    Extension(client): Extension<ClientAddr>,
) -> Response {
    let wanted = match turn_query(query.as_deref()) {
        Ok(turn) => turn,
        Err(status) => return status.into_response(),
    };
    let (id, agent) = match current_agent(&state, &raw).await {
        Ok(found) => found,
        Err(status) => return status.into_response(),
    };
    let found = match wanted {
        None => state.tldrs.get(&id, &agent, None).await,
        Some(turn) => {
            let requester = Budget::of(&principal, client);
            state.tldrs.request(&id, &agent, turn, &requester).await
        }
    };
    match found {
        TldrState::Ready(tldr) => Json(Some(tldr)).into_response(),
        TldrState::Generating => (StatusCode::ACCEPTED, Json(None::<Tldr>)).into_response(),
        TldrState::None => Json(None::<Tldr>).into_response(),
        TldrState::Busy => too_many(BUSY_RETRY_AFTER_SECS),
        TldrState::Limited(wait) => too_many(retry_after_secs(wait)),
    }
}

/// `429` with `Retry-After: secs` and a `null` body.
fn too_many(secs: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, HeaderValue::from(secs))],
        Json(None::<Tldr>),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_query_parses_an_optional_turn() {
        assert_eq!(turn_query(None), Ok(None));
        assert_eq!(turn_query(Some("")), Ok(None));
        assert_eq!(turn_query(Some("turn=3")), Ok(Some(TurnIndex(3))));
        assert!(turn_query(Some("turn=-1")).is_err());
        assert!(turn_query(Some("turn")).is_err());
        assert!(turn_query(Some("since=1")).is_err());
    }
}
