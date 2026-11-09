//! `GET /v1/agents`.

use axum::Json;
use axum::extract::State;
use protocol::Agent;

use super::AppState;

/// Every agent, as a bare JSON array of [`Agent`]s ordered by pane, each
/// with its latest turn headline and, while it works, its status line (see
/// [`crate::tldr::Tldrs::decorate`]), or while it is blocked, what it asks
/// (see [`crate::registry::Hub::cards`]: no screen is read here).
/// A client parses it as `Vec<Lenient<Agent>>`, so one entry it cannot read
/// never hides the rest.
pub(crate) async fn list(State(state): State<AppState>) -> Json<Vec<Agent>> {
    let mut agents = state.hub.cards();
    for agent in &mut agents {
        state.tldrs.decorate(agent);
    }
    Json(agents)
}
