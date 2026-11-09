//! One log line per request: method, path, status and latency.
//!
//! The query string is deliberately left out (a credential may ride in it on
//! a WebSocket upgrade), as are headers and bodies.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tokio::time::Instant;

/// Middleware: logs the request once it has been answered.
pub(crate) async fn requests(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    let status = response.status();
    let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    if status.is_server_error() {
        tracing::warn!(%method, path, status = status.as_u16(), micros, "request failed");
    } else {
        tracing::debug!(%method, path, status = status.as_u16(), micros, "request");
    }
    response
}
