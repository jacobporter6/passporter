//! Reader behaviour on synthetic transcripts that mimic Claude Code 2.1.x.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{ThreadContent, ThreadCursor, ThreadEpoch, ThreadItem, ToolStatus, TurnIndex};
use serde_json::{Value, json};
use transcripts::{
    INVALID_JSON, Limits, PARSER_VERSION, Refresh, Scope, Thread, TranscriptReader, Window,
};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/session.jsonl");
/// The rest of the fixture's partial final line, and one more record.
const REST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/session.rest");

/// A fresh temp dir, removed on drop.
struct Scratch(tempfile::TempDir);

impl Scratch {
    fn new(_name: &str) -> Self {
        Self(tempfile::tempdir().expect("temp dir"))
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }
}

fn append(path: &Path, text: &str) {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .expect("append");
}

/// A one-line rendering of an item, so whole threads compare readably.
fn show(item: &ThreadItem) -> String {
    let body = match &item.content {
        ThreadContent::User { text } => format!("user {text:?}"),
        ThreadContent::Assistant { text } => format!("assistant {text:?}"),
        ThreadContent::Tool {
            name,
            input,
            output,
            status,
        } => format!(
            "tool {name} {input:?} {status:?} out={}",
            output.as_ref().map_or(0, String::len)
        ),
        ThreadContent::Diff {
            path,
            added,
            removed,
            patch,
        } => format!("diff {path} +{added} -{removed} patch={}", patch.is_some()),
        ThreadContent::Aside { label, text } => format!("aside {label:?} {text:?}"),
        ThreadContent::Unknown => "unknown".to_owned(),
    };
    format!("{} t{} {body}", item.cursor.0, item.turn.0)
}

fn shown(items: &[ThreadItem]) -> Vec<String> {
    items.iter().map(show).collect()
}

fn fixture_reader(scratch: &Scratch) -> (TranscriptReader, PathBuf) {
    let path = scratch.file("session.jsonl");
    fs::copy(FIXTURE, &path).expect("copy fixture");
    let mut reader = TranscriptReader::new(&path, Scope::Main);
    reader.refresh().expect("refresh");
    (reader, path)
}

#[test]
fn normalizes_a_realistic_session() {
    let scratch = Scratch::new("realistic");
    let (reader, _) = fixture_reader(&scratch);
    let thread = reader.thread();
    assert_eq!(
        shown(thread.items()),
        [
            r#"1 t1 user "Fix the failing parser test and add a changelog entry.""#,
            r#"2 t1 assistant "I'll run the tests first.""#,
            r#"3 t1 tool Bash "cargo test -p parser" Error out=70"#,
            r#"4 t1 tool Read "/home/dev/src/widget/src/parse.rs" Ok out=103"#,
            r#"5 t1 tool Edit "/home/dev/src/widget/src/parse.rs" Ok out=176"#,
            "6 t1 diff src/parse.rs +1 -1 patch=true",
            r#"7 t1 tool Write "/home/dev/src/widget/CHANGELOG.md" Ok out=63"#,
            "8 t1 diff CHANGELOG.md +3 -0 patch=true",
            r#"9 t1 assistant "Fixed: `parse` now splits on any whitespace. Added a changelog entry.""#,
            r#"10 t2 user "Now ask a subagent to review it.\n\n[image]""#,
            r#"11 t2 tool Agent "general-purpose: Review parser fix" Ok out=41"#,
            r#"12 t2 tool SendMessage "→ main: Review done" Ok out=12"#,
            r#"13 t2 tool Bash "sleep 600" Error out=42"#,
            r#"14 t3 user "Run the full suite.""#,
            r#"15 t3 tool Bash "cargo nextest run --workspace" Running out=0"#,
        ]
    );
    assert_eq!(thread.current_turn(), Some(TurnIndex(3)));

    let first = &thread.items()[0];
    assert_eq!(
        first.at.map(protocol::Timestamp::unix_millis),
        Some(1_794_042_002_074)
    );

    let ThreadContent::Diff { patch, .. } = &thread.items()[5].content else {
        panic!("item 6 is a diff");
    };
    assert_eq!(
        patch.as_deref(),
        Some(
            "@@ -1,3 +1,3 @@\n pub fn parse(s: &str) -> Vec<Token> {\n-    s.split(' ').map(Token::new).collect()\n+    s.split_whitespace().map(Token::new).collect()\n }\n"
        )
    );
}

