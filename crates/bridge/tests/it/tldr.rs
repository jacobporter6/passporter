//! `GET /v1/agents/:id/tldr` and `tldr_ready` events, with
//! `FakeSummarizer` in place of `claude -p`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bridge::tldr::{DynSummarizer, SharedSummarizer};
use herdr::{AgentSession, AgentStatus, Herdr, PaneId};
use protocol::{AgentEvent, Tldr, TurnIndex};
use serde_json::json;
use summarize::{
    FakeSummarizer, Headline, StatusText, Suggestion, SuggestionContext, Summarizer, TurnDigest,
};

use crate::agents::start_agent;
use crate::harness::{Bridge, CuttableHerdr, HttpResponse, PATIENCE, Stream};

const SESSION: &str = "7a1d2c3b-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

fn line(record: &serde_json::Value) -> String {
    format!("{record}\n")
}

pub(crate) fn user(text: &str) -> String {
    line(&json!({"type": "user", "message": {"content": text}}))
}

pub(crate) fn assistant(text: &str) -> String {
    line(&json!({"type": "assistant", "message": {"content": text}}))
}

/// An assistant message long enough to make its turn long (design §6.4).
pub(crate) fn long_reply() -> String {
    assistant(&"done ".repeat(summarize::LONG_TURN_WORDS + 10))
}

pub(crate) fn append(path: &Path, text: &str) {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .expect("append");
}

pub(crate) fn summary() -> Tldr {
    Tldr {
        outcome: "Refactored the parser; tests pass.".into(),
        changed: vec!["parser.rs (+40 −12)".into()],
        decide: Some("Merge now or wait for review?".into()),
        risks: vec!["No benchmark run.".into()],
    }
}

/// A working Claude agent whose transcript holds `transcript`, behind a
/// bridge using `fake` (or no summarizer).
pub(crate) struct Fixture {
    pub(crate) bridge: Bridge,
    pub(crate) pane: PaneId,
    pub(crate) id: String,
    pub(crate) transcript: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Fixture {
    pub(crate) async fn start(fake: Option<Arc<FakeSummarizer>>, transcript_text: &str) -> Self {
        Self::start_with(fake, transcript_text, |_| {}).await
    }

    pub(crate) async fn start_with(
        fake: Option<Arc<FakeSummarizer>>,
        transcript_text: &str,
        adjust: impl FnOnce(&mut bridge::Settings),
    ) -> Self {
        Self::launch(fake, transcript_text, adjust, true).await
    }

    /// [`Self::start_with`], but herdr reports no session for the agent
    /// (hooks off): its transcript is matched by folder.
    pub(crate) async fn start_by_folder(
        fake: Option<Arc<FakeSummarizer>>,
        transcript_text: &str,
        adjust: impl FnOnce(&mut bridge::Settings),
    ) -> Self {
        Self::launch(fake, transcript_text, adjust, false).await
    }

    async fn launch(
        fake: Option<Arc<FakeSummarizer>>,
        transcript_text: &str,
        adjust: impl FnOnce(&mut bridge::Settings),
        with_session: bool,
    ) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        let projects = tmp.path().join("projects");
        fs::create_dir_all(&cwd).expect("cwd");
        let project = projects.join(transcripts::project_slug(&cwd));
        fs::create_dir_all(&project).expect("project dir");
        let transcript = project.join(format!("{SESSION}.jsonl"));
        append(&transcript, transcript_text);

        let bridge = Bridge::start_with(CuttableHerdr::new(), |settings| {
            settings.threads.projects_dir = Some(projects);
            settings.tldr.summarizer = fake.map(|fake| SharedSummarizer::new(fake));
            adjust(settings);
        })
        .await;
        let pane = start_agent(&bridge.herdr, "worker", "claude", &cwd).await;
        if with_session {
            bridge.fake().set_session(
                &pane,
                AgentSession {
                    source: "herdr:claude".into(),
                    agent: "claude".into(),
                    kind: "id".into(),
                    value: SESSION.into(),
                },
            );
        }
        bridge.fake().set_status(&pane, AgentStatus::Working);
        let agents = bridge
            .wait_for_agents("the session", |a| {
                a.iter().any(|a| {
                    a.id.session().is_some() == with_session
                        && a.status == protocol::AgentStatus::Working
                })
            })
            .await;
        let id = agents[0].id.to_string();
        Self {
            bridge,
            pane,
            id,
            transcript,
            _tmp: tmp,
        }
    }

