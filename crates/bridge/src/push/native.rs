//! The built-in Android transport (design §6.6,
//! `docs/architecture/native-notifications.md`): a private ntfy companion
//! behind the bridge holds each phone's encrypted notices until the phone
//! collects them, so no push service or second app is needed.
//!
//! - `POST /v1/push/native` ([`register`], app scope) stores a notice
//!   subscription at a reserved endpoint,
//!   `https://native.passporter.invalid/<registration>`, that no DNS
//!   resolves and the ordinary subscribe route refuses. Its answer names
//!   the key the notices are signed with.
//! - The dispatcher hands that subscription's messages to the [`Companion`]:
//!   encrypted to the phone (RFC 8291, as every notice), wrapped in an
//!   [`Envelope`] signed with the bridge's VAPID key, spooled durably
//!   ([`super::native_spool`]) and published to the companion's topic
//!   `pp-<registration>` by [`forward`].
//! - `GET /v1/push/native/<registration>/stream` ([`stream`], app scope)
//!   proxies the companion's JSON stream of that topic, only to the device
//!   that registered it, and ends on session expiry, revocation, a dropped
//!   registration or shutdown. Device tokens never reach the companion.
//!
//! The companion listens on loopback only (`push.native_ntfy`, checked in
//! `config.rs`) and sees only ciphertext, opaque topics and timing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use protocol::{IdempotencyKey, NativeRegister, NativeRegistration, PushMessage, PushPayload};
use serde::Serialize;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};

use super::native_spool::{Pending, Spool};
use super::subscription::BASE64URL;
use super::{Endpoint, Push, StoreError, Subscription, VapidKey, ece};
use crate::server::{AppState, Principal, Revocation, query};
use crate::shutdown::Shutdown;

/// Host of the reserved endpoints. `.invalid` never resolves (RFC 6761).
pub const RESERVED_HOST: &str = "native.passporter.invalid";

/// The reserved endpoint's prefix; the registration id follows.
const PREFIX: &str = "https://native.passporter.invalid/";

/// Version of the [`Envelope`] and of the registration answer.
const VERSION: u8 = 1;

/// Largest notice plaintext: its ciphertext, base64url, in the envelope,
/// fits [`MAX_ENVELOPE`].
const MAX_PLAINTEXT: usize = 2400;

/// Largest envelope (the companion's message text), in bytes.
pub(super) const MAX_ENVELOPE: usize = 4096;

/// How long an envelope is valid, and stays spooled: a Web Push
/// message's TTL.
const LIFETIME_SECS: u64 = 3600;

/// How long a request to the companion may take to answer.
const COMPANION_TIMEOUT: Duration = Duration::from_secs(10);

/// Most streams open at once, across all devices.
const MAX_STREAMS: usize = 32;

/// How long a stream waits for a free slot (the previous stream of its
/// registration gives its back as it ends).
const PERMIT_WAIT: Duration = Duration::from_secs(2);

/// How often an open stream checks that its registration still exists.
const RECHECK: Duration = Duration::from_secs(15);

/// Longest wait between two retries of the spool.
const MAX_BACKOFF: Duration = Duration::from_mins(1);

/// Domain separation of the envelope signature (a VAPID JWT's signed
/// input starts with base64url, never with this).
const SIGNATURE_DOMAIN: &str = "passporter-notice-v1";

