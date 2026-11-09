//! Turning Codex rollout records into thread items.
//!
//! A rollout line is `{"timestamp", "type", "payload"}`. What the model saw
//! and did is in `response_item`s, which map onto the same items as a
//! Claude transcript:
//!
//! | `response_item` payload | thread item |
//! |---|---|
//! | `message`, role `user` | [`ThreadContent::User`], starting a turn, unless it is context Codex injected (`<environment_context>`, AGENTS.md, …) |
//! | `message`, role `assistant` | [`ThreadContent::Assistant`] (commentary and final answers alike) |
//! | `function_call`, `custom_tool_call`, `local_shell_call` | a running [`ThreadContent::Tool`] |
//! | `function_call_output`, `custom_tool_call_output` | that tool's output and status |
//! | `apply_patch` succeeding | one [`ThreadContent::Diff`] per file |
//! | `web_search_call` | a finished tool |
//!
//! A typed message starts a thread turn only when it opens a new Codex
//! turn: one the user sends while Codex is working (steering) carries the
//! running turn's id and is shown in the current thread turn, leaving its
//! tool calls running (see [`Turns`]).
//!
//! Developer messages (system instructions), reasoning and every other
//! payload are skipped. `session_meta` and `turn_context` only update the
//! working directory used for diff paths. `event_msg`s repeat the same
//! conversation for Codex's own UI and are skipped, as are token counts,
//! world state and any record type this reader predates.

use protocol::{ThreadContent, Timestamp, ToolStatus};
use serde::Deserialize;
use serde_json::Value;

use super::{PendingTool, Thread, mark_unknown};
use crate::diagnostics::{INVALID_JSON, NO_TYPE};
use crate::diff;
use crate::render::truncate;
use crate::time::parse_rfc3339;

/// The tool name shell executions are shown under, as for Claude, so the
/// app renders them as commands.
const SHELL_TOOL: &str = "Bash";

/// Tags Codex wraps around user-role context it writes itself.
const INJECTED_TAGS: &[&str] = &[
    "environment_context",
    "user_instructions",
    "user_shell_command",
    "turn_aborted",
    "subagent_notification",
    "permissions",
    "skill",
    "INSTRUCTIONS",
];

/// How Codex introduces the AGENTS.md it injects as a user message.
const AGENTS_MD: &str = "# AGENTS.md instructions";

/// Codex turn ids, to tell a new prompt from a steering message.
#[derive(Debug, Default)]
pub(super) struct Turns {
    /// The id of the latest turn Codex started (`task_started`).
    context: Option<String>,
    /// The Codex turn of the latest typed message that started a thread
    /// turn.
    started: Option<String>,
}

/// One rollout line.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Rollout {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Value,
}

/// A `response_item` payload. Shapes that vary (content, arguments,
/// output) stay [`Value`]s and are read leniently.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Item {
    Message {
        #[serde(default)]
        role: String,
        #[serde(default)]
        content: Value,
        /// Carries the Codex `turn_id` the message was sent in.
        #[serde(default)]
        internal_chat_message_metadata_passthrough: Value,
    },
    FunctionCall {
        #[serde(default)]
        name: String,
        /// A JSON-encoded string of arguments (or, leniently, an object).
        #[serde(default)]
        arguments: Value,
        call_id: String,
    },
    CustomToolCall {
        #[serde(default)]
        name: String,
        /// Free-form input (an `apply_patch` envelope, `exec` JavaScript).
        #[serde(default)]
        input: Value,
        call_id: String,
    },
    LocalShellCall {
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        action: Value,
    },
    FunctionCallOutput {
        call_id: String,
        #[serde(default)]
        output: Value,
    },
    CustomToolCallOutput {
        call_id: String,
        #[serde(default)]
        output: Value,
    },
    WebSearchCall {
        #[serde(default)]
        action: Value,
    },
    /// Reasoning, tool lists and anything newer: not rendered.
    #[serde(other)]
    Other,
}

