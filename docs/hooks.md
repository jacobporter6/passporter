# Claude Code hooks

Claude Code hooks are where the bridge gets pending input first (design §6.2).
Its screen parser is the fallback. This page records the payloads the bridge
relies on, how they reach it, and how to install the hook.

## Source of truth: Claude Code 2.1.317

These shapes were read from the installed Claude Code, **2.1.317** (`claude
--version`, the native build under `~/.local/share/mise/installs/claude/`).
No hook docs or schema files ship with that build. The shapes below come from
the hook input builders in its bundled source: the common-fields function and
the code that calls the `PermissionRequest`, `Notification` and `Stop` hooks.
Nothing here is guessed. When Claude Code is upgraded, check again and update
this page and `crates/bridge/src/pending/hook.rs`.

### Common fields (every event)

| Field | Type | Notes |
|---|---|---|
| `session_id` | string | The session UUID. herdr reports the same value as the agent's `agent_session.value` (`agent: "claude"`, `kind: "id"`). Remote calls send `served:<id>` instead, which the bridge refuses. |
| `transcript_path` | string | Empty in some modes. |
| `cwd` | string | |
| `permission_mode` | string | e.g. `default`, `plan`. |
| `hook_event_name` | string | The event. |
| `scratchpad_dir` | string, optional | |
| `prompt_id` | string, optional | |
| `agent_id` | string, optional | Set inside subagents. |
| `agent_type` | string, optional | |
| `effort` | `{ "level": … }`, optional | |

### `PermissionRequest`

Runs just before Claude shows a permission dialog. The matcher is the tool name.

| Field | Type | Notes |
|---|---|---|
| `tool_name` | string | `Bash`, `Edit`, `Write`, `WebFetch`, `ExitPlanMode`, `AskUserQuestion`, `mcp__<server>__<tool>`, … |
| `tool_input` | object | The tool's input, e.g. `{ "command", "description" }` for `Bash`, `{ "file_path", … }` for edits, `{ "plan" }` for `ExitPlanMode`. |
| `permission_suggestions` | array | Permission-rule updates Claude would offer (*always allow*). The bridge does not read it. |
| `mcp_server` | string, optional | For MCP tools. |

A hook can answer this event on stdout with a JSON decision. `passporter-hook`
never writes to stdout, so Claude always shows its own dialog. The phone then
answers that dialog with keys, as it does for screen-parsed input.

### `Notification`

| Field | Type | Notes |
|---|---|---|
| `message` | string | e.g. "Claude needs your permission to use Bash". |
| `title` | string, optional | |
| `notification_type` | string | The matcher. Values in 2.1.317 include `permission_prompt`, `idle_prompt`, `agent_needs_input`, `agent_completed`, `elicitation_complete`, `elicitation_response`, `auth_success`, `worker_permission_prompt`, `push_notification`, `plugin_notification`, `computer_use_exit`, `auth_storage_failure`. |

### `Stop`

| Field | Type | Notes |
|---|---|---|
| `stop_hook_active` | bool | |
| `last_assistant_message` | string, optional | |
| `background_tasks` | array | |
| `session_crons` | array | |

`SubagentStop` has its own event name, so it is not a `Stop`.

## What the bridge does with them

- **`PermissionRequest`** for `Bash`, `ExitPlanMode` or `AskUserQuestion`:
  the last 8 requests of each session are kept, newest first. From each one
  the bridge builds a prompt (``Run `cargo test`?``, `Approve the plan?`, or
  the question itself) and a detail (the full command plus its description,
  or the plan), cleaned and bounded. Then it re-checks the blocked agent of
  that session at once.
- **`PermissionRequest` for any other tool** (edits, `WebFetch`, MCP tools):
  validated and ignored. Their dialogs can't be matched exactly. The screen
  shows a diff with line numbers, a URL or a tool summary, and parallel
  requests to the same file, host or tool would look alike. Showing the wrong
  diff or arguments is worse than showing the screen's own, which already
  has the diff.
- **`Stop`** and **`Notification`**: not installed and not used yet. Their
  shapes are recorded above for the end-of-turn questions of design §6.2,
  which come later. If they are forwarded anyway, the bridge refuses them as
  unknown events. They are not needed to clear requests: an `idle_prompt`,
  or a subagent's turn ending, can arrive while a dialog is still open, and a
  request applies only while the screen shows it (below).

### Correlation and preference

When an agent is blocked, the bridge reads and parses its screen as before.
Then it takes the requests stored under the agent's herdr session id and
uses the newest one that **is exactly the dialog on screen**:

- If the screen parsed confidently, it must show a dialog of the request's
  kind.
- **Commands:** the parsed detail must be the whole command, or the whole
  command followed by its description, as Claude draws them. The comparison
  is character for character, with only runs of whitespace collapsed. A line
  wrapped at a space still matches. `ls` never matches `rm -rf ./tools`,
  `git push` never matches `git push --force origin main`, and
  `rm -rf ./build` never matches `rm -rf /build`. A word wrapped mid-way
  doesn't match, and the screen stands alone.
- **Questions:** the parsed prompt must be the whole question, compared the
  same way.
- **Plans:** the parsed detail must be the whole plan, comparing letters and
  digits only. Claude renders the plan's markdown, so `#` and `**` are not on
  screen. A plan is approved, not run as a command.
- **Text the parser cut:** the parser caps the detail at 20 lines or 2000
  characters, and the prompt at 200 characters, and ends cut text with `…`.
  For a long command, plan or question, what the screen kept must be the
  start of the request's text, and **no other stored request may fit**. If
  two requests share that start, neither is used. This is the case the hook
  exists for: the phone gets the whole command or plan (up to 100 lines or
  4000 characters), which the screen parse can't show. A dialog so tall
  that its title is more than 40 lines above the question (or scrolled off
  screen) has no parsed detail at all, so nothing can match it and the
  screen's wording stands.

