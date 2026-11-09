//! Golden wire-format tests: the JSON in `tests/golden/*.json` is the stable
//! contract between bridge and app. Each test checks both directions:
//! the Rust values serialize to exactly the golden text, and the golden text
//! deserializes back to the same values.
//!
//! A deliberate wire change: rerun with `UPDATE_GOLDEN=1` and review the diff.

// Fixture builders are plain fns (not `#[test]`), which clippy's
// allow-unwrap-in-tests does not cover; a bad fixture should just panic.
#![allow(clippy::unwrap_used)]

use std::fmt::Debug;
use std::path::PathBuf;

use protocol::keys::Digit;
use protocol::{
    Action, ActionKind, ActionResult, Agent, AgentEvent, AgentId, AgentKind, AgentStatus,
    AllowedKey, AttachmentId, Attachments, AudioId, CarRefusal, ChoosePayload, Confidence,
    FeedbackText, IdempotencyKey, InterruptPayload, KeySequence, KeysPayload, OptionId, Origin,
    PendingId, PendingInput, PendingKind, PendingOption, PendingOptions, PendingSource,
    ProjectName, PromptPayload, PromptText, RefusalReason, Risk, ScreenTail, Seq, SpawnKind,
    SpawnOptions, SpawnRefusal, SpawnRequest, SpawnResult, StatusLine, StepUpReason, ThreadContent,
    ThreadCursor, ThreadEpoch, ThreadItem, ThreadPage, Timestamp, Tldr, ToolStatus, Transcript,
    TurnHeadline, TurnIndex,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

const KEY: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";
const T0: Timestamp = Timestamp::from_unix_millis(1_762_500_000_000);

fn check<T>(name: &str, golden: &str, values: &[T])
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let actual = serde_json::to_string_pretty(values).unwrap() + "\n";
    // When regenerating, write the new text and still check that it parses
    // back, so a broken round-trip can't hide behind an update run.
    let golden = if std::env::var_os("UPDATE_GOLDEN").is_some() {
        let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "golden", name]
            .iter()
            .collect();
        std::fs::write(path, &actual).unwrap();
        actual.as_str()
    } else {
        golden
    };
    assert_eq!(
        actual, golden,
        "{name}: serialized form drifted from golden"
    );
    let parsed: Vec<T> = serde_json::from_str(golden).unwrap();
    assert_eq!(
        parsed, values,
        "{name}: golden does not parse back to the same values"
    );
}

fn agent_id() -> AgentId {
    format!("w6:p1~{KEY}").parse().unwrap()
}

fn agent() -> Agent {
    Agent {
        id: agent_id(),
        title: "login-lead".into(),
        repo: "gadget".into(),
        worktree: Some("login".into()),
        status: AgentStatus::Blocked,
        seq: Seq(136),
        status_since: T0,
        unread: false,
        kind: AgentKind::Claude,
        headline: Some("Run `pnpm test`?".into()),
        status_line: None,
        ask: Some("Run `pnpm test`?".into()),
        quiet_done: false,
    }
}

fn agents() -> Vec<Agent> {
    // A pane with no agent session: the id is just the pane.
    let minimal = Agent {
        id: "w7:p2".parse().unwrap(),
        title: "docs".into(),
        repo: "passporter".into(),
        worktree: None,
        status: AgentStatus::Done,
        seq: Seq(4),
        status_since: Timestamp::from_unix_millis(1_762_499_000_000),
        unread: true,
        kind: AgentKind::Codex,
        headline: None,
        status_line: None,
        ask: None,
        quiet_done: false,
    };
    let working = Agent {
        id: format!("w7:p3~{KEY}").parse().unwrap(),
        title: "circus-master".into(),
        repo: "passporter".into(),
        worktree: None,
        status: AgentStatus::Working,
        seq: Seq(57),
        status_since: T0,
        unread: false,
        kind: AgentKind::Claude,
        headline: None,
        status_line: Some(status_line()),
        ask: None,
        quiet_done: false,
    };
    // The orchestrator's implementer, done: quiet, so the flag is sent.
    let quiet = Agent {
        id: format!("w8:p2~{KEY}").parse().unwrap(),
        title: "impl-t26f".into(),
        repo: "passporter".into(),
        worktree: None,
        status: AgentStatus::Done,
        seq: Seq(9),
        status_since: T0,
        unread: true,
        kind: AgentKind::Claude,
        headline: None,
        status_line: None,
        ask: None,
        quiet_done: true,
    };
    vec![agent(), minimal, working, quiet]
}