impl Thread {
    /// Normalizes one Codex rollout line.
    pub(super) fn codex(&mut self, line: &[u8]) {
        let Ok(rollout) = serde_json::from_slice::<Rollout>(line) else {
            return self.diagnostics.malformed(INVALID_JSON);
        };
        let at = rollout.timestamp.as_deref().and_then(parse_rfc3339);
        match rollout.kind.as_deref() {
            Some("response_item") => match serde_json::from_value::<Item>(rollout.payload) {
                Ok(item) => self.codex_item(at, item),
                Err(_) => self.diagnostics.malformed("codex:response_item"),
            },
            Some(kind @ ("session_meta" | "turn_context")) => {
                if let Some(cwd) = rollout.payload.get("cwd").and_then(Value::as_str) {
                    self.cwd = Some(cwd.to_owned());
                }
                self.diagnostics.skip(&format!("codex:{kind}"));
            }
            Some("event_msg") => {
                // Written as a turn starts, before its first message (unlike
                // `turn_context`, written after it).
                if rollout.payload.get("type").and_then(Value::as_str) == Some("task_started") {
                    self.codex_turns.context = turn_id(&rollout.payload);
                }
                self.diagnostics.skip("codex:event_msg");
            }
            Some(kind @ ("world_state" | "token_usage_record" | "compacted")) => {
                self.diagnostics.skip(&format!("codex:{kind}"));
            }
            Some(kind) => self.diagnostics.skip_type(kind),
            None => self.diagnostics.malformed(NO_TYPE),
        }
    }

    fn codex_item(&mut self, at: Option<Timestamp>, item: Item) {
        match item {
            Item::Message {
                role,
                content,
                internal_chat_message_metadata_passthrough: meta,
            } => match role.as_str() {
                "user" => {
                    let turn = turn_id(&meta).or_else(|| self.codex_turns.context.clone());
                    self.codex_user(at, &content, turn);
                }
                "assistant" => {
                    let text = message_text(&content);
                    if !text.trim().is_empty() {
                        let text = truncate(text, self.limits.text);
                        self.push(at, ThreadContent::Assistant { text });
                    }
                }
                _ => self.diagnostics.skip("codex:message:other-role"),
            },
            Item::FunctionCall {
                name,
                arguments,
                call_id,
            } => {
                let arguments = match arguments {
                    Value::String(text) => {
                        serde_json::from_str(&text).unwrap_or(Value::String(text))
                    }
                    other => other,
                };
                let call = function_call(&name, &arguments);
                self.codex_call(at, call_id, call);
            }
            Item::CustomToolCall {
                name,
                input,
                call_id,
            } => {
                let input = input.as_str().unwrap_or_default();
                let call = match name.as_str() {
                    "apply_patch" => Call::patch(input.to_owned()),
                    "exec" => exec_call(input)
                        .unwrap_or_else(|| Call::plain(name.clone(), input.to_owned())),
                    _ => Call::plain(name.clone(), input.to_owned()),
                };
                self.codex_call(at, call_id, call);
            }
            Item::LocalShellCall {
                call_id,
                id,
                action,
            } => {
                let Some(call_id) = call_id.or(id) else {
                    return self.diagnostics.malformed("codex:local_shell_call");
                };
                let call = action
                    .get("command")
                    .map_or_else(|| Call::shell(String::new()), shell_call);
                self.codex_call(at, call_id, call);
            }
            Item::FunctionCallOutput { call_id, output }
            | Item::CustomToolCallOutput { call_id, output } => {
                self.codex_output(at, &call_id, &output);
            }
            Item::WebSearchCall { action } => {
                let query = field(&action, &["query", "url"]).unwrap_or_default();
                let input = truncate(query, self.limits.tool_input);
                self.push(
                    at,
                    ThreadContent::Tool {
                        name: "web_search".to_owned(),
                        input,
                        output: None,
                        status: ToolStatus::Ok,
                    },
                );
            }
            Item::Other => self.diagnostics.skip("codex:response_item:other"),
        }
    }

    /// A user-role message sent in Codex turn `turn` (if known): typed
    /// text starts a thread turn unless it was sent into the Codex turn
    /// the current thread turn began with; injected context is skipped.
    fn codex_user(&mut self, at: Option<Timestamp>, content: &Value, turn: Option<String>) {
        let parts: Vec<String> = parts(content)
            .filter_map(|part| match part {
                Part::Text(text) if is_injected(text) => None,
                Part::Text(text) => Some(text.to_owned()),
                Part::Image => Some("[image]".to_owned()),
            })
            .filter(|part| !part.trim().is_empty())
            .collect();
        if parts.is_empty() {
            return self.diagnostics.skip("user:notice");
        }
        let steering = turn.is_some() && turn == self.codex_turns.started;
        if !steering {
            self.start_turn();
            self.codex_turns.started = turn;
        }
        let text = truncate(parts.join("\n\n"), self.limits.text);
        self.push(at, ThreadContent::User { text });
    }

