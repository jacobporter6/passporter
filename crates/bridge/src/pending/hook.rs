//! Claude Code `PermissionRequest` hook payloads, as the `passporter-hook`
//! binary forwards them over the hook socket. The shapes are those of
//! Claude Code 2.1.317, recorded in `docs/hooks.md` (with `Notification`
//! and `Stop`, which the bridge does not use yet and refuses).
//!
//! Payloads are untrusted: they are capped at [`MAX_PAYLOAD_BYTES`],
//! deserialized into typed structs (the event name must be
//! `PermissionRequest`, every field the bridge reads must have its documented type, and
//! the session id and tool name must be well-formed), and turned into plain
//! display text cleaned by the same rules as agent labels.
//! Fields the bridge does not read are ignored rather than refused, an
//! exception to the strict-serde convention (`AGENTS.md`): Claude Code adds
//! payload fields between releases, and refusing them would turn every
//! upgrade into a silent fall back to the screen parse.
//!
//! Errors never quote the payload: a malformed one is reported by serde's
//! error category and position only, so no untrusted text reaches the log.

use std::env;
use std::path::PathBuf;

use protocol::{PendingInput, PendingKind, SessionId, text};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::parse::{MAX_PROMPT_CHARS, PLAN_PROMPT, bounded, run_prompt};

/// Largest payload the hook forwards and the bridge reads (512 KiB). A
/// bigger one (say, a `Write` of a large file) is dropped, and the screen
/// parse stands in for it.
pub const MAX_PAYLOAD_BYTES: usize = 512 * 1024;

/// Overrides the hook socket path (both the bridge and the hook read it).
pub const SOCKET_ENV: &str = "PASSPORTER_HOOK_SOCKET";

/// The hook socket: `$PASSPORTER_HOOK_SOCKET` if set to an absolute path,
/// else `$XDG_RUNTIME_DIR/passporter/hooks.sock`. `None` when neither is
/// usable: there is no fallback to a shared directory such as `/tmp`.
#[must_use]
pub fn default_socket_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os(SOCKET_ENV).map(PathBuf::from) {
        return path.is_absolute().then_some(path);
    }
    let runtime = PathBuf::from(env::var_os("XDG_RUNTIME_DIR")?);
    runtime
        .is_absolute()
        .then(|| runtime.join("passporter").join("hooks.sock"))
}

/// Longest accepted tool name.
const MAX_TOOL_CHARS: usize = 128;
/// Longest detail, in lines and characters.
const MAX_DETAIL_LINES: usize = 100;
const MAX_DETAIL_CHARS: usize = 4000;

/// A validated hook event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookEvent {
    /// The Claude Code session id, which herdr reports as the agent's
    /// session.
    pub(crate) session: SessionId,
    /// What happened.
    pub(crate) kind: HookEventKind,
}

/// What a hook event means for pending input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookEventKind {
    /// Claude is about to show a dialog the bridge can match exactly
    /// against the screen (`PermissionRequest` for `Bash`, `ExitPlanMode`
    /// or `AskUserQuestion`).
    Permission(HookPermission),
    /// A request for another tool (edits, fetches, MCP), ignored because
    /// its dialog cannot be matched exactly: parallel requests to one file,
    /// host or tool would be indistinguishable, and showing the wrong diff
    /// or URL is worse than showing the screen's own.
    Ignored,
}

/// A dialog as the hook describes it: kind, prompt and detail, plus what
/// the screen must show for the description to apply. The options and
/// their keys always come from the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookPermission {
    pub(crate) kind: PendingKind,
    pub(crate) prompt: String,
    pub(crate) detail: Option<String>,
    shows: Shows,
}

/// What a dialog on screen must show, exactly, for a request to describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shows {
    /// The parsed detail, whitespace collapsed, is the whole command, or
    /// the command and its description (as Claude draws them).
    Command(Vec<String>),
    /// The parsed detail, letters and digits only, is the whole plan:
    /// Claude renders the plan's markdown, so its symbols are not on screen.
    Plan(String),
    /// The parsed prompt, whitespace collapsed, is the whole question.
    Question(String),
}

