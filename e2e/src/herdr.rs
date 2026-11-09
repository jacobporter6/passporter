//! A scripted fake herdr: a unix socket speaking herdr's newline-delimited
//! JSON protocol, serving a set of agents.
//!
//! The bridge binary runs unmodified against it (`herdr_socket` in its
//! config), so no test-only mode exists in the bridge itself. It answers
//! what the bridge asks while it serves read-only traffic (`agent.list`,
//! `agent.get`, `agent.read`, `events.subscribe`) and what starting a new
//! agent from the phone takes (`workspace.list`, `workspace.create`,
//! `tab.create`, `agent.start`, `agent.wait`, `agent.prompt`, `tab.close`).
//! A started agent is announced on every subscription with
//! `pane_agent_detected`, as herdr does; a prompted one goes `working`.
//! For runbooks it plays a tab's shell: a line typed into a fresh pane
//! (`pane.send_text`) runs with `sh -c` in the pane's folder, and its
//! output is what `pane.read` returns; `pane.get` reports the pane with the
//! terminal it was made with (terminal mode checks it before a read).
//! Keys (`agent.send_keys`) are recorded, as the answer to a dialog.
//! Anything else that would act on an agent is refused with an error reply.
//!
//! It reports agents the way herdr 0.9.1 really does (see the recorded
//! replies in `crates/herdr/tests/fixtures`), because a tidier fake let
//! bugs through: an agent may have no `agent_session` (swarm agents run
//! with hooks off, restarted ones lose theirs) and no herdr name (started
//! by hand: it is known by its terminal title), every workspace has a
//! label (the swarm's is quiet), `state_change_seq` moves with every status
//! change, screens tick between reads ([`TICK`]), and a call can fail
//! ([`FakeHerdr::fail_next`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// herdr's status words, as the fake reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Waiting at a dialog: its screen is a permission prompt.
    Blocked,
    /// Mid-turn.
    Working,
    /// Finished, not yet looked at.
    Done,
    /// At its prompt (a freshly started agent).
    Idle,
}

impl Status {
    fn wire(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Working => "working",
            Self::Done => "done",
            Self::Idle => "idle",
        }
    }
}

/// The agent programs the fake runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Claude Code: transcripts under the Claude projects dir.
    Claude,
    /// Codex: rollouts under the Codex sessions dir.
    Codex,
}

