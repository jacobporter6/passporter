//! Hook-sourced pending input: the `passporter-hook` binary forwarding
//! Claude Code payloads to a bridge's hook socket, correlation by session
//! id, preference over the screen parse, and refusal of oversized and
//! malformed payloads. Also the installer's dry run, against a temp
//! settings file only (`HOME` points at the temp dir too).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use herdr::{AgentSession, AgentStatus, Call, Herdr, PaneId, Target};
use protocol::{ActionResult, PendingInput, PendingKind, PendingSource, RefusalReason};
use serde_json::{Value, json};

use crate::harness::{Bridge, CuttableHerdr, PATIENCE};

pub(crate) const BASH: &str =
    include_str!("../../src/pending/fixtures/synthetic_bash_permission.txt");
/// What the screen alone says of [`wrapped`]: the command's first screen
/// line, marked cut where the narrow pane wrapped it.
const SCREEN_PROMPT: &str = "Run `cargo nextest run -p …`?";
/// What the hook says: the whole command.
pub(crate) const HOOK_PROMPT: &str = "Run `cargo nextest run -p bridge`?";
const HOOK: &str = env!("CARGO_BIN_EXE_passporter-hook");

pub(crate) const SESSION_A: &str = "00000000-0000-4000-8000-00000000000a";
pub(crate) const SESSION_B: &str = "00000000-0000-4000-8000-00000000000b";

/// How long a test waits to be sure something did *not* happen.
const QUIET: Duration = Duration::from_millis(300);

pub(crate) struct Agent {
    pub(crate) pane: PaneId,
    pub(crate) id: String,
}

pub(crate) struct Fixture {
    pub(crate) bridge: Bridge,
    pub(crate) socket: PathBuf,
    pub(crate) a: Agent,
    pub(crate) b: Agent,
    pub(crate) tmp: tempfile::TempDir,
}

/// Two blocked Claude agents showing the same Bash dialog, in sessions A
/// and B, and a bridge listening for hooks.
pub(crate) async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tmp");
    let socket = tmp.path().join("run").join("hooks.sock");
    let audit = tmp.path().join("audit.jsonl");
    let herdr = CuttableHerdr::new();
    let mut panes = Vec::new();
    for (name, session) in [("a", SESSION_A), ("b", SESSION_B)] {
        let pane = herdr.fake.add_agent(name, "claude", AgentStatus::Idle);
        herdr.fake.set_screen(&pane, &wrapped());
        herdr.fake.set_session(
            &pane,
            AgentSession {
                source: "herdr:claude".to_owned(),
                agent: "claude".to_owned(),
                kind: "id".to_owned(),
                value: session.to_owned(),
            },
        );
        panes.push(pane);
    }
    let hook_socket = socket.clone();
    let bridge = Bridge::start_with(herdr, move |settings| {
        settings.actions.audit_log = audit;
        settings.pending.hook_socket = Some(hook_socket);
    })
    .await;
    for pane in &panes {
        bridge.fake().set_status(pane, AgentStatus::Blocked);
    }
    let agents = bridge
        .wait_for_agents("two blocked agents", |a| {
            a.len() == 2 && a.iter().all(|a| a.status == protocol::AgentStatus::Blocked)
        })
        .await;
    let id_of = |name: &str| {
        agents
            .iter()
            .find(|a| a.title == name)
            .unwrap_or_else(|| panic!("agent {name}"))
            .id
            .to_string()
    };
    let (a, b) = (id_of("a"), id_of("b"));
    let mut panes = panes.into_iter();
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !socket.exists() {
        assert!(tokio::time::Instant::now() < deadline, "no hook socket");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Fixture {
        bridge,
        socket,
        a: Agent {
            pane: panes.next().expect("a"),
            id: a,
        },
        b: Agent {
            pane: panes.next().expect("b"),
            id: b,
        },
        tmp,
    }
}

/// [`BASH`] in a pane narrow enough to wrap its command. The parse still
/// sees the whole dialog, but its prompt can only name the command's first
/// screen line ([`SCREEN_PROMPT`]): the hook request, which matches the
/// command with whitespace collapsed, is what names all of it.
fn wrapped() -> String {
    BASH.replace(
        "   cargo nextest run -p bridge\n",
        "   cargo nextest run -p\n   bridge\n",
    )
}

