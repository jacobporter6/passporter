//! File changes from `Edit`/`MultiEdit`/`Write` tool calls.

use std::fmt::Write as _;
use std::path::Path;

use protocol::ThreadContent;
use serde_json::Value;

use crate::record::{Hunk, ToolUseResult};

/// What a running file-editing call will need to render its diff, computed
/// when the call is seen so the (possibly huge) tool input can be dropped
/// right away. Its size is bounded by the patch cap.
#[derive(Debug)]
pub(crate) struct Planned {
    /// The `file_path` argument, as given.
    path: String,
    /// The diff implied by the input, for when the result has no
    /// `structuredPatch`.
    fallback: Option<Change>,
}

impl Planned {
    /// For a file-editing call (`Edit`, `MultiEdit`, `Write`) that names a
    /// file; `None` for anything else.
    pub(crate) fn new(tool: &str, input: &Value, max_patch: usize) -> Option<Self> {
        if !matches!(tool, "Edit" | "MultiEdit" | "Write") {
            return None;
        }
        let path = input.get("file_path").and_then(Value::as_str)?.to_owned();
        Some(Self {
            path,
            fallback: from_input(tool, input, max_patch),
        })
    }

    /// The diff for the call's *successful* result.
    ///
    /// Prefers the hunks Claude Code computed against the real file
    /// (`toolUseResult.structuredPatch`, with true line numbers). Without
    /// them, falls back to the input: a `Write` is all additions, an `Edit`
    /// is its `old_string` → `new_string` with shared leading and trailing
    /// lines trimmed; that patch has no `@@` header, since line numbers are
    /// unknown. Returns `None` when a `Write` left the file unchanged.
    ///
    /// The path is made relative to `cwd` when it lies inside it; otherwise
    /// it stays absolute. Patches over `max_patch` bytes are never built in
    /// full: only the line counts are kept.
    pub(crate) fn finish(
        self,
        result: &ToolUseResult,
        cwd: Option<&str>,
        max_patch: usize,
    ) -> Option<ThreadContent> {
        let change = match result.structured_patch.as_deref() {
            Some(hunks) if !hunks.is_empty() => from_hunks(hunks, max_patch),
            // Claude Code compared the file and found nothing to change.
            Some(_) if result.kind.as_deref() == Some("update") => return None,
            _ => self.fallback?,
        };
        Some(ThreadContent::Diff {
            path: relative(&self.path, cwd),
            added: change.added,
            removed: change.removed,
            patch: change.into_patch(),
        })
    }
}

/// Line counts and a patch body that stops growing past its cap.
#[derive(Debug)]
struct Change {
    added: u32,
    removed: u32,
    patch: String,
    max_patch: usize,
    /// The patch passed its cap and was discarded.
    overflow: bool,
}

impl Change {
    fn new(max_patch: usize) -> Self {
        Self {
            added: 0,
            removed: 0,
            patch: String::new(),
            max_patch,
            overflow: false,
        }
    }

    fn line(&mut self, prefix: char, text: &str) {
        match prefix {
            '+' => self.added = self.added.saturating_add(1),
            '-' => self.removed = self.removed.saturating_add(1),
            _ => {}
        }
        self.push(&[&prefix.to_string(), text, "\n"]);
    }

    fn push(&mut self, parts: &[&str]) {
        if self.overflow {
            return;
        }
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if self.patch.len() + len > self.max_patch {
            self.overflow = true;
            self.patch = String::new();
            return;
        }
        for part in parts {
            self.patch.push_str(part);
        }
    }

    fn into_patch(self) -> Option<String> {
        (!self.overflow && !self.patch.is_empty()).then_some(self.patch)
    }
}

fn from_hunks(hunks: &[Hunk], max_patch: usize) -> Change {
    let mut change = Change::new(max_patch);
    for hunk in hunks {
        let mut header = String::new();
        let _ = writeln!(
            header,
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        );
        change.push(&[&header]);
        for line in &hunk.lines {
            let mut chars = line.chars();
            let prefix = chars.next().unwrap_or(' ');
            change.line(prefix, chars.as_str());
        }
    }
    change
}

