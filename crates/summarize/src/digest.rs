//! The model's view of one turn: a compact, size-capped digest of its
//! thread items (design §6.4: "the transcript slice for the turn plus
//! `git diff --stat`. Never the whole repo.").

use std::fmt::Write as _;

use protocol::{ThreadContent, ThreadItem, ToolStatus};

use crate::text::{collapse_whitespace, truncate_bytes};

/// A turn with more assistant output words than this is long (design §6.4).
pub const LONG_TURN_WORDS: usize = 1500;

/// A turn that changed more files than this is long (design §6.4).
pub const LONG_TURN_FILES: usize = 5;

/// Size caps for a [`TurnDigest`], in bytes unless noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DigestLimits {
    /// The user's message(s) that started the turn.
    pub prompt: usize,
    /// The final assistant message (usually the agent's own wrap-up).
    pub final_message: usize,
    /// All earlier assistant messages together; the most recent are kept.
    pub earlier_messages: usize,
    /// One earlier assistant message.
    pub earlier_message: usize,
    /// One tool-call line.
    pub tool_line: usize,
    /// Number of tool-call lines; the most recent are kept.
    pub tool_lines: usize,
    /// Number of changed-file lines.
    pub files: usize,
    /// The caller's `git diff --stat` text.
    pub diff_stat: usize,
    /// The whole digest.
    pub total: usize,
}

impl Default for DigestLimits {
    fn default() -> Self {
        Self {
            prompt: 2 * 1024,
            final_message: 6 * 1024,
            earlier_messages: 4 * 1024,
            earlier_message: 1024,
            tool_line: 200,
            tool_lines: 40,
            files: 40,
            diff_stat: 4 * 1024,
            total: 24 * 1024,
        }
    }
}

impl DigestLimits {
    /// Caps for a headline digest: a fraction of a TL;DR's, since one
    /// sentence needs the ask and the wrap-up, not the whole turn. Keeps
    /// the call cheap.
    #[must_use]
    pub fn headline() -> Self {
        Self {
            prompt: 1024,
            final_message: 3 * 1024,
            earlier_messages: 1024,
            earlier_message: 512,
            tool_line: 160,
            tool_lines: 12,
            files: 12,
            diff_stat: 1024,
            total: 8 * 1024,
        }
    }

    /// Caps for a status-line digest of a turn still in progress: weighted
    /// to its tail (the latest message and the most recent tool calls),
    /// since the line says what the agent is doing *now*. Smaller still
    /// than a headline's: it is made every few minutes while an agent
    /// works.
    #[must_use]
    pub fn status() -> Self {
        Self {
            prompt: 512,
            final_message: 2 * 1024,
            earlier_messages: 512,
            earlier_message: 256,
            tool_line: 160,
            tool_lines: 12,
            files: 8,
            diff_stat: 0,
            total: 6 * 1024,
        }
    }
}

/// Size measures of a turn, used for the "long turn" threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnStats {
    /// Words of assistant text.
    pub output_words: usize,
    /// Distinct files changed: the larger of the turn's diff items and the
    /// count in the caller's `git diff --stat` summary line.
    pub files_changed: usize,
    /// Lines added across the turn's diff items.
    pub lines_added: u64,
    /// Lines removed across the turn's diff items.
    pub lines_removed: u64,
    /// Tool calls.
    pub tool_calls: usize,
    /// Tool calls that failed.
    pub failed_tool_calls: usize,
}

impl TurnStats {
    /// Measures a turn's items, plus an optional `git diff --stat` text.
    #[must_use]
    pub fn measure(items: &[ThreadItem], diff_stat: Option<&str>) -> Self {
        let mut stats = Self::default();
        let mut paths: Vec<&str> = Vec::new();
        for item in items {
            match &item.content {
                ThreadContent::Assistant { text } => {
                    stats.output_words += text.split_whitespace().count();
                }
                ThreadContent::Tool { status, .. } => {
                    stats.tool_calls += 1;
                    if *status == ToolStatus::Error {
                        stats.failed_tool_calls += 1;
                    }
                }
                ThreadContent::Diff {
                    path,
                    added,
                    removed,
                    ..
                } => {
                    stats.lines_added += u64::from(*added);
                    stats.lines_removed += u64::from(*removed);
                    if !paths.contains(&path.as_str()) {
                        paths.push(path);
                    }
                }
                ThreadContent::User { .. }
                | ThreadContent::Aside { .. }
                | ThreadContent::Unknown => {}
            }
        }
        let stat_files = diff_stat.and_then(diff_stat_files).unwrap_or(0);
        stats.files_changed = paths.len().max(stat_files);
        stats
    }

