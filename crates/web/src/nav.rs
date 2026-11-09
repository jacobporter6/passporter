//! Client-side navigation over [`AppRoute`].
//!
//! The app has four screens and one path parameter, already parsed by
//! [`AppRoute::parse`], so it routes itself instead of using a general
//! router (whose matching machinery cost about 50 KB of WASM): the current
//! path lives in a signal that follows the History API, and clicks on
//! same-origin links become in-app navigations.

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Element, MouseEvent, Url};

use crate::routes::AppRoute;

/// The current location. [`provide`] it at the app root, read it anywhere
/// with [`use_nav`].
#[derive(Debug, Clone, Copy)]
pub struct Nav {
    /// `location.pathname`.
    path: RwSignal<String>,
    /// The screen at that path; `None` for an unknown path.
    route: Memo<Option<AppRoute>>,
}

/// How a navigation treats the session history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum History {
    /// Add an entry: Back returns here.
    Push,
    /// Replace the current entry.
    Replace,
}

impl Nav {
    /// The current screen (tracked); `None` for an unknown path.
    #[must_use]
    pub fn route(self) -> Memo<Option<AppRoute>> {
        self.route
    }

    /// Goes to `route` and scrolls to the top.
    pub fn go(self, route: &AppRoute, history: History) {
        self.go_to(&route.href(), history);
    }

    /// Goes to `url`, a same-origin path (with any query and fragment),
    /// and scrolls to the top.
    fn go_to(self, url: &str, history: History) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let Ok(session) = window.history() else {
            return;
        };
        // Following a link to where you are adds no history entry.
        let push = history == History::Push && current_url().as_deref() != Some(url);
        let changed = if push {
            session.push_state_with_url(&JsValue::NULL, "", Some(url))
        } else {
            session.replace_state_with_url(&JsValue::NULL, "", Some(url))
        };
        if changed.is_ok() {
            self.sync();
            window.scroll_to_with_x_and_y(0.0, 0.0);
        }
    }

    /// Reads the path back from `location`.
    fn sync(self) {
        self.path.set(current_path());
    }
}

/// Creates the app's [`Nav`] from the current URL, keeps it current on
/// Back and Forward, routes clicks on in-app links through it, and provides
/// it as context. App-lifetime: the listeners are never removed.
pub fn provide() -> Nav {
    let path = RwSignal::new(current_path());
    let route = Memo::new(move |_| path.with(|p| AppRoute::parse(p)));
    let nav = Nav { path, route };
    provide_context(nav);
    let _ = window_event_listener(leptos::ev::popstate, move |_| nav.sync());
    let _ = window_event_listener(leptos::ev::click, move |ev| follow_link(nav, &ev));
    nav
}

/// The app's [`Nav`].
///
/// # Panics
///
/// Outside the app root that [`provide`]s it.
#[must_use]
pub fn use_nav() -> Nav {
    expect_context()
}

/// The current path, query and fragment.
fn current_url() -> Option<String> {
    let location = web_sys::window()?.location();
    let (path, search, hash) = (location.pathname(), location.search(), location.hash());
    Some(format!("{}{}{}", path.ok()?, search.ok()?, hash.ok()?))
}

fn current_path() -> String {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .unwrap_or_else(|| "/".to_owned())
}

/// Turns a click on a same-origin link into an in-app navigation: a plain
/// primary-button click (no modifier keys, not already handled) on an
/// `<a href>` without `target`, `download` or `rel="external"`. Anything
/// else keeps the browser's default.
fn follow_link(nav: Nav, ev: &MouseEvent) {
    if ev.default_prevented()
        || ev.button() != 0
        || ev.meta_key()
        || ev.alt_key()
        || ev.ctrl_key()
        || ev.shift_key()
    {
        return;
    }
    let Some(link) = ev
        .target()
        .and_then(|t| t.dyn_into::<Element>().ok())
        .and_then(|el| el.closest("a[href]").ok().flatten())
    else {
        return;
    };
    if leaves_the_app(
        link.get_attribute("target").as_deref(),
        link.has_attribute("download"),
        link.get_attribute("rel").as_deref(),
    ) {
        return;
    }
    let Some(window) = web_sys::window() else {
        return;
    };
    let location = window.location();
    let (Ok(base), Ok(origin), Some(href)) = (
        location.href(),
        location.origin(),
        link.get_attribute("href"),
    ) else {
        return;
    };
    let Ok(url) = Url::new_with_base(&href, &base) else {
        return;
    };
    if url.origin() != origin {
        return;
    }
    ev.prevent_default();
    let path = format!("{}{}{}", url.pathname(), url.search(), url.hash());
    nav.go_to(&path, History::Push);
}

/// Whether a link's attributes ask the browser to handle it: another
/// browsing context (`target`), a download, or `rel="external"`.
fn leaves_the_app(target: Option<&str>, download: bool, rel: Option<&str>) -> bool {
    target.is_some_and(|t| !t.is_empty() && t != "_self")
        || download
        || rel.is_some_and(|rel| rel.split_ascii_whitespace().any(|r| r == "external"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_links_stay_in_the_app() {
        assert!(!leaves_the_app(None, false, None));
        assert!(!leaves_the_app(Some(""), false, None));
        assert!(!leaves_the_app(Some("_self"), false, Some("noopener")));
    }

    #[test]
    fn targets_downloads_and_external_links_leave() {
        assert!(leaves_the_app(Some("_blank"), false, None));
        assert!(leaves_the_app(None, true, None));
        assert!(leaves_the_app(None, false, Some("noopener external")));
    }
}
