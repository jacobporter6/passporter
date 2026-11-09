//! The normalized transcript (`GET /v1/agents/:id/thread?since=`).
//! Outbound: tolerant.
//!
//! All strings here are untrusted agent output (design §8): render them as
//! sanitized text, never as HTML.
//!
//! # Syncing a thread
//!
//! A cursor identifies one item for the lifetime of an [`ThreadEpoch`].
//! Items are not strictly append-only: a [`ThreadContent::Tool`] item is
//! first sent `running` and sent again, with the same cursor, once it
//! finishes, and a shell command's [`ThreadContent::Aside`] once its output
//! arrives. So a client:
//!
//! 1. **upserts by cursor, keeping the higher `rev`**: an item whose cursor
//!    it already holds replaces the old one only if its [`ThreadItem::rev`]
//!    is higher, so a page fetched before an update and delivered after the
//!    update's event cannot roll it back;
//! 2. **drops its cache when the epoch changes**, then refetches with
//!    `since=0`;
//! 3. **asks for `since = resumeCursor`** from the last [`ThreadPage`] or
//!    [`AgentEvent::ThreadItems`](crate::AgentEvent::ThreadItems), not the
//!    last cursor it saw: the resume cursor never moves past an item that
//!    may still change;
//! 4. **keeps fetching while `hasMore`**: pages are size-limited.

use core::fmt;

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::headline::TurnHeadline;
use crate::loose::{Loose, required, set_once};
use crate::time::Timestamp;

/// A position in an agent's thread, identifying one item within a
/// [`ThreadEpoch`]. Items are returned in increasing cursor order. See the
/// module docs for how `since` works.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ThreadCursor(pub u64);

/// A turn index within an agent session. Turn `n ≥ 1` starts with the
/// user's `n`-th typed message and runs until the next one; turn `0` holds
/// whatever came before the first typed message (a resumed session's
/// preamble, startup output). It ties [`ThreadItem`]s to their TL;DR
/// (`AgentEvent::TldrReady.turn`, `GET /v1/agents/:id/tldr?turn=`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TurnIndex(pub u32);

/// Identifies one consistent numbering of an agent's thread. Opaque: compare
/// for equality only. It changes whenever cursors stop being comparable with
/// earlier ones, and a client must then discard what it cached. The bridge
/// changes it when:
///
/// - the transcript is truncated or replaced;
/// - a new reader or a different transcript serves the same agent (e.g. the
///   session changed after `/clear`), even if the new numbering happens to
///   reach the same cursors;
/// - the bridge restarts, or its transcript parser changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadEpoch(pub String);

/// The response of `GET /v1/agents/:id/thread?since=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPage {
    /// The numbering these cursors belong to.
    pub epoch: ThreadEpoch,
    /// Items with a cursor above `since` (new or changed), in cursor order.
    pub items: Vec<ThreadItem>,
    /// What to pass as `since` next.
    pub resume_cursor: ThreadCursor,
    /// Set when the bridge no longer holds older history: items before this
    /// cursor were dropped to bound memory and cannot be fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_before: Option<ThreadCursor>,
    /// More items after these were left out to bound the response; fetch
    /// again with `since = resumeCursor`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_more: bool,
    /// Headlines of the session's finished turns, in turn order: on a page
    /// fetched from the start (`since=0`) the bridge's recent set (replace
    /// what was held), on a later page the last few (merge them; new ones
    /// also arrive as `headline_ready` events). Entries this app cannot
    /// parse are skipped rather than failing the page.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "crate::tolerant::skip_invalid"
    )]
    pub headlines: Vec<TurnHeadline>,
    /// The transcript was matched by the agent's folder, not named by its
    /// session (herdr reported none): it may be another session from the
    /// same folder. Sent only when true; absent (an older bridge) is false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inferred: bool,
}

/// One entry in an agent's thread.
///
/// `Deserialize` is hand-written (see [`crate::loose`]) and accepts exactly
/// what the derived `Serialize` writes, plus unknown fields and item types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadItem {
    /// Position in the thread.
    pub cursor: ThreadCursor,
    /// The turn the entry belongs to.
    pub turn: TurnIndex,
    /// Revision of this item: per thread (and epoch), strictly increasing
    /// with every change to any item. When two copies of the same cursor
    /// arrive, keep the one with the higher `rev`.
    #[serde(default)]
    pub rev: u64,
    /// When the entry was recorded, if the source says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Timestamp>,
    /// What the entry is.
    #[serde(flatten)]
    pub content: ThreadContent,
}

