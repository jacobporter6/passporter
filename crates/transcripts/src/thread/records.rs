//! Turning `user` and `assistant` records into thread items.

use protocol::{ThreadContent, Timestamp, ToolStatus, TurnIndex};
use serde_json::Value;

use super::said::{self, Basis, Said};
use super::{PendingTool, Scope, Thread, mark_unknown};
use crate::diff::Planned;
use crate::record::{Block, Content, Origin, Record, ToolUseResult};
use crate::render::{content_text, tool_input, truncate};
use crate::time::parse_rfc3339;

/// Tags Claude Code wraps around user-role text it writes on the user's
/// behalf. Text in them is not rendered and does not start a turn.
const NOTICE_TAGS: &[&str] = &[
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "bash-stdout",
    "bash-stderr",
    "command-name",
    "command-message",
    "command-args",
    "task-notification",
    "system-reminder",
];

/// The interruption marker Claude Code writes as user text.
const INTERRUPTED: &str = "[Request interrupted by user";

/// What remains of user-role text once leading injected blocks are cut:
/// known notice tags, hook output (`<…-hook>` tags) and the interruption
/// marker. An unterminated injected block swallows the rest.
fn strip_injected(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        if rest.starts_with(INTERRUPTED) {
            return "";
        }
        let Some(tag) = injected_tag(rest) else {
            return rest;
        };
        let close = format!("</{tag}>");
        match rest.find(&close) {
            Some(end) => {
                rest = rest
                    .get(end + close.len()..)
                    .unwrap_or_default()
                    .trim_start();
            }
            None => return "",
        }
    }
}

/// The name of the injected tag `text` opens with, if any.
fn injected_tag(text: &str) -> Option<&str> {
    let (tag, _) = text.strip_prefix('<')?.split_once('>')?;
    let well_formed = !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    (well_formed && (NOTICE_TAGS.contains(&tag) || tag.ends_with("-hook"))).then_some(tag)
}

/// The text the user typed in a message: each text block with injected
/// blocks cut, images as `[image]`, joined by blank lines. Empty when the
/// message was entirely injected.
fn typed_text(content: &Content) -> String {
    match content {
        Content::Empty => String::new(),
        Content::Text(text) => strip_injected(text).to_owned(),
        Content::Blocks(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(strip_injected(text)),
                    Block::Image => Some("[image]"),
                    _ => None,
                })
                .filter(|part| !part.trim().is_empty())
                .collect();
            parts.join("\n\n")
        }
    }
}

impl Thread {
    pub(super) fn user(&mut self, record: Record) {
        if let Some(why) = self.skip_reason(&record) {
            return self.diagnostics.skip(why);
        }
        let at = record.timestamp.as_deref().and_then(parse_rfc3339);
        let content = record.message.map(|m| m.content).unwrap_or_default();

        if let Content::Blocks(blocks) = &content {
            let results = blocks
                .iter()
                .filter(|b| matches!(b, Block::ToolResult { .. }))
                .count();
            // `toolUseResult` describes the record's tool result; with several
            // results in one record it is ambiguous, so it is ignored.
            let structured = (results == 1).then_some(&record.tool_use_result);
            for block in blocks {
                match block {
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => self.tool_result(tool_use_id, content, *is_error, structured, at),
                    Block::Unknown => self.diagnostics.unknown_blocks += 1,
                    _ => {}
                }
            }
        }

        let raw = content_text(&content);
        if raw.trim().is_empty() {
            return;
        }
        let said = self.classify(record.origin.0.as_ref(), record.is_meta, &raw);
        if said == Said::Prompt {
            let text = typed_text(&content);
            if text.trim().is_empty() {
                return self.diagnostics.skip("user:notice");
            }
            self.start_turn();
            let text = truncate(text, self.limits.text);
            self.push(at, ThreadContent::User { text });
        } else {
            self.not_typed(at, said);
        }
    }

