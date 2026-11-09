//! The `WS /v1/stream` socket: connect, authenticate, reconnect.
//!
//! One socket per page. Each connection gets a generation number; callbacks
//! from an older socket see a different generation and do nothing, so a
//! late `close` can never tear down its successor.

use core::cell::RefCell;
use core::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{AgentEvent, StreamAuth};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{MessageEvent, VisibilityState, WebSocket};

use super::{Conn, Live};
use crate::api::BearerToken;
use crate::session;
use crate::ui::AppState;

/// First retry delay.
const BASE_DELAY_MS: u32 = 1_000;
/// Retry delay cap.
const MAX_DELAY_MS: u32 = 30_000;
/// A page hidden at least this long reconnects when it comes back, even if
/// its socket still looks open.
const STALE_AFTER_HIDDEN_MS: f64 = 20_000.0;
/// A visible page whose stream has been silent this long checks it.
const SILENT_MS: f64 = 30_000.0;
/// How often the silence check runs.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// The stream URL for a page at `protocol` (`http:`/`https:`) and `host`.
#[must_use]
pub fn stream_url(protocol: &str, host: &str) -> String {
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    format!("{scheme}://{host}/v1/stream")
}

/// The delay before retry number `attempt` (0-based): doubling from one
/// second up to thirty, plus up to 25% jitter from `jitter` (0..=255) so
/// phones don't reconnect in lockstep after a bridge restart.
#[must_use]
pub fn backoff_ms(attempt: u32, jitter: u8) -> u32 {
    let base = BASE_DELAY_MS
        .saturating_mul(1_u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX))
        .min(MAX_DELAY_MS);
    base + base / 4 * u32::from(jitter) / 255
}

struct Socket {
    ws: WebSocket,
    _on_open: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut()>,
}

impl Socket {
    fn shut(&self) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        let _ = self.ws.close();
    }
}

#[derive(Default)]
struct State {
    generation: u32,
    socket: Option<Socket>,
    attempt: u32,
    snapshot_seen: bool,
    retry: Option<TimeoutHandle>,
    hidden_since: Option<f64>,
    last_message: f64,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// Closes the socket and cancels any scheduled retry.
pub(super) fn disconnect() {
    STATE.with_borrow_mut(|state| {
        state.generation = state.generation.wrapping_add(1);
        if let Some(socket) = state.socket.take() {
            socket.shut();
        }
        if let Some(retry) = state.retry.take() {
            retry.clear();
        }
    });
}

/// (Re)connects with `token`, replacing any current socket.
pub(super) fn connect(app: AppState, live: Live, token: BearerToken) {
    disconnect();
    let Some(location) = web_sys::window().map(|w| w.location()) else {
        return;
    };
    let (Ok(protocol), Ok(host)) = (location.protocol(), location.host()) else {
        return;
    };
    let generation = STATE.with_borrow_mut(|state| {
        state.snapshot_seen = false;
        state.last_message = js_sys::Date::now();
        state.generation
    });
    live.conn.set(Conn::Connecting);
    let Ok(ws) = WebSocket::new(&stream_url(&protocol, &host)) else {
        schedule_retry(app, live);
        return;
    };

    let current = move || STATE.with_borrow(|s| s.generation == generation);

    let opened = ws.clone();
    let on_open = Closure::<dyn FnMut()>::new(move || {
        let auth = StreamAuth {
            token: token.expose().to_owned(),
        };
        if let Ok(json) = serde_json::to_string(&auth) {
            let _ = opened.send_with_str(&json);
        }
    });
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        if !current() {
            return;
        }
        let Some(text) = event.data().as_string() else {
            return;
        };
        let Ok(event) = serde_json::from_str::<AgentEvent>(&text) else {
            return;
        };
        if matches!(event, AgentEvent::Snapshot { .. }) {
            STATE.with_borrow_mut(|s| {
                s.snapshot_seen = true;
                s.attempt = 0;
            });
        }
        STATE.with_borrow_mut(|s| s.last_message = js_sys::Date::now());
        live.apply(event);
    });
    let on_close = Closure::<dyn FnMut()>::new(move || {
        if !current() {
            return;
        }
        let authenticated = STATE.with_borrow_mut(|s| {
            s.socket = None;
            s.snapshot_seen
        });
        if !authenticated {
            check_token(app);
        }
        schedule_retry(app, live);
    });
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    STATE.with_borrow_mut(|state| {
        state.socket = Some(Socket {
            ws,
            _on_open: on_open,
            _on_message: on_message,
            _on_close: on_close,
        });
    });
}