/// The kind-specific part of a [`ThreadItem`], tagged by `"type"`. It is
/// only ever on the wire flattened into its item, so it has no
/// `Deserialize` of its own: [`ThreadItem`]'s decodes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ThreadContent {
    /// A message from the user.
    User {
        /// Message text.
        text: String,
    },
    /// A message from the agent.
    Assistant {
        /// Message text (Markdown).
        text: String,
    },
    /// A tool call and, once finished, its result.
    Tool {
        /// Tool name, e.g. `Bash`.
        name: String,
        /// A short rendering of the input, e.g. the shell command.
        input: String,
        /// The tool's output, possibly truncated by the bridge.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// Whether the call is still running, succeeded or failed.
        status: ToolStatus,
    },
    /// A file change made during the turn.
    Diff {
        /// Path relative to the agent's working directory, or absolute
        /// (leading `/`) for a file outside it.
        path: String,
        /// Lines added.
        added: u32,
        /// Lines removed.
        removed: u32,
        /// Unified diff body, when small enough to ship.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        patch: Option<String>,
    },
    /// Something Claude Code recorded as user input that the user did not
    /// type as a message: a subagent's report, another session's message, a
    /// background task's notification, or the user's own `!` shell command.
    /// Never show it as the user's message.
    Aside {
        /// The one line shown for it, e.g. `Report from an agent
        /// (code-review)` or ``You ran `git status` ``. The bridge builds it
        /// from structured transcript fields only, never from message text.
        label: String,
        /// What it opens to: the message, the task's result or the command's
        /// output, as plain text. May be empty.
        text: String,
    },
    /// An entry type this app predates; skip it.
    #[serde(other)]
    Unknown,
}

/// The state of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// No result yet.
    Running,
    /// Completed without error.
    Ok,
    /// Completed with an error.
    Error,
    /// A status this app predates.
    #[serde(other)]
    Unknown,
}

impl<'de> Deserialize<'de> for ThreadItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ThreadItemVisitor)
    }
}

struct ThreadItemVisitor;

/// The fields of every [`ThreadContent`] variant, until `type` is known.
#[derive(Default)]
struct ContentFields {
    kind: Option<String>,
    text: Option<Loose>,
    name: Option<Loose>,
    input: Option<Loose>,
    output: Option<Loose>,
    status: Option<Loose>,
    path: Option<Loose>,
    added: Option<Loose>,
    removed: Option<Loose>,
    patch: Option<Loose>,
    label: Option<Loose>,
}

impl ContentFields {
    /// The slot for a content field's key, if it is one.
    fn slot(&mut self, key: &str) -> Option<(&mut Option<Loose>, &'static str)> {
        Some(match key {
            "text" => (&mut self.text, "text"),
            "name" => (&mut self.name, "name"),
            "input" => (&mut self.input, "input"),
            "output" => (&mut self.output, "output"),
            "status" => (&mut self.status, "status"),
            "path" => (&mut self.path, "path"),
            "added" => (&mut self.added, "added"),
            "removed" => (&mut self.removed, "removed"),
            "patch" => (&mut self.patch, "patch"),
            "label" => (&mut self.label, "label"),
            _ => return None,
        })
    }

    fn into_content<E: serde::de::Error>(self) -> Result<ThreadContent, E> {
        Ok(match required::<_, E>(self.kind, "type")?.as_str() {
            "user" => ThreadContent::User {
                text: required(self.text, "text")?.string()?,
            },
            "assistant" => ThreadContent::Assistant {
                text: required(self.text, "text")?.string()?,
            },
            "tool" => ThreadContent::Tool {
                name: required(self.name, "name")?.string()?,
                input: required(self.input, "input")?.string()?,
                output: self.output.map_or(Ok(None), Loose::opt_string)?,
                status: required(self.status, "status")?.parse_str()?,
            },
            "diff" => ThreadContent::Diff {
                path: required(self.path, "path")?.string()?,
                added: required(self.added, "added")?.u32()?,
                removed: required(self.removed, "removed")?.u32()?,
                patch: self.patch.map_or(Ok(None), Loose::opt_string)?,
            },
            "aside" => ThreadContent::Aside {
                label: required(self.label, "label")?.string()?,
                text: required(self.text, "text")?.string()?,
            },
            _ => ThreadContent::Unknown,
        })
    }
}