    pub(super) fn assistant(&mut self, record: Record) {
        if let Some(why) = self.skip_reason(&record) {
            return self.diagnostics.skip(why);
        }
        let at = record.timestamp.as_deref().and_then(parse_rfc3339);
        let content = record.message.map(|m| m.content).unwrap_or_default();
        match content {
            Content::Empty => {}
            Content::Text(text) => self.assistant_text(at, text),
            Content::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        Block::Text { text } => self.assistant_text(at, text),
                        Block::ToolUse { id, name, input } => {
                            self.tool_use(at, id, name, &input, record.cwd.clone());
                        }
                        Block::Unknown => self.diagnostics.unknown_blocks += 1,
                        Block::ToolResult { .. }
                        | Block::Image
                        | Block::Thinking
                        | Block::RedactedThinking => {}
                    }
                }
            }
        }
    }

    /// A queued prompt (typed, or relayed, while the agent was busy) is
    /// shown in the current turn; other attachments are skipped.
    pub(super) fn attachment(&mut self, record: Record) {
        let skip = self.skip_reason(&record);
        let at = record.timestamp.as_deref().and_then(parse_rfc3339);
        let queued = record
            .attachment
            .filter(|a| a.kind.as_ref().and_then(Value::as_str) == Some("queued_command"))
            .and_then(|a| match a.prompt {
                Some(Value::String(prompt)) if !prompt.trim().is_empty() => {
                    Some((prompt, a.origin))
                }
                _ => None,
            });
        let (Some((prompt, origin)), None) = (queued, skip) else {
            return self.diagnostics.skip_type("attachment");
        };
        match self.classify(origin.0.as_ref(), false, &prompt) {
            Said::Prompt => {
                let text = truncate(prompt, self.limits.text);
                self.push(at, ThreadContent::User { text });
            }
            Said::Meta => self.diagnostics.skip_type("attachment"),
            said => self.not_typed(at, said),
        }
    }

    /// Classifies user-role text, counting the outcome in diagnostics.
    fn classify(&mut self, origin: Option<&Origin>, is_meta: bool, text: &str) -> Said {
        let (said, basis) = said::classify(origin, is_meta, text);
        let key = match basis {
            Basis::UnknownOrigin => format!(
                "{} (unknown origin {})",
                said.name(),
                origin.and_then(|o| o.kind.as_deref()).unwrap_or_default()
            ),
            basis => format!("{} ({basis})", said.name()),
        };
        self.diagnostics.said(&key);
        said
    }

    /// Shows what the user did not type in the current turn: a shell
    /// command, its output, or a relayed message. Injected context is
    /// skipped.
    fn not_typed(&mut self, at: Option<Timestamp>, said: Said) {
        match said {
            Said::Prompt | Said::Meta => self.diagnostics.skip("user:meta"),
            Said::ShellCommand(command) => {
                let label = said::shell_label(&command);
                let cursor = self.push(
                    at,
                    ThreadContent::Aside {
                        label,
                        text: String::new(),
                    },
                );
                self.shell = Some(cursor);
            }
            Said::ShellOutput(output) => {
                let Some(cursor) = self.shell.take() else {
                    return self.diagnostics.skip("user:shell-output");
                };
                let output = truncate(output, self.limits.tool_output);
                self.store.update(cursor, |content| {
                    if let ThreadContent::Aside { text, .. } = content {
                        *text = output;
                    }
                });
            }
            Said::Relayed(relay) => {
                let label = relay.label();
                let text = truncate(relay.text, self.limits.text);
                self.push(at, ThreadContent::Aside { label, text });
            }
        }
    }

    /// Why a `user`/`assistant` record is not rendered, as a diagnostics key.
    fn skip_reason(&self, record: &Record) -> Option<&'static str> {
        if record.is_sidechain && self.scope == Scope::Main {
            Some("sidechain")
        } else if record.is_compact_summary {
            Some("user:compact-summary")
        } else {
            None
        }
    }

    fn assistant_text(&mut self, at: Option<Timestamp>, text: String) {
        if !text.trim().is_empty() {
            let text = truncate(text, self.limits.text);
            self.push(at, ThreadContent::Assistant { text });
        }
    }

    fn tool_use(
        &mut self,
        at: Option<Timestamp>,
        id: String,
        name: String,
        input: &Value,
        cwd: Option<String>,
    ) {
        let rendered = truncate(tool_input(&name, input), self.limits.tool_input);
        let planned = Planned::new(&name, input, self.limits.patch);
        let cursor = self.push(
            at,
            ThreadContent::Tool {
                name,
                input: rendered,
                output: None,
                status: ToolStatus::Running,
            },
        );
        let pending = PendingTool {
            cursor,
            planned,
            diffs: Vec::new(),
            cwd,
        };
        // A reused id: the older call can no longer be told apart.
        if let Some(older) = self.pending.insert(id, pending) {
            mark_unknown(&mut self.store, older.cursor);
        }
        self.settle_stale();
    }

    fn tool_result(
        &mut self,
        tool_use_id: &str,
        content: &Content,
        is_error: Option<bool>,
        structured: Option<&ToolUseResult>,
        at: Option<Timestamp>,
    ) {
        let Some(pending) = self.pending.remove(tool_use_id) else {
            self.diagnostics.orphan_results += 1;
            return;
        };
        let status = if is_error == Some(true) {
            ToolStatus::Error
        } else {
            ToolStatus::Ok
        };
        let text = truncate(content_text(content), self.limits.tool_output);
        self.store.update(pending.cursor, |content| {
            if let ThreadContent::Tool {
                output,
                status: item_status,
                ..
            } = content
            {
                *output = (!text.is_empty()).then_some(text);
                *item_status = status;
            }
        });
        if status == ToolStatus::Ok
            && let Some(planned) = pending.planned
        {
            let fallback = ToolUseResult::default();
            let diff = planned.finish(
                structured.unwrap_or(&fallback),
                pending.cwd.as_deref(),
                self.limits.patch,
            );
            if let Some(diff) = diff {
                self.push(at, diff);
            }
        }
    }

    pub(super) fn start_turn(&mut self) {
        self.settle_pending();
        self.turn = Some(TurnIndex(self.turn.map_or(1, |t| t.0.saturating_add(1))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_blocks_are_cut_and_typed_text_kept() {
        for text in [
            "\n<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>",
            "  <task-notification><task-id>a1</task-id></task-notification>",
            "<system-reminder>be brief</system-reminder>",
            "<user-prompt-submit-hook>ok</user-prompt-submit-hook>",
            "<session-start-hook>context",
            "[Request interrupted by user for tool use]",
        ] {
            assert_eq!(strip_injected(text), "", "{text:?}");
        }
        assert_eq!(
            strip_injected("<system-reminder>todo changed</system-reminder>\nFix the parser"),
            "Fix the parser"
        );
        for text in [
            "Fix the hook in <x-hook>?",
            "<b>bold</b> request",
            "please run the pre-commit-hook",
        ] {
            assert_eq!(strip_injected(text), text, "{text:?}");
        }
    }
}
