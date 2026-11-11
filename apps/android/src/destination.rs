//! Where the shell sends the webview: the page it opens on ([`FirstPage`]),
//! and where the app's Kotlin side asks for later: a notification tap, the
//! launcher's *Bridge* shortcut, a bridge page that failed to load, or a
//! webview wry rebuilt.
//!
//! The Kotlin side only names a [`Request`]; every URL is built here, from
//! the saved bridge origin and nothing else. wry checks the shell's own
//! loads against the navigation guard too, so the shell lets the Bridge
//! page through once by its exact URL ([`crate::navigation::Guard`]);
//! the bridge's pages still cannot navigate to the bundled pages themselves.

use protocol::AgentId;
use serde::Deserialize;
use url::Url;

use crate::navigation::{self, LOCAL_HOST};
use crate::origin::BridgeOrigin;

/// The bundled Connect page, which asks for the bridge's address
/// (`connect/index.html`).
pub const CONNECT_PAGE: &str = "index.html";

/// The bundled Bridge page: the address, *Retry* or *Open*, and *Change
/// address* (`connect/bridge.html`).
pub const BRIDGE_PAGE: &str = "bridge.html";

/// The query that makes the Bridge page say the bridge did not answer.
pub const UNREACHABLE_QUERY: &str = "unreachable";

/// The path segment of an agent's screen in the PWA (`crates/web/src/routes.rs`
/// `AGENT_SEGMENT`): `/agent/<id>`. The id's charset is path-safe, so it
/// goes in as is.
const AGENT_SEGMENT: &str = "agent";

/// What the Kotlin side (`ShellRequest.kt`) asks the shell to show, as the
/// JSON it sends: `{"kind":"openAgent","agent":"w1:p1"}`,
/// `{"kind":"manageBridge"}`, `{"kind":"loadFailed"}` or
/// `{"kind":"webViewRebuilt"}`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Request {
    /// A notification tap: that agent's screen on the bridge.
    OpenAgent {
        /// The agent, checked against the bridge's id rules.
        agent: AgentId,
    },
    /// The launcher's *Bridge* shortcut: the Bridge page. (Written `{}`, as
    /// is [`Request::LoadFailed`], so serde refuses unknown fields on it.)
    ManageBridge {},
    /// The bridge's page did not load (no network, the bridge down, a 5xx
    /// from its proxy): the Bridge page, saying so.
    LoadFailed {},
    /// wry built a new webview in the same process, and is loading the
    /// first page again ([`FirstPage::after_rebuild`]).
    WebViewRebuilt {},
}

/// The first page the shell gives wry.
///
/// wry loads it at launch, and loads it again whenever it rebuilds the
/// webview: Android re-creates the activity for a configuration change the
/// manifest does not list (a new font or display size, for one), and wry
/// builds the new webview from what it was first given. A process Android
/// ended and restores from Recents starts afresh instead, and chooses again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirstPage {
    /// The bundled Connect page ([`CONNECT_PAGE`]): no bridge was paired at
    /// launch.
    Connect,
    /// A page on the paired bridge: its start page, or the agent a
    /// notification tap asked for.
    Bridge(Url),
}

impl FirstPage {
    /// The first page, given the bridge paired at launch (if any) and what
    /// the intent that started the app asked for (`launch`). Also answers
    /// the request to show once that page has loaded: the Bridge page is
    /// never the first page, because the guard lets it through once only
    /// and wry may load the first page again. Before pairing, every request
    /// is ignored.
    #[must_use]
    pub fn choose(
        bridge: Option<&BridgeOrigin>,
        launch: Option<Request>,
    ) -> (Self, Option<Request>) {
        let Some(bridge) = bridge else {
            return (Self::Connect, None);
        };
        if let Some(Request::OpenAgent { agent }) = &launch
            && let Some(url) = agent_page(bridge, agent)
        {
            return (Self::Bridge(url), None);
        }
        (Self::Bridge(bridge.url().clone()), launch)
    }

