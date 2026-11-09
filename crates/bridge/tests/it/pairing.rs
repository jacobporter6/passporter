//! Device auth end to end (design §8): pairing, signed sessions, replay,
//! expiry (of codes, and of sessions under an open stream), rate limits,
//! revocation of an open stream, the refusal statuses, and the static-token
//! opt-in.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bridge::auth::{AuthSettings, DeviceAuth, RateLimit};
use bridge::config::Config;
use bridge::push::store::STORE_FILE;
use bridge::push::{SubscriptionStore, VapidKey};
use bridge::server::StaticToken;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use protocol::{
    ActionResult, Challenge, ChallengeResponse, DeviceId, PairResponse, PairingCode, RefusalReason,
    Scope, SessionGrant, signed_message,
};
use tokio::net::TcpStream;

use crate::harness::{Bridge, CuttableHerdr, HttpResponse, PATIENCE, Stream, request};
use crate::push;

const STATIC_SECRET: &str = "feedfacefeedfacefeedfacefeedface";

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_slice(&[seed; 32]).expect("valid scalar")
}

fn public_hex(key: &SigningKey) -> String {
    hex(key.verifying_key().to_sec1_point(false).as_bytes())
}

fn sign(key: &SigningKey, device: &DeviceId, challenge: &Challenge) -> String {
    let signature: Signature = key.sign(&signed_message(device, challenge));
    hex(&signature.to_bytes())
}

/// A bridge running [`DeviceAuth`] with its state in a temp dir.
struct Paired {
    bridge: Bridge,
    auth: Arc<DeviceAuth>,
    state: tempfile::TempDir,
}

async fn start(settings: AuthSettings, static_token: Option<StaticToken>) -> Paired {
    start_with(settings, static_token, |_, _| {}).await
}

/// [`start`], adjusting the bridge settings first; `adjust` also gets the
/// state dir.
async fn start_with(
    settings: AuthSettings,
    static_token: Option<StaticToken>,
    adjust: impl FnOnce(&mut bridge::Settings, &Path),
) -> Paired {
    let state = tempfile::tempdir().expect("tmp");
    let auth = Arc::new(DeviceAuth::open(state.path(), settings, static_token).expect("auth"));
    let bridge = Bridge::start_full(CuttableHerdr::new(), auth.clone(), |bridge| {
        adjust(bridge, state.path());
    })
    .await;
    Paired {
        bridge,
        auth,
        state,
    }
}

/// [`start`] with default settings, and one working Claude agent in herdr
/// from the start.
async fn start_with_working_agent() -> Paired {
    let state = tempfile::tempdir().expect("tmp");
    let auth =
        Arc::new(DeviceAuth::open(state.path(), AuthSettings::default(), None).expect("auth"));
    let herdr = CuttableHerdr::new();
    herdr
        .fake
        .add_agent("busy", "claude", herdr::AgentStatus::Working);
    let bridge = Bridge::start_full(herdr, auth.clone(), |_| {}).await;
    Paired {
        bridge,
        auth,
        state,
    }
}

impl Paired {
    async fn post(&self, path: &str, body: &str) -> HttpResponse {
        let tcp = TcpStream::connect(self.bridge.addr).await.expect("connect");
        request(
            tcp,
            "POST",
            path,
            &[("content-type", "application/json")],
            body,
        )
        .await
    }

    async fn pair(&self, code: &PairingCode, key: &SigningKey) -> HttpResponse {
        let body = format!(
            r#"{{"code":"{}","publicKey":"{}","name":"Test phone"}}"#,
            code.as_str(),
            public_hex(key)
        );
        self.post("/v1/auth/pair", &body).await
    }

    async fn paired_device(&self, key: &SigningKey) -> DeviceId {
        let code = self.auth.start_pairing(None).expect("code").code;
        let response = self.pair(&code, key).await;
        assert_eq!(response.status, 200, "{}", response.body);
        serde_json::from_str::<PairResponse>(&response.body)
            .expect("pair response")
            .device_id
    }

