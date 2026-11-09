//! Codex agents: threads and TL;DRs from rollouts under a temp sessions
//! dir (the real codex-cli 0.166 fixtures, cwd rewritten), and approval
//! chips from the real approval screen.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bridge::tldr::SharedSummarizer;
use herdr::{AgentSession, AgentStatus, PaneId};
use protocol::{
    AgentEvent, PendingInput, PendingKind, Risk, ThreadContent, ThreadPage, Tldr, TurnIndex,
};
use serde_json::json;
use summarize::FakeSummarizer;

use crate::agents::start_agent;
use crate::harness::{Bridge, CuttableHerdr};
use crate::tldr::{append, live_stream, summary};

/// The session id of `exec_tool_call.jsonl`.
const EXEC_SESSION: &str = "01a1bb68-3fa5-78d3-9809-5887b47e4fcc";
/// The working directory the fixtures were captured in.
const PROBE_CWD: &str = "/work/codex-probe";

const APPROVAL: &str = include_str!("../../src/pending/fixtures/codex_0_166_command_approval.txt");

/// When the fixture sessions started (their `session_meta` timestamps).
const PROBE_STARTS: [&str; 2] = ["2026-11-08T13:45:33.866Z", "2026-11-08T13:53:14.157Z"];

/// A fixture rollout with its cwd moved to `cwd` and its start moved to
/// `started` (RFC 3339).
fn rollout_at(name: &str, cwd: &Path, started: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../transcripts/tests/fixtures/codex")
        .join(name);
    let mut text = fs::read_to_string(path)
        .expect("fixture")
        .replace(PROBE_CWD, &cwd.to_string_lossy());
    for start in PROBE_STARTS {
        text = text.replace(start, started);
    }
    text
}

/// A fixture rollout in `cwd`, started now: after the agent arrived.
fn rollout(name: &str, cwd: &Path) -> String {
    rollout_at(name, cwd, &now_rfc3339())
}

/// The current UTC time as RFC 3339 (`2026-11-08T13:45:33.000Z`).
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    let days = i64::try_from(secs / 86_400).expect("days");
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// A rollout record of an assistant message.
fn assistant(text: &str) -> String {
    let record = json!({
        "timestamp": "2026-11-08T13:46:00.000Z",
        "type": "response_item",
        "payload": {"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]},
    });
    format!("{record}\n")
}

/// A Codex agent in a temp cwd, its rollout in a temp sessions dir.
struct Fixture {
    bridge: Bridge,
    pane: PaneId,
    cwd: PathBuf,
    day: PathBuf,
    /// The start time in the rollout's file name (`2026-11-08T13-45-33`).
    stamp: String,
    rollout: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    /// `text` as the agent's rollout, with herdr reporting `session` (or
    /// none) for it.
    async fn start(
        text: impl FnOnce(&Path) -> String,
        session: Option<&str>,
        fake: Option<Arc<FakeSummarizer>>,
    ) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).expect("cwd");
        let sessions = tmp.path().join("sessions");
        // Named for now, as Codex names a session it starts: the locator
        // stops at file names a day older than the agent's arrival.
        let now = now_rfc3339();
        let day = sessions.join(now[..10].replace('-', "/"));
        fs::create_dir_all(&day).expect("day dir");
        let stamp = now[..19].replace(':', "-");
        let rollout = day.join(format!("rollout-{stamp}-{EXEC_SESSION}.jsonl"));
        append(&rollout, &text(&cwd));

        let bridge = Bridge::start_with(CuttableHerdr::new(), |settings| {
            settings.threads.codex_sessions_dir = Some(sessions);
            settings.tldr.summarizer = fake.map(|fake| SharedSummarizer::new(fake));
        })
        .await;
        // Another agent first: once the registry shows it, the bridge is
        // subscribed, so it sees the Codex agent's detection event (its
        // arrival, see `Hub::note_arrival`).
        start_agent(&bridge.herdr, "other", "pi", tmp.path()).await;
        bridge
            .wait_for_agents("the first agent", |a| a.len() == 1)
            .await;
        let pane = start_agent(&bridge.herdr, "worker", "codex", &cwd).await;
        if let Some(session) = session {
            bridge.fake().set_session(
                &pane,
                AgentSession {
                    source: "herdr:codex".into(),
                    agent: "codex".into(),
                    kind: "id".into(),
                    value: session.into(),
                },
            );
        }
        bridge.fake().set_status(&pane, AgentStatus::Working);
        bridge
            .wait_for_agents("the working agent", |a| {
                a.iter().any(|a| {
                    a.status == protocol::AgentStatus::Working
                        && (session.is_none() || a.id.session().is_some())
                })
            })
            .await;
        Self {
            bridge,
            pane,
            cwd,
            day,
            stamp,
            rollout,
            _tmp: tmp,
        }
    }

    async fn id(&self) -> String {
        let codex = |a: &protocol::Agent| a.kind == protocol::AgentKind::Codex;
        let agents = self
            .bridge
            .wait_for_agents("the Codex agent", |a| a.iter().any(codex))
            .await;
        agents
            .iter()
            .find(|a| codex(a))
            .expect("codex agent")
            .id
            .to_string()
    }

    async fn page(&self) -> ThreadPage {
        let response = self
            .bridge
            .get_authed(&format!("/v1/agents/{}/thread", self.id().await))
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        serde_json::from_str(&response.body).expect("thread page")
    }
}

