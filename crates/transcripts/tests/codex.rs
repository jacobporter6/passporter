//! Reading Codex rollouts: the real codex-cli 0.166 fixtures (see
//! `fixtures/codex/README.md`) and synthetic records for shapes they lack.

use std::path::Path;

use protocol::{ThreadContent, ThreadItem, ToolStatus, TurnIndex};
use serde_json::json;
use transcripts::{CodexLocator, CodexSession, Format, Scope, Thread, TranscriptReader};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex")
        .join(name);
    std::fs::read_to_string(&path).expect("fixture")
}

fn thread_of(text: &str) -> Thread {
    let mut thread = Thread::new(Scope::Main).with_format(Format::Codex);
    for line in text.lines() {
        thread.ingest(line.as_bytes());
    }
    thread
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
            output
                .as_deref()
                .map_or("-", |o| if o.is_empty() { "" } else { "yes" })
        ),
        ThreadContent::Diff {
            path,
            added,
            removed,
            patch,
        } => format!("diff {path} +{added} -{removed} patch={}", patch.is_some()),
        // Claude-only kinds: a Codex thread never has them.
        other @ (ThreadContent::Aside { .. } | ThreadContent::Unknown) => {
            format!("unexpected {other:?}")
        }
    };
    format!("{} {body}", item.turn.0)
}

fn shown(thread: &Thread) -> Vec<String> {
    thread.items().iter().map(show).collect()
}

#[test]
fn an_exec_session_reads_as_a_thread() {
    let thread = thread_of(&fixture("exec_tool_call.jsonl"));
    assert_eq!(
        shown(&thread),
        [
            "1 user \"Create a file hello.txt containing the word hi, run ls -la, then tell me in one sentence what you did.\"",
            "1 assistant \"I’ll try to create `hello.txt` and run `ls -la`, though this session has read-only filesystem access.\"",
            "1 tool Bash \"printf hi > hello.txt\\nls -la\" Ok out=yes",
            "1 assistant \"I ran `ls -la`, but couldn’t create `hello.txt` containing `hi` because the filesystem is read-only.\"",
        ]
    );
    assert_eq!(thread.current_turn(), Some(TurnIndex(1)));
    let diagnostics = thread.diagnostics();
    assert!(!diagnostics.has_malformed(), "{diagnostics}");
    assert_eq!(diagnostics.orphan_results, 0);
    assert!(diagnostics.unknown_types.is_empty(), "{diagnostics}");
    assert_eq!(diagnostics.skipped.get("codex:world_state"), Some(&1));
    assert_eq!(
        diagnostics.skipped.get("codex:message:other-role"),
        Some(&4)
    );
}

#[test]
fn a_declined_approval_ends_the_call_in_error() {
    let thread = thread_of(&fixture("interactive_declined_approval.jsonl"));
    assert_eq!(
        shown(&thread),
        [
            "1 user \"Create a file hello.txt containing the word hi.\"",
            "1 assistant \"I’ll create `hello.txt` containing `hi`.\"",
            "1 tool Bash \"printf 'hi\\\\n' > hello.txt\" Error out=yes",
        ]
    );
    assert!(!thread.diagnostics().has_malformed());
}

#[test]
fn unknown_records_and_payloads_are_skipped() {
    let lines = [
        json!({"type": "session_meta", "payload": {"cwd": "/w"}}),
        json!({"type": "brand_new_record", "payload": {"x": 1}}),
        json!({"type": "response_item", "payload": {"type": "hologram", "id": "h1"}}),
        json!({"type": "response_item", "payload": {"type": "reasoning", "summary": []}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}, {"type": "sticker"}]}}),
        json!({"no": "type"}),
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "x"}}),
    ];
    let mut text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    text.push_str("{not json\n");
    let thread = thread_of(&text);
    assert_eq!(shown(&thread), ["1 user \"hi\""]);
    let diagnostics = thread.diagnostics();
    assert_eq!(diagnostics.unknown_types.get("brand_new_record"), Some(&1));
    assert_eq!(
        diagnostics.skipped.get("codex:response_item:other"),
        Some(&2)
    );
    assert_eq!(
        diagnostics.malformed.get(transcripts::INVALID_JSON),
        Some(&1)
    );
    assert_eq!(diagnostics.malformed.get("codex:response_item"), Some(&1));
}

