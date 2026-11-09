//! Web Push: the subscription endpoints, and a blocked agent reaching a
//! (fake) push service as an encrypted ping. No network beyond loopback.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bridge::push::store::{STORE_FILE, SubscriptionStore};
use bridge::push::{
    Delivery, PushHosts, PushRequest, PushSender, PushSettings, PushTiming, SendFuture, VapidKey,
};
use bridge::registry::QuietDone;
use bridge::server::{AuthFuture, AuthRejection, AuthRequest, Authenticator, Principal};
use herdr::{AgentStatus, ErrorCode, Fault, Herdr, Method};
use ring::agreement::{ECDH_P256, EphemeralPrivateKey};
use ring::rand::SystemRandom;
use tokio::net::TcpStream;

use crate::harness::{self, Bridge, CuttableHerdr, HttpResponse, PATIENCE, TOKEN};

const PATH: &str = "/v1/push/subscriptions";

/// Records requests, accepts them all.
#[derive(Debug, Default)]
pub(crate) struct Recorder(Mutex<Vec<PushRequest>>);

impl PushSender for Recorder {
    fn send(&self, request: PushRequest) -> SendFuture<'_> {
        self.0.lock().expect("lock").push(request);
        Box::pin(async { Ok(Delivery::Accepted) })
    }
}

impl Recorder {
    fn sent(&self) -> Vec<PushRequest> {
        self.0.lock().expect("lock").clone()
    }
}

struct PushBridge {
    bridge: Bridge,
    recorder: Arc<Recorder>,
    public_key: String,
    /// The key and store, kept for a restart.
    dir: tempfile::TempDir,
}

/// A bridge with push on, its key and store in a fresh temp dir.
async fn start(herdr: CuttableHerdr) -> PushBridge {
    start_in(herdr, tempfile::tempdir().expect("tmp")).await
}

/// A bridge with push on, its key and store in `dir` (made on first use,
/// kept across restarts). `push.example.com` is an allowed extra host.
async fn start_in(herdr: CuttableHerdr, dir: tempfile::TempDir) -> PushBridge {
    let key = dir.path().join("vapid.pem");
    let vapid = if key.exists() {
        VapidKey::load(&key)
    } else {
        VapidKey::generate(&key)
    }
    .expect("key");
    let public_key = vapid.public_key();
    let recorder = Arc::new(Recorder::default());
    let push = settings(vapid, dir.path(), Arc::clone(&recorder));
    let bridge = Bridge::start_with(herdr, |settings| settings.push = Some(push)).await;
    PushBridge {
        bridge,
        recorder,
        public_key,
        dir,
    }
}

/// Push settings with `vapid`, the store in `dir`, `recorder` as the push
/// service, `push.example.com` allowed and short timings.
pub(crate) fn settings(vapid: VapidKey, dir: &Path, recorder: Arc<Recorder>) -> PushSettings {
    let mut push = PushSettings::new(
        Arc::new(vapid),
        "mailto:ops@example.com".parse().expect("subject"),
        dir.join(STORE_FILE),
        recorder as Arc<dyn PushSender>,
    );
    push.hosts = PushHosts::new(vec!["push.example.com".parse().expect("host")]);
    push.timing = PushTiming {
        quiet: Duration::from_millis(20),
        max_delay: Duration::from_millis(100),
        min_interval: Duration::from_secs(1),
    };
    push
}

