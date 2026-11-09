//! `GET /v1/agents/:id/suggestions`, with `FakeSummarizer` in place of
//! `claude -p`.

use std::sync::Arc;
use std::time::Duration;

use herdr::AgentStatus;
use protocol::{MAX_SUGGESTION_CHARS, MAX_SUGGESTIONS};
use summarize::FakeSummarizer;

use crate::harness::{HttpResponse, PATIENCE};
use crate::tldr::{Fixture, append, assistant, user};

const QUESTION_MENU: &str = include_str!("../../src/pending/fixtures/synthetic_question_menu.txt");
const BASH: &str = include_str!("../../src/pending/fixtures/synthetic_bash_permission.txt");

async fn get(f: &Fixture) -> HttpResponse {
    f.bridge
        .get_authed(&format!("/v1/agents/{}/suggestions", f.id))
        .await
}

/// The suggestions once they are served, polling while they generate.
async fn ready(f: &Fixture) -> Vec<String> {
    let wait = async {
        loop {
            let response = get(f).await;
            match response.status {
                200 => return serde_json::from_str(&response.body).expect("suggestions"),
                202 => {
                    assert_eq!(response.body, "[]");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                status => panic!("unexpected {status}: {}", response.body),
            }
        }
    };
    tokio::time::timeout(PATIENCE, wait)
        .await
        .expect("suggestions in time")
}

/// Every suggestion is short, single-line and free of control characters.
fn assert_safe(suggestions: &[String]) {
    assert!(suggestions.len() <= MAX_SUGGESTIONS, "{suggestions:?}");
    for s in suggestions {
        assert!(!s.trim().is_empty(), "{s:?}");
        assert!(s.chars().count() <= MAX_SUGGESTION_CHARS, "{s:?}");
        assert!(
            !s.chars()
                .any(|c| c.is_control() || protocol::text::is_hidden_char(c)),
            "{s:?}"
        );
        assert!(!s.starts_with(['!', '/', '#']), "{s:?}");
    }
}

/// A working agent with `transcript`, a short settle pause and `adjust`.
async fn fixture(
    fake: Option<&Arc<FakeSummarizer>>,
    transcript: &str,
    adjust: impl FnOnce(&mut bridge::Settings),
) -> Fixture {
    Fixture::start_with(fake.map(Arc::clone), transcript, |settings| {
        settings.tldr.settle = Duration::from_millis(10);
        adjust(settings);
    })
    .await
}

/// An idle agent whose last turn ends with `reply`.
async fn finished(fake: &Arc<FakeSummarizer>, reply: &str) -> Fixture {
    let f = fixture(Some(fake), &(user("go") + &assistant(reply)), |_| {}).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    f
}

#[tokio::test]
async fn an_end_of_turn_question_gets_suggestions_generated_once() {
    let fake = Arc::new(
        FakeSummarizer::new()
            .with_suggestions(&["Yes, open the PR", "Not yet"])
            .with_delay(Duration::from_millis(100)),
    );
    let f = finished(&fake, "Tests pass.\n\nShall I open a PR?").await;
    let first = get(&f).await;
    assert_eq!((first.status, first.body.as_str()), (202, "[]"));
    assert_eq!(ready(&f).await, ["Yes, open the PR", "Not yet"]);
    // Served from the cache.
    assert_eq!(ready(&f).await, ["Yes, open the PR", "Not yet"]);
    assert_eq!(fake.suggestion_calls(), 1);
    // A status change without a new turn (a new herdr seq) re-reads the
    // turn, but the same question reuses its answer.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let wait = async {
        while f.bridge.agents().await[0].status != protocol::AgentStatus::Done {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait).await.expect("done");
    assert_eq!(ready(&f).await, ["Yes, open the PR", "Not yet"]);
    assert_eq!(fake.suggestion_calls(), 1);

    // A new turn with a new question gets its own.
    append(&f.transcript, &(user("ok") + &assistant("Merge it too?")));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let wait = async {
        while fake.suggestion_calls() < 2 {
            let _ = ready(&f).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait)
        .await
        .expect("regenerated");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn unsafe_candidates_never_reach_the_app() {
    let long = "x".repeat(MAX_SUGGESTION_CHARS + 1);
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&[
        "/clear",
        "!rm -rf ~",
        "ok\u{1b}[2J",
        "fine\u{202E}",
        &long,
        "Yes,\n  go ahead",
        "yes, GO ahead",
        "Run the tests first",
        "Stop",
        "One too many",
    ]));
    let f = finished(&fake, "Proceed?").await;
    let got = ready(&f).await;
    assert_safe(&got);
    assert_eq!(got, ["Yes, go ahead", "Run the tests first", "Stop"]);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn nothing_is_suggested_without_a_question() {
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&["Yes"]));
    let f = finished(&fake, "All done, tests pass.").await;
    assert_eq!(ready(&f).await, Vec::<String>::new());
    // Working: nothing asked either.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    assert_eq!(ready(&f).await, Vec::<String>::new());
    assert_eq!(fake.suggestion_calls(), 0);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn dialogs_get_no_suggestions_their_options_answer_them() {
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&["SQLite", "Postgres"]));
    let f = fixture(Some(&fake), &user("set up storage"), |_| {}).await;
    // A question with a menu is a dialog: a prompt into it is refused.
    for screen in [QUESTION_MENU, BASH] {
        f.bridge.fake().set_screen(&f.pane, screen);
        f.bridge.fake().set_status(&f.pane, AgentStatus::Blocked);
        assert_eq!(ready(&f).await, Vec::<String>::new());
        let pending = f
            .bridge
            .get_authed(&format!("/v1/agents/{}/pending", f.id))
            .await;
        let pending: protocol::PendingInput = serde_json::from_str(&pending.body).expect("pending");
        assert_eq!(pending.suggestions, Vec::<String>::new());
    }
    assert_eq!(fake.suggestion_calls(), 0);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_turn_still_being_written_is_read_once_it_settles() {
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&["Yes"]));
    let f = fixture(Some(&fake), &user("go"), |settings| {
        settings.tldr.settle = Duration::from_millis(200);
    })
    .await;
    // herdr reports idle before the last message reaches the transcript.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let first = get(&f).await;
    assert_eq!(first.status, 202, "{}", first.body);
    tokio::time::sleep(Duration::from_millis(50)).await;
    append(&f.transcript, &assistant("Shall I go on?"));
    assert_eq!(ready(&f).await, ["Yes"]);
    assert_eq!(fake.suggestion_calls(), 1);
    f.bridge.stop().await.expect("clean stop");
}