impl Kind {
    fn wire(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// One agent the fake reports.
#[derive(Debug, Clone)]
pub struct FakeAgent {
    /// herdr pane id, e.g. `w1:p1`; its workspace is the part before `:`.
    pub pane: String,
    /// herdr name, given by `agent start`; `None` for an agent started by
    /// hand, which herdr knows only by its terminal title.
    pub name: Option<String>,
    /// The terminal title (`terminal_title_stripped`).
    pub title: String,
    /// Claude or Codex.
    pub kind: Kind,
    /// Its status.
    pub status: Status,
    /// herdr's session id, when it reports one. Claude: the bridge reads
    /// `<projects>/<cwd slug>/<id>.jsonl`.
    pub session: Option<String>,
    /// Working directory (its last component becomes the repo name).
    pub cwd: PathBuf,
    /// What `agent.read` returns; each [`TICK`] in it becomes a number
    /// that grows with every read, like a spinner's elapsed time.
    pub screen: String,
}

impl FakeAgent {
    /// A named Claude agent with a session id, at `status`.
    #[must_use]
    pub fn claude(pane: &str, name: &str, status: Status, session: &str, cwd: PathBuf) -> Self {
        Self {
            pane: pane.to_owned(),
            name: Some(name.to_owned()),
            title: format!("✳ {name}"),
            kind: Kind::Claude,
            status,
            session: Some(session.to_owned()),
            cwd,
            screen: "⏺ Working on it…\n".to_owned(),
        }
    }

    fn workspace(&self) -> &str {
        self.pane.split(':').next().unwrap_or("w1")
    }
}

/// herdr's agent JSON for `agent`, as `agent list` and `agent get` give
/// it: no `name` for an unnamed agent, no `agent_session` without one.
fn info(agent: &FakeAgent, seq: u64) -> Value {
    let workspace = agent.workspace();
    let mut info = json!({
        "terminal_id": format!("term_{}", agent.pane.replace(':', "_")),
        "pane_id": agent.pane,
        "tab_id": format!("{workspace}:t1"),
        "workspace_id": workspace,
        "agent": agent.kind.wire(),
        "agent_status": agent.status.wire(),
        "state_change_seq": seq,
        "revision": 4,
        "terminal_title": format!("◐ {}", agent.title),
        "terminal_title_stripped": agent.title,
        "cwd": agent.cwd,
        "foreground_cwd": agent.cwd,
        "focused": false,
    });
    if let Some(name) = &agent.name {
        info["name"] = json!(name);
        info["interactive_ready"] = json!(true);
    }
    if let Some(session) = &agent.session {
        info["agent_session"] = json!({
            "source": format!("herdr:{}", agent.kind.wire()),
            "agent": agent.kind.wire(),
            "kind": "id",
            "value": session,
        });
    }
    info
}

/// Replaced by a count of reads so far in [`FakeAgent::screen`]: the
/// part of a real screen that changes while a dialog waits (a spinner's
/// timer, a status line clock).
pub const TICK: &str = "{tick}";

/// An `agent.start` the fake served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// The pane it started in.
    pub pane: String,
    /// The agent kind asked for.
    pub kind: String,
    /// Native arguments (the bridge must send none).
    pub args: Vec<String>,
    /// The new tab's shell directory.
    pub cwd: PathBuf,
}

/// An `agent.send_keys` the fake served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyed {
    /// The agent's pane.
    pub pane: String,
    /// The keys, in order.
    pub keys: Vec<String>,
}

/// An `agent.prompt` the fake served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompted {
    /// The agent's pane.
    pub pane: String,
    /// The text.
    pub text: String,
}

/// A workspace or tab the fake made: id and label.
#[derive(Debug, Clone)]
struct Made {
    id: String,
    label: String,
}