/// How a request matches the dialog on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Match {
    /// The screen shows the whole command, plan or question.
    Exact,
    /// The parser cut the screen's text (it ends in `…`), and what it kept
    /// is the start of the request's. Usable only when no other stored
    /// request also fits.
    Prefix,
}

/// What a parsed dialog shows, normalized once for matching.
#[derive(Debug)]
pub(crate) struct Shown {
    /// The kind, if the screen parsed confidently (it has options).
    kind: Option<PendingKind>,
    prompt: ShownText,
    detail: ShownText,
}

#[derive(Debug)]
struct ShownText {
    collapsed: String,
    letters: String,
    /// The parser cut it.
    cut: bool,
}

impl ShownText {
    fn new(raw: &str) -> Self {
        let kept = raw.strip_suffix('…');
        let text = kept.unwrap_or(raw);
        Self {
            collapsed: collapse(text),
            letters: letters(text),
            cut: kept.is_some(),
        }
    }

    /// `whole` against this text, `normalized` the same way.
    fn compare(&self, shown: &str, whole: &str) -> Option<Match> {
        if shown.is_empty() || whole.is_empty() {
            None
        } else if shown == whole {
            Some(Match::Exact)
        } else if self.cut && whole.starts_with(shown) {
            Some(Match::Prefix)
        } else {
            None
        }
    }
}

impl Shown {
    /// Normalizes `screen`, a parsed dialog.
    pub(crate) fn new(screen: &PendingInput) -> Self {
        Self {
            kind: (!screen.options.as_slice().is_empty()).then_some(screen.kind),
            prompt: ShownText::new(&screen.prompt),
            detail: ShownText::new(screen.detail.as_deref().unwrap_or_default()),
        }
    }
}

impl HookPermission {
    /// Whether, and how, this request is the dialog `shown`: the same kind,
    /// if the screen parsed confidently, and the whole command, plan or
    /// question. Commands and questions are compared character for
    /// character with only whitespace runs collapsed, so a line wrapped at
    /// a space still matches while `rm -rf /build` never matches
    /// `rm -rf ./build` and `git push` never matches `git push --force`. A
    /// word wrapped mid-way does not match. Text the parser cut matches as
    /// a [`Match::Prefix`] at most.
    pub(crate) fn describes(&self, shown: &Shown) -> Option<Match> {
        if shown.kind.is_some_and(|kind| kind != self.kind) {
            return None;
        }
        let detail = &shown.detail;
        match &self.shows {
            Shows::Command(forms) => {
                let matches = forms
                    .iter()
                    .filter_map(|form| detail.compare(&detail.collapsed, form));
                matches.min_by_key(|found| *found != Match::Exact)
            }
            Shows::Plan(plan) => detail.compare(&detail.letters, plan),
            Shows::Question(question) => shown.prompt.compare(&shown.prompt.collapsed, question),
        }
    }
}

