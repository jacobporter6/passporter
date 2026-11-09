//! Turn headlines: `headline_ready` events, the card headline on the agent
//! list and the stream, and headlines on thread pages, with
//! `FakeSummarizer` in place of `claude -p`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bridge::tldr::{DynSummarizer, SharedSummarizer};
use herdr::{AgentSession, AgentStatus, PaneId};
use protocol::Tldr;
use protocol::{AgentEvent, ThreadPage, TurnHeadline, TurnIndex};
use summarize::{
    FakeSummarizer, Headline, StatusText, Suggestion, SuggestionContext, Summarizer, TurnDigest,
};

use crate::agents::start_agent;
use crate::harness::{Bridge, CuttableHerdr, PATIENCE, Stream};
use crate::tldr::{
    Fixture, append, assert_null, assistant, live_stream, long_reply, status_seen, summary, user,
};

const HEADLINE: &str = "Fixed the parser; asks whether to push.";

fn headline_ready(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::HeadlineReady { .. })
}

/// Waits for `headline_ready`, returning its turn and text.
async fn next_headline(stream: &mut Stream<tokio::net::TcpStream>) -> (TurnIndex, String) {
    match stream.wait_for("headline_ready", headline_ready).await {
        AgentEvent::HeadlineReady { turn, headline, .. } => (turn, headline),
        _ => unreachable!("matched above"),
    }
}

async fn thread_page(f: &Fixture) -> ThreadPage {
    thread_page_since(f, 0).await
}

async fn thread_page_since(f: &Fixture, since: u64) -> ThreadPage {
    let response = f
        .bridge
        .get_authed(&format!("/v1/agents/{}/thread?since={since}", f.id))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    serde_json::from_str(&response.body).expect("thread page")
}

fn card_headline(f: &Fixture, agents: &[protocol::Agent]) -> Option<String> {
    agents
        .iter()
        .find(|a| a.id.to_string() == f.id)
        .and_then(|a| a.headline.clone())
}