/// Why a notice did not reach the spool or the companion. Never carries
/// content, keys or endpoints.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeError {
    /// Even with every notice dropped the message does not fit.
    #[error("notice message too large for the built-in transport")]
    TooLarge,
    /// RFC 8291 encryption failed (a bad subscription key).
    #[error("encrypting the notice failed")]
    Encrypt,
    /// The VAPID key could not sign.
    #[error("signing the notice failed")]
    Sign,
    /// The spool could not be written.
    #[error("spooling the notice failed: {0}")]
    Spool(#[source] std::io::Error),
    /// The companion did not take it, for now: retried.
    #[error("the notification companion {0}")]
    Companion(&'static str),
    /// The companion refused it for good (a 4xx other than 408 and 429):
    /// dropped, so it never holds back the envelopes behind it.
    #[error("the notification companion refused the notice ({0})")]
    Rejected(StatusCode),
}

/// The private ntfy companion, configured by the operator, never named by
/// a client.
#[derive(Debug)]
pub struct Companion {
    address: SocketAddr,
    client: Client<HttpConnector, Full<Bytes>>,
    streams: Arc<Semaphore>,
    /// The open stream of each registration: a new one ends the old one
    /// (dropping its sender), so a phone that reconnects from a new network
    /// never holds two of [`MAX_STREAMS`] while the old connection lingers.
    live: Mutex<HashMap<IdempotencyKey, (u64, watch::Sender<()>)>>,
    /// Numbers the streams, to tell whose entry [`Self::live`] holds.
    opened: AtomicU64,
    spool: Arc<Spool>,
    /// Wakes [`forward`] when a notice is spooled.
    spooled: Notify,
}

/// What the companion carries for one notice. The signature binds the
/// registration, the expiry and the ciphertext to the bridge's key.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    version: u8,
    registration_id: IdempotencyKey,
    /// Unix seconds.
    expires_at: u64,
    /// The RFC 8291 ciphertext, base64url.
    body: String,
    /// ECDSA P-256 over [`signing_input`], fixed-width `r || s`, base64url.
    signature: String,
}

/// The registration a reserved endpoint names, or `None` for any other
/// endpoint (an external push service's).
#[must_use]
pub fn registration(endpoint: &Endpoint) -> Option<IdempotencyKey> {
    IdempotencyKey::new(endpoint.as_str().strip_prefix(PREFIX)?).ok()
}

/// Whether `endpoint` is on [`RESERVED_HOST`], well formed or not.
#[must_use]
pub fn is_reserved(endpoint: &Endpoint) -> bool {
    endpoint.host() == RESERVED_HOST
}

/// The reserved endpoint of `registration`.
fn endpoint_of(registration: &IdempotencyKey) -> Result<Endpoint, StatusCode> {
    format!("{PREFIX}{registration}")
        .parse()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

impl Companion {
    /// The companion at `address`, with its spool at `spool`.
    ///
    /// # Errors
    /// The spool exists but cannot be read privately, or is invalid.
    pub(crate) fn new(address: SocketAddr, spool: &std::path::Path) -> Result<Self, StoreError> {
        Ok(Self {
            address,
            client: Client::builder(TokioExecutor::new()).build_http(),
            streams: Arc::new(Semaphore::new(MAX_STREAMS)),
            live: Mutex::new(HashMap::new()),
            opened: AtomicU64::new(0),
            spool: Arc::new(Spool::open(spool)?),
            spooled: Notify::new(),
        })
    }

    /// Seals `message` for `subscription` and spools it; [`forward`]
    /// publishes it. Spooled first, so a companion that is down (or a
    /// bridge restart) delays the notice instead of losing it.
    pub(super) async fn publish(
        &self,
        registration: &IdempotencyKey,
        subscription: &Subscription,
        message: &PushMessage,
        key: &VapidKey,
    ) -> Result<(), NativeError> {
        let now = now();
        let envelope = seal(registration, subscription, message, key, now)?;
        let pending = Pending {
            registration: registration.clone(),
            expires: now + LIFETIME_SECS,
            envelope,
            seq: 0,
        };
        // One ping per message and device: unique per notice message.
        let id = format!("{registration}:{}", message.ping);
        let spool = Arc::clone(&self.spool);
        tokio::task::spawn_blocking(move || spool.put(&id, &pending))
            .await
            .map_err(|e| NativeError::Spool(std::io::Error::other(e)))?
            .map_err(NativeError::Spool)?;
        self.spooled.notify_one();
        Ok(())
    }

    /// Publishes one envelope to the registration's topic with ntfy's JSON
    /// API (no `UnifiedPush` translation, no upstream).
    async fn send(&self, registration: &IdempotencyKey, envelope: &str) -> Result<(), NativeError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "topic": topic(registration),
            "message": envelope,
        }))
        .map_err(|_| NativeError::Companion("could not be asked"))?;
        let request = Request::post(format!("http://{}/", self.address))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| NativeError::Companion("could not be asked"))?;
        let response = tokio::time::timeout(COMPANION_TIMEOUT, self.client.request(request))
            .await
            .map_err(|_| NativeError::Companion("did not answer in time"))?
            .map_err(|_| NativeError::Companion("is unreachable"))?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else if status.is_client_error()
            && status != StatusCode::REQUEST_TIMEOUT
            && status != StatusCode::TOO_MANY_REQUESTS
        {
            Err(NativeError::Rejected(status))
        } else {
            Err(NativeError::Companion("refused the notice for now"))
        }
    }

    /// Makes `registration`'s new stream the live one, ending the one
    /// before; its number and what tells it that a newer one took over.
    fn take_over(&self, registration: &IdempotencyKey) -> (u64, watch::Receiver<()>) {
        let number = self.opened.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = watch::channel(());
        let previous = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(registration.clone(), (number, sender));
        // Dropping its sender ends the previous stream.
        drop(previous);
        (number, receiver)
    }

    /// Forgets stream `number` of `registration`, unless a newer one took over.
    fn release(&self, registration: &IdempotencyKey, number: u64) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if live.get(registration).is_some_and(|(n, _)| *n == number) {
            live.remove(registration);
        }
    }
}

