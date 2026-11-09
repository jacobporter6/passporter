//! Corpus acceptance test: reads every transcript under `$PASSPORTER_CORPUS`
//! (the scrubbed corpus, see docs/corpus.md) and reports what was skipped.
//!
//! Dev workstation only; without the variable it returns early with a note.
//! Set `PASSPORTER_CORPUS_STRICT=1` to also fail on malformed records.
//! Only counts and record-type names are printed, never transcript content.
//!
//! It also checks how user-role records were classified (prompt, shell
//! command, agent message, notification, …): every `origin.kind` Claude
//! Code wrote must be one the reader knows. Records without an `origin`
//! (older Claude Code) are classified by markers and counted separately.
//!
//! Transcripts are the `*.jsonl` files under a `projects/` directory (the
//! scrubbed layout is `<dump>/projects/<slug>/...`): other dumps, such as
//! `~/.claude/history.jsonl` prompt history, are not transcripts. Under
//! `subagents/`, only `agent-*.jsonl` are (a workflow's `journal.jsonl` is not).

use std::fs;
use std::path::{Path, PathBuf};

use protocol::ThreadContent;
use transcripts::{Diagnostics, Scope, TranscriptReader};

#[test]
fn every_corpus_transcript_parses() {
    let Some(root) = std::env::var_os("PASSPORTER_CORPUS").map(PathBuf::from) else {
        eprintln!("corpus: PASSPORTER_CORPUS is unset; skipping (see docs/corpus.md)");
        return;
    };
    let strict = std::env::var_os("PASSPORTER_CORPUS_STRICT").is_some_and(|v| v == "1");

    let mut files = Vec::new();
    collect_jsonl(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "no *.jsonl under {}: wrong PASSPORTER_CORPUS?",
        root.display()
    );

    let mut total = Diagnostics::default();
    let mut counts = [0_u64; 6];
    let mut failures = Vec::new();
    for path in &files {
        let scope = if path.components().any(|c| c.as_os_str() == "subagents") {
            Scope::Subagent
        } else {
            Scope::Main
        };
        let mut reader = TranscriptReader::new(path, scope);
        if let Err(err) = reader.refresh() {
            failures.push(format!("{}: {err}", path.display()));
            continue;
        }
        let thread = reader.thread();
        check_invariants(path, thread.items());
        for item in thread.items() {
            let slot = match item.content {
                ThreadContent::User { .. } => 0,
                ThreadContent::Assistant { .. } => 1,
                ThreadContent::Tool { .. } => 2,
                ThreadContent::Diff { .. } => 3,
                ThreadContent::Aside { .. } => 4,
                ThreadContent::Unknown => 5,
            };
            counts[slot] += 1;
        }
        if strict && thread.diagnostics().has_malformed() {
            failures.push(format!(
                "{}: malformed records {:?}",
                path.display(),
                thread.diagnostics().malformed
            ));
        }
        total.merge(thread.diagnostics());
    }

    let [user, assistant, tool, diff, aside, unknown] = counts;
    eprintln!(
        "corpus: {} transcripts\nitems: user {user}, assistant {assistant}, tool {tool}, \
         diff {diff}, aside {aside}, unknown {unknown}\n{total}",
        files.len()
    );
    let unclassified: Vec<&String> = total
        .said
        .keys()
        .filter(|key| key.contains("unknown origin"))
        .collect();
    assert!(
        unclassified.is_empty(),
        "user records with an origin this reader predates: {unclassified:?}"
    );
    assert!(
        failures.is_empty(),
        "corpus failures:\n{}",
        failures.join("\n")
    );
}

/// Cursors strictly increase and turns never go backwards.
fn check_invariants(path: &Path, items: &[protocol::ThreadItem]) {
    for pair in items.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert!(a.cursor < b.cursor, "{}: cursor order", path.display());
        assert!(a.turn <= b.turn, "{}: turn order", path.display());
    }
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("listing {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            collect_jsonl(&path, out);
        } else if kind.is_file() && is_transcript(&path) {
            out.push(path);
        }
    }
}

fn is_transcript(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let in_dir = |dir: &str| path.components().any(|c| c.as_os_str() == dir);
    if !name.ends_with(".jsonl") || !in_dir("projects") {
        return false;
    }
    !in_dir("subagents") || name.starts_with("agent-")
}
