//! `GET /v1/agents/:id/thread` (synthetic transcripts) and
//! `GET /v1/agents/:id/screen`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use herdr::{AgentSession, AgentStatus, PaneId};
use protocol::{AgentEvent, AgentScreen, MAX_SCREEN_LINES, Seq, ThreadContent, ThreadPage};
use serde_json::json;

use crate::agents::start_agent;
use crate::harness::{Bridge, CuttableHerdr};

const SESSION: &str = "5f0c6a1e-2b7d-4c39-9e51-0a8d3f6b2c71";

fn line(record: &serde_json::Value) -> String {
    format!("{record}\n")
}

fn user(text: &str) -> String {
    line(&json!({"type": "user", "message": {"content": text}}))
}

fn assistant(text: &str) -> String {
    line(&json!({"type": "assistant", "message": {"content": text}}))
}

fn append(path: &Path, text: &str) {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .expect("append");
}

fn texts(page_items: &[protocol::ThreadItem]) -> Vec<String> {
    page_items
        .iter()
        .map(|item| match &item.content {
            ThreadContent::User { text } => format!("user: {text}"),
            ThreadContent::Assistant { text } => format!("assistant: {text}"),
            other => format!("{other:?}"),
        })
        .collect()
}

/// A Claude agent with a session whose transcript lives in a temp projects
/// dir, registered (with its session) by the bridge.
struct ClaudeFixture {
    bridge: Bridge,
    pane: PaneId,
    id: String,
    transcript: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn claude_fixture() -> ClaudeFixture {
    let tmp = tempfile::tempdir().expect("tmp");
    let cwd = tmp.path().join("repo");
    let projects = tmp.path().join("projects");
    fs::create_dir_all(&cwd).expect("cwd");
    let project = projects.join(transcripts::project_slug(&cwd));
    fs::create_dir_all(&project).expect("project dir");
    let transcript = project.join(format!("{SESSION}.jsonl"));
    append(&transcript, &(user("hello") + &assistant("hi there")));

    let herdr = CuttableHerdr::new();
    let bridge = Bridge::start_with(herdr, |settings| {
        settings.threads.projects_dir = Some(projects);
    })
    .await;
    let pane = start_agent(&bridge.herdr, "worker", "claude", &cwd).await;
    bridge.fake().set_session(
        &pane,
        AgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            kind: "id".into(),
            value: SESSION.into(),
        },
    );
    // A status change makes the registry re-read the agent, session included.
    bridge.fake().set_status(&pane, AgentStatus::Working);
    let agents = bridge
        .wait_for_agents("the session", |a| {
            a.iter().any(|a| a.id.session().is_some())
        })
        .await;
    let id = agents[0].id.to_string();
    ClaudeFixture {
        bridge,
        pane,
        id,
        transcript,
        _tmp: tmp,
    }
}

async fn page(bridge: &Bridge, id: &str, query: &str) -> ThreadPage {
    let response = bridge
        .get_authed(&format!("/v1/agents/{id}/thread{query}"))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    serde_json::from_str(&response.body).expect("thread page")
}

