//! A lenient serde model of Claude Code transcript records (2.1.x).
//!
//! Only the fields the normalizer uses are modelled; everything else is
//! skipped without being buffered. Shapes that vary between versions
//! (message content, `toolUseResult`) have hand-written visitors that accept
//! anything and keep what they recognise, so a surprising record degrades to
//! "nothing to render" instead of a parse error.

use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// One line of a transcript.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Record {
    /// Record type: `user`, `assistant`, `system`, `attachment`, …
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// RFC 3339 time the record was written.
    pub timestamp: Option<String>,
    /// Part of a subagent's conversation rather than the main thread.
    pub is_sidechain: bool,
    /// Injected context (caveats, skill bodies), not typed by the user.
    pub is_meta: bool,
    /// The synthetic summary that replaces history after `/compact`.
    pub is_compact_summary: bool,
    /// The session's working directory when the record was written.
    pub cwd: Option<String>,
    /// The API message, for `user` and `assistant` records.
    pub message: Option<Message>,
    /// Structured tool result Claude Code attaches to `user` records.
    pub tool_use_result: ToolUseResult,
    /// The payload of an `attachment` record.
    pub attachment: Option<Attachment>,
    /// Who produced a `user` record (Claude Code 2.1.x; older ones lack it).
    pub origin: LenientOrigin,
}

/// An `attachment` record's payload. Only queued prompts are rendered; any
/// other shape reads as an attachment of no interest.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Attachment {
    /// e.g. `queued_command`, `todo`, `file`.
    #[serde(rename = "type")]
    pub kind: Option<Value>,
    /// For `queued_command`: what was queued while the agent was busy.
    pub prompt: Option<Value>,
    /// For `queued_command`: who queued it.
    pub origin: LenientOrigin,
}

/// Who produced a user-role record or queued prompt: `kind` is `human` for
/// what the person typed, `peer` for a subagent's report or another
/// session's message, `task-notification` for a background task,
/// `coordinator` for the lead agent writing to a subagent.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Origin {
    pub kind: Option<String>,
    /// A peer's name, e.g. a subagent's `code-review`.
    pub name: Option<String>,
    /// A peer message's body, without Claude Code's wrapping.
    pub body: Option<String>,
    /// Set on messages from another session (absent on subagent reports).
    pub from_mode: Option<IgnoredAny>,
}

/// An [`Origin`], or `None` when absent or of an unexpected shape.
#[derive(Debug, Default)]
pub(crate) struct LenientOrigin(pub Option<Origin>);

/// An API message.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Message {
    /// String or array of content blocks.
    pub content: Content,
}

/// Message content: a bare string or a list of blocks.
#[derive(Debug, Default)]
pub(crate) enum Content {
    /// Absent or of an unexpected shape.
    #[default]
    Empty,
    /// A plain string.
    Text(String),
    /// Content blocks.
    Blocks(Vec<Block>),
}

/// One content block.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Block {
    /// Prose.
    Text {
        #[serde(default)]
        text: String,
    },
    /// A tool call by the assistant.
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    /// The result of a tool call, sent back as a user message.
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Content,
        #[serde(default)]
        is_error: Option<bool>,
    },
    /// An image (pasted screenshot, image tool output).
    Image,
    /// Extended thinking: never rendered.
    Thinking,
    /// Redacted extended thinking: never rendered.
    RedactedThinking,
    /// A block type this reader predates.
    #[serde(other)]
    Unknown,
}

/// The parts of `toolUseResult` used for diffs. Claude Code stores very
/// different shapes here per tool (objects, strings for errors, arrays), so
/// anything that is not an object reads as empty.
#[derive(Debug, Default)]
pub(crate) struct ToolUseResult {
    /// `Edit`/`MultiEdit`/`Write` hunks, already computed by Claude Code.
    pub structured_patch: Option<Vec<Hunk>>,
    /// `Write`: `create` or `update`.
    pub kind: Option<String>,
}

/// One hunk of a unified diff, as Claude Code records it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct Hunk {
    pub old_start: u64,
    pub old_lines: u64,
    pub new_start: u64,
    pub new_lines: u64,
    /// Lines prefixed with ` `, `-` or `+`.
    pub lines: Vec<String>,
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ContentVisitor)
    }
}

struct ContentVisitor;

