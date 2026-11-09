//! The [`Herdr`] trait and its socket implementation.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{Instant, timeout_at};

use crate::error::{Error, ErrorCode, Result};
use crate::events::{Envelope, Event, Subscription};
use crate::ids::{AgentName, PaneId, TabId, Target, WorkspaceId};
use crate::model::{
    Agent, AgentStatus, CreatedTab, CreatedWorkspace, Pane, ReadSource, Screen, ServerInfo,
    SplitDirection, Workspace,
};

/// Everything Passporter needs from herdr. Implemented by [`SocketClient`]
/// for real use and by `FakeHerdr` (feature `fake`) for tests.
pub trait Herdr: Send + Sync {
    type Events: EventStream;

    fn ping(&self) -> impl Future<Output = Result<ServerInfo>> + Send;

    /// Every live agent across all workspaces.
    fn agents(&self) -> impl Future<Output = Result<Vec<Agent>>> + Send;

    fn agent(&self, target: &Target) -> impl Future<Output = Result<Agent>> + Send;

    fn read_agent(
        &self,
        target: &Target,
        source: ReadSource,
        lines: Option<u32>,
    ) -> impl Future<Output = Result<Screen>> + Send;

    /// Submits `text` plus Enter and returns once submitted (the `working`
    /// transition arrives later as an event). Fails with `agent_blocked`,
    /// sending nothing, if the agent is at a dialog.
    fn prompt(&self, target: &Target, text: &str) -> impl Future<Output = Result<Agent>> + Send;

    /// Submits `text` and waits server-side for the turn to settle (one of
    /// `until`; empty means idle, done or blocked). Fails with
    /// `agent_prompt_stalled` if no activity follows within five seconds;
    /// that does not prove the prompt wasn't delivered.
    fn prompt_and_wait(
        &self,
        target: &Target,
        text: &str,
        until: &[AgentStatus],
        timeout: Option<Duration>,
    ) -> impl Future<Output = Result<Agent>> + Send;

    /// Sends logical keys (`enter`, `esc`, `1`, `down`, `ctrl+c`, ...). herdr
    /// validates all keys before writing any (`invalid_key`).
    fn send_keys(&self, target: &Target, keys: &[&str]) -> impl Future<Output = Result<()>> + Send;

    /// Waits for one of `until` (empty: any settled state: idle, done, blocked).
    fn wait(
        &self,
        target: &Target,
        until: &[AgentStatus],
        timeout: Option<Duration>,
    ) -> impl Future<Output = Result<Agent>> + Send;

    fn start_agent(&self, spec: &StartAgent) -> impl Future<Output = Result<Agent>> + Send;

    /// [`start_agent`](Self::start_agent) in a fresh pane, once the pane's
    /// shell is at its prompt. A new tab's shell runs its rc files first,
    /// and until it shows a prompt herdr refuses `agent.start` with
    /// `agent_pane_busy` (nothing started); this retries every
    /// `shell.poll` for up to `shell.timeout`, then returns that refusal.
    /// Any other result, success or error, is returned at once.
    fn start_agent_at_shell(
        &self,
        spec: &StartAgent,
        shell: ShellWait,
    ) -> impl Future<Output = Result<Agent>> + Send
    where
        Self: Sized,
    {
        async move {
            let deadline = Instant::now() + shell.timeout;
            loop {
                match self.start_agent(spec).await {
                    Err(error)
                        if error.code() == Some(&ErrorCode::AgentPaneBusy)
                            && Instant::now() + shell.poll < deadline =>
                    {
                        tokio::time::sleep(shell.poll).await;
                    }
                    other => return other,
                }
            }
        }
    }

    /// Every workspace, with its label.
    fn workspaces(&self) -> impl Future<Output = Result<Vec<Workspace>>> + Send;

    fn create_workspace(
        &self,
        label: &str,
        cwd: Option<&Path>,
    ) -> impl Future<Output = Result<CreatedWorkspace>> + Send;

    fn create_tab(
        &self,
        workspace: &WorkspaceId,
        label: &str,
        cwd: Option<&Path>,
    ) -> impl Future<Output = Result<CreatedTab>> + Send;

