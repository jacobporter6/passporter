//! `FakeHerdr`: an in-memory [`Herdr`] with scripted state transitions.
//!
//! It models what Passporter relies on, with herdr 0.9.1's semantics and
//! error codes: workspaces, tabs and panes with stable IDs; agents with a
//! status and `state_change_seq`; screen text; filtered event streams; and
//! herdr's refusals (prompting a blocked agent, invalid keys, taken names,
//! occupied panes).
//!
//! Reality it can model, because tests passing against a tidier fake
//! shipped bugs: agents with no session id (the default; swarm agents run
//! with hooks off and restarted ones often lose theirs), screens that tick
//! between reads ([`TICK`]: a spinner's timer, a status-line clock),
//! terminal titles without an agent title ([`FakeHerdr::set_title`]), agents
//! announced by `pane_agent_detected` ([`FakeHerdr::detect_agent`]), and
//! slow or failing calls ([`Fault`]).
//!
//! Like herdr, `prompt` returns once submitted and changes no state: a test
//! scripts what the agent does next with [`FakeHerdr::set_status`].
//! [`FakeHerdr::fail_next`] injects faults either before the effect (a
//! definite rejection) or after it (a timeout or dropped connection after
//! the prompt landed), the case callers must handle via
//! [`Error::may_have_taken_effect`](crate::Error::may_have_taken_effect).

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::broadcast;

use crate::client::{EventStream, Herdr, StartAgent};
use crate::error::{Error, ErrorCode, Result};
use crate::events::{Event, Subscription};
use crate::ids::{AgentName, PaneId, TabId, Target, TerminalId, WorkspaceId};
use crate::model::{
    Agent, AgentSession, AgentStatus, CreatedTab, CreatedWorkspace, Pane, ReadSource, Screen,
    ServerInfo, SplitDirection, Tab, Workspace,
};

/// Something a test can assert was sent to herdr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Prompt {
        pane: PaneId,
        text: String,
    },
    Keys {
        pane: PaneId,
        keys: Vec<String>,
    },
    Run {
        pane: PaneId,
        command: String,
    },
    Start {
        pane: PaneId,
        name: AgentName,
        kind: String,
        args: Vec<String>,
    },
}

/// A herdr socket method, for [`FakeHerdr::fail_next`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Ping,
    AgentList,
    AgentGet,
    AgentRead,
    AgentPrompt,
    AgentSendKeys,
    AgentWait,
    AgentStart,
    WorkspaceList,
    WorkspaceCreate,
    TabCreate,
    PaneSplit,
    PaneGet,
    PaneRead,
    PaneSendText,
    PaneSendKeys,
    PaneClose,
    TabClose,
    WorkspaceClose,
    EventsSubscribe,
}

impl Method {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::AgentList => "agent.list",
            Self::AgentGet => "agent.get",
            Self::AgentRead => "agent.read",
            Self::AgentPrompt => "agent.prompt",
            Self::AgentSendKeys => "agent.send_keys",
            Self::AgentWait => "agent.wait",
            Self::AgentStart => "agent.start",
            Self::WorkspaceList => "workspace.list",
            Self::WorkspaceCreate => "workspace.create",
            Self::TabCreate => "tab.create",
            Self::PaneSplit => "pane.split",
            Self::PaneGet => "pane.get",
            Self::PaneRead => "pane.read",
            Self::PaneSendText => "pane.send_text",
            Self::PaneSendKeys => "pane.send_keys",
            Self::PaneClose => "pane.close",
            Self::TabClose => "tab.close",
            Self::WorkspaceClose => "workspace.close",
            Self::EventsSubscribe => "events.subscribe",
        }
    }
}

/// A fault to inject into the next call of a [`Method`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// herdr rejects the request; nothing happens.
    Reject(ErrorCode),
    /// The effect happens, then herdr reports this error (e.g. `timeout`,
    /// `agent_prompt_stalled`): the ambiguous case.
    AfterEffect(ErrorCode),
    /// The effect happens, then the connection drops before the reply.
    ClosedAfterEffect,
    /// herdr answers normally, but only after this long: a busy server, or
    /// one slow enough for the caller's own timeout to fire first.
    Delay(Duration),
}

/// In a screen given to [`FakeHerdr::set_screen`], each `{tick}` reads as
/// the number of screen reads so far: the part of a real screen that moves
/// while nothing else changes (a spinner's elapsed time, a status-line
/// clock), so no two reads are the same text.
pub const TICK: &str = "{tick}";

#[derive(Debug)]
pub struct FakeHerdr {
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
}

#[derive(Debug, Default)]
struct State {
    next_id: u32,
    workspaces: BTreeMap<WorkspaceId, String>,
    tabs: BTreeMap<TabId, (WorkspaceId, String)>,
    panes: BTreeMap<PaneId, FakePane>,
    calls: Vec<Call>,
    faults: VecDeque<(Method, Fault)>,
    /// `agent.start` attempts each new pane refuses as `agent_pane_busy`.
    shell_startup: u32,
    /// The status `agent.start` leaves a new agent in (`None`: idle).
    start_status: Option<AgentStatus>,
    /// Screen reads so far, for [`TICK`].
    reads: u64,
}

#[derive(Debug, Clone)]
struct FakePane {
    pane_id: PaneId,
    terminal_id: TerminalId,
    tab_id: TabId,
    workspace_id: WorkspaceId,
    cwd: Option<PathBuf>,
    screen: String,
    revision: u64,
    agent: Option<FakeAgent>,
    /// `agent.start` attempts left before the shell is at its prompt.
    shell_busy: u32,
}

#[derive(Debug, Clone)]
struct FakeAgent {
    name: Option<AgentName>,
    kind: String,
    status: AgentStatus,
    seq: u64,
    session: Option<AgentSession>,
    /// What herdr reports as `terminal_title_stripped`.
    title: Option<String>,
}

impl Default for FakeHerdr {
    fn default() -> Self {
        Self::new()
    }
}

fn server_err(method: Method, code: ErrorCode, message: impl Into<String>) -> Error {
    Error::Server {
        method: method.as_str(),
        code,
        message: message.into(),
    }
}

/// herdr's logical keys: named keys, single printable characters, `ctrl+<letter>`.
fn valid_key(key: &str) -> bool {
    const NAMED: &[&str] = &[
        "enter",
        "esc",
        "escape",
        "tab",
        "backspace",
        "space",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "pageup",
        "pagedown",
        "delete",
    ];
    let mut chars = key.chars();
    let single_printable =
        matches!((chars.next(), chars.next()), (Some(c), None) if c.is_ascii_graphic());
    let ctrl_letter = key
        .strip_prefix("ctrl+")
        .is_some_and(|rest| rest.len() == 1 && rest.chars().all(|c| c.is_ascii_lowercase()));
    NAMED.contains(&key) || single_printable || ctrl_letter
}

