//! Screen-parsed pending input: `GET /v1/agents/:id/pending`,
//! `pending_changed` events, and the `choose` action.

use std::time::Duration;

use bridge::pending::PendingSettings;
use herdr::{AgentStatus, Call, Herdr, PaneId, Target};
use protocol::{ActionResult, AgentEvent, PendingInput, PendingKind, RefusalReason, Risk};
use serde_json::{Value, json};

use crate::harness::{Bridge, CuttableHerdr, PATIENCE};

const BASH: &str = include_str!("../../src/pending/fixtures/synthetic_bash_permission.txt");
const AMEND: &str = include_str!("../../src/pending/fixtures/synthetic_bash_amend.txt");

const KEY1: &str = "00000000-0000-4000-8000-000000000001";
const KEY2: &str = "00000000-0000-4000-8000-000000000002";

struct Fixture {
    bridge: Bridge,
    pane: PaneId,
    id: String,
    _tmp: tempfile::TempDir,
}

/// An agent showing `screen`, at `status`.
async fn fixture(status: AgentStatus, screen: &str) -> Fixture {
    fixture_with(status, screen, |_| {}).await
}

/// [`fixture`], with the pending settings adjusted.
async fn fixture_with(
    status: AgentStatus,
    screen: &str,
    adjust: impl FnOnce(&mut PendingSettings),
) -> Fixture {
    let tmp = tempfile::tempdir().expect("tmp");
    let audit = tmp.path().join("audit.jsonl");
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("worker", "claude", AgentStatus::Idle);
    herdr.fake.set_screen(&pane, screen);
    let bridge = Bridge::start_with(herdr, move |settings| {
        settings.actions.audit_log = audit;
        adjust(&mut settings.pending);
    })
    .await;
    bridge.fake().set_status(&pane, status);
    let id = bridge
        .wait_for_agents("the agent", |a| {
            a.len() == 1 && a[0].status == status.into_protocol()
        })
        .await[0]
        .id
        .to_string();
    Fixture {
        bridge,
        pane,
        id,
        _tmp: tmp,
    }
}

trait IntoProtocol {
    fn into_protocol(self) -> protocol::AgentStatus;
}

impl IntoProtocol for AgentStatus {
    fn into_protocol(self) -> protocol::AgentStatus {
        match self {
            AgentStatus::Blocked => protocol::AgentStatus::Blocked,
            AgentStatus::Working => protocol::AgentStatus::Working,
            AgentStatus::Done => protocol::AgentStatus::Done,
            _ => protocol::AgentStatus::Idle,
        }
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

    async fn pending(&self) -> (u16, Option<PendingInput>) {
        let response = self
            .bridge
            .get_authed(&format!("/v1/agents/{}/pending", self.id))
            .await;
        let body = if response.status == 200 {
            serde_json::from_str(&response.body).expect("pending json")
        } else {
            None
        };
        (response.status, body)
    }

    async fn choose(
        &self,
        key: &str,
        pending_id: &str,
        option: &str,
        feedback: Option<&str>,
        origin: &str,
    ) -> (u16, ActionResult) {
        let mut payload = json!({"pendingId": pending_id, "optionId": option});
        if let Some(feedback) = feedback {
            payload["feedbackText"] = Value::from(feedback);
        }
        let body = json!({
            "idempotencyKey": key,
            "agentId": self.id,
            "expectedSeq": self.seq().await,
            "createdAt": 1_762_500_000_000_u64,
            "origin": origin,
            "kind": "choose",
            "payload": payload,
        });
        let response = self
            .bridge
            .post_authed("/v1/actions", &body.to_string())
            .await;
        let result = serde_json::from_str(&response.body)
            .unwrap_or_else(|e| panic!("{e}: {}", response.body));
        (response.status, result)
    }

    /// The pending id `GET …/pending` reports now.
    async fn current_id(&self) -> String {
        self.pending()
            .await
            .1
            .expect("a pending input")
            .id
            .to_string()
    }

    /// Whether `id` is `screen-<current seq>-<12 hex>`.
    async fn is_current_screen_id(&self, id: &str) -> bool {
        let prefix = format!("screen-{}-", self.seq().await);
        id.strip_prefix(&prefix)
            .is_some_and(|hex| hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()))
    }