#[test]
fn counts_what_it_skips() {
    let scratch = Scratch::new("diagnostics");
    let (reader, _) = fixture_reader(&scratch);
    let d = reader.thread().diagnostics();
    assert_eq!(d.lines, 40, "the partial final line is not read");
    assert_eq!(d.unknown_types.keys().collect::<Vec<_>>(), ["hologram"]);
    assert_eq!(d.malformed.get(INVALID_JSON), Some(&1));
    assert_eq!(d.malformed.len(), 1);
    assert_eq!(d.unknown_blocks, 1, "server_tool_use");
    assert_eq!(d.orphan_results, 1);
    for kind in [
        "permission-mode",
        "file-history-snapshot",
        "ai-title",
        "queue-operation",
        "attachment",
        "pr-link",
        "api-request",
        "api-request-blob",
        "sidechain",
        "user:meta",
        "user:compact-summary",
    ] {
        assert_eq!(d.skipped.get(kind), Some(&1), "{kind}");
    }
    assert_eq!(d.skipped.get("system"), Some(&2));
    assert_eq!(
        d.skipped.get("user:notice"),
        Some(&3),
        "slash command, its output, interruption"
    );
}

#[test]
fn completes_a_partial_line_and_reports_the_update() {
    let scratch = Scratch::new("partial");
    let (mut reader, path) = fixture_reader(&scratch);
    let thread = reader.thread();
    assert_eq!(thread.last_cursor(), ThreadCursor(15));
    assert_eq!(
        thread.resume_cursor(),
        ThreadCursor(14),
        "pinned before the running tool"
    );

    // Claude Code finishes writing the tool result, then answers.
    append(&path, &fs::read_to_string(REST).expect("rest fixture"));

    let refresh = reader.refresh().expect("refresh");
    assert!(!refresh.reset);
    assert_eq!(refresh.changed, [ThreadCursor(15), ThreadCursor(16)]);
    let thread = reader.thread();
    let changed: Vec<ThreadItem> = refresh
        .changed
        .iter()
        .filter_map(|c| thread.get(*c).cloned())
        .collect();
    assert_eq!(
        shown(&changed),
        [
            r#"15 t3 tool Bash "cargo nextest run --workspace" Ok out=20"#,
            r#"16 t3 assistant "All green.""#,
        ]
    );
    assert_eq!(thread.resume_cursor(), ThreadCursor(16));
    assert_eq!(reader.refresh().expect("idle"), Refresh::default());
}

#[test]
fn since_turn_slices_and_pages() {
    let scratch = Scratch::new("since");
    let (reader, _) = fixture_reader(&scratch);
    let thread = reader.thread();
    assert_eq!(thread.since(ThreadCursor(0)).len(), 15);
    assert_eq!(thread.since(ThreadCursor(13)).len(), 2);
    assert_eq!(thread.since(ThreadCursor(15)), []);
    assert_eq!(thread.since(ThreadCursor(u64::MAX)), []);
    assert_eq!(thread.first_cursor(), ThreadCursor(1));

    assert_eq!(thread.turn(TurnIndex(1)).len(), 9);
    assert_eq!(thread.turn(TurnIndex(2)).len(), 4);
    assert_eq!(thread.turn(TurnIndex(3)).len(), 2);
    assert_eq!(thread.turn(TurnIndex(9)), []);

    let epoch = thread.epoch("boot");
    assert!(
        epoch.0.starts_with(&format!("boot-{PARSER_VERSION}-")),
        "{epoch:?}"
    );
    let page = thread.page(epoch.clone(), ThreadCursor(13), 100);
    assert_eq!(page.epoch, epoch);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.resume_cursor, ThreadCursor(14));
    assert_eq!(page.truncated_before, None);
    assert!(!page.has_more);

    // Size-limited pages chain through resume_cursor.
    let first = thread.page(epoch.clone(), ThreadCursor(0), 4);
    assert_eq!(first.items.len(), 4);
    assert!(first.has_more);
    assert_eq!(first.resume_cursor, ThreadCursor(4));
    let mut since = first.resume_cursor;
    let mut seen = first.items.len();
    loop {
        let page = thread.page(epoch.clone(), since, 4);
        seen += page.items.len();
        since = page.resume_cursor;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(seen, 15);
    assert_eq!(since, ThreadCursor(14), "pinned before the running tool");
}