impl<'de> Visitor<'de> for ThreadItemVisitor {
    type Value = ThreadItem;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a thread item")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ThreadItem, A::Error> {
        let (mut cursor, mut turn, mut rev, mut at) = (None, None, None, None);
        let mut content = ContentFields::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "cursor" => set_once(&mut cursor, "cursor", map.next_value()?)?,
                "turn" => set_once(&mut turn, "turn", map.next_value()?)?,
                "rev" => set_once(&mut rev, "rev", map.next_value()?)?,
                "at" => set_once(&mut at, "at", map.next_value::<Option<Timestamp>>()?)?,
                "type" => set_once(&mut content.kind, "type", map.next_value()?)?,
                other => match content.slot(other) {
                    Some((slot, field)) => set_once(slot, field, map.next_value()?)?,
                    None => {
                        map.next_value::<IgnoredAny>()?;
                    }
                },
            }
        }
        Ok(ThreadItem {
            cursor: required(cursor, "cursor")?,
            turn: required(turn, "turn")?,
            rev: rev.unwrap_or_default(),
            at: at.flatten(),
            content: content.into_content()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_headline_this_app_cannot_parse_does_not_fail_the_page() {
        let page: ThreadPage = serde_json::from_str(
            r#"{"epoch":"e","items":[],"resumeCursor":0,"headlines":[
                {"turn":1,"text":"Fixed it."},{"turn":-1,"text":"bad"},{"text":7}]}"#,
        )
        .unwrap();
        assert_eq!(
            page.headlines,
            [TurnHeadline {
                turn: TurnIndex(1),
                text: "Fixed it.".into(),
                provisional: false,
            }]
        );
        let bare: ThreadPage =
            serde_json::from_str(r#"{"epoch":"e","items":[],"resumeCursor":0}"#).unwrap();
        assert_eq!(bare.headlines, Vec::new());
    }

    #[test]
    fn unknown_item_types_are_tolerated() {
        let item: ThreadItem =
            serde_json::from_str(r#"{"cursor":3,"turn":0,"type":"hologram","depth":9}"#).unwrap();
        assert_eq!(item.content, ThreadContent::Unknown);
        assert_eq!(item.at, None);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let item: ThreadItem = serde_json::from_str(
            r#"{"cursor":1,"turn":2,"rev":4,"type":"user","text":"hi","mood":"ok"}"#,
        )
        .unwrap();
        assert_eq!(item.turn, TurnIndex(2));
        assert_eq!(item.content, ThreadContent::User { text: "hi".into() });
    }

    #[test]
    fn unknown_item_types_take_fields_of_any_shape() {
        let item: ThreadItem = serde_json::from_str(
            r#"{"cursor":3,"turn":0,"text":{"rich":true},"added":-2,"name":[1],"type":"canvas"}"#,
        )
        .unwrap();
        assert_eq!(item.content, ThreadContent::Unknown);
    }

    #[test]
    fn fields_may_come_in_any_order() {
        let item: ThreadItem = serde_json::from_str(
            r#"{"status":"wat","input":"ls","type":"tool","turn":1,"name":"Bash","cursor":9}"#,
        )
        .unwrap();
        assert_eq!(item.cursor, ThreadCursor(9));
        assert_eq!(item.rev, 0);
        assert_eq!(
            item.content,
            ThreadContent::Tool {
                name: "Bash".into(),
                input: "ls".into(),
                output: None,
                status: ToolStatus::Unknown,
            }
        );
    }

    #[test]
    fn known_item_types_are_checked() {
        let parse = |json: &str| serde_json::from_str::<ThreadItem>(json);
        // Missing tag, missing field, wrong field type, overflow, duplicate.
        assert!(parse(r#"{"cursor":1,"turn":0,"text":"hi"}"#).is_err());
        assert!(parse(r#"{"cursor":1,"turn":0,"type":"user"}"#).is_err());
        assert!(parse(r#"{"cursor":1,"turn":0,"type":"user","text":5}"#).is_err());
        assert!(
            parse(
                r#"{"cursor":1,"turn":0,"type":"diff","path":"a","added":4294967296,"removed":0}"#
            )
            .is_err()
        );
        assert!(parse(r#"{"cursor":1,"turn":0,"type":"user","text":"a","text":"b"}"#).is_err());
        assert!(parse(r#"{"turn":0,"type":"user","text":"hi"}"#).is_err());
        assert!(parse(r#"[1,0,"user"]"#).is_err());
    }

    #[test]
    fn asides_need_their_fields() {
        let parse = |json: &str| serde_json::from_str::<ThreadItem>(json);
        let item =
            parse(r#"{"cursor":1,"turn":1,"type":"aside","label":"You ran `ls`","text":""}"#)
                .unwrap();
        assert_eq!(
            item.content,
            ThreadContent::Aside {
                label: "You ran `ls`".into(),
                text: String::new(),
            }
        );
        assert!(parse(r#"{"cursor":1,"turn":1,"type":"aside","text":"hi"}"#).is_err());
        assert!(parse(r#"{"cursor":1,"turn":1,"type":"aside","label":"x"}"#).is_err());
    }

    #[test]
    fn null_optionals_are_absent() {
        let item: ThreadItem = serde_json::from_str(
            r#"{"cursor":1,"turn":0,"at":null,"type":"diff","path":"a","added":1,"removed":2,"patch":null}"#,
        )
        .unwrap();
        assert_eq!(item.at, None);
        assert!(matches!(
            item.content,
            ThreadContent::Diff { patch: None, .. }
        ));
    }
}