    /// Where to send the webview once wry has rebuilt it, or `None` to let
    /// wry's reload of this page stand: a bridge page, or the Connect page
    /// until a bridge is paired. Once paired, the guard refuses the Connect
    /// page and the webview would stay blank, so the bridge's start page
    /// opens instead (and the Bridge page, if that fails to load).
    #[must_use]
    pub fn after_rebuild(&self, bridge: Option<&BridgeOrigin>) -> Option<Url> {
        match (self, bridge) {
            (Self::Connect, Some(bridge)) => Some(bridge.url().clone()),
            (Self::Connect, None) | (Self::Bridge(_), _) => None,
        }
    }
}

/// The URL to show for `request`, given the bridge and the webview's
/// `first` page, or `None` to ignore it: before a bridge is paired the
/// Connect page stays, whatever native code asks.
#[must_use]
pub fn destination(
    request: &Request,
    bridge: Option<&BridgeOrigin>,
    first: &FirstPage,
) -> Option<Url> {
    let bridge = bridge?;
    match request {
        Request::OpenAgent { agent } => agent_page(bridge, agent),
        Request::ManageBridge {} => local_page(BRIDGE_PAGE, None),
        Request::LoadFailed {} => local_page(BRIDGE_PAGE, Some(UNREACHABLE_QUERY)),
        Request::WebViewRebuilt {} => first.after_rebuild(Some(bridge)),
    }
}

/// `agent`'s screen on `bridge`.
fn agent_page(bridge: &BridgeOrigin, agent: &AgentId) -> Option<Url> {
    let mut url = bridge.url().clone();
    url.set_path(&format!("/{AGENT_SEGMENT}/{agent}"));
    // Always true for a valid id; checked so no change here can send the
    // webview off the bridge.
    navigation::allows(&url, Some(bridge)).then_some(url)
}

