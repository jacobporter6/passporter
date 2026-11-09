//! A bridge running on loopback (or an in-memory pipe) in front of a
//! `FakeHerdr`, with a stub authenticator and a fake PWA build.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::connect_info::Connected;
use axum::serve::{IncomingStream, Listener, ListenerExt, TapIo};
use bridge::Settings;
use bridge::actions::ActionSettings;
use bridge::attachments::AttachmentSettings;
use bridge::audio::{AudioSettings, Recognized, TranscribeFuture, Transcriber};
use bridge::pending::PendingSettings;
use bridge::registry::RegistrySettings;
use bridge::runbooks::RunbookSettings;
use bridge::server::{
    AuthFuture, AuthRejection, AuthRequest, Authenticator, Principal, ServerSettings,
    StreamSettings,
};
use bridge::shutdown::{self, ShutdownTrigger};
use bridge::spawn::SpawnSettings;
use bridge::threads::ThreadSettings;
use bridge::tldr::TldrSettings;
use bridge::tls::{CertStore, TlsListener};
use futures_util::{SinkExt, StreamExt};
use herdr::fake::FakeEvents;
use herdr::{
    Agent, CreatedTab, CreatedWorkspace, EventStream, FakeHerdr, Herdr, Pane, PaneId, ReadSource,
    Screen, ServerInfo, ShellWait, SplitDirection, StartAgent, Subscription, TabId, Target,
    Workspace, WorkspaceId,
};
use protocol::AgentEvent;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

/// The only token [`StubAuth`] accepts.
pub(crate) const TOKEN: &str = "test-token";

/// How long any single expectation may take: a deadline for waiting on a
/// real condition (a poll loop, an event, a reply), never a pause. A
/// passing test returns as soon as the condition holds, so this only bounds
/// a real hang. It is generous because `just gate` runs the suite while
/// other builds load the machine, and a multi-step flow (a spawned runner
/// process, a herdr round trip) can then take several seconds.
pub(crate) const PATIENCE: Duration = Duration::from_secs(30);

/// The bridge's own deadlines for multi-step flows (a tab's shell, a new
/// agent settling, a runbook's runner starting, feedback after a choice).
/// As generous as [`PATIENCE`] for the same reason: a test that needs one
/// to expire sets it short itself.
const FLOW_WAIT: Duration = PATIENCE;

/// Accepts `Authorization: Bearer test-token`, refuses everything else.
#[derive(Debug)]
pub(crate) struct StubAuth;

impl Authenticator for StubAuth {
    fn authenticate<'a>(&'a self, request: AuthRequest<'a>) -> AuthFuture<'a> {
        let ok = request
            .headers
            .get("authorization")
            .is_some_and(|v| v.as_bytes() == format!("Bearer {TOKEN}").as_bytes());
        Box::pin(async move {
            if ok {
                Ok(Principal::new("test-device"))
            } else {
                Err(AuthRejection::Unauthenticated)
            }
        })
    }
}

/// A [`FakeHerdr`] whose event streams a test can cut, as if herdr dropped
/// every subscription connection.
#[derive(Debug)]
pub(crate) struct CuttableHerdr {
    pub(crate) fake: FakeHerdr,
    cut: watch::Sender<u64>,
    lifecycle_subscriptions: AtomicUsize,
    unlisted: Mutex<Vec<PaneId>>,
    /// See [`CuttableHerdr::then_report`].
    later_status: Mutex<Option<LaterStatus>>,
}

/// A status `agent.get` reports for a pane after its next read.
#[derive(Debug)]
struct LaterStatus {
    pane: PaneId,
    status: herdr::AgentStatus,
    /// The first read (still true) has happened.
    armed: bool,
}

impl CuttableHerdr {
    pub(crate) fn new() -> Self {
        Self {
            fake: FakeHerdr::new(),
            cut: watch::channel(0).0,
            lifecycle_subscriptions: AtomicUsize::new(0),
            unlisted: Mutex::new(Vec::new()),
            later_status: Mutex::new(None),
        }
    }

    /// Lets the next `agent.get` of `pane` answer truly, then has every
    /// later one report `status`: herdr moving on (a quick reply) right
    /// after the registry has read the agent.
    pub(crate) fn then_report(&self, pane: &PaneId, status: herdr::AgentStatus) {
        *self.later_status.lock().expect("lock") = Some(LaterStatus {
            pane: pane.clone(),
            status,
            armed: false,
        });
    }