fn status_line() -> StatusLine {
    StatusLine {
        text: "Merging T26B; waiting on the release build".into(),
        at: Timestamp::from_unix_millis(1_762_500_120_000),
    }
}

fn option(id: &str, label: &str, risk: Risk) -> PendingOption {
    PendingOption {
        id: OptionId::new(id).unwrap(),
        label: label.into(),
        risk,
        accepts_feedback: id == "deny",
    }
}

fn permission() -> PendingInput {
    PendingInput {
        id: PendingId::new("toolu_01HkP").unwrap(),
        kind: PendingKind::Permission,
        prompt: "Run `pnpm test`?".into(),
        detail: Some("pnpm test --filter bridge".into()),
        options: PendingOptions::new(vec![
            option("allow_once", "Allow once", Risk::Low),
            option("always_allow", "Always allow", Risk::Elevated),
            option("deny", "Deny", Risk::Low),
        ])
        .unwrap(),
        suggestions: vec![],
        source: PendingSource::Hook,
    }
}

fn question() -> PendingInput {
    PendingInput {
        id: PendingId::new("screen-42").unwrap(),
        kind: PendingKind::Question,
        prompt: "Shall I open the PR?".into(),
        detail: None,
        options: PendingOptions::default(),
        suggestions: vec!["Yes, open it".into(), "Run the tests first".into()],
        source: PendingSource::Screen {
            confidence: Confidence::new(62).unwrap(),
            screen_tail: ScreenTail::new(vec![
                "All tests pass.".into(),
                "Shall I open the PR?".into(),
            ]),
        },
    }
}

fn tldr() -> Tldr {
    Tldr {
        outcome: "Thermostat schedule works; 2 tests still fail.".into(),
        changed: vec![
            "src/schedule.rs (+120 -8)".into(),
            "tests/schedule.rs (+64)".into(),
        ],
        decide: Some("Keep the 5-minute debounce?".into()),
        risks: vec!["DST transition untested".into()],
    }
}

fn action(kind: ActionKind, origin: Origin) -> Action {
    Action {
        idempotency_key: IdempotencyKey::new(KEY).unwrap(),
        agent_id: agent_id(),
        expected_seq: Seq(136),
        created_at: T0,
        kind,
        origin,
    }
}

fn item(cursor: u64, turn: u32, at: Option<Timestamp>, content: ThreadContent) -> ThreadItem {
    ThreadItem {
        cursor: ThreadCursor(cursor),
        turn: TurnIndex(turn),
        rev: cursor + 100,
        at,
        content,
    }
}

#[test]
fn golden_agent() {
    check("agent.json", include_str!("golden/agent.json"), &agents());
}

#[test]
fn golden_thread_item() {
    let items = vec![
        item(
            1,
            0,
            Some(T0),
            ThreadContent::User {
                text: "Add a login page".into(),
            },
        ),
        item(
            2,
            0,
            Some(Timestamp::from_unix_millis(T0.unix_millis() + 1_000)),
            ThreadContent::Assistant {
                text: "I'll start with the model.".into(),
            },
        ),
        item(
            3,
            0,
            None,
            ThreadContent::Tool {
                name: "Bash".into(),
                input: "cargo test".into(),
                output: Some("test result: ok. 12 passed".into()),
                status: ToolStatus::Ok,
            },
        ),
        item(
            4,
            1,
            None,
            ThreadContent::Tool {
                name: "Bash".into(),
                input: "cargo build".into(),
                output: None,
                status: ToolStatus::Running,
            },
        ),
        item(
            5,
            1,
            None,
            ThreadContent::Diff {
                path: "src/schedule.rs".into(),
                added: 120,
                removed: 8,
                patch: Some("@@ -1 +1 @@\n-old\n+new\n".into()),
            },
        ),
        item(
            6,
            1,
            None,
            ThreadContent::Aside {
                label: "Message from another session (gadget-helper)".into(),
                text: "The release notes are ready.".into(),
            },
        ),
        item(
            7,
            1,
            None,
            ThreadContent::Aside {
                label: "You ran `git status --short`".into(),
                text: " M src/schedule.rs".into(),
            },
        ),
    ];
    check(
        "thread_item.json",
        include_str!("golden/thread_item.json"),
        &items,
    );
}