fn ingest_all(thread: &mut Thread, lines: &[Value]) {
    for line in lines {
        thread.ingest(line.to_string().as_bytes());
    }
}

fn tool_use(id: &str, command: &str) -> Value {
    json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}
    ]}})
}

fn tool_result(id: &str, output: &str) -> Value {
    json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": id, "content": output}
    ]}})
}

fn user(text: &str) -> Value {
    json!({"type": "user", "message": {"content": text}})
}

#[test]
fn a_new_turn_settles_abandoned_tools() {
    let mut thread = Thread::new(Scope::Main);
    ingest_all(&mut thread, &[user("go"), tool_use("t1", "make")]);
    assert_eq!(thread.resume_cursor(), ThreadCursor(1));
    thread.take_changed();
    ingest_all(&mut thread, &[user("never mind")]);
    assert_eq!(
        shown(thread.items()),
        [
            r#"1 t1 user "go""#,
            r#"2 t1 tool Bash "make" Unknown out=0"#,
            r#"3 t2 user "never mind""#,
        ]
    );
    assert_eq!(thread.resume_cursor(), ThreadCursor(3));
    assert_eq!(thread.take_changed(), [ThreadCursor(2), ThreadCursor(3)]);
}

#[test]
fn injected_text_does_not_split_a_turn_or_settle_tools() {
    let mut thread = Thread::new(Scope::Main);
    ingest_all(
        &mut thread,
        &[
            user("go"),
            tool_use("t1", "cargo build"),
            user(
                "\n<task-notification><task-id>b7</task-id><status>completed</status></task-notification>",
            ),
            user("<system-reminder>todo list changed</system-reminder>"),
            user("<command-name>/context</command-name>"),
            user("<user-prompt-submit-hook>checked</user-prompt-submit-hook>"),
            tool_result("t1", "Finished"),
        ],
    );
    assert_eq!(
        shown(thread.items()),
        [
            r#"1 t1 user "go""#,
            r#"2 t1 tool Bash "cargo build" Ok out=8"#,
            r#"3 t1 aside "Background task update" """#,
        ]
    );
    assert_eq!(thread.diagnostics().orphan_results, 0);
}

/// A user-role record as Claude Code 2.1.x writes it, with an `origin`.
fn from(origin: &Value, is_meta: bool, text: &str) -> Value {
    json!({"type": "user", "isMeta": is_meta, "origin": origin,
        "message": {"role": "user", "content": text}})
}

/// A prompt queued while the agent was busy.
fn queued(origin: &Value, prompt: &str) -> Value {
    json!({"type": "attachment", "attachment":
        {"type": "queued_command", "prompt": prompt, "origin": origin}})
}

#[test]
fn messages_from_other_agents_do_not_split_a_turn() {
    let human = json!({"kind": "human"});
    let report = json!({"kind": "peer", "from": "a1b2", "name": "code-review",
        "body": "No issues found.", "senderTaskId": "t9"});
    let session = json!({"kind": "peer", "from": "uds:/run/s.sock", "fromMode": "bypass",
        "name": "gadget-helper", "body": "Release notes are ready."});
    let task = json!({"kind": "task-notification", "producer": "session-task"});
    let mut thread = Thread::new(Scope::Main);
    ingest_all(
        &mut thread,
        &[
            from(&human, false, "Review the parser fix"),
            tool_use("t1", "cargo test"),
            queued(
                &session,
                "<cross-session-message from=\"uds:/run/s.sock\" from-name=\"gadget-helper\">\
                 Release notes are ready.</cross-session-message>",
            ),
            tool_result("t1", "ok"),
            from(
                &report,
                true,
                "Another Claude session sent a message: <agent-message from=\"a1b2\">\
                 No issues found.</agent-message>",
            ),
            queued(
                &task,
                "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n\
                 <summary>Background command \"sleep 5\" completed</summary>\n</task-notification>",
            ),
            json!({"type": "assistant", "message": {"content": "All clear."}}),
            queued(&human, "also bump the version"),
            from(&human, false, "Thanks, now release it"),
        ],
    );
    assert_eq!(
        shown(thread.items()),
        [
            r#"1 t1 user "Review the parser fix""#,
            r#"2 t1 tool Bash "cargo test" Ok out=2"#,
            r#"3 t1 aside "Message from another session (gadget-helper)" "Release notes are ready.""#,
            r#"4 t1 aside "Report from an agent (code-review)" "No issues found.""#,
            r#"5 t1 aside "Background command \"sleep 5\" completed" """#,
            r#"6 t1 assistant "All clear.""#,
            r#"7 t1 user "also bump the version""#,
            r#"8 t2 user "Thanks, now release it""#,
        ]
    );
    assert_eq!(thread.turn(TurnIndex(1)).len(), 7, "one turn, not four");
    let said = &thread.diagnostics().said;
    assert_eq!(said.get("agent-message (by origin)"), Some(&1));
    assert_eq!(said.get("session-message (by origin)"), Some(&1));
    assert_eq!(said.get("notification (by origin)"), Some(&1));
    assert_eq!(said.get("prompt (by origin)"), Some(&3));
}

