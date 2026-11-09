//! Short text renderings of tool calls and results.

use std::fmt::Write as _;

use serde_json::Value;

use crate::record::{Block, Content};

/// Size caps for rendered strings, in bytes. Transcripts can embed whole
/// files; the thread view only needs enough to read at a glance, and every
/// item is shipped to a phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// User and assistant message text.
    pub text: usize,
    /// The one-line rendering of a tool's input.
    pub tool_input: usize,
    /// A tool's output.
    pub tool_output: usize,
    /// A diff body; larger patches are dropped (the line counts remain).
    pub patch: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            text: 64 * 1024,
            tool_input: 2 * 1024,
            tool_output: 16 * 1024,
            patch: 64 * 1024,
        }
    }
}

/// Cuts `s` to at most `max` bytes (on a char boundary) and says how much
/// was dropped.
pub(crate) fn truncate(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    s.truncate(end);
    let _ = write!(s, "\n… [{dropped} more bytes]");
    s
}

/// The text of message content: text blocks joined by blank lines, images
/// as `[image]`. Tool blocks are not text and are left out.
pub(crate) fn content_text(content: &Content) -> String {
    match content {
        Content::Empty => String::new(),
        Content::Text(text) => text.clone(),
        Content::Blocks(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    Block::Image => Some("[image]"),
                    _ => None,
                })
                .collect();
            parts.join("\n\n")
        }
    }
}

/// A one-line summary of a tool call, chosen per tool: the shell command for
/// `Bash`, the path for file tools, the pattern for searches, and so on.
/// Unknown tools (MCP servers, new built-ins) fall back to their first
/// recognisable argument, then to compact JSON.
pub(crate) fn tool_input(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let rendered = match name {
        "Bash" | "BashOutput" | "PowerShell" => field("command").map(str::to_owned),
        "Read" | "Write" | "Edit" | "MultiEdit" => field("file_path").map(str::to_owned),
        "NotebookEdit" => field("notebook_path").map(str::to_owned),
        "Glob" | "Grep" => field("pattern").map(|pattern| match field("path") {
            Some(path) => format!("{pattern} in {path}"),
            None => pattern.to_owned(),
        }),
        "Agent" | "Task" => field("description").map(|desc| match field("subagent_type") {
            Some(kind) => format!("{kind}: {desc}"),
            None => desc.to_owned(),
        }),
        "SendMessage" => {
            let to = field("to").or_else(|| field("recipient"));
            let body = field("summary")
                .or_else(|| field("message"))
                .or_else(|| field("content"));
            match (to, body) {
                (Some(to), Some(body)) => Some(format!("→ {to}: {body}")),
                (Some(to), None) => Some(format!("→ {to}")),
                (None, body) => body.map(str::to_owned),
            }
        }
        "TodoWrite" => input
            .get("todos")
            .and_then(Value::as_array)
            .map(|todos| format!("{} todos", todos.len())),
        _ => None,
    };
    rendered
        .or_else(|| {
            [
                "command",
                "file_path",
                "path",
                "pattern",
                "url",
                "query",
                "description",
                "prompt",
                "skill",
            ]
            .iter()
            .find_map(|key| field(key))
            .map(str::to_owned)
        })
        .unwrap_or_else(|| match input {
            Value::Null => String::new(),
            other => other.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate("short".into(), 10), "short");
        let cut = truncate("ééé".into(), 3);
        assert_eq!(cut, "é\n… [4 more bytes]");
    }

    #[test]
    fn renders_common_tools() {
        let cases = [
            (
                "Bash",
                json!({"command": "cargo test", "description": "Run tests"}),
                "cargo test",
            ),
            (
                "Read",
                json!({"file_path": "/w/src/lib.rs", "limit": 50}),
                "/w/src/lib.rs",
            ),
            (
                "Grep",
                json!({"pattern": "fn main", "path": "src"}),
                "fn main in src",
            ),
            (
                "Agent",
                json!({"description": "Find callers", "subagent_type": "Explore", "prompt": "…"}),
                "Explore: Find callers",
            ),
            (
                "SendMessage",
                json!({"to": "main", "message": "done"}),
                "→ main: done",
            ),
            ("TodoWrite", json!({"todos": [{}, {}]}), "2 todos"),
            (
                "WebFetch",
                json!({"url": "https://example.com", "prompt": "summarize"}),
                "https://example.com",
            ),
            ("mcp__thing__do", json!({"x": 1}), r#"{"x":1}"#),
            ("Mystery", Value::Null, ""),
        ];
        for (name, input, want) in cases {
            assert_eq!(tool_input(name, &input), want, "{name}");
        }
    }

    #[test]
    fn content_text_joins_text_and_images() {
        let content: Content = serde_json::from_value(json!([
            {"type": "text", "text": "look"},
            {"type": "image", "source": {"type": "base64", "data": "…"}},
            {"type": "tool_result", "tool_use_id": "t", "content": "no"},
        ]))
        .expect("content");
        assert_eq!(content_text(&content), "look\n\n[image]");
    }
}