    fn codex_call(&mut self, at: Option<Timestamp>, call_id: String, call: Call) {
        let diffs = call
            .patch
            .map(|patch| diff::apply_patch(&patch, self.cwd.as_deref(), self.limits.patch))
            .unwrap_or_default();
        let input = truncate(call.input, self.limits.tool_input);
        let cursor = self.push(
            at,
            ThreadContent::Tool {
                name: call.name,
                input,
                output: None,
                status: ToolStatus::Running,
            },
        );
        let pending = PendingTool {
            cursor,
            planned: None,
            diffs,
            cwd: None,
        };
        if let Some(older) = self.pending.insert(call_id, pending) {
            mark_unknown(&mut self.store, older.cursor);
        }
        self.settle_stale();
    }

    fn codex_output(&mut self, at: Option<Timestamp>, call_id: &str, output: &Value) {
        let Some(pending) = self.pending.remove(call_id) else {
            self.diagnostics.orphan_results += 1;
            return;
        };
        let Output {
            text,
            failed,
            patched,
        } = output_text(output);
        let status = if failed {
            ToolStatus::Error
        } else {
            ToolStatus::Ok
        };
        let text = truncate(text, self.limits.tool_output);
        self.store.update(pending.cursor, |content| {
            if let ThreadContent::Tool {
                output,
                status: item_status,
                ..
            } = content
            {
                *output = (!text.trim().is_empty()).then_some(text);
                *item_status = status;
            }
        });
        // A patch is shown only once Codex says it applied.
        if status == ToolStatus::Ok && patched {
            for diff in pending.diffs {
                self.push(at, diff);
            }
        }
    }
}

/// A piece of message content.
enum Part<'a> {
    Text(&'a str),
    Image,
}

/// The parts of a message's `content`: a string, or an array of
/// `input_text`/`output_text`/`text` and `input_image` entries. Anything
/// else is left out.
fn parts(content: &Value) -> impl Iterator<Item = Part<'_>> {
    let (single, many) = match content {
        Value::String(text) => (Some(Part::Text(text)), None),
        Value::Array(entries) => (None, Some(entries.iter())),
        _ => (None, None),
    };
    single
        .into_iter()
        .chain(many.into_iter().flatten().filter_map(|entry| {
            match entry.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => {
                    entry.get("text").and_then(Value::as_str).map(Part::Text)
                }
                Some("input_image" | "image") => Some(Part::Image),
                _ => None,
            }
        }))
}

/// A message's text parts joined by blank lines.
fn message_text(content: &Value) -> String {
    let texts: Vec<&str> = parts(content)
        .filter_map(|part| match part {
            Part::Text(text) => Some(text),
            Part::Image => None,
        })
        .filter(|text| !text.trim().is_empty())
        .collect();
    texts.join("\n\n")
}

/// Whether user-role text is context Codex wrote: AGENTS.md, or a block
/// opening with one of [`INJECTED_TAGS`] (closed or cut short).
fn is_injected(text: &str) -> bool {
    let text = text.trim_start();
    if text.starts_with(AGENTS_MD) {
        return true;
    }
    text.strip_prefix('<')
        .and_then(|rest| rest.split_once('>'))
        .is_some_and(|(tag, _)| INJECTED_TAGS.contains(&tag))
}

/// The `turn_id` in a record's payload or message metadata.
fn turn_id(value: &Value) -> Option<String> {
    value
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The first string among `keys` in `value`.
fn field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(str::to_owned)
}

/// A command given as an argv array or a string. `bash -lc <script>`
/// (and `sh -c`, `zsh -lc`) shows as the script.
fn command_text(command: &Value) -> String {
    match command {
        Value::String(text) => text.clone(),
        Value::Array(argv) => {
            let argv: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
            match argv.as_slice() {
                [shell, flag, script]
                    if matches!(*flag, "-c" | "-lc")
                        && ["bash", "sh", "zsh"]
                            .iter()
                            .any(|name| shell.rsplit('/').next() == Some(name)) =>
                {
                    (*script).to_owned()
                }
                _ => argv.join(" "),
            }
        }
        _ => String::new(),
    }
}

