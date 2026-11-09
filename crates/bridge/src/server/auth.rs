//! Authentication seam. Every `/v1` route, including the WebSocket upgrade,
//! goes through [`require_auth`] and therefore through an [`Authenticator`].
//!
//! The binary runs [`crate::auth::DeviceAuth`] (pairing, signed challenges,
//! short-lived tokens, revocation; design §8). An authenticator can also
//! serve unauthenticated routes of its own under `/v1/auth/`
//! ([`Authenticator::public_routes`]), and hands every [`Principal`] a
//! [`Revocation`] that long-lived connections watch.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::extract::{MatchedPath, OriginalUri, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use protocol::Scope;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

use super::client_addr::ClientAddr;

/// What an [`Authenticator`] gets to look at. Note that browsers cannot set
/// headers on a WebSocket upgrade, so credentials may arrive in the URI.
#[derive(Debug, Clone, Copy)]
pub struct AuthRequest<'a> {
    /// Request headers (e.g. `Authorization`).
    pub headers: &'a HeaderMap,
    /// Path and query, as the client sent them (before `/v1` routing).
    pub uri: &'a Uri,
    /// The resolved client address.
    pub client: ClientAddr,
}

/// Who made an authenticated request. Handlers read it from the request
/// extensions (e.g. to attribute an action in the audit log).
#[derive(Debug, Clone)]
pub struct Principal {
    /// The paired device's id. Never a credential.
    pub device: String,
    /// When the credential expires; open streams close then. `None` for a
    /// credential that does not expire (the static token).
    pub expires: Option<Instant>,
    /// Fires when the device is revoked; open streams close on it.
    pub revocation: Revocation,
    /// What the credential may do: [`Scope::Car`] reaches only
    /// [`CAR_ROUTES`] ([`require_auth`] refuses the rest), and its
    /// actions are judged as `origin: "car"` ([`Scope::origin`]).
    pub scope: Scope,
}

impl Principal {
    /// An app-scoped principal for `device` that never expires and is
    /// never revoked (the static token, test stubs).
    #[must_use]
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            expires: None,
            revocation: Revocation::never(),
            scope: Scope::App,
        }
    }
}

/// Whether, and when, a [`Principal`]'s device is revoked.
#[derive(Debug, Clone)]
pub struct Revocation(Option<watch::Receiver<bool>>);

impl Revocation {
    /// A credential that is never revoked.
    #[must_use]
    pub fn never() -> Self {
        Self(None)
    }

    /// Revoked once `revoked` holds `true`, or once its sender is gone
    /// (the device was removed).
    #[must_use]
    pub fn watching(revoked: watch::Receiver<bool>) -> Self {
        Self(Some(revoked))
    }

    /// Whether the device is revoked now.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|rx| *rx.borrow() || rx.has_changed().is_err())
    }

    /// Resolves when the device is revoked; never for [`Self::never`].
    pub async fn revoked(&mut self) {
        match &mut self.0 {
            // Err means the sender is gone, which also means revoked.
            Some(rx) => {
                let _ = rx.wait_for(|revoked| *revoked).await;
            }
            None => std::future::pending().await,
        }
    }
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthRejection {
    /// No credentials, or invalid or expired ones: HTTP 401.
    #[error("unauthenticated")]
    Unauthenticated,
    /// Valid credentials that may not do this (e.g. a revoked device): HTTP 403.
    #[error("forbidden")]
    Forbidden,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
            )
                .into_response(),
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
        }
    }
}

/// The future an [`Authenticator`] returns.
pub type AuthFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Principal, AuthRejection>> + Send + 'a>>;

/// Decides whether a `/v1` request may proceed. Boxed-future style so the
/// server can hold any implementation as `Arc<dyn Authenticator>`.
pub trait Authenticator: Send + Sync + 'static {
    /// Authenticates one request. Implementations must not log credentials.
    fn authenticate<'a>(&'a self, request: AuthRequest<'a>) -> AuthFuture<'a>;

    /// Unauthenticated routes served under `/v1/auth/` (e.g. pairing). They
    /// must do their own rate limiting. The default serves none.
    fn public_routes(self: Arc<Self>) -> Router {
        Router::new()
    }

    /// The ids of devices as they are revoked, so state kept per device
    /// (push subscriptions) can be dropped. The default never revokes.
    fn revocations(&self) -> Option<broadcast::Receiver<String>> {
        None
    }

    /// Every device id a request may authenticate as right now, so state
    /// kept for devices that are gone (revoked while the bridge was down)
    /// can be dropped at startup. `None`, the default, when the set is not
    /// known: nothing is dropped.
    fn known_devices(&self) -> Option<BTreeSet<String>> {
        None
    }
}