#[test]
fn golden_thread_page() {
    let pages = vec![
        ThreadPage {
            epoch: ThreadEpoch("b7f3-1-0".into()),
            items: vec![
                item(
                    4,
                    1,
                    None,
                    ThreadContent::Tool {
                        name: "Bash".into(),
                        input: "cargo build".into(),
                        output: None,
                        status: ToolStatus::Running,
                    },
                ),
                item(
                    5,
                    1,
                    Some(T0),
                    ThreadContent::Assistant {
                        text: "Building.".into(),
                    },
                ),
            ],
            resume_cursor: ThreadCursor(3),
            truncated_before: None,
            has_more: true,
            headlines: vec![
                TurnHeadline {
                    turn: TurnIndex(0),
                    text: "Resumed the session; nothing to do yet.".into(),
                    provisional: false,
                },
                TurnHeadline {
                    turn: TurnIndex(1),
                    text: "Merging the swarm's PRs one by one.".into(),
                    provisional: true,
                },
            ],
            inferred: true,
        },
        ThreadPage {
            epoch: ThreadEpoch("b7f3-1-1".into()),
            items: vec![],
            resume_cursor: ThreadCursor(20_412),
            truncated_before: Some(ThreadCursor(10_000)),
            has_more: false,
            headlines: Vec::new(),
            inferred: false,
        },
    ];
    check(
        "thread_page.json",
        include_str!("golden/thread_page.json"),
        &pages,
    );
}

#[test]
fn golden_pending_input() {
    check(
        "pending_input.json",
        include_str!("golden/pending_input.json"),
        &[permission(), question()],
    );
}

#[test]
fn golden_tldr() {
    let minimal = Tldr {
        outcome: "Done.".into(),
        changed: vec![],
        decide: None,
        risks: vec![],
    };
    check(
        "tldr.json",
        include_str!("golden/tldr.json"),
        &[tldr(), minimal],
    );
}

#[test]
fn golden_action() {
    let actions = vec![
        action(
            ActionKind::Prompt(PromptPayload::text(
                PromptText::new("Run the tests\nthen push").unwrap(),
            )),
            Origin::App,
        ),
        action(
            ActionKind::Prompt(PromptPayload {
                text: PromptText::new("What is this error?").unwrap(),
                attachments: Attachments::new(vec![
                    AttachmentId::new("6f1e1a52-7c1b-4b8e-9a43-0c2d5e8f9a10").unwrap(),
                ])
                .unwrap(),
            }),
            Origin::App,
        ),
        action(
            ActionKind::Choose(ChoosePayload {
                pending_id: PendingId::new("toolu_01HkP").unwrap(),
                option_id: OptionId::new("allow_once").unwrap(),
                feedback_text: None,
            }),
            Origin::Notification,
        ),
        action(
            ActionKind::Choose(ChoosePayload {
                pending_id: PendingId::new("toolu_01HkP").unwrap(),
                option_id: OptionId::new("deny").unwrap(),
                feedback_text: Some(FeedbackText::new("Use nextest instead").unwrap()),
            }),
            Origin::Car,
        ),
        action(
            ActionKind::Keys(KeysPayload {
                pending_id: Some(PendingId::new("screen-42").unwrap()),
                keys: KeySequence::new(vec![
                    AllowedKey::Digit(Digit::new(2).unwrap()),
                    AllowedKey::Enter,
                    AllowedKey::CtrlC,
                ])
                .unwrap(),
            }),
            Origin::App,
        ),
        action(ActionKind::Interrupt(InterruptPayload {}), Origin::App),
    ];
    check("action.json", include_str!("golden/action.json"), &actions);
}

#[test]
fn golden_action_result() {
    let results = vec![
        ActionResult::Sent { replayed: false },
        ActionResult::Sent { replayed: true },
        ActionResult::Stale {
            current: Some(Box::new(agent())),
            pending: Some(permission()),
        },
        ActionResult::Stale {
            current: None,
            pending: None,
        },
        ActionResult::Refused {
            reason: RefusalReason::CarPolicy,
            message: CarRefusal::StepUp(StepUpReason::ElevatedOption).to_string(),
        },
        ActionResult::Refused {
            reason: RefusalReason::IdempotencyConflict,
            message: "This idempotency key was already used for a different action.".into(),
        },
        ActionResult::Refused {
            reason: RefusalReason::InvalidAction,
            message: "unknown field(s): shell".into(),
        },
        ActionResult::Failed {
            message: "herdr: timed out".into(),
        },
    ];
    check(
        "action_result.json",
        include_str!("golden/action_result.json"),
        &results,
    );
}