/// A tool call as shown: its name, one-line input, and the `apply_patch`
/// envelope it carries, if any.
struct Call {
    name: String,
    input: String,
    patch: Option<String>,
}

impl Call {
    fn plain(name: String, input: String) -> Self {
        Self {
            name,
            input,
            patch: None,
        }
    }

    /// A shell command, as [`SHELL_TOOL`]. A script that runs
    /// `apply_patch` on an envelope (`apply_patch <<'EOF'` with the
    /// envelope below) carries it; one that only writes or prints a patch
    /// (`cat > fix.patch <<EOF`) does not.
    fn shell(command: String) -> Self {
        let patch = runs_apply_patch(&command).then(|| command.clone());
        Self {
            name: SHELL_TOOL.to_owned(),
            input: command,
            patch,
        }
    }

    /// An `apply_patch` call, shown with the files it touches.
    fn patch(patch: String) -> Self {
        let files = diff::patched_paths(&patch);
        let input = if files.is_empty() {
            patch.clone()
        } else {
            files.join("\n")
        };
        Self {
            name: "apply_patch".to_owned(),
            input,
            patch: Some(patch),
        }
    }
}

/// Whether a shell script runs `apply_patch` (or `applypatch`) as a
/// command whose envelope follows it: a line starting with the program
/// before the `*** Begin Patch` line.
fn runs_apply_patch(script: &str) -> bool {
    let mut invoked = false;
    for line in script.lines() {
        let line = line.trim();
        if line == diff::BEGIN_PATCH {
            return invoked;
        }
        let program = line.split_whitespace().next().unwrap_or_default();
        invoked |= matches!(program, "apply_patch" | "applypatch");
    }
    false
}

/// A shell call from its `command` (argv or string). `["apply_patch",
/// <envelope>]` is an `apply_patch` call.
fn shell_call(command: &Value) -> Call {
    if let Some([program, patch]) = command.as_array().map(Vec::as_slice)
        && matches!(program.as_str(), Some("apply_patch" | "applypatch"))
        && let Some(patch) = patch.as_str()
    {
        return Call::patch(patch.to_owned());
    }
    Call::shell(command_text(command))
}

/// A `function_call` as shown: shell calls as [`SHELL_TOOL`] with their
/// command, `apply_patch` with the files it touches, anything else with
/// its first recognisable argument or compact JSON.
fn function_call(name: &str, arguments: &Value) -> Call {
    match name {
        "shell" | "container.exec" | "local_shell" | "exec_command" | "shell_command" => arguments
            .get("command")
            .or_else(|| arguments.get("cmd"))
            .map_or_else(|| Call::shell(String::new()), shell_call),
        "apply_patch" => Call::patch(field(arguments, &["input", "patch"]).unwrap_or_default()),
        _ => {
            let input = match arguments {
                Value::String(text) => text.clone(),
                _ => field(
                    arguments,
                    &["command", "cmd", "path", "query", "url", "prompt"],
                )
                .unwrap_or_else(|| arguments.to_string()),
            };
            Call::plain(name.to_owned(), input)
        }
    }
}

/// The one tool call in Codex's `exec` tool input, JavaScript such as
/// `text(await tools.exec_command({cmd:"ls -la",max_output_tokens:1000}))`:
/// a shell command (the string literal after `cmd:`), or an `apply_patch`
/// (the first string literal in its arguments), if it is the only tool
/// the script calls. `None` for anything else, which then shows as the
/// script itself.
fn exec_call(script: &str) -> Option<Call> {
    if script.matches("tools.").count() != 1 {
        return None;
    }
    if let Some(at) = script.find("tools.exec_command(") {
        let args = script.get(at..)?;
        let start = ["cmd:", "\"cmd\":"]
            .iter()
            .filter_map(|key| args.find(key).map(|at| at + key.len()))
            .min()?;
        return string_literal(args.get(start..)?).map(Call::shell);
    }
    let at = script.find("tools.apply_patch(")? + "tools.apply_patch(".len();
    patch_argument(script.get(at..)?).map(Call::patch)
}

