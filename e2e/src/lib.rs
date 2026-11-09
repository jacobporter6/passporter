//! Headless browser tests for Passporter (`just e2e`).
//!
//! The real `passporter-bridge` binary serves the real PWA build on
//! loopback, following a scripted fake herdr ([`herdr`]): a unix socket
//! that speaks herdr's protocol, so the bridge needs no test-only mode.
//! System Chromium runs headless at phone size and is driven over the
//! `DevTools` protocol by the small client in [`cdp`].
//!
//! - [`herdr`]: the fake herdr socket.
//! - [`bridge`]: the bridge process with a throwaway config.
//! - [`cdp`]: Chromium discovery, launch and a minimal CDP client.
//! - [`whisper`]: a fake `whisper-server` with a canned transcript.
//! - [`app`]: the rig (all of the above, started together) and the steps
//!   the scenarios share: tap, wait for the page, connect.
//! - [`World`]: the fixture agents and transcripts the scenarios expect.
//!
//! The fixtures follow reality, not convenience: the finished agent's
//! transcript is `crates/transcripts/tests/fixtures/realistic.jsonl`
//! (every record kind Claude Code writes, injected ones included), the
//! blocked agent's screen ticks, and [`World::reality`] adds agents the
//! way herdr reports them on a busy day (no session id, no name, the
//! swarm's quiet workspace, a Codex agent).
//!
//! The scenarios live in `tests/`; they are `#[ignore]`d so `just gate`
//! (offline, sandboxed) never runs them.

pub mod app;
pub mod bridge;
pub mod cdp;
pub mod herdr;
pub mod whisper;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::herdr::{FakeAgent, Kind, Status};

/// A blocked agent: its screen is Claude Code's bash permission prompt.
pub const BLOCKED: &str = "parser-fix";
/// A working agent.
pub const WORKING: &str = "docs-pass";
/// A finished agent with a realistic transcript.
pub const DONE: &str = "release-notes";
/// The finished agent's pane.
pub const DONE_PANE: &str = "w2:p1";
/// The blocked agent's pane.
pub const BLOCKED_PANE: &str = "w1:p1";
/// The working agent's pane.
pub const WORKING_PANE: &str = "w1:p2";
/// The last thing [`DONE`] said, as its thread should show it.
/// It ends on a question, so the bridge offers reply suggestions.
pub const DONE_REPLY: &str = "Release notes are drafted in CHANGELOG.md. Shall I open the PR?";
/// The reply the fake summarizer suggests to every question.
pub const SUGGESTED: &str = "Yes, open the PR.";
/// What the fake `whisper-server` hears in every clip, as the dictated
/// message should read.
pub const DICTATED: &str = "Run the release checklist next.";
/// [`DICTATED`] as whisper.cpp's server writes it: one line per segment,
/// each starting with a space only where it starts a word. The segments
/// here split a word and come before a full stop, as real ones do.
pub const WHISPER_TEXT: &str = " Run the re\nlease checklist\n next\n.\n";
/// The paragraph before [`DONE_REPLY`] in [`DONE`]'s reply: a `!` command
/// for its session ([`DONE_COMMAND`]) and a decoy for another terminal
/// ([`DONE_DECOY`]), so terminal mode offers both.
pub const DONE_COMMANDS: &str = "Before I open it, run `! git log --oneline -5` here so I can \
     check the history, or `git log --stat` in another terminal to read it yourself.";
/// The command [`DONE_COMMANDS`] asks for in the session.
pub const DONE_COMMAND: &str = "git log --oneline -5";
/// The decoy [`DONE_COMMANDS`] offers alongside.
pub const DONE_DECOY: &str = "git log --stat";
/// What the user asked [`DONE`] in its last turn.
pub const DONE_PROMPT: &str = "Draft the release notes for 0.3.";
/// What the user asked [`DONE`] in its first, long turn (before `/compact`).
pub const DONE_LONG_PROMPT: &str =
    "Refactor the parser's error type into an enum and update every caller.";
/// A prompt the user queued while [`DONE`] worked: theirs, in the last turn.
pub const DONE_QUEUED: &str = "Mention the parser fix too.";
/// A subagent's report in [`DONE`]'s transcript: Claude Code records it as
/// user input, but the thread must not show it as the user's.
pub const DONE_REPORT: &str = "The changelog has no entry for the parser fix.";
/// How the thread labels [`DONE_REPORT`].
pub const DONE_REPORT_LABEL: &str = "Report from an agent (changelog-check)";
/// How the thread labels a background task's notification.
pub const DONE_TASK_LABEL: &str =
    "Background command \"cargo test -p widget\" completed (exit code 0)";