fn from_input(tool: &str, input: &Value, max_patch: usize) -> Option<Change> {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let mut change = Change::new(max_patch);
    match tool {
        "Write" => {
            for line in field("content")?.lines() {
                change.line('+', line);
            }
        }
        "Edit" => edit(&mut change, field("old_string")?, field("new_string")?),
        "MultiEdit" => {
            for e in input.get("edits").and_then(Value::as_array)? {
                let old = e.get("old_string").and_then(Value::as_str).unwrap_or("");
                let new = e.get("new_string").and_then(Value::as_str).unwrap_or("");
                edit(&mut change, old, new);
            }
        }
        _ => return None,
    }
    Some(change)
}

fn edit(change: &mut Change, old: &str, new: &str) {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    for line in &old[prefix..old.len() - suffix] {
        change.line('-', line);
    }
    for line in &new[prefix..new.len() - suffix] {
        change.line('+', line);
    }
}

/// The file changes in a Codex `apply_patch` envelope:
///
/// ```text
/// *** Begin Patch
/// *** Add File: a.txt
/// +new line
/// *** Update File: src/lib.rs
/// *** Move to: src/main.rs
/// @@ fn main
/// -old
/// +new
/// *** Delete File: gone.txt
/// *** End Patch
/// ```
///
/// One [`ThreadContent::Diff`] per file, in order, its path made relative to
/// `cwd` when inside it (a moved file is shown under its new path). A
/// deleted file's lines are not in the patch, so it counts nothing removed.
/// Text that is not an envelope yields nothing.
pub(crate) fn apply_patch(text: &str, cwd: Option<&str>, max_patch: usize) -> Vec<ThreadContent> {
    let mut files: Vec<(String, Change)> = Vec::new();
    let mut lines = text
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line));
    if !lines.any(|line| line.trim() == BEGIN_PATCH) {
        return Vec::new();
    }
    for line in lines {
        if line.trim() == "*** End Patch" {
            break;
        }
        if let Some(path) = file_header(line) {
            files.push((path.to_owned(), Change::new(max_patch)));
            continue;
        }
        let Some((path, change)) = files.last_mut() else {
            continue;
        };
        if let Some(to) = line.strip_prefix("*** Move to: ") {
            to.trim().clone_into(path);
        } else if line.starts_with("@@") {
            change.push(&[line, "\n"]);
        } else if let Some(prefix) = line.chars().next().filter(|c| matches!(c, '+' | '-' | ' ')) {
            change.line(prefix, &line[1..]);
        }
    }
    files
        .into_iter()
        .filter(|(path, _)| !path.is_empty())
        .map(|(path, change)| ThreadContent::Diff {
            path: relative(&path, cwd),
            added: change.added,
            removed: change.removed,
            patch: change.into_patch(),
        })
        .collect()
}

/// The line that opens an `apply_patch` envelope.
pub(crate) const BEGIN_PATCH: &str = "*** Begin Patch";

/// The path an envelope line names as a file to add, update or delete.
fn file_header(line: &str) -> Option<&str> {
    ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
        .map(str::trim)
}

/// The files an `apply_patch` envelope adds, updates or deletes, in order.
pub(crate) fn patched_paths(patch: &str) -> Vec<&str> {
    patch.lines().filter_map(file_header).collect()
}