    /// Splits `pane`, never moving focus.
    fn split_pane(
        &self,
        pane: &PaneId,
        direction: SplitDirection,
        cwd: Option<&Path>,
    ) -> impl Future<Output = Result<Pane>> + Send;

    fn read_pane(
        &self,
        pane: &PaneId,
        source: ReadSource,
        lines: Option<u32>,
    ) -> impl Future<Output = Result<Screen>> + Send;

    /// Types `command` plus Enter into a shell pane.
    fn run_in_pane(&self, pane: &PaneId, command: &str) -> impl Future<Output = Result<()>> + Send;

    /// Sends logical keys to any pane, a plain shell included (`pane.send_keys`;
    /// herdr validates them like [`send_keys`](Self::send_keys)).
    fn send_pane_keys(
        &self,
        pane: &PaneId,
        keys: &[&str],
    ) -> impl Future<Output = Result<()>> + Send;

    fn close_pane(&self, pane: &PaneId) -> impl Future<Output = Result<()>> + Send;

    fn close_tab(&self, tab: &TabId) -> impl Future<Output = Result<()>> + Send;

    fn close_workspace(&self, workspace: &WorkspaceId) -> impl Future<Output = Result<()>> + Send;

    fn subscribe(
        &self,
        subscriptions: &[Subscription],
    ) -> impl Future<Output = Result<Self::Events>> + Send;
}

/// A live stream of herdr events.
pub trait EventStream: Send {
    /// The next event, or `None` once the stream has ended.
    fn next(&mut self) -> impl Future<Output = Option<Result<Event>>> + Send;
}

/// How to start an agent in an existing, idle shell pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartAgent {
    pub name: AgentName,
    /// herdr agent kind, e.g. `claude`.
    pub kind: String,
    pub pane: PaneId,
    /// Native agent arguments. Sent as a JSON array: no shell quoting involved.
    pub args: Vec<String>,
    /// Startup timeout; herdr accepts 3 s < t <= 300 s (default 30 s).
    pub timeout: Option<Duration>,
}

/// How long [`Herdr::start_agent_at_shell`] waits for a fresh pane's shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellWait {
    /// Give up after this long.
    pub timeout: Duration,
    /// Try again this often.
    pub poll: Duration,
}

impl Default for ShellWait {
    /// 30 s, checked every 500 ms: rc files that take longer are broken.
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            poll: Duration::from_millis(500),
        }
    }
}

/// Talks to a herdr server over its unix socket (newline-delimited JSON).
///
/// Each request uses a fresh connection, so calls are independent and a
/// dropped connection affects only the call in flight.
#[derive(Debug)]
pub struct SocketClient {
    path: PathBuf,
    /// Client-side cap for calls that don't wait server-side.
    request_timeout: Duration,
    next_id: AtomicU64,
}

/// Extra time allowed beyond a server-side wait before the client gives up.
const WAIT_SLACK: Duration = Duration::from_secs(5);
const SUBSCRIBE: &str = "events.subscribe";

impl SocketClient {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            request_timeout: Duration::from_secs(30),
            next_id: AtomicU64::new(1),
        }
    }

    /// The socket of the session this process runs in (`HERDR_SOCKET_PATH`,
    /// set by herdr in every pane), else the default session's socket.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") {
            return Some(Self::new(path));
        }
        let home = std::env::var_os("HOME")?;
        Some(Self::new(Path::new(&home).join(".config/herdr/herdr.sock")))
    }

    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Connects and sends one request line, all before `deadline`.
    async fn open(
        &self,
        method: &'static str,
        params: Value,
        deadline: Instant,
        budget: Duration,
    ) -> Result<(Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf)> {
        let not_sent = |_| Error::TimedOut {
            method,
            after: budget,
            sent: false,
        };
        let stream = timeout_at(deadline, UnixStream::connect(&self.path))
            .await
            .map_err(not_sent)?
            .map_err(|source| Error::Connect {
                path: self.path.clone(),
                source,
            })?;
        let (read, mut write) = stream.into_split();
        let id = format!(
            "passporter-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let mut line = json!({"id": id, "method": method, "params": params}).to_string();
        line.push('\n');
        // A partial write may already have reached herdr, so a timeout here counts as sent.
        timeout_at(deadline, write.write_all(line.as_bytes()))
            .await
            .map_err(|_| Error::TimedOut {
                method,
                after: budget,
                sent: true,
            })?
            .map_err(|source| Error::Io { method, source })?;
        Ok((BufReader::new(read).lines(), write))
    }

    /// One request, one reply, checked against the expected reply `type`s.
    async fn call<R: DeserializeOwned>(
        &self,
        method: &'static str,
        params: Value,
        expected: &'static [&'static str],
        wait: Duration,
    ) -> Result<R> {
        let deadline = Instant::now().checked_add(wait).unwrap_or_else(far_future);
        let (mut lines, _write) = self.open(method, params, deadline, wait).await?;
        let reply = timeout_at(deadline, read_reply(&mut lines, method))
            .await
            .map_err(|_| Error::TimedOut {
                method,
                after: wait,
                sent: true,
            })??;
        reply.into_result(method, expected)
    }

    async fn call_ok(&self, method: &'static str, params: Value) -> Result<()> {
        let _: Value = self
            .call(method, params, &["ok"], self.request_timeout)
            .await?;
        Ok(())
    }
}