    /// Leaves `pane` out of `agent.list` (as herdr does for an agent still
    /// launching) while `agent.get` keeps answering for it.
    pub(crate) fn unlist(&self, pane: &PaneId) {
        self.unlisted.lock().expect("lock").push(pane.clone());
    }

    /// How many lifecycle (`pane.created`, ...) subscriptions were opened:
    /// one per registry session.
    pub(crate) fn lifecycle_subscriptions(&self) -> usize {
        self.lifecycle_subscriptions.load(Ordering::SeqCst)
    }

    /// Ends every open event stream; new subscriptions work as usual.
    pub(crate) fn cut_streams(&self) {
        self.cut.send_modify(|generation| *generation += 1);
    }
}

/// A [`FakeEvents`] that ends when its herdr cuts streams.
#[derive(Debug)]
pub(crate) struct CuttableEvents {
    inner: FakeEvents,
    cut: watch::Receiver<u64>,
}

impl EventStream for CuttableEvents {
    async fn next(&mut self) -> Option<herdr::Result<herdr::Event>> {
        tokio::select! {
            event = self.inner.next() => event,
            _ = self.cut.changed() => None,
        }
    }
}

impl Herdr for CuttableHerdr {
    type Events = CuttableEvents;

    async fn ping(&self) -> herdr::Result<ServerInfo> {
        self.fake.ping().await
    }
    async fn agents(&self) -> herdr::Result<Vec<Agent>> {
        let mut agents = self.fake.agents().await?;
        let unlisted = self.unlisted.lock().expect("lock").clone();
        agents.retain(|agent| !unlisted.contains(&agent.pane_id));
        Ok(agents)
    }
    async fn agent(&self, target: &Target) -> herdr::Result<Agent> {
        let mut agent = self.fake.agent(target).await?;
        let mut later = self.later_status.lock().expect("lock");
        if let Some(later) = later.as_mut().filter(|l| l.pane == agent.pane_id) {
            if later.armed {
                agent.agent_status = later.status;
            }
            later.armed = true;
        }
        Ok(agent)
    }
    async fn read_agent(
        &self,
        target: &Target,
        source: ReadSource,
        lines: Option<u32>,
    ) -> herdr::Result<Screen> {
        self.fake.read_agent(target, source, lines).await
    }
    async fn prompt(&self, target: &Target, text: &str) -> herdr::Result<Agent> {
        self.fake.prompt(target, text).await
    }
    async fn prompt_and_wait(
        &self,
        target: &Target,
        text: &str,
        until: &[herdr::AgentStatus],
        timeout: Option<Duration>,
    ) -> herdr::Result<Agent> {
        self.fake
            .prompt_and_wait(target, text, until, timeout)
            .await
    }
    async fn send_keys(&self, target: &Target, keys: &[&str]) -> herdr::Result<()> {
        self.fake.send_keys(target, keys).await
    }
    async fn wait(
        &self,
        target: &Target,
        until: &[herdr::AgentStatus],
        timeout: Option<Duration>,
    ) -> herdr::Result<Agent> {
        self.fake.wait(target, until, timeout).await
    }
    async fn start_agent(&self, spec: &StartAgent) -> herdr::Result<Agent> {
        self.fake.start_agent(spec).await
    }
    async fn workspaces(&self) -> herdr::Result<Vec<Workspace>> {
        self.fake.workspaces().await
    }
    async fn create_workspace(
        &self,
        label: &str,
        cwd: Option<&Path>,
    ) -> herdr::Result<CreatedWorkspace> {
        self.fake.create_workspace(label, cwd).await
    }
    async fn create_tab(
        &self,
        workspace: &WorkspaceId,
        label: &str,
        cwd: Option<&Path>,
    ) -> herdr::Result<CreatedTab> {
        self.fake.create_tab(workspace, label, cwd).await
    }
    async fn split_pane(
        &self,
        pane: &PaneId,
        direction: SplitDirection,
        cwd: Option<&Path>,
    ) -> herdr::Result<Pane> {
        self.fake.split_pane(pane, direction, cwd).await
    }
    async fn read_pane(
        &self,
        pane: &PaneId,
        source: ReadSource,
        lines: Option<u32>,
    ) -> herdr::Result<Screen> {
        self.fake.read_pane(pane, source, lines).await
    }
    async fn run_in_pane(&self, pane: &PaneId, command: &str) -> herdr::Result<()> {
        self.fake.run_in_pane(pane, command).await
    }
    async fn send_pane_keys(&self, pane: &PaneId, keys: &[&str]) -> herdr::Result<()> {
        self.fake.send_pane_keys(pane, keys).await
    }
    async fn close_pane(&self, pane: &PaneId) -> herdr::Result<()> {
        self.fake.close_pane(pane).await
    }
    async fn close_tab(&self, tab: &TabId) -> herdr::Result<()> {
        self.fake.close_tab(tab).await
    }
    async fn close_workspace(&self, workspace: &WorkspaceId) -> herdr::Result<()> {
        self.fake.close_workspace(workspace).await
    }
    async fn subscribe(&self, subscriptions: &[Subscription]) -> herdr::Result<CuttableEvents> {
        if subscriptions.contains(&Subscription::PaneCreated) {
            self.lifecycle_subscriptions.fetch_add(1, Ordering::SeqCst);
        }
        Ok(CuttableEvents {
            inner: self.fake.subscribe(subscriptions).await?,
            cut: self.cut.subscribe(),
        })
    }
}