fn relative(path: &str, cwd: Option<&str>) -> String {
    cwd.and_then(|cwd| Path::new(path).strip_prefix(cwd).ok())
        .filter(|rel| !rel.as_os_str().is_empty())
        .map_or_else(|| path.to_owned(), |rel| rel.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn file_change(
        tool: &str,
        input: &Value,
        result: &ToolUseResult,
        cwd: Option<&str>,
        max_patch: usize,
    ) -> Option<ThreadContent> {
        Planned::new(tool, input, max_patch)?.finish(result, cwd, max_patch)
    }

    fn diff(
        tool: &str,
        input: &Value,
        result: &ToolUseResult,
    ) -> (String, u32, u32, Option<String>) {
        match file_change(tool, input, result, Some("/w"), 1000) {
            Some(ThreadContent::Diff {
                path,
                added,
                removed,
                patch,
            }) => (path, added, removed, patch),
            other => panic!("expected a diff, got {other:?}"),
        }
    }

    #[test]
    fn prefers_structured_patches() {
        let result = ToolUseResult {
            structured_patch: Some(vec![Hunk {
                old_start: 10,
                old_lines: 2,
                new_start: 10,
                new_lines: 3,
                lines: vec![" keep".into(), "-old".into(), "+new".into(), "+more".into()],
            }]),
            ..ToolUseResult::default()
        };
        let input = json!({"file_path": "/w/src/a.rs", "old_string": "x", "new_string": "y"});
        assert_eq!(
            diff("Edit", &input, &result),
            (
                "src/a.rs".into(),
                2,
                1,
                Some("@@ -10,2 +10,3 @@\n keep\n-old\n+new\n+more\n".into())
            )
        );
    }

    #[test]
    fn edit_fallback_trims_shared_lines() {
        let input = json!({
            "file_path": "/elsewhere/b.rs",
            "old_string": "fn a() {\n    1\n}",
            "new_string": "fn a() {\n    2\n    3\n}",
        });
        assert_eq!(
            diff("Edit", &input, &ToolUseResult::default()),
            (
                "/elsewhere/b.rs".into(),
                2,
                1,
                Some("-    1\n+    2\n+    3\n".into())
            )
        );
    }

    #[test]
    fn write_and_multiedit_fallbacks() {
        let write = json!({"file_path": "/w/new.md", "content": "# T\n\nbody\n"});
        let (_, added, removed, _) = diff("Write", &write, &ToolUseResult::default());
        assert_eq!((added, removed), (3, 0));

        let multi = json!({"file_path": "/w/m.rs", "edits": [
            {"old_string": "a", "new_string": "b"},
            {"old_string": "c\nd", "new_string": "c"},
        ]});
        assert_eq!(
            diff("MultiEdit", &multi, &ToolUseResult::default()),
            ("m.rs".into(), 1, 2, Some("-a\n+b\n-d\n".into()))
        );
    }

    #[test]
    fn large_patches_keep_counts_only_and_stay_small() {
        let content = "line\n".repeat(500);
        let input = json!({"file_path": "/w/big.txt", "content": content});
        let planned = Planned::new("Write", &input, 1000).expect("planned");
        let fallback = planned.fallback.as_ref().expect("fallback");
        assert!(fallback.overflow);
        assert_eq!(fallback.patch.capacity(), 0, "the big patch was never kept");
        let (_, added, _, patch) = diff("Write", &input, &ToolUseResult::default());
        assert_eq!(added, 500);
        assert_eq!(patch, None);
    }

    #[test]
    fn codex_apply_patch_envelopes() {
        let patch = "*** Begin Patch\n*** Add File: /w/notes.txt\n+one\n+two\n\
                     *** Update File: src/lib.rs\n*** Move to: src/main.rs\n@@ fn main\n \
                     keep\n-old\n+new\n*** End of File\n*** Delete File: gone.txt\n*** End Patch";
        let diffs = apply_patch(patch, Some("/w"), 1024);
        let shown: Vec<(String, u32, u32, Option<String>)> = diffs
            .into_iter()
            .map(|d| match d {
                ThreadContent::Diff {
                    path,
                    added,
                    removed,
                    patch,
                } => (path, added, removed, patch),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "notes.txt".to_owned(),
                    2,
                    0,
                    Some("+one\n+two\n".to_owned())
                ),
                (
                    "src/main.rs".to_owned(),
                    1,
                    1,
                    Some("@@ fn main\n keep\n-old\n+new\n".to_owned())
                ),
                ("gone.txt".to_owned(), 0, 0, None),
            ]
        );
        assert_eq!(apply_patch("just text", None, 1024), []);
        assert_eq!(apply_patch("", None, 1024), []);
    }

    #[test]
    fn no_file_no_diff() {
        assert!(file_change("Edit", &json!({}), &ToolUseResult::default(), None, 10).is_none());
        let unchanged = ToolUseResult {
            structured_patch: Some(Vec::new()),
            kind: Some("update".into()),
        };
        let write = json!({"file_path": "/w/a", "content": "same\n"});
        assert!(file_change("Write", &write, &unchanged, None, 10).is_none());
        assert!(Planned::new("Bash", &json!({"file_path": "/w/a"}), 10).is_none());
    }
}
