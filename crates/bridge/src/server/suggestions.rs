//! `GET /v1/agents/:id/suggestions` (design §6.2): short replies to a
//! question at the end of an idle or done agent's last finished turn,
//! offered as chips (a tap sends one; never sent without a tap).
//!
//! A blocked agent gets `[]` and nothing is read: herdr refuses every
//! prompt to a blocked agent, so a typed reply could not be delivered; its
//! dialog is answered with its options or the keypad.
//!
//! - `200` with a JSON array of at most [`protocol::MAX_SUGGESTIONS`]
//!   valid suggestions ([`protocol::suggestion`]); empty when nothing is
//!   asked, the model had nothing safe to offer, the agent is not at its
//!   prompt, or suggestions are off. Final until the agent's status or seq
//!   changes;
//! - `202` with `[]` while they are generated, or while a failed try or the
//!   usage limit holds them back (a request after the pause tries again):
//!   ask again;
//! - `400` for a malformed id, `404` if the agent is gone, `502` if herdr
//!   fails.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::views::current_agent;
use crate::tldr::SuggestionsState;

pub(crate) async fn suggestions(
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> Response {
    let (id, agent) = match current_agent(&state, &raw).await {
        Ok(found) => found,
        Err(status) => return status.into_response(),
    };
    match state.tldrs.suggestions().for_turn(&id, &agent) {
        SuggestionsState::Ready(found) => {
            let texts: Vec<String> = found.into_iter().map(String::from).collect();
            Json(texts).into_response()
        }
        SuggestionsState::Generating | SuggestionsState::Later => {
            (StatusCode::ACCEPTED, Json(Vec::<String>::new())).into_response()
        }
        SuggestionsState::None => Json(Vec::<String>::new()).into_response(),
    }
}
