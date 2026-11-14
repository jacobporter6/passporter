//! The built-in Android transport (`push::native`): registration and the
//! stream are app-scoped and bound to the registering device, and a
//! blocked agent reaches the phone as a signed envelope through a fake ntfy
//! companion on loopback, also after the companion was down. No network
//! beyond loopback, no real ntfy.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bridge::auth::AuthSettings;
use bridge::push::VapidKey;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use p256::ecdsa::SigningKey;
use protocol::{DeviceId, NativeRegistration};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::harness::CuttableHerdr;
use crate::harness::PATIENCE;
use crate::pairing::{Paired, key, start_on};
use crate::push;

const REGISTRATION: &str = "00000000-0000-4000-8000-0000000000aa";

/// A stand-in for ntfy's JSON publish and subscribe API: what the bridge
/// uses of it, kept in memory.
#[derive(Default)]
struct FakeNtfy {
    /// Per topic, `(id, message)` in order.
    topics: Mutex<BTreeMap<String, Vec<(String, String)>>>,
    /// Bumped on every publish.
    published: watch::Sender<u64>,
    /// While set, publishing answers 503.
    down: AtomicBool,
}

impl FakeNtfy {
    /// Serves on a fresh loopback port.
    async fn start() -> (SocketAddr, Arc<Self>) {
        let ntfy = Arc::new(Self::default());
        let app = Router::new()
            .route("/", post(publish))
            .route("/{topic}/json", get(subscribe))
            .with_state(Arc::clone(&ntfy));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await });
        (addr, ntfy)
    }

    fn messages_after(&self, topic: &str, since: &str) -> Vec<(String, String)> {
        let topics = self.topics.lock().expect("lock");
        let all = topics.get(topic).cloned().unwrap_or_default();
        match all.iter().position(|(id, _)| id == since) {
            Some(at) => all[at + 1..].to_vec(),
            None => all,
        }
    }
}

async fn publish(State(ntfy): State<Arc<FakeNtfy>>, body: Bytes) -> StatusCode {
    if ntfy.down.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    let json: serde_json::Value = serde_json::from_slice(&body).expect("publish json");
    let topic = json["topic"].as_str().expect("topic").to_owned();
    let message = json["message"].as_str().expect("message").to_owned();
    {
        let mut topics = ntfy.topics.lock().expect("lock");
        let list = topics.entry(topic).or_default();
        let id = format!("m{}", list.len());
        list.push((id, message));
    }
    ntfy.published.send_modify(|n| *n += 1);
    StatusCode::OK
}

async fn subscribe(
    State(ntfy): State<Arc<FakeNtfy>>,
    Path(topic): Path<String>,
    RawQuery(query): RawQuery,
) -> Body {
    let since = query
        .as_deref()
        .and_then(|q| q.strip_prefix("since="))
        .unwrap_or("all")
        .to_owned();
    let open = serde_json::json!({"event": "open", "topic": topic}).to_string() + "\n";
    let changes = ntfy.published.subscribe();
    let stream = futures_util::stream::unfold(
        (ntfy, changes, topic, since, Some(open)),
        |(ntfy, mut changes, topic, mut since, first)| async move {
            if let Some(open) = first {
                let state = (ntfy, changes, topic, since, None);
                return Some((Ok::<_, std::io::Error>(open), state));
            }
            loop {
                let fresh = ntfy.messages_after(&topic, &since);
                if let Some((id, message)) = fresh.into_iter().next() {
                    let line = serde_json::json!({
                        "id": id, "event": "message", "topic": topic, "message": message,
                    })
                    .to_string()
                        + "\n";
                    since = id;
                    return Some((Ok(line), (ntfy, changes, topic, since, None)));
                }
                changes.changed().await.ok()?;
            }
        },
    );
    Body::from_stream(stream)
}

/// A paired bridge whose built-in transport uses `ntfy` (or no companion
/// at all), with one device and its app session.
struct Phone {
    paired: Paired,
    device: DeviceId,
    key: SigningKey,
    token: String,
}

async fn phone_with(companion: Option<SocketAddr>) -> Phone {
    phone_on(CuttableHerdr::new(), companion).await
}

/// [`phone_with`], in front of `herdr`.
async fn phone_on(herdr: CuttableHerdr, companion: Option<SocketAddr>) -> Phone {
    let paired = start_on(herdr, AuthSettings::default(), None, |settings, dir| {
        let vapid = VapidKey::generate(&dir.join("vapid.pem")).expect("key");
        let mut push = push::settings(vapid, dir, Arc::new(push::Recorder::default()));
        push.native_ntfy = companion;
        settings.push = Some(push);
    })
    .await;
    let key = key(9);
    let device = paired.paired_device(&key).await;
    let token = paired.session(&device, &key).await;
    Phone {
        paired,
        device,
        key,
        token,
    }
}

