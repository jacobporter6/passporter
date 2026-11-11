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

use url::Url;

use crate::origin::BridgeOrigin;

/// The host Tauri serves the bundled pages from on Android
/// (`https://tauri.localhost`, with the HTTPS scheme the shell asks for).
pub const LOCAL_HOST: &str = "tauri.localhost";

/// Whether the webview may navigate to `url`, given the paired bridge.
///
/// Once paired, the bundled pages are refused too: nothing on the bridge's
/// pages can send the webview back to a page that may call app commands.
#[must_use]
pub fn allows(url: &Url, bridge: Option<&BridgeOrigin>) -> bool {
    match bridge {
        None => is_local(url),
        Some(bridge) => bridge.contains(url),
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
        ] {
            assert!(!allows(&url(page), Some(&bridge)), "{page}");
        }
    }
}
