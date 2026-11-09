//! `POST /v1/agents/:id/car-reply` end to end (design §6.6): Android
//! Auto's raw utterance mapped by the bridge to Allow once, Deny or a
//! prompt, under the car policy, with a sentence to read aloud.

use herdr::{AgentStatus, Call};
use protocol::{ActionResult, CarReply, RefusalReason};
use serde_json::json;

use crate::hooks::{Agent, Fixture, HOOK_PROMPT, SESSION_B, fixture, permission_request, send};

/// What the car hears back after allowing the fixture's dialog.
const ALLOWED: &str = "Allowed: cargo nextest run -p bridge";

impl Fixture {
    /// Posts a car reply with idempotency key number `key`.
    async fn car_reply(
        &self,
        key: u8,
        agent: &Agent,
        utterance: &str,
        pending_id: Option<&str>,
    ) -> (u16, CarReply) {
        let mut body = json!({
            "idempotencyKey": format!("00000000-0000-4000-8000-{key:012}"),
            "utterance": utterance,
        });
        if let Some(id) = pending_id {
            body["pendingId"] = id.into();
        }
        let response = self
            .bridge
            .post_authed(
                &format!("/v1/agents/{}/car-reply", agent.id),
                &body.to_string(),
            )
            .await;
        let reply = serde_json::from_str(&response.body)
            .unwrap_or_else(|e| panic!("{e}: {}", response.body));
        (response.status, reply)
    }

    /// Hooks session B's dialog; its pending id.
    async fn hooked_b(&self) -> String {
        send(&self.socket, &permission_request(SESSION_B)).await;
        let pending = self.wait_for_hook(&self.b, true).await;
        assert_eq!(pending.prompt, HOOK_PROMPT);
        pending.id.to_string()
    }

    /// The audit log's lines, parsed.
    fn audit(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.tmp.path().join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect()
    }

    /// What was typed into any pane.
    fn typed(&self) -> Vec<Call> {
        self.bridge
            .fake()
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Keys { .. } | Call::Prompt { .. }))
            .collect()
    }
}

fn car_policy(reply: &CarReply) -> bool {
    matches!(
        reply.result,
        ActionResult::Refused {
            reason: RefusalReason::CarPolicy,
            ..
        }
    )
}

#[tokio::test]
async fn yes_allows_the_hook_described_dialog_once_and_says_what() {
    let f = fixture().await;
    let id = f.hooked_b().await;

    let (status, reply) = f.car_reply(1, &f.b, "Yes, please.", Some(&id)).await;
    assert_eq!(status, 200, "{reply:?}");
    assert_eq!(reply.result, ActionResult::Sent { replayed: false });
    assert_eq!(reply.speech, ALLOWED);
    let allow_once = Call::Keys {
        pane: f.b.pane.clone(),
        keys: vec!["1".to_owned()],
    };
    assert_eq!(
        f.typed(),
        std::slice::from_ref(&allow_once),
        "Allow once, nothing else"
    );

    // A retry replays the reply and types nothing more.
    let (status, replayed) = f.car_reply(1, &f.b, "Yes, please.", Some(&id)).await;
    assert_eq!(status, 200);
    assert_eq!(replayed.result, ActionResult::Sent { replayed: true });
    assert_eq!(replayed.speech, ALLOWED);
    // The same key for something else is a conflict.
    let (status, conflict) = f.car_reply(1, &f.b, "no", Some(&id)).await;
    assert_eq!(status, 422, "{conflict:?}");
    assert!(matches!(
        conflict.result,
        ActionResult::Refused {
            reason: RefusalReason::IdempotencyConflict,
            ..
        }
    ));
    // The action it became used that key too: not again from the app.
    let body = json!({
        "idempotencyKey": "00000000-0000-4000-8000-000000000001",
        "agentId": f.b.id,
        "expectedSeq": f.seq(&f.b).await,
        "createdAt": 1_762_500_000_000_u64,
        "origin": "app",
        "kind": "choose",
        "payload": {"pendingId": id, "optionId": "yes"},
    });
    let response = f.bridge.post_authed("/v1/actions", &body.to_string()).await;
    assert_eq!(response.status, 422, "{}", response.body);
    assert_eq!(f.typed(), [allow_once]);
}

#[tokio::test]
async fn no_denies_the_hook_described_dialog() {
    let f = fixture().await;
    let id = f.hooked_b().await;
    let (status, reply) = f.car_reply(1, &f.b, "Um, no.", Some(&id)).await;
    assert_eq!(status, 200, "{reply:?}");
    assert_eq!(reply.speech, "Denied.");
    assert_eq!(
        f.typed(),
        [Call::Keys {
            pane: f.b.pane.clone(),
            keys: vec!["esc".to_owned()],
        }],
        "the deny option's key"
    );
}