#[test]
fn shell_commands_get_their_output_and_hold_the_resume_cursor() {
    let mut thread = Thread::new(Scope::Main);
    ingest_all(
        &mut thread,
        &[
            user("go"),
            user("<bash-input>git status --short</bash-input>"),
        ],
    );
    assert_eq!(thread.resume_cursor(), ThreadCursor(1), "output may follow");
    thread.take_changed();
    ingest_all(
        &mut thread,
        &[user(
            "<bash-stdout> M src/lib.rs\n</bash-stdout><bash-stderr></bash-stderr>",
        )],
    );
    assert_eq!(thread.take_changed(), [ThreadCursor(2)]);
    ingest_all(
        &mut thread,
        &[
            json!({"type": "assistant", "message": {"content": "Noted."}}),
            user("<bash-stdout>stray</bash-stdout>"),
        ],
    );
    assert_eq!(
        shown(thread.items()),
        [
            r#"1 t1 user "go""#,
            r#"2 t1 aside "You ran `git status --short`" " M src/lib.rs""#,
            r#"3 t1 assistant "Noted.""#,
        ]
    );
    assert_eq!(thread.resume_cursor(), ThreadCursor(3));
    assert_eq!(
        thread.diagnostics().skipped.get("user:shell-output"),
        Some(&1),
        "output with no command before it"
    );
}

#[test]
fn a_reused_tool_id_settles_the_older_call() {
    let mut thread = Thread::new(Scope::Main);
    ingest_all(
        &mut thread,
        &[
            user("go"),
            tool_use("dup", "first"),
            tool_use("dup", "second"),
            tool_result("dup", "done"),
        ],
    );
    assert_eq!(
        shown(thread.items()),
        [
            r#"1 t1 user "go""#,
            r#"2 t1 tool Bash "first" Unknown out=0"#,
            r#"3 t1 tool Bash "second" Ok out=4"#,
        ]
    );
}

#[test]
fn running_tools_are_capped_by_count_and_age() {
    let window = Window {
        max_pending: 2,
        max_pending_age: 5,
        ..Window::default()
    };
    let mut thread = Thread::new(Scope::Main).with_window(window);
    ingest_all(
        &mut thread,
        &[
            user("go"),
            tool_use("a", "a"),
            tool_use("b", "b"),
            tool_use("c", "c"),
        ],
    );
    assert_eq!(
        shown(thread.items())[1..],
        [
            r#"2 t1 tool Bash "a" Unknown out=0"#,
            r#"3 t1 tool Bash "b" Running out=0"#,
            r#"4 t1 tool Bash "c" Running out=0"#,
        ]
    );
    assert_eq!(thread.resume_cursor(), ThreadCursor(2));

    let chatter: Vec<Value> = (0..6)
        .map(|i| json!({"type": "assistant", "message": {"content": format!("note {i}")}}))
        .collect();
    ingest_all(&mut thread, &chatter);
    assert_eq!(thread.last_cursor(), ThreadCursor(10));
    assert!(
        thread.items()[1..4].iter().all(|item| matches!(
            item.content,
            ThreadContent::Tool {
                status: ToolStatus::Unknown,
                ..
            }
        )),
        "{:?}",
        shown(thread.items())
    );
    assert_eq!(thread.resume_cursor(), ThreadCursor(10), "no longer pinned");
}