/// A 2.1.317 `PermissionRequest` payload for the command [`BASH`] shows,
/// in `session`.
pub(crate) fn permission_request(session: &str) -> Value {
    bash_request(session, "cargo nextest run -p bridge")
}

/// A 2.1.317 `PermissionRequest` payload for `command` in `session`.
fn bash_request(session: &str, command: &str) -> Value {
    json!({
        "session_id": session,
        "transcript_path": format!("/home/dev/.claude/projects/-x/{session}.jsonl"),
        "cwd": "/home/dev/git/passporter",
        "permission_mode": "default",
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": {"command": command, "description": "Run the bridge tests"},
        "permission_suggestions": [],
    })
}

fn stop(session: &str) -> Value {
    json!({
        "session_id": session, "transcript_path": "", "cwd": "/",
        "permission_mode": "default", "hook_event_name": "Stop",
        "stop_hook_active": false, "last_assistant_message": "Done.",
        "background_tasks": [], "session_crons": [],
    })
}

/// Runs `passporter-hook forward` with `stdin`, pointed at `socket`.
async fn forward(socket: &Path, stdin: Vec<u8>) -> Output {
    let socket = socket.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new(HOOK)
            .arg("forward")
            .env("PASSPORTER_HOOK_SOCKET", &socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawns");
        // The hook may stop reading early (an oversized payload).
        let _ = child.stdin.take().expect("stdin").write_all(&stdin);
        child.wait_with_output().expect("runs")
    })
    .await
    .expect("joins")
}

/// Forwards `payload` through the hook binary, which must stay silent.
pub(crate) async fn send(socket: &Path, payload: &Value) {
    let output = forward(socket, payload.to_string().into_bytes()).await;
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "the hook never writes stdout");
}

/// Writes `bytes` to the socket directly, as a hostile local client could.
async fn send_raw(socket: &Path, bytes: Vec<u8>) {
    let socket = socket.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut stream = UnixStream::connect(socket).expect("connects");
        // The bridge stops reading an oversized payload and closes.
        let _ = stream.write_all(&bytes);
    })
    .await
    .expect("joins");
}

/// Whether a hook request describes `pending`. Either way the options,
/// keys and source are the screen's.
fn is_hooked(pending: &PendingInput) -> bool {
    assert!(
        matches!(pending.source, PendingSource::Screen { .. }),
        "options come from the screen, so the source stays screen: {pending:?}"
    );
    // The wrapped screen names part of the command; the hook all of it.
    pending.prompt == HOOK_PROMPT
}

impl Fixture {
    pub(crate) async fn pending(&self, agent: &Agent) -> PendingInput {
        let response = self
            .bridge
            .get_authed(&format!("/v1/agents/{}/pending", agent.id))
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        serde_json::from_str::<Option<PendingInput>>(&response.body)
            .expect("pending json")
            .expect("a blocked agent has an input")
    }

