//! Counters describing what a reader saw but did not render.

use std::collections::BTreeMap;
use std::fmt;

/// Distinct names kept per map; the rest are folded into [`OTHER`]. Keeps a
/// hostile or corrupt transcript from growing the maps without bound.
const MAX_NAMES: usize = 128;
/// Longest name kept verbatim.
const MAX_NAME_LEN: usize = 64;
/// The bucket for names past [`MAX_NAMES`].
pub const OTHER: &str = "<other>";
/// The key for lines that are not JSON objects at all.
pub const INVALID_JSON: &str = "<invalid json>";
/// The key for records without a string `type`.
pub const NO_TYPE: &str = "<no type>";

/// Record types Claude Code 2.1.x writes that carry nothing for the thread
/// view: session bookkeeping (titles, modes, ownership, worktree and cost
/// state, fork/continuation links), API request logs and file-history
/// snapshots. Classified from the real corpus (2.1.311–2.1.319). Matched exactly, or by prefix when the entry ends in `-`.
const KNOWN_SKIPPED: &[&str] = &[
    "agent-name",
    "agent-setting",
    "ai-title",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "artifact-autoreact-ledger",
    "atis-latch",
    "attachment",
    "continued-in",
    "cost-state",
    "custom-title",
    "file-history-",
    "fork-context-ref",
    "last-prompt",
    "launched",
    "mode",
    "permission-mode",
    "pr-link",
    "progress",
    "queue-operation",
    "relocated",
    "result",
    "session-owner",
    "started",
    "summary",
    "system",
    "tag",
    "tools-used",
    "worktree-state",
];

/// What a reader skipped, by reason. Nothing here is an error: the corpus
/// acceptance test prints it to spot record types worth supporting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// Non-empty lines read.
    pub lines: u64,
    /// Known record types skipped because they are not rendered, by type,
    /// plus rendered types skipped for a known reason (`sidechain`,
    /// `user:meta`, `user:compact-summary`, `user:notice`).
    pub skipped: BTreeMap<String, u64>,
    /// What user-role records and queued prompts were classified as
    /// (`prompt`, `agent-message`, `notification`, …) and on what basis
    /// (`by origin`, `by markers`, or `unknown origin` when Claude Code
    /// wrote an `origin.kind` this reader predates).
    pub said: BTreeMap<String, u64>,
    /// Record types this reader does not know, by type.
    pub unknown_types: BTreeMap<String, u64>,
    /// Content blocks of an unknown type inside rendered records.
    pub unknown_blocks: u64,
    /// Lines that are not valid JSON, or rendered types (`user`,
    /// `assistant`) whose shape did not match, by type.
    pub malformed: BTreeMap<String, u64>,
    /// Tool results whose tool call was not seen (e.g. it preceded a reset).
    pub orphan_results: u64,
    /// Lines dropped for exceeding the tailer's size cap.
    pub oversized_lines: u64,
    /// Times the file was truncated or replaced and re-read from the start.
    pub resets: u64,
}

impl Diagnostics {
    /// Adds `other`'s counts to these.
    pub fn merge(&mut self, other: &Self) {
        self.lines += other.lines;
        for (name, n) in &other.skipped {
            bump_by(&mut self.skipped, name, *n);
        }
        for (name, n) in &other.said {
            bump_by(&mut self.said, name, *n);
        }
        for (name, n) in &other.unknown_types {
            bump_by(&mut self.unknown_types, name, *n);
        }
        for (name, n) in &other.malformed {
            bump_by(&mut self.malformed, name, *n);
        }
        self.unknown_blocks += other.unknown_blocks;
        self.orphan_results += other.orphan_results;
        self.oversized_lines += other.oversized_lines;
        self.resets += other.resets;
    }

    /// True if any line was invalid or of an unexpected shape.
    #[must_use]
    pub fn has_malformed(&self) -> bool {
        !self.malformed.is_empty()
    }

