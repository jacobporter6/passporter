//! The real `passporter-bridge` binary, started on loopback with a
//! throwaway config dir, state dir and Claude projects dir.

use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::hash::BuildHasher;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// The bridge reads its hook socket path from this variable (see
/// `bridge::pending::hook::SOCKET_ENV`).
const HOOK_SOCKET_ENV: &str = "PASSPORTER_HOOK_SOCKET";

use tokio::process::{Child, Command};

/// How long the bridge may take to accept connections.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Why the bridge could not be started.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// Setting up its files or spawning it failed.
    #[error("{what}: {source}")]
    Io {
        /// What we were doing.
        what: String,
        /// Why it failed.
        source: std::io::Error,
    },
    /// It exited or never listened.
    #[error("the bridge did not start ({reason}); its log:\n{log}")]
    NotListening {
        /// What happened.
        reason: String,
        /// Its stderr so far.
        log: String,
    },
}

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> BridgeError {
    let what = what.into();
    move |source| BridgeError::Io { what, source }
}

/// What to start the bridge with.
#[derive(Debug)]
pub struct BridgeSpec<'a> {
    /// The `passporter-bridge` binary.
    pub binary: &'a Path,
    /// The PWA build (`crates/web/dist`).
    pub static_dir: &'a Path,
    /// The (fake) herdr socket.
    pub herdr_socket: &'a Path,
    /// Claude's projects dir, holding the agents' transcripts.
    pub projects_dir: &'a Path,
    /// The `claude` executable the summarizer runs.
    pub claude: &'a Path,
    /// Scratch space: config, state and the log go here.
    pub work_dir: &'a Path,
    /// The (fake) `whisper-server`, if dictation is transcribed.
    pub whisper: Option<SocketAddr>,
    /// Codex's sessions dir (`codex_sessions_dir`).
    pub codex_dir: Option<&'a Path>,
    /// Whether to configure Web Push (`[push]`, with a key made by
    /// `passporter-bridge push-keygen`), with [`crate::SWARM_WORKSPACE`]
    /// quiet when done.
    pub push: bool,
    /// The `[spawn]` allowlist: project names and folders.
    pub spawn_projects: &'a [(&'a str, &'a Path)],
    /// `[[runbooks]]`: name, folder and argv of each.
    pub runbooks: &'a [(&'a str, &'a Path, &'a [&'a str])],
}

/// A running bridge; killed when dropped.
#[derive(Debug)]
pub struct Bridge {
    child: Child,
    addr: SocketAddr,
    token: String,
    log: PathBuf,
    audit: PathBuf,
}