    /// Waits until `agent`'s input is described by a hook (or not).
    pub(crate) async fn wait_for_hook(&self, agent: &Agent, hook: bool) -> PendingInput {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let pending = self.pending(agent).await;
            if is_hooked(&pending) == hook {
                return pending;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "never {}: {pending:?}",
                if hook {
                    "hook-described"
                } else {
                    "screen-only"
                }
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Asserts `agent`'s input stays screen-only for a while.
    async fn stays_screen(&self, agent: &Agent) {
        tokio::time::sleep(QUIET).await;
        let pending = self.pending(agent).await;
        assert!(!is_hooked(&pending), "{pending:?}");
    }

    pub(crate) async fn seq(&self, agent: &Agent) -> u64 {
        self.bridge
            .fake()
            .agent(&Target::from(&agent.pane))
            .await
            .expect("agent")
            .state_change_seq
    }

    /// Posts a `choose` with idempotency key number `key`.
    async fn choose(
        &self,
        key: u8,
        agent: &Agent,
        pending_id: &str,
        option: &str,
        origin: &str,
    ) -> (u16, ActionResult) {
        let body = json!({
            "idempotencyKey": format!("00000000-0000-4000-8000-{key:012}"),
            "agentId": agent.id,
            "expectedSeq": self.seq(agent).await,
            "createdAt": 1_762_500_000_000_u64,
            "origin": origin,
            "kind": "choose",
            "payload": {"pendingId": pending_id, "optionId": option},
        });
        let response = self
            .bridge
            .post_authed("/v1/actions", &body.to_string())
            .await;
        let result = serde_json::from_str(&response.body)
            .unwrap_or_else(|e| panic!("{e}: {}", response.body));
        (response.status, result)
    }
}

#[tokio::test]
async fn hook_input_is_correlated_by_session_and_preferred_over_the_screen() {
    let f = fixture().await;
    let screen = f.pending(&f.b).await;
    assert!(!is_hooked(&screen));
    assert_eq!(screen.prompt, SCREEN_PROMPT);

    send(&f.socket, &permission_request(SESSION_B)).await;
    let hooked = f.wait_for_hook(&f.b, true).await;
    assert_eq!(hooked.kind, PendingKind::Permission);
    assert_eq!(
        hooked.detail.as_deref(),
        Some("cargo nextest run -p bridge\nRun the bridge tests")
    );
    assert_eq!(
        hooked.options, screen.options,
        "options still come from the screen"
    );
    assert_eq!(
        hooked.id, screen.id,
        "the id does not flip when a hook arrives"
    );

    // Session A has no hook request: its agent keeps the screen parse.
    f.stays_screen(&f.a).await;

    // The cards say what each agent asks: the hook's line, else the screen's.
    let agents = f
        .bridge
        .wait_for_agents("the hook's ask on the card", |agents| {
            agents
                .iter()
                .any(|a| a.id.to_string() == f.b.id && a.ask.as_deref() == Some(HOOK_PROMPT))
        })
        .await;
    let a = agents.iter().find(|a| a.id.to_string() == f.a.id);
    assert_eq!(a.and_then(|a| a.ask.as_deref()), Some(SCREEN_PROMPT));

    // The car refuses what only the screen parsed, and elevated options...
    let car_policy = |result: &ActionResult| {
        matches!(
            result,
            ActionResult::Refused {
                reason: RefusalReason::CarPolicy,
                ..
            }
        )
    };
    let screen_a = f.pending(&f.a).await;
    let (status, result) = f.choose(1, &f.a, screen_a.id.as_str(), "yes", "car").await;
    assert!(car_policy(&result), "{status} {result:?}");
    let (status, result) = f
        .choose(2, &f.b, hooked.id.as_str(), "yes_always", "car")
        .await;
    assert!(car_policy(&result), "{status} {result:?}");
    assert!(
        !f.bridge
            .fake()
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Keys { .. })),
        "nothing typed"
    );
    // ...but may allow once on the dialog the hook described, with the
    // screen's keys, as the app may.
    let (status, result) = f.choose(3, &f.b, hooked.id.as_str(), "yes", "car").await;
    assert_eq!(status, 200, "{result:?}");
    assert_eq!(result, ActionResult::Sent { replayed: false });
    let (status, result) = f.choose(4, &f.b, hooked.id.as_str(), "yes", "app").await;
    assert_eq!(status, 200, "{result:?}");
    assert_eq!(result, ActionResult::Sent { replayed: false });
    assert!(f.bridge.fake().calls().contains(&Call::Keys {
        pane: f.b.pane.clone(),
        keys: vec!["1".to_owned()],
    }));
}

