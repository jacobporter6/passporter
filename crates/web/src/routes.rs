//! The app's routes, as one typed enum.
//!
//! Every in-app link is built from [`AppRoute::href`] and every path parameter
//! is parsed back through [`AppRoute::parse`], so a link and the router can
//! never disagree ([`crate::nav`] routes by it); the tests below pin the
//! round trip.

use core::fmt;

use protocol::AgentId;

/// Path segment of an agent's detail screen: `/agent/:id`.
pub const AGENT_SEGMENT: &str = "agent";
/// Path segment of the pairing screen.
pub const PAIR_SEGMENT: &str = "pair";
/// Path segment of the settings screen.
pub const SETTINGS_SEGMENT: &str = "settings";

/// A screen of the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRoute {
    /// Blocked / Working / Done segments (design §6.1).
    Triage,
    /// One agent's thread and reply surface.
    Agent(AgentId),
    /// Device pairing: QR scan or manual code entry (design §8).
    Pair,
    /// Device, storage and app information.
    Settings,
}

impl AppRoute {
    /// The path to link to.
    #[must_use]
    pub fn href(&self) -> String {
        self.to_string()
    }

    /// Parses an in-app path. Query strings and fragments are ignored; a
    /// trailing slash is tolerated. Returns `None` for unknown paths and for
    /// agent ids that fail validation, which the router renders as "not
    /// found".
    #[must_use]
    pub fn parse(path: &str) -> Option<Self> {
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let path = match path.strip_suffix('/') {
            Some(trimmed) if !trimmed.is_empty() => trimmed,
            _ => path,
        };
        let rest = path.strip_prefix('/')?;
        match rest.split_once('/') {
            None if rest.is_empty() => Some(Self::Triage),
            None if rest == PAIR_SEGMENT => Some(Self::Pair),
            None if rest == SETTINGS_SEGMENT => Some(Self::Settings),
            Some((AGENT_SEGMENT, id)) => Self::agent_from_param(id).map(Self::Agent),
            _ => None,
        }
    }

    /// Parses the `:id` parameter of the agent route. The [`AgentId`]
    /// charset is URL-path safe, so no percent-decoding is needed, and
    /// anything outside it (including `/`) is rejected.
    #[must_use]
    pub fn agent_from_param(param: &str) -> Option<AgentId> {
        param.parse().ok()
    }
}

impl fmt::Display for AppRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Triage => f.write_str("/"),
            Self::Agent(id) => write!(f, "/{AGENT_SEGMENT}/{id}"),
            Self::Pair => write!(f, "/{PAIR_SEGMENT}"),
            Self::Settings => write!(f, "/{SETTINGS_SEGMENT}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str) -> AgentId {
        id.parse().unwrap()
    }

    #[test]
    fn hrefs_round_trip() {
        let routes = [
            AppRoute::Triage,
            AppRoute::Pair,
            AppRoute::Settings,
            AppRoute::Agent(agent("w6:p1")),
            AppRoute::Agent(agent("w6:p1~9ca09978-ef89-46fd-9a85-118b84bee7b4")),
        ];
        for route in routes {
            assert_eq!(
                AppRoute::parse(&route.href()),
                Some(route.clone()),
                "{route}"
            );
        }
    }

    #[test]
    fn agent_href_embeds_the_wire_id() {
        let route = AppRoute::Agent(agent("w6:p1~abc"));
        assert_eq!(route.href(), "/agent/w6:p1~abc");
    }

    #[test]
    fn query_fragment_and_trailing_slash_are_ignored() {
        assert_eq!(AppRoute::parse("/pair?code=x"), Some(AppRoute::Pair));
        assert_eq!(AppRoute::parse("/settings/"), Some(AppRoute::Settings));
        assert_eq!(AppRoute::parse("/#top"), Some(AppRoute::Triage));
        assert_eq!(
            AppRoute::parse("/agent/w1:p2/"),
            Some(AppRoute::Agent(agent("w1:p2")))
        );
    }

    #[test]
    fn unknown_paths_and_bad_ids_do_not_match() {
        for path in [
            "",
            "/nope",
            "/agent/",
            "/agent/..",
            "/agent/a/b",
            "/agent/<script>",
            "/agent/w1:p1~",
            "/agent/%2e%2e",
        ] {
            assert_eq!(AppRoute::parse(path), None, "{path:?}");
        }
    }
}
