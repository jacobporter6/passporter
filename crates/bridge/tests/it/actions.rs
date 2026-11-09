//! `POST /v1/actions`: verbs, stale guard, idempotency, policy and audit.

use std::path::{Path, PathBuf};

use bridge::actions::ActionSettings;
use bridge::auth::RateLimit;
use herdr::{AgentSession, AgentStatus, Call, ErrorCode, Fault, Herdr, Method, PaneId, Target};
use protocol::{ActionResult, RefusalReason, Seq};
use serde_json::{Value, json};

use crate::harness::{Bridge, CuttableHerdr, HttpResponse};

const KEY1: &str = "00000000-0000-4000-8000-000000000001";
const KEY2: &str = "00000000-0000-4000-8000-000000000002";
const KEY3: &str = "00000000-0000-4000-8000-000000000003";
const KEY4: &str = "00000000-0000-4000-8000-000000000004";
const KEY5: &str = "00000000-0000-4000-8000-000000000005";

struct Fixture {
    bridge: Bridge,
    pane: PaneId,
    id: String,
    audit: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn fixture(status: AgentStatus) -> Fixture {
    fixture_with_audit(status, |tmp| tmp.join("state/audit.jsonl")).await
}

async fn fixture_with_audit(status: AgentStatus, audit: impl FnOnce(&Path) -> PathBuf) -> Fixture {
    fixture_with(status, audit, |_| {}).await
}

async fn fixture_with(
    status: AgentStatus,
    audit: impl FnOnce(&Path) -> PathBuf,
    adjust: impl FnOnce(&mut ActionSettings),
) -> Fixture {
    let tmp = tempfile::tempdir().expect("tmp");
    let audit = audit(tmp.path());
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("worker", "claude", AgentStatus::Idle);
    let audit_path = audit.clone();
    let bridge = Bridge::start_with(herdr, move |settings| {
        settings.actions.audit_log = audit_path;
        adjust(&mut settings.actions);
    })
    .await;
    bridge.fake().set_status(&pane, status);
    let id = bridge.wait_for_agents("the agent", |a| a.len() == 1).await[0]
        .id
        .to_string();
    Fixture {
        bridge,
        pane,
        id,
        audit,
        _tmp: tmp,
    }
}

impl Fixture {
    async fn seq(&self) -> u64 {
        self.bridge
            .fake()
            .agent(&Target::from(&self.pane))
            .await
            .expect("agent")
            .state_change_seq
    }

    fn body(&self, key: &str, seq: u64, kind: &Value, origin: &str) -> String {
        let mut body = json!({
            "idempotencyKey": key,
            "agentId": self.id,
            "expectedSeq": seq,
            "createdAt": 1_762_500_000_000_u64,
            "origin": origin,
        });
        let fields = body.as_object_mut().expect("object");
        for (k, v) in kind.as_object().expect("object") {
            fields.insert(k.clone(), v.clone());
        }
        body.to_string()
    }

    async fn post(&self, body: &str) -> (u16, ActionResult) {
        let response = self.post_raw(body).await;
        let result = serde_json::from_str(&response.body)
            .unwrap_or_else(|e| panic!("{e}: {}", response.body));
        (response.status, result)
    }

    async fn post_raw(&self, body: &str) -> HttpResponse {
        self.bridge.post_authed("/v1/actions", body).await
    }

    async fn act(&self, key: &str, kind: &Value, origin: &str) -> (u16, ActionResult) {
        let body = self.body(key, self.seq().await, kind, origin);
        self.post(&body).await
    }

    fn sent(&self) -> Vec<Call> {
        self.bridge
            .fake()
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Prompt { .. } | Call::Keys { .. }))
            .collect()
    }

    fn audit_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.audit)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("json line"))
            .collect()
    }
}

fn prompt(text: &str) -> Value {
    json!({"kind": "prompt", "payload": {"text": text}})
}

fn keys(keys: &[&str]) -> Value {
    json!({"kind": "keys", "payload": {"keys": keys}})
}

fn refusal(result: &ActionResult) -> Option<RefusalReason> {
    match result {
        ActionResult::Refused { reason, .. } => Some(*reason),
        _ => None,
    }
}

const SENT: ActionResult = ActionResult::Sent { replayed: false };

