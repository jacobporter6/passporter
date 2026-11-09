//! Claude agents herdr reports without a session id (hooks off, as the
//! swarm runs them): their transcripts matched by folder, and TL;DRs,
//! headlines and status lines keyed on the matched session.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bridge::tldr::StatusSettings;
use herdr::AgentStatus;
use protocol::{AgentEvent, ThreadContent, ThreadPage, Tldr, TurnIndex};
use summarize::FakeSummarizer;

use crate::agents::start_agent;
use crate::harness::{Bridge, CuttableHerdr, PATIENCE};
use crate::tldr::{Fixture, append, assistant, live_stream, long_reply, summary, user};

const A: &str = "11111111-1111-4111-8111-111111111111";
const B: &str = "22222222-2222-4222-8222-222222222222";

/// A Claude agent in `cwd` (under a temp dir) with no session, behind a
/// bridge searching a temp projects dir every 50 ms.
struct Folder {
    bridge: Bridge,
    id: String,
    project: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Folder {
    /// `prepare` writes the project directory of `cwd` (relative to the
    /// temp dir) before the agent starts.
    async fn start(cwd: &str, prepare: impl FnOnce(&Path)) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        fs::create_dir_all(tmp.path().join("repo")).expect("repo");
        let cwd = tmp.path().join(cwd);
        let projects = tmp.path().join("projects");
        let project = projects.join(transcripts::project_slug(&cwd));
        fs::create_dir_all(&project).expect("project dir");
        prepare(&project);
        let bridge = Bridge::start_with(CuttableHerdr::new(), |settings| {
            settings.threads.projects_dir = Some(projects);
            settings.threads.scan_every = Duration::from_millis(50);
        })
        .await;
        start_agent(&bridge.herdr, "worker", "claude", &cwd).await;
        let agents = bridge.wait_for_agents("the agent", |a| a.len() == 1).await;
        assert_eq!(agents[0].id.session(), None, "herdr names no session");
        Self {
            id: agents[0].id.to_string(),
            bridge,
            project,
            _tmp: tmp,
        }
    }

    fn transcript(&self, session: &str) -> PathBuf {
        self.project.join(format!("{session}.jsonl"))
    }

    async fn page(&self) -> ThreadPage {
        let response = self
            .bridge
            .get_authed(&format!("/v1/agents/{}/thread", self.id))
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        serde_json::from_str(&response.body).expect("thread page")
    }