/// herdr takes paths as JSON strings, so a non-UTF-8 path is a caller error, not a panic.
fn utf8_path<'p>(method: &'static str, path: Option<&'p Path>) -> Result<Option<&'p str>> {
    path.map(|p| {
        p.to_str().ok_or_else(|| Error::NonUtf8Path {
            method,
            path: p.to_path_buf(),
        })
    })
    .transpose()
}

/// Roughly thirty years out: "no deadline" for waits without a timeout.
fn far_future() -> Instant {
    Instant::now() + Duration::from_hours(24 * 365 * 30)
}

async fn read_reply(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
    method: &'static str,
) -> Result<Reply> {
    let line = lines
        .next_line()
        .await
        .map_err(|source| Error::Io { method, source })?
        .ok_or(Error::Closed { method })?;
    serde_json::from_str(&line).map_err(|source| Error::Decode { method, source })
}

/// `{"id": ..., "result": {...}}` or `{"id": ..., "error": {"code", "message"}}`.
///
/// Request-level rejections (`invalid_request`, ...) arrive with `"id": ""`;
/// since each connection carries exactly one request, the first reply is ours.
#[derive(Debug, Deserialize)]
struct Reply {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<ServerError>,
}

#[derive(Debug, Deserialize)]
struct ServerError {
    code: String,
    message: String,
}

impl Reply {
    fn into_result<R: DeserializeOwned>(
        self,
        method: &'static str,
        expected: &'static [&'static str],
    ) -> Result<R> {
        if let Some(err) = self.error {
            return Err(Error::Server {
                method,
                code: err.code.as_str().into(),
                message: err.message,
            });
        }
        let result = self.result.unwrap_or(Value::Null);
        let got = result
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<none>");
        if !expected.contains(&got) {
            return Err(Error::UnexpectedReply {
                method,
                expected: expected.first().copied().unwrap_or("<any>"),
                got: got.to_owned(),
            });
        }
        serde_json::from_value(result).map_err(|source| Error::Decode { method, source })
    }
}

#[derive(Deserialize)]
struct AgentReply {
    agent: Agent,
}

#[derive(Deserialize)]
struct AgentsReply {
    agents: Vec<Agent>,
}

#[derive(Deserialize)]
struct WorkspacesReply {
    workspaces: Vec<Workspace>,
}

#[derive(Deserialize)]
struct ReadReply {
    read: Screen,
}

#[derive(Deserialize)]
struct PaneReply {
    pane: Pane,
}

fn timeout_ms(timeout: Option<Duration>) -> Option<u64> {
    timeout.map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX))
}

/// Client-side budget for a call that waits server-side for `timeout`.
fn wait_budget(timeout: Option<Duration>) -> Duration {
    timeout.map_or(Duration::MAX, |t| t.saturating_add(WAIT_SLACK))
}

impl Herdr for SocketClient {
    type Events = SocketEvents;

    async fn ping(&self) -> Result<ServerInfo> {
        self.call("ping", json!({}), &["pong"], self.request_timeout)
            .await
    }

