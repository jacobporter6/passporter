//! Where the shell sends the webview when the app's Kotlin side asks: a
//! notification tap, the launcher's *Bridge* shortcut, or a bridge page that
//! failed to load.
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
/// `{"kind":"manageBridge"}` or `{"kind":"loadFailed"}`.
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
}

/// The URL to show for `request`, or `None` to ignore it: before a bridge
/// is paired the Connect page stays, whatever native code asks.
#[must_use]
pub fn destination(request: &Request, bridge: Option<&BridgeOrigin>) -> Option<Url> {
    let bridge = bridge?;
    match request {
        Request::OpenAgent { agent } => {
            let mut url = bridge.url().clone();
            url.set_path(&format!("/{AGENT_SEGMENT}/{agent}"));
            // Always true for a valid id; checked so no change here can
            // send the webview off the bridge.
            navigation::allows(&url, Some(bridge)).then_some(url)
        }
        Request::ManageBridge {} => local_page(BRIDGE_PAGE, None),
        Request::LoadFailed {} => local_page(BRIDGE_PAGE, Some(UNREACHABLE_QUERY)),
    }
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
            let url = destination(&open(agent), Some(&bridge)).expect("a destination");
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
        assert_eq!(
            destination(&Request::ManageBridge {}, Some(&bridge)).map(String::from),
            Some("https://tauri.localhost/bridge.html".to_owned())
        );
        assert_eq!(
            destination(&Request::LoadFailed {}, Some(&bridge)).map(String::from),
            Some("https://tauri.localhost/bridge.html?unreachable".to_owned())
        );
    }

    #[test]
    fn nothing_moves_before_a_bridge_is_paired() {
        for request in [
            open("w1:p1"),
            Request::ManageBridge {},
            Request::LoadFailed {},
        ] {
            assert_eq!(destination(&request, None), None, "{request:?}");
        }
    }
}
