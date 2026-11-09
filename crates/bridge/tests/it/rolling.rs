//! Rolling headlines of the turn in progress: made by the status-line job
//! from the turn so far (throttle and growth threshold), served and
//! announced marked provisional, replaced by the final headline when the
//! turn finishes, and held back by the usage-limit pause, with
//! `FakeSummarizer` in place of `claude -p`.

use std::sync::Arc;
use std::time::Duration;

use bridge::tldr::StatusSettings;
use herdr::AgentStatus;
use protocol::{AgentEvent, ThreadPage, TurnHeadline, TurnIndex};
use summarize::FakeSummarizer;

use crate::harness::{PATIENCE, Stream};
use crate::tldr::{Fixture, append, assistant, live_stream, user};

/// A provisional headline at most this often (status lines: once, then
/// not again during a test).
const EVERY: Duration = Duration::from_millis(500);

fn fast(settings: &mut bridge::Settings) {
    settings.tldr.status = StatusSettings {
        every: Duration::from_secs(3600),
        check_every: Duration::from_millis(20),
        tap_every: Duration::from_secs(3600),
        headline_every: EVERY,
    };
}

/// `n` short assistant messages: `n` new thread items.
fn messages(n: usize) -> String {
    (0..n).map(|i| assistant(&format!("step {i}"))).collect()
}

/// Waits for a `headline_ready`, returning its turn, text and whether it
/// is provisional.
async fn next_headline(stream: &mut Stream<tokio::net::TcpStream>) -> (TurnIndex, String, bool) {
    let event = stream
        .wait_for("headline_ready", |e| {
            matches!(e, AgentEvent::HeadlineReady { .. })
        })
        .await;
    let AgentEvent::HeadlineReady {
        turn,
        headline,
        provisional,
        ..
    } = event
    else {
        unreachable!("matched above");
    };
    (turn, headline, provisional)
}

async fn page_headlines(f: &Fixture) -> Vec<TurnHeadline> {
    let response = f
        .bridge
        .get_authed(&format!("/v1/agents/{}/thread?since=0", f.id))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    serde_json::from_str::<ThreadPage>(&response.body)
        .expect("thread page")
        .headlines
}

/// The agent's card headline in the agent list.
async fn card_headline(f: &Fixture) -> Option<String> {
    f.bridge
        .agents()
        .await
        .into_iter()
        .find(|a| a.id.to_string() == f.id)
        .and_then(|a| a.headline)
}

async fn wait_for_calls(fake: &FakeSummarizer, calls: usize) {
    let wait = async {
        while fake.headline_calls() < calls {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait)
        .await
        .expect("the headline call");
}

#[tokio::test]
async fn the_turn_in_progress_gets_a_throttled_provisional_headline() {
    let fake = Arc::new(FakeSummarizer::new().with_headline("Starting the swarm."));
    let f = Fixture::start_with(Some(Arc::clone(&fake)), &user("run the swarm"), fast).await;
    let mut stream = live_stream(&f.bridge).await;

    // The prompt alone, then two items: not yet meaningful growth.
    tokio::time::sleep(Duration::from_millis(200)).await;
    append(&f.transcript, &messages(1));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.headline_calls(), 0);

    // A third item is.
    append(&f.transcript, &messages(1));
    assert_eq!(
        next_headline(&mut stream).await,
        (TurnIndex(1), "Starting the swarm.".to_owned(), true)
    );
    assert_eq!(
        page_headlines(&f).await,
        [TurnHeadline {
            turn: TurnIndex(1),
            text: "Starting the swarm.".into(),
            provisional: true,
        }]
    );
    assert_eq!(
        card_headline(&f).await,
        None,
        "the card shows only final ones"
    );
    assert_eq!(fake.headline_calls(), 1);

    // Grown, but within the interval: nothing yet.
    let grown = std::time::Instant::now();
    fake.set_headline("Merging the swarm's PRs.");
    append(&f.transcript, &messages(3));
    tokio::time::sleep(EVERY / 3).await;
    assert_eq!(fake.headline_calls(), 1, "at most one per interval");

    // After it, the grown turn gets a new one.
    let (_, text, provisional) = next_headline(&mut stream).await;
    assert_eq!(
        (text.as_str(), provisional),
        ("Merging the swarm's PRs.", true)
    );
    assert!(grown.elapsed() >= EVERY / 2, "{:?}", grown.elapsed());
    assert_eq!(fake.headline_calls(), 2);

    // A turn that has not grown is not sent to the model again.
    tokio::time::sleep(EVERY * 3).await;
    assert_eq!(fake.headline_calls(), 2);
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_final_headline_replaces_the_provisional_one() {
    let fake = Arc::new(FakeSummarizer::new().with_headline("Merging PRs so far."));
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("run the swarm") + &messages(3)),
        fast,
    )
    .await;
    let mut stream = live_stream(&f.bridge).await;
    let (turn, _, provisional) = next_headline(&mut stream).await;
    assert!(provisional);

    // The turn finishes: its job makes the final headline, though the
    // provisional one was made from almost all of it.
    fake.set_headline("Merged four PRs.");
    append(&f.transcript, &assistant("All merged."));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    assert_eq!(
        next_headline(&mut stream).await,
        (turn, "Merged four PRs.".to_owned(), false)
    );
    assert_eq!(
        page_headlines(&f).await,
        [TurnHeadline {
            turn,
            text: "Merged four PRs.".into(),
            provisional: false,
        }]
    );
    assert_eq!(fake.headline_calls(), 2);

    // The agent goes on in the same turn (a task notification): a new
    // provisional headline takes over once the turn has grown again ...
    fake.set_headline("Merging the next batch.");
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    f.bridge
        .wait_for_agents("working", |a| {
            a.iter().all(|a| a.status == protocol::AgentStatus::Working)
        })
        .await;
    tokio::time::sleep(EVERY).await;
    assert_eq!(fake.headline_calls(), 2, "the final headline is current");
    append(&f.transcript, &messages(3));
    assert_eq!(
        next_headline(&mut stream).await,
        (turn, "Merging the next batch.".to_owned(), true)
    );
    // The card keeps the final headline, which is kept beside it.
    assert_eq!(card_headline(&f).await.as_deref(), Some("Merged four PRs."));
    // ... and is replaced again when it finishes.
    fake.set_headline("Merged the next batch.");
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    assert_eq!(
        next_headline(&mut stream).await,
        (turn, "Merged the next batch.".to_owned(), false)
    );
    stream.close().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_usage_limit_pauses_provisional_headlines() {
    let fake = Arc::new(FakeSummarizer::new());
    fake.fail_next_headline(summarize::Error::UsageLimit {
        message: "limit".into(),
    });
    let f = Fixture::start_with(
        Some(Arc::clone(&fake)),
        &(user("run the swarm") + &messages(3)),
        |settings| {
            fast(settings);
            settings.tldr.max_retry_after = Duration::from_secs(3600);
        },
    )
    .await;

    // The first provisional headline hits the limit.
    wait_for_calls(&fake, 1).await;

    // From then on nothing is made however the turn grows: no headline,
    // no status line.
    let lines = fake.status_calls();
    append(&f.transcript, &messages(10));
    tokio::time::sleep(EVERY * 3).await;
    assert_eq!(fake.headline_calls(), 1);
    assert_eq!(fake.status_calls(), lines);
    assert_eq!(page_headlines(&f).await, []);
    f.bridge.stop().await.expect("clean stop");
}
