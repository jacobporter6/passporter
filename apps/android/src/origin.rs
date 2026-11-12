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
    /// A name that is not an RFC 1123 host name (labels of letters, digits
    /// and inner `-`, once in punycode, the last one starting with a
    /// letter), or a `localhost` name, which on the phone means the phone
    /// itself (`tauri.localhost` is the app's own pages).
    #[error("use the bridge's DNS name")]
    Host,
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
            Some(Host::Domain(name)) if !is_dns_name(name) => return Err(OriginError::Host),
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
    /// Hosts are plain DNS names (letters, digits, `-`, `.`) or IPv4
    /// addresses, so they hold no `URLPattern` syntax (`*`, `(`, `{`, `+`,
    /// `:` and brackets are all refused by [`BridgeOrigin::parse`]).
    #[must_use]
    pub fn capability_pattern(&self) -> String {
        format!("{}*", self.0)
    }
}

/// Whether `name` (already lowercased and punycoded by `url`) is a host
/// name that can be a remote bridge: RFC 1123 labels (letters, digits and
/// `-`, never first or last) with a last label that starts with a letter,
/// and not `localhost` or under it.
///
/// The device plugin's Kotlin reads the saved origin back with the same
/// rule (`BridgeOrigin.isBridgeHost`, behind `java.net.URI`, whose host
/// grammar it is too), so native code finds every bridge the shell saves:
/// `bridge-origins.json` holds the cases both check.
fn is_dns_name(name: &str) -> bool {
    let label_ok = |label: &str| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    let top_ok = name
        .rsplit('.')
        .next()
        .is_some_and(|top| top.starts_with(|c: char| c.is_ascii_alphabetic()));
    name.split('.').all(label_ok) && top_ok && name != "localhost" && !name.ends_with(".localhost")
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
            ("*", OriginError::Host),
            ("https://*.example.org", OriginError::Host),
            ("https://a(b).example", OriginError::Host),
            ("https://a+.example", OriginError::Host),
            ("https://{a}.example", OriginError::Host),
            ("https://a..example", OriginError::Host),
            ("https://localhost:8443", OriginError::Host),
            ("https://tauri.localhost", OriginError::Host),
            ("https://bridge.localhost", OriginError::Host),
            ("https://-bridge.example", OriginError::Host),
            ("https://bridge-.example", OriginError::Host),
            ("https://bridge.-example", OriginError::Host),
            ("https://bridge.1lan", OriginError::Host),
            ("https://bridge.example.", OriginError::Host),
        ];
        for (input, expected) in cases {
            assert_eq!(BridgeOrigin::parse(input), Err(expected), "{input:?}");
        }
    }

    /// The cases the device plugin's Kotlin (`BridgeOriginTest`) checks too:
    /// the shell saves only origins native code can read back.
    #[test]
    fn agrees_with_the_native_reader() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Vectors {
            accept: Vec<String>,
            reject: Vec<String>,
        }
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/device/android/src/test/resources/bridge-origins.json"
        );
        let text = std::fs::read_to_string(path).expect("vector file");
        let vectors: Vectors = serde_json::from_str(&text).expect("vector json");
        assert!(!vectors.accept.is_empty() && !vectors.reject.is_empty());
        for input in vectors.accept {
            // As saved: the origin, unchanged.
            assert_eq!(origin(&input).to_string(), input, "{input:?}");
        }
        for input in vectors.reject {
            assert!(BridgeOrigin::parse(&input).is_err(), "{input:?}");
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
                "https://xn--brcke-lva.example",
                [
                    "https://xn--brcke-lva.example/agents",
                    "https://brücke.example/",
                ],
                [
                    "https://brcke.example/",
                    "https://evil.xn--brcke-lva.example/",
                    "https://xn--brcke-lva.example:8443/",
                    "https://tauri.localhost/",
                    "https://example/",
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