async fn call(bridge: &Bridge, method: &str, path: &str, body: &str) -> HttpResponse {
    let stream = TcpStream::connect(bridge.addr).await.expect("connect");
    let auth = format!("Bearer {TOKEN}");
    harness::request(
        stream,
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

/// What a browser's `PushSubscription.toJSON()` gives, with a real key.
pub(crate) fn browser_subscription(endpoint: &str) -> String {
    let rng = SystemRandom::new();
    let key = EphemeralPrivateKey::generate(&ECDH_P256, &rng).expect("key");
    let public = key.compute_public_key().expect("public");
    serde_json::json!({
        "endpoint": endpoint,
        "expirationTime": null,
        "keys": {
            "p256dh": URL_SAFE_NO_PAD.encode(public.as_ref()),
            "auth": URL_SAFE_NO_PAD.encode([7u8; 16]),
        },
    })
    .to_string()
}

fn endpoints(response: &HttpResponse) -> Vec<String> {
    assert_eq!(response.status, 200, "{}", response.body);
    let json: serde_json::Value = serde_json::from_str(&response.body).expect("json");
    json["subscriptions"]
        .as_array()
        .expect("array")
        .iter()
        .map(|s| s["endpoint"].as_str().expect("endpoint").to_owned())
        .collect()
}

#[tokio::test]
async fn subscription_crud() {
    let push = start(CuttableHerdr::new()).await;
    let bridge = &push.bridge;

    let key = bridge.get_authed("/v1/push/key").await;
    assert_eq!(key.status, 200, "{}", key.body);
    let json: serde_json::Value = serde_json::from_str(&key.body).expect("json");
    assert_eq!(json["vapidPublicKey"], push.public_key.as_str());

    assert_eq!(
        endpoints(&bridge.get_authed(PATH).await),
        Vec::<String>::new()
    );
    let a = browser_subscription("https://push.example.com/a");
    assert_eq!(call(bridge, "POST", PATH, &a).await.status, 201);
    assert_eq!(call(bridge, "POST", PATH, &a).await.status, 204, "replaced");
    let b = browser_subscription("https://push.example.com/b");
    assert_eq!(call(bridge, "POST", PATH, &b).await.status, 201);
    let listed = bridge.get_authed(PATH).await;
    assert_eq!(
        endpoints(&listed),
        ["https://push.example.com/a", "https://push.example.com/b"]
    );
    assert!(!listed.body.contains("p256dh"), "keys stay private");

    let drop_a = r#"{"endpoint":"https://push.example.com/a"}"#;
    assert_eq!(call(bridge, "DELETE", PATH, drop_a).await.status, 204);
    assert_eq!(call(bridge, "DELETE", PATH, drop_a).await.status, 404);
    assert_eq!(
        endpoints(&bridge.get_authed(PATH).await),
        ["https://push.example.com/b"]
    );
    push.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn invalid_and_unauthenticated_requests_are_refused() {
    let push = start(CuttableHerdr::new()).await;
    let bridge = &push.bridge;
    for body in [
        browser_subscription("http://push.example.com/plain"),
        browser_subscription("https://192.0.2.1/lan"),
        browser_subscription("https://0x7f000001/lan"),
        browser_subscription("https://push.example.com:8443/x"),
        browser_subscription("https://push.example.com.attacker.example/x"),
        browser_subscription("https://push.unknown.example/x"),
        r#"{"endpoint":"https://push.example.com/x","keys":{"p256dh":"AA","auth":"AA"}}"#
            .to_owned(),
        r#"{"endpoint":"https://push.example.com/x"}"#.to_owned(),
        "not json".to_owned(),
    ] {
        let response = call(bridge, "POST", PATH, &body).await;
        assert_eq!(response.status, 400, "{body}: {}", response.body);
    }
    let response = bridge
        .http("GET", PATH, &[("authorization", "Bearer wrong")])
        .await;
    assert_eq!(response.status, 401);
    let response = bridge.http("GET", "/v1/push/key", &[]).await;
    assert_eq!(response.status, 401);
    push.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn push_routes_are_absent_when_push_is_off() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    assert_eq!(bridge.get_authed("/v1/push/key").await.status, 404);
    assert_eq!(bridge.get_authed(PATH).await.status, 404);
    let body = browser_subscription("https://push.example.com/a");
    let response = call(&bridge, "POST", PATH, &body).await;
    assert_eq!(response.status, 404);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_blocked_agent_sends_one_encrypted_content_free_ping() {
    let herdr = CuttableHerdr::new();
    let pane = herdr
        .fake
        .add_agent("secret-agent-title", "claude", AgentStatus::Idle);
    let push = start(herdr).await;
    let bridge = &push.bridge;
    bridge.wait_for_agents("the agent", |a| a.len() == 1).await;
    let body = browser_subscription("https://push.example.com/phone");
    let response = call(bridge, "POST", PATH, &body).await;
    assert_eq!(response.status, 201, "{}", response.body);

    bridge.fake().set_status(&pane, AgentStatus::Working);
    bridge.fake().set_status(&pane, AgentStatus::Blocked);
    let sent = wait_for_pings(&push.recorder, 1).await;
    // Coalesced: give a second ping the chance to (wrongly) show up.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(push.recorder.sent().len(), 1, "{sent:?}");

    let request = &sent[0];
    assert_eq!(request.endpoint.as_str(), "https://push.example.com/phone");
    let headers = request.headers();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.clone())
            .expect(name)
    };
    assert_eq!(header("content-encoding"), "aes128gcm");
    assert!(header("authorization").starts_with("vapid t="));
    assert!(header("authorization").ends_with(&format!("k={}", push.public_key)));
    let shown = format!("{headers:?}");
    assert!(!shown.contains("secret-agent"), "{shown}");
    assert!(
        !request
            .body
            .windows(b"secret-agent".len())
            .any(|w| w == b"secret-agent"),
        "the body is encrypted and content-free"
    );
    // Header (86 bytes) + `{"ping":"<32 hex>"}` (43) + delimiter + tag.
    assert_eq!(request.body.len(), 86 + 43 + 1 + 16);
    push.bridge.stop().await.expect("clean stop");
}

/// Waits until the recorder has at least `count` requests.
async fn wait_for_pings(recorder: &Recorder, count: usize) -> Vec<PushRequest> {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let sent = recorder.sent();
            if sent.len() >= count {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pings in time")
}

#[tokio::test]
async fn a_restart_does_not_ping_for_an_agent_already_blocked() {
    // First run: the phone subscribes.
    let push = start(CuttableHerdr::new()).await;
    let body = browser_subscription("https://push.example.com/phone");
    let response = call(&push.bridge, "POST", PATH, &body).await;
    assert_eq!(response.status, 201, "{}", response.body);
    push.bridge.stop().await.expect("clean stop");

    // Restart with an agent that has been blocked all along.
    let herdr = CuttableHerdr::new();
    let pane = herdr
        .fake
        .add_agent("waiting", "claude", AgentStatus::Blocked);
    let push = start_in(herdr, push.dir).await;
    let bridge = &push.bridge;
    bridge
        .wait_for_agents("the blocked agent", |a| {
            a.len() == 1 && a[0].status == protocol::AgentStatus::Blocked
        })
        .await;
    // Well past the debounce (20 ms): a ping would have gone out by now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(push.recorder.sent().len(), 0, "already blocked: no ping");

    // A real transition still pings.
    bridge.fake().set_status(&pane, AgentStatus::Working);
    bridge.fake().set_status(&pane, AgentStatus::Blocked);
    let sent = wait_for_pings(&push.recorder, 1).await;
    assert_eq!(sent[0].endpoint.as_str(), "https://push.example.com/phone");
    push.bridge.stop().await.expect("clean stop");
}

/// The user-reported "all caught up" ping: a dialog answered before its
/// ping is due leaves nothing to tell, so nothing is sent.
#[tokio::test]
async fn a_dialog_answered_within_the_debounce_sends_nothing() {
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("asks", "claude", AgentStatus::Idle);
    let dir = tempfile::tempdir().expect("tmp");
    let vapid = VapidKey::generate(&dir.path().join("vapid.pem")).expect("key");
    let recorder = Arc::new(Recorder::default());
    let mut push = settings(vapid, dir.path(), Arc::clone(&recorder));
    // A debounce wide enough that the answer lands inside it under load.
    push.timing.quiet = Duration::from_secs(1);
    push.timing.max_delay = Duration::from_secs(2);
    let bridge = Bridge::start_with(herdr, |settings| settings.push = Some(push)).await;
    bridge.wait_for_agents("the agent", |a| a.len() == 1).await;
    let body = browser_subscription("https://push.example.com/phone");
    assert_eq!(call(&bridge, "POST", PATH, &body).await.status, 201);

    bridge.fake().set_status(&pane, AgentStatus::Working);
    working(&bridge, "asks").await;
    bridge.fake().set_status(&pane, AgentStatus::Blocked);
    bridge.fake().set_status(&pane, AgentStatus::Working);
    // Past the longest the ping could wait.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(recorder.sent().len(), 0, "answered: no ping");

    // A dialog still open when the ping is due is sent.
    bridge.fake().set_status(&pane, AgentStatus::Blocked);
    wait_for_pings(&recorder, 1).await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn swarm_workspace_agents_only_ping_when_blocked() {
    // FakeHerdr labels each `add_agent` workspace with the agent's name.
    let herdr = CuttableHerdr::new();
    let swarm = herdr.fake.add_agent("swarm", "claude", AgentStatus::Idle);
    let mine = herdr.fake.add_agent("mine", "claude", AgentStatus::Idle);
    let dir = tempfile::tempdir().expect("tmp");
    let vapid = VapidKey::generate(&dir.path().join("vapid.pem")).expect("key");
    let recorder = Arc::new(Recorder::default());
    let mut push = settings(vapid, dir.path(), Arc::clone(&recorder));
    push.quiet_done.workspaces = BTreeSet::from(["swarm".to_owned()]);
    let bridge = Bridge::start_with(herdr, |settings| settings.push = Some(push)).await;
    bridge
        .wait_for_agents("both agents", |a| a.len() == 2)
        .await;
    let body = browser_subscription("https://push.example.com/phone");
    assert_eq!(call(&bridge, "POST", PATH, &body).await.status, 201);

    bridge.fake().set_status(&swarm, AgentStatus::Working);
    working(&bridge, "swarm").await;
    bridge.fake().set_status(&swarm, AgentStatus::Done);
    // Well past the debounce (20 ms): a ping would have gone out by now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(recorder.sent().len(), 0, "a swarm agent finishing is quiet");

    bridge.fake().set_status(&mine, AgentStatus::Working);
    working(&bridge, "mine").await;
    bridge.fake().set_status(&mine, AgentStatus::Done);
    wait_for_pings(&recorder, 1).await;

    bridge.fake().set_status(&swarm, AgentStatus::Working);
    bridge.fake().set_status(&swarm, AgentStatus::Blocked);
    wait_for_pings(&recorder, 2).await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_swarm_agent_the_registry_could_not_label_is_looked_up_before_pinging() {
    let herdr = CuttableHerdr::new();
    herdr.fake.add_agent("mine", "claude", AgentStatus::Idle);
    let dir = tempfile::tempdir().expect("tmp");
    let vapid = VapidKey::generate(&dir.path().join("vapid.pem")).expect("key");
    let recorder = Arc::new(Recorder::default());
    let mut push = settings(vapid, dir.path(), Arc::clone(&recorder));
    push.quiet_done.workspaces = BTreeSet::from(["agent-swarm".to_owned()]);
    let bridge = Bridge::start_with(herdr, |settings| settings.push = Some(push)).await;
    bridge.wait_for_agents("my agent", |a| a.len() == 1).await;
    let body = browser_subscription("https://push.example.com/phone");
    assert_eq!(call(&bridge, "POST", PATH, &body).await.status, 201);

    // The orchestrator opens its workspace and a tab while the bridge runs,
    // and herdr fails the registry's one `workspace.list` for it: the
    // registry lists the agent, unlabeled, and won't list again before its
    // next full resync (an hour away here).
    let fake = bridge.fake();
    let workspace = fake
        .create_workspace("agent-swarm", None)
        .await
        .expect("workspace");
    let tab = fake
        .create_tab(&workspace.workspace.workspace_id, "t26f", None)
        .await
        .expect("tab");
    let pane = tab.root_pane.pane_id;
    fake.fail_next(
        Method::WorkspaceList,
        Fault::Reject(ErrorCode::InternalError),
    );
    fake.detect_agent(&pane, "claude", AgentStatus::Idle);
    bridge
        .wait_for_agents("the swarm agent", |a| a.len() == 2)
        .await;

    fake.set_status(&pane, AgentStatus::Working);
    working(&bridge, "claude").await;
    fake.set_status(&pane, AgentStatus::Done);
    // Well past the debounce (20 ms): a ping would have gone out by now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(recorder.sent().len(), 0, "its first finished turn is quiet");

    fake.set_status(&pane, AgentStatus::Working);
    fake.set_status(&pane, AgentStatus::Blocked);
    wait_for_pings(&recorder, 1).await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_list_flags_the_orchestrators_agents_quiet_when_done() {
    let herdr = CuttableHerdr::new();
    // FakeHerdr labels each `add_agent` workspace with the agent's name.
    herdr
        .fake
        .add_agent("impl-t26f", "claude", AgentStatus::Done);
    herdr.fake.add_agent("circus", "claude", AgentStatus::Done);
    let swarm = herdr
        .fake
        .create_workspace("agent-swarm", None)
        .await
        .expect("workspace");
    herdr
        .fake
        .detect_agent(&swarm.root_pane.pane_id, "claude", AgentStatus::Done);
    let bridge = Bridge::start_with(herdr, |settings| {
        settings.registry.quiet_done = QuietDone {
            workspaces: BTreeSet::from(["agent-swarm".to_owned()]),
            agent_prefixes: BTreeSet::from(["impl-".to_owned(), "rev-".to_owned()]),
        };
    })
    .await;
    let agents = bridge.wait_for_agents("all three", |a| a.len() == 3).await;
    let quiet = |title: &str| {
        agents
            .iter()
            .find(|a| a.title == title)
            .unwrap_or_else(|| panic!("{title}"))
            .quiet_done
    };
    assert!(quiet("impl-t26f"), "an orchestrator name");
    assert!(quiet("claude"), "the swarm's workspace");
    assert!(!quiet("circus"), "one of the user's agents");
    bridge.stop().await.expect("clean stop");
}

/// Waits until the bridge has seen the agent titled `title` at work, so a
/// following finish is a real `working` -> `done` transition.
async fn working(bridge: &Bridge, title: &str) {
    bridge
        .wait_for_agents(title, |agents| {
            agents
                .iter()
                .any(|a| a.title == title && a.status == protocol::AgentStatus::Working)
        })
        .await;
}

/// Authenticates `Bearer <device id>` as that device; lists `known` as its
/// devices.
#[derive(Debug)]
struct NamedDevices {
    known: Option<BTreeSet<String>>,
}

impl Authenticator for NamedDevices {
    fn authenticate<'a>(&'a self, request: AuthRequest<'a>) -> AuthFuture<'a> {
        let device = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(Principal::new);
        Box::pin(async move { device.ok_or(AuthRejection::Unauthenticated) })
    }

    fn known_devices(&self) -> Option<BTreeSet<String>> {
        self.known.clone()
    }
}

#[tokio::test]
async fn subscriptions_of_devices_gone_while_down_are_swept_at_startup() {
    const KEPT: &str = "dev-00000000000000aa";
    const GONE: &str = "dev-00000000000000bb";
    let dir = tempfile::tempdir().expect("tmp");
    VapidKey::generate(&dir.path().join("vapid.pem")).expect("key");
    let start = |known: Option<BTreeSet<String>>| {
        let vapid = VapidKey::load(&dir.path().join("vapid.pem")).expect("key");
        let push = settings(vapid, dir.path(), Arc::new(Recorder::default()));
        Bridge::start_full(
            CuttableHerdr::new(),
            Arc::new(NamedDevices { known }),
            |settings| settings.push = Some(push),
        )
    };

    // First run: an authenticator that cannot list its devices sweeps
    // nothing, and both devices subscribe.
    let bridge = start(None).await;
    for device in [KEPT, GONE] {
        let stream = TcpStream::connect(bridge.addr).await.expect("connect");
        let auth = format!("Bearer {device}");
        let body = browser_subscription(&format!("https://push.example.com/{device}"));
        let response = harness::request(
            stream,
            "POST",
            PATH,
            &[
                ("authorization", &auth),
                ("content-type", "application/json"),
            ],
            &body,
        )
        .await;
        assert_eq!(response.status, 201, "{}", response.body);
    }
    bridge.stop().await.expect("clean stop");
    let store = dir.path().join(STORE_FILE);
    assert_eq!(
        SubscriptionStore::open(&store).expect("store").devices(),
        [KEPT, GONE]
    );

    // GONE was revoked while the bridge was down.
    let bridge = start(Some(BTreeSet::from([KEPT.to_owned()]))).await;
    let stream = TcpStream::connect(bridge.addr).await.expect("connect");
    let auth = format!("Bearer {KEPT}");
    let listed = harness::http(stream, "GET", PATH, &[("authorization", &auth)]).await;
    assert_eq!(
        endpoints(&listed),
        [format!("https://push.example.com/{KEPT}")]
    );
    assert_eq!(
        SubscriptionStore::open(&store).expect("store").devices(),
        [KEPT],
        "swept before serving"
    );
    bridge.stop().await.expect("clean stop");
}