/// A stand-in for whisper-server: "hears" `heard <n> bytes`, reporting
/// 1.5 s of audio. A gated one holds every transcription until released.
#[derive(Debug)]
pub(crate) struct FakeWhisper {
    gate: Semaphore,
}

impl FakeWhisper {
    /// Answers at once.
    pub(crate) fn open() -> Arc<Self> {
        Arc::new(Self {
            gate: Semaphore::new(Semaphore::MAX_PERMITS),
        })
    }

    /// Answers only after [`release`](Self::release).
    pub(crate) fn gated() -> Arc<Self> {
        Arc::new(Self {
            gate: Semaphore::new(0),
        })
    }

    /// Lets one held transcription finish.
    pub(crate) fn release(&self) {
        self.gate.add_permits(1);
    }
}

impl Transcriber for FakeWhisper {
    fn transcribe(&self, audio: axum::body::Bytes) -> TranscribeFuture<'_> {
        Box::pin(async move {
            self.gate.acquire().await.expect("gate open").forget();
            Ok(Recognized {
                text: format!(" heard {} bytes ", audio.len()),
                duration: Some(Duration::from_millis(1500)),
            })
        })
    }
}

/// Fast pacing for tests: quick retries, and a resync interval long enough
/// that a test passing proves the events worked, not the safety net.
pub(crate) fn settings(static_dir: &Path) -> Settings {
    let mut server = ServerSettings::new(static_dir);
    server.stream = StreamSettings {
        ping_every: Duration::from_secs(30),
        write_timeout: Duration::from_secs(30),
        auth_timeout: Duration::from_secs(5),
    };
    let mut threads = ThreadSettings::new(None);
    threads.poll_every = Duration::from_millis(50);
    Settings {
        server,
        registry: RegistrySettings {
            resync_every: Duration::from_secs(3600),
            retry_min: Duration::from_millis(10),
            retry_max: Duration::from_millis(100),
            ..RegistrySettings::default()
        },
        stream_queue: 64,
        threads,
        actions: ActionSettings::new(static_dir.with_file_name("audit.jsonl")),
        spawn: SpawnSettings {
            shell: ShellWait {
                timeout: FLOW_WAIT,
                poll: Duration::from_millis(5),
            },
            ready_wait: FLOW_WAIT,
            listed_wait: FLOW_WAIT,
            poll: Duration::from_millis(10),
            ..SpawnSettings::new(None)
        },
        runbooks: RunbookSettings {
            poll: Duration::from_millis(20),
            shell_wait: Duration::from_millis(50),
            start_wait: FLOW_WAIT,
            ..RunbookSettings::new(Vec::new(), static_dir.with_file_name("state").as_path())
        },
        commands: bridge::commands::CommandSettings {
            shell_wait: Duration::from_millis(50),
            ..bridge::commands::CommandSettings::default()
        },
        pending: PendingSettings {
            poll_every: Duration::from_millis(50),
            feedback_wait: FLOW_WAIT,
            feedback_poll: Duration::from_millis(10),
            ..PendingSettings::default()
        },
        push: None,
        audio: AudioSettings::new(static_dir.with_file_name("audio"), FakeWhisper::open()),
        attachments: AttachmentSettings::new(static_dir.with_file_name("attachments")),
        tldr: TldrSettings {
            settle: Duration::ZERO,
            ..TldrSettings::default()
        },
        insecure_http: false,
        tls: None,
    }
}

