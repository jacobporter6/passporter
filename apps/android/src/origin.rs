//! The paired bridge's origin: the one remote site the app loads and the
//! only one its native plugins answer.

use std::fmt;

use thiserror::Error;
use url::{Host, Url};

/// The HTTPS origin (`https://host[:port]`) of the bridge this app is
/// paired with.
///
/// It is always `https`, has no credentials, and keeps nothing past the
/// origin: a pasted `https://host/some/page?x` becomes `https://host/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeOrigin(Url);

/// Why an address cannot be the bridge's origin.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum OriginError {
    /// Nothing was entered.
    #[error("enter the bridge's address")]
    Empty,
    /// Not a URL at all.
    #[error("that is not a web address")]
    Invalid,
    /// `http:` or any other scheme: the app only talks HTTPS.
    #[error("the bridge must be an https:// address")]
    NotHttps,
    /// `https://user:pass@host`: credentials never go in the address.
    #[error("the address must not contain a user name or password")]
    Credentials,
    /// An IPv6 literal such as `https://[::1]`, which the app's IPC allowlist
    /// cannot express. Use the bridge's DNS name.
    #[error("use the bridge's name, not an IPv6 address")]
    Ipv6,
}

impl BridgeOrigin {
    /// Reads what the person typed or pasted. A bare host
    /// (`bridge.example.ts.net`) means `https://` that host.
    pub fn parse(input: &str) -> Result<Self, OriginError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(OriginError::Empty);
        }
        let url = if input.contains("://") {
            Url::parse(input)
        } else {
            Url::parse(&format!("https://{input}"))
        }
        .map_err(|_| OriginError::Invalid)?;
        Self::from_url(&url)
    }

    /// The origin of `url`, if it can be a bridge.
    fn from_url(url: &Url) -> Result<Self, OriginError> {
        if url.scheme() != "https" {
            return Err(OriginError::NotHttps);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(OriginError::Credentials);
        }
        match url.host() {
            None => return Err(OriginError::Invalid),
            Some(Host::Ipv6(_)) => return Err(OriginError::Ipv6),
            Some(Host::Domain(_) | Host::Ipv4(_)) => {}
        }
        let origin = url.origin().ascii_serialization();
        Url::parse(&origin)
            .map(Self)
            .map_err(|_| OriginError::Invalid)
    }

    /// The origin as a URL, `https://host[:port]/`: where the PWA starts.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.0
    }

    /// Whether `url` is on this origin (same scheme, host and port).
    #[must_use]
    pub fn contains(&self, url: &Url) -> bool {
        url.origin() == self.0.origin()
    }

    /// The Tauri remote-capability pattern that matches every page of this
    /// origin and nothing else: `https://host[:port]/*`.
    ///
    /// Hosts are ASCII (punycode) domains or IPv4 addresses, so they hold
    /// no `URLPattern` syntax; IPv6 literals, whose brackets and colons would,
    /// are refused by [`BridgeOrigin::parse`].
    #[must_use]
    pub fn capability_pattern(&self) -> String {
        format!("{}*", self.0)
    }
}

impl fmt::Display for BridgeOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.origin().ascii_serialization().as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri_utils::acl::RemoteUrlPattern;

    fn origin(input: &str) -> BridgeOrigin {
        BridgeOrigin::parse(input).expect("valid origin")
    }

    fn url(input: &str) -> Url {
        Url::parse(input).expect("valid url")
    }

    #[test]
    fn keeps_only_the_https_origin() {
        assert_eq!(
            origin("https://bridge.example.ts.net").url().as_str(),
            "https://bridge.example.ts.net/"
        );
        assert_eq!(
            origin("  https://Bridge.Example.org:8443/agents/w1?x=1#top ")
                .url()
                .as_str(),
            "https://bridge.example.org:8443/"
        );
        assert_eq!(
            origin("https://bridge.example.org:443").to_string(),
            "https://bridge.example.org"
        );
        assert_eq!(
            origin("https://10.0.0.2:8443").to_string(),
            "https://10.0.0.2:8443"
        );
    }

    #[test]
    fn a_bare_host_means_https() {
        assert_eq!(
            origin("bridge.example.org").url().as_str(),
            "https://bridge.example.org/"
        );
    }

    #[test]
    fn international_names_become_punycode() {
        assert_eq!(
            origin("https://brücke.example").to_string(),
            "https://xn--brcke-lva.example"
        );
    }

    #[test]
    fn refuses_what_cannot_be_a_bridge() {
        let cases = [
            ("", OriginError::Empty),
            ("   ", OriginError::Empty),
            ("http://bridge.example.org", OriginError::NotHttps),
            ("ftp://bridge.example.org", OriginError::NotHttps),
            ("javascript:alert(1)", OriginError::Invalid),
            ("file:///etc/passwd", OriginError::NotHttps),
            ("https://me:pw@bridge.example.org", OriginError::Credentials),
            ("https://me@bridge.example.org", OriginError::Credentials),
            ("https://[::1]:8443", OriginError::Ipv6),
            ("https://", OriginError::Invalid),
            ("https://exa mple.org", OriginError::Invalid),
        ];
        for (input, expected) in cases {
            assert_eq!(BridgeOrigin::parse(input), Err(expected), "{input:?}");
        }
    }

    #[test]
    fn contains_only_the_same_origin() {
        let bridge = origin("https://bridge.example.org:8443");
        assert!(bridge.contains(&url("https://bridge.example.org:8443/agents/w1:p1")));
        for other in [
            "https://bridge.example.org/",
            "http://bridge.example.org:8443/",
            "https://bridge.example.org.evil.example:8443/",
            "https://evil.example/?u=https://bridge.example.org:8443/",
            "https://sub.bridge.example.org:8443/",
        ] {
            assert!(!bridge.contains(&url(other)), "{other}");
        }
    }

    /// Checks the pattern with Tauri's own matcher, so the IPC allowlist is
    /// exactly the origin: every page on it, nothing beside it.
    #[test]
    fn the_capability_pattern_matches_exactly_the_origin() {
        for (bridge, inside, outside) in [
            (
                "https://bridge.example.org",
                [
                    "https://bridge.example.org/",
                    "https://bridge.example.org/agents/w1:p1?tab=tldr#end",
                ],
                [
                    "https://bridge.example.org:8443/",
                    "http://bridge.example.org/",
                    "https://bridge.example.org.evil.example/",
                    "https://sub.bridge.example.org/",
                    "https://evil.example/https://bridge.example.org/",
                ],
            ),
            (
                "https://10.0.0.2:8443",
                ["https://10.0.0.2:8443/", "https://10.0.0.2:8443/settings"],
                [
                    "https://10.0.0.2/",
                    "https://10.0.0.20:8443/",
                    "https://10.0.0.2:18443/",
                    "http://10.0.0.2:8443/",
                    "https://tauri.localhost/",
                ],
            ),
        ] {
            let bridge = origin(bridge);
            let pattern: RemoteUrlPattern = bridge
                .capability_pattern()
                .parse()
                .expect("Tauri accepts the pattern");
            for page in inside {
                assert!(pattern.test(&url(page)), "{bridge} should match {page}");
            }
            for page in outside {
                assert!(!pattern.test(&url(page)), "{bridge} must not match {page}");
            }
        }
    }
}
