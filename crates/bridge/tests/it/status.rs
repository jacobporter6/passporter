//! Live status lines of working agents: the periodic check (throttle and
//! growth threshold), taps on `POST /v1/agents/:id/status` (rate limit) and
//! the usage-limit pause, with `FakeSummarizer` in place of `claude -p`.

use std::sync::Arc;
use std::time::Duration;

use bridge::tldr::StatusSettings;
use herdr::AgentStatus;
use protocol::{AgentEvent, StatusLine};
use summarize::FakeSummarizer;

use crate::harness::{HttpResponse, Stream};
use crate::tldr::{Fixture, append, assistant, live_stream, user};

const LINE: &str = "Running the test suite; waiting on CI";

/// Short intervals, so the throttle shows in a test: a periodic line at
/// most every `EVERY`, checked every 20 ms, taps at most every `TAP_EVERY`.
const EVERY: Duration = Duration::from_millis(500);
const TAP_EVERY: Duration = Duration::from_millis(400);

fn fast(every: Duration) -> impl FnOnce(&mut bridge::Settings) {
    move |settings| {
        settings.tldr.status = StatusSettings {
            every,
            check_every: Duration::from_millis(20),
            tap_every: TAP_EVERY,
            // Long: these tests are about the line (a turn in progress
            // still gets its first provisional headline at once).
            headline_every: Duration::from_secs(3600),
        };
    }
}

/// `n` short assistant messages: `n` new thread items.
fn messages(n: usize) -> String {
    (0..n).map(|i| assistant(&format!("step {i}"))).collect()
}

async fn next_line(stream: &mut Stream<tokio::net::TcpStream>) -> StatusLine {
    let event = stream
        .wait_for("status_line_ready", |e| {
            matches!(e, AgentEvent::StatusLineReady { .. })
        })
        .await;
    let AgentEvent::StatusLineReady {
        agent_id: _,
        status_line,
    } = event
    else {
        unreachable!("matched above");
    };
    status_line
}

async fn tap(f: &Fixture) -> HttpResponse {
    f.bridge
        .post_authed(&format!("/v1/agents/{}/status", f.id), "")
        .await
}

fn listed_line(f: &Fixture, agents: &[protocol::Agent]) -> Option<StatusLine> {
    agents
        .iter()
        .find(|a| a.id.to_string() == f.id)
        .and_then(|a| a.status_line.clone())
}