/// A transcript line written long ago.
fn old(kind: &str, text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type": kind, "timestamp": "2026-01-01T00:00:00Z",
            "message": {"content": text}})
    )
}

#[tokio::test]
async fn a_turn_whose_last_lines_are_old_still_settles() {
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&["Yes"]));
    let f = fixture(
        Some(&fake),
        &(old("user", "go") + &old("assistant", "Running the tests.")),
        |settings| settings.tldr.settle = Duration::from_millis(200),
    )
    .await;
    // herdr reports idle before the final message is written; what is
    // written so far is old (a long tool run), which says nothing about
    // whether the turn is done being written.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let first = get(&f).await;
    assert_eq!(first.status, 202, "{}", first.body);
    tokio::time::sleep(Duration::from_millis(50)).await;
    append(&f.transcript, &assistant("Shall I go on?"));
    assert_eq!(ready(&f).await, ["Yes"]);
    assert_eq!(fake.suggestion_calls(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_same_question_at_a_new_seq_is_served_without_settling() {
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&["Yes"]));
    let f = fixture(
        Some(&fake),
        &(user("go") + &assistant("Proceed?")),
        |settings| settings.tldr.settle = Duration::from_millis(500),
    )
    .await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    assert_eq!(ready(&f).await, ["Yes"]);
    // Seen: done -> idle moves the seq, not the turn.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let wait = async {
        while f.bridge.agents().await[0].status != protocol::AgentStatus::Idle {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait).await.expect("idle");
    let started = tokio::time::Instant::now();
    assert_eq!(ready(&f).await, ["Yes"]);
    // Settling would take at least two settle pauses (1 s).
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(fake.suggestion_calls(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_fresh_session_has_nothing_to_suggest() {
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&["Yes"]));
    let f = fixture(
        Some(&fake),
        &(user("go") + &assistant("Proceed?")),
        |settings| settings.tldr.settle = Duration::from_secs(1),
    )
    .await;
    // No transcript yet: Claude writes it lazily, on the first prompt.
    let text = std::fs::read_to_string(&f.transcript).expect("transcript");
    std::fs::remove_file(&f.transcript).expect("remove");
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let started = tokio::time::Instant::now();
    assert_eq!(ready(&f).await, Vec::<String>::new());
    // Final at once: nothing to settle (that would take 2 s), no backoff.
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(get(&f).await.status, 200);
    // Its first turn is read afresh.
    append(&f.transcript, &text);
    f.bridge.fake().set_status(&f.pane, AgentStatus::Working);
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let wait = async {
        while f.bridge.agents().await[0].status != protocol::AgentStatus::Done {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait).await.expect("done");
    assert_eq!(ready(&f).await, ["Yes"]);
    assert_eq!(fake.suggestion_calls(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_failed_generation_is_retried_while_the_app_polls() {
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&["Yes"]));
    fake.fail_next(summarize::Error::NoStructuredOutput);
    let f = fixture(
        Some(&fake),
        &(user("go") + &assistant("Proceed?")),
        |settings| {
            settings.tldr.retry_after = Duration::from_millis(300);
        },
    )
    .await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let wait = async {
        while fake.suggestion_calls() < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait)
        .await
        .expect("first try");
    // During the backoff: "not yet" (202), never a final "nothing", and
    // not re-run on every poll.
    for _ in 0..3 {
        let response = get(&f).await;
        assert_eq!((response.status, response.body.as_str()), (202, "[]"));
    }
    assert_eq!(fake.suggestion_calls(), 1);
    // Polling on, as the app does, gets the retry's answer.
    assert_eq!(ready(&f).await, ["Yes"]);
    assert_eq!(fake.suggestion_calls(), 2);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_usage_limit_holds_suggestions_back() {
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&["Yes"]));
    fake.fail_next(summarize::Error::UsageLimit {
        message: "limit".into(),
    });
    let f = fixture(
        Some(&fake),
        &(user("go") + &assistant("Proceed?")),
        |settings| {
            settings.tldr.retry_after = Duration::from_millis(10);
            settings.tldr.max_retry_after = Duration::from_secs(3600);
        },
    )
    .await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let wait = async {
        while fake.suggestion_calls() < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait)
        .await
        .expect("first try");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(get(&f).await.status, 202);
    // Account-wide: a new question waits too ("not yet", not "nothing").
    append(&f.transcript, &(user("next") + &assistant("And now?")));
    f.bridge.fake().set_status(&f.pane, AgentStatus::Done);
    let wait = async {
        while f.bridge.agents().await[0].status != protocol::AgentStatus::Done {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait).await.expect("done");
    let response = get(&f).await;
    assert_eq!((response.status, response.body.as_str()), (202, "[]"));
    assert_eq!(fake.suggestion_calls(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_finished_turn_gets_its_suggestions_before_anyone_asks() {
    let fake = Arc::new(FakeSummarizer::new().with_suggestions(&["Yes"]));
    let f = fixture(Some(&fake), &(user("go") + &assistant("Proceed?")), |_| {}).await;
    // The watcher sees working -> idle and starts the job itself.
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let wait = async {
        while fake.suggestion_calls() < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(PATIENCE, wait)
        .await
        .expect("made eagerly");
    assert_eq!(ready(&f).await, ["Yes"]);
    assert_eq!(fake.suggestion_calls(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn suggestions_are_off_without_a_summarizer() {
    let f = fixture(None, &(user("go") + &assistant("Proceed?")), |_| {}).await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    let response = get(&f).await;
    assert_eq!((response.status, response.body.as_str()), (200, "[]"));
    let gone = f.bridge.get_authed("/v1/agents/w9:p9/suggestions").await;
    assert_eq!(gone.status, 404);
    f.bridge.stop().await.expect("clean stop");
}