impl Bridge {
    /// Writes a config (loopback, static token opted in) and starts the
    /// bridge, returning once it accepts TCP connections.
    pub async fn start(spec: &BridgeSpec<'_>) -> Result<Self, BridgeError> {
        let addr = free_port()?;
        let token = random_token();
        let config_dir = spec.work_dir.join("config");
        let state_dir = spec.work_dir.join("state");
        for dir in [&config_dir, &state_dir] {
            std::fs::create_dir_all(dir).map_err(io(format!("creating {}", dir.display())))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(io(format!("chmod {}", dir.display())))?;
        }
        if spec.push {
            push_keygen(spec.binary, &config_dir).await?;
        }
        let config = render_config(spec, addr, &state_dir, &token);
        let config_file = config_dir.join("config.toml");
        std::fs::write(&config_file, config).map_err(io("writing config.toml"))?;
        std::fs::set_permissions(&config_file, std::fs::Permissions::from_mode(0o600))
            .map_err(io("chmod config.toml"))?;

        let log = spec.work_dir.join("bridge.log");
        let log_file = std::fs::File::create(&log).map_err(io("creating bridge.log"))?;
        let child = Command::new(spec.binary)
            .arg("--config-dir")
            .arg(&config_dir)
            .env("PASSPORTER_LOG", "info")
            // Never the live bridge's hook socket under $XDG_RUNTIME_DIR.
            .env(HOOK_SOCKET_ENV, spec.work_dir.join("hooks.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log_file)
            .kill_on_drop(true)
            .spawn()
            .map_err(io(format!("spawning {}", spec.binary.display())))?;
        let mut bridge = Self {
            child,
            addr,
            token,
            log,
            audit: state_dir.join("audit.jsonl"),
        };
        bridge.wait_listening().await?;
        Ok(bridge)
    }

    /// `http://<host>:<port>/` with `host` in place of the loopback IP.
    #[must_use]
    pub fn url_for_host(&self, host: &str) -> String {
        format!("http://{host}:{}/", self.addr.port())
    }

    /// The loopback address it listens on.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The static bearer token the config accepts.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Its action audit log so far: one JSON object per line, written
    /// before each action reaches herdr.
    #[must_use]
    pub fn audit(&self) -> String {
        std::fs::read_to_string(&self.audit).unwrap_or_default()
    }

    /// Its stderr so far, for failure messages.
    #[must_use]
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    async fn wait_listening(&mut self) -> Result<(), BridgeError> {
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(BridgeError::NotListening {
                    reason: format!("exited with {status}"),
                    log: self.log(),
                });
            }
            if tokio::net::TcpStream::connect(self.addr).await.is_ok() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BridgeError::NotListening {
                    reason: format!("nothing listening on {} after {START_TIMEOUT:?}", self.addr),
                    log: self.log(),
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Runs `passporter-bridge push-keygen`, as the deploy docs do, so the
/// config's `[push]` table has its VAPID key.
async fn push_keygen(binary: &Path, config_dir: &Path) -> Result<(), BridgeError> {
    let out = Command::new(binary)
        .arg("push-keygen")
        .arg("--config-dir")
        .arg(config_dir)
        .env("PASSPORTER_LOG", "warn")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(io("running push-keygen"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(BridgeError::NotListening {
            reason: format!("push-keygen exited with {}", out.status),
            log: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

fn render_config(spec: &BridgeSpec<'_>, addr: SocketAddr, state_dir: &Path, token: &str) -> String {
    // TOML basic strings: paths from tempfile have no quotes or backslashes,
    // and serde_json's escaping is a valid subset of TOML's.
    let quote = |path: &Path| serde_json::Value::from(path.to_string_lossy()).to_string();
    let mut text = String::new();
    let lines = [
        (
            "listen",
            serde_json::Value::from(addr.to_string()).to_string(),
        ),
        ("static_dir", quote(spec.static_dir)),
        ("herdr_socket", quote(spec.herdr_socket)),
        ("state_dir", quote(state_dir)),
        ("audit_log", quote(&state_dir.join("audit.jsonl"))),
        ("claude_projects_dir", quote(spec.projects_dir)),
        ("api_token", serde_json::Value::from(token).to_string()),
        ("allow_static_token", "true".to_owned()),
    ];
    for (key, value) in lines {
        let _ = writeln!(text, "{key} = {value}");
    }
    if let Some(codex) = spec.codex_dir {
        let _ = writeln!(text, "codex_sessions_dir = {}", quote(codex));
    }
    if spec.push {
        let _ = writeln!(
            text,
            "\n[push]\nsubject = \"mailto:e2e@example.com\"\nquiet_done_workspaces = [{}]",
            serde_json::Value::from(crate::SWARM_WORKSPACE)
        );
    }
    if let Some(whisper) = spec.whisper {
        let _ = writeln!(text, "\n[audio]\nwhisper = \"{whisper}\"");
    }
    if !spec.spawn_projects.is_empty() {
        let projects: Vec<String> = spec
            .spawn_projects
            .iter()
            .map(|(name, path)| {
                format!(
                    "{{ name = {}, path = {} }}",
                    serde_json::Value::from(*name),
                    quote(path)
                )
            })
            .collect();
        let _ = writeln!(text, "[spawn]\nprojects = [{}]", projects.join(", "));
    }
    for (name, cwd, argv) in spec.runbooks {
        let _ = writeln!(
            text,
            "\n[[runbooks]]\nname = {}\ncwd = {}\nargv = {}",
            serde_json::Value::from(*name),
            quote(cwd),
            serde_json::Value::from(argv.to_vec())
        );
    }
    let _ = writeln!(text, "\n[tldr]\nclaude = {}", quote(spec.claude));
    text
}

/// A loopback port nothing is using right now.
fn free_port() -> Result<SocketAddr, BridgeError> {
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(io("finding a free port"))?;
    listener.local_addr().map_err(io("finding a free port"))
}

/// 64 hex characters, unique per run (a throwaway credential for a
/// loopback bridge that lives for one test).
fn random_token() -> String {
    let mut out = String::with_capacity(64);
    for i in 0_u64..4 {
        let _ = write!(out, "{:016x}", RandomState::new().hash_one(i));
    }
    out
}