/// The envelope passed to `tools.apply_patch(`, given what follows the
/// parenthesis: only a lone string literal, `"…")`, or one in an object,
/// `{input: "…"})`. Anything computed (`p + "…"`, a variable) is not
/// read, since the literal would not be the patch applied.
fn patch_argument(args: &str) -> Option<String> {
    let args = args.trim_start();
    let (literal, close) = match args.strip_prefix('{') {
        Some(object) => {
            let object = object.trim_start();
            let value = ["input:", "\"input\":"]
                .iter()
                .find_map(|key| object.strip_prefix(key))?;
            (value, '}')
        }
        None => (args, ')'),
    };
    let (patch, rest) = string_literal_and_rest(literal)?;
    rest.trim_start().starts_with(close).then_some(patch)
}

/// The JSON-compatible string literal `text` starts with (after spaces).
fn string_literal(text: &str) -> Option<String> {
    string_literal_and_rest(text).map(|(literal, _)| literal)
}

/// The string literal `text` starts with (after spaces), and the text
/// after it.
fn string_literal_and_rest(text: &str) -> Option<(String, &str)> {
    let literal = text.trim_start();
    if !literal.starts_with('"') {
        return None;
    }
    let mut stream = serde_json::Deserializer::from_str(literal).into_iter::<String>();
    let value = stream.next()?.ok()?;
    Some((value, literal.get(stream.byte_offset()..)?))
}

/// How Codex words a declined approval or an interrupted call.
const ABORTED: &str = "aborted by user";

/// How `apply_patch` reports a patch it applied.
const PATCH_APPLIED: &str = "Success. Updated the following files";

/// A tool output as shown.
#[derive(Debug, PartialEq, Eq)]
struct Output {
    text: String,
    /// The call failed: `success: false`, a non-zero exit code in a
    /// structured result, or Codex's abort notice.
    failed: bool,
    /// An `apply_patch` in the call reported success, or a structured
    /// result exited 0: its diffs may be shown.
    patched: bool,
}

/// A tool output's text and outcome. Outputs are a string (sometimes a
/// JSON result with `output` and `metadata.exit_code`), an array of text
/// parts (Codex's `exec` tool: each command's JSON result is a part of its
/// own), or an object with `content`/`output` and `success`. Only fields
/// of a structured result count: free text is never searched for words or
/// exit codes, so `rg error` succeeds.
fn output_text(output: &Value) -> Output {
    let (text, results): (String, Vec<&Value>) = match output {
        Value::Object(_) => (result_text(output), vec![output]),
        Value::String(text) => (text.clone(), Vec::new()),
        Value::Array(_) => (exec_text(output), Vec::new()),
        _ => (String::new(), Vec::new()),
    };
    // JSON results inside strings: the whole output, or one `exec` part.
    let parsed: Vec<Value> = match output {
        Value::String(text) => vec![text.as_str()],
        Value::Array(_) => parts(output)
            .filter_map(|part| match part {
                Part::Text(text) => Some(text),
                Part::Image => None,
            })
            .collect(),
        _ => Vec::new(),
    }
    .into_iter()
    .filter_map(as_result)
    .collect();
    let text = match (output, parsed.first()) {
        (Value::String(_), Some(result)) => result_text(result),
        _ => text,
    };
    let results: Vec<&Value> = results.into_iter().chain(&parsed).collect();
    let codes: Vec<i64> = results
        .iter()
        .filter_map(|result| {
            result
                .pointer("/metadata/exit_code")
                .or_else(|| result.get("exit_code"))
                .and_then(Value::as_i64)
        })
        .collect();
    // `success` is Codex's only in an output object (a JSON document a tool
    // printed may well have its own).
    let unsuccessful = output.get("success").and_then(Value::as_bool) == Some(false);
    let codes: Vec<i64> = codes.into_iter().chain(header_exit_code(&text)).collect();
    let failed = unsuccessful
        || codes.iter().any(|code| *code != 0)
        || text.trim_start().starts_with(ABORTED);
    let patched = !failed && (text.contains(PATCH_APPLIED) || codes.contains(&0));
    Output {
        text,
        failed,
        patched,
    }
}

/// The text of an `exec` output (an array of parts): each part's text,
/// a JSON result shown by its `output`, joined by blank lines.
fn exec_text(output: &Value) -> String {
    let texts: Vec<String> = parts(output)
        .filter_map(|part| match part {
            Part::Text(text) => {
                Some(as_result(text).map_or_else(|| text.to_owned(), |result| result_text(&result)))
            }
            Part::Image => None,
        })
        .filter(|text| !text.trim().is_empty())
        .collect();
    texts.join("\n\n")
}