/// Content of a file planted next to (outside) the static dir.
pub(crate) const SENTINEL: &str = "SENTINEL-outside-the-static-dir";

/// A fake PWA build in `<tmp>/web`: the shell, the service worker and one
/// asset, plus a sentinel file at `<tmp>/secret.txt` and symlinks that try
/// to reach it (`leak.txt`, `leakdir/`).
pub(crate) struct WebDist {
    tmp: tempfile::TempDir,
}

impl WebDist {
    /// The static dir.
    pub(crate) fn path(&self) -> PathBuf {
        self.tmp.path().join("web")
    }
}

pub(crate) fn web_dist() -> WebDist {
    let tmp = tempfile::tempdir().expect("tempdir");
    let web = tmp.path().join("web");
    fs::create_dir(&web).expect("web dir");
    fs::write(
        web.join("index.html"),
        "<!doctype html><title>shell</title>",
    )
    .expect("index");
    fs::write(web.join("sw.js"), "// service worker").expect("sw");
    fs::write(web.join("app-1234.js"), "export {}").expect("asset");
    fs::write(tmp.path().join("secret.txt"), SENTINEL).expect("sentinel");
    std::os::unix::fs::symlink("../secret.txt", web.join("leak.txt")).expect("file link");
    std::os::unix::fs::symlink("..", web.join("leakdir")).expect("dir link");
    WebDist { tmp }
}

/// A running bridge.
pub(crate) struct Bridge {
    pub(crate) herdr: Arc<CuttableHerdr>,
    pub(crate) addr: SocketAddr,
    trigger: ShutdownTrigger,
    task: JoinHandle<Result<(), bridge::Error>>,
    _dist: WebDist,
}

impl Bridge {
    /// Starts a bridge on 127.0.0.1 with [`settings`].
    pub(crate) async fn start(herdr: CuttableHerdr) -> Self {
        Self::start_with(herdr, |_| {}).await
    }

    /// Starts a bridge on 127.0.0.1, adjusting the settings first.
    pub(crate) async fn start_with(
        herdr: CuttableHerdr,
        adjust: impl FnOnce(&mut Settings),
    ) -> Self {
        Self::start_full(herdr, Arc::new(StubAuth), adjust).await
    }

    /// Starts a bridge on 127.0.0.1 with a custom authenticator.
    pub(crate) async fn start_full(
        herdr: CuttableHerdr,
        auth: Arc<dyn Authenticator>,
        adjust: impl FnOnce(&mut Settings),
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        Self::serve(herdr, auth, adjust, listener)
    }

    /// Starts a bridge serving HTTPS with `store`'s leaf on 127.0.0.1.
    pub(crate) async fn start_https(herdr: CuttableHerdr, store: Arc<CertStore>) -> Self {
        let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let config = store.server_config().expect("server config");
        let listener = TlsListener::new(tcp, config)
            .expect("listener")
            .into_https();
        Self::serve(
            herdr,
            Arc::new(StubAuth),
            |settings| settings.tls = Some(store),
            listener,
        )
    }

    fn serve<L>(
        herdr: CuttableHerdr,
        auth: Arc<dyn Authenticator>,
        adjust: impl FnOnce(&mut Settings),
        listener: L,
    ) -> Self
    where
        L: Listener<Addr = SocketAddr>,
        for<'a> SocketAddr: Connected<IncomingStream<'a, L>>,
    {
        let dist = web_dist();
        let mut settings = settings(&dist.path());
        adjust(&mut settings);
        let addr = listener.local_addr().expect("addr");
        let herdr = Arc::new(herdr);
        let (trigger, shutdown) = shutdown::channel();
        let task = tokio::spawn(bridge::run(
            Arc::clone(&herdr),
            auth,
            settings,
            listener,
            shutdown,
        ));
        Self {
            herdr,
            addr,
            trigger,
            task,
            _dist: dist,
        }
    }

