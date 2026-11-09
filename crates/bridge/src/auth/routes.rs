//! `/v1/auth/*`: the unauthenticated pairing and session endpoints.
//!
//! Every one is rate limited per client and answers a refusal with a bare
//! status: 400 for a step-up request, 401 for anything wrong with the
//! credentials (never saying which), 410 for a device the bridge does not
//! know ([`protocol::auth::DEVICE_UNKNOWN_STATUS`]), 409 when the device
//! limit is reached, 429 with `Retry-After`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use protocol::{ChallengeRequest, PairRequest, SessionRequest};

use super::{DeviceAuth, Refusal};
use crate::server::{AuthRejection, ClientAddr};

/// The routes, relative to `/v1/auth`.
pub(super) fn router(auth: Arc<DeviceAuth>) -> Router {
    Router::new()
        .route("/pair", post(pair))
        .route("/challenge", post(challenge))
        .route("/session", post(session))
        .with_state(auth)
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthenticated => AuthRejection::Unauthenticated.into_response(),
            Self::UnknownDevice => StatusCode::GONE.into_response(),
            Self::BadRequest => StatusCode::BAD_REQUEST.into_response(),
            Self::RateLimited(wait) => {
                // Round up: "0" would invite an immediate retry.
                let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, HeaderValue::from(secs.max(1)))],
                )
                    .into_response()
            }
            Self::TooManyDevices => StatusCode::CONFLICT.into_response(),
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

fn answer<T: serde::Serialize>(result: Result<T, Refusal>) -> Response {
    result.map(Json).into_response()
}

async fn pair(
    State(auth): State<Arc<DeviceAuth>>,
    Extension(client): Extension<ClientAddr>,
    Json(request): Json<PairRequest>,
) -> Response {
    answer(auth.pair(client.0, request).await)
}

async fn challenge(
    State(auth): State<Arc<DeviceAuth>>,
    Extension(client): Extension<ClientAddr>,
    Json(request): Json<ChallengeRequest>,
) -> Response {
    answer(auth.challenge(client.0, &request))
}

async fn session(
    State(auth): State<Arc<DeviceAuth>>,
    Extension(client): Extension<ClientAddr>,
    Json(request): Json<SessionRequest>,
) -> Response {
    answer(auth.session(client.0, &request))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_map_to_the_documented_statuses() {
        let status = |refusal: Refusal| refusal.into_response().status();
        assert_eq!(status(Refusal::Unauthenticated), StatusCode::UNAUTHORIZED);
        assert_eq!(
            status(Refusal::UnknownDevice).as_u16(),
            protocol::DEVICE_UNKNOWN_STATUS
        );
        assert_eq!(status(Refusal::BadRequest), StatusCode::BAD_REQUEST);
        assert_eq!(status(Refusal::TooManyDevices), StatusCode::CONFLICT);
    }
}