/// Whitespace runs as one space, trimmed.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Lower-case letters and digits only.
fn letters(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Why a payload was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum HookError {
    /// Over [`MAX_PAYLOAD_BYTES`].
    #[error("payload over {MAX_PAYLOAD_BYTES} bytes")]
    TooLarge,
    /// Not a known hook payload. Only serde's category and position are
    /// kept, never the offending text.
    #[error("malformed payload ({category} error at line {line}, column {column})")]
    Malformed {
        category: &'static str,
        line: usize,
        column: usize,
    },
    /// Well-formed JSON with an invalid field.
    #[error("invalid {0}")]
    Invalid(&'static str),
}

impl From<serde_json::Error> for HookError {
    fn from(error: serde_json::Error) -> Self {
        let category = match error.classify() {
            serde_json::error::Category::Io => "io",
            serde_json::error::Category::Syntax => "syntax",
            serde_json::error::Category::Data => "data",
            serde_json::error::Category::Eof => "eof",
        };
        Self::Malformed {
            category,
            line: error.line(),
            column: error.column(),
        }
    }
}

/// The payloads the bridge accepts, by `hook_event_name`.
///
/// No `deny_unknown_fields` here or on [`PermissionRequest`], on purpose:
/// Claude Code adds payload fields between releases, and refusing them
/// would silently turn hooks off after an upgrade (`docs/hooks.md`).
#[derive(Debug, Deserialize)]
#[serde(tag = "hook_event_name")]
enum Payload {
    PermissionRequest(PermissionRequest),
}

/// The fields of a `PermissionRequest` the bridge reads; the others are
/// ignored (see [`Payload`]).
#[derive(Debug, Deserialize)]
struct PermissionRequest {
    session_id: String,
    tool_name: String,
    tool_input: Map<String, Value>,
}

/// Decodes one forwarded payload.
pub(crate) fn decode(bytes: &[u8]) -> Result<HookEvent, HookError> {
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(HookError::TooLarge);
    }
    let Payload::PermissionRequest(request) = serde_json::from_slice::<Payload>(bytes)?;
    if !is_tool_name(&request.tool_name) {
        return Err(HookError::Invalid("tool_name"));
    }
    let kind = permission(&request.tool_name, &request.tool_input)
        .map_or(HookEventKind::Ignored, HookEventKind::Permission);
    let session = request.session_id;
    // The same rule as the session part of an agent id, which it must equal.
    let session = SessionId::new(session).map_err(|_| HookError::Invalid("session_id"))?;
    Ok(HookEvent { session, kind })
}

/// 1 to [`MAX_TOOL_CHARS`] ASCII letters, digits, `_`, `-`, `.` or `:`.
fn is_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_CHARS
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// The dialog a `PermissionRequest` announces, for the tools whose dialog
/// can be matched exactly (see [`HookEventKind::Ignored`]).
fn permission(tool: &str, input: &Map<String, Value>) -> Option<HookPermission> {
    let field = |name: &str| input.get(name).and_then(Value::as_str);
    let permission = match tool {
        "Bash" => {
            let command = block(field("command")?);
            let mut forms = vec![collapse(&command)];
            let detail = match field("description").map(block) {
                Some(description) => {
                    let detail = format!("{command}\n{description}");
                    forms.push(collapse(&detail));
                    detail
                }
                None => command.clone(),
            };
            HookPermission {
                kind: PendingKind::Permission,
                // A blank command describes no dialog: the request is
                // dropped. More text lines after the first: a cut command.
                prompt: {
                    let mut lines = command.lines().filter(|line| !line.trim().is_empty());
                    run_prompt(lines.next()?, lines.next().is_some())?
                },
                detail: Some(detail),
                shows: Shows::Command(forms),
            }
        }
        "ExitPlanMode" => {
            let plan = block(field("plan")?);
            HookPermission {
                kind: PendingKind::PlanApproval,
                prompt: PLAN_PROMPT.to_owned(),
                shows: Shows::Plan(letters(&plan)),
                detail: Some(plan),
            }
        }
        "AskUserQuestion" => {
            let question = input
                .get("questions")?
                .as_array()?
                .first()?
                .get("question")?
                .as_str()?;
            HookPermission {
                kind: PendingKind::Question,
                prompt: bounded(&collapse(&block(question)), MAX_PROMPT_CHARS),
                detail: None,
                shows: Shows::Question(collapse(&block(question))),
            }
        }
        _ => return None,
    };
    Some(HookPermission {
        detail: permission.detail.filter(|detail| !detail.is_empty()),
        ..permission
    })
}

/// Multi-line text, cleaned by the same character rules as
/// [`sanitize_label`](crate::registry::sanitize_label) but keeping line breaks and indentation: at most
/// [`MAX_DETAIL_LINES`] lines and [`MAX_DETAIL_CHARS`] characters.
fn block(raw: &str) -> String {
    let lines: Vec<String> = raw
        .lines()
        .map(|line| {
            let mut clean = String::new();
            for c in line.chars().map(|c| if c == '\t' { ' ' } else { c }) {
                if c.is_control() || text::is_hidden_char(c) {
                    continue;
                }
                if text::is_misplaced_modifier(clean.chars().next_back(), c) {
                    continue;
                }
                clean.push(c);
            }
            clean.trim_end().to_owned()
        })
        .collect();
    let mut out = lines
        .iter()
        .take(MAX_DETAIL_LINES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > MAX_DETAIL_LINES {
        out.push_str("\n…");
    }
    bounded(out.trim_matches('\n'), MAX_DETAIL_CHARS)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SESSION: &str = "00000000-0000-4000-8000-000000000001";

    /// A 2.1.317 `PermissionRequest` payload (the common fields as Claude
    /// Code builds them, the tool fields per tool).
    fn permission_request(tool: &str, input: &Value) -> Value {
        json!({
            "session_id": SESSION,
            "transcript_path": "/home/dev/.claude/projects/-x/00000000.jsonl",
            "cwd": "/home/dev/git/passporter",
            "permission_mode": "default",
            "effort": {"level": "high"},
            "hook_event_name": "PermissionRequest",
            "tool_name": tool,
            "tool_input": input,
            "permission_suggestions": [
                {"type": "addRules", "rules": [{"toolName": tool}],
                 "behavior": "allow", "destination": "localSettings"}
            ],
        })
    }

    fn decode_json(value: &Value) -> Result<HookEvent, HookError> {
        decode(value.to_string().as_bytes())
    }

    fn permission_of(value: &Value) -> HookPermission {
        match decode_json(value).expect("decodes").kind {
            HookEventKind::Permission(permission) => permission,
            HookEventKind::Ignored => panic!("not a permission"),
        }
    }

    fn bash(command: &str, description: Option<&str>) -> HookPermission {
        let mut input = json!({"command": command});
        if let Some(description) = description {
            input["description"] = json!(description);
        }
        permission_of(&permission_request("Bash", &input))
    }

    /// A parsed screen dialog of `kind` with options, showing `prompt` and
    /// `detail`.
    fn screen(kind: PendingKind, prompt: &str, detail: Option<&str>) -> PendingInput {
        let option = protocol::PendingOption {
            id: protocol::OptionId::new("yes").expect("id"),
            label: "Yes".to_owned(),
            risk: protocol::Risk::Low,
            accepts_feedback: false,
        };
        PendingInput {
            id: protocol::PendingId::new("screen-1-000000000000").expect("id"),
            kind,
            prompt: prompt.to_owned(),
            detail: detail.map(str::to_owned),
            options: protocol::PendingOptions::new(vec![option]).expect("options"),
            suggestions: Vec::new(),
            source: protocol::PendingSource::Unknown,
        }
    }

    /// Whether `permission` exactly describes `screen`.
    fn shows(permission: &HookPermission, screen: &PendingInput) -> bool {
        permission.describes(&Shown::new(screen)) == Some(Match::Exact)
    }

    fn bash_screen(detail: &str) -> PendingInput {
        screen(
            PendingKind::Permission,
            "Do you want to proceed?",
            Some(detail),
        )
    }

    #[test]
    fn a_bash_request_becomes_a_permission_with_the_command() {
        let permission = bash("cargo nextest run -p bridge", Some("Run the bridge tests"));
        assert_eq!(permission.kind, PendingKind::Permission);
        assert_eq!(permission.prompt, "Run `cargo nextest run -p bridge`?");
        assert_eq!(
            permission.detail.as_deref(),
            Some("cargo nextest run -p bridge\nRun the bridge tests")
        );
        let event = decode_json(&permission_request("Bash", &json!({"command": "ls"})));
        assert_eq!(event.expect("decodes").session.as_str(), SESSION);
    }

    #[test]
    fn a_multi_line_command_is_marked_cut_and_a_blank_one_is_dropped() {
        let multi = bash("\ncargo build\ncargo test", None);
        assert_eq!(multi.prompt, "Run `cargo build …`?");
        let blank = permission_request("Bash", &json!({"command": " \n "}));
        assert!(matches!(
            decode_json(&blank).expect("decodes").kind,
            HookEventKind::Ignored
        ));
    }

    #[test]
    fn a_command_describes_only_the_exact_command_on_screen() {
        let nextest = bash("cargo nextest run -p bridge", Some("Run the bridge tests"));
        // Wrapped at spaces and indented as Claude draws it, with or
        // without the description line.
        assert!(shows(
            &nextest,
            &bash_screen("cargo nextest run\n   -p bridge\nRun the bridge tests")
        ));
        assert!(shows(&nextest, &bash_screen("cargo nextest run -p bridge")));
        // Never part of a command, a longer command, or a lookalike.
        let ls = bash("ls", None);
        assert!(!shows(&ls, &bash_screen("rm -rf ./tools")));
        let push = bash("git push", Some("Push the branch"));
        assert!(!shows(
            &push,
            &bash_screen("git push --force origin main\nPush the branch")
        ));
        let build = bash("rm -rf ./build", None);
        assert!(!shows(&build, &bash_screen("rm -rf /build")));
        assert!(
            !shows(&build, &bash_screen("rm -rf ./bui\n   ld")),
            "a mid-word wrap fails safe"
        );
        assert!(!shows(&build, &bash_screen("")));
        // Another kind of parsed dialog.
        assert!(!shows(
            &build,
            &screen(
                PendingKind::PlanApproval,
                "Would you like to proceed?",
                Some("rm -rf ./build")
            )
        ));
    }

    #[test]
    fn text_the_parser_cut_matches_as_a_prefix_only() {
        let long = format!("echo {}", "x ".repeat(1500));
        let request = bash(&long, None);
        let cut = format!("{}…", &long[..1999]);
        assert_eq!(
            request.describes(&Shown::new(&bash_screen(&cut))),
            Some(Match::Prefix)
        );
        let other = format!("{}…", &format!("rm {}", "x ".repeat(1500))[..1999]);
        assert_eq!(request.describes(&Shown::new(&bash_screen(&other))), None);
        // An uncut screen never matches a longer request.
        assert_eq!(
            bash("git push --force", None).describes(&Shown::new(&bash_screen("git push"))),
            None
        );
    }

    #[test]
    fn plans_and_questions_must_match_whole() {
        let plan = permission_of(&permission_request(
            "ExitPlanMode",
            &json!({"plan": "# Plan\n1. **Do** it"}),
        ));
        assert_eq!(plan.kind, PendingKind::PlanApproval);
        assert_eq!(plan.prompt, "Approve the plan?");
        assert_eq!(plan.detail.as_deref(), Some("# Plan\n1. **Do** it"));
        let shown = |detail| {
            screen(
                PendingKind::PlanApproval,
                "Would you like to proceed?",
                Some(detail),
            )
        };
        assert!(shows(&plan, &shown("Plan\n1. Do it")), "markdown rendered");
        assert!(!shows(&plan, &shown("Plan\n1. Do it\n2. And more")));

        let question = permission_of(&permission_request(
            "AskUserQuestion",
            &json!({"questions": [{"question": "Which storage backend?", "options": []}]}),
        ));
        assert_eq!(question.kind, PendingKind::Question);
        assert_eq!(question.prompt, "Which storage backend?");
        let long = format!("Which of these {}should we use?", "options ".repeat(20));
        let long_question = permission_of(&permission_request(
            "AskUserQuestion",
            &json!({"questions": [{"question": long}]}),
        ));
        assert_eq!(
            long_question.prompt, long,
            "the screen's 200-char cap, not 120"
        );
        assert!(shows(
            &question,
            &screen(PendingKind::Question, "Which storage backend?", None)
        ));
        assert!(!shows(
            &question,
            &screen(
                PendingKind::Question,
                "Which storage backend should we drop?",
                None
            )
        ));
    }

    #[test]
    fn tools_that_cannot_be_matched_exactly_are_ignored() {
        for (tool, input) in [
            (
                "Edit",
                json!({"file_path": "/a.rs", "old_string": "a", "new_string": "b"}),
            ),
            ("Write", json!({"file_path": "/a.rs", "content": "x"})),
            ("WebFetch", json!({"url": "https://docs.rs"})),
            ("mcp__github__create_issue", json!({"title": "t"})),
            ("Bash", json!({"description": "no command"})),
        ] {
            let event = decode_json(&permission_request(tool, &input)).expect(tool);
            assert_eq!(event.kind, HookEventKind::Ignored, "{tool}");
        }
    }

    #[test]
    fn display_text_is_cleaned_and_bounded() {
        let command = format!("echo \u{202e}hi\u{1b}[31m `x` {}", "y".repeat(500));
        let permission = bash(&command, None);
        assert!(
            permission.prompt.chars().count() <= 130,
            "{}",
            permission.prompt
        );
        assert!(permission.prompt.starts_with("Run `echo hi[31m 'x' y"));
        let detail = permission.detail.expect("detail");
        assert!(!detail.contains('\u{202e}') && !detail.contains('\u{1b}'));

        let huge = "line\n".repeat(1000);
        let detail = bash(&huge, None).detail.expect("detail");
        assert!(detail.lines().count() <= MAX_DETAIL_LINES + 1);
    }

    #[test]
    fn oversized_payloads_are_refused_before_parsing() {
        let padding = "x".repeat(MAX_PAYLOAD_BYTES);
        let payload = permission_request("Bash", &json!({"command": padding}));
        assert_eq!(decode_json(&payload), Err(HookError::TooLarge));
    }

    #[test]
    fn malformed_payloads_are_refused_without_quoting_them() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("not json", b"SECRET{".to_vec()),
            ("truncated", br#"{"hook_event_name":"Stop","session_id":"#.to_vec()),
            ("array", b"[1,2]".to_vec()),
            (
                "unknown event",
                json!({"hook_event_name": "PreToolUse", "session_id": SESSION})
                    .to_string()
                    .into_bytes(),
            ),
            (
                "no event name",
                json!({"session_id": SESSION}).to_string().into_bytes(),
            ),
            (
                "stop (not used)",
                json!({"hook_event_name": "Stop", "session_id": SESSION, "stop_hook_active": false})
                    .to_string()
                    .into_bytes(),
            ),
            (
                "notification (not used)",
                json!({"hook_event_name": "Notification", "session_id": SESSION, "message": "m"})
                    .to_string()
                    .into_bytes(),
            ),
            (
                "missing session",
                json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {}})
                    .to_string()
                    .into_bytes(),
            ),
            (
                "wrong type",
                json!({"hook_event_name": "PermissionRequest", "session_id": SESSION, "tool_name": 7, "tool_input": {}, "x": "SECRET"})
                    .to_string()
                    .into_bytes(),
            ),
            (
                "tool input not an object",
                json!({"hook_event_name": "PermissionRequest", "session_id": SESSION,
                       "tool_name": "Bash", "tool_input": "SECRET"})
                .to_string()
                .into_bytes(),
            ),
            ("not utf-8", b"{\"hook_event_name\":\"\xff\"}".to_vec()),
        ];
        for (name, bytes) in cases {
            let error = decode(&bytes).expect_err(name);
            assert!(
                matches!(error, HookError::Malformed { .. }),
                "{name}: {error:?}"
            );
            assert!(!error.to_string().contains("SECRET"), "{name}: {error}");
        }
    }

    #[test]
    fn ids_are_validated() {
        let mut bad_session = permission_request("Bash", &json!({"command": "ls"}));
        for bad in ["../etc/passwd", "", "a~b", ".."] {
            bad_session["session_id"] = json!(bad);
            assert_eq!(
                decode_json(&bad_session),
                Err(HookError::Invalid("session_id")),
                "{bad:?}"
            );
        }
        let mut bad_tool = permission_request("Bash", &json!({"command": "ls"}));
        bad_tool["tool_name"] = json!("Bash\n1");
        assert_eq!(decode_json(&bad_tool), Err(HookError::Invalid("tool_name")));
    }
}