/// The exit code in the header Codex writes above a command's output
/// (`Exit code: 2` or `Process exited with code 2`, then `Output:`). Only
/// lines before an `Output:` line count: the command's own output may say
/// anything.
fn header_exit_code(text: &str) -> Option<i64> {
    let mut code = None;
    for line in text.lines() {
        let line = line.trim();
        if line == "Output:" {
            return code;
        }
        let value = line
            .strip_prefix("Exit code:")
            .or_else(|| line.strip_prefix("Process exited with code"));
        if let Some(value) = value {
            code = value.trim().parse().ok();
        }
    }
    None
}

/// `text` as a Codex command result: a JSON object with
/// `metadata.exit_code`, or with both `exit_code` and `output` (one `exec`
/// part). Any other text, a JSON document a tool printed included, is
/// plain output.
fn as_result(text: &str) -> Option<Value> {
    let json = serde_json::from_str::<Value>(text).ok()?;
    let shaped = json.pointer("/metadata/exit_code").is_some()
        || (json.get("exit_code").is_some() && json.get("output").is_some());
    shaped.then_some(json)
}

/// The text of a structured result: its `output` or `content`, else the
/// whole result as JSON.
fn result_text(result: &Value) -> String {
    result
        .get("output")
        .or_else(|| result.get("content"))
        .map_or_else(
            || result.to_string(),
            |inner| match inner {
                Value::String(text) => text.clone(),
                other => message_text(other),
            },
        )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn injected_context_is_recognized() {
        for text in [
            "<environment_context>\n  <cwd>/w</cwd>…[trimmed]",
            "<user_instructions>be brief</user_instructions>",
            "# AGENTS.md instructions for /w\n\n<INSTRUCTIONS>…",
            "  <turn_aborted>\nThe previous turn was interrupted",
        ] {
            assert!(is_injected(text), "{text:?}");
        }
        for text in [
            "Fix the parser",
            "<b>bold</b> please",
            "use <environment_context>",
        ] {
            assert!(!is_injected(text), "{text:?}");
        }
    }

    /// The command of an `exec` script that runs one.
    fn exec_cmd(script: &str) -> Option<String> {
        exec_call(script)
            .filter(|call| call.name == SHELL_TOOL)
            .map(|call| call.input)
    }

    #[test]
    fn exec_scripts_show_their_command() {
        assert_eq!(
            exec_cmd(
                "text(await tools.exec_command({cmd:\"printf hi > hello.txt\\nls -la\",max_output_tokens:1000}));\n"
            )
            .as_deref(),
            Some("printf hi > hello.txt\nls -la")
        );
        assert_eq!(
            exec_cmd(
                "text(await tools.exec_command({\"cmd\":\"printf 'hi\\\\n' > x\",\"sandbox_permissions\":\"require_escalated\"}));"
            )
            .as_deref(),
            Some("printf 'hi\\n' > x")
        );
        // Several tools, or no string literal: the script is shown as is.
        assert_eq!(
            exec_cmd("await tools.exec_command({cmd:\"a\"}); await tools.view({path:\"b\"})"),
            None
        );
        assert_eq!(exec_cmd("await tools.exec_command({cmd: name})"), None);
        assert_eq!(exec_cmd("console.log(1)"), None);
        // An `apply_patch` in a script carries its envelope.
        let patch = exec_call(
            "text(await tools.apply_patch(\"*** Begin Patch\\n*** Add File: a.txt\\n+hi\\n*** End Patch\\n\"));",
        )
        .expect("a patch");
        assert_eq!(
            (
                patch.name.as_str(),
                patch.input.as_str(),
                patch.patch.is_some()
            ),
            ("apply_patch", "a.txt", true)
        );
        let in_object = exec_call(
            "await tools.apply_patch({input: \"*** Begin Patch\\n*** Add File: b.txt\\n+x\\n*** End Patch\"})",
        )
        .expect("a patch");
        assert_eq!(in_object.input, "b.txt");
        // A computed argument: the literal is not the patch applied.
        for script in [
            "await tools.apply_patch(p + \"*** Begin Patch\\n*** Add File: c.txt\\n*** End Patch\")",
            "await tools.apply_patch(\"*** Begin Patch\\n\" + rest)",
            "await tools.apply_patch(patch)",
        ] {
            let call = exec_call(script);
            assert!(call.is_none_or(|c| c.patch.is_none()), "{script}");
        }
    }

    #[test]
    fn shell_commands_unwrap_the_shell() {
        assert_eq!(
            command_text(&json!(["/usr/bin/bash", "-lc", "ls -la"])),
            "ls -la"
        );
        assert_eq!(command_text(&json!(["rg", "-n", "todo"])), "rg -n todo");
        assert_eq!(command_text(&json!("git status")), "git status");
        let shown = |call: Call| (call.name, call.input, call.patch.is_some());
        assert_eq!(
            shown(function_call(
                "shell",
                &json!({"command": ["bash", "-lc", "cargo test"]})
            )),
            ("Bash".to_owned(), "cargo test".to_owned(), false)
        );
        assert_eq!(
            shown(function_call("view_image", &json!({"path": "/w/a.png"}))),
            ("view_image".to_owned(), "/w/a.png".to_owned(), false)
        );
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch\n";
        assert_eq!(
            shown(function_call(
                "shell",
                &json!({"command": ["apply_patch", patch]})
            )),
            ("apply_patch".to_owned(), "a.txt".to_owned(), true)
        );
        let written = format!("cat > fix.patch <<'EOF'\n{patch}EOF");
        assert_eq!(
            shown(function_call(
                "shell",
                &json!({"command": ["bash", "-lc", written]})
            )),
            ("Bash".to_owned(), written.clone(), false)
        );
        let heredoc = format!("apply_patch <<'EOF'\n{patch}EOF");
        assert_eq!(
            shown(function_call(
                "shell",
                &json!({"command": ["bash", "-lc", heredoc]})
            )),
            ("Bash".to_owned(), heredoc.clone(), true)
        );
    }

    #[test]
    fn outputs_report_failures_from_their_structure_only() {
        let out = |output: Value| {
            let Output {
                text,
                failed,
                patched,
            } = output_text(&output);
            (text, failed, patched)
        };
        assert_eq!(out(json!("done")), ("done".to_owned(), false, false));
        // Words and exit codes in free text are content, not outcomes.
        assert!(!out(json!("error: no match in src/")).1);
        assert!(!out(json!("failed tests: 0\n{\"exit_code\": 3} in a log")).1);
        assert_eq!(
            out(json!(
                "{\"output\":\"boom\\n\",\"metadata\":{\"exit_code\":2}}"
            )),
            ("boom\n".to_owned(), true, false)
        );
        assert_eq!(
            out(json!({"content": "nope", "success": false})),
            ("nope".to_owned(), true, false)
        );
        let exec = |code: i64| {
            json!([{"type": "input_text", "text": "Script completed"},
                   {"type": "input_text", "text": format!("{{\"exit_code\":{code},\"output\":\"x\"}}")}])
        };
        assert_eq!(
            out(exec(0)),
            ("Script completed\n\nx".to_owned(), false, true)
        );
        assert!(out(exec(1)).1);
        assert!(out(json!("aborted by user after 47.4s")).1);
        assert!(out(json!("{\"exit_code\": 1, \"output\": \"\"}")).1);
        assert_eq!(
            out(json!("Success. Updated the following files:\nM a.rs\n")),
            (
                "Success. Updated the following files:\nM a.rs\n".to_owned(),
                false,
                true
            )
        );
        assert!(!out(json!("apply_patch verification failed: no such file")).2);
        // Codex's own header above the output states the exit code; the
        // same words in the output itself do not count.
        assert!(out(json!("Exit code: 2\nWall time: 0.1 seconds\nOutput:\nboom")).1);
        assert!(out(json!("Chunk ID: 1\nProcess exited with code 1\nOutput:\n")).1);
        assert!(!out(json!("Exit code: 0\nOutput:\nExit code: 9")).1);
        assert!(!out(json!("Process exited with code 2")).1, "no header");
        // A JSON document a tool printed is output, not a result.
        let api = "{\"success\": false, \"exit_code\": 3, \"items\": []}";
        assert_eq!(out(json!(api)), (api.to_owned(), false, false));
    }
}