/// The companion's topic of `registration`.
fn topic(registration: &IdempotencyKey) -> String {
    format!("pp-{registration}")
}

/// Publishes the spool to the companion, in the order it was spooled,
/// until `shutdown`: at once when a notice is spooled (and at start, for
/// what a restart left), and again with a doubling backoff (up to a
/// minute) while the companion fails. An envelope whose registration is
/// gone, that expired, or that the companion refuses for good, is dropped.
pub(super) async fn forward(push: Arc<Push>, mut shutdown: Shutdown) {
    let Some(companion) = &push.native else {
        return;
    };
    let mut backoff: Option<Duration> = Some(Duration::ZERO);
    loop {
        match backoff {
            None => tokio::select! {
                () = shutdown.wait() => return,
                () = companion.spooled.notified() => {}
            },
            Some(delay) => tokio::select! {
                () = shutdown.wait() => return,
                () = tokio::time::sleep(delay) => {}
            },
        }
        let mut failed = false;
        let mut stopping = false;
        let mut pending: Vec<_> = companion.spool.snapshot().into_iter().collect();
        pending.sort_by_key(|(_, item)| item.seq);
        // Removed in one write per pass, not one per envelope.
        let mut done = Vec::new();
        for (id, item) in pending {
            if item.expires > now() && registered(&push, &item.registration) {
                let sent = tokio::select! {
                    () = shutdown.wait() => None,
                    sent = companion.send(&item.registration, &item.envelope) => Some(sent),
                };
                // Stopping: what this pass published is still removed below, so a
                // restart does not send it again (the one cut short may be).
                let Some(sent) = sent else {
                    stopping = true;
                    break;
                };
                match sent {
                    Ok(()) => {}
                    Err(error @ NativeError::Rejected(_)) => {
                        tracing::warn!(%error, "built-in notice dropped");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "built-in notice not published; retrying");
                        failed = true;
                        break;
                    }
                }
            }
            done.push(id);
        }
        if !done.is_empty() {
            let spool = Arc::clone(&companion.spool);
            if !matches!(
                tokio::task::spawn_blocking(move || spool.remove_all(&done)).await,
                Ok(Ok(()))
            ) {
                tracing::error!("cannot update the built-in notice spool");
                failed = true;
            }
        }
        if stopping {
            return;
        }
        backoff = failed.then(|| {
            backoff
                .filter(|d| !d.is_zero())
                .map_or(Duration::from_secs(1), |d| (d * 2).min(MAX_BACKOFF))
        });
    }
}

/// Whether any device holds `registration`.
fn registered(push: &Push, registration: &IdempotencyKey) -> bool {
    push.store
        .devices()
        .iter()
        .any(|device| owns(push, device, registration))
}

/// Whether `device` holds `registration`.
fn owns(push: &Push, device: &str, registration: &IdempotencyKey) -> bool {
    push.store
        .of(device)
        .iter()
        .any(|s| self::registration(&s.endpoint).as_ref() == Some(registration))
}