#[tokio::test]
async fn screen_only_unnamed_stale_and_ambiguous_replies_send_nothing() {
    let f = fixture().await;
    let b = f.hooked_b().await;
    let a = f.pending(&f.a).await.id.to_string();

    // Only the screen parsed session A's dialog: never answered unseen.
    let (status, reply) = f.car_reply(1, &f.a, "yes", Some(&a)).await;
    assert_eq!(status, 403, "{reply:?}");
    assert!(car_policy(&reply));
    assert!(reply.speech.contains("on your phone"), "{}", reply.speech);

    // Allow must name the dialog the car read aloud...
    let (status, reply) = f.car_reply(2, &f.b, "yes", None).await;
    assert_eq!(status, 403);
    assert!(reply.speech.contains("which question"), "{}", reply.speech);
    // ...and it must still be the one on screen.
    let (status, reply) = f
        .car_reply(3, &f.b, "yes", Some("screen-1-000000000000"))
        .await;
    assert_eq!(status, 409, "{reply:?}");
    assert!(matches!(reply.result, ActionResult::Stale { .. }));
    assert_eq!(
        reply.speech,
        "The agent has moved on. Please check your phone."
    );
    let audited = f.audit();
    assert_eq!(audited.len(), 1, "the stale reply is audited: {audited:?}");
    assert_eq!(audited[0]["origin"], "car", "{audited:?}");

    // "Yes, but…" is not a yes.
    let (status, reply) = f
        .car_reply(4, &f.b, "yes but run the linter first", Some(&b))
        .await;
    assert_eq!(status, 403);
    assert!(car_policy(&reply));
    assert!(reply.speech.contains("Say just yes"), "{}", reply.speech);

    // Anything else would be typed into the dialog: refused.
    let (status, reply) = f.car_reply(5, &f.b, "carry on", Some(&b)).await;
    assert_eq!(status, 403);
    assert!(
        reply.speech.contains("waiting for an answer"),
        "{}",
        reply.speech
    );

    assert_eq!(f.typed(), [], "nothing was typed anywhere");
}

#[tokio::test]
async fn anything_else_is_a_prompt_when_no_dialog_is_open() {
    let f = fixture().await;
    f.bridge.fake().set_status(&f.a.pane, AgentStatus::Idle);
    f.bridge
        .wait_for_agents("agent a idle", |agents| {
            agents
                .iter()
                .any(|a| a.id.to_string() == f.a.id && a.status == protocol::AgentStatus::Idle)
        })
        .await;

    let (status, reply) = f.car_reply(1, &f.a, "/clear", None).await;
    assert_eq!(status, 403, "{reply:?}");
    assert!(reply.speech.contains("need your phone"), "{}", reply.speech);
    assert_eq!(f.typed(), [], "no commands from the car");

    let (status, reply) = f.car_reply(2, &f.a, "Carry on with the tests", None).await;
    assert_eq!(status, 200, "{reply:?}");
    assert_eq!(reply.speech, "Sent.");
    let typed = f.typed();
    assert_eq!(typed.len(), 1, "{typed:?}");
    assert!(
        matches!(&typed[0], Call::Prompt { text, .. } if text == "Carry on with the tests"),
        "{typed:?}"
    );

    // A yes with nothing open answers what the agent asked in its words.
    let (status, reply) = f.car_reply(3, &f.a, "Yes.", None).await;
    assert_eq!(status, 200, "{reply:?}");
    assert_eq!(reply.speech, "Sent.");
    assert!(
        matches!(f.typed().last(), Some(Call::Prompt { text, .. }) if text == "Yes."),
        "{:?}",
        f.typed()
    );
}

#[tokio::test]
async fn malformed_replies_are_refused_before_anything_happens() {
    let f = fixture().await;
    for body in [
        json!({"utterance": "yes"}),
        json!({"idempotencyKey": "00000000-0000-4000-8000-000000000001", "utterance": "a\nb"}),
        json!({"idempotencyKey": "00000000-0000-4000-8000-000000000001", "utterance": "yes",
               "origin": "app"}),
    ] {
        let response = f
            .bridge
            .post_authed(
                &format!("/v1/agents/{}/car-reply", f.b.id),
                &body.to_string(),
            )
            .await;
        assert_eq!(response.status, 400, "{body}: {}", response.body);
        let reply: CarReply = serde_json::from_str(&response.body).expect("a reply");
        assert_ne!(reply.speech, "", "a sentence to read aloud");
    }
    assert_eq!(f.typed(), []);
}