    async fn keys_with(&self, key: &str, pending_id: &str, keys: &[&str]) -> (u16, ActionResult) {
        let body = json!({
            "idempotencyKey": key,
            "agentId": self.id,
            "expectedSeq": self.seq().await,
            "createdAt": 1_762_500_000_000_u64,
            "origin": "app",
            "kind": "keys",
            "payload": {"pendingId": pending_id, "keys": keys},
        });
        let response = self
            .bridge
            .post_authed("/v1/actions", &body.to_string())
            .await;
        let result = serde_json::from_str(&response.body)
            .unwrap_or_else(|e| panic!("{e}: {}", response.body));
        (response.status, result)
    }

    fn sent(&self) -> Vec<Call> {
        self.bridge
            .fake()
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Prompt { .. } | Call::Keys { .. }))
            .collect()
    }

    fn keys(&self, keys: &[&str]) -> Call {
        Call::Keys {
            pane: self.pane.clone(),
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
        }
    }
}

fn option_ids(pending: &PendingInput) -> Vec<String> {
    pending
        .options
        .as_slice()
        .iter()
        .map(|o| o.id.to_string())
        .collect()
}

fn refusal(result: &ActionResult) -> Option<RefusalReason> {
    match result {
        ActionResult::Refused { reason, .. } => Some(*reason),
        _ => None,
    }
}

const SENT: ActionResult = ActionResult::Sent { replayed: false };