/// `message` encrypted to `subscription` and signed, at `now`: the
/// envelope's JSON. Notices are dropped (finished ones first, then the
/// oldest) until the plaintext fits.
fn seal(
    registration: &IdempotencyKey,
    subscription: &Subscription,
    message: &PushMessage,
    key: &VapidKey,
    now: u64,
) -> Result<String, NativeError> {
    let mut message = message.clone();
    let plaintext = loop {
        let bytes = serde_json::to_vec(&message).map_err(|_| NativeError::TooLarge)?;
        if bytes.len() <= MAX_PLAINTEXT {
            break bytes;
        }
        if message.notices.is_empty() {
            return Err(NativeError::TooLarge);
        }
        let drop = message
            .notices
            .iter()
            .position(|notice| notice.state == protocol::NoticeState::Finished)
            .unwrap_or(0);
        message.notices.remove(drop);
    };
    let ciphertext = ece::encrypt(
        &subscription.p256dh,
        &subscription.auth,
        &plaintext,
        &ring::rand::SystemRandom::new(),
    )
    .map_err(|_| NativeError::Encrypt)?;
    let body = BASE64URL.encode(ciphertext);
    let expires_at = now + LIFETIME_SECS;
    let signature = key
        .sign_notice(signing_input(registration, expires_at, &body).as_bytes())
        .map_err(|_| NativeError::Sign)?;
    let envelope = serde_json::to_string(&Envelope {
        version: VERSION,
        registration_id: registration.clone(),
        expires_at,
        body,
        signature,
    })
    .map_err(|_| NativeError::TooLarge)?;
    if envelope.len() > MAX_ENVELOPE {
        return Err(NativeError::TooLarge);
    }
    Ok(envelope)
}

/// Unix seconds now (0 before 1970: everything then reads as expired).
pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What the envelope's signature covers. The Android receiver
/// (`NativeEnvelope.kt`) builds the same text.
fn signing_input(registration: &IdempotencyKey, expires_at: u64, body: &str) -> String {
    format!("{SIGNATURE_DOMAIN}\n{registration}\n{expires_at}\n{body}")
}