#[test]
fn golden_spawn() {
    let requests = vec![
        SpawnRequest {
            idempotency_key: IdempotencyKey::new(KEY).unwrap(),
            project: ProjectName::new("passporter").unwrap(),
            kind: SpawnKind::Claude,
            prompt: Some(PromptText::new("Fix the flaky stream test").unwrap()),
            created_at: T0,
        },
        SpawnRequest {
            idempotency_key: IdempotencyKey::new(KEY).unwrap(),
            project: ProjectName::new("web-2").unwrap(),
            kind: SpawnKind::Codex,
            prompt: None,
            created_at: T0,
        },
    ];
    check(
        "spawn_request.json",
        include_str!("golden/spawn_request.json"),
        &requests,
    );
    let options = vec![SpawnOptions {
        projects: vec![ProjectName::new("passporter").unwrap()],
        kinds: SpawnKind::ALL.to_vec(),
    }];
    check(
        "spawn_options.json",
        include_str!("golden/spawn_options.json"),
        &options,
    );
    let results = vec![
        SpawnResult::Spawned {
            agent_id: agent_id(),
            replayed: false,
            prompt_error: None,
        },
        SpawnResult::Spawned {
            agent_id: "w9:p2".parse().unwrap(),
            replayed: true,
            prompt_error: Some("The agent asked something first.".into()),
        },
        SpawnResult::Refused {
            reason: SpawnRefusal::UnknownProject,
            message: "No project by that name.".into(),
        },
        SpawnResult::Failed {
            message: "herdr: timed out".into(),
        },
    ];
    check(
        "spawn_result.json",
        include_str!("golden/spawn_result.json"),
        &results,
    );
}

#[test]
fn golden_agent_event() {
    let events = vec![
        AgentEvent::Snapshot { agents: agents() },
        AgentEvent::AgentUpdated {
            agent: Some(agent()),
        },
        AgentEvent::AgentRemoved {
            agent_id: agent_id(),
        },
        AgentEvent::ThreadItems {
            agent_id: agent_id(),
            epoch: ThreadEpoch("b7f3-1-0".into()),
            resume_cursor: ThreadCursor(9),
            items: vec![item(
                9,
                3,
                Some(T0),
                ThreadContent::Assistant {
                    text: "Done.".into(),
                },
            )],
        },
        AgentEvent::PendingChanged {
            agent_id: agent_id(),
            pending: Some(question()),
        },
        AgentEvent::PendingChanged {
            agent_id: agent_id(),
            pending: None,
        },
        AgentEvent::TldrReady {
            agent_id: agent_id(),
            turn: TurnIndex(3),
            tldr: tldr(),
            provisional: false,
        },
        AgentEvent::TldrReady {
            agent_id: agent_id(),
            turn: TurnIndex(4),
            tldr: tldr(),
            provisional: true,
        },
        AgentEvent::HeadlineReady {
            agent_id: agent_id(),
            turn: TurnIndex(3),
            headline: "Fixed the flaky test; wants to know whether to push.".into(),
            provisional: false,
        },
        AgentEvent::HeadlineReady {
            agent_id: agent_id(),
            turn: TurnIndex(4),
            headline: "Merging the swarm's PRs one by one.".into(),
            provisional: true,
        },
        AgentEvent::StatusLineReady {
            agent_id: agent_id(),
            status_line: status_line(),
        },
    ];
    check(
        "agent_event.json",
        include_str!("golden/agent_event.json"),
        &events,
    );
}

#[test]
fn golden_transcript() {
    let transcript = Transcript {
        audio_id: AudioId::new("583dc90c-15e5-4f5c-aeea-2d4701cb9d28").unwrap(),
        text: "yes go ahead and run the tests".into(),
        duration_ms: 2_350,
    };
    check(
        "transcript.json",
        include_str!("golden/transcript.json"),
        &[transcript],
    );
}