fn kinds(page: &ThreadPage) -> Vec<String> {
    page.items
        .iter()
        .map(|item| match &item.content {
            ThreadContent::User { .. } => "user".to_owned(),
            ThreadContent::Assistant { .. } => "assistant".to_owned(),
            ThreadContent::Tool { name, input, .. } => format!("{name}: {input}"),
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_codex_thread_is_found_by_cwd_and_followed() {
    let f = Fixture::start(|cwd| rollout("exec_tool_call.jsonl", cwd), None, None).await;
    // A session in the same repo that started before this agent arrived
    // (another pane's, or an old one) is not its thread, even though its
    // file is the newest.
    // Same start in its name, and a session id that sorts after the
    // agent's: listed first.
    let stale = f.day.join(format!(
        "rollout-{}-{}.jsonl",
        f.stamp, "01a1bb6f-45a9-7332-adc0-fd8600af2b96"
    ));
    append(
        &stale,
        &rollout_at(
            "interactive_declined_approval.jsonl",
            &f.cwd,
            "2020-01-01T00:00:00.000Z",
        ),
    );
    let first = f.page().await;
    assert_eq!(
        kinds(&first),
        [
            "user",
            "assistant",
            "Bash: printf hi > hello.txt\nls -la",
            "assistant"
        ]
    );
    assert!(first.inferred, "matched by folder");
    let ThreadContent::User { text } = &first.items[0].content else {
        panic!("a user message first");
    };
    assert!(text.starts_with("Create a file hello.txt"), "{text}");

    // Records the reader predates are skipped; new messages still arrive.
    append(&f.rollout, "{\"type\":\"future_record\",\"payload\":{}}\n");
    append(&f.rollout, &assistant("one more thing"));
    let next = f.page().await;
    assert_eq!(next.epoch, first.epoch);
    assert_eq!(next.items.len(), 5);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_codex_session_id_from_herdr_picks_its_rollout() {
    let f = Fixture::start(
        |cwd| rollout("interactive_declined_approval.jsonl", cwd),
        // Another session's id: its rollout does not exist.
        Some("01a1bb6f-45a9-7332-adc0-fd8600af2b96"),
        None,
    )
    .await;
    assert_eq!(f.page().await.items, []);
    f.bridge.stop().await.expect("clean stop");

    let f = Fixture::start(
        |cwd| rollout("exec_tool_call.jsonl", cwd),
        Some(EXEC_SESSION),
        None,
    )
    .await;
    let page = f.page().await;
    assert_eq!(page.items.len(), 4);
    assert!(!page.inferred, "named by herdr");
    f.bridge.stop().await.expect("clean stop");
}

/// TL;DRs are cached by session: herdr's, or the one matched by folder.
#[tokio::test]
async fn a_finished_codex_turn_gets_a_tldr() {
    finished_turn_gets_a_tldr(Some(EXEC_SESSION)).await;
}

#[tokio::test]
async fn a_finished_turn_of_a_codex_agent_without_a_session_gets_a_tldr() {
    finished_turn_gets_a_tldr(None).await;
}

async fn finished_turn_gets_a_tldr(session: Option<&str>) {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let long = "done ".repeat(summarize::LONG_TURN_WORDS + 10);
    let f = Fixture::start(
        |cwd| rollout("exec_tool_call.jsonl", cwd),
        session,
        Some(Arc::clone(&fake)),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.rollout, &assistant(&long));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let event = stream
        .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
        .await;
    let AgentEvent::TldrReady { turn, tldr, .. } = event else {
        unreachable!("matched above");
    };
    assert_eq!(turn, TurnIndex(1));
    assert_eq!(tldr, summary());

    let response = f
        .bridge
        .get_authed(&format!("/v1/agents/{}/tldr", f.id().await))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        serde_json::from_str::<Tldr>(&response.body).expect("tldr"),
        summary()
    );
    assert_eq!(fake.tldr_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_codex_approval_screen_offers_chips_with_risk() {
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("worker", "codex", AgentStatus::Idle);
    herdr.fake.set_screen(&pane, APPROVAL);
    let bridge = Bridge::start(herdr).await;
    bridge.fake().set_status(&pane, AgentStatus::Blocked);
    let id = bridge
        .wait_for_agents("the blocked agent", |a| {
            a.len() == 1 && a[0].status == protocol::AgentStatus::Blocked
        })
        .await[0]
        .id
        .to_string();
    let response = bridge.get_authed(&format!("/v1/agents/{id}/pending")).await;
    assert_eq!(response.status, 200, "{}", response.body);
    let pending: PendingInput = serde_json::from_str(&response.body).expect("pending");
    assert_eq!(pending.kind, PendingKind::Permission);
    assert_eq!(pending.prompt, "Run `printf 'hi\\n' > hello.txt`?");
    let chips: Vec<(&str, Risk, bool)> = pending
        .options
        .as_slice()
        .iter()
        .map(|o| (o.id.as_str(), o.risk, o.accepts_feedback))
        .collect();
    assert_eq!(
        chips,
        [
            ("yes", Risk::Low, false),
            ("yes_always", Risk::Elevated, false),
            ("no_feedback", Risk::Low, true),
        ]
    );
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_codex_agent_offers_its_commands_and_takes_a_bang_line_in_session() {
    let f = Fixture::start(
        |cwd| {
            rollout("exec_tool_call.jsonl", cwd)
                + &assistant(
                    "I can't reach the registry. Run `! docker login ghcr.io` so I see the result.",
                )
        },
        Some(EXEC_SESSION),
        None,
    )
    .await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let id = f.id().await;
    let response = f
        .bridge
        .get_authed(&format!("/v1/agents/{id}/commands"))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let list: protocol::CommandList = serde_json::from_str(&response.body).expect("list");
    let [candidate] = list.candidates.as_slice() else {
        panic!("one candidate: {list:?}");
    };
    assert_eq!(candidate.text, "docker login ghcr.io");
    let run = json!({
        "idempotencyKey": "00000000-0000-4000-8000-000000000001", "origin": "app",
        "createdAt": 1, "candidate": candidate.id, "hash": candidate.hash.as_str(),
        "target": "here",
    });
    let response = f
        .bridge
        .post_authed(&format!("/v1/agents/{id}/commands/run"), &run.to_string())
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    // Codex runs a `!` line as a shell command in its session, like Claude.
    assert!(f.bridge.fake().calls().contains(&herdr::Call::Prompt {
        pane: f.pane.clone(),
        text: "!docker login ghcr.io".into(),
    }));
    f.bridge.stop().await.expect("clean stop");
}