    async fn challenge(&self, device: &DeviceId) -> Challenge {
        let response = self
            .post(
                "/v1/auth/challenge",
                &format!(r#"{{"deviceId":"{device}"}}"#),
            )
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        serde_json::from_str::<ChallengeResponse>(&response.body)
            .expect("challenge")
            .challenge
    }

    async fn redeem(
        &self,
        device: &DeviceId,
        challenge: &Challenge,
        signature: &str,
    ) -> HttpResponse {
        let body = format!(
            r#"{{"deviceId":"{device}","challenge":"{challenge}","signature":"{signature}"}}"#
        );
        self.post("/v1/auth/session", &body).await
    }

    async fn session(&self, device: &DeviceId, key: &SigningKey) -> String {
        let challenge = self.challenge(device).await;
        let response = self
            .redeem(device, &challenge, &sign(key, device, &challenge))
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        let grant: SessionGrant = serde_json::from_str(&response.body).expect("grant");
        grant.token
    }

    /// A session token of scope `car` (Android Auto).
    async fn car_session(&self, device: &DeviceId, key: &SigningKey) -> String {
        let challenge = self.challenge(device).await;
        let body = format!(
            r#"{{"deviceId":"{device}","challenge":"{challenge}","signature":"{}","scope":"car"}}"#,
            sign(key, device, &challenge)
        );
        let response = self.post("/v1/auth/session", &body).await;
        assert_eq!(response.status, 200, "{}", response.body);
        let grant: SessionGrant = serde_json::from_str(&response.body).expect("grant");
        assert_eq!(grant.scope, Scope::Car);
        grant.token
    }

    /// `method path` with `token` and a JSON `body`: the status.
    async fn status_of(&self, token: &str, method: &str, path: &str, body: &str) -> u16 {
        self.with(token, method, path, body).await.status
    }

    async fn with(&self, token: &str, method: &str, path: &str, body: &str) -> HttpResponse {
        let tcp = TcpStream::connect(self.bridge.addr).await.expect("connect");
        let auth = format!("Bearer {token}");
        request(
            tcp,
            method,
            path,
            &[
                ("authorization", &auth),
                ("content-type", "application/json"),
            ],
            body,
        )
        .await
    }

    async fn agents_status(&self, token: &str) -> u16 {
        self.bridge
            .http(
                "GET",
                "/v1/agents",
                &[("authorization", &format!("Bearer {token}"))],
            )
            .await
            .status
    }
}

#[tokio::test]
async fn a_paired_device_signs_in_and_uses_the_api() {
    let paired = start(AuthSettings::default(), None).await;
    let key = key(7);
    let device = paired.paired_device(&key).await;
    let challenge = paired.challenge(&device).await;
    let response = paired
        .redeem(&device, &challenge, &sign(&key, &device, &challenge))
        .await;
    let grant: SessionGrant = serde_json::from_str(&response.body).expect("grant");
    assert_eq!(grant.expires_in_secs, 15 * 60);
    let token = grant.token;
    assert_eq!(paired.agents_status(&token).await, 200);
    assert_eq!(paired.agents_status("0".repeat(64).as_str()).await, 401);
    let devices = paired.auth.devices();
    assert_eq!(devices[0].record.name.as_str(), "Test phone");
    let refreshed = paired.session(&device, &key).await;
    assert_ne!(refreshed, token, "each signature mints a fresh token");
    assert_eq!(paired.agents_status(&refreshed).await, 200);
}

#[tokio::test]
async fn a_replayed_challenge_is_refused() {
    let paired = start(AuthSettings::default(), None).await;
    let key = key(7);
    let device = paired.paired_device(&key).await;
    let challenge = paired.challenge(&device).await;
    let signature = sign(&key, &device, &challenge);
    assert_eq!(
        paired.redeem(&device, &challenge, &signature).await.status,
        200
    );
    let replay = paired.redeem(&device, &challenge, &signature).await;
    assert_eq!(replay.status, 401);
    assert_eq!(replay.header("www-authenticate"), Some("Bearer"));
}

#[tokio::test]
async fn an_expired_pairing_code_is_refused() {
    let paired = start(
        AuthSettings {
            pairing_ttl: Duration::from_millis(50),
            ..AuthSettings::default()
        },
        None,
    )
    .await;
    let code = paired.auth.start_pairing(None).expect("code").code;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(paired.pair(&code, &key(7)).await.status, 401);
    assert_eq!(paired.auth.devices().len(), 0);
}

#[tokio::test]
async fn pairing_and_challenges_are_rate_limited() {
    let paired = start(
        AuthSettings {
            pair_rate: RateLimit::per_minute(2),
            session_rate: RateLimit::per_minute(2),
            ..AuthSettings::default()
        },
        None,
    )
    .await;
    let wrong = PairingCode::new("00000000").expect("code");
    for _ in 0..2 {
        assert_eq!(paired.pair(&wrong, &key(7)).await.status, 401);
    }
    let limited = paired.pair(&wrong, &key(7)).await;
    assert_eq!(limited.status, 429);
    let retry: u64 = limited
        .header("retry-after")
        .and_then(|v| v.parse().ok())
        .expect("retry-after");
    assert!((1..=60).contains(&retry), "{retry}");

    let unknown = r#"{"deviceId":"dev-0000000000000001"}"#;
    for _ in 0..2 {
        assert_eq!(paired.post("/v1/auth/challenge", unknown).await.status, 410);
    }
    assert_eq!(paired.post("/v1/auth/challenge", unknown).await.status, 429);
}

#[tokio::test]
async fn a_car_session_reaches_only_the_car_routes() {
    let paired = start(AuthSettings::default(), None).await;
    let key = key(7);
    let device = paired.paired_device(&key).await;
    let car = paired.car_session(&device, &key).await;
    let app = paired.session(&device, &key).await;
    let agent = "w1:p1";

    // Car-safe: past the scope check (the handler answers, whatever it says).
    assert_eq!(paired.status_of(&car, "GET", "/v1/agents", "").await, 200);
    for path in [
        format!("/v1/agents/{agent}/pending"),
        format!("/v1/agents/{agent}/tldr"),
    ] {
        assert_ne!(
            paired.status_of(&car, "GET", &path, "").await,
            403,
            "{path}"
        );
    }
    for path in [
        format!("/v1/agents/{agent}/car-reply"),
        "/v1/actions".to_owned(),
    ] {
        assert_ne!(
            paired.status_of(&car, "POST", &path, "{}").await,
            403,
            "{path}"
        );
    }

    // Everything else is the app's (deny by default), also routes the app
    // cannot use here, and paths that match no route at all.
    for (method, path) in [
        ("GET", format!("/v1/agents/{agent}/thread")),
        ("GET", format!("/v1/agents/{agent}/screen")),
        ("GET", format!("/v1/agents/{agent}/suggestions")),
        ("POST", format!("/v1/agents/{agent}/status")),
        ("GET", format!("/v1/agents/{agent}/commands")),
        ("POST", format!("/v1/agents/{agent}/commands/run")),
        ("GET", "/v1/terminals/w1:p2/screen".to_owned()),
        ("POST", "/v1/terminals/w1:p2/keys".to_owned()),
        ("GET", "/v1/no-such-route".to_owned()),
        ("POST", "/v1/agents".to_owned()),
        ("GET", "/v1/projects".to_owned()),
        ("GET", "/v1/runbooks".to_owned()),
        ("POST", "/v1/runbooks/deploy/run".to_owned()),
        ("GET", "/v1/push/key".to_owned()),
        ("POST", "/v1/push/subscriptions".to_owned()),
        ("POST", "/v1/audio".to_owned()),
        (
            "PATCH",
            "/v1/audio/00000000-0000-4000-8000-000000000001".to_owned(),
        ),
        ("POST", "/v1/attachments".to_owned()),
    ] {
        assert_eq!(
            paired.status_of(&car, method, &path, "{}").await,
            403,
            "{method} {path}"
        );
        assert_ne!(
            paired.status_of(&app, method, &path, "{}").await,
            403,
            "the app may: {method} {path}"
        );
    }

    // The stream is the app's too: a car token closes it like a bad one.
    let tcp = TcpStream::connect(paired.bridge.addr)
        .await
        .expect("connect");
    let mut stream = Stream::connect(tcp, Some(&car)).await.expect("handshake");
    assert_eq!(stream.close_code().await, Some(1008));
}

#[tokio::test]
async fn a_car_session_acts_as_the_car_whatever_the_body_says() {
    let paired = start_with_working_agent().await;
    let key = key(7);
    let device = paired.paired_device(&key).await;
    let car = paired.car_session(&device, &key).await;
    let app = paired.session(&device, &key).await;
    let listed = async {
        loop {
            let response = paired.with(&app, "GET", "/v1/agents", "").await;
            let agents: Vec<protocol::Agent> =
                serde_json::from_str(&response.body).expect("agent list");
            if let Some(agent) = agents
                .into_iter()
                .find(|agent| agent.status == protocol::AgentStatus::Working)
            {
                return agent;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let agent = tokio::time::timeout(PATIENCE, listed)
        .await
        .expect("the agent is listed");
    let interrupt = |key: u8| {
        serde_json::json!({
            "idempotencyKey": format!("00000000-0000-4000-8000-{key:012}"),
            "agentId": agent.id,
            "expectedSeq": agent.seq,
            "createdAt": 1_762_500_000_000_u64,
            "kind": "interrupt",
            "payload": {},
            "origin": "app",
        })
        .to_string()
    };
    // The body claims the app, but a car token is the car: no interrupt.
    let response = paired
        .with(&car, "POST", "/v1/actions", &interrupt(1))
        .await;
    assert_eq!(response.status, 403, "{}", response.body);
    let result: ActionResult = serde_json::from_str(&response.body).expect("result");
    assert!(
        matches!(
            result,
            ActionResult::Refused {
                reason: RefusalReason::CarPolicy,
                ..
            }
        ),
        "{result:?}"
    );
    // The same body from an app token interrupts.
    let response = paired
        .with(&app, "POST", "/v1/actions", &interrupt(2))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
}

#[tokio::test]
async fn revoking_a_device_closes_its_open_stream() {
    let paired = start(AuthSettings::default(), None).await;
    let key = key(7);
    let device = paired.paired_device(&key).await;
    let token = paired.session(&device, &key).await;
    let tcp = TcpStream::connect(paired.bridge.addr)
        .await
        .expect("connect");
    let mut stream = Stream::connect(tcp, Some(&token)).await.expect("handshake");
    // Authenticated: the snapshot arrives.
    stream.next_event().await;

    assert_eq!(paired.auth.revoke(&device).await.expect("revoked"), 1);
    assert_eq!(stream.close_code().await, Some(1008));
    assert_eq!(paired.agents_status(&token).await, 401);
    let challenge = paired
        .post(
            "/v1/auth/challenge",
            &format!(r#"{{"deviceId":"{device}"}}"#),
        )
        .await;
    assert_eq!(challenge.status, 410, "revoked: the phone forgets its key");
}

#[tokio::test]
async fn revoking_a_device_drops_its_push_subscriptions() {
    let paired = start_with(AuthSettings::default(), None, |settings, dir| {
        let vapid = VapidKey::generate(&dir.join("vapid.pem")).expect("key");
        settings.push = Some(push::settings(vapid, dir, Arc::default()));
    })
    .await;
    let store = paired.state.path().join(STORE_FILE);
    let devices = || SubscriptionStore::open(&store).expect("store").devices();
    let key = key(9);
    let device = paired.paired_device(&key).await;
    let token = paired.session(&device, &key).await;
    let tcp = TcpStream::connect(paired.bridge.addr)
        .await
        .expect("connect");
    let subscribed = request(
        tcp,
        "POST",
        "/v1/push/subscriptions",
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", "application/json"),
        ],
        &push::browser_subscription("https://push.example.com/phone"),
    )
    .await;
    assert_eq!(subscribed.status, 201, "{}", subscribed.body);
    assert_eq!(
        devices(),
        [device.to_string()],
        "keyed by the paired device"
    );

    paired.auth.revoke(&device).await.expect("revoked");
    tokio::time::timeout(PATIENCE, async {
        while !devices().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the revoked device's subscriptions are dropped");
    paired.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn an_open_stream_closes_when_its_session_expires() {
    let paired = start(
        AuthSettings {
            session_ttl: Duration::from_millis(300),
            ..AuthSettings::default()
        },
        None,
    )
    .await;
    let key = key(7);
    let device = paired.paired_device(&key).await;
    let token = paired.session(&device, &key).await;
    let tcp = TcpStream::connect(paired.bridge.addr)
        .await
        .expect("connect");
    let mut stream = Stream::connect(tcp, Some(&token)).await.expect("handshake");
    stream.next_event().await;
    assert_eq!(stream.close_code().await, Some(1008));
    assert_eq!(paired.agents_status(&token).await, 401);
}

#[tokio::test]
async fn refusals_tell_a_stale_challenge_from_a_gone_device() {
    let paired = start(AuthSettings::default(), None).await;
    let key = key(7);
    let device = paired.paired_device(&key).await;
    // Signed for a challenge the bridge never issued (or forgot): 401.
    let forgotten = Challenge::new("ab".repeat(32)).expect("challenge");
    let stale = paired
        .redeem(&device, &forgotten, &sign(&key, &device, &forgotten))
        .await;
    assert_eq!(stale.status, 401);
    // A step-up claim the signature cannot back: 400.
    let challenge = paired.challenge(&device).await;
    let body = format!(
        r#"{{"deviceId":"{device}","challenge":"{challenge}","signature":"{}","stepUp":true}}"#,
        sign(&key, &device, &challenge)
    );
    assert_eq!(paired.post("/v1/auth/session", &body).await.status, 400);
    // A device the bridge does not know: 410.
    let stranger = DeviceId::new("dev-0000000000000001").expect("id");
    let gone = paired
        .redeem(&stranger, &forgotten, &sign(&key, &stranger, &forgotten))
        .await;
    assert_eq!(gone.status, 410);
}

#[tokio::test]
async fn malformed_requests_are_rejected_before_any_state_changes() {
    let paired = start(AuthSettings::default(), None).await;
    let code = paired.auth.start_pairing(None).expect("code").code;
    let extra = format!(
        r#"{{"code":"{}","publicKey":"{}","admin":true}}"#,
        code.as_str(),
        public_hex(&key(7))
    );
    assert_eq!(paired.post("/v1/auth/pair", &extra).await.status, 422);
    let off_curve = format!(
        r#"{{"code":"{}","publicKey":"04{}"}}"#,
        code.as_str(),
        "00".repeat(64)
    );
    assert_eq!(paired.post("/v1/auth/pair", &off_curve).await.status, 401);
    // The code survives both: neither got as far as using it.
    assert_eq!(paired.pair(&code, &key(7)).await.status, 200);
}

/// The static token from a config file, as the binary builds it.
async fn start_from_config(text: &str, dir: &Path) -> Bridge {
    let config = Config::parse(text).expect("config");
    let auth = DeviceAuth::open(
        dir,
        AuthSettings::default(),
        config.static_token().cloned().map(StaticToken::new),
    )
    .expect("auth");
    Bridge::start_full(CuttableHerdr::new(), Arc::new(auth), |_| {}).await
}

#[tokio::test]
async fn the_static_token_works_when_the_config_opts_in() {
    let tmp = tempfile::tempdir().expect("tmp");
    let config = format!("api_token = \"{STATIC_SECRET}\"\nallow_static_token = true\n");
    let bridge = start_from_config(&config, tmp.path()).await;
    let good = format!("Bearer {STATIC_SECRET}");
    let ok = bridge
        .http("GET", "/v1/agents", &[("authorization", &good)])
        .await;
    assert_eq!(ok.status, 200);
    let tcp = TcpStream::connect(bridge.addr).await.expect("connect");
    let mut stream = Stream::connect(tcp, Some(STATIC_SECRET))
        .await
        .expect("handshake");
    stream.next_event().await;
}

#[tokio::test]
async fn the_static_token_is_refused_when_the_config_says_so() {
    let tmp = tempfile::tempdir().expect("tmp");
    let config = format!("api_token = \"{STATIC_SECRET}\"\nallow_static_token = false\n");
    let bridge = start_from_config(&config, tmp.path()).await;
    let good = format!("Bearer {STATIC_SECRET}");
    let refused = bridge
        .http("GET", "/v1/agents", &[("authorization", &good)])
        .await;
    assert_eq!(refused.status, 401);
    let tcp = TcpStream::connect(bridge.addr).await.expect("connect");
    let mut stream = Stream::connect(tcp, Some(STATIC_SECRET))
        .await
        .expect("handshake");
    assert_eq!(stream.close_code().await, Some(1008));
}