/// How the thread labels the `!` command the user ran.
pub const DONE_SHELL_LABEL: &str = "You ran `git status --short`";
/// Text Claude Code injects as user input that must never show: the
/// compact summary, the slash-command caveat, a system reminder and a task
/// notification's raw tags.
pub const INJECTED: [&str; 4] = [
    "This session is being continued",
    "Caveat: The messages below",
    "The user ran a shell command",
    "<task-notification>",
];
/// The headline the fake summarizer gives every turn.
pub const HEADLINE: &str = "Drafted the 0.3 release notes.";
/// The TL;DR outcome the fake summarizer gives every turn.
pub const TLDR_OUTCOME: &str = "Release notes drafted; nothing left to decide.";
/// The status line the fake summarizer gives a working agent.
pub const STATUS_LINE: &str = "Working through the docs";
/// The project the phone may start new agents in.
pub const SPAWN_PROJECT: &str = "gizmo";

/// [`World::reality`]: a working Claude agent herdr reports with no
/// session id and no name (it is known by its terminal title), as after a
/// restart: the bridge matches its transcript by folder.
pub const NO_SESSION: &str = "Greenhouse controller brief";
/// What [`NO_SESSION`] said last: in the newest transcript in its folder.
pub const NO_SESSION_REPLY: &str = "Wiring the greenhouse fan controller now.";
/// [`World::reality`]: an orchestrator agent, finished, in the swarm's
/// quiet workspace: its finished turn is not news for the user.
pub const SWARM: &str = "impl-t41b";
/// The swarm's herdr workspace label (quiet when done: the bridge's
/// `[push]` lists it).
pub const SWARM_WORKSPACE: &str = "agent-swarm";
/// [`World::reality`]: one of the user's agents, finished, unnamed.
pub const USER_DONE: &str = "Soil sensor integration";
/// [`World::reality`]: a Codex agent at its command approval dialog.
pub const CODEX: &str = "codex-hello";
/// [`CODEX`]'s pane.
pub const CODEX_PANE: &str = "w6:p9";

/// A stand-in for `claude -p` (the bridge's `[tldr] claude`): answers each
/// structured request by its schema, so the bridge's TL;DRs, headlines and
/// status lines run end to end without a model, and never the real CLI.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
cat >/dev/null
case "$*" in
  *'"status_line"'*) out='{"status_line":"STATUS_LINE"}' ;;
  *'"headline"'*) out='{"headline":"HEADLINE"}' ;;
  *'"outcome"'*) out='{"outcome":"TLDR_OUTCOME","changed":[],"decide":null,"risks":[]}' ;;
  *) out='{"suggestions":["SUGGESTED"]}' ;;
esac
printf '{"type":"result","subtype":"success","is_error":false,"structured_output":%s}\n' "$out"
"#;

/// A synthetic Claude Code permission prompt, as herdr's `agent.read`
/// returns it; above the dialog, a timer that moves on every read.
const PERMISSION_SCREEN: &str = "\
⏺ I'll run the bridge tests to check the change.

⏺ Bash(cargo nextest run -p bridge)
  ⎿  Running… ({tick}s)

────────────────────────────────────────────────────────────────────────────────
 Bash command

   cargo nextest run -p bridge
   Run the bridge tests

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for cargo nextest run commands in
      /home/dev/git/passporter
   3. No, and tell Claude what to do differently (esc)
";

/// The finished agent's transcript: every record kind Claude Code writes.
const REALISTIC: &str = include_str!("../../crates/transcripts/tests/fixtures/realistic.jsonl");
/// The folder and session `REALISTIC` was written for.
const REALISTIC_CWD: &str = "/home/dev/git/widget";
const REALISTIC_SESSION: &str = "0e2e0000-0000-4000-8000-000000000003";

/// A real Codex 0.166 rollout (declined approval) and its dialog screen.
const CODEX_ROLLOUT: &str = include_str!(
    "../../crates/transcripts/tests/fixtures/codex/interactive_declined_approval.jsonl"
);
const CODEX_SESSION: &str = "01a1bb6f-45a9-7332-adc0-fd8600af2b96";
const CODEX_CWD: &str = "/work/codex-probe";
const CODEX_SCREEN: &str =
    include_str!("../../crates/bridge/src/pending/fixtures/codex_0_166_command_approval.txt");