#[test]
fn settle_pending_ends_a_session() {
    let scratch = Scratch::new("settle");
    let (mut reader, _) = fixture_reader(&scratch);
    let refresh = reader.settle_pending();
    assert_eq!(refresh.changed, [ThreadCursor(15)]);
    assert_eq!(reader.thread().resume_cursor(), ThreadCursor(15));
}

#[test]
fn the_memory_window_drops_old_items_and_says_so() {
    let mut thread = Thread::new(Scope::Main).with_window(Window {
        max_items: 100,
        ..Window::default()
    });
    let lines: Vec<Value> = (0..150).map(|i| user(&format!("message {i}"))).collect();
    ingest_all(&mut thread, &lines);
    assert!(thread.items().len() <= 100);
    assert_eq!(thread.last_cursor(), ThreadCursor(150));
    let first = thread.first_cursor();
    assert!(first > ThreadCursor(1));
    assert_eq!(thread.truncated_before(), Some(first));
    assert_eq!(thread.since(ThreadCursor(0))[0].cursor, first);
    assert_eq!(
        thread
            .page(ThreadEpoch("e".into()), ThreadCursor(0), usize::MAX)
            .truncated_before,
        Some(first)
    );
    assert_eq!(thread.take_changed().first(), Some(&first));
}

#[test]
fn truncation_rebuilds_with_fresh_cursors() {
    let scratch = Scratch::new("reset");
    let (mut reader, path) = fixture_reader(&scratch);
    assert_eq!(reader.thread().last_cursor(), ThreadCursor(15));
    assert_eq!(reader.thread().generation(), 0);

    fs::write(
        &path,
        format!(
            "{}\n",
            json!({"type": "user", "message": {"content": "fresh start"}})
        ),
    )
    .expect("truncate");
    let refresh = reader.refresh().expect("refresh");
    assert!(refresh.reset);
    assert_eq!(refresh.changed, [ThreadCursor(16)]);
    let thread = reader.thread();
    assert_eq!(thread.generation(), 1);
    assert_eq!(thread.first_cursor(), ThreadCursor(16));
    assert_eq!(shown(thread.items()), [r#"16 t1 user "fresh start""#]);
    assert_eq!(
        thread.since(ThreadCursor(3)).len(),
        1,
        "stale cursors see only new items"
    );
    assert_eq!(thread.diagnostics().resets, 1);
}

#[test]
fn a_huge_turn_stays_bounded() {
    let scratch = Scratch::new("huge");
    let path = scratch.file("huge.jsonl");
    let output = "x".repeat(32 * 1024);
    let mut body = String::new();
    body.push_str(&json!({"type": "user", "message": {"content": "do everything"}}).to_string());
    body.push('\n');
    for i in 0..1_000 {
        let id = format!("toolu_{i}");
        body.push_str(
            &json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": format!("step {i}")},
                {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": format!("echo {i}")}}
            ]}})
            .to_string(),
        );
        body.push('\n');
        body.push_str(
            &json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": id, "content": output}
            ]}})
            .to_string(),
        );
        body.push('\n');
    }
    // One record far beyond the line cap, e.g. an api-request-blob.
    body.push_str(
        &json!({"type": "api-request-blob", "blob": "b".repeat(2 * 1024 * 1024)}).to_string(),
    );
    body.push('\n');
    body.push_str(&json!({"type": "assistant", "message": {"content": "done"}}).to_string());
    body.push('\n');
    append(&path, &body);

    let limits = Limits {
        tool_output: 1024,
        ..Limits::default()
    };
    let mut reader = TranscriptReader::new(&path, Scope::Main)
        .with_limits(limits)
        .with_max_line(1024 * 1024);
    reader.refresh().expect("refresh");
    let thread = reader.thread();
    assert_eq!(thread.items().len(), 1 + 2 * 1_000 + 1);
    assert_eq!(thread.current_turn(), Some(TurnIndex(1)));
    assert_eq!(thread.diagnostics().oversized_lines, 1);
    for item in thread.items() {
        if let ThreadContent::Tool { output, status, .. } = &item.content {
            assert_eq!(*status, ToolStatus::Ok);
            let output = output.as_deref().expect("output");
            assert!(output.len() < 1100, "output capped: {}", output.len());
            assert!(output.ends_with("more bytes]"), "{output}");
        }
    }
}