impl<'de> Visitor<'de> for ContentVisitor {
    type Value = Content;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("message content")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Content, E> {
        Ok(Content::Text(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Content, E> {
        Ok(Content::Text(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Content, A::Error> {
        let mut blocks = Vec::new();
        while let Some(block) = seq.next_element::<LenientBlock>()? {
            if let Some(block) = block.0 {
                blocks.push(block);
            }
        }
        Ok(Content::Blocks(blocks))
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Content, A::Error> {
        ignore_map(map).map(|()| Content::Empty)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Content, E> {
        Ok(Content::Empty)
    }

    fn visit_none<E: de::Error>(self) -> Result<Content, E> {
        Ok(Content::Empty)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Content, E> {
        Ok(Content::Empty)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Content, E> {
        Ok(Content::Empty)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Content, E> {
        Ok(Content::Empty)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Content, E> {
        Ok(Content::Empty)
    }
}

/// A block, or `None` when a known block type has an unexpected shape (e.g.
/// a `tool_use` without an id). Buffers through [`Value`] so one odd block
/// cannot fail the whole record.
struct LenientBlock(Option<Block>);

impl<'de> Deserialize<'de> for LenientBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Ok(Self(Block::deserialize(value).ok()))
    }
}

impl<'de> Deserialize<'de> for LenientOrigin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Ok(Self(Origin::deserialize(value).ok()))
    }
}

impl<'de> Deserialize<'de> for ToolUseResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ToolUseResultVisitor)
    }
}

struct ToolUseResultVisitor;

impl<'de> Visitor<'de> for ToolUseResultVisitor {
    type Value = ToolUseResult;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a tool use result")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ToolUseResult, A::Error> {
        let mut result = ToolUseResult::default();
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match key.as_ref() {
                "structuredPatch" => {
                    let hunks: Value = map.next_value()?;
                    result.structured_patch = serde_json::from_value(hunks).ok();
                }
                "type" => result.kind = map.next_value::<Value>()?.as_str().map(str::to_owned),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(result)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ToolUseResult, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(ToolUseResult::default())
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<ToolUseResult, E> {
        Ok(ToolUseResult::default())
    }

    fn visit_unit<E: de::Error>(self) -> Result<ToolUseResult, E> {
        Ok(ToolUseResult::default())
    }

    fn visit_none<E: de::Error>(self) -> Result<ToolUseResult, E> {
        Ok(ToolUseResult::default())
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<ToolUseResult, E> {
        Ok(ToolUseResult::default())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<ToolUseResult, E> {
        Ok(ToolUseResult::default())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<ToolUseResult, E> {
        Ok(ToolUseResult::default())
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<ToolUseResult, E> {
        Ok(ToolUseResult::default())
    }
}

fn ignore_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

/// Just the record type, for lines that fail to parse as a [`Record`].
#[derive(Debug, Deserialize)]
pub(crate) struct Kind {
    #[serde(rename = "type")]
    pub kind: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Record {
        serde_json::from_str(json).expect("record parses")
    }

    #[test]
    fn content_may_be_a_string_or_blocks() {
        let text = parse(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#);
        assert!(matches!(text.message.expect("msg").content, Content::Text(t) if t == "hi"));

        let blocks = parse(
            r#"{"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"hmm","signature":"x"},
                {"type":"text","text":"ok"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}},
                {"type":"hologram","depth":3},
                {"type":"tool_use","name":"missing id"}
            ]}}"#,
        );
        let Content::Blocks(blocks) = blocks.message.expect("msg").content else {
            panic!("blocks expected");
        };
        assert_eq!(blocks.len(), 4, "the malformed tool_use is dropped");
        assert!(matches!(blocks[0], Block::Thinking));
        assert!(matches!(&blocks[1], Block::Text { text } if text == "ok"));
        assert!(matches!(&blocks[2], Block::ToolUse { name, .. } if name == "Bash"));
        assert!(matches!(blocks[3], Block::Unknown));
    }

    #[test]
    fn odd_shapes_degrade_to_empty() {
        for json in [
            r#"{"type":"user","message":{"content":{"weird":true}}}"#,
            r#"{"type":"user","message":{"content":42}}"#,
            r#"{"type":"user","message":{"content":null},"toolUseResult":"Error: boom"}"#,
            r#"{"type":"user","toolUseResult":[1,2,{"a":null}]}"#,
            r#"{"type":"user","toolUseResult":{"structuredPatch":"nope","type":7}}"#,
        ] {
            let record = parse(json);
            assert!(record.tool_use_result.structured_patch.is_none(), "{json}");
            assert!(record.tool_use_result.kind.is_none(), "{json}");
        }
    }

    #[test]
    fn reads_structured_patches() {
        let record = parse(
            r#"{"type":"user","toolUseResult":{"filePath":"/w/a.rs","originalFile":"huge",
                "structuredPatch":[{"oldStart":3,"oldLines":1,"newStart":3,"newLines":2,
                "lines":["-a","+b","+c"]}],"type":"update","content":"x"}}"#,
        );
        let patch = record.tool_use_result.structured_patch.expect("patch");
        assert_eq!(patch[0].lines, ["-a", "+b", "+c"]);
        assert_eq!(record.tool_use_result.kind.as_deref(), Some("update"));
    }

    #[test]
    fn origins_are_read_leniently() {
        let record = parse(
            r#"{"type":"user","origin":{"kind":"peer","name":"code-review","body":"done",
                "fromMode":"x","hopChain":[1]}}"#,
        );
        let origin = record.origin.0.expect("origin");
        assert_eq!(origin.kind.as_deref(), Some("peer"));
        assert_eq!(origin.name.as_deref(), Some("code-review"));
        assert_eq!(origin.body.as_deref(), Some("done"));
        assert!(origin.from_mode.is_some());
        for json in [
            r#"{"type":"user","origin":null}"#,
            r#"{"type":"user","origin":"human"}"#,
            r#"{"type":"user","origin":{"kind":7}}"#,
            r#"{"type":"user"}"#,
        ] {
            assert!(parse(json).origin.0.is_none(), "{json}");
        }
    }

    #[test]
    fn unknown_fields_and_types_are_ignored() {
        let record = parse(r#"{"type":"ai-title","title":"x","extra":{"deep":[1,2]}}"#);
        assert_eq!(record.kind.as_deref(), Some("ai-title"));
        assert!(record.message.is_none());
    }
}
