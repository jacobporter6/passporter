//! Validated Claude session ids.

use std::fmt;
use std::str::FromStr;

/// The longest id accepted. Real ids are UUIDs (36 chars) or subagent ids
/// such as `agent-a68d166416019d2c0`; the slack is for future formats.
const MAX_LEN: usize = 128;

/// A Claude Code session id, safe to use as a file name.
///
/// Ids come from herdr's `agent_session`, i.e. from outside this process, and
/// are joined onto `~/.claude/projects/<slug>/`. Only ASCII letters, digits,
/// `-` and `_` are accepted, so an id can never contain a path separator,
/// `..`, a NUL or anything a filesystem treats specially.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    /// Validates `id`.
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidSessionId> {
        let id = id.into();
        let reason = if id.is_empty() {
            Some("is empty")
        } else if id.len() > MAX_LEN {
            Some("is too long")
        } else if !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            Some("may only contain ASCII letters, digits, `-` and `_`")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(InvalidSessionId { id, reason }),
            None => Ok(Self(id)),
        }
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SessionId {
    type Err = InvalidSessionId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A rejected session id. Its `Display` shows at most the first 64
/// characters of the (untrusted) id, debug-escaped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid session id {}: {reason}", Shown(id))]
pub struct InvalidSessionId {
    id: String,
    reason: &'static str,
}

/// Longest prefix of a rejected id shown in messages.
const SHOWN_CHARS: usize = 64;

/// Debug-escapes and truncates an untrusted id for display.
struct Shown<'a>(&'a str);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut chars = self.0.chars();
        let head: String = chars.by_ref().take(SHOWN_CHARS).collect();
        let more = if chars.next().is_some() { "…" } else { "" };
        write!(f, "{head:?}{more}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_shapes() {
        for id in [
            "bcb59b47-a307-4f07-9239-9e7513f93895",
            "agent-a68d166416019d2c0",
            "a_b",
        ] {
            assert_eq!(SessionId::new(id).expect(id).as_str(), id);
        }
    }

    #[test]
    fn rejects_anything_that_could_escape() {
        for id in [
            "",
            "..",
            "../etc/passwd",
            "a/b",
            "a\\b",
            "a.jsonl",
            "a\0b",
            " a",
            "ü",
            &"a".repeat(MAX_LEN + 1),
        ] {
            assert!(SessionId::new(id).is_err(), "{id:?}");
        }
        let err = "x/y".parse::<SessionId>().expect_err("rejected");
        assert!(err.to_string().contains("x/y"), "{err}");
        let long = SessionId::new("\u{1b}[31m".repeat(100)).expect_err("rejected");
        let shown = long.to_string();
        assert!(shown.len() < 400, "{shown}");
        assert!(!shown.contains('\u{1b}'), "escaped: {shown}");
        assert!(shown.contains('…'), "{shown}");
    }
}