impl FakeHerdr {
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            state: Mutex::new(State::default()),
            events,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, events: impl IntoIterator<Item = Event>) {
        for event in events {
            let _ = self.events.send(event);
        }
    }

    /// Convenience: a new workspace whose root pane hosts a named agent.
    ///
    /// # Panics
    /// If `name` isn't a valid agent name (a test bug).
    pub fn add_agent(&self, name: &str, kind: &str, status: AgentStatus) -> PaneId {
        let name = AgentName::new(name).unwrap_or_else(|e| panic!("FakeHerdr::add_agent: {e}"));
        let mut state = self.state();
        let (_, _, pane) = state.new_workspace(name.as_str(), None);
        state.pane_mut(&pane).agent = Some(FakeAgent {
            name: Some(name),
            kind: kind.to_owned(),
            status,
            seq: 1,
            session: None,
            title: None,
        });
        pane
    }

    /// An unnamed agent appears in an existing shell pane (a user started it
    /// by hand), emitting `pane_agent_detected` like herdr.
    ///
    /// # Panics
    /// If `pane` doesn't exist (a test bug).
    pub fn detect_agent(&self, pane: &PaneId, kind: &str, status: AgentStatus) {
        let event = {
            let mut state = self.state();
            let p = state.pane_mut(pane);
            p.agent = Some(FakeAgent {
                name: None,
                kind: kind.to_owned(),
                status,
                seq: 1,
                session: None,
                title: None,
            });
            Event::PaneAgentDetected {
                pane_id: p.pane_id.clone(),
                workspace_id: p.workspace_id.clone(),
                agent: Some(kind.to_owned()),
                released: false,
                final_status: None,
            }
        };
        self.emit([event]);
    }

    /// The agent in `pane` exits or is released, leaving a shell: herdr reports
    /// it as `pane_agent_detected` with `released: true`.
    ///
    /// # Panics
    /// If `pane` doesn't exist or hosts no agent (a test bug).
    pub fn release_agent(&self, pane: &PaneId) {
        let event = {
            let mut state = self.state();
            let p = state.pane_mut(pane);
            let agent = p
                .agent
                .take()
                .unwrap_or_else(|| panic!("no agent in {pane}"));
            Event::PaneAgentDetected {
                pane_id: p.pane_id.clone(),
                workspace_id: p.workspace_id.clone(),
                agent: Some(agent.kind),
                released: true,
                final_status: Some(agent.status),
            }
        };
        self.emit([event]);
    }

    /// Moves an agent to `status`. Like herdr, a real change bumps
    /// `state_change_seq` and emits `pane_agent_status_changed`; setting the
    /// current status again does nothing.
    ///
    /// # Panics
    /// If `pane` doesn't exist or hosts no agent (a test bug).
    pub fn set_status(&self, pane: &PaneId, status: AgentStatus) {
        let event = {
            let mut state = self.state();
            let p = state.pane_mut(pane);
            let agent = p
                .agent
                .as_mut()
                .unwrap_or_else(|| panic!("no agent in {pane}"));
            if agent.status == status {
                return;
            }
            agent.status = status;
            agent.seq += 1;
            p.revision += 1;
            Event::PaneAgentStatusChanged {
                pane_id: p.pane_id.clone(),
                workspace_id: p.workspace_id.clone(),
                agent_status: status,
                agent: Some(agent.kind.clone()),
                title: None,
            }
        };
        self.emit([event]);
    }

    /// Sets the text `read_agent`/`read_pane` return; each [`TICK`] in it
    /// changes on every read.
    ///
    /// # Panics
    /// If `pane` doesn't exist (a test bug).
    pub fn set_screen(&self, pane: &PaneId, text: &str) {
        let mut state = self.state();
        let p = state.pane_mut(pane);
        text.clone_into(&mut p.screen);
        p.revision += 1;
    }

    /// Gives `pane` a new terminal under the same id, as if the pane had
    /// closed and herdr (restarted) handed its id to an unrelated one.
    ///
    /// # Panics
    /// If `pane` doesn't exist (a test bug).
    pub fn renew_terminal(&self, pane: &PaneId) {
        let mut state = self.state();
        let n = state.fresh();
        let p = state.pane_mut(pane);
        p.terminal_id = TerminalId::new(format!("term_{n}"));
        p.revision += 1;
    }

    /// Sets the terminal title herdr reports for the agent
    /// (`terminal_title_stripped`; herdr 0.9.1 reports no agent `title`).
    ///
    /// # Panics
    /// If `pane` doesn't exist or hosts no agent (a test bug).
    pub fn set_title(&self, pane: &PaneId, title: &str) {
        let mut state = self.state();
        let agent = state
            .pane_mut(pane)
            .agent
            .as_mut()
            .unwrap_or_else(|| panic!("no agent in {pane}"));
        agent.title = Some(title.to_owned());
    }

    /// Sets the agent's session handle (e.g. a Claude session ID).
    ///
    /// # Panics
    /// If `pane` doesn't exist or hosts no agent (a test bug).
    pub fn set_session(&self, pane: &PaneId, session: AgentSession) {
        let mut state = self.state();
        let agent = state
            .pane_mut(pane)
            .agent
            .as_mut()
            .unwrap_or_else(|| panic!("no agent in {pane}"));
        agent.session = Some(session);
    }

    /// Panes created from now on take `attempts` refused `agent.start`
    /// calls (`agent_pane_busy`, nothing started) before their shell is at
    /// its prompt, like a real pane running its rc files.
    pub fn set_shell_startup(&self, attempts: u32) {
        self.state().shell_startup = attempts;
    }

    /// Agents started from now on begin in `status` instead of idle: e.g.
    /// `Unknown` or `Working` while Claude boots, until the test moves them
    /// on with [`set_status`](Self::set_status).
    pub fn set_start_status(&self, status: AgentStatus) {
        self.state().start_status = Some(status);
    }

    /// Injects `fault` into the next call of `method`.
    pub fn fail_next(&self, method: Method, fault: Fault) {
        self.state().faults.push_back((method, fault));
    }

    /// Everything sent so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// The next pending fault for `method`, removed only if `take` accepts it.
    fn take_fault(&self, method: Method, take: impl Fn(&Fault) -> bool) -> Option<Fault> {
        let mut state = self.state();
        let i = state.faults.iter().position(|(m, _)| *m == method)?;
        if !take(&state.faults[i].1) {
            return None;
        }
        state.faults.remove(i).map(|(_, fault)| fault)
    }

    /// A pending `Reject` fails the call before anything happens; a
    /// pending `Delay` holds it up first.
    async fn before(&self, method: Method) -> Result<()> {
        let fault = self.take_fault(method, |f| matches!(f, Fault::Reject(_) | Fault::Delay(_)));
        match fault {
            Some(Fault::Reject(code)) => Err(server_err(method, code, "injected by FakeHerdr")),
            Some(Fault::Delay(delay)) => {
                tokio::time::sleep(delay).await;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A pending after-effect fault fails the call once its effect has happened.
    /// Calls that fail validation never reach this, so they don't consume it.
    fn after(&self, method: Method) -> Result<()> {
        match self.take_fault(method, |f| !matches!(f, Fault::Reject(_) | Fault::Delay(_))) {
            Some(Fault::AfterEffect(code)) => {
                Err(server_err(method, code, "injected by FakeHerdr"))
            }
            Some(Fault::ClosedAfterEffect) => Err(Error::Closed {
                method: method.as_str(),
            }),
            Some(Fault::Reject(_) | Fault::Delay(_)) | None => Ok(()),
        }
    }

    /// Waits for `pane`'s agent to reach a status accepted by `matches`.
    async fn wait_on(
        &self,
        method: Method,
        pane: &PaneId,
        matches: impl Fn(AgentStatus) -> bool + Send,
        timeout: Option<Duration>,
    ) -> Result<Agent> {
        // Subscribe before checking, so a transition in between isn't missed.
        let mut rx = self.events.subscribe();
        let check = || -> Result<Option<Agent>> {
            let agent = self.state().agent_in(method, pane)?;
            Ok(matches(agent.agent_status).then_some(agent))
        };
        let watch = async {
            loop {
                if let Some(agent) = check()? {
                    return Ok(agent);
                }
                match rx.recv().await {
                    // Re-check on any event; a lagged receiver re-checks too.
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(Error::Closed {
                            method: method.as_str(),
                        });
                    }
                }
            }
        };
        match timeout {
            Some(t) => tokio::time::timeout(t, watch)
                .await
                .map_err(|_| server_err(method, ErrorCode::Timeout, "wait timed out"))?,
            None => watch.await,
        }
    }
}

/// How long herdr waits for activity after a prompt-with-wait before `agent_prompt_stalled`.
const PROMPT_ACTIVITY_WINDOW: Duration = Duration::from_secs(5);

fn settled(status: AgentStatus) -> bool {
    status.is_ready() || status == AgentStatus::Blocked
}

fn until_matcher(until: &[AgentStatus]) -> impl Fn(AgentStatus) -> bool + Send + use<> {
    let until = until.to_vec();
    move |status| {
        if until.is_empty() {
            settled(status)
        } else {
            until.contains(&status)
        }
    }
}

impl State {
    fn fresh(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn new_workspace(&mut self, label: &str, cwd: Option<&Path>) -> (WorkspaceId, TabId, PaneId) {
        let ws = WorkspaceId::new(format!("w{}", self.fresh()));
        self.workspaces.insert(ws.clone(), label.to_owned());
        let (tab, pane) = self.new_tab(&ws, label, cwd);
        (ws, tab, pane)
    }

    fn new_tab(&mut self, ws: &WorkspaceId, label: &str, cwd: Option<&Path>) -> (TabId, PaneId) {
        let tab = TabId::new(format!("{ws}:t{}", self.fresh()));
        self.tabs
            .insert(tab.clone(), (ws.clone(), label.to_owned()));
        let pane = self.new_pane(ws, &tab, cwd);
        (tab, pane)
    }

    fn new_pane(&mut self, ws: &WorkspaceId, tab: &TabId, cwd: Option<&Path>) -> PaneId {
        let n = self.fresh();
        let pane = PaneId::new(format!("{ws}:p{n}"));
        self.panes.insert(
            pane.clone(),
            FakePane {
                pane_id: pane.clone(),
                terminal_id: TerminalId::new(format!("term_{n}")),
                tab_id: tab.clone(),
                workspace_id: ws.clone(),
                cwd: cwd.map(Path::to_path_buf),
                screen: String::new(),
                revision: 0,
                agent: None,
                shell_busy: self.shell_startup,
            },
        );
        pane
    }

    fn pane_mut(&mut self, pane: &PaneId) -> &mut FakePane {
        self.panes
            .get_mut(pane)
            .unwrap_or_else(|| panic!("FakeHerdr: no pane {pane}"))
    }

    /// The pane hosting `target`'s agent, or herdr's `agent_not_found`.
    fn resolve(&self, method: Method, target: &Target) -> Result<PaneId> {
        let found = match target {
            Target::Pane(pane) => self.panes.get(pane).filter(|p| p.agent.is_some()),
            Target::Name(name) => self
                .panes
                .values()
                .find(|p| p.agent.as_ref().and_then(|a| a.name.as_ref()) == Some(name)),
        };
        found.map(|p| p.pane_id.clone()).ok_or_else(|| {
            server_err(
                method,
                ErrorCode::AgentNotFound,
                format!("agent target {target} not found"),
            )
        })
    }

    fn agent_in(&self, method: Method, pane: &PaneId) -> Result<Agent> {
        let not_found = || {
            server_err(
                method,
                ErrorCode::AgentNotFound,
                format!("agent target {pane} not found"),
            )
        };
        let p = self.panes.get(pane).ok_or_else(not_found)?;
        let a = p.agent.as_ref().ok_or_else(not_found)?;
        Ok(Agent {
            terminal_id: p.terminal_id.clone(),
            pane_id: p.pane_id.clone(),
            tab_id: p.tab_id.clone(),
            workspace_id: p.workspace_id.clone(),
            name: a.name.clone(),
            kind: Some(a.kind.clone()),
            agent_status: a.status,
            agent_session: a.session.clone(),
            state_change_seq: a.seq,
            revision: p.revision,
            title: None,
            terminal_title_stripped: a.title.clone(),
            cwd: p.cwd.clone(),
            foreground_cwd: p.cwd.clone(),
            focused: false,
            interactive_ready: true,
        })
    }

    fn pane(&self, pane: &PaneId) -> Pane {
        let p = &self.panes[pane];
        Pane {
            pane_id: p.pane_id.clone(),
            terminal_id: p.terminal_id.clone(),
            tab_id: p.tab_id.clone(),
            workspace_id: p.workspace_id.clone(),
            agent_kind: p.agent.as_ref().map(|a| a.kind.clone()),
            agent_status: p.agent.as_ref().map_or(AgentStatus::Unknown, |a| a.status),
            cwd: p.cwd.clone(),
            focused: false,
            revision: p.revision,
        }
    }

    fn tab(&self, tab: &TabId) -> Tab {
        let (ws, label) = &self.tabs[tab];
        Tab {
            tab_id: tab.clone(),
            workspace_id: ws.clone(),
            label: label.clone(),
            number: 1,
            focused: false,
            pane_count: count(self.panes.values().filter(|p| &p.tab_id == tab)),
            agent_status: AgentStatus::Unknown,
        }
    }

    fn workspace(&self, ws: &WorkspaceId, active_tab: &TabId) -> Workspace {
        Workspace {
            workspace_id: ws.clone(),
            label: self.workspaces[ws].clone(),
            number: 1,
            focused: false,
            pane_count: count(self.panes.values().filter(|p| &p.workspace_id == ws)),
            tab_count: count(self.tabs.values().filter(|(w, _)| w == ws)),
            active_tab_id: active_tab.clone(),
            agent_status: AgentStatus::Unknown,
        }
    }

    fn screen(&mut self, pane: &PaneId, source: ReadSource, lines: Option<u32>) -> Screen {
        self.reads += 1;
        let p = &self.panes[pane];
        let text = p.screen.replace(TICK, &self.reads.to_string());
        let all: Vec<&str> = text.lines().collect();
        let keep = lines.map_or(all.len(), |n| (n as usize).min(all.len()));
        Screen {
            pane_id: pane.clone(),
            source,
            text: all[all.len() - keep..].join("\n"),
            revision: p.revision,
            truncated: keep < all.len(),
        }
    }

    /// Removes panes matching `which`, returning `pane_closed` events for them.
    fn remove_panes(&mut self, which: impl Fn(&FakePane) -> bool) -> Vec<Event> {
        let closed: Vec<PaneId> = self
            .panes
            .values()
            .filter(|p| which(p))
            .map(|p| p.pane_id.clone())
            .collect();
        closed
            .into_iter()
            .filter_map(|id| self.panes.remove(&id))
            .map(|p| Event::PaneClosed {
                pane_id: p.pane_id,
                workspace_id: p.workspace_id,
            })
            .collect()
    }
}

fn count<T>(iter: impl Iterator<Item = T>) -> u32 {
    u32::try_from(iter.count()).unwrap_or(u32::MAX)
}

// The trait is async for the socket client; in memory there is nothing to await.
#[allow(clippy::unused_async_trait_impl)]
impl Herdr for FakeHerdr {
    type Events = FakeEvents;

    async fn ping(&self) -> Result<ServerInfo> {
        self.before(Method::Ping).await?;
        self.after(Method::Ping)?;
        Ok(ServerInfo {
            version: "fake".to_owned(),
            protocol: 22,
        })
    }

    async fn agents(&self) -> Result<Vec<Agent>> {
        const M: Method = Method::AgentList;
        self.before(M).await?;
        let agents = {
            let state = self.state();
            let hosts = state.panes.values().filter(|p| p.agent.is_some());
            hosts
                .map(|p| state.agent_in(M, &p.pane_id))
                .collect::<Result<_>>()?
        };
        self.after(M)?;
        Ok(agents)
    }

    async fn agent(&self, target: &Target) -> Result<Agent> {
        const M: Method = Method::AgentGet;
        self.before(M).await?;
        let agent = {
            let state = self.state();
            state.agent_in(M, &state.resolve(M, target)?)?
        };
        self.after(M)?;
        Ok(agent)
    }

    async fn read_agent(
        &self,
        target: &Target,
        source: ReadSource,
        lines: Option<u32>,
    ) -> Result<Screen> {
        const M: Method = Method::AgentRead;
        self.before(M).await?;
        let screen = {
            let mut state = self.state();
            let pane = state.resolve(M, target)?;
            state.screen(&pane, source, lines)
        };
        self.after(M)?;
        Ok(screen)
    }

    async fn prompt(&self, target: &Target, text: &str) -> Result<Agent> {
        const M: Method = Method::AgentPrompt;
        self.before(M).await?;
        let agent = {
            let mut state = self.state();
            let pane = state.resolve(M, target)?;
            let agent = state.agent_in(M, &pane)?;
            if agent.agent_status == AgentStatus::Blocked {
                return Err(server_err(
                    M,
                    ErrorCode::AgentBlocked,
                    "agent is waiting at a dialog",
                ));
            }
            if text.trim().is_empty() {
                return Err(server_err(M, ErrorCode::EmptyAgentPrompt, "empty prompt"));
            }
            state.calls.push(Call::Prompt {
                pane,
                text: text.to_owned(),
            });
            agent
        };
        self.after(M)?;
        Ok(agent)
    }

    async fn prompt_and_wait(
        &self,
        target: &Target,
        text: &str,
        until: &[AgentStatus],
        timeout: Option<Duration>,
    ) -> Result<Agent> {
        const M: Method = Method::AgentPrompt;
        let submitted = self.prompt(target, text).await?;
        let pane = submitted.pane_id;
        // herdr requires observed `working`/`blocked` activity within five seconds of
        // submitting from a non-working state, else `agent_prompt_stalled`.
        if submitted.agent_status != AgentStatus::Working {
            let active = |s: AgentStatus| matches!(s, AgentStatus::Working | AgentStatus::Blocked);
            self.wait_on(M, &pane, active, Some(PROMPT_ACTIVITY_WINDOW))
                .await
                .map_err(|e| match e.code() {
                    Some(ErrorCode::Timeout) => {
                        server_err(M, ErrorCode::AgentPromptStalled, "no activity after prompt")
                    }
                    _ => e,
                })?;
        }
        self.wait_on(M, &pane, until_matcher(until), timeout).await
    }

    async fn send_keys(&self, target: &Target, keys: &[&str]) -> Result<()> {
        const M: Method = Method::AgentSendKeys;
        self.before(M).await?;
        {
            let mut state = self.state();
            let pane = state.resolve(M, target)?;
            if let Some(bad) = keys.iter().find(|k| !valid_key(k)) {
                return Err(server_err(
                    M,
                    ErrorCode::InvalidKey,
                    format!("invalid key {bad:?}"),
                ));
            }
            let keys = keys.iter().map(|k| (*k).to_owned()).collect();
            state.calls.push(Call::Keys { pane, keys });
        }
        self.after(M)
    }

    async fn wait(
        &self,
        target: &Target,
        until: &[AgentStatus],
        timeout: Option<Duration>,
    ) -> Result<Agent> {
        const M: Method = Method::AgentWait;
        self.before(M).await?;
        let pane = self.state().resolve(M, target)?;
        let agent = self
            .wait_on(M, &pane, until_matcher(until), timeout)
            .await?;
        self.after(M)?;
        Ok(agent)
    }

    async fn start_agent(&self, spec: &StartAgent) -> Result<Agent> {
        const M: Method = Method::AgentStart;
        self.before(M).await?;
        let agent = {
            let mut state = self.state();
            let taken = state
                .panes
                .values()
                .any(|p| p.agent.as_ref().and_then(|a| a.name.as_ref()) == Some(&spec.name));
            if taken {
                return Err(server_err(
                    M,
                    ErrorCode::AgentNameTaken,
                    spec.name.to_string(),
                ));
            }
            let status = state.start_status.unwrap_or(AgentStatus::Idle);
            let p = state.panes.get_mut(&spec.pane).ok_or_else(|| {
                server_err(M, ErrorCode::AgentPaneNotFound, spec.pane.to_string())
            })?;
            if p.agent.is_some() {
                let msg = "pane already hosts an agent";
                return Err(server_err(M, ErrorCode::AgentPaneUnavailable, msg));
            }
            if p.shell_busy > 0 {
                p.shell_busy -= 1;
                let msg = "pane shell is not at its prompt yet";
                return Err(server_err(M, ErrorCode::AgentPaneBusy, msg));
            }
            p.agent = Some(FakeAgent {
                name: Some(spec.name.clone()),
                kind: spec.kind.clone(),
                status,
                seq: 1,
                session: None,
                title: None,
            });
            state.calls.push(Call::Start {
                pane: spec.pane.clone(),
                name: spec.name.clone(),
                kind: spec.kind.clone(),
                args: spec.args.clone(),
            });
            state.agent_in(M, &spec.pane)?
        };
        self.emit([Event::PaneAgentDetected {
            pane_id: agent.pane_id.clone(),
            workspace_id: agent.workspace_id.clone(),
            agent: agent.kind.clone(),
            released: false,
            final_status: None,
        }]);
        self.after(M)?;
        Ok(agent)
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>> {
        const M: Method = Method::WorkspaceList;
        self.before(M).await?;
        let workspaces = {
            let state = self.state();
            state
                .workspaces
                .keys()
                .filter_map(|ws| {
                    let (tab, _) = state.tabs.iter().find(|(_, (w, _))| w == ws)?;
                    Some(state.workspace(ws, tab))
                })
                .collect()
        };
        self.after(M)?;
        Ok(workspaces)
    }

    async fn create_workspace(&self, label: &str, cwd: Option<&Path>) -> Result<CreatedWorkspace> {
        const M: Method = Method::WorkspaceCreate;
        self.before(M).await?;
        let created = {
            let mut state = self.state();
            let (ws, tab, pane) = state.new_workspace(label, cwd);
            CreatedWorkspace {
                workspace: state.workspace(&ws, &tab),
                tab: state.tab(&tab),
                root_pane: state.pane(&pane),
            }
        };
        self.emit([Event::PaneCreated {
            pane: created.root_pane.clone(),
        }]);
        self.after(M)?;
        Ok(created)
    }

    async fn create_tab(
        &self,
        workspace: &WorkspaceId,
        label: &str,
        cwd: Option<&Path>,
    ) -> Result<CreatedTab> {
        const M: Method = Method::TabCreate;
        self.before(M).await?;
        let created = {
            let mut state = self.state();
            if !state.workspaces.contains_key(workspace) {
                return Err(server_err(
                    M,
                    ErrorCode::WorkspaceNotFound,
                    workspace.to_string(),
                ));
            }
            let (tab, pane) = state.new_tab(workspace, label, cwd);
            CreatedTab {
                tab: state.tab(&tab),
                root_pane: state.pane(&pane),
            }
        };
        self.emit([Event::PaneCreated {
            pane: created.root_pane.clone(),
        }]);
        self.after(M)?;
        Ok(created)
    }

    async fn split_pane(
        &self,
        pane: &PaneId,
        _direction: SplitDirection,
        cwd: Option<&Path>,
    ) -> Result<Pane> {
        const M: Method = Method::PaneSplit;
        self.before(M).await?;
        let created = {
            let mut state = self.state();
            let (ws, tab) = state
                .panes
                .get(pane)
                .map(|p| (p.workspace_id.clone(), p.tab_id.clone()))
                .ok_or_else(|| server_err(M, ErrorCode::PaneNotFound, pane.to_string()))?;
            let new = state.new_pane(&ws, &tab, cwd);
            state.pane(&new)
        };
        self.emit([Event::PaneCreated {
            pane: created.clone(),
        }]);
        self.after(M)?;
        Ok(created)
    }

    async fn pane(&self, pane: &PaneId) -> Result<Pane> {
        const M: Method = Method::PaneGet;
        self.before(M).await?;
        let found = {
            let state = self.state();
            if !state.panes.contains_key(pane) {
                let msg = format!("pane {pane} not found");
                return Err(server_err(M, ErrorCode::PaneNotFound, msg));
            }
            state.pane(pane)
        };
        self.after(M)?;
        Ok(found)
    }

    async fn read_pane(
        &self,
        pane: &PaneId,
        source: ReadSource,
        lines: Option<u32>,
    ) -> Result<Screen> {
        const M: Method = Method::PaneRead;
        self.before(M).await?;
        let screen = {
            let mut state = self.state();
            if !state.panes.contains_key(pane) {
                let msg = format!("pane {pane} not found");
                return Err(server_err(M, ErrorCode::PaneNotFound, msg));
            }
            state.screen(pane, source, lines)
        };
        self.after(M)?;
        Ok(screen)
    }

    async fn run_in_pane(&self, pane: &PaneId, command: &str) -> Result<()> {
        const M: Method = Method::PaneSendText;
        self.before(M).await?;
        {
            let mut state = self.state();
            if !state.panes.contains_key(pane) {
                return Err(server_err(M, ErrorCode::PaneNotFound, pane.to_string()));
            }
            state.calls.push(Call::Run {
                pane: pane.clone(),
                command: command.to_owned(),
            });
        }
        self.after(M)
    }

    async fn send_pane_keys(&self, pane: &PaneId, keys: &[&str]) -> Result<()> {
        const M: Method = Method::PaneSendKeys;
        self.before(M).await?;
        {
            let mut state = self.state();
            if !state.panes.contains_key(pane) {
                return Err(server_err(M, ErrorCode::PaneNotFound, pane.to_string()));
            }
            if let Some(bad) = keys.iter().find(|k| !valid_key(k)) {
                return Err(server_err(
                    M,
                    ErrorCode::InvalidKey,
                    format!("invalid key {bad:?}"),
                ));
            }
            let keys = keys.iter().map(|k| (*k).to_owned()).collect();
            state.calls.push(Call::Keys {
                pane: pane.clone(),
                keys,
            });
        }
        self.after(M)
    }

    async fn close_pane(&self, pane: &PaneId) -> Result<()> {
        const M: Method = Method::PaneClose;
        self.before(M).await?;
        let events = {
            let mut state = self.state();
            if !state.panes.contains_key(pane) {
                return Err(server_err(M, ErrorCode::PaneNotFound, pane.to_string()));
            }
            state.remove_panes(|p| &p.pane_id == pane)
        };
        self.emit(events);
        self.after(M)
    }

    async fn close_tab(&self, tab: &TabId) -> Result<()> {
        const M: Method = Method::TabClose;
        self.before(M).await?;
        let events = {
            let mut state = self.state();
            let (ws, _) = state
                .tabs
                .remove(tab)
                .ok_or_else(|| server_err(M, ErrorCode::TabNotFound, tab.to_string()))?;
            let mut events = state.remove_panes(|p| &p.tab_id == tab);
            events.push(Event::TabClosed {
                tab_id: tab.clone(),
                workspace_id: ws,
            });
            events
        };
        self.emit(events);
        self.after(M)
    }

    async fn close_workspace(&self, workspace: &WorkspaceId) -> Result<()> {
        const M: Method = Method::WorkspaceClose;
        self.before(M).await?;
        let events = {
            let mut state = self.state();
            if state.workspaces.remove(workspace).is_none() {
                return Err(server_err(
                    M,
                    ErrorCode::WorkspaceNotFound,
                    workspace.to_string(),
                ));
            }
            let mut events = state.remove_panes(|p| &p.workspace_id == workspace);
            let tabs: Vec<TabId> = state
                .tabs
                .iter()
                .filter(|(_, (w, _))| w == workspace)
                .map(|(t, _)| t.clone())
                .collect();
            for tab in tabs {
                state.tabs.remove(&tab);
                events.push(Event::TabClosed {
                    tab_id: tab,
                    workspace_id: workspace.clone(),
                });
            }
            events.push(Event::WorkspaceClosed {
                workspace_id: workspace.clone(),
            });
            events
        };
        self.emit(events);
        self.after(M)
    }

    async fn subscribe(&self, subscriptions: &[Subscription]) -> Result<FakeEvents> {
        const M: Method = Method::EventsSubscribe;
        self.before(M).await?;
        let events = FakeEvents {
            rx: self.events.subscribe(),
            subscriptions: subscriptions.to_vec(),
        };
        self.after(M)?;
        Ok(events)
    }
}

/// Events from [`FakeHerdr::subscribe`], filtered like herdr filters them.
#[derive(Debug)]
pub struct FakeEvents {
    rx: broadcast::Receiver<Event>,
    subscriptions: Vec<Subscription>,
}

impl FakeEvents {
    fn wanted(&self, event: &Event) -> bool {
        self.subscriptions.iter().any(|sub| match (sub, event) {
            (
                Subscription::AgentStatusChanged {
                    pane_id,
                    agent_status,
                },
                Event::PaneAgentStatusChanged {
                    pane_id: p,
                    agent_status: s,
                    ..
                },
            ) => pane_id == p && agent_status.is_none_or(|want| want == *s),
            (Subscription::PaneCreated, Event::PaneCreated { .. })
            | (Subscription::PaneClosed, Event::PaneClosed { .. })
            | (Subscription::PaneExited, Event::PaneExited { .. })
            | (Subscription::AgentDetected, Event::PaneAgentDetected { .. })
            | (Subscription::TabClosed, Event::TabClosed { .. })
            | (Subscription::WorkspaceClosed, Event::WorkspaceClosed { .. }) => true,
            _ => false,
        })
    }
}

impl EventStream for FakeEvents {
    async fn next(&mut self) -> Option<Result<Event>> {
        loop {
            match self.rx.recv().await {
                Ok(event) if self.wanted(&event) => return Some(Ok(event)),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::client::ShellWait;

    fn name(s: &str) -> Target {
        Target::Name(AgentName::new(s).expect("valid"))
    }

    #[tokio::test]
    async fn prompt_records_without_changing_state() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("impl-t11", "claude", AgentStatus::Idle);

        let agent = herdr
            .prompt(&name("impl-t11"), "do the thing")
            .await
            .expect("prompt");

        assert_eq!(
            agent.agent_status,
            AgentStatus::Idle,
            "working arrives later, as with herdr"
        );
        assert_eq!(agent.state_change_seq, 1);
        assert_eq!(
            herdr.calls(),
            vec![Call::Prompt {
                pane,
                text: "do the thing".to_owned()
            }]
        );
    }

    #[tokio::test]
    async fn a_ticking_screen_differs_on_every_read() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("impl-t11", "claude", AgentStatus::Blocked);
        herdr.set_screen(&pane, "✻ Thinking… ({tick}s)\nDo you want to proceed?");
        let target = Target::from(&pane);
        let first = herdr
            .read_agent(&target, ReadSource::Visible, None)
            .await
            .expect("read");
        let second = herdr
            .read_pane(&pane, ReadSource::Visible, None)
            .await
            .expect("read");
        assert_eq!(first.text, "✻ Thinking… (1s)\nDo you want to proceed?");
        assert_eq!(second.text, "✻ Thinking… (2s)\nDo you want to proceed?");
        assert_eq!(
            first.revision, second.revision,
            "herdr's revision need not move"
        );
    }

    #[tokio::test]
    async fn agents_have_no_session_or_title_until_given_one() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("impl-t11", "claude", AgentStatus::Working);
        let agent = herdr.agent(&Target::from(&pane)).await.expect("agent");
        assert_eq!(
            agent.agent_session, None,
            "like a swarm agent with hooks off"
        );
        assert_eq!(agent.terminal_title_stripped, None);
        herdr.set_title(&pane, "Fix the parser");
        let agent = herdr.agent(&Target::from(&pane)).await.expect("agent");
        assert_eq!(
            agent.terminal_title_stripped.as_deref(),
            Some("Fix the parser")
        );
        assert_eq!(agent.title, None, "herdr 0.9.1 reports only the terminal's");
    }

    #[tokio::test(start_paused = true)]
    async fn a_delayed_call_answers_late() {
        let herdr = FakeHerdr::new();
        herdr.add_agent("impl-t11", "claude", AgentStatus::Idle);
        herdr.fail_next(Method::AgentList, Fault::Delay(Duration::from_secs(30)));
        let started = tokio::time::Instant::now();
        let slow = tokio::time::timeout(Duration::from_secs(5), herdr.agents()).await;
        assert!(slow.is_err(), "a caller's own timeout fires first");
        assert!(started.elapsed() >= Duration::from_secs(5));
        assert_eq!(herdr.agents().await.expect("list").len(), 1, "once only");
    }

    #[tokio::test]
    async fn prompting_a_blocked_agent_is_refused_and_sends_nothing() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("impl-t11", "claude", AgentStatus::Blocked);

        let err = herdr
            .prompt(&(&pane).into(), "hello")
            .await
            .expect_err("blocked");

        assert_eq!(err.code(), Some(&ErrorCode::AgentBlocked));
        assert!(!err.may_have_taken_effect());
        assert_eq!(herdr.calls(), Vec::new());
    }

    #[tokio::test]
    async fn unknown_targets_use_herdr_codes() {
        let herdr = FakeHerdr::new();
        let err = herdr.agent(&name("nobody")).await.expect_err("missing");
        assert_eq!(err.code(), Some(&ErrorCode::AgentNotFound));
        let missing = PaneId::new("w9:p9");
        let err = herdr
            .read_pane(&missing, ReadSource::Visible, None)
            .await
            .expect_err("missing");
        assert_eq!(err.code(), Some(&ErrorCode::PaneNotFound));
    }

    #[tokio::test]
    async fn set_status_bumps_seq_only_on_real_changes() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("a", "claude", AgentStatus::Idle);
        herdr.set_status(&pane, AgentStatus::Idle);
        assert_eq!(
            herdr
                .agent(&(&pane).into())
                .await
                .expect("agent")
                .state_change_seq,
            1
        );
        herdr.set_status(&pane, AgentStatus::Working);
        assert_eq!(
            herdr
                .agent(&(&pane).into())
                .await
                .expect("agent")
                .state_change_seq,
            2
        );
    }

    #[tokio::test]
    async fn wait_returns_on_scripted_transition() {
        let herdr = Arc::new(FakeHerdr::new());
        let pane = herdr.add_agent("impl-t11", "claude", AgentStatus::Working);
        let (scripted, p) = (herdr.clone(), pane.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            scripted.set_status(&p, AgentStatus::Blocked);
        });

        let agent = herdr
            .wait(
                &(&pane).into(),
                &[AgentStatus::Blocked],
                Some(Duration::from_secs(5)),
            )
            .await
            .expect("wait");

        assert_eq!(agent.agent_status, AgentStatus::Blocked);
    }

    #[tokio::test]
    async fn wait_with_empty_until_means_settled() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("a", "claude", AgentStatus::Blocked);
        let agent = herdr
            .wait(&(&pane).into(), &[], Some(Duration::from_millis(50)))
            .await;
        assert_eq!(
            agent.expect("blocked is settled").agent_status,
            AgentStatus::Blocked
        );
    }

    #[tokio::test]
    async fn wait_fails_when_the_pane_closes() {
        let herdr = Arc::new(FakeHerdr::new());
        let pane = herdr.add_agent("a", "claude", AgentStatus::Working);
        let (closer, p) = (herdr.clone(), pane.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            closer.close_pane(&p).await.expect("close");
        });

        let err = herdr
            .wait(
                &(&pane).into(),
                &[AgentStatus::Done],
                Some(Duration::from_secs(5)),
            )
            .await
            .expect_err("pane gone");

        assert_eq!(err.code(), Some(&ErrorCode::AgentNotFound));
    }

    #[tokio::test]
    async fn wait_times_out_with_timeout_code() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("impl-t11", "claude", AgentStatus::Working);
        let err = herdr
            .wait(
                &(&pane).into(),
                &[AgentStatus::Done],
                Some(Duration::from_millis(20)),
            )
            .await
            .expect_err("times out");
        assert_eq!(err.code(), Some(&ErrorCode::Timeout));
    }

    #[tokio::test]
    async fn prompt_and_wait_settles_on_scripted_turn() {
        let herdr = Arc::new(FakeHerdr::new());
        let pane = herdr.add_agent("a", "claude", AgentStatus::Idle);
        let (agent_side, p) = (herdr.clone(), pane.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            agent_side.set_status(&p, AgentStatus::Working);
            tokio::time::sleep(Duration::from_millis(10)).await;
            agent_side.set_status(&p, AgentStatus::Done);
        });

        let until = [AgentStatus::Done];
        let agent = herdr
            .prompt_and_wait(&(&pane).into(), "go", &until, Some(Duration::from_secs(5)))
            .await
            .expect("settles");

        assert_eq!(agent.agent_status, AgentStatus::Done);
        assert_eq!(agent.state_change_seq, 3);
    }

    #[tokio::test]
    async fn after_effect_faults_model_ambiguous_delivery() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("a", "claude", AgentStatus::Idle);
        herdr.fail_next(Method::AgentPrompt, Fault::AfterEffect(ErrorCode::Timeout));
        herdr.fail_next(Method::AgentPrompt, Fault::ClosedAfterEffect);
        herdr.fail_next(Method::AgentPrompt, Fault::Reject(ErrorCode::InternalError));

        let timeout = herdr
            .prompt(&(&pane).into(), "one")
            .await
            .expect_err("timeout");
        let closed = herdr
            .prompt(&(&pane).into(), "two")
            .await
            .expect_err("closed");
        let rejected = herdr
            .prompt(&(&pane).into(), "three")
            .await
            .expect_err("rejected");
        herdr
            .prompt(&(&pane).into(), "four")
            .await
            .expect("no faults left");

        assert!(timeout.may_have_taken_effect() && closed.may_have_taken_effect());
        assert!(!rejected.may_have_taken_effect());
        let sent: Vec<String> = herdr
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Prompt { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            sent,
            ["one", "two", "four"],
            "after-effect faults deliver; rejects don't"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn prompt_and_wait_stalls_without_activity() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("a", "claude", AgentStatus::Done);
        let target = Target::from(&pane);

        // A `done` agent must not satisfy `until=[Done]` just by having been done before.
        let err = herdr
            .prompt_and_wait(&target, "go", &[AgentStatus::Done], None)
            .await
            .expect_err("no activity");

        assert_eq!(err.code(), Some(&ErrorCode::AgentPromptStalled));
        assert!(err.may_have_taken_effect());
        assert_eq!(herdr.calls().len(), 1, "the prompt was delivered");
    }

    #[tokio::test]
    async fn validation_failures_do_not_consume_after_effect_faults() {
        let herdr = FakeHerdr::new();
        let blocked = herdr.add_agent("a", "claude", AgentStatus::Blocked);
        let idle = herdr.add_agent("b", "claude", AgentStatus::Idle);
        herdr.fail_next(Method::AgentPrompt, Fault::AfterEffect(ErrorCode::Timeout));

        let refused = herdr
            .prompt(&(&blocked).into(), "x")
            .await
            .expect_err("blocked");
        assert_eq!(refused.code(), Some(&ErrorCode::AgentBlocked));
        let faulted = herdr
            .prompt(&(&idle).into(), "y")
            .await
            .expect_err("fault still pending");
        assert_eq!(faulted.code(), Some(&ErrorCode::Timeout));
    }

    #[tokio::test]
    async fn agent_lifecycle_emits_detection_and_release() {
        let herdr = FakeHerdr::new();
        let created = herdr.create_workspace("w", None).await.expect("ws");
        let pane = created.root_pane.pane_id.clone();
        let mut events = herdr
            .subscribe(&[Subscription::AgentDetected])
            .await
            .expect("sub");
        let spec = StartAgent {
            name: AgentName::new("impl-t11").expect("valid"),
            kind: "claude".to_owned(),
            pane: pane.clone(),
            args: Vec::new(),
            timeout: None,
        };

        herdr.start_agent(&spec).await.expect("start");
        herdr.release_agent(&pane);

        let started = events.next().await.expect("event").expect("ok");
        assert!(matches!(
            started,
            Event::PaneAgentDetected {
                released: false,
                ..
            }
        ));
        let released = events.next().await.expect("event").expect("ok");
        assert!(matches!(
            released,
            Event::PaneAgentDetected {
                released: true,
                final_status: Some(AgentStatus::Idle),
                ..
            }
        ));
        assert!(
            herdr.agent(&(&pane).into()).await.is_err(),
            "pane no longer hosts the agent"
        );
    }

    #[tokio::test]
    async fn invalid_keys_are_rejected_before_any_are_sent() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("a", "claude", AgentStatus::Blocked);
        let target = Target::from(&pane);
        herdr
            .send_keys(&target, &["2", "enter", "ctrl+c", "down"])
            .await
            .expect("valid");
        let err = herdr
            .send_keys(&target, &["1", "rm -rf"])
            .await
            .expect_err("invalid");
        assert_eq!(err.code(), Some(&ErrorCode::InvalidKey));
        assert_eq!(herdr.calls().len(), 1);
    }

    #[tokio::test]
    async fn events_are_filtered_by_subscription() {
        let herdr = FakeHerdr::new();
        let watched = herdr.add_agent("a", "claude", AgentStatus::Idle);
        let other = herdr.add_agent("b", "claude", AgentStatus::Idle);
        let subs = [
            Subscription::agent_status(watched.clone()),
            Subscription::PaneClosed,
        ];
        let mut events = herdr.subscribe(&subs).await.expect("subscribe");

        herdr.set_status(&other, AgentStatus::Working); // not subscribed
        herdr.set_status(&watched, AgentStatus::Blocked);
        herdr.close_pane(&other).await.expect("close");

        let first = events.next().await.expect("event").expect("ok");
        assert!(
            matches!(first, Event::PaneAgentStatusChanged { pane_id, .. } if pane_id == watched)
        );
        let second = events.next().await.expect("event").expect("ok");
        assert!(matches!(second, Event::PaneClosed { pane_id, .. } if pane_id == other));
    }

    /// The discovery pattern the events module documents: watch for new
    /// panes and agents, then subscribe per pane.
    #[tokio::test]
    async fn discovery_events_for_new_panes_and_agents() {
        let herdr = FakeHerdr::new();
        let subs = [Subscription::PaneCreated, Subscription::AgentDetected];
        let mut events = herdr.subscribe(&subs).await.expect("subscribe");

        let created = herdr.create_workspace("w", None).await.expect("ws");
        herdr.detect_agent(&created.root_pane.pane_id, "claude", AgentStatus::Idle);

        let root = created.root_pane.pane_id;
        let first = events.next().await.expect("event").expect("ok");
        assert!(matches!(first, Event::PaneCreated { pane } if pane.pane_id == root));
        let second = events.next().await.expect("event").expect("ok");
        assert!(
            matches!(second, Event::PaneAgentDetected { agent: Some(kind), .. } if kind == "claude")
        );
    }

    #[tokio::test]
    async fn closing_a_workspace_reports_every_pane_and_tab() {
        let herdr = FakeHerdr::new();
        let created = herdr.create_workspace("w", None).await.expect("ws");
        let ws = created.workspace.workspace_id.clone();
        herdr.create_tab(&ws, "second", None).await.expect("tab");
        let subs = [
            Subscription::PaneClosed,
            Subscription::TabClosed,
            Subscription::WorkspaceClosed,
        ];
        let mut events = herdr.subscribe(&subs).await.expect("subscribe");

        herdr.close_workspace(&ws).await.expect("close");

        let mut kinds = Vec::new();
        for _ in 0..5 {
            kinds.push(match events.next().await.expect("event").expect("ok") {
                Event::PaneClosed { .. } => "pane",
                Event::TabClosed { .. } => "tab",
                Event::WorkspaceClosed { .. } => "workspace",
                other => panic!("unexpected {other:?}"),
            });
        }
        assert_eq!(kinds, ["pane", "pane", "tab", "tab", "workspace"]);
    }

    #[tokio::test]
    async fn agent_start_refusals_use_herdr_codes() {
        let herdr = FakeHerdr::new();
        let created = herdr
            .create_workspace("agent-swarm", None)
            .await
            .expect("ws");
        let ws = &created.workspace.workspace_id;
        let tab = herdr.create_tab(ws, "T11", None).await.expect("tab");
        let root = tab.root_pane.pane_id.clone();
        let split = herdr
            .split_pane(&root, SplitDirection::Right, None)
            .await
            .expect("split");
        let spec = StartAgent {
            name: AgentName::new("impl-t11").expect("valid"),
            kind: "claude".to_owned(),
            pane: root,
            args: vec!["--permission-mode".to_owned(), "auto".to_owned()],
            timeout: None,
        };

        assert_eq!(
            herdr.start_agent(&spec).await.expect("start").agent_status,
            AgentStatus::Idle
        );
        let same_name = StartAgent {
            pane: split.pane_id.clone(),
            ..spec.clone()
        };
        let err = herdr.start_agent(&same_name).await.expect_err("taken");
        assert_eq!(err.code(), Some(&ErrorCode::AgentNameTaken));
        let occupied = StartAgent {
            name: AgentName::new("rev-t11-r1").expect("valid"),
            ..spec
        };
        let err = herdr.start_agent(&occupied).await.expect_err("occupied");
        assert_eq!(err.code(), Some(&ErrorCode::AgentPaneUnavailable));

        herdr.close_tab(&tab.tab.tab_id).await.expect("close tab");
        let gone = herdr
            .agent(&name("impl-t11"))
            .await
            .expect_err("tab closed");
        assert!(gone.code().is_some_and(ErrorCode::is_not_found));
    }

    #[tokio::test]
    async fn a_fresh_shell_refuses_agents_until_it_is_at_its_prompt() {
        let herdr = FakeHerdr::new();
        herdr.set_shell_startup(2);
        herdr.set_start_status(AgentStatus::Unknown);
        let created = herdr
            .create_workspace("passporter", None)
            .await
            .expect("ws");
        let spec = StartAgent {
            name: AgentName::new("pp-web-1").expect("valid"),
            kind: "claude".to_owned(),
            pane: created.root_pane.pane_id.clone(),
            args: Vec::new(),
            timeout: None,
        };
        for _ in 0..2 {
            let busy = herdr.start_agent(&spec).await.expect_err("shell booting");
            assert_eq!(busy.code(), Some(&ErrorCode::AgentPaneBusy));
            assert!(!busy.may_have_taken_effect());
        }
        assert!(herdr.calls().is_empty(), "nothing started while busy");
        let started = herdr.start_agent(&spec).await.expect("at its prompt");
        assert_eq!(started.agent_status, AgentStatus::Unknown);
    }

    #[tokio::test]
    async fn start_at_shell_waits_out_a_booting_shell() {
        let herdr = FakeHerdr::new();
        herdr.set_shell_startup(3);
        let created = herdr
            .create_workspace("passporter", None)
            .await
            .expect("ws");
        let spec = StartAgent {
            name: AgentName::new("pp-web-2").expect("valid"),
            kind: "codex".to_owned(),
            pane: created.root_pane.pane_id.clone(),
            args: Vec::new(),
            timeout: None,
        };
        let quick = ShellWait {
            timeout: Duration::from_secs(5),
            poll: Duration::from_millis(1),
        };
        let agent = herdr
            .start_agent_at_shell(&spec, quick)
            .await
            .expect("started once the shell was ready");
        assert_eq!(agent.kind.as_deref(), Some("codex"));

        // A shell that never gets there: the busy refusal comes back.
        herdr.set_shell_startup(u32::MAX);
        let tab = herdr
            .create_tab(&created.workspace.workspace_id, "slow", None)
            .await
            .expect("tab");
        let slow = StartAgent {
            name: AgentName::new("pp-web-3").expect("valid"),
            pane: tab.root_pane.pane_id,
            ..spec
        };
        let short = ShellWait {
            timeout: Duration::from_millis(20),
            poll: Duration::from_millis(5),
        };
        let err = herdr
            .start_agent_at_shell(&slow, short)
            .await
            .expect_err("gave up");
        assert_eq!(err.code(), Some(&ErrorCode::AgentPaneBusy));
    }

    #[tokio::test]
    async fn screen_reads_respect_line_limits() {
        let herdr = FakeHerdr::new();
        let pane = herdr.add_agent("a", "claude", AgentStatus::Blocked);
        herdr.set_screen(&pane, "one\ntwo\nRun tests?\n1. Yes\n2. No");

        let target = Target::from(&pane);
        let screen = herdr
            .read_agent(&target, ReadSource::Detection, Some(3))
            .await
            .expect("read");

        assert_eq!(screen.text, "Run tests?\n1. Yes\n2. No");
        assert!(screen.truncated);
    }

    #[test]
    fn key_validation() {
        for ok in ["enter", "esc", "1", "y", "ctrl+c", "down", "tab"] {
            assert!(valid_key(ok), "{ok}");
        }
        for bad in ["", "ENTER", "ctrl+", "ctrl+cc", "rm -rf", "é", "f13"] {
            assert!(!valid_key(bad), "{bad}");
        }
    }
}