    /// Whether the turn is long enough to deserve a TL;DR: more than
    /// [`LONG_TURN_WORDS`] words of output or more than [`LONG_TURN_FILES`]
    /// files changed (design §6.4).
    #[must_use]
    pub fn is_long(&self) -> bool {
        self.output_words > LONG_TURN_WORDS || self.files_changed > LONG_TURN_FILES
    }
}

/// The file count from a `git diff --stat` summary line
/// (` 7 files changed, 120 insertions(+), 3 deletions(-)`).
fn diff_stat_files(stat: &str) -> Option<usize> {
    stat.lines().rev().find_map(|line| {
        let line = line.trim();
        let (count, rest) = line.split_once(' ')?;
        let rest = rest.trim_start();
        if rest.starts_with("files changed") || rest.starts_with("file changed") {
            count.parse().ok()
        } else {
            None
        }
    })
}

/// A compact, size-capped plain-text digest of one turn, ready to be put
/// in the model's data block. Build it with [`TurnDigest::build`].
///
/// It holds, in this order: turn statistics, the user's prompt, the labels
/// of what others sent in and of the user's own shell commands (marked as
/// context, not the request), tool calls
/// (Bash commands with their exit status), changed files with +/- counts,
/// the caller's `git diff --stat`, earlier assistant messages and the final
/// assistant message. Patches and tool output are never included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnDigest {
    text: String,
    stats: TurnStats,
    has_content: bool,
}

impl TurnDigest {
    /// Digests a turn's items (e.g. `transcripts::Thread::turn`) with the
    /// default [`DigestLimits`]. `diff_stat` is optional `git diff --stat`
    /// output for the turn, supplied by the caller.
    #[must_use]
    pub fn build(items: &[ThreadItem], diff_stat: Option<&str>) -> Self {
        Self::build_with(items, diff_stat, &DigestLimits::default())
    }

    /// Like [`build`](Self::build), with explicit limits.
    #[must_use]
    pub fn build_with(
        items: &[ThreadItem],
        diff_stat: Option<&str>,
        limits: &DigestLimits,
    ) -> Self {
        let stats = TurnStats::measure(items, diff_stat);
        let mut head = String::new();
        write_stats(&mut head, &stats);
        let header = head.len();
        write_prompt(&mut head, items, limits);
        let mut tail = String::new();
        write_final_message(&mut tail, items, limits);
        let mut middle = String::new();
        write_others(&mut middle, items, limits);
        write_tools(&mut middle, items, limits);
        write_files(&mut middle, items, limits);
        if let Some(stat) = diff_stat.map(str::trim).filter(|s| !s.is_empty()) {
            let _ = write!(
                middle,
                "\n## git diff --stat\n{}\n",
                truncate_bytes(stat, limits.diff_stat)
            );
        }
        write_earlier_messages(&mut middle, items, limits);
        let has_content = head.len() > header || !middle.is_empty() || !tail.is_empty();

        // The prompt and the wrap-up are reserved first, so they are never
        // cut by the total; the other sections fill what is left, losing
        // earlier messages first (they come last).
        let head = fit(&head, limits.total);
        let tail = fit(&tail, limits.total.saturating_sub(head.len()));
        let middle = fit(
            &middle,
            limits.total.saturating_sub(head.len() + tail.len()),
        );
        Self {
            has_content,
            text: format!("{head}{middle}{tail}"),
            stats,
        }
    }

    /// The digest text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The turn's size measures.
    #[must_use]
    pub fn stats(&self) -> &TurnStats {
        &self.stats
    }

    /// Whether the turn had neither user nor assistant text, tool calls nor
    /// changes: nothing worth summarizing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.has_content
    }
}

fn write_stats(out: &mut String, stats: &TurnStats) {
    let _ = writeln!(
        out,
        "Turn statistics: {} words of assistant output; {} files changed (+{} -{}); \
         {} tool calls ({} failed).",
        stats.output_words,
        stats.files_changed,
        stats.lines_added,
        stats.lines_removed,
        stats.tool_calls,
        stats.failed_tool_calls,
    );
}