/// Middleware: runs the [`Authenticator`] and attaches the [`Principal`], or
/// answers with the rejection. A car-scoped principal off [`CAR_ROUTES`]
/// gets a bare 403. Needs [`ClientAddr`] attached first.
pub(crate) async fn require_auth(
    State(auth): State<Arc<dyn Authenticator>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let Some(client) = parts.extensions.get::<ClientAddr>().copied() else {
        tracing::error!("client address missing; refusing request");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    // Nesting strips `/v1`; authenticate and log the URI the client sent.
    let uri = parts
        .extensions
        .get::<OriginalUri>()
        .map_or(&parts.uri, |original| &original.0);
    let verdict = auth
        .authenticate(AuthRequest {
            headers: &parts.headers,
            uri,
            client,
        })
        .await;
    match verdict {
        Ok(principal) => {
            let matched = parts
                .extensions
                .get::<MatchedPath>()
                .map(MatchedPath::as_str);
            if !scope_allows(&principal, &parts.method, matched) {
                tracing::debug!(%client, path = uri.path(), "car session refused off the car routes");
                return StatusCode::FORBIDDEN.into_response();
            }
            let mut request = Request::from_parts(parts, body);
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(rejection) => {
            // Debug only: anyone who reaches the bridge could otherwise flood the log.
            tracing::debug!(%client, path = uri.path(), %rejection, "request refused");
            rejection.into_response()
        }
    }
}

/// The only routes a [`Scope::Car`] session may use, as method and
/// route template (as matched under `/v1`). Everything else is refused
/// by [`require_auth`]: deny by default, so a new route is the app's
/// until it is added here.
pub(crate) const CAR_ROUTES: [(Method, &str); 5] = [
    (Method::GET, "/agents"),
    (Method::GET, "/agents/{id}/pending"),
    (Method::GET, "/agents/{id}/tldr"),
    (Method::POST, "/agents/{id}/car-reply"),
    (Method::POST, "/actions"),
];

/// Whether `principal` may use the route `matched` (its template, with
/// or without the `/v1` prefix) with `method`. App sessions may use every
/// route; car sessions only [`CAR_ROUTES`], and nothing that matched no
/// route.
fn scope_allows(principal: &Principal, method: &Method, matched: Option<&str>) -> bool {
    match principal.scope {
        Scope::App => true,
        Scope::Car => matched.is_some_and(|path| {
            let path = path.strip_prefix("/v1").unwrap_or(path);
            CAR_ROUTES
                .iter()
                .any(|(allowed, route)| allowed == method && *route == path)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn revocation_fires_on_true_or_a_dropped_sender() {
        assert!(!Revocation::never().is_revoked());
        let (tx, rx) = watch::channel(false);
        let mut revocation = Revocation::watching(rx);
        assert!(!revocation.is_revoked());
        tx.send_replace(true);
        assert!(revocation.is_revoked());
        revocation.revoked().await;

        let (tx, rx) = watch::channel(false);
        let mut dropped = Revocation::watching(rx);
        drop(tx);
        assert!(dropped.is_revoked());
        dropped.revoked().await;
    }

    #[test]
    fn car_sessions_reach_only_the_listed_routes() {
        let car = Principal {
            scope: Scope::Car,
            ..Principal::new("dev-0000000000000001")
        };
        let app = Principal::new("dev-0000000000000001");
        for (method, path) in CAR_ROUTES {
            assert!(scope_allows(&car, &method, Some(path)), "{method} {path}");
            let nested = format!("/v1{path}");
            assert!(
                scope_allows(&car, &method, Some(&nested)),
                "{method} {nested}"
            );
        }
        for (method, path) in [
            (Method::POST, "/agents/{id}/pending"),
            (Method::DELETE, "/actions"),
            (Method::POST, "/agents/{id}/commands/run"),
            (Method::POST, "/terminals/{pane}/keys"),
            (Method::GET, "/agents/{id}/thread"),
            (Method::POST, "/agents"),
        ] {
            assert!(!scope_allows(&car, &method, Some(path)), "{method} {path}");
            assert!(scope_allows(&app, &method, Some(path)), "{method} {path}");
        }
        assert!(!scope_allows(&car, &Method::GET, None), "no route: refused");
        assert!(scope_allows(&app, &Method::GET, None));
    }

    #[test]
    fn rejections_map_to_status_codes() {
        let response = AuthRejection::Unauthenticated.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(
            AuthRejection::Forbidden.into_response().status(),
            StatusCode::FORBIDDEN
        );
    }
}