#[test]
fn subagent_scope_renders_sidechain_records() {
    let line = json!({"type": "assistant", "isSidechain": true, "agentId": "a1",
        "message": {"content": [{"type": "text", "text": "reviewing"}]}})
    .to_string();
    let mut main = Thread::new(Scope::Main);
    main.ingest(line.as_bytes());
    assert_eq!(main.items(), []);
    let mut sub = Thread::new(Scope::Subagent);
    sub.ingest(line.as_bytes());
    assert_eq!(shown(sub.items()), [r#"1 t0 assistant "reviewing""#]);
}

#[test]
fn hostile_lines_never_panic() {
    let mut thread = Thread::new(Scope::Main);
    let lines: &[&[u8]] = &[
        b"",
        b"null",
        b"[]",
        b"42",
        b"\"user\"",
        b"{}",
        b"{\"type\":7}",
        b"{\"type\":null}",
        b"{\"type\":\"user\"}",
        b"{\"type\":\"user\",\"message\":null}",
        b"{\"type\":\"user\",\"message\":\"hi\"}",
        b"{\"type\":\"user\",\"message\":{\"content\":[null,1,\"x\",{}]}}",
        b"{\"type\":\"user\",\"isMeta\":\"yes\",\"message\":{\"content\":\"x\"}}",
        b"{\"type\":\"user\",\"timestamp\":12,\"message\":{\"content\":\"x\"}}",
        b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":1}]}}",
        b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"x\"}]}}",
        b"{\"type\":\"system\",\"isMeta\":[1],\"message\":{\"content\":{}}}",
        b"{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"e\",\"name\":\"Edit\",\"input\":\"oops\"}]}}",
        b"{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"e\",\"content\":{\"a\":1}}]},\"toolUseResult\":{\"structuredPatch\":[{\"lines\":[\"\"]}]}}",
        b"\xff\xfe{not utf8",
        b"{\"type\":\"user\",\"message\":{\"content\":\"\xff\"}}",
    ];
    for line in lines {
        thread.ingest(line);
    }
    let d = thread.diagnostics();
    assert!(d.malformed.values().sum::<u64>() >= 5, "{d}");
    assert_eq!(d.lines, lines.len() as u64);
}

#[test]
fn every_reader_gets_its_own_epoch_and_revs_grow() {
    let mut a = Thread::new(Scope::Main);
    let b = Thread::new(Scope::Main);
    assert_ne!(a.identity(), b.identity());
    assert_ne!(a.epoch("boot"), b.epoch("boot"));

    ingest_all(&mut a, &[user("go"), tool_use("t1", "make")]);
    let running = a.get(ThreadCursor(2)).expect("tool").rev;
    ingest_all(&mut a, &[tool_result("t1", "ok")]);
    let done = a.get(ThreadCursor(2)).expect("tool").rev;
    assert!(done > running, "an update bumps the item's rev");
    assert!(done > a.get(ThreadCursor(1)).expect("user").rev);
}

#[test]
fn typed_text_after_an_injected_block_starts_a_turn() {
    let mut thread = Thread::new(Scope::Main);
    ingest_all(
        &mut thread,
        &[
            json!({"type": "assistant", "message": {"content": "Resumed session."}}),
            json!({"type": "user", "message": {"content": [
                {"type": "text", "text": "<system-reminder>todo list changed</system-reminder>"},
                {"type": "text", "text": "Fix the parser"}
            ]}}),
            user("<user-prompt-submit-hook>ok</user-prompt-submit-hook>\nAnd add a test"),
        ],
    );
    assert_eq!(
        shown(thread.items()),
        [
            r#"1 t0 assistant "Resumed session.""#,
            r#"2 t1 user "Fix the parser""#,
            r#"3 t2 user "And add a test""#,
        ]
    );
    assert_eq!(
        thread.turn(TurnIndex(0)).len(),
        1,
        "the preamble is its own turn"
    );
}

#[test]
fn queued_prompts_show_in_the_current_turn() {
    let mut thread = Thread::new(Scope::Main);
    ingest_all(
        &mut thread,
        &[
            user("go"),
            tool_use("t1", "cargo build"),
            json!({"type": "attachment", "attachment":
                {"type": "queued_command", "prompt": "also run clippy"}}),
            json!({"type": "attachment", "attachment": {"type": "todo", "content": []}}),
            tool_result("t1", "done"),
        ],
    );
    assert_eq!(
        shown(thread.items()),
        [
            r#"1 t1 user "go""#,
            r#"2 t1 tool Bash "cargo build" Ok out=4"#,
            r#"3 t1 user "also run clippy""#,
        ]
    );
    assert_eq!(thread.diagnostics().skipped.get("attachment"), Some(&1));
}

#[test]
fn running_edits_hold_only_a_capped_plan() {
    let limits = Limits {
        patch: 1024,
        ..Limits::default()
    };
    let mut thread = Thread::new(Scope::Main).with_limits(limits);
    let content = "x\n".repeat(500_000);
    ingest_all(
        &mut thread,
        &[
            user("write it"),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use",
                "id": "w1", "name": "Write",
                "input": {"file_path": "/w/big.txt", "content": content}}]}}),
            tool_result("w1", "File created"),
        ],
    );
    let ThreadContent::Diff {
        added, patch, path, ..
    } = &thread.items()[2].content
    else {
        panic!("a diff: {:?}", shown(thread.items()));
    };
    assert_eq!(
        (*added, patch, path.as_str()),
        (500_000, &None, "/w/big.txt")
    );
}