#[tokio::test]
async fn a_request_applies_only_to_the_dialog_it_describes() {
    let f = fixture().await;
    // Another command (a parallel call, or a stale request), or only part
    // of this one: not this dialog.
    send(&f.socket, &bash_request(SESSION_A, "git push origin main")).await;
    send(&f.socket, &bash_request(SESSION_A, "cargo nextest run")).await;
    f.stays_screen(&f.a).await;

    // The matching one applies, even with the other one newer.
    send(&f.socket, &permission_request(SESSION_A)).await;
    send(&f.socket, &bash_request(SESSION_A, "git push origin main")).await;
    let hooked = f.wait_for_hook(&f.a, true).await;

    // A `Stop` hook (not installed, and refused if forwarded anyway) does
    // not flip an open dialog back to the screen.
    send(&f.socket, &stop(SESSION_A)).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(f.pending(&f.a).await.id, hooked.id);

    // Another dialog on screen: the request no longer describes it.
    f.bridge.fake().set_screen(
        &f.a.pane,
        &BASH.replace("cargo nextest run -p bridge", "cargo build --release"),
    );
    f.wait_for_hook(&f.a, false).await;
}

#[tokio::test]
async fn oversized_and_malformed_payloads_are_dropped() {
    let f = fixture().await;
    let mut huge = permission_request(SESSION_A);
    huge["tool_input"]["command"] =
        Value::from("x".repeat(bridge::pending::hook::MAX_PAYLOAD_BYTES));

    // The hook binary refuses to forward it, quietly...
    let output = forward(&f.socket, huge.to_string().into_bytes()).await;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");
    // ...and the bridge refuses it from any other client.
    send_raw(&f.socket, huge.to_string().into_bytes()).await;

    let mut wrong_type = permission_request(SESSION_A);
    wrong_type["tool_input"] = Value::from("not an object");
    let mut bad_session = permission_request(SESSION_A);
    bad_session["session_id"] = Value::from("../../etc");
    let mut unknown_event = permission_request(SESSION_A);
    unknown_event["hook_event_name"] = Value::from("PreToolUse");
    for bytes in [
        b"{not json".to_vec(),
        b"\xff\xfe".to_vec(),
        Vec::new(),
        wrong_type.to_string().into_bytes(),
        bad_session.to_string().into_bytes(),
        unknown_event.to_string().into_bytes(),
    ] {
        send_raw(&f.socket, bytes).await;
    }
    f.stays_screen(&f.a).await;

    // The listener survived all of it.
    send(&f.socket, &permission_request(SESSION_A)).await;
    f.wait_for_hook(&f.a, true).await;
}

#[tokio::test]
async fn the_hook_is_silent_and_succeeds_without_a_bridge() {
    let tmp = tempfile::tempdir().expect("tmp");
    let output = forward(
        &tmp.path().join("no-bridge.sock"),
        permission_request(SESSION_A).to_string().into_bytes(),
    )
    .await;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"");
}

#[test]
fn the_installer_dry_run_leaves_a_temp_settings_file_alone() {
    let tmp = tempfile::tempdir().expect("tmp");
    let settings = tmp.path().join("settings.json");
    let original = "{\n  \"model\": \"opus\"\n}\n";
    std::fs::write(&settings, original).expect("write");
    let hook = Path::new(HOOK).canonicalize().expect("hook path");
    let install = |extra: &[&str]| {
        Command::new(HOOK)
            .arg("install")
            .arg("--settings")
            .arg(&settings)
            .arg("--hook")
            .arg(&hook)
            .args(extra)
            .env("HOME", tmp.path())
            .output()
            .expect("runs")
    };

    let output = install(&[]);
    assert!(output.status.success(), "{output:?}");
    let report = String::from_utf8(output.stdout).expect("utf-8");
    let command = format!("{} forward", hook.display());
    assert!(
        report
            .lines()
            .any(|line| line.starts_with('+') && line.contains(&command)),
        "{report}"
    );
    assert!(report.contains("--write"), "{report}");
    assert_eq!(std::fs::read_to_string(&settings).expect("read"), original);

    // Applying it to the temp file, then planning again, is a no-op.
    assert!(install(&["--write"]).status.success());
    let again = String::from_utf8(install(&[]).stdout).expect("utf-8");
    assert!(again.contains("already installed"), "{again}");
    assert!(
        !tmp.path().join(".claude").exists(),
        "HOME was never touched"
    );
}

#[test]
fn the_installer_needs_an_explicit_settings_file() {
    let tmp = tempfile::tempdir().expect("tmp");
    let output = Command::new(HOOK)
        .arg("install")
        .env("HOME", tmp.path())
        .output()
        .expect("runs");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--settings"));
}