/// What the fake holds and what it was asked.
#[derive(Debug, Default)]
struct State {
    /// The agents, each with its `state_change_seq`.
    agents: Vec<(FakeAgent, u64)>,
    workspaces: Vec<Made>,
    keyed: Vec<Keyed>,
    /// Methods whose next call fails, with herdr's error code.
    faults: Vec<(String, &'static str)>,
    /// Fresh panes (no agent yet), by id, with their shell directory.
    shells: Vec<(String, PathBuf)>,
    started: Vec<Started>,
    prompted: Vec<Prompted>,
    /// Lines typed into shell panes, with the pane.
    typed: Vec<(String, String)>,
    /// What each shell pane has printed so far.
    output: Vec<(String, String)>,
    /// Every pane a tab or workspace was made with, as `pane.get` reports it.
    panes: Vec<Value>,
    next: u32,
}

/// The fake's shared state, with the events subscriptions forward.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    events: broadcast::Sender<Value>,
    reads: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The running fake; stops serving when dropped.
#[derive(Debug)]
pub struct FakeHerdr {
    socket: PathBuf,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl FakeHerdr {
    /// Binds `socket` and serves `agents`, in `workspaces` (id and label,
    /// one for each workspace an agent's pane is in), until dropped.
    pub fn serve(
        socket: &Path,
        agents: Vec<FakeAgent>,
        workspaces: &[(&str, &str)],
    ) -> std::io::Result<Self> {
        let listener = UnixListener::bind(socket)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                agents: agents.into_iter().map(|agent| (agent, 7)).collect(),
                workspaces: workspaces
                    .iter()
                    .map(|(id, label)| Made {
                        id: (*id).to_owned(),
                        label: (*label).to_owned(),
                    })
                    .collect(),
                // Above any fixture workspace (`w1`, `w2`, …).
                next: 90,
                ..State::default()
            }),
            events: broadcast::channel(64).0,
            reads: AtomicU64::new(0),
        });
        let serving = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(connection(stream, Arc::clone(&serving)));
            }
        });
        Ok(Self {
            socket: socket.to_path_buf(),
            shared,
            task,
        })
    }

    /// The socket path to put in the bridge's config.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Every `agent.start` served so far.
    #[must_use]
    pub fn started(&self) -> Vec<Started> {
        self.shared.lock().started.clone()
    }

    /// Every `agent.prompt` served so far.
    #[must_use]
    pub fn prompted(&self) -> Vec<Prompted> {
        self.shared.lock().prompted.clone()
    }

    /// Every `agent.send_keys` served so far.
    #[must_use]
    pub fn keyed(&self) -> Vec<Keyed> {
        self.shared.lock().keyed.clone()
    }

    /// Sets the agent in `pane` to `status`, bumping its
    /// `state_change_seq` and announcing it on every subscription, as
    /// herdr does.
    pub fn set_status(&self, pane: &str, status: Status) {
        let mut state = self.shared.lock();
        set_status(&self.shared, &mut state, pane, status);
    }

    /// Sets what `agent.read` returns for the agent in `pane`.
    pub fn set_screen(&self, pane: &str, screen: &str) {
        let mut state = self.shared.lock();
        if let Some((agent, _)) = state.agents.iter_mut().find(|(a, _)| a.pane == pane) {
            screen.clone_into(&mut agent.screen);
        }
    }

    /// The next call of `method` fails with herdr's error `code`.
    pub fn fail_next(&self, method: &str, code: &'static str) {
        self.shared.lock().faults.push((method.to_owned(), code));
    }

    /// Every line typed into a shell pane so far, with the pane.
    #[must_use]
    pub fn typed(&self) -> Vec<(String, String)> {
        self.shared.lock().typed.clone()
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Reply = Result<Value, &'static str>;

/// One connection carries one request (herdr's model); subscriptions stay
/// open, forwarding events, until the client hangs up.
async fn connection(stream: UnixStream, shared: Arc<Shared>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Ok(Some(line)) = lines.next_line().await else {
        return;
    };
    let Ok(request) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or_default();
    let params = &request["params"];
    let mut events = shared.events.subscribe();
    let result = answer(&shared, method, params);
    let reply = match result {
        Ok(result) => json!({"id": id, "result": result}),
        Err(code) => json!({"id": id, "error": {
            "code": code,
            "message": format!("fake herdr: {method}"),
        }}),
    };
    if write
        .write_all(format!("{reply}\n").as_bytes())
        .await
        .is_err()
    {
        return;
    }
    if method != "events.subscribe" {
        return;
    }
    // herdr ends a subscription when the client closes its side.
    loop {
        tokio::select! {
            line = lines.next_line() => if !matches!(line, Ok(Some(_))) { return },
            event = events.recv() => match event {
                Ok(event) => {
                    if write.write_all(format!("{event}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Moves the agent in `pane` to `status`: a real change bumps its seq and
/// is announced, as herdr does; the same status again does nothing.
fn set_status(shared: &Shared, state: &mut State, pane: &str, status: Status) {
    let Some((agent, seq)) = state.agents.iter_mut().find(|(a, _)| a.pane == pane) else {
        return;
    };
    if agent.status == status {
        return;
    }
    agent.status = status;
    *seq += 1;
    let _ = shared.events.send(json!({
        "event": "pane.agent_status_changed",
        "data": {"pane_id": pane, "workspace_id": agent.workspace(),
                 "agent_status": status.wire(), "agent": agent.kind.wire()},
    }));
}

fn answer(shared: &Arc<Shared>, method: &str, params: &Value) -> Reply {
    let target = params["target"].as_str().unwrap_or_default();
    let mut state = shared.lock();
    if let Some(at) = state.faults.iter().position(|(m, _)| m == method) {
        return Err(state.faults.remove(at).1);
    }
    let find = |state: &State| state.agents.iter().find(|(a, _)| a.pane == target).cloned();
    match method {
        "agent.list" => Ok(json!({
            "type": "agent_list",
            "agents": state.agents.iter().map(|(a, seq)| info(a, *seq)).collect::<Vec<_>>(),
        })),
        "agent.get" | "agent.wait" => find(&state)
            .map(|(agent, seq)| json!({"type": "agent_info", "agent": info(&agent, seq)}))
            .ok_or("agent_not_found"),
        "agent.send_keys" => send_keys(&mut state, target, params),
        "agent.read" => find(&state)
            .map(|(agent, _)| {
                json!({"type": "pane_read", "read": {
                    "pane_id": agent.pane,
                    "source": params["source"],
                    "text": agent.screen.replace(
                        TICK,
                        &shared.reads.fetch_add(1, Ordering::Relaxed).to_string(),
                    ),
                    "revision": 1,
                    "truncated": false,
                }})
            })
            .ok_or("agent_not_found"),
        "agent.prompt" => prompt(shared, &mut state, target, params),
        "workspace.list" => Ok(json!({
            "type": "workspace_list",
            "workspaces": state.workspaces.iter().map(|w| workspace(w, 1)).collect::<Vec<_>>(),
        })),
        "workspace.create" => {
            let ws = Made {
                id: format!("w{}", fresh(&mut state)),
                label: params["label"].as_str().unwrap_or_default().to_owned(),
            };
            state.workspaces.push(ws.clone());
            let (tab, pane) = new_tab(&mut state, &ws.id, &ws.label, params);
            Ok(json!({
                "type": "workspace_created",
                "workspace": workspace(&ws, 1),
                "tab": tab,
                "root_pane": pane,
            }))
        }
        "tab.create" => {
            let ws = params["workspace_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if !state.workspaces.iter().any(|w| w.id == ws) {
                return Err("workspace_not_found");
            }
            let label = params["label"].as_str().unwrap_or_default().to_owned();
            let (tab, pane) = new_tab(&mut state, &ws, &label, params);
            Ok(json!({"type": "tab_created", "tab": tab, "root_pane": pane}))
        }
        "tab.close" => Ok(json!({"type": "ok"})),
        "pane.send_text" => type_line(shared, &mut state, params),
        "pane.get" => state
            .panes
            .iter()
            .find(|p| p["pane_id"] == params["pane_id"])
            .map(|pane| json!({"type": "pane_info", "pane": pane}))
            .ok_or("pane_not_found"),
        "pane.read" => {
            let pane = params["pane_id"].as_str().unwrap_or_default();
            let text = state
                .output
                .iter()
                .find(|(p, _)| p == pane)
                // A fresh shell at its prompt.
                .map_or_else(|| "$ ".to_owned(), |(_, text)| text.clone());
            Ok(json!({"type": "pane_read", "read": {
                "pane_id": pane, "source": params["source"], "text": text,
                "revision": 1, "truncated": false,
            }}))
        }
        "agent.start" => start(shared, &mut state, params),
        "events.subscribe" => Ok(json!({"type": "subscription_started"})),
        _ => Err("unsupported"),
    }
}

/// Records an `agent.send_keys` for an existing agent.
fn send_keys(state: &mut State, target: &str, params: &Value) -> Reply {
    if !state.agents.iter().any(|(a, _)| a.pane == target) {
        return Err("agent_not_found");
    }
    let keys = params["keys"]
        .as_array()
        .map(|keys| {
            keys.iter()
                .filter_map(|k| k.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    state.keyed.push(Keyed {
        pane: target.to_owned(),
        keys,
    });
    Ok(json!({"type": "ok"}))
}

fn fresh(state: &mut State) -> u32 {
    state.next += 1;
    state.next
}

fn workspace(ws: &Made, tabs: u32) -> Value {
    json!({
        "workspace_id": ws.id, "label": ws.label, "number": 1, "focused": false,
        "pane_count": tabs, "tab_count": tabs, "active_tab_id": format!("{}:t1", ws.id),
        "agent_status": "unknown",
    })
}

/// A tab with one shell pane in `params["cwd"]`: its JSON and its pane's.
fn new_tab(state: &mut State, ws: &str, label: &str, params: &Value) -> (Value, Value) {
    let n = fresh(state);
    let tab = format!("{ws}:t{n}");
    let pane = format!("{ws}:p{n}");
    let cwd = PathBuf::from(params["cwd"].as_str().unwrap_or("/"));
    state.shells.push((pane.clone(), cwd.clone()));
    let root = json!({
        "pane_id": pane, "terminal_id": format!("term_{n}"), "tab_id": tab,
        "workspace_id": ws, "agent": null, "agent_status": "unknown", "cwd": cwd,
        "focused": false, "revision": 0,
    });
    state.panes.push(root.clone());
    (
        json!({
            "tab_id": tab, "workspace_id": ws, "label": label, "number": 1,
            "focused": false, "pane_count": 1, "agent_status": "unknown",
        }),
        root,
    )
}

fn start(shared: &Shared, state: &mut State, params: &Value) -> Reply {
    let pane = params["pane_id"].as_str().unwrap_or_default().to_owned();
    let Some(at) = state.shells.iter().position(|(p, _)| *p == pane) else {
        return Err("agent_pane_not_found");
    };
    let (_, cwd) = state.shells.remove(at);
    let n = fresh(state);
    let name = params["name"].as_str().unwrap_or_default().to_owned();
    let agent = FakeAgent {
        pane: pane.clone(),
        title: name.clone(),
        name: Some(name),
        kind: Kind::Claude,
        status: Status::Idle,
        session: Some(format!("0e2e0000-0000-4000-8000-0000000{n:05}")),
        cwd: cwd.clone(),
        screen: "╭─ Claude Code ─╮\n> ".to_owned(),
    };
    state.started.push(Started {
        pane: pane.clone(),
        kind: params["kind"].as_str().unwrap_or_default().to_owned(),
        args: params["args"]
            .as_array()
            .map(|args| {
                args.iter()
                    .filter_map(|a| a.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        cwd,
    });
    let info = info(&agent, 1);
    state.agents.push((agent, 1));
    let _ = shared.events.send(json!({
        "event": "pane_agent_detected",
        "data": {"type": "pane_agent_detected", "pane_id": pane,
                 "workspace_id": info["workspace_id"], "agent": "claude"},
    }));
    Ok(json!({"type": "agent_started", "agent": info}))
}

/// Plays a fresh pane's shell: runs the typed line with `sh -c` in the
/// pane's folder, in the background, keeping what it prints.
fn type_line(shared: &Arc<Shared>, state: &mut State, params: &Value) -> Reply {
    let pane = params["pane_id"].as_str().unwrap_or_default().to_owned();
    let Some(cwd) = state
        .shells
        .iter()
        .find(|(p, _)| *p == pane)
        .map(|(_, cwd)| cwd.clone())
    else {
        return Err("pane_not_found");
    };
    let line = params["text"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    state.typed.push((pane.clone(), line.clone()));
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        let ran = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&line)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        let text = match ran {
            Ok(out) => {
                String::from_utf8_lossy(&out.stdout).into_owned()
                    + &String::from_utf8_lossy(&out.stderr)
            }
            Err(error) => format!("sh: {error}"),
        };
        shared.lock().output.push((pane, text));
    });
    Ok(json!({"type": "ok"}))
}

fn prompt(shared: &Shared, state: &mut State, target: &str, params: &Value) -> Reply {
    let Some((agent, _)) = state.agents.iter().find(|(a, _)| a.pane == target) else {
        return Err("agent_not_found");
    };
    if agent.status == Status::Blocked {
        return Err("agent_blocked");
    }
    state.prompted.push(Prompted {
        pane: target.to_owned(),
        text: params["text"].as_str().unwrap_or_default().to_owned(),
    });
    set_status(shared, state, target, Status::Working);
    let (agent, seq) = state
        .agents
        .iter()
        .find(|(a, _)| a.pane == target)
        .ok_or("agent_not_found")?;
    Ok(json!({"type": "agent_prompted", "agent": info(agent, *seq)}))
}