    /// The page once `done` says it is what was expected.
    async fn page_until(&self, what: &str, done: impl Fn(&ThreadPage) -> bool) -> ThreadPage {
        let wait = async {
            loop {
                let page = self.page().await;
                if done(&page) {
                    return page;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(PATIENCE, wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }
}

/// Writes `text` as session `session` in `project`, last written `ago`.
fn session(project: &Path, session: &str, text: &str, ago: Duration) -> PathBuf {
    let path = project.join(format!("{session}.jsonl"));
    append(&path, text);
    File::options()
        .append(true)
        .open(&path)
        .and_then(|f| f.set_modified(SystemTime::now() - ago))
        .expect("mtime");
    path
}

fn texts(page: &ThreadPage) -> Vec<String> {
    page.items
        .iter()
        .map(|item| match &item.content {
            ThreadContent::User { text } => format!("user: {text}"),
            ThreadContent::Assistant { text } => format!("assistant: {text}"),
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn an_agent_without_a_session_gets_the_thread_of_its_folder() {
    let f = Folder::start("repo", |project| {
        session(
            project,
            A,
            &(user("hello") + &assistant("hi")),
            Duration::ZERO,
        );
    })
    .await;
    let first = f.page().await;
    assert_eq!(texts(&first), ["user: hello", "assistant: hi"]);
    assert!(first.inferred, "the page says it was matched by folder");

    append(&f.transcript(A), &assistant("more"));
    let next = f
        .page_until("the new message", |p| p.items.len() == 3)
        .await;
    assert_eq!(next.epoch, first.epoch);
    // The id stays herdr's, the pane alone.
    assert_eq!(f.bridge.agents().await[0].id.to_string(), f.id);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn of_several_sessions_in_the_folder_the_latest_is_matched() {
    let f = Folder::start("repo", |project| {
        session(project, A, &user("older"), Duration::from_secs(20));
        session(project, B, &user("latest"), Duration::ZERO);
    })
    .await;
    let page = f.page().await;
    assert_eq!(texts(&page), ["user: latest"]);
    assert!(page.inferred);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_stale_session_gives_no_thread() {
    let f = Folder::start("repo", |project| {
        session(project, A, &user("last week"), Duration::from_hours(48));
    })
    .await;
    let page = f.page().await;
    assert_eq!(page.items, []);
    assert!(!page.inferred);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_session_started_after_clear_replaces_the_match() {
    let f = Folder::start("repo", |project| {
        session(project, A, &user("before clear"), Duration::from_secs(10));
    })
    .await;
    let first = f.page().await;
    assert_eq!(texts(&first), ["user: before clear"]);
    // `/clear` starts a new session file in the same folder.
    append(&f.transcript(B), &user("after clear"));
    let next = f
        .page_until("the new session", |p| texts(p) == ["user: after clear"])
        .await;
    assert_ne!(next.epoch, first.epoch, "a new numbering");
    assert!(next.inferred);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_crafted_cwd_is_never_followed_out_of_its_folder() {
    // herdr reports `<tmp>/repo/../repo`: a transcript under its slug is
    // never read.
    let f = Folder::start("repo/../repo", |project| {
        session(project, A, &user("not for you"), Duration::ZERO);
    })
    .await;
    let page = f.page().await;
    assert_eq!(page.items, []);
    assert!(!page.inferred);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_finished_turn_of_an_agent_matched_by_folder_gets_a_tldr_and_headline() {
    const HEADLINE: &str = "Refactored the parser.";
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_tldr(summary())
            .with_headline(HEADLINE),
    );
    let f = Fixture::start_by_folder(Some(Arc::clone(&fake)), &user("refactor"), |_| {}).await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.transcript, &long_reply());
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let event = stream
        .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
        .await;
    let AgentEvent::TldrReady { turn, tldr, .. } = event else {
        unreachable!("matched above");
    };
    assert_eq!((turn, tldr), (TurnIndex(1), summary()));
    // Its card gets the headline: keyed on the matched session, sent for
    // the agent's own id.
    stream
        .wait_for("agent_updated with the headline", |e| {
            matches!(e, AgentEvent::AgentUpdated { agent: Some(a) }
                if a.id.to_string() == f.id && a.headline.as_deref() == Some(HEADLINE))
        })
        .await;
    let response = f.get("").await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        serde_json::from_str::<Tldr>(&response.body).expect("tldr"),
        summary()
    );
    let page = f
        .bridge
        .get_authed(&format!("/v1/agents/{}/thread", f.id))
        .await;
    let page: ThreadPage = serde_json::from_str(&page.body).expect("page");
    assert!(page.inferred);
    assert_eq!(page.headlines.len(), 1, "{:?}", page.headlines);
    assert_eq!(fake.tldr_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_working_agent_matched_by_folder_gets_status_lines() {
    const LINE: &str = "Running the test suite";
    let fake = Arc::new(FakeSummarizer::new().with_status(LINE));
    let f = Fixture::start_by_folder(Some(Arc::clone(&fake)), &user("run it"), |settings| {
        settings.tldr.status = StatusSettings {
            every: Duration::from_millis(100),
            check_every: Duration::from_millis(20),
            tap_every: Duration::from_millis(100),
            headline_every: Duration::from_secs(3600),
        };
    })
    .await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.transcript, &(assistant("one") + &assistant("two")));
    let event = stream
        .wait_for("status_line_ready", |e| {
            matches!(e, AgentEvent::StatusLineReady { .. })
        })
        .await;
    let AgentEvent::StatusLineReady {
        agent_id,
        status_line,
    } = event
    else {
        unreachable!("matched above");
    };
    assert_eq!(agent_id.to_string(), f.id);
    assert_eq!(status_line.text, LINE);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}
