//! MVP auth: one static bearer token from the config (`api_token`).
//!
//! A fallback for clients without a secure context (the device-key flow
//! needs `WebCrypto`, so plain HTTP over `WireGuard` cannot use it) until TLS
//! lands. It is only accepted when the config opts in with
//! `allow_static_token = true`; see the crate README, "MVP mode". The token
//! is compared in constant time and is never logged (its `Debug` is
//! redacted).

use std::collections::BTreeSet;
use std::fmt;

use axum::http::header;
use serde::Deserialize;

use super::auth::{AuthFuture, AuthRejection, AuthRequest, Authenticator, Principal};

/// Shortest token accepted. `deploy/bridge/setup.sh` generates 64 hex chars.
pub const MIN_TOKEN_CHARS: usize = 32;

/// Longest token accepted (keeps headers and the WS auth message small).
pub const MAX_TOKEN_CHARS: usize = 256;

/// The device name every request authenticated by [`StaticToken`] gets.
pub const STATIC_DEVICE: &str = "static-token";

/// A bearer token: [`MIN_TOKEN_CHARS`]..=[`MAX_TOKEN_CHARS`] visible ASCII
/// characters (no spaces), so it fits an `Authorization` header verbatim.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct ApiToken(String);

/// Why a configured token was rejected. Never contains the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidToken {
    /// Too short or too long.
    #[error("api_token must be {MIN_TOKEN_CHARS} to {MAX_TOKEN_CHARS} characters")]
    Length,
    /// Contains whitespace, control or non-ASCII characters.
    #[error("api_token may only contain visible ASCII characters")]
    Charset,
}

impl ApiToken {
    /// Validates `token`.
    pub fn new(token: impl Into<String>) -> Result<Self, InvalidToken> {
        let token = token.into();
        if !(MIN_TOKEN_CHARS..=MAX_TOKEN_CHARS).contains(&token.len()) {
            return Err(InvalidToken::Length);
        }
        if !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(InvalidToken::Charset);
        }
        Ok(Self(token))
    }

    /// Whether `candidate` is this token, in time independent of where the
    /// two first differ. (Length is not secret: every generated token has
    /// the same length.)
    #[must_use]
    pub fn matches(&self, candidate: &[u8]) -> bool {
        constant_time_eq(self.0.as_bytes(), candidate)
    }
}

impl TryFrom<String> for ApiToken {
    type Error = InvalidToken;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiToken(<redacted>)")
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// Accepts `Authorization: Bearer <api_token>`; refuses everything else.
#[derive(Debug, Clone)]
pub struct StaticToken {
    token: ApiToken,
}

impl StaticToken {
    /// An authenticator for `token`.
    #[must_use]
    pub fn new(token: ApiToken) -> Self {
        Self { token }
    }
}

impl StaticToken {
    /// Whether `request` carries `Authorization: Bearer <api_token>`.
    #[must_use]
    pub fn accepts(&self, request: &AuthRequest<'_>) -> bool {
        bearer(request).is_some_and(|candidate| self.token.matches(candidate))
    }
}

/// The credential in `Authorization: Bearer <credential>`, if any.
#[must_use]
pub fn bearer<'a>(request: &AuthRequest<'a>) -> Option<&'a [u8]> {
    request
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
}

impl Authenticator for StaticToken {
    fn authenticate<'a>(&'a self, request: AuthRequest<'a>) -> AuthFuture<'a> {
        let ok = self.accepts(&request);
        Box::pin(async move {
            if ok {
                Ok(Principal::new(STATIC_DEVICE))
            } else {
                Err(AuthRejection::Unauthenticated)
            }
        })
    }

    fn known_devices(&self) -> Option<BTreeSet<String>> {
        Some(BTreeSet::from([STATIC_DEVICE.to_owned()]))
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, Uri};

    use super::*;
    use crate::server::ClientAddr;

    const GOOD: &str = "0123456789abcdef0123456789abcdef";

    async fn verdict(auth: &StaticToken, value: Option<&str>) -> Result<Principal, AuthRejection> {
        let mut headers = HeaderMap::new();
        if let Some(value) = value {
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(value).expect("header"),
            );
        }
        let uri = Uri::from_static("/v1/agents");
        auth.authenticate(AuthRequest {
            headers: &headers,
            uri: &uri,
            client: ClientAddr("127.0.0.1".parse().expect("ip")),
        })
        .await
    }

    #[test]
    fn tokens_are_validated() {
        assert!(ApiToken::new(GOOD).is_ok());
        assert_eq!(ApiToken::new("short"), Err(InvalidToken::Length));
        assert_eq!(
            ApiToken::new("x".repeat(MAX_TOKEN_CHARS + 1)),
            Err(InvalidToken::Length)
        );
        let spaced = format!("{} x", &GOOD[..30]);
        assert_eq!(ApiToken::new(spaced), Err(InvalidToken::Charset));
        let unicode = format!("{}é", &GOOD[..31]);
        assert_eq!(ApiToken::new(unicode), Err(InvalidToken::Charset));
    }

    #[test]
    fn debug_is_redacted() {
        let token = ApiToken::new(GOOD).expect("valid");
        assert!(!format!("{token:?}").contains(GOOD));
    }

    #[test]
    fn matching_is_exact() {
        let token = ApiToken::new(GOOD).expect("valid");
        assert!(token.matches(GOOD.as_bytes()));
        assert!(!token.matches(&GOOD.as_bytes()[..31]));
        assert!(!token.matches(format!("{GOOD}0").as_bytes()));
        assert!(!token.matches(GOOD.replace('f', "e").as_bytes()));
        assert!(!token.matches(b""));
    }

    #[tokio::test]
    async fn static_token_checks_the_bearer_header() {
        let auth = StaticToken::new(ApiToken::new(GOOD).expect("valid"));
        let ok = verdict(&auth, Some(&format!("Bearer {GOOD}"))).await;
        let principal = ok.expect("accepted");
        assert_eq!(principal.device, STATIC_DEVICE);
        assert_eq!(principal.expires, None);
        for bad in [
            None,
            Some(GOOD),
            Some("Bearer wrong"),
            Some(&*format!("bearer {GOOD}")),
            Some(&*format!("Bearer {GOOD} ")),
        ] {
            assert_eq!(
                verdict(&auth, bad).await.err(),
                Some(AuthRejection::Unauthenticated),
                "{bad:?}"
            );
        }
    }
}
