//! `GET /v1/agents/:id/tldr?turn=<n>` on request (the app's "Summarise"):
//! short turns and the turn in progress get a TL;DR too, one on-demand job
//! at a time, rate-limited per device, cached by the turn's size.

use std::sync::Arc;
use std::time::Duration;

use bridge::auth::RateLimit;
use herdr::AgentStatus;
use protocol::{AgentEvent, Tldr, TurnIndex};
use summarize::FakeSummarizer;

use crate::harness::{HttpResponse, Stream};
use crate::tldr::{
    Fixture, append, assert_null, assistant, live_stream, long_reply, status_seen, summary, user,
};

/// Waits for the next `tldr_ready`; returns its turn and whether it is
/// provisional.
async fn ready_event(stream: &mut Stream<tokio::net::TcpStream>) -> (TurnIndex, bool) {
    match stream
        .wait_for("tldr_ready", |e| matches!(e, AgentEvent::TldrReady { .. }))
        .await
    {
        AgentEvent::TldrReady {
            turn,
            tldr,
            provisional,
            ..
        } => {
            assert_eq!(tldr, summary());
            (turn, provisional)
        }
        _ => unreachable!("matched above"),
    }
}

fn served(response: &HttpResponse) -> Tldr {
    assert_eq!(response.status, 200, "{}", response.body);
    serde_json::from_str(&response.body).expect("tldr")
}