#[test]
fn function_calls_and_patches_become_tools_and_diffs() {
    let patch =
        "*** Begin Patch\n*** Update File: /w/src/lib.rs\n@@\n-old\n+new\n+more\n*** End Patch\n";
    let lines = [
        json!({"type": "session_meta", "payload": {"cwd": "/w"}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": "system rules"}]}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "# AGENTS.md instructions for /w\n\n<INSTRUCTIONS>"}]}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Fix it"}]}}),
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell",
            "arguments": "{\"command\":[\"bash\",\"-lc\",\"cargo test\"]}", "call_id": "c1"}}),
        json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c1",
            "output": "{\"output\":\"1 failed\",\"metadata\":{\"exit_code\":101}}"}}),
        json!({"type": "response_item", "payload": {"type": "custom_tool_call", "name": "apply_patch",
            "input": patch, "call_id": "c2"}}),
        json!({"type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": "c2",
            "output": "Success. Updated the following files:\nM src/lib.rs\n"}}),
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "apply_patch",
            "arguments": json!({"input": patch}).to_string(), "call_id": "c3"}}),
        json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c3",
            "output": "error: patch did not apply"}}),
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell",
            "arguments": json!({"command": ["apply_patch", patch]}).to_string(), "call_id": "c5"}}),
        json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c5",
            "output": "{\"output\":\"Success. Updated the following files:\\nM src/lib.rs\\n\",\"metadata\":{\"exit_code\":0}}"}}),
        json!({"type": "response_item", "payload": {"type": "local_shell_call", "call_id": "c4",
            "action": {"type": "exec", "command": ["ls"]}, "status": "completed"}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "next"}, {"type": "input_image", "image_url": "data:"}]}}),
    ];
    let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    let thread = thread_of(&text);
    assert_eq!(
        shown(&thread),
        [
            "1 user \"Fix it\"",
            "1 tool Bash \"cargo test\" Error out=yes",
            "1 tool apply_patch \"/w/src/lib.rs\" Ok out=yes",
            "1 diff src/lib.rs +2 -1 patch=true",
            // Not reported as applied: no diff (and no failure either:
            // free text is not searched for "error").
            "1 tool apply_patch \"/w/src/lib.rs\" Ok out=yes",
            // Sent through the shell tool: a diff all the same.
            "1 tool apply_patch \"/w/src/lib.rs\" Ok out=yes",
            "1 diff src/lib.rs +2 -1 patch=true",
            // Never answered: settled when the next turn starts.
            "1 tool Bash \"ls\" Unknown out=-",
            "2 user \"next\\n\\n[image]\"",
        ]
    );
    let ThreadContent::Diff { patch, .. } = &thread.items()[3].content else {
        panic!("a diff");
    };
    assert_eq!(patch.as_deref(), Some("@@\n-old\n+new\n+more\n"));
    assert_eq!(
        thread.items()[1].content,
        ThreadContent::Tool {
            name: "Bash".into(),
            input: "cargo test".into(),
            output: Some("1 failed".into()),
            status: ToolStatus::Error,
        }
    );
}

#[test]
fn a_reader_finds_and_follows_a_rollout() {
    let root = tempfile::tempdir().expect("temp dir");
    let sessions = root.path().join("sessions");
    let day = sessions.join("2026/11/08");
    std::fs::create_dir_all(&day).expect("mkdir");
    let path = day.join("rollout-2026-11-08T13-45-33-01a1bb68-3fa5-78d3-9809-5887b47e4fcc.jsonl");
    let text = fixture("exec_tool_call.jsonl");
    let (head, tail) = text.split_at(text.len() / 2);
    std::fs::write(&path, head).expect("write");

    let locator = CodexLocator::new(&sessions);
    let found = locator
        .find(&CodexSession::LatestIn {
            cwd: "/work/codex-probe".into(),
            not_before: None,
        })
        .expect("search");
    assert_eq!(found.as_deref(), Some(path.as_path()));

    let mut reader = TranscriptReader::new(&path, Scope::Main).with_format(Format::Codex);
    reader.refresh().expect("first read");
    let before = reader.thread().items().len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, tail.as_bytes()))
        .expect("append");
    let refresh = reader.refresh().expect("second read");
    assert_ne!(refresh.changed, []);
    assert_eq!(reader.thread().items().len(), 4);
    assert!(before < 4);
}

#[test]
fn a_message_sent_while_codex_works_stays_in_its_turn() {
    let user = |text: &str, turn: Option<&str>| {
        let mut payload = json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": text}]});
        if let Some(turn) = turn {
            payload["internal_chat_message_metadata_passthrough"] = json!({"turn_id": turn});
        }
        json!({"type": "response_item", "payload": payload})
    };
    // Codex writes `task_started` before a turn's first message, and
    // `turn_context` after it (so the latter must not be the fallback).
    let context = |turn: &str| json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": turn}});
    let lines = [
        context("t1"),
        user("go", Some("t1")),
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell",
            "arguments": "{\"command\":[\"cargo\",\"test\"]}", "call_id": "c1"}}),
        // Steering: same Codex turn (from its metadata).
        user("and the docs", Some("t1")),
        // Steering without metadata: the last `task_started` says t1.
        user("quickly", None),
        json!({"type": "response_item", "payload": {"type": "function_call_output",
            "call_id": "c1", "output": "ok"}}),
        context("t2"),
        user("next", None),
        json!({"type": "turn_context", "payload": {"turn_id": "t2", "cwd": "/w"}}),
    ];
    let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    let thread = thread_of(&text);
    assert_eq!(
        shown(&thread),
        [
            "1 user \"go\"",
            "1 tool Bash \"cargo test\" Ok out=yes",
            "1 user \"and the docs\"",
            "1 user \"quickly\"",
            "2 user \"next\"",
        ]
    );
    assert_eq!(thread.diagnostics().orphan_results, 0);
}