    async fn agents(&self) -> Result<Vec<Agent>> {
        let reply: AgentsReply = self
            .call(
                "agent.list",
                json!({}),
                &["agent_list"],
                self.request_timeout,
            )
            .await?;
        Ok(reply.agents)
    }

    async fn agent(&self, target: &Target) -> Result<Agent> {
        let params = json!({"target": target.as_str()});
        let reply: AgentReply = self
            .call("agent.get", params, &["agent_info"], self.request_timeout)
            .await?;
        Ok(reply.agent)
    }

    async fn read_agent(
        &self,
        target: &Target,
        source: ReadSource,
        lines: Option<u32>,
    ) -> Result<Screen> {
        let params = json!({"target": target.as_str(), "source": source, "lines": lines});
        let reply: ReadReply = self
            .call("agent.read", params, &["pane_read"], self.request_timeout)
            .await?;
        Ok(reply.read)
    }

    async fn prompt(&self, target: &Target, text: &str) -> Result<Agent> {
        let params = json!({"target": target.as_str(), "text": text});
        let reply: AgentReply = self
            .call(
                "agent.prompt",
                params,
                &["agent_prompted"],
                self.request_timeout,
            )
            .await?;
        Ok(reply.agent)
    }

    async fn prompt_and_wait(
        &self,
        target: &Target,
        text: &str,
        until: &[AgentStatus],
        timeout: Option<Duration>,
    ) -> Result<Agent> {
        let params = json!({
            "target": target.as_str(),
            "text": text,
            "wait": {"until": until, "timeout_ms": timeout_ms(timeout)},
        });
        let expected = &["agent_prompted", "agent_info"];
        let reply: AgentReply = self
            .call("agent.prompt", params, expected, wait_budget(timeout))
            .await?;
        Ok(reply.agent)
    }

    async fn send_keys(&self, target: &Target, keys: &[&str]) -> Result<()> {
        self.call_ok(
            "agent.send_keys",
            json!({"target": target.as_str(), "keys": keys}),
        )
        .await
    }

    async fn wait(
        &self,
        target: &Target,
        until: &[AgentStatus],
        timeout: Option<Duration>,
    ) -> Result<Agent> {
        let params =
            json!({"target": target.as_str(), "until": until, "timeout_ms": timeout_ms(timeout)});
        let reply: AgentReply = self
            .call("agent.wait", params, &["agent_info"], wait_budget(timeout))
            .await?;
        Ok(reply.agent)
    }