/// A registration body with a real public key.
fn register_body(registration: &str) -> String {
    let browser: serde_json::Value =
        serde_json::from_str(&push::browser_subscription("https://push.example.com/x"))
            .expect("json");
    serde_json::json!({
        "registrationId": registration,
        "p256dh": browser["keys"]["p256dh"],
        "auth": browser["keys"]["auth"],
    })
    .to_string()
}

fn stream_path(registration: &str) -> String {
    format!("/v1/push/native/{registration}/stream")
}

#[tokio::test]
async fn registration_and_streams_are_app_scoped_and_bound_to_the_registering_device() {
    let (companion, _ntfy) = FakeNtfy::start().await;
    let phone = phone_with(Some(companion)).await;
    let paired = &phone.paired;
    let body = register_body(REGISTRATION);

    assert_eq!(
        paired
            .status_of("not-a-token", "POST", "/v1/push/native", &body)
            .await,
        401
    );
    let car = paired.car_session(&phone.device, &phone.key).await;
    assert_eq!(
        paired
            .status_of(&car, "POST", "/v1/push/native", &body)
            .await,
        403,
        "a car session cannot register"
    );
    assert_eq!(
        paired
            .status_of(
                &phone.token,
                "POST",
                "/v1/push/native",
                "{\"registrationId\":1}"
            )
            .await,
        400
    );

    let registered = paired
        .with(&phone.token, "POST", "/v1/push/native", &body)
        .await;
    assert_eq!(registered.status, 200, "{}", registered.body);
    let registration: NativeRegistration =
        serde_json::from_str(&registered.body).expect("registration");
    assert_eq!(registration.version, 1);
    assert_eq!(registration.stream_path, stream_path(REGISTRATION));
    let vapid = paired.with(&phone.token, "GET", "/v1/push/key", "").await;
    assert!(vapid.body.contains(&registration.signing_key));
    assert_eq!(
        paired
            .with(&phone.token, "POST", "/v1/push/native", &body)
            .await
            .status,
        200,
        "registering again replaces the keys"
    );

    // The ordinary route never creates (or re-points) a reserved endpoint.
    let reserved =
        push::browser_subscription(&format!("https://native.passporter.invalid/{REGISTRATION}"));
    assert_eq!(
        paired
            .status_of(&phone.token, "POST", "/v1/push/subscriptions", &reserved)
            .await,
        400
    );

    // Another device can neither take the registration nor read it.
    let other_key = key(10);
    let other = paired.paired_device(&other_key).await;
    let other_token = paired.session(&other, &other_key).await;
    assert_eq!(
        paired
            .status_of(&other_token, "POST", "/v1/push/native", &body)
            .await,
        409
    );
    let path = stream_path(REGISTRATION);
    assert_eq!(paired.status_of(&other_token, "GET", &path, "").await, 404);
    assert_eq!(paired.status_of(&car, "GET", &path, "").await, 403);
    assert_eq!(
        paired
            .status_of(&phone.token, "GET", &format!("{path}?since=..%2Fx"), "")
            .await,
        400,
        "a bad cursor never reaches the companion"
    );

    paired.auth.revoke(&phone.device).await.expect("revoke");
    assert_eq!(paired.status_of(&phone.token, "GET", &path, "").await, 401);
    phone.paired.bridge.stop().await.expect("stop");
}

#[tokio::test]
async fn without_a_companion_the_built_in_routes_are_absent() {
    let phone = phone_with(None).await;
    let paired = &phone.paired;
    let body = register_body(REGISTRATION);
    assert_eq!(
        paired
            .status_of(&phone.token, "POST", "/v1/push/native", &body)
            .await,
        404
    );
    assert_eq!(
        paired
            .status_of(&phone.token, "GET", &stream_path(REGISTRATION), "")
            .await,
        404
    );
    phone.paired.bridge.stop().await.expect("stop");
}

/// Opens the phone's stream; its body.
async fn open_stream(phone: &Phone) -> hyper::body::Incoming {
    let client = Client::builder(TokioExecutor::new()).build_http();
    let request = axum::http::Request::get(format!(
        "http://{}{}",
        phone.paired.bridge.addr,
        stream_path(REGISTRATION)
    ))
    .header("authorization", format!("Bearer {}", phone.token))
    .body(Full::new(Bytes::new()))
    .expect("request");
    let response = client.request(request).await.expect("stream response");
    assert_eq!(response.status(), 200);
    response.into_body()
}

