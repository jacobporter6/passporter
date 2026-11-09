//! The client's address, honouring `X-Forwarded-For` only from the trusted
//! proxy (the "edge").
//!
//! In production every connection comes from the HTTPS reverse proxy (design §3), so
//! the TCP peer alone says nothing about the phone. The edge appends the
//! address it saw to `X-Forwarded-For`; the **right-most** entry is therefore
//! the one the edge vouches for, and anything to its left was supplied by
//! the client. The header is ignored entirely from any other peer.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;

const X_FORWARDED_FOR: &str = "x-forwarded-for";

/// Where a request really came from. Every request carries one as an
/// extension, for logging, auth and (later) rate limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientAddr(pub IpAddr);

impl fmt::Display for ClientAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ClientAddr {
    /// The client behind `peer`: the right-most `X-Forwarded-For` entry when
    /// `peer` is the trusted proxy and that entry is an address, else `peer`.
    #[must_use]
    pub fn resolve(peer: IpAddr, headers: &HeaderMap, trusted_proxy: Option<IpAddr>) -> Self {
        let peer = peer.to_canonical();
        if trusted_proxy.map(|ip| ip.to_canonical()) != Some(peer) {
            return Self(peer);
        }
        let forwarded = headers
            .get_all(X_FORWARDED_FOR)
            .iter()
            .next_back()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit(',').next())
            .and_then(parse_entry);
        Self(forwarded.unwrap_or(peer))
    }
}

/// One `X-Forwarded-For` entry: an IP, optionally with a port.
fn parse_entry(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    entry
        .parse::<IpAddr>()
        .or_else(|_| entry.parse::<SocketAddr>().map(|s| s.ip()))
        .ok()
        .map(|ip| ip.to_canonical())
}

/// Middleware: attaches the request's [`ClientAddr`].
pub(crate) async fn attach(
    State(trusted_proxy): State<Option<IpAddr>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let client = ClientAddr::resolve(peer.ip(), request.headers(), trusted_proxy);
    request.extensions_mut().insert(client);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    const EDGE: &str = "192.0.2.46";

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for v in values {
            map.append(X_FORWARDED_FOR, HeaderValue::from_str(v).expect("header"));
        }
        map
    }

    #[test]
    fn edge_forwarded_address_is_used() {
        let client = ClientAddr::resolve(ip(EDGE), &headers(&["198.51.100.2"]), Some(ip(EDGE)));
        assert_eq!(client, ClientAddr(ip("198.51.100.2")));
    }

    #[test]
    fn only_the_right_most_entry_is_trusted() {
        let spoofed = headers(&["1.2.3.4, 198.51.100.2"]);
        let client = ClientAddr::resolve(ip(EDGE), &spoofed, Some(ip(EDGE)));
        assert_eq!(client.0, ip("198.51.100.2"));
        let two_headers = headers(&["1.2.3.4", "198.51.100.2"]);
        let client = ClientAddr::resolve(ip(EDGE), &two_headers, Some(ip(EDGE)));
        assert_eq!(client.0, ip("198.51.100.2"));
    }

    #[test]
    fn other_peers_cannot_forward() {
        let client =
            ClientAddr::resolve(ip("10.0.0.9"), &headers(&["198.51.100.2"]), Some(ip(EDGE)));
        assert_eq!(client.0, ip("10.0.0.9"));
        let client = ClientAddr::resolve(ip(EDGE), &headers(&["198.51.100.2"]), None);
        assert_eq!(client.0, ip(EDGE), "no trusted proxy configured");
    }

    #[test]
    fn garbage_or_missing_header_falls_back_to_the_peer() {
        for h in [
            headers(&[]),
            headers(&["not-an-ip"]),
            headers(&["10.0.0.1, "]),
        ] {
            assert_eq!(
                ClientAddr::resolve(ip(EDGE), &h, Some(ip(EDGE))).0,
                ip(EDGE)
            );
        }
    }

    #[test]
    fn ports_and_mapped_addresses_are_normalized() {
        let client = ClientAddr::resolve(
            ip("::ffff:192.0.2.46"),
            &headers(&["[2001:db8::1]:443"]),
            Some(ip(EDGE)),
        );
        assert_eq!(client.0, ip("2001:db8::1"));
        let client =
            ClientAddr::resolve(ip(EDGE), &headers(&["198.51.100.2:5555"]), Some(ip(EDGE)));
        assert_eq!(client.to_string(), "198.51.100.2");
    }
}