#[tokio::test]
async fn a_finished_short_turn_gets_a_headline_on_its_card_and_thread() {
    let fake = Arc::new(FakeSummarizer::new().with_headline(HEADLINE));
    let f = Fixture::start(Some(Arc::clone(&fake)), &user("fix the parser")).await;
    let mut stream = live_stream(&f.bridge).await;
    assert_eq!(card_headline(&f, &f.bridge.agents().await), None);

    append(&f.transcript, &assistant("Fixed. Shall I push?"));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    assert_eq!(
        next_headline(&mut stream).await,
        (TurnIndex(1), HEADLINE.to_owned())
    );
    // The agent is re-sent with its new card headline.
    let updated = stream
        .wait_for("agent_updated with the headline", |e| {
            matches!(e, AgentEvent::AgentUpdated { agent: Some(a) }
                if a.headline.as_deref() == Some(HEADLINE))
        })
        .await;
    let AgentEvent::AgentUpdated { agent: Some(agent) } = updated else {
        unreachable!("matched above");
    };
    assert_eq!(agent.id.to_string(), f.id);
    assert_eq!(agent.status, protocol::AgentStatus::Done);

    // The list, a new stream's snapshot and the thread page carry it too.
    assert_eq!(
        card_headline(&f, &f.bridge.agents().await).as_deref(),
        Some(HEADLINE)
    );
    let mut fresh = f.bridge.stream().await;
    let snapshot = fresh
        .wait_for("snapshot", |e| matches!(e, AgentEvent::Snapshot { .. }))
        .await;
    let AgentEvent::Snapshot { agents } = snapshot else {
        unreachable!("matched above");
    };
    assert_eq!(card_headline(&f, &agents).as_deref(), Some(HEADLINE));
    assert_eq!(
        thread_page(&f).await.headlines,
        [TurnHeadline {
            turn: TurnIndex(1),
            text: HEADLINE.into(),
            provisional: false,
        }]
    );

    // A short turn: a headline, no TL;DR.
    assert_null(
        &f.bridge
            .get_authed(&format!("/v1/agents/{}/tldr", f.id))
            .await,
        200,
    );
    assert_eq!(fake.headline_calls(), 1);
    assert_eq!(fake.tldr_calls(), 0);
    fresh.close().await;
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_finished_long_turn_gets_a_headline_and_a_tldr() {
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_headline(HEADLINE)
            .with_tldr(summary()),
    );
    let f = Fixture::start(Some(Arc::clone(&fake)), &user("refactor the parser")).await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.transcript, &long_reply());
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    // The TL;DR first, then the headline in a queue slot of its own.
    let first = stream
        .wait_for("headline or TL;DR", |e| {
            headline_ready(e) || matches!(e, AgentEvent::TldrReady { .. })
        })
        .await;
    assert!(matches!(first, AgentEvent::TldrReady { .. }), "{first:?}");
    stream.wait_for("headline_ready", headline_ready).await;
    assert_eq!(fake.headline_calls(), 1);
    assert_eq!(fake.tldr_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn each_turn_keeps_its_own_headline_and_the_card_shows_the_latest() {
    let fake = Arc::new(FakeSummarizer::new());
    let f = Fixture::start(Some(Arc::clone(&fake)), &user("one")).await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.transcript, &assistant("first answer"));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let (turn, first) = next_headline(&mut stream).await;
    assert_eq!(turn, TurnIndex(1));

    append(&f.transcript, &user("two"));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    status_seen(&mut stream, protocol::AgentStatus::Working).await;
    append(&f.transcript, &assistant(&"second answer ".repeat(5)));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let (turn, second) = next_headline(&mut stream).await;
    assert_eq!(turn, TurnIndex(2));
    assert_ne!(first, second, "the fake's headline counts words");

    let turns: Vec<(u32, String)> = thread_page(&f)
        .await
        .headlines
        .into_iter()
        .map(|h| (h.turn.0, h.text))
        .collect();
    assert_eq!(turns, [(1, first), (2, second.clone())]);
    // A later page carries the last few too, to catch up after a
    // disconnect.
    let full = thread_page(&f).await;
    let later = thread_page_since(&f, full.resume_cursor.0).await;
    assert_eq!(later.headlines, full.headlines);
    assert_eq!(card_headline(&f, &f.bridge.agents().await), Some(second));
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_short_turn_finished_before_the_bridge_saw_it_gets_a_headline_on_request() {
    let fake = Arc::new(FakeSummarizer::new().with_headline(HEADLINE));
    // Finished before the bridge started: no watcher event for it.
    let f = Fixture::start(
        Some(Arc::clone(&fake)),
        &(user("go") + &assistant("done") + &user("next")),
    )
    .await;
    // The app's TL;DR read finds a short turn: no TL;DR, and never
    // "generating" for it, but its headline is made.
    let tldr = format!("/v1/agents/{}/tldr", f.id);
    assert_null(&f.bridge.get_authed(&tldr).await, 200);
    let made = async {
        while fake.headline_calls() == 0 {
            assert_null(&f.bridge.get_authed(&tldr).await, 200);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, made)
        .await
        .expect("headline requested");
    f.bridge
        .wait_for_agents("the card headline", |agents| {
            card_headline(&f, agents).as_deref() == Some(HEADLINE)
        })
        .await;
    assert_null(&f.bridge.get_authed(&tldr).await, 200);
    assert_eq!(fake.headline_calls(), 1);
    assert_eq!(fake.tldr_calls(), 0);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_failed_headline_backs_off_and_the_bridge_retries_it() {
    let fake = Arc::new(FakeSummarizer::new().with_headline(HEADLINE));
    fake.fail_next_headline(summarize::Error::NoStructuredOutput);
    let f = Fixture::start_with(Some(Arc::clone(&fake)), &user("go"), |settings| {
        settings.tldr.retry_after = Duration::from_millis(100);
    })
    .await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.transcript, &assistant("done"));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    // The retry is headline-only: the short turn is never "summarizing".
    let tldr = format!("/v1/agents/{}/tldr", f.id);
    let retried = async {
        while fake.headline_calls() < 2 {
            // Once the first job has measured the turn (it called the
            // model), nothing for this turn is "generating" again.
            let measured = fake.headline_calls() >= 1;
            let response = f.bridge.get_authed(&tldr).await;
            if measured {
                assert_null(&response, 200);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(PATIENCE, retried)
        .await
        .expect("headline retried");
    assert_eq!(
        next_headline(&mut stream).await,
        (TurnIndex(1), HEADLINE.to_owned())
    );
    assert_eq!(fake.headline_calls(), 2);
    assert_eq!(fake.tldr_calls(), 0);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_failing_headline_never_holds_back_the_tldr_and_vice_versa() {
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_headline(HEADLINE)
            .with_tldr(summary()),
    );
    for _ in 0..10 {
        fake.fail_next_headline(summarize::Error::NoStructuredOutput);
    }
    let f = Fixture::start(Some(Arc::clone(&fake)), &user("refactor")).await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.transcript, &long_reply());
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    stream
        .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
        .await;
    assert_eq!(fake.headline_calls(), 1, "the headline was tried too");

    // And a failing TL;DR does not stop the headline.
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_headline(HEADLINE)
            .with_tldr(summary()),
    );
    fake.fail_next(summarize::Error::NoStructuredOutput);
    let g = Fixture::start(Some(Arc::clone(&fake)), &user("refactor")).await;
    let mut other = live_stream(&g.bridge).await;
    append(&g.transcript, &long_reply());
    g.bridge.fake().set_status(&g.pane, AgentStatus::Done);
    assert_eq!(
        next_headline(&mut other).await,
        (TurnIndex(1), HEADLINE.to_owned())
    );
    assert_eq!(fake.tldr_calls(), 1);
    other.close().await;
    stream.close().await;
    g.bridge.stop().await.expect("clean stop");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_card_never_shows_an_older_turns_headline() {
    let fake = Arc::new(FakeSummarizer::new().with_headline(HEADLINE));
    let f = Fixture::start_with(Some(Arc::clone(&fake)), &user("one"), |settings| {
        settings.tldr.retry_after = Duration::from_secs(3600);
    })
    .await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.transcript, &assistant("Shall I push?"));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    next_headline(&mut stream).await;
    f.bridge
        .wait_for_agents("turn 1's headline", |a| {
            card_headline(&f, a).as_deref() == Some(HEADLINE)
        })
        .await;

    // Turn 2 finishes, and its headline fails: turn 1's question is stale.
    fake.fail_next_headline(summarize::Error::NoStructuredOutput);
    append(&f.transcript, &user("yes, push"));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    status_seen(&mut stream, protocol::AgentStatus::Working).await;
    append(&f.transcript, &assistant("Pushed."));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let failed = async {
        while fake.headline_calls() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(PATIENCE, failed)
        .await
        .expect("turn 2 tried");
    f.bridge
        .wait_for_agents("no card headline", |a| card_headline(&f, a).is_none())
        .await;
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

/// A summarizer whose TL;DR is instant and whose headline takes a while.
#[derive(Debug)]
struct SlowHeadline;

impl Summarizer for SlowHeadline {
    fn tldr(&self, _turn: &TurnDigest) -> impl Future<Output = summarize::Result<Tldr>> + Send {
        std::future::ready(Ok(summary()))
    }

    async fn headline(&self, _turn: &TurnDigest) -> summarize::Result<Headline> {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Headline::new(HEADLINE).ok_or(summarize::Error::NoStructuredOutput)
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
async fn a_made_tldr_is_served_while_the_headline_is_still_being_made() {
    let summarizer = SharedSummarizer::new(Arc::new(SlowHeadline) as Arc<dyn DynSummarizer>);
    let f = Fixture::start_with(None, &user("refactor"), |settings| {
        settings.tldr.summarizer = Some(summarizer);
    })
    .await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.transcript, &long_reply());
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    stream
        .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
        .await;
    // The headline call is still running: the TL;DR is not "generating".
    let response = f
        .bridge
        .get_authed(&format!("/v1/agents/{}/tldr", f.id))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        serde_json::from_str::<Tldr>(&response.body).expect("tldr"),
        summary()
    );
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_turn_that_goes_on_after_finishing_gets_a_new_headline() {
    // The circus master's turns rarely end: task notifications and queued
    // prompts continue a turn after it was seen finished. Its headline
    // must follow, not describe only the first stretch.
    let fake = Arc::new(FakeSummarizer::new());
    let f = Fixture::start(Some(Arc::clone(&fake)), &user("run the swarm")).await;
    let mut stream = live_stream(&f.bridge).await;
    append(&f.transcript, &assistant("Started three agents."));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let (turn, first) = next_headline(&mut stream).await;
    assert_eq!(turn, TurnIndex(1));

    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    status_seen(&mut stream, protocol::AgentStatus::Working).await;
    append(
        &f.transcript,
        &assistant(&"merged another pull request ".repeat(4)),
    );
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let (turn, second) = next_headline(&mut stream).await;
    assert_eq!(turn, TurnIndex(1), "the same turn");
    assert_ne!(first, second, "made from the whole turn");
    assert_eq!(fake.headline_calls(), 2);
    assert_eq!(
        card_headline(&f, &f.bridge.agents().await).as_deref(),
        Some(second.as_str())
    );
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

const DONE_SESSION: &str = "3c9e1f0a-7b2d-4e8f-9a61-5d4c3b2a1f0e";

/// A Claude agent named `name` on `herdr`, in a workspace rooted at
/// `cwd`, in `session`, with `status`.
async fn agent_in(
    herdr: &CuttableHerdr,
    name: &str,
    cwd: &Path,
    session: &str,
    status: AgentStatus,
) -> PaneId {
    let pane = start_agent(herdr, name, "claude", cwd).await;
    herdr.fake.set_session(
        &pane,
        AgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            kind: "id".into(),
            value: session.into(),
        },
    );
    herdr.fake.set_status(&pane, status);
    pane
}

/// A working directory under `root` named `name`, with a transcript of
/// `session` holding `text` in `projects`; returns the directory and the
/// transcript.
fn workdir(
    root: &Path,
    projects: &Path,
    name: &str,
    session: &str,
    text: &str,
) -> (PathBuf, PathBuf) {
    let cwd = root.join(name);
    let project = projects.join(transcripts::project_slug(&cwd));
    fs::create_dir_all(&cwd).expect("cwd");
    fs::create_dir_all(&project).expect("project dir");
    let transcript = project.join(format!("{session}.jsonl"));
    append(&transcript, text);
    (cwd, transcript)
}

/// A bridge on `herdr` reading transcripts from `projects`, with `fake`.
async fn bridge_with(
    herdr: CuttableHerdr,
    projects: PathBuf,
    fake: &Arc<FakeSummarizer>,
) -> Bridge {
    bridge_adjusted(herdr, projects, fake, |_| {}).await
}

/// [`bridge_with`], adjusting the settings too.
async fn bridge_adjusted(
    herdr: CuttableHerdr,
    projects: PathBuf,
    fake: &Arc<FakeSummarizer>,
    adjust: impl FnOnce(&mut bridge::Settings),
) -> Bridge {
    let summarizer = SharedSummarizer::new(Arc::clone(fake) as Arc<dyn DynSummarizer>);
    Bridge::start_with(herdr, |settings| {
        settings.threads.projects_dir = Some(projects);
        settings.tldr.summarizer = Some(summarizer);
        settings.tldr.settle = Duration::from_millis(10);
        adjust(settings);
    })
    .await
}

fn pane_headline(agents: &[protocol::Agent], pane: &PaneId) -> Option<String> {
    agents
        .iter()
        .find(|a| a.id.pane().as_str() == pane.as_str() && a.id.session().is_some())
        .and_then(|a| a.headline.clone())
}

#[tokio::test]
async fn an_agent_already_done_when_the_bridge_starts_gets_its_card_headline() {
    let tmp = tempfile::tempdir().expect("tmp");
    let projects = tmp.path().join("projects");
    // A long turn, finished before the bridge started.
    let text = user("refactor the parser") + &long_reply();
    let (cwd, _) = workdir(tmp.path(), &projects, "repo", DONE_SESSION, &text);
    let herdr = CuttableHerdr::new();
    let pane = agent_in(&herdr, "worker", &cwd, DONE_SESSION, AgentStatus::Done).await;
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_headline(HEADLINE)
            .with_tldr(summary())
            .with_delay(Duration::from_millis(100)),
    );
    let bridge = bridge_with(herdr, projects, &fake).await;

    // A client connected before the headline lands sees the card update
    // live: the agent is re-sent with it, its state unchanged.
    let mut stream = bridge.stream().await;
    let AgentEvent::Snapshot { agents } = stream
        .wait_for("snapshot", |e| matches!(e, AgentEvent::Snapshot { .. }))
        .await
    else {
        unreachable!("matched above");
    };
    if pane_headline(&agents, &pane).is_none() {
        stream.wait_for("headline_ready", headline_ready).await;
        let updated = stream
            .wait_for("agent_updated with the headline", |e| {
                matches!(e, AgentEvent::AgentUpdated { agent: Some(a) }
                    if a.headline.as_deref() == Some(HEADLINE))
            })
            .await;
        let AgentEvent::AgentUpdated { agent: Some(agent) } = updated else {
            unreachable!("matched above");
        };
        assert_eq!(agent.status, protocol::AgentStatus::Done);
    }
    bridge
        .wait_for_agents("the backfilled card headline", |agents| {
            pane_headline(agents, &pane).as_deref() == Some(HEADLINE)
        })
        .await;
    // A headline only: TL;DRs of turns the bridge did not see finish are
    // made on request.
    assert_eq!(fake.headline_calls(), 1);
    assert_eq!(fake.tldr_calls(), 0);

    // The same session showing up in another pane, done, is not
    // headlined again: its turn has one, and its card shows it.
    let moved = agent_in(
        &bridge.herdr,
        "mover",
        &cwd,
        DONE_SESSION,
        AgentStatus::Done,
    )
    .await;
    bridge
        .wait_for_agents("the moved agent's card headline", |agents| {
            pane_headline(agents, &moved).as_deref() == Some(HEADLINE)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fake.headline_calls(), 1);
    assert_eq!(fake.tldr_calls(), 0);
    stream.close().await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_turn_finishing_live_goes_ahead_of_the_backfill() {
    let tmp = tempfile::tempdir().expect("tmp");
    let projects = tmp.path().join("projects");
    let herdr = CuttableHerdr::new();
    // Three agents done before the bridge started, each in its own
    // session: three headlines to backfill.
    for n in 0..3 {
        let session = format!("3c9e1f0a-7b2d-4e8f-9a61-5d4c3b2a1f0{n}");
        let text = user("go") + &assistant("done");
        let (cwd, _) = workdir(tmp.path(), &projects, &format!("idle{n}"), &session, &text);
        agent_in(
            &herdr,
            &format!("idle{n}"),
            &cwd,
            &session,
            AgentStatus::Done,
        )
        .await;
    }
    // And one at work.
    let (cwd, transcript) = workdir(tmp.path(), &projects, "live", DONE_SESSION, &user("fix it"));
    let live = agent_in(&herdr, "live", &cwd, DONE_SESSION, AgentStatus::Working).await;
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_headline(HEADLINE)
            .with_delay(Duration::from_millis(300)),
    );
    let bridge = bridge_with(herdr, projects, &fake).await;
    let mut stream = live_stream(&bridge).await;

    append(&transcript, &assistant("Fixed."));
    bridge.fake().set_status(&live, AgentStatus::Done);
    // At most the backfilled headline already being made goes first (or
    // two, should that one end in the moment before the live turn's job
    // is queued). Backfilling all at once would put all three first.
    let mut before = 0;
    loop {
        let AgentEvent::HeadlineReady { agent_id, .. } =
            stream.wait_for("headline_ready", headline_ready).await
        else {
            unreachable!("matched above");
        };
        if agent_id.pane().as_str() == live.as_str() {
            break;
        }
        before += 1;
    }
    assert!(before <= 2, "{before} backfilled headlines went first");
    // The rest are backfilled after it.
    bridge
        .wait_for_agents("every card headline", |agents| {
            agents.iter().filter(|a| a.headline.is_some()).count() == 4
        })
        .await;
    assert_eq!(fake.headline_calls(), 4);
    stream.close().await;
    bridge.stop().await.expect("clean stop");
}

/// Polls until `done` holds.
async fn until(what: &str, done: impl Fn() -> bool) {
    let poll = async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(PATIENCE, poll)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn the_backfill_waits_out_a_usage_limit_and_retries_headlines_only() {
    let tmp = tempfile::tempdir().expect("tmp");
    let projects = tmp.path().join("projects");
    let herdr = CuttableHerdr::new();
    // Two long turns finished before the bridge started.
    for n in 0..2 {
        let session = format!("3c9e1f0a-7b2d-4e8f-9a61-5d4c3b2a1f0{n}");
        let text = user("refactor") + &long_reply();
        let (cwd, _) = workdir(tmp.path(), &projects, &format!("idle{n}"), &session, &text);
        agent_in(
            &herdr,
            &format!("idle{n}"),
            &cwd,
            &session,
            AgentStatus::Done,
        )
        .await;
    }
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_headline(HEADLINE)
            .with_tldr(summary()),
    );
    fake.fail_next_headline(summarize::Error::UsageLimit {
        message: "limit".into(),
    });
    let pause = Duration::from_millis(600);
    let bridge = bridge_adjusted(herdr, projects, &fake, |settings| {
        settings.tldr.retry_after = Duration::from_millis(50);
        settings.tldr.max_retry_after = pause;
    })
    .await;

    // The first backfilled headline hits the limit: nothing else is
    // backfilled while it lasts.
    until("the first headline call", || fake.headline_calls() >= 1).await;
    let limited_at = std::time::Instant::now();
    tokio::time::sleep(pause / 3).await;
    if limited_at.elapsed() < pause {
        assert_eq!(fake.headline_calls(), 1, "backfilled during the limit");
    }
    // Then both are made, the failed one retried, as headlines only.
    bridge
        .wait_for_agents("both card headlines", |agents| {
            agents
                .iter()
                .filter(|a| a.headline.as_deref() == Some(HEADLINE))
                .count()
                == 2
        })
        .await;
    assert!(limited_at.elapsed() >= pause / 2, "waited for the limit");
    assert_eq!(fake.headline_calls(), 3);
    assert_eq!(fake.tldr_calls(), 0);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_tldr_asked_for_during_a_backfill_is_made() {
    let tmp = tempfile::tempdir().expect("tmp");
    let projects = tmp.path().join("projects");
    let text = user("refactor the parser") + &long_reply();
    let (cwd, _) = workdir(tmp.path(), &projects, "repo", DONE_SESSION, &text);
    let herdr = CuttableHerdr::new();
    agent_in(&herdr, "worker", &cwd, DONE_SESSION, AgentStatus::Done).await;
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_headline(HEADLINE)
            .with_tldr(summary())
            .with_delay(Duration::from_millis(300)),
    );
    let bridge = bridge_with(herdr, projects, &fake).await;
    let agents = bridge
        .wait_for_agents("the agent", |a| a.iter().any(|a| a.id.session().is_some()))
        .await;
    let id = agents
        .iter()
        .find(|a| a.id.session().is_some())
        .map(|a| a.id.to_string())
        .expect("agent");
    let mut stream = live_stream(&bridge).await;

    // The backfill job is making the headline (it has decided: no TL;DR)
    // when the app opens the turn's TL;DR: it is generating, not "none".
    until("the backfilled headline call", || {
        fake.headline_calls() >= 1
    })
    .await;
    let tldr = format!("/v1/agents/{id}/tldr");
    let response = bridge.get_authed(&tldr).await;
    assert_eq!(response.status, 202, "{}", response.body);
    stream
        .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
        .await;
    let response = bridge.get_authed(&tldr).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        serde_json::from_str::<Tldr>(&response.body).expect("tldr"),
        summary()
    );
    assert_eq!(fake.tldr_calls(), 1);
    assert_eq!(fake.headline_calls(), 1, "the headline is not made twice");
    stream.close().await;
    bridge.stop().await.expect("clean stop");
}
