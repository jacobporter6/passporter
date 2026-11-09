//! `GET /v1/version`: the bridge's build (see [`crate::version`]).
//!
//! Its own endpoint, outside auth, rather than a field on an authenticated
//! read: the PWA must learn it is stale even when it can't sign in (not
//! paired yet, a rejected token, a pairing flow the old build gets wrong),
//! and an operator can `curl` it after a deploy. It discloses only the
//! commit and build time, which the served app carries anyway.

use axum::Json;

use crate::version::BUILD;

pub(crate) async fn version() -> Json<protocol::BridgeVersion> {
    Json(BUILD.wire())
}