/// Reads `body` until a `message` event; that event.
async fn next_message(body: &mut hyper::body::Incoming) -> serde_json::Value {
    tokio::time::timeout(PATIENCE, async {
        let mut bytes = Vec::new();
        loop {
            let frame = body.frame().await.expect("stream open").expect("frame");
            if let Ok(data) = frame.into_data() {
                bytes.extend_from_slice(&data);
            }
            assert!(bytes.len() < 64 * 1024, "bounded test stream");
            for line in bytes.split(|b| *b == b'\n') {
                if let Ok(event) = serde_json::from_slice::<serde_json::Value>(line)
                    && event["event"] == "message"
                {
                    return event;
                }
            }
        }
    })
    .await
    .expect("a message in time")
}

/// Checks `event`'s envelope against `registration`'s signing key; the
/// envelope.
fn verified(event: &serde_json::Value, registration: &NativeRegistration) -> serde_json::Value {
    let text = event["message"].as_str().expect("envelope text");
    let envelope: serde_json::Value = serde_json::from_str(text).expect("envelope");
    assert_eq!(envelope["version"], 1);
    assert_eq!(envelope["registrationId"], REGISTRATION);
    let public = URL_SAFE_NO_PAD
        .decode(&registration.signing_key)
        .expect("key");
    let signature = URL_SAFE_NO_PAD
        .decode(envelope["signature"].as_str().expect("signature"))
        .expect("signature");
    let input = format!(
        "passporter-notice-v1\n{REGISTRATION}\n{}\n{}",
        envelope["expiresAt"].as_u64().expect("expiry"),
        envelope["body"].as_str().expect("body"),
    );
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, public)
        .verify(input.as_bytes(), &signature)
        .expect("signed by the bridge");
    envelope
}

/// Waits until the bridge lists one agent (with the phone's session).
async fn wait_for_one_agent(phone: &Phone) {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let response = phone
                .paired
                .with(&phone.token, "GET", "/v1/agents", "")
                .await;
            assert_eq!(response.status, 200, "{}", response.body);
            let agents: Vec<protocol::Agent> =
                serde_json::from_str(&response.body).expect("agents");
            if agents.len() == 1 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the agent is listed");
}

async fn register(phone: &Phone) -> NativeRegistration {
    let response = phone
        .paired
        .with(
            &phone.token,
            "POST",
            "/v1/push/native",
            &register_body(REGISTRATION),
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    serde_json::from_str(&response.body).expect("registration")
}

#[tokio::test]
async fn a_blocked_agent_reaches_the_stream_as_a_signed_envelope_and_revocation_ends_it() {
    let (companion, _ntfy) = FakeNtfy::start().await;
    let herdr = CuttableHerdr::new();
    let pane = herdr
        .fake
        .add_agent("secret-agent-title", "claude", herdr::AgentStatus::Idle);
    let phone = phone_on(herdr, Some(companion)).await;
    let fake = phone.paired.bridge.fake();
    wait_for_one_agent(&phone).await;
    let registration = register(&phone).await;
    let mut stream = open_stream(&phone).await;

    fake.set_status(&pane, herdr::AgentStatus::Working);
    fake.set_status(&pane, herdr::AgentStatus::Blocked);
    let event = next_message(&mut stream).await;
    verified(&event, &registration);
    assert!(
        !event.to_string().contains("secret-agent"),
        "the companion and the stream carry ciphertext only"
    );

    phone
        .paired
        .auth
        .revoke(&phone.device)
        .await
        .expect("revoke");
    tokio::time::timeout(PATIENCE, async {
        while let Some(Ok(_)) = stream.frame().await {}
    })
    .await
    .expect("a revoked device's stream ends");
    phone.paired.bridge.stop().await.expect("stop");
}

#[tokio::test]
async fn a_notice_spooled_while_the_companion_is_down_arrives_once_it_is_back() {
    let (companion, ntfy) = FakeNtfy::start().await;
    ntfy.down.store(true, Ordering::SeqCst);
    let herdr = CuttableHerdr::new();
    let pane = herdr
        .fake
        .add_agent("waiting", "claude", herdr::AgentStatus::Idle);
    let phone = phone_on(herdr, Some(companion)).await;
    let fake = phone.paired.bridge.fake();
    wait_for_one_agent(&phone).await;
    let registration = register(&phone).await;

    fake.set_status(&pane, herdr::AgentStatus::Working);
    fake.set_status(&pane, herdr::AgentStatus::Blocked);
    // Long enough for the first publish to have failed.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let topic = format!("pp-{REGISTRATION}");
    assert!(
        ntfy.messages_after(&topic, "all").is_empty(),
        "not published yet"
    );
    ntfy.down.store(false, Ordering::SeqCst);

    let mut stream = open_stream(&phone).await;
    let event = next_message(&mut stream).await;
    verified(&event, &registration);
    phone.paired.bridge.stop().await.expect("stop");
}
