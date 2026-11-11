//! Which pages the app's webview may show. Only Passporter's own UI: the
//! shell's bundled Connect page until a bridge is paired, then only that
//! bridge's PWA. Every other
//! navigation is refused, so no other page ever runs where Tauri's IPC is
//! injected (design §8, "no page but Passporter's own reaches native IPC").
//!
//! Android does not ask about POST navigations, so form submissions are
//! fenced by the pages' CSP instead: the bridge's `form-action 'self'`
//! (`crates/bridge/src/server/headers.rs`) and the Connect page's
//! `form-action 'none'`.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use url::Url;

use crate::origin::BridgeOrigin;

/// The host Tauri serves the bundled pages from on Android
/// (`https://tauri.localhost`, with the HTTPS scheme the shell asks for).
pub const LOCAL_HOST: &str = "tauri.localhost";

/// Whether the webview may navigate to `url`, given the paired bridge.
///
/// Once paired, the bundled pages are refused too: nothing on the bridge's
/// pages can send the webview back to a page that may call app commands.
/// Only the shell opens its Bridge page then, through [`Guard`].
/// History navigations (Back) skip this check, so the Connect page leaves
/// history when it opens the bridge (`connect.js`).
#[must_use]
pub fn allows(url: &Url, bridge: Option<&BridgeOrigin>) -> bool {
    match bridge {
        None => is_local(url),
        Some(bridge) => bridge.contains(url),
    }
}

/// The navigation guard as the shell runs it: [`allows`] against the
/// paired bridge, plus one exception for the shell's own loads.
///
/// wry puts the shell's own loads (`navigate`, the first page) through the
/// guard too, so before the shell opens its Bridge page while paired it
/// names that exact URL ([`Guard::expect`]) and the guard lets that one
/// load through once. The Kotlin side refuses every navigation a page
/// starts to the Bridge page (`ShellWebViewClient`), so only the shell's
/// own load can use the allowance.
#[derive(Debug)]
pub struct Guard {
    bridge: Arc<OnceLock<BridgeOrigin>>,
    expected: Mutex<Option<Url>>,
}

impl Guard {
    /// The guard for the bridge in `bridge`, set once paired.
    #[must_use]
    pub fn new(bridge: Arc<OnceLock<BridgeOrigin>>) -> Self {
        Self {
            bridge,
            expected: Mutex::new(None),
        }
    }

    /// The paired bridge, if any.
    #[must_use]
    pub fn bridge(&self) -> Option<&BridgeOrigin> {
        self.bridge.get()
    }

    /// Whether the webview may load `url`. A load the shell expects passes
    /// once, then the allowance is used up.
    pub fn allows(&self, url: &Url) -> bool {
        if allows(url, self.bridge()) {
            return true;
        }
        let mut expected = self.expected();
        if expected.as_ref() == Some(url) {
            *expected = None;
            true
        } else {
            false
        }
    }

    /// Readies the guard for the shell's own load of `url`, replacing any
    /// earlier allowance not yet used. Nothing to do for a URL [`allows`]
    /// lets through anyway.
    pub fn expect(&self, url: &Url) {
        if !allows(url, self.bridge()) {
            *self.expected() = Some(url.clone());
        }
    }

    /// Drops the allowance, for a load the shell could not start.
    pub fn unexpect(&self) {
        *self.expected() = None;
    }

    fn expected(&self) -> MutexGuard<'_, Option<Url>> {
        self.expected.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The shell's own bundled pages.
fn is_local(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str() == Some(LOCAL_HOST) && url.port().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(input: &str) -> Url {
        Url::parse(input).expect("valid url")
    }

    #[test]
    fn the_bundled_pages_are_allowed_until_paired() {
        let bridge = BridgeOrigin::parse("https://bridge.example.org").expect("origin");
        for page in [
            "https://tauri.localhost/",
            "https://tauri.localhost/index.html",
        ] {
            assert!(allows(&url(page), None), "{page}");
            assert!(!allows(&url(page), Some(&bridge)), "{page}");
        }
    }

    #[test]
    fn the_paired_bridge_is_allowed_once_paired() {
        let bridge = BridgeOrigin::parse("https://bridge.example.org").expect("origin");
        let page = url("https://bridge.example.org/agents/w1:p1");
        assert!(!allows(&page, None));
        assert!(allows(&page, Some(&bridge)));
        // A blob the PWA made carries the bridge's origin, and its CSP.
        assert!(allows(
            &url("blob:https://bridge.example.org/1b4f"),
            Some(&bridge)
        ));
    }

    fn paired_guard() -> Guard {
        let bridge = Arc::new(OnceLock::new());
        let _ = bridge.set(BridgeOrigin::parse("https://bridge.example.org").expect("origin"));
        Guard::new(bridge)
    }

    #[test]
    fn an_expected_navigation_passes_once_and_exactly() {
        let guard = paired_guard();
        let page = url("https://tauri.localhost/bridge.html?unreachable");
        assert!(!guard.allows(&page), "nothing is expected at first");
        guard.expect(&page);
        for other in [
            "https://tauri.localhost/bridge.html",
            "https://tauri.localhost/index.html",
            "https://tauri.localhost/bridge.html?unreachable#x",
            "https://example.com/",
        ] {
            assert!(!guard.allows(&url(other)), "{other}");
        }
        assert!(guard.allows(&page));
        assert!(!guard.allows(&page), "used up");
        guard.expect(&url("https://tauri.localhost/bridge.html"));
        guard.expect(&page);
        assert!(
            !guard.allows(&url("https://tauri.localhost/bridge.html")),
            "replaced"
        );
        assert!(guard.allows(&page));
    }

    #[test]
    fn an_allowance_can_be_dropped() {
        let guard = paired_guard();
        let page = url("https://tauri.localhost/bridge.html");
        guard.expect(&page);
        guard.unexpect();
        assert!(!guard.allows(&page));
    }

    #[test]
    fn the_guard_still_allows_the_bridge_and_nothing_else() {
        let guard = paired_guard();
        let agent = url("https://bridge.example.org/agent/w1:p1");
        guard.expect(&agent);
        assert!(guard.allows(&agent));
        assert!(guard.allows(&agent), "the bridge needs no allowance");
        guard.expect(&url("https://tauri.localhost/bridge.html"));
        assert!(
            guard.allows(&agent),
            "an allowance does not narrow the bridge"
        );
        let unpaired = Guard::new(Arc::new(OnceLock::new()));
        assert!(unpaired.allows(&url("https://tauri.localhost/index.html")));
        assert!(!unpaired.allows(&agent));
    }

    #[test]
    fn everything_else_is_refused() {
        let bridge = BridgeOrigin::parse("https://bridge.example.org").expect("origin");
        for page in [
            "https://example.com/",
            "https://bridge.example.org.evil.example/",
            "http://bridge.example.org/",
            "https://bridge.example.org:8443/",
            "http://tauri.localhost/",
            "https://tauri.localhost:8443/",
            "https://evil.tauri.localhost/",
            "intent://scan/#Intent;scheme=zxing;end",
            "javascript:alert(1)",
            "data:text/html,<p>hi</p>",
            "file:///sdcard/page.html",
            "about:blank",
            "blob:https://evil.example/1b4f",
        ] {
            assert!(!allows(&url(page), Some(&bridge)), "{page}");
        }
    }
}
