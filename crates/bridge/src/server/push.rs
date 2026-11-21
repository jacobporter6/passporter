//! The Web Push endpoints (see [`crate::push`]), all per authenticated
//! device. With push off, each answers 404.
//!
//! - `GET /v1/push/key`: `{"vapidPublicKey": "<base64url>"}`, the PWA's
//!   `applicationServerKey`.
//! - `GET /v1/push/subscriptions`: `{"subscriptions": [{"endpoint": "…"}]}`,
//!   this device's subscriptions.
//! - `POST /v1/push/subscriptions` with `PushSubscription.toJSON()`: 201
//!   when new, 204 when it replaced the one at that endpoint, 400 when
//!   invalid or not on an allowed push service, 409 when the device is at
//!   its limit.
//! - `DELETE /v1/push/subscriptions` with `{"endpoint": "…"}`: 204, or 404
//!   if this device has no such subscription.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use protocol::{PushEndpoint, PushKey, PushSubscriptions};
use serde::de::DeserializeOwned;

use super::{AppState, Principal};
use crate::push::{Push, Put, StoreError, Subscription, Unsubscribe};

/// Longest parse error echoed back to the client.
const MAX_ERROR_CHARS: usize = 200;

/// A refused request: a status and, for client errors, what was wrong.
#[derive(Debug)]
pub(crate) struct Refusal(StatusCode, Option<String>);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        match self.1 {
            Some(message) => (self.0, message).into_response(),
            None => self.0.into_response(),
        }
    }
}

/// The push service, or 404 when push is off.
fn service(state: &AppState) -> Result<Arc<Push>, Refusal> {
    state
        .push
        .clone()
        .ok_or(Refusal(StatusCode::NOT_FOUND, None))
}

/// `body` as `T`, or a 400 naming what is wrong.
fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, Refusal> {
    serde_json::from_slice(body).map_err(|error| {
        let message = error.to_string().chars().take(MAX_ERROR_CHARS).collect();
        Refusal(StatusCode::BAD_REQUEST, Some(message))
    })
}

/// Runs a store change on the blocking pool (it rewrites a file).
async fn change<T: Send + 'static>(
    push: Arc<Push>,
    op: impl FnOnce(&Push) -> Result<T, StoreError> + Send + 'static,
) -> Result<T, Refusal> {
    match tokio::task::spawn_blocking(move || op(&push)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(StoreError::Full)) => Err(Refusal(
            StatusCode::CONFLICT,
            Some(StoreError::Full.to_string()),
        )),
        Ok(Err(error)) => {
            tracing::error!(%error, "push subscription store failed");
            Err(Refusal(StatusCode::INTERNAL_SERVER_ERROR, None))
        }
        Err(error) => {
            tracing::error!(%error, "push subscription store task failed");
            Err(Refusal(StatusCode::INTERNAL_SERVER_ERROR, None))
        }
    }
}

pub(crate) async fn key(State(state): State<AppState>) -> Result<Json<PushKey>, Refusal> {
    let push = service(&state)?;
    Ok(Json(PushKey {
        vapid_public_key: push.public_key(),
    }))
}

pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<PushSubscriptions>, Refusal> {
    let push = service(&state)?;
    let subscriptions = push
        .store()
        .of(&principal.device)
        .into_iter()
        .map(|s| PushEndpoint {
            endpoint: s.endpoint.as_str().to_owned(),
        })
        .collect();
    Ok(Json(PushSubscriptions { subscriptions }))
}

pub(crate) async fn subscribe(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    body: Bytes,
) -> Result<StatusCode, Refusal> {
    let push = service(&state)?;
    let subscription: Subscription = parse(&body)?;
    if crate::push::native::is_reserved(&subscription.endpoint) {
        return Err(Refusal(
            StatusCode::BAD_REQUEST,
            Some("reserved for the built-in transport (POST /v1/push/native)".to_owned()),
        ));
    }
    push.hosts()
        .check(&subscription.endpoint)
        .map_err(|error| Refusal(StatusCode::BAD_REQUEST, Some(error.to_string())))?;
    let host = subscription.endpoint.host();
    let revocation = principal.revocation.clone();
    let put = change(push, move |push| {
        push.store()
            .put_unless(&principal.device, subscription, || revocation.is_revoked())
    })
    .await?;
    // Revoked while this request was in flight: nothing was kept.
    let put = put.ok_or(Refusal(StatusCode::FORBIDDEN, None))?;
    tracing::info!(%host, ?put, "push subscription stored");
    Ok(match put {
        Put::Created => StatusCode::CREATED,
        Put::Replaced => StatusCode::NO_CONTENT,
    })
}

pub(crate) async fn unsubscribe(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    body: Bytes,
) -> Result<StatusCode, Refusal> {
    let push = service(&state)?;
    let Unsubscribe { endpoint } = parse(&body)?;
    let removed = change(push, move |push| {
        push.store().remove(&principal.device, &endpoint)
    })
    .await?;
    Ok(if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}