    pub(crate) fn fake(&self) -> &FakeHerdr {
        &self.herdr.fake
    }

    /// One HTTP/1.1 request over a fresh connection.
    pub(crate) async fn http(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> HttpResponse {
        let stream = TcpStream::connect(self.addr).await.expect("connect");
        http(stream, method, path, headers).await
    }

    /// `POST path` with the test token and a JSON body.
    pub(crate) async fn post_authed(&self, path: &str, body: &str) -> HttpResponse {
        let stream = TcpStream::connect(self.addr).await.expect("connect");
        let auth = format!("Bearer {TOKEN}");
        request(
            stream,
            "POST",
            path,
            &[
                ("authorization", &auth),
                ("content-type", "application/json"),
            ],
            body,
        )
        .await
    }

    /// Any request with the test token, extra headers and a body.
    pub(crate) async fn send_authed(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> HttpResponse {
        let stream = TcpStream::connect(self.addr).await.expect("connect");
        let auth = format!("Bearer {TOKEN}");
        let mut all = vec![("authorization", auth.as_str())];
        all.extend_from_slice(headers);
        request(stream, method, path, &all, body).await
    }

    /// `GET path` with the test token.
    pub(crate) async fn get_authed(&self, path: &str) -> HttpResponse {
        self.http(
            "GET",
            path,
            &[("authorization", &format!("Bearer {TOKEN}"))],
        )
        .await
    }

    /// `GET /v1/agents`, parsed.
    pub(crate) async fn agents(&self) -> Vec<protocol::Agent> {
        let response = self.get_authed("/v1/agents").await;
        assert_eq!(response.status, 200, "{}", response.body);
        serde_json::from_str(&response.body).expect("agent list")
    }

    /// Polls `GET /v1/agents` until `done` holds.
    pub(crate) async fn wait_for_agents(
        &self,
        what: &str,
        done: impl Fn(&[protocol::Agent]) -> bool,
    ) -> Vec<protocol::Agent> {
        let poll = async {
            loop {
                let agents = self.agents().await;
                if done(&agents) {
                    return agents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(PATIENCE, poll)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// Opens `WS /v1/stream` with the test token.
    pub(crate) async fn stream(&self) -> Stream<TcpStream> {
        let tcp = TcpStream::connect(self.addr).await.expect("connect");
        Stream::connect(tcp, Some(TOKEN)).await.expect("handshake")
    }

    /// Triggers shutdown and waits for `run` to return.
    pub(crate) async fn stop(self) -> Result<(), bridge::Error> {
        self.trigger.trigger();
        tokio::time::timeout(PATIENCE, self.task)
            .await
            .expect("bridge stops in time")
            .expect("bridge task")
    }
}

/// A raw HTTP response.
#[derive(Debug)]
pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl HttpResponse {
    /// The first value of header `name` (case-insensitive).
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends one request with `Connection: close` and reads the whole reply.
/// Bodies in these tests always come with a `Content-Length`.
pub(crate) async fn http<S>(
    stream: S,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> HttpResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    request(stream, method, path, headers, "").await
}

/// Like [`http`], with a request body (sent with its `Content-Length`).
pub(crate) async fn request<S>(
    mut stream: S,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: impl AsRef<[u8]>,
) -> HttpResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = body.as_ref();
    let mut request = format!("{method} {path} HTTP/1.1\r\nhost: bridge\r\nconnection: close\r\n");
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    if !body.is_empty() {
        request = request + "content-length: " + &body.len().to_string() + "\r\n";
    }
    request.push_str("\r\n");
    let mut request = request.into_bytes();
    request.extend_from_slice(body);
    stream.write_all(&request).await.expect("send");
    let mut raw = Vec::new();
    tokio::time::timeout(PATIENCE, stream.read_to_end(&mut raw))
        .await
        .expect("reply in time")
        .expect("read");
    let raw = String::from_utf8(raw).expect("utf-8 reply");
    let (head, body) = raw.split_once("\r\n\r\n").expect("header end");
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    HttpResponse {
        status,
        headers,
        body: body.to_owned(),
    }
}

/// A client of `WS /v1/stream`.
pub(crate) struct Stream<S> {
    pub(crate) ws: WebSocketStream<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Stream<S> {
    /// Performs the upgrade, then sends the `StreamAuth` message if `token`
    /// is given.
    pub(crate) async fn connect(io: S, token: Option<&str>) -> Result<Self, tungstenite::Error> {
        let mut stream = Self::upgrade(io).await?;
        if let Some(token) = token {
            stream.send_auth(token).await;
        }
        Ok(stream)
    }

    /// Performs the upgrade only.
    pub(crate) async fn upgrade(io: S) -> Result<Self, tungstenite::Error> {
        let request = "ws://bridge/v1/stream".into_client_request()?;
        let (ws, _) = tokio_tungstenite::client_async(request, io).await?;
        Ok(Self { ws })
    }

    /// Sends a `StreamAuth` message.
    pub(crate) async fn send_auth(&mut self, token: &str) {
        let auth = serde_json::to_string(&protocol::StreamAuth {
            token: token.to_owned(),
        })
        .expect("serialize");
        self.ws
            .send(Message::Text(auth.into()))
            .await
            .expect("send auth");
    }

    /// The next event, skipping control frames.
    pub(crate) async fn next_event(&mut self) -> AgentEvent {
        tokio::time::timeout(PATIENCE, self.next_event_inner())
            .await
            .expect("event in time")
    }

    async fn next_event_inner(&mut self) -> AgentEvent {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(&text).expect("valid event");
                }
                Some(Ok(Message::Close(frame))) => panic!("stream closed: {frame:?}"),
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("stream error: {error}"),
                None => panic!("stream ended"),
            }
        }
    }

    /// Reads until `matches` accepts an event, returning it.
    pub(crate) async fn wait_for(
        &mut self,
        what: &str,
        matches: impl Fn(&AgentEvent) -> bool,
    ) -> AgentEvent {
        let wait = async {
            loop {
                let event = self.next_event_inner().await;
                if matches(&event) {
                    return event;
                }
            }
        };
        tokio::time::timeout(PATIENCE, wait)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// The close frame's code, once the server closes (control frames and
    /// events before it are skipped).
    pub(crate) async fn close_code(&mut self) -> Option<u16> {
        let wait = async {
            loop {
                match self.ws.next().await {
                    Some(Ok(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return None,
                }
            }
        };
        tokio::time::timeout(PATIENCE, wait)
            .await
            .expect("close in time")
    }

    /// Politely closes the connection.
    pub(crate) async fn close(mut self) {
        let _ = self.ws.send(Message::Close(None)).await;
    }
}

/// An in-memory listener: each [`DuplexListener::connect`] is one accepted
/// connection, with a pipe of `buffer` bytes in each direction. A tiny
/// buffer makes backpressure deterministic, which loopback TCP's large
/// socket buffers do not.
pub(crate) struct DuplexListener {
    accepted: mpsc::UnboundedReceiver<DuplexStream>,
}

/// Opens connections to a [`DuplexListener`].
#[derive(Clone)]
pub(crate) struct DuplexConnector {
    tx: mpsc::UnboundedSender<DuplexStream>,
    buffer: usize,
}

const DUPLEX_PEER: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 40_000);

/// A [`DuplexListener`] as axum serves it: `TapIo` gives it the
/// `ConnectInfo<SocketAddr>` the bridge expects.
pub(crate) type ServedDuplex = TapIo<DuplexListener, fn(&mut DuplexStream)>;

pub(crate) fn duplex_listener(buffer: usize) -> (ServedDuplex, DuplexConnector) {
    fn untouched(_: &mut DuplexStream) {}
    let (tx, accepted) = mpsc::unbounded_channel();
    let listener = DuplexListener { accepted }.tap_io(untouched as fn(&mut DuplexStream));
    (listener, DuplexConnector { tx, buffer })
}

impl DuplexConnector {
    pub(crate) fn connect(&self) -> DuplexStream {
        let (client, server) = tokio::io::duplex(self.buffer);
        self.tx.send(server).expect("listener alive");
        client
    }
}

impl Listener for DuplexListener {
    type Io = DuplexStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (DuplexStream, SocketAddr) {
        match self.accepted.recv().await {
            Some(io) => (io, DUPLEX_PEER),
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(DUPLEX_PEER)
    }
}