    pub(crate) async fn get(&self, query: &str) -> HttpResponse {
        self.bridge
            .get_authed(&format!("/v1/agents/{}/tldr{query}", self.id))
            .await
    }

    /// The TL;DR once it is served, polling while it generates.
    async fn ready(&self) -> Tldr {
        let wait = async {
            loop {
                let response = self.get("").await;
                match response.status {
                    200 if response.body != "null" => {
                        return serde_json::from_str(&response.body).expect("tldr");
                    }
                    202 => tokio::time::sleep(Duration::from_millis(20)).await,
                    _ => panic!("unexpected {}: {}", response.status, response.body),
                }
            }
        };
        tokio::time::timeout(PATIENCE, wait)
            .await
            .expect("TL;DR in time")
    }
}

/// A stream past its first snapshot, so later changes arrive as events.
pub(crate) async fn live_stream(bridge: &Bridge) -> Stream<tokio::net::TcpStream> {
    let mut stream = bridge.stream().await;
    stream
        .wait_for("snapshot", |e| matches!(e, AgentEvent::Snapshot { .. }))
        .await;
    stream
}

pub(crate) fn assert_null(response: &HttpResponse, status: u16) {
    assert_eq!(response.status, status, "{}", response.body);
    assert_eq!(response.body, "null");
}

#[tokio::test]
async fn a_finished_long_turn_is_summarized_eagerly_and_served_from_the_cache() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let f = Fixture::start(Some(Arc::clone(&fake)), &user("refactor the parser")).await;
    let mut stream = live_stream(&f.bridge).await;
    // Mid-turn, nothing is finished yet.
    assert_null(&f.get("").await, 200);

    append(&f.transcript, &long_reply());
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let event = stream
        .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
        .await;
    let AgentEvent::TldrReady {
        agent_id,
        turn,
        tldr,
        provisional,
    } = event
    else {
        unreachable!("matched above");
    };
    assert_eq!(agent_id.to_string(), f.id);
    assert_eq!(turn, TurnIndex(1));
    assert_eq!(tldr, summary());
    assert!(!provisional, "made from the finished turn");

    // Cache hits: served without another model call.
    for query in ["", "?turn=1"] {
        let response = f.get(query).await;
        assert_eq!(response.status, 200);
        assert_eq!(
            serde_json::from_str::<Tldr>(&response.body).expect("tldr"),
            summary()
        );
    }
    assert_eq!(fake.tldr_calls(), 1);

    // Summarizing read the transcript once: no reader is held for this
    // unviewed agent, so new lines are not polled and pushed.
    append(&f.transcript, &assistant("more"));
    // Nor is the TL;DR announced twice.
    let pushed = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            let event = stream.next_event().await;
            if matches!(
                event,
                AgentEvent::ThreadItems { .. } | AgentEvent::TldrReady { .. }
            ) {
                return event;
            }
        }
    })
    .await;
    assert!(pushed.is_err(), "unexpected {pushed:?}");
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_tldr_being_generated_is_202_and_generated_once() {
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_tldr(summary())
            .with_delay(Duration::from_millis(400)),
    );
    let f = Fixture::start(
        Some(Arc::clone(&fake)),
        &(user("refactor the parser") + &long_reply()),
    )
    .await;
    // Turn 1 is finished once a second prompt starts turn 2.
    append(&f.transcript, &user("now the lexer"));
    assert_null(&f.get("").await, 202);
    assert_null(&f.get("?turn=1").await, 202);
    assert_eq!(f.ready().await, summary());
    assert_eq!(fake.tldr_calls(), 1);
    f.bridge.stop().await.expect("clean stop");
}