/// Stores the caller's built-in notice subscription at the registration's
/// reserved endpoint (new keys replace the old) and names the key that
/// signs its notices. `POST /v1/push/native`, [`NativeRegister`] →
/// [`NativeRegistration`]. 404 without a companion, 400 for a bad body,
/// 409 when the registration is another device's or the device is at its
/// limit, 403 when revoked meanwhile.
pub(crate) async fn register(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    body: Bytes,
) -> Result<Json<NativeRegistration>, StatusCode> {
    let push = state.push.ok_or(StatusCode::NOT_FOUND)?;
    if push.native.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let request: NativeRegister =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let subscription = Subscription {
        endpoint: endpoint_of(&request.registration_id)?,
        p256dh: request
            .p256dh
            .parse()
            .map_err(|_| StatusCode::BAD_REQUEST)?,
        auth: request.auth.parse().map_err(|_| StatusCode::BAD_REQUEST)?,
        payload: PushPayload::Notice,
    };
    let signing_key = push.public_key();
    let revocation = principal.revocation;
    let device = principal.device;
    let stored = tokio::task::spawn_blocking(move || {
        push.store()
            .put_unless(&device, subscription, || revocation.is_revoked())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match stored {
        Ok(Some(_)) => {}
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(StoreError::Full | StoreError::Owned) => return Err(StatusCode::CONFLICT),
        Err(error) => {
            tracing::error!(%error, "push subscription store failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    tracing::info!("built-in notice subscription stored");
    Ok(Json(NativeRegistration {
        version: VERSION,
        signing_key,
        stream_path: format!("/v1/push/native/{}/stream", request.registration_id),
    }))
}

/// The `since` cursor of a stream request: `all`, or the id of the last
/// message the phone has (ntfy's ids are short base62 strings). 400 for
/// anything else, which never reaches the companion.
fn since(query: Option<&str>) -> Result<&str, StatusCode> {
    let mut since = "all";
    for pair in query::pairs(query) {
        let (key, value) = pair?;
        let valid = key == "since"
            && (1..=64).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid {
            return Err(StatusCode::BAD_REQUEST);
        }
        since = value;
    }
    Ok(since)
}

/// Streams the companion's newline-delimited JSON events of the caller's
/// own registration (404 for any other), after `?since=<id>` if given.
/// `GET /v1/push/native/<registration>/stream`. Ends when the
/// session expires (the phone signs in again), the device is revoked, the
/// registration is dropped or the bridge shuts down. 429 past
/// [`MAX_STREAMS`], 502/504 when the companion fails.
pub(crate) async fn stream(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(registration): Path<IdempotencyKey>,
    RawQuery(raw): RawQuery,
) -> Result<Response, StatusCode> {
    let push = state.push.clone().ok_or(StatusCode::NOT_FOUND)?;
    let companion = push.native.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    if !owns(&push, &principal.device, &registration) {
        return Err(StatusCode::NOT_FOUND);
    }
    let since = since(raw.as_deref())?;
    // A free slot first, if there is one: a request refused for capacity
    // then never costs the companion a subscription.
    let free = Arc::clone(&companion.streams).try_acquire_owned().ok();
    let request = Request::get(format!(
        "http://{}/{}/json?since={since}",
        companion.address,
        topic(&registration)
    ))
    .body(Full::new(Bytes::new()))
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let response = tokio::time::timeout(COMPANION_TIMEOUT, companion.client.request(request))
        .await
        .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !response.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    // Only now, with the companion answering, does this stream replace the
    // phone's previous one (a connection it has left). Without a free slot,
    // that one's permit comes back as it ends: hence the short wait.
    let (number, superseded) = companion.take_over(&registration);
    let permit = if let Some(permit) = free {
        permit
    } else {
        let waited =
            tokio::time::timeout(PERMIT_WAIT, Arc::clone(&companion.streams).acquire_owned()).await;
        let Ok(Ok(permit)) = waited else {
            companion.release(&registration, number);
            return Err(StatusCode::TOO_MANY_REQUESTS);
        };
        permit
    };
    // The static token never expires: such a stream still ends every
    // quarter hour.
    let expires = principal
        .expires
        .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_mins(15));
    let open = Open {
        body: response.into_body(),
        _permit: permit,
        push: Arc::clone(&push),
        registration,
        number,
        superseded,
        device: principal.device,
        revocation: principal.revocation,
        shutdown: state.shutdown.clone(),
        expires,
    };
    let events = futures_util::stream::unfold(open, |mut open| async move {
        let bytes = open.next().await?;
        Some((Ok::<_, std::io::Error>(bytes), open))
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (header::CACHE_CONTROL, "no-store"),
            // Asks nginx-style proxies not to buffer the stream.
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(events),
    )
        .into_response())
}

/// One open stream, and what ends it.
struct Open {
    body: hyper::body::Incoming,
    /// Held for the stream's life ([`MAX_STREAMS`]).
    _permit: OwnedSemaphorePermit,
    push: Arc<Push>,
    registration: IdempotencyKey,
    /// Its number ([`Companion::take_over`]).
    number: u64,
    /// Changes (or closes) once a newer stream of the registration opens.
    superseded: watch::Receiver<()>,
    device: String,
    revocation: Revocation,
    shutdown: Shutdown,
    expires: tokio::time::Instant,
}

impl Drop for Open {
    fn drop(&mut self) {
        if let Some(companion) = &self.push.native {
            companion.release(&self.registration, self.number);
        }
    }
}

impl Open {
    /// The companion's next bytes, or `None` once the stream must end.
    async fn next(&mut self) -> Option<Bytes> {
        loop {
            if !owns(&self.push, &self.device, &self.registration) {
                return None;
            }
            tokio::select! {
                () = self.revocation.revoked() => return None,
                // A newer stream of the registration: this one is left.
                _ = self.superseded.changed() => return None,
                () = self.shutdown.wait() => return None,
                () = tokio::time::sleep_until(self.expires) => return None,
                () = tokio::time::sleep(RECHECK) => {}
                frame = self.body.frame() => {
                    // A trailers frame carries no data: read on.
                    if let Ok(bytes) = frame?.ok()?.into_data() {
                        return Some(bytes);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    use super::*;

    const REGISTRATION: &str = "00000000-0000-4000-8000-000000000001";

    fn registration_id() -> IdempotencyKey {
        IdempotencyKey::new(REGISTRATION).expect("uuid")
    }

    fn envelope_json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("envelope json")
    }

    /// A subscription whose messages `receiver` can decrypt.
    fn subscription_for(receiver: &ece::tests::Receiver) -> Subscription {
        let mut subscription = super::super::store::tests::subscription("example");
        subscription.p256dh = receiver.p256dh().parse().expect("p256dh");
        subscription.auth = receiver.auth().parse().expect("auth");
        subscription
    }

    /// A companion that takes the first publish (and says so) and never
    /// answers the next.
    async fn stalling_companion() -> (SocketAddr, Arc<Notify>) {
        let published = Arc::new(Notify::new());
        let calls = Arc::new(AtomicU64::new(0));
        let told = Arc::clone(&published);
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                let told = Arc::clone(&told);
                async move {
                    if !first {
                        std::future::pending::<()>().await;
                    }
                    told.notify_one();
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move { axum::serve(listener, app).await });
        (address, published)
    }

    #[tokio::test]
    async fn a_shutdown_mid_pass_still_unspools_what_was_published() {
        let dir = tempfile::tempdir().expect("tmp");
        let (address, published) = stalling_companion().await;
        let key = Arc::new(super::super::vapid::tests::key());
        let mut settings = super::super::PushSettings::new(
            Arc::clone(&key),
            "mailto:ops@example.com".parse().expect("subject"),
            dir.path().join(super::super::store::STORE_FILE),
            Arc::new(super::super::sender::tests::FakeSender::new()),
        );
        settings.native_ntfy = Some(address);
        let push = Arc::new(Push::open(settings).expect("opens"));
        let receiver = ece::tests::Receiver::new();
        let mut subscription = subscription_for(&receiver);
        subscription.endpoint = endpoint_of(&registration_id()).expect("endpoint");
        push.store
            .put("dev-0000000000000001", subscription.clone())
            .expect("registered");
        let companion = push.native.as_ref().expect("companion");
        for ping in ["first", "second"] {
            let message = PushMessage {
                ping: ping.into(),
                notices: vec![],
            };
            companion
                .publish(&registration_id(), &subscription, &message, &key)
                .await
                .expect("spooled");
        }
        let (trigger, shutdown) = crate::shutdown::channel();
        let forwarding = tokio::spawn(forward(Arc::clone(&push), shutdown));
        tokio::time::timeout(Duration::from_secs(10), published.notified())
            .await
            .expect("the first is published");
        // The second's publish hangs: the shutdown cuts it short.
        trigger.trigger();
        tokio::time::timeout(Duration::from_secs(10), forwarding)
            .await
            .expect("forward ends")
            .expect("forward did not panic");
        let left = vec![format!("{REGISTRATION}:second")];
        let in_memory: Vec<_> = companion.spool.snapshot().into_keys().collect();
        assert_eq!(in_memory, left);
        let path = dir.path().join(super::super::native_spool::SPOOL_FILE);
        let on_disk: Vec<_> = Spool::open(&path)
            .expect("spool")
            .snapshot()
            .into_keys()
            .collect();
        assert_eq!(on_disk, left, "a restart sends only the second again");
    }

    #[test]
    fn only_well_formed_reserved_endpoints_name_a_registration() {
        let ok: Endpoint = format!("{PREFIX}{REGISTRATION}").parse().expect("endpoint");
        assert_eq!(registration(&ok), Some(registration_id()));
        assert!(is_reserved(&ok));
        for other in [
            "https://native.passporter.invalid/../secret",
            "https://native.passporter.invalid/not-a-uuid",
        ] {
            let endpoint: Endpoint = other.parse().expect("endpoint");
            assert_eq!(registration(&endpoint), None, "{other}");
            assert!(is_reserved(&endpoint), "{other}");
        }
        let external: Endpoint = "https://push.example.com/x".parse().expect("endpoint");
        assert_eq!(registration(&external), None);
        assert!(!is_reserved(&external));
    }

    #[test]
    fn the_envelope_is_encrypted_bound_to_its_registration_and_signed() {
        let key = super::super::vapid::tests::key();
        let receiver = ece::tests::Receiver::new();
        let subscription = subscription_for(&receiver);
        let message = PushMessage {
            ping: "unique-ping".into(),
            notices: vec![],
        };
        let text = seal(&registration_id(), &subscription, &message, &key, 1_000).expect("seal");
        assert!(!text.contains("unique-ping"), "ciphertext only");
        let envelope = envelope_json(&text);
        assert_eq!(envelope["version"], 1);
        assert_eq!(envelope["registrationId"], REGISTRATION);
        assert_eq!(envelope["expiresAt"], 1_000 + LIFETIME_SECS);
        let body = envelope["body"].as_str().expect("body");
        let signature = BASE64URL
            .decode(envelope["signature"].as_str().expect("signature"))
            .expect("base64");
        let public = BASE64URL.decode(key.public_key()).expect("public key");
        let verifier = UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, public);
        let input = signing_input(&registration_id(), 1_000 + LIFETIME_SECS, body);
        verifier
            .verify(input.as_bytes(), &signature)
            .expect("valid signature");
        let other = IdempotencyKey::new("00000000-0000-4000-8000-000000000002").expect("uuid");
        for forged in [
            signing_input(&other, 1_000 + LIFETIME_SECS, body),
            signing_input(&registration_id(), 9_999_999, body),
        ] {
            assert!(verifier.verify(forged.as_bytes(), &signature).is_err());
        }
        let plaintext = receiver.decrypt(&BASE64URL.decode(body).expect("base64"));
        let opened: PushMessage = serde_json::from_slice(&plaintext).expect("message");
        assert_eq!(opened, message);
    }

    #[test]
    fn a_full_message_keeps_blocked_notices_before_finished_turns() {
        let receiver = ece::tests::Receiver::new();
        let subscription = subscription_for(&receiver);
        let blocked = protocol::Notice {
            agent_id: "w1:p1".parse().expect("agent"),
            name: "界".repeat(60),
            state: protocol::NoticeState::Blocked,
            line: Some("😀".repeat(160)),
            decide: None,
            pending_id: None,
            announce: true,
        };
        let finished = protocol::Notice {
            state: protocol::NoticeState::Finished,
            decide: Some("😀".repeat(160)),
            ..blocked.clone()
        };
        let message = PushMessage {
            ping: "ab".into(),
            notices: vec![
                finished.clone(),
                blocked.clone(),
                finished.clone(),
                finished,
            ],
        };
        let key = super::super::vapid::tests::key();
        let text = seal(&registration_id(), &subscription, &message, &key, 0).expect("seal");
        assert!(text.len() <= MAX_ENVELOPE);
        let body = envelope_json(&text)["body"]
            .as_str()
            .expect("body")
            .to_owned();
        let plaintext = receiver.decrypt(&BASE64URL.decode(body).expect("base64"));
        let opened: PushMessage = serde_json::from_slice(&plaintext).expect("message");
        assert!(opened.notices.contains(&blocked), "the blocked agent stays");
        assert!(opened.notices.len() < message.notices.len());
    }

    /// The vector the Android app's `NativeEnvelopeTest` opens: an
    /// envelope in this module's format, signed over [`signing_input`].
    #[test]
    fn the_android_envelope_vector_is_one_the_bridge_signs() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/native-envelope-vector.json"))
                .expect("vector");
        let notice: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/webpush-notice-vector.json"))
                .expect("notice vector");
        let envelope = envelope_json(vector["envelope"].as_str().expect("envelope"));
        let registration =
            IdempotencyKey::new(envelope["registrationId"].as_str().expect("id")).expect("uuid");
        let body = envelope["body"].as_str().expect("body");
        assert_eq!(body, notice["body"], "the notice vector's ciphertext");
        let input = signing_input(
            &registration,
            envelope["expiresAt"].as_u64().expect("expiry"),
            body,
        );
        let public = BASE64URL
            .decode(vector["signer"].as_str().expect("key"))
            .expect("base64");
        let signature = BASE64URL
            .decode(envelope["signature"].as_str().expect("signature"))
            .expect("base64");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, public)
            .verify(input.as_bytes(), &signature)
            .expect("signed as the bridge signs");
    }

    #[test]
    fn the_stream_cursor_is_checked_before_reaching_the_companion() {
        assert_eq!(since(None), Ok("all"));
        assert_eq!(since(Some("since=aBc_9-x")), Ok("aBc_9-x"));
        for bad in [
            "since=../x",
            "since=",
            "since=a%2Fb",
            "poll=1",
            "since=a&poll=1",
            "since",
        ] {
            assert_eq!(since(Some(bad)), Err(StatusCode::BAD_REQUEST), "{bad}");
        }
        let long = format!("since={}", "a".repeat(65));
        assert_eq!(since(Some(&long)), Err(StatusCode::BAD_REQUEST));
    }
}