#[tokio::test]
async fn pending_is_null_unless_blocked_and_parsed_when_blocked() {
    let f = fixture(AgentStatus::Idle, BASH).await;
    assert_eq!(f.pending().await, (200, None));

    f.bridge.fake().set_status(&f.pane, AgentStatus::Blocked);
    let (status, pending) = f.pending().await;
    assert_eq!(status, 200);
    let pending = pending.expect("pending input");
    assert!(
        f.is_current_screen_id(pending.id.as_str()).await,
        "{}",
        pending.id
    );
    assert_eq!(pending.kind, PendingKind::Permission);
    assert_eq!(pending.prompt, "Run `cargo nextest run -p bridge`?");
    assert_eq!(option_ids(&pending), ["yes", "yes_always", "no_feedback"]);
    let risks: Vec<Risk> = pending.options.as_slice().iter().map(|o| o.risk).collect();
    assert_eq!(risks, [Risk::Low, Risk::Elevated, Risk::Low]);

    let gone = f.bridge.get_authed("/v1/agents/w99:p9/pending").await;
    assert_eq!(gone.status, 404);
    let bad = f.bridge.get_authed("/v1/agents/w1%2Fp1/pending").await;
    assert_eq!(bad.status, 400);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn an_unparseable_screen_is_a_dialog_without_options() {
    let f = fixture(AgentStatus::Blocked, "something odd\nwithout a menu").await;
    let pending = f.pending().await.1.expect("pending input");
    assert_eq!(pending.kind, PendingKind::Unknown);
    assert_eq!(option_ids(&pending), Vec::<String>::new());
    assert!(pending.is_dialog());
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_low_risk_option_sends_its_key() {
    let f = fixture(AgentStatus::Blocked, BASH).await;
    let id = f.current_id().await;
    assert_eq!(f.choose(KEY1, &id, "yes", None, "app").await, (200, SENT));
    assert_eq!(f.sent(), [f.keys(&["1"])]);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn elevated_options_and_the_car_are_refused() {
    let f = fixture(AgentStatus::Blocked, BASH).await;
    let id = f.current_id().await;
    let (status, result) = f.choose(KEY1, &id, "yes_always", None, "app").await;
    assert_eq!(
        (status, refusal(&result)),
        (403, Some(RefusalReason::StepUpRequired))
    );
    let (status, result) = f.choose(KEY2, &id, "yes", None, "car").await;
    assert_eq!(
        (status, refusal(&result)),
        (403, Some(RefusalReason::CarPolicy))
    );
    let key3 = "00000000-0000-4000-8000-000000000003";
    let (status, result) = f.choose(key3, &id, "yes", Some("extra"), "app").await;
    assert_eq!(
        (status, refusal(&result)),
        (422, Some(RefusalReason::FeedbackNotAccepted))
    );
    assert_eq!(f.sent(), []);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn choose_is_stale_unless_it_matches_the_screen_now() {
    let f = fixture(AgentStatus::Blocked, BASH).await;
    let id = f.current_id().await;

    let (status, _) = f.choose(KEY1, "screen-1", "yes", None, "app").await;
    assert_eq!(status, 409, "another pending id");

    // Another dialog at the same seq (herdr did not bump it). Both offer
    // `yes`, but a "Yes" meant for the first must not approve the second.
    let seq = f.seq().await;
    f.bridge.fake().set_screen(&f.pane, AMEND);
    assert_eq!(f.seq().await, seq, "same seq");
    let (status, result) = f.choose(KEY2, &id, "yes", None, "app").await;
    assert_eq!(status, 409);
    let ActionResult::Stale { pending, .. } = result else {
        panic!("stale expected, got {result:?}");
    };
    let pending = pending.expect("current pending");
    assert_ne!(pending.id.to_string(), id);
    assert_eq!(option_ids(&pending), ["yes", "yes_always", "no"]);
    assert_eq!(f.sent(), []);

    let key3 = "00000000-0000-4000-8000-000000000003";
    let new_id = pending.id.to_string();
    assert_eq!(f.current_id().await, new_id);
    assert_eq!(
        f.choose(key3, &new_id, "no", None, "app").await,
        (200, SENT)
    );
    assert_eq!(f.sent(), [f.keys(&["3"])]);
    f.bridge.stop().await.expect("clean stop");
}

/// A real screen keeps moving while its dialog waits (a spinner's timer,
/// a status-line clock): every read differs, but it is the same dialog,
/// so its id holds and a choice made several reads ago still lands.
/// Option chips vanished after backgrounding the app when this was not so.
#[tokio::test]
async fn a_ticking_screen_keeps_its_dialog_id() {
    let ticking = BASH.replace("Running…", &format!("Running… ({}s)", herdr::TICK));
    assert_ne!(ticking, BASH, "the fixture has a spinner line to tick");
    let f = fixture(AgentStatus::Blocked, &ticking).await;
    let id = f.current_id().await;
    for _ in 0..3 {
        let screen = f
            .bridge
            .fake()
            .read_agent(
                &Target::Pane(f.pane.clone()),
                herdr::ReadSource::Visible,
                None,
            )
            .await
            .expect("read");
        assert!(!screen.text.contains(herdr::TICK), "the clock ticks");
        assert_eq!(f.current_id().await, id, "the same dialog after a tick");
    }
    assert_eq!(f.choose(KEY1, &id, "yes", None, "app").await, (200, SENT));
    assert_eq!(f.sent(), [f.keys(&["1"])]);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn feedback_is_sent_once_claude_is_back_at_its_prompt() {
    let f = fixture(AgentStatus::Blocked, BASH).await;
    let id = f.current_id().await;
    // Play Claude: once Esc arrives, leave the dialog for the prompt.
    let herdr = std::sync::Arc::clone(&f.bridge.herdr);
    let pane = f.pane.clone();
    let claude = tokio::spawn(async move {
        loop {
            if herdr
                .fake
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Keys { keys, .. } if keys == &["esc"]))
            {
                herdr.fake.set_status(&pane, AgentStatus::Idle);
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let result = f
        .choose(KEY1, &id, "no_feedback", Some("use pnpm instead"), "app")
        .await;
    assert_eq!(result, (200, SENT));
    tokio::time::timeout(PATIENCE, claude)
        .await
        .expect("in time")
        .expect("task");
    assert_eq!(
        f.sent(),
        [
            f.keys(&["esc"]),
            Call::Prompt {
                pane: f.pane.clone(),
                text: "use pnpm instead".into()
            }
        ]
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn feedback_is_not_guessed_when_claude_stays_in_the_dialog() {
    // Claude never leaves the dialog, so the feedback wait runs out: keep
    // it short.
    let f = fixture_with(AgentStatus::Blocked, BASH, |pending| {
        pending.feedback_wait = Duration::from_millis(300);
    })
    .await;
    let id = f.current_id().await;
    let (status, result) = f
        .choose(KEY1, &id, "no_feedback", Some("use pnpm instead"), "app")
        .await;
    assert_eq!(status, 200);
    assert!(matches!(result, ActionResult::Failed { .. }), "{result:?}");
    assert_eq!(f.sent(), [f.keys(&["esc"])], "the feedback was not typed");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn pending_changes_are_streamed() {
    let f = fixture(AgentStatus::Idle, BASH).await;
    let mut stream = f.bridge.stream().await;
    assert!(matches!(
        stream.next_event().await,
        AgentEvent::Snapshot { .. }
    ));

    let pending_changed = |event: &AgentEvent| matches!(event, AgentEvent::PendingChanged { .. });
    f.bridge.fake().set_status(&f.pane, AgentStatus::Blocked);
    let AgentEvent::PendingChanged { agent_id, pending } =
        stream.wait_for("the dialog", pending_changed).await
    else {
        unreachable!();
    };
    assert_eq!(agent_id.to_string(), f.id);
    let pending = pending.expect("a pending input");
    assert_eq!(option_ids(&pending), ["yes", "yes_always", "no_feedback"]);
    assert!(f.is_current_screen_id(pending.id.as_str()).await);

    // A new subscriber gets it right after the snapshot.
    let mut late = f.bridge.stream().await;
    assert!(matches!(
        late.next_event().await,
        AgentEvent::Snapshot { .. }
    ));
    assert!(
        matches!(late.next_event().await, AgentEvent::PendingChanged { pending: Some(p), .. } if p == pending)
    );

    // The dialog changes while blocked: picked up by the poller.
    f.bridge.fake().set_screen(&f.pane, AMEND);
    let AgentEvent::PendingChanged { pending, .. } =
        stream.wait_for("the redrawn dialog", pending_changed).await
    else {
        unreachable!();
    };
    assert_eq!(
        option_ids(&pending.expect("a pending input")),
        ["yes", "yes_always", "no"]
    );

    // Answered: the agent works again and the input clears.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    let event = stream.wait_for("the clear", pending_changed).await;
    assert!(
        matches!(event, AgentEvent::PendingChanged { pending: None, .. }),
        "{event:?}"
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn keys_with_a_pending_id_must_match_the_dialog_on_screen() {
    let f = fixture(AgentStatus::Blocked, BASH).await;
    let id = f.current_id().await;
    f.bridge.fake().set_screen(&f.pane, AMEND);
    let (status, result) = f.keys_with(KEY1, &id, &["1"]).await;
    assert_eq!(status, 409, "{result:?}");
    assert_eq!(f.sent(), []);

    let new_id = f.current_id().await;
    assert_eq!(f.keys_with(KEY2, &new_id, &["1"]).await, (200, SENT));
    assert_eq!(f.sent(), [f.keys(&["1"])]);
    f.bridge.stop().await.expect("clean stop");
}

/// The `agent_updated` for agent `id` whose ask is `ask`.
fn updated_with_ask(event: &AgentEvent, id: &str, ask: Option<&str>) -> bool {
    matches!(
        event,
        AgentEvent::AgentUpdated { agent: Some(agent) }
            if agent.id.to_string() == id && agent.ask.as_deref() == ask
    )
}

#[tokio::test]
async fn a_blocked_agents_card_says_what_it_asks_until_it_is_answered() {
    const ASK: &str = "Run `cargo nextest run -p bridge`?";
    let f = fixture(AgentStatus::Idle, BASH).await;
    assert_eq!(f.bridge.agents().await[0].ask, None, "not blocked");
    let mut stream = f.bridge.stream().await;
    stream
        .wait_for("the snapshot", |e| matches!(e, AgentEvent::Snapshot { .. }))
        .await;

    f.bridge.fake().set_status(&f.pane, AgentStatus::Blocked);
    f.bridge
        .wait_for_agents("the ask on the card", |a| a[0].ask.as_deref() == Some(ASK))
        .await;
    stream
        .wait_for("the agent re-sent with its ask", |e| {
            updated_with_ask(e, &f.id, Some(ASK))
        })
        .await;

    // A client connecting now gets the ask in its snapshot.
    let mut late = f.bridge.stream().await;
    let snapshot = late.next_event().await;
    let AgentEvent::Snapshot { agents } = snapshot else {
        panic!("snapshot expected, got {snapshot:?}");
    };
    assert_eq!(agents[0].ask.as_deref(), Some(ASK));

    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    f.bridge
        .wait_for_agents("the ask cleared", |a| {
            a[0].status == protocol::AgentStatus::Working && a[0].ask.is_none()
        })
        .await;
    stream
        .wait_for("the working agent, without an ask", |e| {
            updated_with_ask(e, &f.id, None)
        })
        .await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_blocked_agent_without_a_parsed_prompt_has_no_ask() {
    let f = fixture(AgentStatus::Blocked, "something odd\nwithout a menu").await;
    assert!(f.pending().await.1.is_some(), "the screen was read");
    let agents = f.bridge.agents().await;
    assert_eq!(agents[0].status, protocol::AgentStatus::Blocked);
    assert_eq!(agents[0].ask, None);
    f.bridge.stop().await.expect("clean stop");
}