/// Polls until the TL;DR request stops answering 202, and returns the
/// response.
async fn settled(f: &Fixture) -> HttpResponse {
    let wait = async {
        loop {
            let response = f.get("").await;
            if response.status != 202 {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait)
        .await
        .expect("generation ended")
}

/// Asserts the request keeps answering `null` and the model is not run
/// again for `window`.
async fn stays_failed(f: &Fixture, fake: &FakeSummarizer, window: Duration) {
    let calls = fake.tldr_calls();
    let until = tokio::time::Instant::now() + window;
    while tokio::time::Instant::now() < until {
        assert_null(&f.get("").await, 200);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fake.tldr_calls(), calls, "no retry during the backoff");
}

#[tokio::test]
async fn a_failed_generation_backs_off_instead_of_rerunning_on_every_poll() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    fake.fail_next(summarize::Error::NoStructuredOutput);
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("go") + &long_reply() + &user("next")),
        |settings| settings.tldr.retry_after = Duration::from_millis(400),
    )
    .await;
    assert_null(&f.get("").await, 202);
    assert_null(&settled(&f).await, 200);
    assert_eq!(fake.tldr_calls(), 1);
    stays_failed(&f, &fake, Duration::from_millis(200)).await;
    // Once the backoff expires, the next request retries, and succeeds.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(f.ready().await, summary());
    assert_eq!(fake.tldr_calls(), 2);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_usage_limit_waits_the_longest_backoff() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    fake.fail_next(summarize::Error::UsageLimit {
        message: "limit".into(),
    });
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("go") + &long_reply() + &user("next")),
        |settings| {
            settings.tldr.retry_after = Duration::from_millis(10);
            settings.tldr.max_retry_after = Duration::from_secs(3600);
        },
    )
    .await;
    assert_null(&f.get("?turn=1").await, 202);
    assert_null(&settled(&f).await, 200);
    stays_failed(&f, &fake, Duration::from_millis(200)).await;
    // The limit is account-wide: other turns wait too.
    assert_null(&f.get("?turn=2").await, 200);
    assert_eq!(fake.tldr_calls(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn each_turn_gets_its_own_job() {
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_tldr(summary())
            .with_delay(Duration::from_millis(200)),
    );
    let f = Fixture::start(
        Some(Arc::clone(&fake)),
        &(user("one") + &long_reply() + &user("two") + &long_reply() + &user("three")),
    )
    .await;
    assert_null(&f.get("?turn=1").await, 202);
    // Turn 2 is not answered by turn 1's job: it gets its own.
    assert_null(&f.get("?turn=2").await, 202);
    let both = async {
        while fake.tldr_calls() < 2
            || f.get("?turn=1").await.status != 200
            || f.get("?turn=2").await.status != 200
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(PATIENCE, both)
        .await
        .expect("both ready");
    assert_eq!(fake.tldr_calls(), 2);
    f.bridge.stop().await.expect("clean stop");
}

/// Waits for `status` to reach the stream.
pub(crate) async fn status_seen(
    stream: &mut Stream<tokio::net::TcpStream>,
    status: protocol::AgentStatus,
) {
    stream
        .wait_for(
            "status",
            |e| matches!(e, AgentEvent::AgentUpdated { agent: Some(a) } if a.status == status),
        )
        .await;
}

#[tokio::test]
async fn a_turn_finishing_while_the_last_one_generates_is_not_lost() {
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_tldr(summary())
            .with_delay(Duration::from_millis(300)),
    );
    let f = Fixture::start(Some(Arc::clone(&fake)), &(user("one") + &long_reply())).await;
    let mut stream = live_stream(&f.bridge).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    status_seen(&mut stream, protocol::AgentStatus::Done).await;
    // A quick follow-up finishes while turn 1 is still being summarized.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    status_seen(&mut stream, protocol::AgentStatus::Working).await;
    append(&f.transcript, &(user("two") + &long_reply()));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let mut turns = Vec::new();
    while turns.len() < 2 {
        let event = stream
            .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
            .await;
        if let AgentEvent::TldrReady { turn, .. } = event {
            turns.push(turn);
        }
    }
    assert_eq!(turns, [TurnIndex(1), TurnIndex(2)]);
    assert_eq!(fake.tldr_calls(), 2);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_generation_that_times_out_backs_off_then_retries() {
    let fake = Arc::new(FakeSummarizer::new().with_delay(Duration::from_millis(300)));
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("go") + &long_reply() + &user("next")),
        |settings| {
            settings.tldr.timeout = Duration::from_millis(50);
            settings.tldr.retry_after = Duration::from_millis(200);
        },
    )
    .await;
    assert_null(&f.get("").await, 202);
    assert_null(&settled(&f).await, 200);
    assert_eq!(fake.tldr_calls(), 1);
    stays_failed(&f, &fake, Duration::from_millis(100)).await;
    let retried = async {
        while fake.tldr_calls() < 2 {
            f.get("").await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(PATIENCE, retried)
        .await
        .expect("retried");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_just_finished_turn_is_read_after_the_settle_pause() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("refactor the parser") + &assistant("working on it")),
        |settings| settings.tldr.settle = Duration::from_millis(300),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    stream
        .wait_for("done", |e| {
            matches!(e, AgentEvent::AgentUpdated { agent: Some(a) }
                if a.status.is_ready())
        })
        .await;
    // herdr says done, but the turn's last lines are not written yet: the
    // turn read now would be short. The request waits for the turn instead.
    assert_null(&f.get("").await, 202);
    append(&f.transcript, &long_reply());
    assert_eq!(f.ready().await, summary());
    assert_eq!(fake.tldr_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_finished_turn_is_kept_until_the_new_prompt_is_written() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let f = Fixture::start(
        Some(Arc::clone(&fake)),
        &(user("refactor the parser") + &long_reply()),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    stream
        .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
        .await;
    // A new prompt: herdr reports working before the prompt reaches the
    // transcript. Turn 1 is still the latest finished one.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    stream
        .wait_for("working", |e| {
            matches!(e, AgentEvent::AgentUpdated { agent: Some(a) }
                if a.status == protocol::AgentStatus::Working)
        })
        .await;
    assert_eq!(f.ready().await, summary());
    append(&f.transcript, &user("now the lexer"));
    assert_eq!(f.ready().await, summary());
    assert_eq!(fake.tldr_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn short_turns_and_disabled_tldrs_have_none() {
    let fake = Arc::new(FakeSummarizer::new());
    let f = Fixture::start(
        Some(Arc::clone(&fake)),
        &(user("hi") + &assistant("hello") + &user("bye")),
    )
    .await;
    assert_null(&f.get("").await, 200);
    assert_eq!(fake.tldr_calls(), 0);
    f.bridge.stop().await.expect("clean stop");

    let disabled = Fixture::start(None, &(user("go") + &long_reply() + &user("next"))).await;
    assert_null(&disabled.get("").await, 200);
    disabled.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn non_claude_agents_have_none_and_gone_agents_404() {
    let fake = Arc::new(FakeSummarizer::new());
    let f = Fixture::start(
        Some(Arc::clone(&fake)),
        &(user("go") + &long_reply() + &user("next")),
    )
    .await;
    let cwd = tempfile::tempdir().expect("tmp");
    let codex_pane = start_agent(&f.bridge.herdr, "other", "codex", cwd.path()).await;
    let agents = f
        .bridge
        .wait_for_agents("both agents", |a| a.len() == 2)
        .await;
    let codex = agents
        .iter()
        .find(|a| a.id.pane().as_str() == codex_pane.as_str())
        .expect("codex agent");
    let response = f
        .bridge
        .get_authed(&format!("/v1/agents/{}/tldr", codex.id))
        .await;
    assert_null(&response, 200);

    let bad_query = f.get("?turn=x").await;
    assert_eq!(bad_query.status, 400);
    let bad_id = f.bridge.get_authed("/v1/agents/a%20b/tldr").await;
    assert_eq!(bad_id.status, 400);

    f.bridge.fake().close_pane(&f.pane).await.expect("close");
    assert_eq!(f.get("").await.status, 404);
    let other_session = format!("{codex_pane}~00000000-0000-4000-8000-000000000000");
    let moved = f
        .bridge
        .get_authed(&format!("/v1/agents/{other_session}/tldr"))
        .await;
    assert_eq!(moved.status, 404);
    assert_eq!(fake.tldr_calls(), 0);
    f.bridge.stop().await.expect("clean stop");
}

/// Waits for the next `tldr_ready` and returns its turn.
async fn tldr_ready(stream: &mut Stream<tokio::net::TcpStream>) -> TurnIndex {
    match stream
        .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
        .await
    {
        AgentEvent::TldrReady { turn, .. } => turn,
        _ => unreachable!("matched above"),
    }
}

#[tokio::test]
async fn a_quick_reply_does_not_cost_the_finished_turn_its_tldr() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let f = Fixture::start(
        Some(Arc::clone(&fake)),
        &(user("refactor the parser") + &long_reply()),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    // The registry reads the agent done; by the time the TL;DR watcher
    // asks, a quick reply has it working again, its prompt not yet in the
    // transcript.
    f.bridge.herdr.then_report(&f.pane, AgentStatus::Working);
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    assert_eq!(tldr_ready(&mut stream).await, TurnIndex(1));
    assert_eq!(fake.tldr_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_failed_generation_is_retried_by_the_bridge_when_its_backoff_ends() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    fake.fail_next(summarize::Error::NoStructuredOutput);
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("refactor the parser") + &long_reply()),
        |settings| settings.tldr.retry_after = Duration::from_millis(200),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    // No request asks again: the TL;DR still arrives once the backoff ends.
    assert_eq!(tldr_ready(&mut stream).await, TurnIndex(1));
    assert_eq!(fake.tldr_calls(), 2);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_turn_that_finishes_during_a_usage_limit_is_summarized_when_it_lifts() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    fake.fail_next(summarize::Error::UsageLimit {
        message: "limit".into(),
    });
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("one") + &long_reply() + &user("two")),
        |settings| {
            settings.tldr.retry_after = Duration::from_millis(10);
            settings.tldr.max_retry_after = Duration::from_millis(400);
        },
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    // Turn 1 hits the limit.
    assert_null(&f.get("").await, 202);
    assert_null(&settled(&f).await, 200);
    // Turn 2 finishes while the limit holds: nothing runs for it now...
    append(&f.transcript, &long_reply());
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    status_seen(&mut stream, protocol::AgentStatus::Done).await;
    assert_eq!(fake.tldr_calls(), 1);
    // ...and its TL;DR arrives once the limit lifts. Turn 1, no longer
    // the latest, is not retried.
    assert_eq!(tldr_ready(&mut stream).await, TurnIndex(2));
    assert_eq!(fake.tldr_calls(), 2);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

/// A summarizer with a bug: it panics.
#[derive(Debug, Default)]
struct Panicking {
    calls: AtomicUsize,
}

impl Summarizer for Panicking {
    fn tldr(&self, _turn: &TurnDigest) -> impl Future<Output = summarize::Result<Tldr>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        async { panic!("summarizer bug") }
    }

    fn headline(
        &self,
        _turn: &TurnDigest,
    ) -> impl Future<Output = summarize::Result<Headline>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        async { panic!("summarizer bug") }
    }

    fn status(
        &self,
        _turn: &TurnDigest,
    ) -> impl Future<Output = summarize::Result<StatusText>> + Send {
        std::future::ready(Err(summarize::Error::NoStructuredOutput))
    }

    fn suggestions(
        &self,
        _context: &SuggestionContext,
    ) -> impl Future<Output = summarize::Result<Vec<Suggestion>>> + Send {
        std::future::ready(Ok(Vec::new()))
    }
}

#[tokio::test]
async fn a_panicking_generation_does_not_leave_its_turn_generating_forever() {
    let panicking = Arc::new(Panicking::default());
    let summarizer = SharedSummarizer::new(Arc::clone(&panicking) as Arc<dyn DynSummarizer>);
    let f = Fixture::start_with(
        None,
        &(user("go") + &long_reply() + &user("next")),
        |settings| settings.tldr.summarizer = Some(summarizer),
    )
    .await;
    assert_null(&f.get("").await, 202);
    // Once the job is gone, a request queues a new one rather than being
    // told "generating" by the dead job's entry.
    let requeued = async {
        while panicking.calls.load(Ordering::SeqCst) < 2 {
            f.get("").await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(PATIENCE, requeued)
        .await
        .expect("a new job after the panic");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn earlier_turns_are_generated_one_at_a_time_on_demand() {
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_tldr(summary())
            .with_delay(Duration::from_millis(300)),
    );
    let f = Fixture::start(
        Some(Arc::clone(&fake)),
        &(user("one")
            + &long_reply()
            + &user("two")
            + &long_reply()
            + &user("three")
            + &long_reply()
            + &user("four")),
    )
    .await;
    assert_null(&f.get("?turn=1").await, 202);
    // Another earlier turn waits its turn: no pile of model runs.
    let busy = f.get("?turn=2").await;
    assert_eq!(busy.status, 429, "{}", busy.body);
    assert!(busy.header("retry-after").is_some());
    // The latest finished turn is never refused.
    assert_null(&f.get("").await, 202);
    let done = async {
        while f.get("?turn=1").await.status != 200 || f.get("?turn=3").await.status != 200 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(PATIENCE, done)
        .await
        .expect("both ready");
    // With turn 1 done, turn 2 may go.
    assert_null(&f.get("?turn=2").await, 202);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_turn_still_being_written_after_the_settle_pause_is_summarized_once_whole() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let settle = Duration::from_millis(500);
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("refactor the parser") + &long_reply()),
        |settings| settings.tldr.settle = settle,
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    status_seen(&mut stream, protocol::AgentStatus::Done).await;
    // A last line lands after the first settle pause.
    tokio::time::sleep(settle + settle / 2).await;
    append(
        &f.transcript,
        &assistant("One more thing: the docs need a pass."),
    );
    assert_eq!(tldr_ready(&mut stream).await, TurnIndex(1));
    // The TL;DR covers the whole turn: the next read is a cache hit.
    assert_eq!(f.ready().await, summary());
    assert_eq!(fake.tldr_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}