#[tokio::test]
async fn thread_pages_follow_the_transcript() {
    let f = claude_fixture().await;
    let first = page(&f.bridge, &f.id, "").await;
    assert_eq!(texts(&first.items), ["user: hello", "assistant: hi there"]);
    assert!(!first.has_more);

    let resume = first.resume_cursor.0;
    let empty = page(&f.bridge, &f.id, &format!("?since={resume}")).await;
    assert_eq!(empty.items, []);
    assert_eq!(empty.epoch, first.epoch);

    let limited = page(&f.bridge, &f.id, "?since=0&limit=1").await;
    assert_eq!(texts(&limited.items), ["user: hello"]);
    assert!(limited.has_more);

    append(&f.transcript, &assistant("more"));
    let next = page(&f.bridge, &f.id, &format!("?since={resume}")).await;
    assert_eq!(texts(&next.items), ["assistant: more"]);
    assert_eq!(next.epoch, first.epoch);

    // Truncation (e.g. a rewritten transcript) starts a new epoch.
    fs::write(&f.transcript, user("fresh")).expect("rewrite");
    let reset = page(&f.bridge, &f.id, "").await;
    assert_ne!(reset.epoch, first.epoch);
    assert_eq!(texts(&reset.items), ["user: fresh"]);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn new_thread_items_are_pushed_on_the_stream() {
    let f = claude_fixture().await;
    let first = page(&f.bridge, &f.id, "").await;
    let mut stream = f.bridge.stream().await;
    // The snapshot proves the subscription is live before the append.
    assert!(matches!(
        stream.next_event().await,
        AgentEvent::Snapshot { .. }
    ));
    append(&f.transcript, &assistant("pushed"));
    let event = stream
        .wait_for(
            "thread items",
            |e| matches!(e, AgentEvent::ThreadItems { items, .. } if !items.is_empty()),
        )
        .await;
    let AgentEvent::ThreadItems {
        agent_id,
        epoch,
        items,
        resume_cursor,
    } = event
    else {
        unreachable!("matched above");
    };
    assert_eq!(agent_id.to_string(), f.id);
    assert_eq!(epoch, first.epoch);
    assert_eq!(texts(&items), ["assistant: pushed"]);
    assert!(resume_cursor > first.resume_cursor);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn threads_without_a_transcript_are_empty_and_unknown_agents_404() {
    let f = claude_fixture().await;
    let cwd = tempfile::tempdir().expect("tmp");
    let shell = start_agent(&f.bridge.herdr, "other", "codex", cwd.path()).await;
    let agents = f
        .bridge
        .wait_for_agents("both agents", |a| a.len() == 2)
        .await;
    let codex = agents
        .iter()
        .find(|a| a.id.pane().as_str() == shell.as_str())
        .expect("codex agent");
    let empty = page(&f.bridge, &codex.id.to_string(), "").await;
    assert_eq!(empty.items, []);

    let gone = format!("{}~00000000-0000-4000-8000-000000000000", f.pane);
    let path = format!("/v1/agents/{gone}/thread");
    assert_eq!(
        f.bridge.get_authed(&path).await.status,
        404,
        "other session"
    );
    let missing = f.bridge.get_authed("/v1/agents/w99:p9/thread").await;
    assert_eq!(missing.status, 404);
    let bad = f.bridge.get_authed("/v1/agents/a%20b/thread").await;
    assert_eq!(bad.status, 400);
    let bad_query = format!("/v1/agents/{}/thread?since=x", f.id);
    assert_eq!(f.bridge.get_authed(&bad_query).await.status, 400);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn screen_falls_back_to_the_visible_screen_when_history_is_refused() {
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("worker", "claude", AgentStatus::Idle);
    let bridge = Bridge::start(herdr).await;
    bridge
        .fake()
        .set_screen(&pane, "Do you want to proceed?\n❯ 1. Yes\n  2. No\n");
    let id = bridge.wait_for_agents("the agent", |a| a.len() == 1).await[0]
        .id
        .to_string();
    // herdr refuses to scroll history unless the agent is idle (`agent_not_idle`).
    bridge.fake().fail_next(
        herdr::fake::Method::AgentRead,
        herdr::fake::Fault::Reject(herdr::ErrorCode::Other("agent_not_idle".to_owned())),
    );

    let response = bridge.get_authed(&format!("/v1/agents/{id}/screen")).await;

    assert_eq!(response.status, 200, "{}", response.body);
    let shown: AgentScreen = serde_json::from_str(&response.body).expect("screen");
    assert!(
        shown.text.contains("Do you want to proceed?"),
        "{}",
        shown.text
    );
}

#[tokio::test]
async fn screen_shows_the_terminal_tail() {
    let herdr = CuttableHerdr::new();
    let pane = herdr.fake.add_agent("worker", "claude", AgentStatus::Idle);
    let bridge = Bridge::start(herdr).await;
    bridge.fake().set_status(&pane, AgentStatus::Blocked);
    let screen = (0..100)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    bridge.fake().set_screen(&pane, &format!("{screen}\n\n"));
    let id = bridge.wait_for_agents("the agent", |a| a.len() == 1).await[0]
        .id
        .to_string();

    let response = bridge.get_authed(&format!("/v1/agents/{id}/screen")).await;
    assert_eq!(response.status, 200, "{}", response.body);
    let shown: AgentScreen = serde_json::from_str(&response.body).expect("screen");
    let lines: Vec<&str> = shown.text.lines().collect();
    assert!(lines.len() <= MAX_SCREEN_LINES, "{} lines", lines.len());
    assert_eq!(lines.last(), Some(&"line 99"));
    assert_eq!(shown.status, protocol::AgentStatus::Blocked);
    assert_eq!(shown.seq, Seq(2));

    let unauthenticated = bridge
        .http("GET", &format!("/v1/agents/{id}/screen"), &[])
        .await;
    assert_eq!(unauthenticated.status, 401);
    bridge.stop().await.expect("clean stop");
}
