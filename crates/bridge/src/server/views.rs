//! `GET /v1/agents/:id/thread`, `GET /v1/agents/:id/screen` and
//! `GET /v1/agents/:id/pending`.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use herdr::ReadSource;
use protocol::{AgentId, AgentScreen, MAX_SCREEN_LINES, Seq, ThreadCursor, text};

use super::{AppState, query};
use crate::control::{Resolved, resolve};
use crate::registry::protocol_status;
use crate::threads::ThreadSource;

/// The agent named in the path, as herdr sees it now, or the response to
/// send instead: 400 for a malformed id, 404 if it is gone or another
/// session runs in its pane, 502 if herdr fails.
pub(crate) async fn current_agent(
    state: &AppState,
    raw: &str,
) -> Result<(AgentId, herdr::Agent), StatusCode> {
    let id: AgentId = raw.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    match resolve(state.control.as_ref(), &id).await {
        Ok(Resolved::Current(agent)) => Ok((id, agent)),
        Ok(Resolved::Moved(_) | Resolved::Gone) => Err(StatusCode::NOT_FOUND),
        Err(error) => {
            tracing::warn!(%error, "herdr agent lookup failed");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

/// `?since=<cursor>&limit=<n>`; both optional, anything else is a 400.
fn thread_query(query: Option<&str>) -> Result<(ThreadCursor, Option<usize>), StatusCode> {
    let mut since = ThreadCursor(0);
    let mut limit = None;
    for pair in query::pairs(query) {
        match pair? {
            ("since", value) => since = ThreadCursor(query::number(value)?),
            ("limit", value) => limit = Some(query::number(value)?),
            _ => return Err(StatusCode::BAD_REQUEST),
        }
    }
    Ok((since, limit))
}

/// The agent's thread after `since`, with its session's recent turn
/// headlines. Agents without a transcript (other kinds, transcript not
/// written yet, none matched by folder) get an empty page.
pub(crate) async fn thread(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let (since, limit) = match thread_query(query.as_deref()) {
        Ok(parsed) => parsed,
        Err(status) => return status.into_response(),
    };
    let (id, agent) = match current_agent(&state, &raw).await {
        Ok(found) => found,
        Err(status) => return status.into_response(),
    };
    let mut page = match ThreadSource::for_agent(id.clone(), &agent) {
        Some(source) => state.threads.page(source, since, limit).await,
        None => state.threads.empty_page(),
    };
    // Read after the transcript, so a headline made meanwhile is included.
    page.headlines = state.tldrs.page_headlines(&id, since == ThreadCursor(0));
    Json(page).into_response()
}

/// The agent's terminal tail for the keypad fallback: the visible viewport
/// while it works, else the last [`MAX_SCREEN_LINES`] lines of recent
/// output (where a dialog's question may have scrolled off the viewport).
pub(crate) async fn screen(State(state): State<AppState>, Path(raw): Path<String>) -> Response {
    let (_, agent) = match current_agent(&state, &raw).await {
        Ok(found) => found,
        Err(status) => return status.into_response(),
    };
    // herdr can only scroll an agent's history while it is idle or done
    // (`agent_not_idle` otherwise): a working or blocked agent — the case the
    // keypad exists for — yields its visible screen.
    let (source, lines) = if agent.agent_status.is_ready() {
        (
            ReadSource::RecentUnwrapped,
            u32::try_from(MAX_SCREEN_LINES).ok(),
        )
    } else {
        (ReadSource::Visible, None)
    };
    let read = match state.control.read(&agent.pane_id, source, lines).await {
        Err(e) if e.code().is_some_and(|c| c.as_str() == "agent_not_idle") => {
            state
                .control
                .read(&agent.pane_id, ReadSource::Visible, None)
                .await
        }
        other => other,
    };
    let screen = match read {
        Ok(screen) => screen,
        Err(error) => {
            tracing::warn!(%error, "herdr screen read failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    Json(AgentScreen {
        text: screen_tail(&screen.text, MAX_SCREEN_LINES),
        revision: screen.revision,
        status: protocol_status(agent.agent_status),
        seq: Seq(agent.state_change_seq),
    })
    .into_response()
}

/// What the agent is asking now: a [`protocol::PendingInput`] parsed from
/// its screen while herdr reports it `blocked` (read fresh on every
/// request), else `null`. 404 if the agent is gone, 502 if herdr fails.
pub(crate) async fn pending(State(state): State<AppState>, Path(raw): Path<String>) -> Response {
    let (_, agent) = match current_agent(&state, &raw).await {
        Ok(found) => found,
        Err(status) => return status.into_response(),
    };
    match state.pending.current(&agent).await {
        Ok(dialog) => Json(dialog.map(|dialog| dialog.input)).into_response(),
        Err(error) => {
            tracing::warn!(%error, "herdr screen read failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// The last `max` lines of `raw`, trailing blank lines dropped, with
/// control characters (other than tab) and hidden characters (bidi
/// controls, zero-width) removed. The result is still untrusted text.
pub(crate) fn screen_tail(raw: &str, max: usize) -> String {
    let lines: Vec<String> = raw
        .lines()
        .map(|line| {
            line.chars()
                .filter(|&c| c == '\t' || !(c.is_control() || text::is_hidden_char(c)))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    let end = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |i| i + 1);
    let start = end.saturating_sub(max);
    lines.get(start..end).unwrap_or_default().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_tail_keeps_the_last_lines() {
        let raw = "a\nb\nc\nd\n\n  \n";
        assert_eq!(screen_tail(raw, 2), "c\nd");
        assert_eq!(screen_tail(raw, 10), "a\nb\nc\nd");
        assert_eq!(screen_tail("", 10), "");
    }

    #[test]
    fn screen_tail_strips_controls() {
        assert_eq!(screen_tail("ok\u{1b}[31m\u{202e}x\ty\r", 5), "ok[31mx\ty");
    }

    #[test]
    fn thread_query_parses_since_and_limit() {
        assert_eq!(thread_query(None).ok(), Some((ThreadCursor(0), None)));
        assert_eq!(
            thread_query(Some("since=12&limit=5")).ok(),
            Some((ThreadCursor(12), Some(5)))
        );
        assert!(thread_query(Some("since=-1")).is_err());
        assert!(thread_query(Some("cursor=1")).is_err());
        assert!(thread_query(Some("since")).is_err());
    }
}