#[tokio::test]
async fn a_line_waits_for_growth_and_is_served_with_the_working_agent() {
    let fake = Arc::new(FakeSummarizer::new().with_status(LINE));
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &user("run the long job"),
        fast(EVERY),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;

    // One item (the prompt) is not enough to say anything.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.status_calls(), 0);
    append(&f.transcript, &messages(1));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.status_calls(), 0, "two items are still too few");

    // A third item is meaningful growth.
    append(&f.transcript, &messages(1));
    let line = next_line(&mut stream).await;
    assert_eq!(line.text, LINE);
    assert!(line.at.unix_millis() > 0);
    // The agent is re-sent with it, still working.
    stream
        .wait_for("agent_updated with the line", |e| {
            matches!(e, AgentEvent::AgentUpdated { agent: Some(a) }
                if a.status_line.as_ref() == Some(&line)
                    && a.status == protocol::AgentStatus::Working)
        })
        .await;
    assert_eq!(listed_line(&f, &f.bridge.agents().await), Some(line));
    assert_eq!(fake.status_calls(), 1);

    // Once the turn finishes, the headline takes over: no line.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    stream
        .wait_for("agent_updated without the line", |e| {
            matches!(e, AgentEvent::AgentUpdated { agent: Some(a) }
                if a.status == protocol::AgentStatus::Done && a.status_line.is_none())
        })
        .await;
    assert_eq!(listed_line(&f, &f.bridge.agents().await), None);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn periodic_lines_are_throttled_and_skip_a_turn_that_has_not_grown() {
    let fake = Arc::new(FakeSummarizer::new().with_status(LINE));
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("go") + &messages(3)),
        fast(EVERY),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    let first = next_line(&mut stream).await;
    assert_eq!(fake.status_calls(), 1);

    // Grown, but within the interval: nothing yet.
    fake.set_status("Fixing the failing test");
    append(&f.transcript, &messages(3));
    tokio::time::sleep(EVERY / 3).await;
    assert_eq!(fake.status_calls(), 1, "at most one line per interval");

    // Once the interval has passed, the grown turn gets a new line.
    let second = next_line(&mut stream).await;
    assert_eq!(second.text, "Fixing the failing test");
    assert!(second.at >= first.at);
    assert_eq!(fake.status_calls(), 2);

    // A turn that has not grown is not sent to the model again.
    tokio::time::sleep(EVERY * 3).await;
    assert_eq!(fake.status_calls(), 2);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn taps_make_a_line_now_but_are_rate_limited_per_agent() {
    let fake = Arc::new(FakeSummarizer::new().with_status(LINE));
    // No periodic line during the test: the interval is long and the
    // turn stays below the growth threshold.
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &user("go"),
        fast(Duration::from_secs(3600)),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;

    // A tap skips the growth threshold.
    assert_eq!(tap(&f).await.status, 202);
    assert_eq!(next_line(&mut stream).await.text, LINE);
    assert_eq!(fake.status_calls(), 1);

    // Again at once: refused, with when to retry.
    let refused = tap(&f).await;
    assert_eq!(refused.status, 429, "{}", refused.body);
    assert_eq!(refused.header("retry-after"), Some("1"));

    // After the pause, a tap on a changed turn makes a new line, ...
    tokio::time::sleep(TAP_EVERY).await;
    append(&f.transcript, &messages(1));
    assert_eq!(tap(&f).await.status, 202);
    next_line(&mut stream).await;
    assert_eq!(fake.status_calls(), 2);

    // ... and one on an unchanged turn is answered at once: the line is
    // current, and no call is made.
    tokio::time::sleep(TAP_EVERY).await;
    assert_eq!(tap(&f).await.status, 200);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.status_calls(), 2);

    // An agent that is not working has nothing to refresh.
    tokio::time::sleep(TAP_EVERY).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    f.bridge
        .wait_for_agents("done", |a| {
            a.iter().all(|a| a.status == protocol::AgentStatus::Done)
        })
        .await;
    assert_eq!(tap(&f).await.status, 204);
    assert_eq!(
        f.bridge
            .post_authed("/v1/agents/w9:p9/status", "")
            .await
            .status,
        404
    );
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_usage_limit_pauses_status_lines() {
    let fake = Arc::new(FakeSummarizer::new().with_status(LINE));
    fake.fail_next_status(summarize::Error::UsageLimit {
        message: "limit".into(),
    });
    let f = Fixture::start_with(Some(Arc::clone(&fake)), &user("go"), |settings| {
        fast(EVERY)(settings);
        settings.tldr.max_retry_after = Duration::from_secs(3600);
    })
    .await;

    // The first try hits the limit.
    assert_eq!(tap(&f).await.status, 202);
    let wait = async {
        while fake.status_calls() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(crate::harness::PATIENCE, wait)
        .await
        .expect("the tap's call");

    // From then on nothing is made: no periodic line however the turn
    // grows, and a tap is told how long the pause lasts.
    append(&f.transcript, &messages(10));
    tokio::time::sleep(EVERY * 2).await;
    assert_eq!(fake.status_calls(), 1);
    let paused = tap(&f).await;
    assert_eq!(paused.status, 503, "{}", paused.body);
    let retry: u64 = paused
        .header("retry-after")
        .and_then(|v| v.parse().ok())
        .expect("retry-after");
    assert!(retry > 3000, "{retry}");
    assert_eq!(fake.status_calls(), 1);
    assert_eq!(listed_line(&f, &f.bridge.agents().await), None);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_previous_turn_is_never_described_as_the_new_one() {
    let fake = Arc::new(FakeSummarizer::new().with_status(LINE));
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("old job") + &messages(5)),
        fast(Duration::from_secs(3600)),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    // Turn 1 finishes; the TL;DR watcher records it (its headline is made).
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    stream
        .wait_for("the final headline", |e| {
            matches!(
                e,
                AgentEvent::HeadlineReady {
                    provisional: false,
                    ..
                }
            )
        })
        .await;

    // Working again, but the new prompt has not reached the transcript:
    // the thread's current turn is still the finished one.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    f.bridge
        .wait_for_agents("working", |a| {
            a.iter().all(|a| a.status == protocol::AgentStatus::Working)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(tap(&f).await.status, 200, "nothing in progress yet");
    assert_eq!(fake.status_calls(), 0);

    // Once the prompt lands, the new turn is described.
    append(&f.transcript, &(user("new job") + &messages(1)));
    tokio::time::sleep(TAP_EVERY).await;
    assert_eq!(tap(&f).await.status, 202);
    next_line(&mut stream).await;
    assert_eq!(fake.status_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_tap_during_a_running_job_is_answered_after_it() {
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_status(LINE)
            .with_delay(Duration::from_millis(300)),
    );
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("go") + &messages(3)),
        fast(Duration::from_secs(3600)),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    // The periodic job is making its line ...
    let busy = async {
        while fake.status_calls() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(crate::harness::PATIENCE, busy)
        .await
        .expect("the periodic call");
    // ... when the turn grows a little and the user taps.
    append(&f.transcript, &messages(1));
    assert_eq!(tap(&f).await.status, 202);
    next_line(&mut stream).await;
    // The tap is not lost: a second line follows for the grown turn,
    // though the periodic threshold and interval would not allow one.
    next_line(&mut stream).await;
    assert_eq!(fake.status_calls(), 2);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_finished_turn_that_goes_on_gets_status_lines_again() {
    // A long-running agent (the circus master) finishes a turn, then a
    // task notification or a queued prompt sets it working again: no new
    // typed prompt, so the thread's current turn is still the finished
    // one, and it grows.
    let fake = Arc::new(FakeSummarizer::new().with_status(LINE));
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("run the swarm") + &messages(5)),
        fast(EVERY),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    stream
        .wait_for("the final headline", |e| {
            matches!(
                e,
                AgentEvent::HeadlineReady {
                    provisional: false,
                    ..
                }
            )
        })
        .await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    f.bridge
        .wait_for_agents("working", |a| {
            a.iter().all(|a| a.status == protocol::AgentStatus::Working)
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.status_calls(), 0, "nothing new in the turn yet");

    // The turn goes on: the periodic check describes it, with no tap.
    append(&f.transcript, &messages(3));
    assert_eq!(next_line(&mut stream).await.text, LINE);
    assert_eq!(fake.status_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}
