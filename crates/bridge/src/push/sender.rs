//! Delivering an encrypted message to a push service.
//!
//! [`PushSender`] is the seam: [`HttpSender`] posts over HTTPS (rustls,
//! the system's trusted roots), tests use a fake that records requests.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use axum::body::Bytes;
use axum::http::{Request, StatusCode, header};
use http_body_util::Full;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use super::subscription::Endpoint;

/// How long a push service keeps an undelivered ping, in seconds. A ping
/// that arrives later than this is not worth waking a phone for.
pub const TTL_SECS: u32 = 60 * 60;

/// Push services replace an undelivered message with the same topic
/// (RFC 8030 §5.4), so a phone that was offline wakes once, not per ping.
pub const TOPIC: &str = "passporter";

/// One encrypted Web Push message (RFC 8030 §5).
#[derive(Clone, PartialEq, Eq)]
pub struct PushRequest {
    /// Where it goes.
    pub endpoint: Endpoint,
    /// The VAPID `Authorization` header value.
    pub authorization: String,
    /// The `aes128gcm` body.
    pub body: Vec<u8>,
}

impl PushRequest {
    /// Every header the request carries besides `Content-Length`.
    #[must_use]
    pub fn headers(&self) -> [(&'static str, String); 6] {
        [
            (header::AUTHORIZATION.as_str(), self.authorization.clone()),
            (header::CONTENT_ENCODING.as_str(), "aes128gcm".to_owned()),
            (
                header::CONTENT_TYPE.as_str(),
                "application/octet-stream".to_owned(),
            ),
            ("ttl", TTL_SECS.to_string()),
            ("urgency", "high".to_owned()),
            ("topic", TOPIC.to_owned()),
        ]
    }
}

impl fmt::Debug for PushRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushRequest")
            .field("endpoint", &self.endpoint)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// What the push service said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Accepted for delivery (2xx).
    Accepted,
    /// The subscription no longer exists (404 or 410): forget it.
    Gone,
    /// Refused for another reason (bad VAPID, too large, rate limited, ...).
    Rejected(StatusCode),
}

impl Delivery {
    /// The delivery outcome a response status means.
    #[must_use]
    pub fn from_status(status: StatusCode) -> Self {
        match status {
            s if s.is_success() => Self::Accepted,
            StatusCode::NOT_FOUND | StatusCode::GONE => Self::Gone,
            s => Self::Rejected(s),
        }
    }
}

/// The push service could not be reached or the request not built.
#[derive(Debug, thiserror::Error)]
#[error("push request failed: {0}")]
pub struct SendError(String);

/// The future a [`PushSender`] returns.
pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<Delivery, SendError>> + Send + 'a>>;

/// Delivers [`PushRequest`]s. Boxed-future style so the dispatcher can hold
/// any implementation as `Arc<dyn PushSender>`.
pub trait PushSender: Send + Sync + 'static {
    /// Sends one message. Implementations must not log the endpoint URL
    /// (it is a capability) or the body.
    fn send(&self, request: PushRequest) -> SendFuture<'_>;
}

/// Posts to push services over HTTPS/1.1.
pub struct HttpSender {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl fmt::Debug for HttpSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSender").finish_non_exhaustive()
    }
}

impl HttpSender {
    /// A client that trusts the system's root certificates and speaks only
    /// HTTPS.
    pub fn new() -> Result<Self, SendError> {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(rustls::crypto::ring::default_provider())
            .map_err(|e| SendError(format!("loading the system root certificates: {e}")))?
            .https_only()
            .enable_http1()
            .build();
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(https),
        })
    }
}

impl PushSender for HttpSender {
    fn send(&self, request: PushRequest) -> SendFuture<'_> {
        Box::pin(async move {
            let mut builder = Request::post(request.endpoint.as_str());
            for (name, value) in request.headers() {
                builder = builder.header(name, value);
            }
            let http = builder
                .body(Full::new(Bytes::from(request.body)))
                .map_err(|e| SendError(format!("building the request: {e}")))?;
            let response = self
                .client
                .request(http)
                .await
                // hyper's errors name the cause, not the URL.
                .map_err(|e| SendError(e.to_string()))?;
            Ok(Delivery::from_status(response.status()))
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Records every request and answers with a fixed [`Delivery`].
    #[derive(Debug)]
    pub(crate) struct FakeSender {
        pub(crate) sent: Mutex<Vec<PushRequest>>,
        pub(crate) answer: Mutex<Delivery>,
    }

    impl FakeSender {
        pub(crate) fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                answer: Mutex::new(Delivery::Accepted),
            }
        }

        pub(crate) fn sent(&self) -> Vec<PushRequest> {
            self.sent.lock().expect("lock").clone()
        }
    }

    impl PushSender for FakeSender {
        fn send(&self, request: PushRequest) -> SendFuture<'_> {
            self.sent.lock().expect("lock").push(request);
            let answer = *self.answer.lock().expect("lock");
            Box::pin(async move { Ok(answer) })
        }
    }

    #[test]
    fn statuses_map_to_deliveries() {
        assert_eq!(
            Delivery::from_status(StatusCode::CREATED),
            Delivery::Accepted
        );
        assert_eq!(Delivery::from_status(StatusCode::GONE), Delivery::Gone);
        assert_eq!(Delivery::from_status(StatusCode::NOT_FOUND), Delivery::Gone);
        assert_eq!(
            Delivery::from_status(StatusCode::TOO_MANY_REQUESTS),
            Delivery::Rejected(StatusCode::TOO_MANY_REQUESTS)
        );
    }

    #[test]
    fn the_real_sender_builds_without_touching_the_network() {
        assert!(HttpSender::new().is_ok());
    }
}