/// A socket that closed before its snapshot may have been refused for a
/// bad token. One authenticated HTTP read tells: a 401 makes the token
/// source forget the token, which stops the stream.
fn check_token(app: AppState) {
    spawn_local(async move {
        let _ = session::api(app).agents().await;
    });
}

fn schedule_retry(app: AppState, live: Live) {
    live.conn.set(Conn::Retrying);
    let attempt = STATE.with_borrow_mut(|s| {
        let attempt = s.attempt;
        s.attempt = s.attempt.saturating_add(1);
        attempt
    });
    let delay = backoff_ms(attempt, jitter());
    let generation = STATE.with_borrow(|s| s.generation);
    let handle = set_timeout_with_handle(
        move || {
            let still_current = STATE.with_borrow(|s| s.generation == generation);
            if still_current && let Some(token) = app.token.get_untracked() {
                connect(app, live, token);
            }
        },
        Duration::from_millis(u64::from(delay)),
    );
    if let Ok(handle) = handle {
        STATE.with_borrow_mut(|s| {
            if let Some(old) = s.retry.replace(handle) {
                old.clear();
            }
        });
    }
}

/// A random byte from `crypto.getRandomValues` for backoff jitter (0 if
/// the browser has none).
#[must_use]
pub fn jitter() -> u8 {
    let mut byte = [0_u8; 1];
    web_sys::window()
        .and_then(|w| w.crypto().ok())
        .and_then(|c| c.get_random_values_with_u8_array(&mut byte).ok());
    byte[0]
}

/// Reconnects straight away when the phone comes back online, or back to
/// the foreground after a while (its socket may have died silently).
pub(super) fn watch_foreground(app: AppState, live: Live) {
    let reconnect_now = move || {
        if let Some(token) = app.token.get_untracked() {
            STATE.with_borrow_mut(|s| s.attempt = 0);
            connect(app, live, token);
        }
    };
    // Heartbeat. The bridge's pings are invisible to JS, and a quiet stream
    // is normal, so silence alone proves nothing: after 30 s of it, compare
    // the list with one HTTP read. A difference (missed events) or no
    // answer means the socket is dead; reconnect for a fresh snapshot.
    let _ = set_interval_with_handle(
        move || {
            let now = js_sys::Date::now();
            let silent = STATE.with_borrow(|s| now - s.last_message >= SILENT_MS);
            if !silent || live.conn.get_untracked() != Conn::Live || !page_visible() {
                return;
            }
            STATE.with_borrow_mut(|s| s.last_message = now);
            spawn_local(async move {
                let stale = match session::api(app).agents().await {
                    Ok(list) => live.agents.with_untracked(|held| *held != list),
                    Err(e) => e.is_retryable(),
                };
                if stale {
                    reconnect_now();
                }
            });
        },
        HEARTBEAT,
    );
    // App-lifetime listeners: the handles are intentionally never removed.
    let _ = window_event_listener(leptos::ev::online, move |_| {
        if live.conn.get_untracked() != Conn::Live {
            reconnect_now();
        }
    });
    let _ = window_event_listener(leptos::ev::visibilitychange, move |_| {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let now = js_sys::Date::now();
        if document.visibility_state() == VisibilityState::Hidden {
            STATE.with_borrow_mut(|s| s.hidden_since = Some(now));
            return;
        }
        let hidden_for = STATE
            .with_borrow_mut(|s| s.hidden_since.take())
            .map_or(0.0, |since| now - since);
        if live.conn.get_untracked() != Conn::Live || hidden_for >= STALE_AFTER_HIDDEN_MS {
            reconnect_now();
        }
    });
}

fn page_visible() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .is_none_or(|d| d.visibility_state() == VisibilityState::Visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_url_follows_the_page_scheme() {
        assert_eq!(
            stream_url("http:", "192.0.2.10:8443"),
            "ws://192.0.2.10:8443/v1/stream"
        );
        assert_eq!(
            stream_url("https:", "bridge.example"),
            "wss://bridge.example/v1/stream"
        );
    }

    #[test]
    fn backoff_doubles_caps_and_jitters() {
        assert_eq!(backoff_ms(0, 0), 1_000);
        assert_eq!(backoff_ms(1, 0), 2_000);
        assert_eq!(backoff_ms(4, 0), 16_000);
        assert_eq!(backoff_ms(5, 0), 30_000);
        assert_eq!(backoff_ms(99, 0), 30_000);
        assert_eq!(backoff_ms(0, 255), 1_250);
        assert_eq!(backoff_ms(99, 255), 37_500);
    }
}