fn write_prompt(out: &mut String, items: &[ThreadItem], limits: &DigestLimits) {
    let prompt: Vec<&str> = items
        .iter()
        .filter_map(|item| match &item.content {
            ThreadContent::User { text } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .collect();
    if !prompt.is_empty() {
        let _ = write!(
            out,
            "\n## User prompt\n{}\n",
            truncate_bytes(&prompt.join("\n\n"), limits.prompt)
        );
    }
}

/// What arrived during the turn without being the user's request: relayed
/// messages, notifications and the user's own `!` shell commands, by their
/// label (the most recent [`DigestLimits::tool_lines`]), so the model can
/// mention them without mistaking them for what the user asked. Their text
/// (message bodies, command output) is left out.
fn write_others(out: &mut String, items: &[ThreadItem], limits: &DigestLimits) {
    let lines: Vec<String> = items
        .iter()
        .filter_map(|item| match &item.content {
            ThreadContent::Aside { label, .. } => Some(truncate_bytes(
                &collapse_whitespace(label),
                limits.tool_line,
            )),
            _ => None,
        })
        .collect();
    if lines.is_empty() {
        return;
    }
    let skipped = lines.len().saturating_sub(limits.tool_lines);
    out.push_str("\n## Also in the turn (context only, not the user's request)\n");
    for line in &lines[skipped..] {
        let _ = writeln!(out, "- {line}");
    }
}

fn write_tools(out: &mut String, items: &[ThreadItem], limits: &DigestLimits) {
    let lines: Vec<String> = items
        .iter()
        .filter_map(|item| match &item.content {
            ThreadContent::Tool {
                name,
                input,
                output,
                status,
            } => Some(tool_line(name, input, output.as_deref(), *status, limits)),
            _ => None,
        })
        .collect();
    if lines.is_empty() {
        return;
    }
    let skipped = lines.len().saturating_sub(limits.tool_lines);
    let _ = writeln!(out, "\n## Tool calls ({})", lines.len());
    if skipped > 0 {
        let _ = writeln!(out, "({skipped} earlier calls omitted)");
    }
    for line in &lines[skipped..] {
        let _ = writeln!(out, "- {line}");
    }
}

fn tool_line(
    name: &str,
    input: &str,
    output: Option<&str>,
    status: ToolStatus,
    limits: &DigestLimits,
) -> String {
    let outcome = match status {
        ToolStatus::Ok if name == "Bash" => "ok (exit 0)".to_owned(),
        ToolStatus::Ok => "ok".to_owned(),
        ToolStatus::Error => match output.and_then(exit_code) {
            Some(code) => format!("failed (exit {code})"),
            None => "failed".to_owned(),
        },
        ToolStatus::Running => "still running".to_owned(),
        ToolStatus::Unknown => "unknown".to_owned(),
    };
    let input = collapse_whitespace(&truncate_bytes(input, limits.tool_line));
    truncate_bytes(&format!("{name}: {input} → {outcome}"), limits.tool_line)
}

/// The exit code from Claude Code's Bash error output (`Exit code 101 …`).
fn exit_code(output: &str) -> Option<i32> {
    let rest = output.trim_start().strip_prefix("Exit code ")?;
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

fn write_files(out: &mut String, items: &[ThreadItem], limits: &DigestLimits) {
    let mut files: Vec<(&str, u64, u64)> = Vec::new();
    for item in items {
        if let ThreadContent::Diff {
            path,
            added,
            removed,
            ..
        } = &item.content
        {
            match files.iter_mut().find(|(p, ..)| p == path) {
                Some(entry) => {
                    entry.1 += u64::from(*added);
                    entry.2 += u64::from(*removed);
                }
                None => files.push((path, u64::from(*added), u64::from(*removed))),
            }
        }
    }
    if files.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Files changed ({})", files.len());
    for (path, added, removed) in files.iter().take(limits.files) {
        let path = truncate_bytes(&collapse_whitespace(path), limits.tool_line);
        let _ = writeln!(out, "- {path} +{added} -{removed}");
    }
    if files.len() > limits.files {
        let _ = writeln!(out, "({} more files omitted)", files.len() - limits.files);
    }
}

/// Room kept for the marker [`truncate_bytes`] appends.
const CUT_MARKER_BYTES: usize = 32;

/// `s` if it fits in `budget` bytes, else cut (marker included) to fit.
fn fit(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        s.to_owned()
    } else if budget < CUT_MARKER_BYTES {
        String::new()
    } else {
        truncate_bytes(s, budget - CUT_MARKER_BYTES)
    }
}

/// The turn's non-blank assistant messages, in order.
fn assistant_messages(items: &[ThreadItem]) -> Vec<&str> {
    items
        .iter()
        .filter_map(|item| match &item.content {
            ThreadContent::Assistant { text } if !text.trim().is_empty() => Some(text.trim()),
            _ => None,
        })
        .collect()
}

fn write_final_message(out: &mut String, items: &[ThreadItem], limits: &DigestLimits) {
    if let Some(last) = assistant_messages(items).last() {
        let _ = write!(
            out,
            "\n## Final assistant message\n{}\n",
            truncate_bytes(last, limits.final_message)
        );
    }
}

fn write_earlier_messages(out: &mut String, items: &[ThreadItem], limits: &DigestLimits) {
    let messages = assistant_messages(items);
    let Some((_, earlier)) = messages.split_last() else {
        return;
    };
    // Keep the most recent earlier messages that fit the budget.
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0;
    for message in earlier.iter().rev() {
        let message = truncate_bytes(message, limits.earlier_message);
        if used + message.len() > limits.earlier_messages {
            break;
        }
        used += message.len();
        kept.push(message);
    }
    if !kept.is_empty() {
        let omitted = earlier.len() - kept.len();
        out.push_str("\n## Earlier assistant messages\n");
        if omitted > 0 {
            let _ = writeln!(out, "({omitted} earlier messages omitted)");
        }
        for message in kept.iter().rev() {
            let _ = write!(out, "{message}\n---\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::{ThreadCursor, TurnIndex};

    use super::*;

    fn item(content: ThreadContent) -> ThreadItem {
        ThreadItem {
            cursor: ThreadCursor(0),
            turn: TurnIndex(1),
            rev: 0,
            at: None,
            content,
        }
    }

    fn user(text: &str) -> ThreadItem {
        item(ThreadContent::User { text: text.into() })
    }

    fn assistant(text: &str) -> ThreadItem {
        item(ThreadContent::Assistant { text: text.into() })
    }

    fn tool(name: &str, input: &str, output: Option<&str>, status: ToolStatus) -> ThreadItem {
        item(ThreadContent::Tool {
            name: name.into(),
            input: input.into(),
            output: output.map(Into::into),
            status,
        })
    }

    fn diff(path: &str, added: u32, removed: u32) -> ThreadItem {
        item(ThreadContent::Diff {
            path: path.into(),
            added,
            removed,
            patch: Some("@@ secret patch body @@".into()),
        })
    }

    #[test]
    fn digest_has_every_section_and_no_patches_or_tool_output() {
        let items = [
            user("fix the flaky test"),
            assistant("Looking at it."),
            tool(
                "Bash",
                "cargo test",
                Some("Exit code 101\nfailures: ..."),
                ToolStatus::Error,
            ),
            tool("Edit", "src/lib.rs", Some("edited"), ToolStatus::Ok),
            diff("src/lib.rs", 10, 2),
            diff("src/lib.rs", 1, 1),
            tool("Bash", "cargo test", Some("all passed"), ToolStatus::Ok),
            assistant("Fixed: the test raced on a temp dir."),
        ];
        let digest = TurnDigest::build(
            &items,
            Some(" 1 file changed, 11 insertions(+), 3 deletions(-)"),
        );
        let text = digest.as_str();
        for want in [
            "## User prompt\nfix the flaky test",
            "- Bash: cargo test → failed (exit 101)",
            "- Edit: src/lib.rs → ok",
            "- Bash: cargo test → ok (exit 0)",
            "## Files changed (1)\n- src/lib.rs +11 -3",
            "## git diff --stat\n1 file changed",
            "## Earlier assistant messages\nLooking at it.",
            "## Final assistant message\nFixed: the test raced on a temp dir.",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        assert!(!text.contains("secret patch body"));
        assert!(!text.contains("all passed"));
        assert!(!text.contains("failures: ..."));
        assert!(!digest.is_empty());
    }

    #[test]
    fn asides_are_context_not_the_prompt() {
        let aside = |label: &str, text: &str| {
            item(ThreadContent::Aside {
                label: label.into(),
                text: text.into(),
            })
        };
        let items = [
            user("ship the parser fix"),
            aside("Report from an agent (code-review)", "Looks good to me."),
            aside("cargo test completed", "all 12 passed"),
            aside("You ran `git log -1`", "abc123 fix"),
            assistant("Shipped."),
        ];
        let text = TurnDigest::build(&items, None).as_str().to_owned();
        assert!(
            text.contains("## User prompt\nship the parser fix\n\n##"),
            "{text}"
        );
        for want in [
            "## Also in the turn (context only, not the user's request)",
            "- Report from an agent (code-review)\n",
            "- cargo test completed\n",
            "- You ran `git log -1`\n",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        assert!(!text.contains("abc123"), "their text stays out");
        assert!(!text.contains("Looks good"), "their text stays out");
    }

    #[test]
    fn digest_is_size_capped() {
        let huge = "word ".repeat(5_000);
        let mut items = vec![user(&huge)];
        for i in 0..500 {
            items.push(tool(
                "Bash",
                &format!("echo {i} {huge}"),
                None,
                ToolStatus::Ok,
            ));
            items.push(diff(&format!("src/f{i}.rs"), 1, 0));
            items.push(assistant(&huge));
        }
        let limits = DigestLimits::default();
        let digest = TurnDigest::build(&items, Some(&huge));
        assert!(
            digest.as_str().len() <= limits.total,
            "{}",
            digest.as_str().len()
        );
        assert!(digest.as_str().contains("(460 earlier calls omitted)"));
        assert!(digest.as_str().contains("(460 more files omitted)"));
        assert_eq!(digest.stats().files_changed, 500);
    }

    #[test]
    fn prompt_and_wrap_up_are_never_cut_by_the_total() {
        let filler = "filler ".repeat(2_000);
        let wrap_up = format!("WRAP-UP-START {} WRAP-UP-END", "w".repeat(5_000));
        let mut items = vec![user("PROMPT-START do it PROMPT-END")];
        for i in 0..200 {
            items.push(tool(
                "Bash",
                &format!("cmd {i} {filler}"),
                None,
                ToolStatus::Ok,
            ));
            items.push(diff(&format!("src/{i}/{filler}"), 1, 1));
            items.push(assistant(&filler));
        }
        items.push(assistant(&wrap_up));
        let limits = DigestLimits {
            total: 8 * 1024,
            ..DigestLimits::default()
        };
        let digest = TurnDigest::build_with(&items, Some(&filler), &limits);
        let text = digest.as_str();
        assert!(text.len() <= limits.total, "{}", text.len());
        assert!(text.contains("PROMPT-START do it PROMPT-END"));
        assert!(text.ends_with(&format!("## Final assistant message\n{wrap_up}\n")));
        assert!(text.contains("## Tool calls (200)"));
    }

    #[test]
    fn headline_digests_are_much_smaller_than_tldr_digests() {
        let limits = DigestLimits::headline();
        assert!(limits.total * 2 < DigestLimits::default().total);
        let mut items = vec![user(&"please ".repeat(1000))];
        items.extend((0..40).map(|_| assistant(&"word ".repeat(2000))));
        let digest = TurnDigest::build_with(&items, None, &limits);
        assert!(
            digest.as_str().len() <= limits.total,
            "{}",
            digest.as_str().len()
        );
        assert!(digest.as_str().len() * 2 < TurnDigest::build(&items, None).as_str().len());
    }

    #[test]
    fn empty_turn_is_empty() {
        assert!(TurnDigest::build(&[], None).is_empty());
        assert!(TurnDigest::build(&[item(ThreadContent::Unknown)], Some("  ")).is_empty());
    }

    #[test]
    fn long_turn_threshold() {
        let short = TurnStats::measure(&[assistant(&"w ".repeat(1500))], None);
        assert!(!short.is_long());
        let wordy = TurnStats::measure(&[assistant(&"w ".repeat(1501))], None);
        assert!(wordy.is_long());

        let five: Vec<_> = (0..5).map(|i| diff(&format!("f{i}"), 1, 1)).collect();
        assert!(!TurnStats::measure(&five, None).is_long());
        let mut six = five.clone();
        six.push(diff("f5", 1, 1));
        assert!(TurnStats::measure(&six, None).is_long());
        // The same path twice is one file.
        let mut repeat = five;
        repeat.push(diff("f0", 1, 1));
        assert!(!TurnStats::measure(&repeat, None).is_long());
        // Files changed outside the agent's edit tools count via diff --stat.
        let stat = " a | 1 +\n 6 files changed, 6 insertions(+)";
        assert!(TurnStats::measure(&[], Some(stat)).is_long());
    }

    #[test]
    fn parses_exit_codes_and_stat_lines() {
        assert_eq!(exit_code("Exit code 2\nboom"), Some(2));
        assert_eq!(exit_code("boom"), None);
        assert_eq!(diff_stat_files(" 1 file changed, 1 insertion(+)"), Some(1));
        assert_eq!(diff_stat_files(" 12 files changed"), Some(12));
        assert_eq!(diff_stat_files("nothing"), None);
    }
}