/// The fixture world under one scratch directory: repos (agent cwds), a
/// Claude projects dir and a Codex sessions dir with their transcripts.
#[derive(Debug)]
pub struct World {
    /// The fake agents, in herdr's order.
    pub agents: Vec<FakeAgent>,
    /// herdr's workspaces: id and label.
    pub workspaces: Vec<(String, String)>,
    /// The Claude projects dir to give the bridge.
    pub projects_dir: PathBuf,
    /// The Codex sessions dir to give the bridge.
    pub codex_dir: PathBuf,
    /// The folder of [`SPAWN_PROJECT`].
    pub spawn_project: PathBuf,
    /// The fake `claude` executable to give the bridge.
    pub claude: PathBuf,
}

impl World {
    /// The everyday world: one agent blocked ([`BLOCKED`]), one working
    /// ([`WORKING`]) and one finished ([`DONE`]), all named. The working
    /// one runs with hooks off, so herdr names no session for it: the
    /// bridge matches its transcript by folder.
    pub fn create(root: &Path) -> std::io::Result<Self> {
        let repos = root.join("repos");
        let mut world = Self {
            agents: Vec::new(),
            workspaces: vec![
                ("w1".to_owned(), "widget".to_owned()),
                ("w2".to_owned(), "releases".to_owned()),
            ],
            projects_dir: root.join("claude-projects"),
            codex_dir: root.join("codex-sessions"),
            spawn_project: repos.join(SPAWN_PROJECT),
            claude: root.join("fake-claude"),
        };
        let mut blocked = FakeAgent::claude(
            BLOCKED_PANE,
            BLOCKED,
            Status::Blocked,
            "0e2e0000-0000-4000-8000-000000000001",
            repos.join("widget"),
        );
        PERMISSION_SCREEN.clone_into(&mut blocked.screen);
        let working_file = FakeAgent::claude(
            WORKING_PANE,
            WORKING,
            Status::Working,
            "0e2e0000-0000-4000-8000-000000000002",
            repos.join("gadget"),
        );
        let working = FakeAgent {
            session: None,
            ..working_file.clone()
        };
        let done = FakeAgent::claude(
            DONE_PANE,
            DONE,
            Status::Done,
            REALISTIC_SESSION,
            repos.join("widget"),
        );
        world.claude_transcript(
            &blocked,
            &simple_transcript(&blocked, "Carry on.", "On it."),
        )?;
        world.claude_transcript(
            &working_file,
            &simple_transcript(&working_file, "Carry on.", "On it."),
        )?;
        world.claude_transcript(&done, &realistic_transcript(&done))?;
        world.agents = vec![blocked, working, done];
        std::fs::create_dir_all(&world.spawn_project)?;
        std::fs::create_dir_all(&world.codex_dir)?;
        std::fs::write(
            &world.claude,
            FAKE_CLAUDE
                .replace("STATUS_LINE", STATUS_LINE)
                .replace("HEADLINE", HEADLINE)
                .replace("TLDR_OUTCOME", TLDR_OUTCOME)
                .replace("SUGGESTED", SUGGESTED),
        )?;
        std::fs::set_permissions(&world.claude, std::fs::Permissions::from_mode(0o755))?;
        Ok(world)
    }