/// A bundled page, as Tauri serves it on Android: `https://tauri.localhost/<page>`.
fn local_page(page: &str, query: Option<&str>) -> Option<Url> {
    let mut url = Url::parse(&format!("https://{LOCAL_HOST}/{page}")).ok()?;
    url.set_query(query);
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> BridgeOrigin {
        BridgeOrigin::parse("https://bridge.example.org:8443").expect("origin")
    }

    fn request(json: &str) -> Request {
        serde_json::from_str(json).expect("a request")
    }

    /// The first page of a launch with `bridge` saved, and no request.
    fn on_bridge(bridge: &BridgeOrigin) -> FirstPage {
        FirstPage::Bridge(bridge.url().clone())
    }

    fn open(agent: &str) -> Request {
        Request::OpenAgent {
            agent: agent.parse().expect("agent id"),
        }
    }

    /// The JSON is what `ShellRequest.toJson` sends (`ShellRequestTest`).
    #[test]
    fn reads_what_the_kotlin_side_sends() {
        assert_eq!(
            request(r#"{"kind":"openAgent","agent":"w1:p1~9ca09978-ef89"}"#),
            open("w1:p1~9ca09978-ef89")
        );
        assert_eq!(
            request(r#"{"kind":"manageBridge"}"#),
            Request::ManageBridge {}
        );
        assert_eq!(request(r#"{"kind":"loadFailed"}"#), Request::LoadFailed {});
        assert_eq!(
            request(r#"{"kind":"webViewRebuilt"}"#),
            Request::WebViewRebuilt {}
        );
    }

    #[test]
    fn refuses_anything_else() {
        for json in [
            r#"{"kind":"openAgent","agent":"../settings"}"#,
            r#"{"kind":"openAgent","agent":"w1/p1"}"#,
            r#"{"kind":"openAgent","agent":"w1:p1?x=1"}"#,
            r#"{"kind":"openAgent","agent":""}"#,
            r#"{"kind":"openAgent"}"#,
            r#"{"kind":"openAgent","agent":"w1:p1","url":"https://evil.example/"}"#,
            r#"{"kind":"manageBridge","url":"https://evil.example/"}"#,
            r#"{"kind":"loadFailed","url":"https://evil.example/"}"#,
            r#"{"kind":"webViewRebuilt","url":"https://evil.example/"}"#,
            r#"{"kind":"navigate","url":"https://evil.example/"}"#,
            r#"{"agent":"w1:p1"}"#,
        ] {
            assert!(serde_json::from_str::<Request>(json).is_err(), "{json}");
        }
    }

    #[test]
    fn an_agent_opens_its_screen_on_the_bridge() {
        let bridge = bridge();
        for (agent, expected) in [
            ("w1:p1", "https://bridge.example.org:8443/agent/w1:p1"),
            (
                "w1:p1~9ca09978-ef89-46fd-9a85-118b84bee7b4",
                "https://bridge.example.org:8443/agent/w1:p1~9ca09978-ef89-46fd-9a85-118b84bee7b4",
            ),
        ] {
            let url = destination(&open(agent), Some(&bridge), &on_bridge(&bridge))
                .expect("a destination");
            assert_eq!(url.as_str(), expected);
            assert!(
                navigation::allows(&url, Some(&bridge)),
                "the guard allows {url}"
            );
        }
    }

    #[test]
    fn the_bridge_page_is_bundled() {
        let bridge = bridge();
        let first = on_bridge(&bridge);
        assert_eq!(
            destination(&Request::ManageBridge {}, Some(&bridge), &first).map(String::from),
            Some("https://tauri.localhost/bridge.html".to_owned())
        );
        assert_eq!(
            destination(&Request::LoadFailed {}, Some(&bridge), &first).map(String::from),
            Some("https://tauri.localhost/bridge.html?unreachable".to_owned())
        );
    }

    #[test]
    fn nothing_moves_before_a_bridge_is_paired() {
        for request in [
            open("w1:p1"),
            Request::ManageBridge {},
            Request::LoadFailed {},
            Request::WebViewRebuilt {},
        ] {
            assert_eq!(
                destination(&request, None, &FirstPage::Connect),
                None,
                "{request:?}"
            );
        }
    }

    #[test]
    fn the_first_page_is_the_connect_page_until_paired() {
        for launch in [None, Some(open("w1:p1")), Some(Request::ManageBridge {})] {
            assert_eq!(
                FirstPage::choose(None, launch.clone()),
                (FirstPage::Connect, None),
                "{launch:?}"
            );
        }
    }

    #[test]
    fn once_paired_the_first_page_is_on_the_bridge() {
        let bridge = bridge();
        assert_eq!(
            FirstPage::choose(Some(&bridge), None),
            (on_bridge(&bridge), None)
        );
        // A tap that started the app opens its agent at once.
        let (first, later) = FirstPage::choose(Some(&bridge), Some(open("w1:p1")));
        assert_eq!(
            first,
            FirstPage::Bridge(
                Url::parse("https://bridge.example.org:8443/agent/w1:p1").expect("url")
            )
        );
        assert_eq!(later, None);
        // The Bridge page waits for the first page: wry may load that again.
        for launch in [Request::ManageBridge {}, Request::LoadFailed {}] {
            assert_eq!(
                FirstPage::choose(Some(&bridge), Some(launch.clone())),
                (on_bridge(&bridge), Some(launch))
            );
        }
    }

    /// wry reloads the first page when it rebuilds the webview. Paired since
    /// launch, that page is on the bridge, and the guard allows it.
    #[test]
    fn a_rebuilt_webview_keeps_a_first_page_on_the_bridge() {
        let bridge = bridge();
        let agent = Url::parse("https://bridge.example.org:8443/agent/w1:p1").expect("url");
        for first in [on_bridge(&bridge), FirstPage::Bridge(agent)] {
            assert_eq!(first.after_rebuild(Some(&bridge)), None, "{first:?}");
            assert_eq!(
                destination(&Request::WebViewRebuilt {}, Some(&bridge), &first),
                None,
                "{first:?}"
            );
        }
        assert_eq!(FirstPage::Connect.after_rebuild(None), None);
    }

    /// Paired on the Connect page, wry's reload of it is refused by the
    /// guard: without this the screen stays blank.
    #[test]
    fn a_rebuilt_webview_paired_since_the_connect_page_opens_the_bridge() {
        let bridge = bridge();
        let url = destination(
            &Request::WebViewRebuilt {},
            Some(&bridge),
            &FirstPage::Connect,
        )
        .expect("a destination");
        assert_eq!(url.as_str(), "https://bridge.example.org:8443/");
        assert!(navigation::allows(&url, Some(&bridge)));
        let connect = local_page(CONNECT_PAGE, None).expect("the Connect page");
        assert!(
            !navigation::allows(&connect, Some(&bridge)),
            "what wry reloads is refused"
        );
    }
}