#[tokio::test]
async fn a_prompt_is_sent_audited_and_replayed_idempotently() {
    let f = fixture(AgentStatus::Idle).await;
    let body = f.body(KEY1, f.seq().await, &prompt("run the tests"), "app");
    assert_eq!(f.post(&body).await, (200, SENT));
    assert_eq!(
        f.sent(),
        [Call::Prompt {
            pane: f.pane.clone(),
            text: "run the tests".into()
        }]
    );

    // Same key, same body: the stored result, nothing sent again.
    assert_eq!(
        f.post(&body).await,
        (200, ActionResult::Sent { replayed: true })
    );
    assert_eq!(f.sent().len(), 1);

    // Same key, different body.
    let other = f.body(KEY1, f.seq().await, &prompt("something else"), "app");
    let (status, result) = f.post(&other).await;
    assert_eq!(status, 422);
    assert_eq!(refusal(&result), Some(RefusalReason::IdempotencyConflict));
    assert_eq!(f.sent().len(), 1);

    let audit = f.audit_lines();
    assert_eq!(audit.len(), 2, "{audit:?}");
    assert_eq!(audit[0]["phase"], "execute");
    assert_eq!(audit[0]["kind"], "prompt");
    assert_eq!(audit[0]["detail"], "run the tests");
    assert_eq!(audit[0]["device"], "test-device");
    assert_eq!(audit[1]["phase"], "result");
    assert_eq!(audit[1]["outcome"], "sent");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn keys_and_interrupt_map_to_herdr_keys() {
    let f = fixture(AgentStatus::Blocked).await;
    assert_eq!(
        f.act(KEY1, &keys(&["2", "enter"]), "app").await,
        (200, SENT)
    );
    let interrupt = json!({"kind": "interrupt", "payload": {}});
    assert_eq!(f.act(KEY2, &interrupt, "app").await, (200, SENT));
    let key_calls = |names: &[&str]| Call::Keys {
        pane: f.pane.clone(),
        keys: names.iter().map(|k| (*k).to_owned()).collect(),
    };
    assert_eq!(
        f.sent(),
        [key_calls(&["2", "enter"]), key_calls(&["ctrl+c"])]
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn keys_need_the_app_and_choose_needs_a_parsed_option() {
    let f = fixture(AgentStatus::Blocked).await;
    let (status, result) = f.act(KEY1, &keys(&["1"]), "notification").await;
    assert_eq!(
        (status, refusal(&result)),
        (403, Some(RefusalReason::StepUpRequired))
    );
    let (status, result) = f.act(KEY1, &keys(&["1"]), "car").await;
    assert_eq!(
        (status, refusal(&result)),
        (403, Some(RefusalReason::CarPolicy))
    );

    let choose = json!({"kind": "choose",
        "payload": {"pendingId": format!("screen-{}", f.seq().await), "optionId": "allow_once"}});
    // The screen shows no dialog, so there is no option to choose: the
    // client's view is stale, and it gets the current (option-less) input.
    let (status, result) = f.act(KEY2, &choose, "app").await;
    assert_eq!(status, 409);
    let ActionResult::Stale { pending, .. } = result else {
        panic!("stale expected, got {result:?}");
    };
    assert_eq!(pending.expect("pending").options.as_slice(), []);

    // A prompt would be typed into the dialog.
    let key3 = "00000000-0000-4000-8000-000000000003";
    let (status, result) = f.act(key3, &prompt("yes"), "app").await;
    assert_eq!(
        (status, refusal(&result)),
        (403, Some(RefusalReason::StepUpRequired))
    );
    assert_eq!(f.sent(), []);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn stale_actions_get_409_and_send_nothing() {
    let f = fixture(AgentStatus::Idle).await;
    let seq = f.seq().await;
    let old = f.body(KEY1, seq - 1, &prompt("go"), "app");
    let (status, result) = f.post(&old).await;
    assert_eq!(status, 409);
    let ActionResult::Stale { current, pending } = result else {
        panic!("stale expected, got {result:?}");
    };
    assert_eq!(current.expect("current state").seq, Seq(seq));
    assert_eq!(pending, None);

    // The pane now runs another session.
    f.bridge.fake().set_session(
        &f.pane,
        AgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            kind: "id".into(),
            value: "583dc90c-15e5-4f5c-aeea-2d4701cb9d28".into(),
        },
    );
    let (status, _) = f.act(KEY2, &prompt("go"), "app").await;
    assert_eq!(status, 409);

    let gone = f
        .body(KEY1, seq, &prompt("go"), "app")
        .replace(&f.id, "w99:p9")
        .replace(KEY1, "00000000-0000-4000-8000-000000000003");
    let (status, result) = f.post(&gone).await;
    assert_eq!(
        (status, refusal(&result)),
        (404, Some(RefusalReason::UnknownAgent))
    );
    assert_eq!(f.sent(), []);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn invalid_bodies_are_refused() {
    let f = fixture(AgentStatus::Idle).await;
    for body in [
        "not json".to_owned(),
        f.body(KEY1, 1, &keys(&["ctrl+z"]), "app"),
        f.body("retry-1", 1, &prompt("go"), "app"),
    ] {
        let (status, result) = f.post(&body).await;
        assert_eq!(
            (status, refusal(&result)),
            (400, Some(RefusalReason::InvalidAction))
        );
    }
    let unauthenticated = f
        .bridge
        .http("POST", "/v1/actions", &[("content-length", "0")])
        .await;
    assert_eq!(unauthenticated.status, 401);
    assert_eq!(f.sent(), []);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn herdr_failures_are_final_for_the_key() {
    let f = fixture(AgentStatus::Blocked).await;
    f.bridge.fake().fail_next(
        Method::AgentSendKeys,
        Fault::AfterEffect(ErrorCode::Timeout),
    );
    let body = f.body(KEY1, f.seq().await, &keys(&["1"]), "app");
    let (status, result) = f.post(&body).await;
    assert_eq!(status, 200);
    assert!(matches!(result, ActionResult::Failed { .. }), "{result:?}");
    // Delivery is uncertain: a retry replays the failure, never resends.
    assert_eq!(f.post(&body).await, (200, result));
    assert_eq!(f.sent().len(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn nothing_executes_without_an_audit_entry() {
    // The audit log path is a directory, so it cannot be opened.
    let f = fixture_with_audit(AgentStatus::Idle, |tmp| {
        let dir = tmp.join("audit.jsonl");
        std::fs::create_dir(&dir).expect("dir");
        dir
    })
    .await;
    let body = f.body(KEY1, f.seq().await, &prompt("go"), "app");
    let (status, result) = f.post(&body).await;
    assert_eq!(status, 503);
    assert!(refusal(&result).is_some(), "{result:?}");
    assert_eq!(f.sent(), [], "executed without an audit entry");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn each_device_is_rate_limited_but_replays_and_interrupts_are_not() {
    let f = fixture_with(
        AgentStatus::Idle,
        |tmp| tmp.join("state/audit.jsonl"),
        |actions| actions.rate = RateLimit::per_minute(2),
    )
    .await;
    let first = f.body(KEY1, f.seq().await, &prompt("one"), "app");
    assert_eq!(f.post(&first).await, (200, SENT));
    assert_eq!(f.act(KEY2, &prompt("two"), "app").await, (200, SENT));

    let third = f.body(KEY3, f.seq().await, &prompt("three"), "app");
    let response = f.post_raw(&third).await;
    assert_eq!(response.status, 429, "{}", response.body);
    let retry: u64 = response
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .expect("seconds");
    assert!((1..=60).contains(&retry), "{retry}");
    let result: ActionResult = serde_json::from_str(&response.body).expect("result");
    assert_eq!(refusal(&result), Some(RefusalReason::RateLimited));
    assert_eq!(f.sent().len(), 2, "the limited action was sent");
    assert_eq!(f.audit_lines().len(), 4, "the limited action was audited");

    // A replay costs nothing and still gets its stored result.
    assert_eq!(
        f.post(&first).await,
        (200, ActionResult::Sent { replayed: true })
    );
    // Stop always works, however busy the device has been.
    let interrupt = json!({"kind": "interrupt", "payload": {}});
    assert_eq!(f.act(KEY4, &interrupt, "app").await, (200, SENT));
    assert_eq!(f.sent().len(), 3);
    // The car may never interrupt, so its interrupts are limited like the rest.
    let (status, result) = f.act(KEY5, &interrupt, "car").await;
    assert_eq!(status, 429, "{result:?}");
    assert_eq!(refusal(&result), Some(RefusalReason::RateLimited));
    // The limited key was released, not recorded: still fresh, still limited.
    let (status, result) = f.post(&third).await;
    assert_eq!(status, 429);
    assert_eq!(refusal(&result), Some(RefusalReason::RateLimited));
    f.bridge.stop().await.expect("clean stop");
}