    /// The everyday world plus agents as herdr reports them on a busy
    /// day: [`NO_SESSION`] (working, no session id, no name),
    /// [`SWARM`] (finished, in [`SWARM_WORKSPACE`], no session id),
    /// [`USER_DONE`] (finished, unnamed) and [`CODEX`] (a Codex agent at
    /// its approval dialog, with its rollout).
    pub fn reality(root: &Path) -> std::io::Result<Self> {
        let mut world = Self::create(root)?;
        let repos = root.join("repos");
        world.workspaces.extend([
            ("w6".to_owned(), "greenhouse".to_owned()),
            ("wD".to_owned(), SWARM_WORKSPACE.to_owned()),
        ]);

        let no_session = FakeAgent {
            pane: "w6:p4".to_owned(),
            name: None,
            title: NO_SESSION.to_owned(),
            kind: Kind::Claude,
            status: Status::Working,
            session: None,
            cwd: repos.join("greenhouse-lead"),
            screen: "✻ Wiring… ({tick}s · esc to interrupt)\n".to_owned(),
        };
        // Its transcript exists (Claude writes one whatever herdr knows);
        // only the session id is missing.
        let file = FakeAgent {
            session: Some("0e2e0000-0000-4000-8000-0000000000b1".to_owned()),
            ..no_session.clone()
        };
        world.claude_transcript(
            &file,
            &simple_transcript(&file, "Wire the fan controller.", NO_SESSION_REPLY),
        )?;

        let swarm = FakeAgent {
            pane: "wD:p7".to_owned(),
            name: Some(SWARM.to_owned()),
            title: "T41B: Load-robust integration tests".to_owned(),
            kind: Kind::Claude,
            status: Status::Done,
            session: None,
            cwd: repos.join("agent-swarm"),
            screen: "> \n".to_owned(),
        };
        let user_done = FakeAgent {
            pane: "w6:p8".to_owned(),
            name: None,
            title: USER_DONE.to_owned(),
            kind: Kind::Claude,
            status: Status::Done,
            session: Some("0e2e0000-0000-4000-8000-0000000000b2".to_owned()),
            cwd: repos.join("greenhouse"),
            screen: "> \n".to_owned(),
        };
        world.claude_transcript(
            &user_done,
            &simple_transcript(
                &user_done,
                "Add the soil sensor.",
                "The soil sensor reports.",
            ),
        )?;

        let codex = FakeAgent {
            pane: CODEX_PANE.to_owned(),
            name: Some(CODEX.to_owned()),
            title: CODEX.to_owned(),
            kind: Kind::Codex,
            status: Status::Blocked,
            session: Some(CODEX_SESSION.to_owned()),
            cwd: repos.join("codex-probe"),
            screen: CODEX_SCREEN.to_owned(),
        };
        let day = world.codex_dir.join("2026/11/08");
        std::fs::create_dir_all(&day)?;
        std::fs::create_dir_all(&codex.cwd)?;
        std::fs::write(
            day.join(format!("rollout-2026-11-08T13-53-14-{CODEX_SESSION}.jsonl")),
            CODEX_ROLLOUT.replace(CODEX_CWD, &codex.cwd.to_string_lossy()),
        )?;
        std::fs::create_dir_all(&swarm.cwd)?;
        world.agents.extend([no_session, swarm, user_done, codex]);
        Ok(world)
    }

    /// The workspaces as [`herdr::FakeHerdr::serve`] takes them.
    #[must_use]
    pub fn workspace_labels(&self) -> Vec<(&str, &str)> {
        self.workspaces
            .iter()
            .map(|(id, label)| (id.as_str(), label.as_str()))
            .collect()
    }

    /// Writes `jsonl` as `agent`'s transcript:
    /// `<projects>/<cwd slug>/<session>.jsonl`.
    fn claude_transcript(&self, agent: &FakeAgent, jsonl: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&agent.cwd)?;
        let slug: String = agent
            .cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = self.projects_dir.join(slug);
        std::fs::create_dir_all(&dir)?;
        let session = agent.session.as_deref().unwrap_or("unknown-session");
        std::fs::write(dir.join(format!("{session}.jsonl")), jsonl)
    }
}

/// [`REALISTIC`], moved to `agent`'s folder and session.
fn realistic_transcript(agent: &FakeAgent) -> String {
    REALISTIC
        .replace(REALISTIC_CWD, &agent.cwd.to_string_lossy())
        .replace(
            REALISTIC_SESSION,
            agent.session.as_deref().unwrap_or(REALISTIC_SESSION),
        )
}

/// One user turn and one assistant reply, in Claude Code's transcript shape.
fn simple_transcript(agent: &FakeAgent, prompt: &str, reply: &str) -> String {
    let common = |uuid: &str, parent: Option<&str>, kind: &str, at: &str| {
        json!({
            "parentUuid": parent,
            "isSidechain": false,
            "userType": "external",
            "cwd": agent.cwd,
            "sessionId": agent.session,
            "version": "2.1.317",
            "gitBranch": "main",
            "type": kind,
            "uuid": uuid,
            "timestamp": at,
        })
    };
    let mut user = common(
        "00000000-0000-4000-8000-0000000000a1",
        None,
        "user",
        "2026-11-07T09:00:01.000Z",
    );
    user["message"] = json!({"role": "user", "content": prompt});
    user["origin"] = json!({"kind": "human"});
    let mut assistant = common(
        "00000000-0000-4000-8000-0000000000a2",
        Some("00000000-0000-4000-8000-0000000000a1"),
        "assistant",
        "2026-11-07T09:00:05.000Z",
    );
    assistant["requestId"] = json!("req_e2e");
    assistant["message"] = json!({
        "id": "msg_e2e",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [{"type": "text", "text": reply}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
    });
    format!("{user}\n{assistant}\n")
}