    /// Records a non-rendered record type, sorting it into known or unknown.
    pub(crate) fn skip_type(&mut self, kind: &str) {
        let known = KNOWN_SKIPPED.iter().any(|k| {
            k.strip_suffix('-')
                .map_or(*k == kind, |_| kind.starts_with(k))
        });
        let map = if known {
            &mut self.skipped
        } else {
            &mut self.unknown_types
        };
        bump_by(map, kind, 1);
    }

    /// Records a rendered record type skipped for a known reason (e.g.
    /// `user:meta`).
    pub(crate) fn skip(&mut self, reason: &str) {
        bump_by(&mut self.skipped, reason, 1);
    }

    /// Records how a user-role record was classified, e.g.
    /// `prompt (by origin)`.
    pub(crate) fn said(&mut self, key: &str) {
        bump_by(&mut self.said, key, 1);
    }

    pub(crate) fn malformed(&mut self, kind: &str) {
        bump_by(&mut self.malformed, kind, 1);
    }
}

fn bump_by(map: &mut BTreeMap<String, u64>, name: &str, n: u64) {
    let name = truncate(name);
    if let Some(count) = map.get_mut(name) {
        *count += n;
    } else if map.len() < MAX_NAMES {
        map.insert(name.to_owned(), n);
    } else {
        *map.entry(OTHER.to_owned()).or_default() += n;
    }
}

fn truncate(name: &str) -> &str {
    let mut end = name.len().min(MAX_NAME_LEN);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.get(..end).unwrap_or(name)
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "lines read:        {}", self.lines)?;
        write_map(f, "user records", &self.said)?;
        write_map(f, "skipped (known)", &self.skipped)?;
        write_map(f, "unknown types", &self.unknown_types)?;
        write_map(f, "malformed", &self.malformed)?;
        writeln!(f, "unknown blocks:    {}", self.unknown_blocks)?;
        writeln!(f, "orphan results:    {}", self.orphan_results)?;
        writeln!(f, "oversized lines:   {}", self.oversized_lines)?;
        writeln!(f, "resets:            {}", self.resets)
    }
}

fn write_map(f: &mut fmt::Formatter<'_>, title: &str, map: &BTreeMap<String, u64>) -> fmt::Result {
    if map.is_empty() {
        return writeln!(f, "{title}: none");
    }
    writeln!(f, "{title}:")?;
    for (name, n) in map {
        writeln!(f, "  {name:<32} {n}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_known_from_unknown() {
        let mut d = Diagnostics::default();
        for kind in [
            "system",
            "file-history-snapshot",
            "file-history-delta",
            "api-request-blob",
            "hologram",
            "file-history",
        ] {
            d.skip_type(kind);
        }
        assert_eq!(d.skipped.len(), 4);
        assert_eq!(
            d.unknown_types.keys().collect::<Vec<_>>(),
            ["file-history", "hologram"]
        );
    }

    #[test]
    fn maps_are_bounded() {
        let mut d = Diagnostics::default();
        for i in 0..1000 {
            d.skip_type(&format!("weird-{i}"));
        }
        d.skip_type(&"é".repeat(200));
        assert_eq!(d.unknown_types.len(), MAX_NAMES + 1);
        assert_eq!(d.unknown_types[OTHER], 1000 - MAX_NAMES as u64 + 1);
        assert!(d.unknown_types.keys().all(|k| k.len() <= MAX_NAME_LEN));
    }

    #[test]
    fn merges_and_displays() {
        let mut a = Diagnostics {
            lines: 2,
            ..Diagnostics::default()
        };
        a.skip_type("system");
        a.malformed(INVALID_JSON);
        a.said("prompt (by origin)");
        let mut b = a.clone();
        b.skip_type("hologram");
        a.merge(&b);
        assert_eq!(a.lines, 4);
        assert_eq!(a.skipped["system"], 2);
        assert_eq!(a.unknown_types["hologram"], 1);
        assert_eq!(a.said["prompt (by origin)"], 2);
        assert!(a.has_malformed());
        let shown = a.to_string();
        assert!(shown.contains("hologram"), "{shown}");
        assert!(shown.contains(INVALID_JSON), "{shown}");
    }
}
