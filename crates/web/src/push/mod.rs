//! Web Push on the client (design §6.6): the Settings toggle that subscribes
//! this browser to the bridge's content-free wake pings, and the wake
//! itself.
//!
//! - [`state`]: the toggle's state machine. [`browser`]: the Push API and
//!   bridge calls behind it, which run in the notice WASM; [`module`] loads
//!   that into the page on demand.
//! - [`notice`]: the notification text, from the agent list.
//! - [`wake()`]: what the service worker runs on a ping. `public/sw.js` loads
//!   it from the tiny `notice` WASM (`src/bin/notice.rs`) and only shows the
//!   [`Notice`] it returns.
//! - [`open_from_notifications`]: routes a notification tap in an open
//!   window.
//!
//! A ping's payload is an opaque id and is never read: the wake fetches
//! `GET /v1/agents` with this device's stored token instead, so no agent
//! content transits the push service.

pub mod browser;
pub mod module;
pub mod notice;
pub mod serial;
pub mod state;
mod wake;

use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

pub use notice::Notice;
pub use state::PushState;
pub use wake::wake;

use crate::api::{ApiClient, BearerToken, MemoryToken, TimedFetch};
use crate::nav::{History, Nav};
use crate::platform;
use crate::routes::AppRoute;

/// The key of the in-app path in the message `sw.js` posts to an open
/// window when its notification is tapped: `{"open": "/agent/…"}`.
const OPEN_MESSAGE_KEY: &str = "open";

/// How long the notice WASM waits for one bridge request. Its work holds a
/// lock (one sign-in per wake, one subscription change across tabs), so a
/// hung tunnel must not hold it for longer. Matches `sw.js`'s wake budget.
const BRIDGE_TIMEOUT_MS: u32 = 8_000;

/// The notice WASM's transport: `fetch`, bounded by [`BRIDGE_TIMEOUT_MS`].
const TRANSPORT: TimedFetch = TimedFetch(BRIDGE_TIMEOUT_MS);

/// A bridge client for the notice WASM, which gets this device's current
/// token from the page with each call (see [`module`]).
#[must_use]
pub fn client(token: Option<String>) -> ApiClient<TimedFetch, MemoryToken> {
    ApiClient::new(
        TRANSPORT,
        MemoryToken::maybe(token.and_then(|raw| BearerToken::new(raw).ok())),
    )
}

/// Listens for the service worker's "open this" message (a notification
/// tapped while the app is open) and navigates there in-app, without a
/// reload. Paths that aren't app routes are ignored. Call it once, at the
/// app root.
pub fn open_from_notifications(nav: Nav) {
    let Some(container) = platform::service_workers() else {
        return;
    };
    let on_message =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            let route = js_sys::Reflect::get(&ev.data(), &JsValue::from_str(OPEN_MESSAGE_KEY))
                .ok()
                .and_then(|path| path.as_string())
                .and_then(|path| AppRoute::parse(&path));
            if let Some(route) = route {
                nav.go(&route, History::Push);
            }
        });
    container.set_onmessage(Some(wasm_bindgen::JsCast::unchecked_ref(
        on_message.as_ref(),
    )));
    // App-lifetime handler, like the app root.
    on_message.forget();
}