The request chosen for a dialog is remembered with that dialog's pending id.
If newer requests (parallel subagents) push it out of the 8 kept, the dialog
keeps its wording while it stays open, rather than flipping back to the
screen's.

A matching request supplies **only the prompt and detail**. A question's
request has no detail, so it leaves the screen's alone. These all stay the
screen's:

- the options and their keys;
- the kind of an unparsed dialog (`unknown`, so it still counts as a dialog
  for step-up);
- the source (`screen`, with its confidence and screen tail for the keypad);
- the pending **id**.

The hook does not say which keys answer the dialog, so the car's
unreliable-source rule must keep applying to the options. The hook's text is
the screen's text, or its continuation, so the hook adds no new facts, only a
fuller wording of what the screen shows. That is why the id stays the screen's: it doesn't change when a
hook arrives late or is evicted, so an answer in flight is never refused as
stale for that reason.

If no request matches, or the hook is not installed, everything works as it
did before, from the screen alone.

### Untrusted input

- Each payload is capped at **512 KiB**, by both the hook and the bridge. A
  bigger one is dropped unparsed, and the screen parse covers that dialog.
- Payloads are deserialized into typed structs. `hook_event_name` must be
  `PermissionRequest`. Every field the bridge reads must exist with
  its documented type, and `tool_input` must be an object. The session id
  must pass the same check as the session part of an agent id
  (`protocol::SessionId`), and the tool name must be 1–128 characters from
  `[A-Za-z0-9_.:-]`. Display text is cleaned by the same rules as agent
  labels.
- **An explicit exception to the strict-serde convention in `AGENTS.md`:**
  fields the bridge does not read are ignored, not refused
  (`deny_unknown_fields` is not set). Claude Code adds payload fields between
  releases (2.1.317 already has several optional ones), and refusing them
  would quietly turn hooks off at every upgrade. Nothing unread can influence
  the result, and everything that is read is strictly typed.
- Errors are logged by category and position only, so no payload text
  reaches the log.

## Transport

- The socket is `$XDG_RUNTIME_DIR/passporter/hooks.sock`, or
  `$PASSPORTER_HOOK_SOCKET` if that is set to an absolute path. Without
  either, hooks are off; there is no fallback to `/tmp`.
- The socket file is mode **600**. Its directory is mode 700: the bridge
  creates it that way and refuses to use it if it is group- or
  world-accessible or owned by someone else. A connection from another uid
  is refused (`SO_PEERCRED`).
- Each connection carries one payload, then EOF. The bridge never replies.
  It reads with a 2 s timeout and handles at most 16 connections at once.
- If the bridge cannot bind the socket, it logs a warning and carries on with
  the screen parse. While it listens, a bridge holds an exclusive lock on
  `hooks.sock.lock`, so two bridges never race: the second fails to bind.
  Only the lock holder replaces a stale socket file left by a crash, or
  removes its own socket on shutdown.
- The deploy unit (`deploy/bridge/passporter-bridge.service`) sets
  `RuntimeDirectory=passporter` (mode 700). Without it, `ProtectSystem=strict`
  leaves `$XDG_RUNTIME_DIR` read-only and hooks would be off.

## The hook binary

`passporter-hook` is built with the bridge (`cargo build --release -p
bridge`).

- `passporter-hook forward` is the hook command. It reads stdin up to the cap
  and then gives the bridge at most 1 s in total. The connect and write run
  on a thread, because a wedged bridge with a full backlog makes `connect`
  block. It always exits 0 and never writes to stdout or stderr, so a
  missing, slow or wedged bridge never gets in Claude's way.
- `passporter-hook install --settings <file> [--hook <path>] [--write]`
  prints the settings change as a diff and, by default, does nothing else.
  `--write` applies it. Whether the hook is already there is decided on the
  parsed JSON, so a file in any format that already has it is reported as
  installed and never rewritten. If an existing `passporter-hook … forward`
  entry points at an old path, it is updated; the command is parsed as a
  shell word, quotes and escapes included. Other settings keep their order,
  and every key, number and string keeps its exact text (`1e3`, big
  integers and `\u` escapes are not rewritten); only the layout becomes
  two-space indentation.
  The write is atomic and keeps the file's mode. A symlinked settings file
  (dotfiles) stays a symlink, even a dangling one, because the file it names
  is what gets replaced. A missing directory is reported as such. The temp
  file is created exclusively with the
  final mode, so a file planted at its name makes the write fail rather than
  be followed. The hook entries look like this:

```json
"PermissionRequest": [
  { "matcher": "*", "hooks": [
    { "type": "command", "command": "/path/to/passporter-hook forward", "timeout": 5 } ] }
]
```

Only `PermissionRequest` is installed. Notification and Stop hooks would
cost a process and a socket round trip per event for nothing.

### Installing (run it yourself; never let an agent do it)

The installer has only been tested against temp files. Nobody has run it
against a real `~/.claude/settings.json`. To install:

```sh
cargo build --release -p bridge
install -m 755 target/release/passporter-hook ~/.local/bin/
# 1. Dry run: read the diff.
~/.local/bin/passporter-hook install --settings ~/.claude/settings.json
# 2. Apply it (this command is also printed at the end of the dry run).
~/.local/bin/passporter-hook install --settings ~/.claude/settings.json \
  --hook ~/.local/bin/passporter-hook --write
```

Claude Code picks up hook changes in new sessions. The `/hooks` menu shows
what is installed.