    async fn start_agent(&self, spec: &StartAgent) -> Result<Agent> {
        let params = json!({
            "name": spec.name.as_str(),
            "kind": spec.kind,
            "pane_id": spec.pane.as_str(),
            "args": spec.args,
            "timeout_ms": timeout_ms(spec.timeout),
        });
        let budget = wait_budget(Some(spec.timeout.unwrap_or(Duration::from_secs(30))));
        let reply: AgentReply = self
            .call("agent.start", params, &["agent_started"], budget)
            .await?;
        Ok(reply.agent)
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>> {
        let reply: WorkspacesReply = self
            .call(
                "workspace.list",
                json!({}),
                &["workspace_list"],
                self.request_timeout,
            )
            .await?;
        Ok(reply.workspaces)
    }

    async fn create_workspace(&self, label: &str, cwd: Option<&Path>) -> Result<CreatedWorkspace> {
        let cwd = utf8_path("workspace.create", cwd)?;
        let params = json!({"label": label, "cwd": cwd, "focus": false});
        self.call(
            "workspace.create",
            params,
            &["workspace_created"],
            self.request_timeout,
        )
        .await
    }

    async fn create_tab(
        &self,
        workspace: &WorkspaceId,
        label: &str,
        cwd: Option<&Path>,
    ) -> Result<CreatedTab> {
        let cwd = utf8_path("tab.create", cwd)?;
        let params = json!({
            "workspace_id": workspace.as_str(),
            "label": label,
            "cwd": cwd,
            "focus": false,
        });
        self.call("tab.create", params, &["tab_created"], self.request_timeout)
            .await
    }

    async fn split_pane(
        &self,
        pane: &PaneId,
        direction: SplitDirection,
        cwd: Option<&Path>,
    ) -> Result<Pane> {
        let cwd = utf8_path("pane.split", cwd)?;
        let params = json!({
            "target_pane_id": pane.as_str(),
            "direction": direction,
            "cwd": cwd,
            "focus": false,
        });
        let reply: PaneReply = self
            .call("pane.split", params, &["pane_info"], self.request_timeout)
            .await?;
        Ok(reply.pane)
    }

    async fn read_pane(
        &self,
        pane: &PaneId,
        source: ReadSource,
        lines: Option<u32>,
    ) -> Result<Screen> {
        let params = json!({"pane_id": pane.as_str(), "source": source, "lines": lines});
        let reply: ReadReply = self
            .call("pane.read", params, &["pane_read"], self.request_timeout)
            .await?;
        Ok(reply.read)
    }

    async fn run_in_pane(&self, pane: &PaneId, command: &str) -> Result<()> {
        let text = format!("{command}\r");
        self.call_ok(
            "pane.send_text",
            json!({"pane_id": pane.as_str(), "text": text}),
        )
        .await
    }

    async fn send_pane_keys(&self, pane: &PaneId, keys: &[&str]) -> Result<()> {
        self.call_ok(
            "pane.send_keys",
            json!({"pane_id": pane.as_str(), "keys": keys}),
        )
        .await
    }

    async fn close_pane(&self, pane: &PaneId) -> Result<()> {
        self.call_ok("pane.close", json!({"pane_id": pane.as_str()}))
            .await
    }

    async fn close_tab(&self, tab: &TabId) -> Result<()> {
        self.call_ok("tab.close", json!({"tab_id": tab.as_str()}))
            .await
    }

    async fn close_workspace(&self, workspace: &WorkspaceId) -> Result<()> {
        self.call_ok(
            "workspace.close",
            json!({"workspace_id": workspace.as_str()}),
        )
        .await
    }

    async fn subscribe(&self, subscriptions: &[Subscription]) -> Result<SocketEvents> {
        let deadline = Instant::now()
            .checked_add(self.request_timeout)
            .unwrap_or_else(far_future);
        let params = json!({"subscriptions": subscriptions});
        let (mut lines, write) = self
            .open(SUBSCRIBE, params, deadline, self.request_timeout)
            .await?;
        let started = timeout_at(deadline, read_reply(&mut lines, SUBSCRIBE))
            .await
            .map_err(|_| Error::TimedOut {
                method: SUBSCRIBE,
                after: self.request_timeout,
                sent: true,
            })??;
        let _: Value = started.into_result(SUBSCRIBE, &["subscription_started"])?;
        // The write half must stay open: herdr ends the subscription when it closes.
        Ok(SocketEvents {
            lines,
            _write: write,
        })
    }
}

/// Events from a [`SocketClient::subscribe`] connection.
#[derive(Debug)]
pub struct SocketEvents {
    lines: Lines<BufReader<OwnedReadHalf>>,
    _write: OwnedWriteHalf,
}

impl EventStream for SocketEvents {
    async fn next(&mut self) -> Option<Result<Event>> {
        match self.lines.next_line().await {
            Ok(Some(line)) => {
                // A server shutting down may send an error reply instead of an event.
                if let Ok(Reply {
                    error: Some(err), ..
                }) = serde_json::from_str::<Reply>(&line)
                {
                    return Some(Err(Error::Server {
                        method: SUBSCRIBE,
                        code: err.code.as_str().into(),
                        message: err.message,
                    }));
                }
                Some(
                    serde_json::from_str::<Envelope>(&line)
                        .and_then(Envelope::into_event)
                        .map_err(|source| Error::Decode {
                            method: SUBSCRIBE,
                            source,
                        }),
                )
            }
            Ok(None) => None,
            Err(source) => Some(Err(Error::Io {
                method: SUBSCRIBE,
                source,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// A unix-socket server in a private temp dir, removed on drop.
    struct TestServer {
        dir: PathBuf,
        socket: PathBuf,
    }

    impl TestServer {
        fn bind() -> (Self, UnixListener) {
            static N: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "herdr-client-test-{}-{nanos}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let socket = dir.join("herdr.sock");
            let listener = UnixListener::bind(&socket).expect("bind");
            (Self { dir, socket }, listener)
        }

        /// Answers each connection's single request with `reply(request)`, then closes it.
        fn replying(reply: impl Fn(&Value) -> Vec<Value> + Send + Sync + 'static) -> Self {
            let (server, listener) = Self::bind();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    if let Ok(Some(line)) = lines.next_line().await {
                        let request: Value = serde_json::from_str(&line).expect("request is JSON");
                        for out in reply(&request) {
                            let line = format!("{out}\n");
                            write.write_all(line.as_bytes()).await.expect("write");
                        }
                    }
                }
            });
            server
        }

        /// Reads requests but never replies, holding each connection open.
        fn silent() -> Self {
            let (server, listener) = Self::bind();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stream).lines();
                        while let Ok(Some(_)) = lines.next_line().await {}
                    });
                }
            });
            server
        }

        fn client(&self) -> SocketClient {
            SocketClient::new(&self.socket)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn ok(request: &Value, result: &Value) -> Value {
        json!({"id": request["id"], "result": result})
    }

    fn agent_json(status: &str) -> Value {
        json!({"terminal_id": "t1", "agent_status": status, "workspace_id": "w1",
               "tab_id": "w1:t1", "pane_id": "w1:p1", "focused": false, "revision": 2,
               "state_change_seq": 9})
    }

    fn pane() -> Target {
        PaneId::new("w1:p1").into()
    }

    #[tokio::test]
    async fn sends_method_and_params_and_decodes_result() {
        let server = TestServer::replying(|req| {
            assert_eq!(req["method"], "agent.prompt");
            assert_eq!(req["params"], json!({"target": "impl-t11", "text": "go"}));
            vec![ok(
                req,
                &json!({"type": "agent_prompted", "agent": agent_json("idle")}),
            )]
        });
        let target = Target::Name(AgentName::new("impl-t11").expect("valid"));

        let agent = server
            .client()
            .prompt(&target, "go")
            .await
            .expect("prompt ok");

        assert_eq!(agent.agent_status, AgentStatus::Idle);
        assert_eq!(agent.state_change_seq, 9);
    }

    #[tokio::test]
    async fn lists_workspaces_with_their_labels() {
        let server = TestServer::replying(|req| {
            assert_eq!(req["method"], "workspace.list");
            vec![ok(
                req,
                &json!({"type": "workspace_list", "workspaces": [{
                    "workspace_id": "w1", "label": "agent-swarm", "number": 1,
                    "focused": false, "pane_count": 3, "tab_count": 2,
                    "active_tab_id": "w1:t1", "agent_status": "working",
                }]}),
            )]
        });

        let workspaces = server.client().workspaces().await.expect("list ok");

        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].workspace_id, WorkspaceId::new("w1"));
        assert_eq!(workspaces[0].label, "agent-swarm");
    }

    #[tokio::test]
    async fn prompt_and_wait_sends_wait_options() {
        let server = TestServer::replying(|req| {
            assert_eq!(
                req["params"]["wait"],
                json!({"until": ["done", "blocked"], "timeout_ms": 60_000})
            );
            vec![ok(
                req,
                &json!({"type": "agent_info", "agent": agent_json("done")}),
            )]
        });
        let until = [AgentStatus::Done, AgentStatus::Blocked];

        let agent = server
            .client()
            .prompt_and_wait(&pane(), "go", &until, Some(Duration::from_secs(60)))
            .await
            .expect("prompt ok");

        assert_eq!(agent.agent_status, AgentStatus::Done);
    }

    #[tokio::test]
    async fn server_errors_are_typed() {
        let server = TestServer::replying(|req| {
            vec![json!({"id": req["id"], "error": {"code": "agent_blocked", "message": "dialog"}})]
        });
        let err = server
            .client()
            .prompt(&pane(), "go")
            .await
            .expect_err("blocked");
        assert_eq!(err.code(), Some(&ErrorCode::AgentBlocked));
        assert!(!err.may_have_taken_effect());
    }

    /// herdr answers request-level rejections with an empty id, then closes.
    #[tokio::test]
    async fn empty_id_rejections_keep_their_code() {
        let server = TestServer::replying(|_req| {
            vec![json!({"id": "", "error": {"code": "invalid_request", "message": "bad params"}})]
        });
        let err = server.client().agents().await.expect_err("rejected");
        assert_eq!(err.code(), Some(&ErrorCode::InvalidRequest));
        assert!(
            !err.may_have_taken_effect(),
            "a definite rejection, not an ambiguous drop"
        );
    }

    #[tokio::test]
    async fn checks_reply_type() {
        let server = TestServer::replying(|req| {
            vec![ok(
                req,
                &json!({"type": "pong", "version": "x", "protocol": 1}),
            )]
        });
        let err = server.client().agents().await.expect_err("wrong type");
        assert!(
            matches!(&err, Error::UnexpectedReply { got, .. } if got == "pong"),
            "{err}"
        );
        assert!(err.may_have_taken_effect(), "herdr did process the request");
    }

    #[tokio::test]
    async fn connection_closed_after_sending_may_have_taken_effect() {
        let server = TestServer::replying(|_req| Vec::new());
        let err = server
            .client()
            .prompt(&pane(), "go")
            .await
            .expect_err("closed");
        assert!(matches!(err, Error::Closed { .. }), "{err}");
        assert!(err.may_have_taken_effect());
    }

    #[tokio::test]
    async fn missing_socket_is_a_connect_error() {
        let err = SocketClient::new("/nonexistent/herdr.sock")
            .ping()
            .await
            .expect_err("no server");
        assert!(matches!(err, Error::Connect { .. }));
        assert!(!err.may_have_taken_effect());
    }

    #[tokio::test]
    async fn client_side_timeout_after_sending() {
        let server = TestServer::silent();
        let client = server
            .client()
            .with_request_timeout(Duration::from_millis(50));
        let err = client.ping().await.expect_err("times out");
        assert!(matches!(err, Error::TimedOut { sent: true, .. }), "{err}");
        assert!(err.may_have_taken_effect());
    }

    #[tokio::test]
    async fn subscription_streams_both_event_shapes() {
        let server = TestServer::replying(|req| {
            assert_eq!(req["method"], "events.subscribe");
            assert_eq!(
                req["params"]["subscriptions"][0]["type"],
                "pane.agent_status_changed"
            );
            vec![
                ok(req, &json!({"type": "subscription_started"})),
                // Subscription-event shape: dotted kind, untagged data (as herdr 0.9.1 sends).
                json!({"event": "pane.agent_status_changed", "data": {
                    "pane_id": "w1:p1", "workspace_id": "w1", "agent_status": "blocked"}}),
                // Lifecycle-event shape: underscore kind, tagged data.
                json!({"event": "pane_closed", "data": {
                    "type": "pane_closed", "pane_id": "w1:p2", "workspace_id": "w1"}}),
            ]
        });
        let subs = [
            Subscription::agent_status(PaneId::new("w1:p1")),
            Subscription::PaneClosed,
        ];
        let mut events = server.client().subscribe(&subs).await.expect("subscribed");

        let first = events.next().await.expect("event").expect("decodes");
        assert!(matches!(
            first,
            Event::PaneAgentStatusChanged {
                agent_status: AgentStatus::Blocked,
                ..
            }
        ));
        let second = events.next().await.expect("event").expect("decodes");
        assert!(matches!(second, Event::PaneClosed { .. }));
        assert!(
            events.next().await.is_none(),
            "stream ends when the server closes"
        );
    }

    #[tokio::test]
    async fn subscribe_handshake_timeout_is_timed_out() {
        let server = TestServer::silent();
        let client = server
            .client()
            .with_request_timeout(Duration::from_millis(50));
        let err = client
            .subscribe(&[Subscription::PaneClosed])
            .await
            .expect_err("times out");
        assert!(matches!(err, Error::TimedOut { .. }), "{err}");
    }

    #[test]
    fn timeout_ms_saturates() {
        assert_eq!(timeout_ms(None), None);
        assert_eq!(timeout_ms(Some(Duration::from_millis(1500))), Some(1500));
        assert_eq!(timeout_ms(Some(Duration::MAX)), Some(u64::MAX));
    }
}