#[tokio::test]
async fn a_short_finished_turn_gets_a_tldr_on_request_cached_by_its_size() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let f = Fixture::start(Some(Arc::clone(&fake)), &(user("hi") + &assistant("hello"))).await;
    let mut stream = live_stream(&f.bridge).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    status_seen(&mut stream, protocol::AgentStatus::Done).await;
    // Short: the latest-turn read has none...
    assert_null(&f.get("").await, 200);
    // ...but asking for it by number makes one.
    assert_null(&f.get("?turn=1").await, 202);
    assert_eq!(ready_event(&mut stream).await, (TurnIndex(1), false));
    assert_eq!(served(&f.get("?turn=1").await), summary());
    // Cached per (session, turn, size), like other TL;DRs: the latest-turn
    // read serves it too, without another model call.
    assert_eq!(served(&f.get("").await), summary());
    assert_eq!(fake.tldr_calls(), 1);

    // The turn goes on (a task notification) and finishes again: a new
    // size is a new key, so the next request makes a new one.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    status_seen(&mut stream, protocol::AgentStatus::Working).await;
    append(&f.transcript, &assistant("one more thing"));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    status_seen(&mut stream, protocol::AgentStatus::Done).await;
    assert_null(&f.get("?turn=1").await, 202);
    assert_eq!(ready_event(&mut stream).await, (TurnIndex(1), false));
    assert_eq!(fake.tldr_calls(), 2);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_turn_in_progress_gets_a_provisional_tldr_that_refreshes() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let fresh_for = Duration::from_millis(400);
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("refactor the parser") + &assistant("looking at it")),
        |settings| settings.tldr.provisional_for = fresh_for,
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    assert_null(&f.get("?turn=1").await, 202);
    assert_eq!(ready_event(&mut stream).await, (TurnIndex(1), true));
    // It grows; reads meanwhile (e.g. prompted by that `tldr_ready`) get
    // the provisional one rather than starting another.
    append(&f.transcript, &assistant("found the bug"));
    assert_eq!(served(&f.get("?turn=1").await), summary());
    assert_eq!(fake.tldr_calls(), 1);
    // Later, a request refreshes it from the turn so far.
    tokio::time::sleep(fresh_for).await;
    assert_null(&f.get("?turn=1").await, 202);
    assert_eq!(ready_event(&mut stream).await, (TurnIndex(1), true));
    assert_eq!(fake.tldr_calls(), 2);
    // Once the turn finishes, a request makes the final one.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    status_seen(&mut stream, protocol::AgentStatus::Done).await;
    assert_null(&f.get("?turn=1").await, 202);
    assert_eq!(ready_event(&mut stream).await, (TurnIndex(1), false));
    assert_eq!(served(&f.get("?turn=1").await), summary());
    assert_eq!(fake.tldr_calls(), 3);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn one_requested_turn_is_generated_at_a_time() {
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_tldr(summary())
            .with_delay(Duration::from_millis(300)),
    );
    let f = Fixture::start(
        Some(Arc::clone(&fake)),
        &(user("one") + &assistant("a") + &user("two") + &assistant("b")),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    assert_null(&f.get("?turn=1").await, 202);
    // Polls while it is made are "generating", not another job.
    assert_null(&f.get("?turn=1").await, 202);
    // Another turn waits its turn: the in-progress one too.
    let busy = f.get("?turn=2").await;
    assert_eq!(busy.status, 429, "{}", busy.body);
    assert_eq!(busy.header("retry-after"), Some("30"));
    assert_eq!(busy.body, "null");
    assert_eq!(ready_event(&mut stream).await, (TurnIndex(1), false));
    // With turn 1 done, turn 2 may go.
    assert_null(&f.get("?turn=2").await, 202);
    assert_eq!(ready_event(&mut stream).await, (TurnIndex(2), true));
    assert_eq!(fake.tldr_calls(), 2);
    // No such turn (turn 0 is never one) has none.
    assert_null(&f.get("?turn=3").await, 200);
    assert_null(&f.get("?turn=0").await, 200);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn requests_that_start_a_generation_are_rate_limited_per_device() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("one") + &assistant("a") + &user("two") + &assistant("b")),
        |settings| settings.tldr.request_rate = RateLimit::per_minute(1),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    assert_null(&f.get("?turn=1").await, 202);
    assert_eq!(ready_event(&mut stream).await, (TurnIndex(1), false));
    // Cache hits are free...
    assert_eq!(served(&f.get("?turn=1").await), summary());
    // ...a second generation is not.
    let limited = f.get("?turn=2").await;
    assert_eq!(limited.status, 429, "{}", limited.body);
    let wait: u64 = limited
        .header("retry-after")
        .and_then(|secs| secs.parse().ok())
        .expect("retry-after seconds");
    assert!((1..=60).contains(&wait), "{wait}");
    assert_eq!(fake.tldr_calls(), 1);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_failed_provisional_tldr_backs_off_without_costing_the_final_one() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    fake.fail_next(summarize::Error::NoStructuredOutput);
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("refactor the parser") + &assistant("looking at it")),
        |settings| settings.tldr.retry_after = Duration::from_secs(3600),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    assert_null(&f.get("?turn=1").await, 202);
    let failed = async {
        while f.get("?turn=1").await.status == 202 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(crate::harness::PATIENCE, failed)
        .await
        .expect("generation ended");
    // Backing off: "none" (the app stops asking), and the model is not
    // run again.
    assert_null(&f.get("?turn=1").await, 200);
    assert_eq!(fake.tldr_calls(), 1);
    // The turn finishes long: its eager TL;DR is not held back by the
    // failed provisional one.
    append(&f.transcript, &long_reply());
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    assert_eq!(ready_event(&mut stream).await, (TurnIndex(1), false));
    assert_eq!(fake.tldr_calls(), 2);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_just_finished_turn_asked_for_waits_to_settle_without_taking_the_slot() {
    let fake = Arc::new(FakeSummarizer::new().with_tldr(summary()));
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("zero") + &assistant("z") + &user("one") + &assistant("working on it")),
        |settings| settings.tldr.settle = Duration::from_millis(300),
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    status_seen(&mut stream, protocol::AgentStatus::Done).await;
    // Turn 2 looks short until its last lines are written: it waits on
    // the eager path, not in the on-demand slot...
    assert_null(&f.get("?turn=2").await, 202);
    // ...which stays free for another turn.
    assert_null(&f.get("?turn=1").await, 202);
    append(&f.transcript, &long_reply());
    let mut turns = vec![
        ready_event(&mut stream).await,
        ready_event(&mut stream).await,
    ];
    turns.sort();
    assert_eq!(turns, [(TurnIndex(1), false), (TurnIndex(2), false)]);
    assert_eq!(fake.tldr_calls(), 2);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}