/// Every record kind the scrubbed corpus shows, in one synthetic session
/// (`fixtures/realistic.jsonl`): slash-command caveats and output, a long
/// turn with six edits and a failed tool, `/compact`'s boundary and
/// summary, a queued prompt, a subagent's report (`origin.kind = peer`),
/// task notifications (direct and queued), a `!` command and its output,
/// and the session noise (titles, snapshots, queue operations). Only the
/// two prompts the person typed, and the one they queued, are theirs.
#[test]
fn the_realistic_session_shows_only_typed_prompts_as_the_users() {
    const REALISTIC: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/realistic.jsonl"
    );
    let mut reader = TranscriptReader::new(Path::new(REALISTIC), Scope::Main);
    reader.refresh().expect("refresh");
    let items = reader.thread().items();
    let users: Vec<&str> = items
        .iter()
        .filter_map(|item| match &item.content {
            ThreadContent::User { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        [
            "Refactor the parser's error type into an enum and update every caller.",
            "Draft the release notes for 0.3.",
            "Mention the parser fix too.",
        ],
        "nothing injected is shown as the user's"
    );
    let asides: Vec<String> = items
        .iter()
        .filter(|item| matches!(item.content, ThreadContent::Aside { .. }))
        .map(show)
        .collect();
    assert_eq!(
        asides,
        [
            "25 t2 aside \"Report from an agent (changelog-check)\" \"The changelog has no entry for the parser fix.\"",
            "26 t2 aside \"Background command \\\"cargo test -p widget\\\" completed (exit code 0)\" \"\"",
            "27 t2 aside \"Monitor \\\"docs build\\\" finished\" \"\"",
            "30 t2 aside \"You ran `git status --short`\" \" M CHANGELOG.md\"",
        ]
    );
    // Two turns: the compact summary starts none and is not shown.
    assert_eq!(items.last().map(|i| i.turn.0), Some(2));
    assert!(items.iter().all(|i| !show(i).contains("being continued")));
    let diffs = items
        .iter()
        .filter(|i| i.turn.0 == 1 && matches!(i.content, ThreadContent::Diff { .. }))
        .count();
    assert_eq!(diffs, 7, "six files, one twice");
    let failed = items.iter().filter(|i| {
        matches!(
            i.content,
            ThreadContent::Tool {
                status: ToolStatus::Error,
                ..
            }
        )
    });
    assert_eq!(failed.count(), 1);
    let longest = items
        .iter()
        .filter_map(|i| match &i.content {
            ThreadContent::Assistant { text } if i.turn.0 == 1 => {
                Some(text.split_whitespace().count())
            }
            _ => None,
        })
        .max();
    assert!(
        longest > Some(1500),
        "turn 1 ends in a long reply (past a TL;DR's threshold): {longest:?} words"
    );
}
