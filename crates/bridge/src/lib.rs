//! `passporter-bridge`: the service on the dev workstation that wraps herdr
//! in a small, typed, authenticated API and serves the PWA (design §3, §5).
//!
//! - [`config`]: the mode-600 config file in `~/.config/passporter/`.
//! - [`registry`]: herdr's agents as protocol types, kept live from events.
//! - [`server`]: routes, auth seam, the event stream and static files.
//! - [`auth`]: device pairing, signed sessions, revocation (design §8).
//! - [`threads`]: agent threads from Claude transcripts.
//! - [`pending`]: what blocked agents are asking, parsed from the screen.
//! - [`tldr`]: TL;DRs of long turns, generated eagerly and cached.
//! - [`actions`]: `POST /v1/actions` (idempotency, stale guard, policy, audit).
//! - [`spawn`]: `POST /v1/agents`, new agents from the phone's project allowlist.
//! - [`runbooks`]: `POST /v1/runbooks/:name/run`, preconfigured operations.
//! - [`commands`]: terminal mode: commands the agent asks the user to run.
//! - [`push`]: content-free Web Push wake pings and device subscriptions.
//! - [`attachments`]: photo uploads, stripped of metadata, for prompts.
//! - [`audio`]: dictation uploads, the transcription queue and retention.
//! - [`control`]: the herdr calls handlers make.
//! - [`tls`]: HTTPS for the edge, with leaf reloads.
//! - [`doctor`]: the read-only deployment self-check behind `passporter doctor`.
//! - [`version`]: this build's commit and build time.
//! - [`run`] wires them together; the binary adds signals and logging.
//!
//! The wire types live in `crates/protocol`; this crate never redefines them.

pub mod actions;
pub mod attachments;
pub mod audio;
pub mod auth;
pub mod commands;
pub mod config;
pub mod control;
pub mod doctor;
mod one_line;
pub mod pending;
pub mod push;
pub mod registry;
pub mod runbooks;
pub mod server;
pub mod shutdown;
pub mod spawn;
pub mod threads;
pub mod tldr;
pub mod tls;
pub mod version;

mod private;
#[cfg(test)]
mod testing;

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::connect_info::Connected;
use axum::serve::{IncomingStream, Listener};
use herdr::Herdr;

use crate::actions::{ActionSettings, Actions};
use crate::attachments::{AttachmentSettings, Attachments};
use crate::audio::{Audio, AudioSettings};
use crate::commands::{CommandDeps, CommandSettings, Commands};
use crate::config::DEFAULT_SPAWN_WORKSPACE;
use crate::config::{Config, PushConfig};
use crate::control::{AgentControl, HerdrControl};
use crate::pending::{PendingSettings, Pendings};
use crate::push::{
    HerdrLookup, HttpSender, PlacementLookup, Push, PushHosts, PushSettings, VapidKey,
};
use crate::registry::{Hub, QuietDone, RegistrySettings};
use crate::runbooks::{RunbookDeps, RunbookSettings, Runbooks};
use crate::server::{AppState, Authenticator, ServerSettings, Services};
use crate::shutdown::Shutdown;
use crate::spawn::tabs::Tabs;
use crate::spawn::{SpawnControl, SpawnSettings, Spawner};
use crate::threads::{ThreadSettings, Threads};
use crate::tldr::{TldrDeps, TldrSettings, Tldrs};
use crate::tls::CertStore;

/// How many events a stream client may fall behind before it is resynced
/// with a snapshot.
pub const DEFAULT_STREAM_QUEUE: usize = 64;

/// Everything [`run`] needs besides herdr, auth and a listener.
#[derive(Debug, Clone)]
pub struct Settings {
    /// HTTP settings.
    pub server: ServerSettings,
    /// herdr pacing.
    pub registry: RegistrySettings,
    /// Per-connection stream queue, in events.
    pub stream_queue: usize,
    /// Thread readers.
    pub threads: ThreadSettings,
    /// The actions endpoint.
    pub actions: ActionSettings,
    /// Starting agents from the phone (`[spawn]`; off without it).
    pub spawn: SpawnSettings,
    /// Runbooks the phone may run (`[[runbooks]]`; none without them).
    pub runbooks: RunbookSettings,
    /// Terminal mode: commands from the agent's latest message.
    pub commands: CommandSettings,
    /// Pending-input parsing.
    pub pending: PendingSettings,
    /// Web Push; `None` turns it off (the push routes answer 404).
    pub push: Option<PushSettings>,
    /// Dictation.
    pub audio: AudioSettings,
    /// Photo attachments.
    pub attachments: AttachmentSettings,
    /// TL;DRs; disabled unless a summarizer is set (the binary sets one).
    pub tldr: TldrSettings,
    /// Plain HTTP off loopback, by explicit opt-in (MVP mode): the binary
    /// logs a loud warning when set.
    pub insecure_http: bool,
    /// The leaf to serve HTTPS with (`[tls]`); `None` serves plain HTTP.
    /// The binary wraps its listener in a [`tls::TlsListener`] when set.
    pub tls: Option<Arc<CertStore>>,
}

/// How the bridge may serve a config ([`transport`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// HTTPS (`[tls]`), on any address.
    Tls,
    /// Plain HTTP on loopback.
    Loopback,
    /// Plain HTTP off loopback by the `WireGuard` opt-in (MVP mode).
    PlainOverWireguard,
}

/// Why the bridge could not start or keep serving.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The config is unusable.
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    /// The TLS leaf could not be loaded.
    #[error(transparent)]
    Tls(#[from] tls::TlsError),
    /// Plain HTTP was asked for on a non-loopback address.
    #[error(
        "refusing plain HTTP on {0}: off loopback the bridge requires TLS (design §8); \
         add a [tls] table (docs/deploy.md), listen on 127.0.0.1, or opt in to MVP mode \
         with insecure_http_over_wireguard = true and the IP in insecure_http_allow"
    )]
    PlaintextOffLoopback(SocketAddr),
    /// The `WireGuard` opt-in is set, but `listen` is not one of the allowed IPs.
    #[error(
        "refusing plain HTTP on {0}: insecure_http_over_wireguard only permits the IPs \
         in insecure_http_allow (never 0.0.0.0 or ::)"
    )]
    PlaintextNotAllowed(SocketAddr),
    /// The VAPID key could not be loaded.
    #[error(transparent)]
    Vapid(#[from] push::VapidError),
    /// The push subscription store could not be opened.
    #[error(transparent)]
    PushStore(#[from] push::StoreError),
    /// The HTTPS client for push services could not be built.
    #[error(transparent)]
    PushSender(#[from] push::SendError),
    /// The audio directory could not be opened.
    #[error("opening the audio directory: {0}")]
    Audio(#[source] io::Error),
    /// The attachments directory could not be opened.
    #[error("opening the attachments directory: {0}")]
    Attachments(#[source] io::Error),
    /// The HTTP server failed.
    #[error("serving HTTP: {0}")]
    Serve(#[source] io::Error),
}

impl Settings {
    /// Settings from a loaded config, with default pacing. Fails if the
    /// config asks for a transport the bridge cannot serve safely, or if
    /// a key (TLS, VAPID) does not load.
    pub fn from_config(config: &Config) -> Result<Self, Error> {
        let insecure_http = transport(config)? == Transport::PlainOverWireguard;
        let tls = config
            .tls
            .as_ref()
            .map(CertStore::load)
            .transpose()?
            .map(Arc::new);
        let mut server = ServerSettings::new(&config.static_dir);
        server.trusted_proxy = config.trusted_proxy;
        let projects_dir = config.claude_projects_dir.clone().or_else(|| {
            transcripts::Locator::from_env()
                .ok()
                .map(|locator| locator.projects_dir().to_path_buf())
        });
        let push = match &config.push {
            Some(push) => {
                let mut settings = PushSettings::new(
                    Arc::new(VapidKey::load(&push.vapid_key)?),
                    push.subject.clone(),
                    config.state_dir.join(push::store::STORE_FILE),
                    Arc::new(HttpSender::new()?),
                );
                settings.hosts =
                    PushHosts::new(push.extra_hosts.clone()).with_ntfy(push.ntfy_host.clone());
                settings.car_announce = push.car_announce_done;
                settings.quiet_done = quiet_done(push);
                Some(settings)
            }
            None => None,
        };
        Ok(Self {
            server,
            registry: RegistrySettings {
                quiet_done: config.push.as_ref().map(quiet_done).unwrap_or_default(),
                ..RegistrySettings::default()
            },
            stream_queue: DEFAULT_STREAM_QUEUE,
            threads: ThreadSettings {
                codex_sessions_dir: config.codex_sessions_dir.clone().or_else(|| {
                    transcripts::CodexLocator::from_env()
                        .ok()
                        .map(|locator| locator.sessions_dir().to_path_buf())
                }),
                ..ThreadSettings::new(projects_dir)
            },
            actions: ActionSettings::new(&config.audit_log),
            spawn: SpawnSettings::new(config.spawn.clone()),
            runbooks: RunbookSettings::new(config.runbooks.clone(), &config.state_dir),
            commands: CommandSettings::default(),
            pending: PendingSettings {
                hook_socket: pending::hook::default_socket_path(),
                ..PendingSettings::default()
            },
            push,
            audio: AudioSettings::from_config(&config.audio, &config.state_dir),
            attachments: AttachmentSettings::new(config.state_dir.join(attachments::DIR)),
            tldr: TldrSettings {
                // A backstop: the summarizer's own timeout (which kills and
                // reaps `claude -p`) is `config.tldr.timeout` and fires first.
                timeout: config.tldr.timeout.saturating_add(tldr::KILL_GRACE),
                ..TldrSettings::default()
            },
            insecure_http,
            tls,
        })
    }
}

/// The agents whose finished turns are quiet, from `[push]`: the push
/// dispatcher and the agent list's `quietDone` flag share it.
fn quiet_done(push: &PushConfig) -> QuietDone {
    QuietDone {
        workspaces: push.quiet_done_workspaces.clone(),
        agent_prefixes: push.quiet_done_agent_prefixes.clone(),
    }
}

/// The transport policy: whether and how the bridge can serve `config`.
/// HTTPS whenever `[tls]` is set (the config refuses the plain-HTTP opt-ins
/// alongside it); plain HTTP on loopback; elsewhere only with the
/// `WireGuard` opt-in and the exact IP allowlisted. Shared with
/// `passporter doctor` ([`doctor`]).
pub(crate) fn transport(config: &Config) -> Result<Transport, Error> {
    if config.tls.is_some() {
        return Ok(Transport::Tls);
    }
    let ip: IpAddr = config.listen.ip();
    if ip.is_loopback() {
        return Ok(Transport::Loopback);
    }
    if !config.insecure_http_over_wireguard {
        return Err(Error::PlaintextOffLoopback(config.listen));
    }
    if ip.is_unspecified() || !config.insecure_http_allow.contains(&ip) {
        return Err(Error::PlaintextNotAllowed(config.listen));
    }
    Ok(Transport::PlainOverWireguard)
}

/// Opens the attachments directory, off the async threads.
async fn open_attachments(settings: AttachmentSettings) -> Result<Arc<Attachments>, Error> {
    tokio::task::spawn_blocking(move || Attachments::open(settings))
        .await
        .map_err(|error| Error::Attachments(io::Error::other(error)))?
        .map(Arc::new)
        .map_err(Error::Attachments)
}

/// The settings of what the phone starts in herdr.
struct LauncherSettings {
    spawn: SpawnSettings,
    runbooks: RunbookSettings,
    commands: CommandSettings,
}

/// What the phone starts in herdr: agents, runbook runs and command
/// terminals.
struct Launchers {
    runbooks: Arc<Runbooks>,
    commands: Arc<Commands>,
    spawner: Arc<Spawner>,
}

/// Builds the [`Launchers`], which share one workspace's tabs (the
/// `[spawn] workspace`) and the actions' audit log.
fn launchers(
    settings: LauncherSettings,
    control: Arc<dyn SpawnControl>,
    hub: &Arc<Hub>,
    actions: &Actions,
) -> Launchers {
    let workspace = settings
        .spawn
        .config
        .as_ref()
        .map_or(DEFAULT_SPAWN_WORKSPACE, |spawn| spawn.workspace.as_str());
    let tabs = Arc::new(Tabs::new(Arc::clone(&control), workspace));
    let runbooks = Arc::new(Runbooks::new(
        settings.runbooks,
        RunbookDeps {
            control: Arc::clone(&control),
            tabs: Arc::clone(&tabs),
            hub: Arc::clone(hub),
            audit: actions.audit(),
        },
    ));
    let commands = Arc::new(Commands::new(
        settings.commands,
        CommandDeps {
            control: Arc::clone(&control),
            tabs: Arc::clone(&tabs),
            audit: actions.audit(),
        },
    ));
    let spawner = Arc::new(Spawner::new(settings.spawn, control, actions.audit(), tabs));
    Launchers {
        runbooks,
        commands,
        spawner,
    }
}

/// Runs the bridge on `listener` until `shutdown`: the registry task
/// following `herdr`, and the HTTP server in front of it.
pub async fn run<H, L>(
    herdr: Arc<H>,
    auth: Arc<dyn Authenticator>,
    settings: Settings,
    listener: L,
    shutdown: Shutdown,
) -> Result<(), Error>
where
    H: Herdr + 'static,
    L: Listener,
    L::Addr: fmt::Debug,
    for<'a> SocketAddr: Connected<IncomingStream<'a, L>>,
{
    let index = settings.server.static_dir.join("index.html");
    if !tokio::fs::try_exists(&index).await.unwrap_or(false) {
        tracing::warn!(path = %index.display(), "no PWA build found; only the API will work");
    }
    let audio = Arc::new(Audio::open(settings.audio).await.map_err(Error::Audio)?);
    let attachments = open_attachments(settings.attachments).await?;
    let hub = Arc::new(Hub::new(settings.stream_queue));
    let registry = registry::spawn(
        Arc::clone(&herdr),
        Arc::clone(&hub),
        settings.registry,
        shutdown.clone(),
    );
    let threads = Arc::new(Threads::new(settings.threads, Arc::clone(&hub)));
    let poller = threads.spawn_poller(shutdown.clone());
    let spawn_control: Arc<dyn SpawnControl> = Arc::new(HerdrControl(Arc::clone(&herdr)));
    let lookup: Arc<dyn PlacementLookup> = Arc::new(HerdrLookup(Arc::clone(&herdr)));
    let control: Arc<dyn AgentControl> = Arc::new(HerdrControl(herdr));
    let pending = Arc::new(Pendings::new(
        settings.pending,
        Arc::clone(&control),
        Arc::clone(&hub),
    ));
    let pending_poller = pending.spawn_poller(shutdown.clone());
    let push = settings.push.map(Push::open).transpose()?.map(Arc::new);
    // The sweep of devices revoked while the bridge was down comes first,
    // so no ping reaches them.
    let forgetter = match &push {
        Some(push) => {
            push.spawn_forgetter(Arc::clone(&auth), shutdown.clone())
                .await
        }
        None => None,
    };
    let dispatcher = push
        .as_ref()
        .map(|push| push.spawn(&hub, lookup, shutdown.clone()));
    let transcriber = audio.spawn_worker(shutdown.clone());
    let sweeper = audio.spawn_sweeper(shutdown.clone());
    let photo_sweeper = attachments.spawn_sweeper(shutdown.clone());
    let tldrs = Arc::new(Tldrs::new(
        settings.tldr,
        TldrDeps {
            hub: Arc::clone(&hub),
            threads: Arc::clone(&threads),
            control: Arc::clone(&control),
        },
        shutdown.clone(),
    ));
    let tldr_watcher = tldrs.spawn_watcher(shutdown.clone());
    let actions = Arc::new(Actions::new(&settings.actions));
    let Launchers {
        runbooks,
        commands,
        spawner,
    } = launchers(
        LauncherSettings {
            spawn: settings.spawn,
            runbooks: settings.runbooks,
            commands: settings.commands,
        },
        spawn_control,
        &hub,
        &actions,
    );
    // Runs a previous bridge started (one may have restarted it) carry on;
    // picked up before serving, so no request can overlap one.
    runbooks.resume().await;
    let watching = Arc::clone(&runbooks);
    let services = Services {
        control,
        threads,
        actions,
        spawner,
        runbooks,
        commands,
        pending,
        push,
        audio,
        attachments,
        tldrs,
    };
    let state = AppState::new(hub, auth, services, settings.server, shutdown.clone());
    let served = server::serve(listener, state, shutdown).await;
    watching.stop();
    // Background tasks stop on the same signal; abort covers a server failure.
    for task in [registry, poller, pending_poller, transcriber, sweeper]
        .into_iter()
        .chain([photo_sweeper])
        .chain(dispatcher)
        .chain(forgetter)
        .chain(tldr_watcher)
    {
        task.abort();
        let _ = task.await;
    }
    served.map_err(Error::Serve)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_plain_http_is_allowed() {
        let config = Config::parse("listen = \"127.0.0.1:0\"\ntrusted_proxy = \"10.0.0.1\"")
            .expect("parses");
        let settings = Settings::from_config(&config).expect("allowed");
        assert_eq!(
            settings.server.trusted_proxy,
            Some("10.0.0.1".parse().expect("ip"))
        );
        let v6 = Config::parse("listen = \"[::1]:0\"").expect("parses");
        assert!(Settings::from_config(&v6).is_ok());
    }

    #[test]
    fn plain_http_off_loopback_is_refused() {
        let config = Config::parse("listen = \"192.0.2.10:8443\"").expect("parses");
        assert!(matches!(
            Settings::from_config(&config),
            Err(Error::PlaintextOffLoopback(_))
        ));
    }

    #[test]
    fn plain_http_over_wireguard_needs_the_opt_in_and_the_ip() {
        let opted_in = Config::parse(
            "listen = \"192.0.2.10:8443\"\ninsecure_http_over_wireguard = true\n\
             insecure_http_allow = [\"192.0.2.10\"]",
        )
        .expect("parses");
        let settings = Settings::from_config(&opted_in).expect("allowed");
        assert!(settings.insecure_http);

        let not_listed = Config::parse(
            "listen = \"192.0.2.11:8443\"\ninsecure_http_over_wireguard = true\n\
             insecure_http_allow = [\"192.0.2.10\"]",
        )
        .expect("parses");
        assert!(matches!(
            Settings::from_config(&not_listed),
            Err(Error::PlaintextNotAllowed(_))
        ));

        let listed_without_opt_in =
            Config::parse("listen = \"192.0.2.10:8443\"\ninsecure_http_allow = [\"192.0.2.10\"]")
                .expect("parses");
        assert!(matches!(
            Settings::from_config(&listed_without_opt_in),
            Err(Error::PlaintextOffLoopback(_))
        ));

        let wildcard = Config::parse(
            "listen = \"0.0.0.0:8443\"\ninsecure_http_over_wireguard = true\n\
             insecure_http_allow = [\"0.0.0.0\"]",
        )
        .expect("parses");
        assert!(matches!(
            Settings::from_config(&wildcard),
            Err(Error::PlaintextNotAllowed(_))
        ));

        let loopback = Config::parse("listen = \"127.0.0.1:0\"").expect("parses");
        assert!(
            !Settings::from_config(&loopback)
                .expect("allowed")
                .insecure_http
        );
    }

    #[test]
    fn push_needs_a_loadable_vapid_key() {
        let dir = tempfile::tempdir().expect("tmp");
        let key = dir.path().join("vapid.pem");
        let text = format!(
            "listen = \"127.0.0.1:0\"\nstate_dir = \"{}\"\n[push]\nsubject = \"mailto:me@example.com\"\nvapid_key = \"{}\"\nextra_hosts = [\"push.example.org\"]\n\
             quiet_done_workspaces = [\"agent-swarm\"]\nquiet_done_agent_prefixes = [\"impl-\"]",
            dir.path().display(),
            key.display()
        );
        let config = Config::parse(&text).expect("parses");
        assert!(matches!(
            Settings::from_config(&config),
            Err(Error::Vapid(push::VapidError::Io { .. }))
        ));
        VapidKey::generate(&key).expect("generates");
        let settings = Settings::from_config(&config).expect("allowed");
        let push = settings.push.expect("push on");
        assert_eq!(push.store, dir.path().join(push::store::STORE_FILE));
        assert_eq!(
            push.hosts,
            PushHosts::new(vec!["push.example.org".parse().expect("host")])
        );
        assert!(
            push.quiet_done.workspaces.contains("agent-swarm"),
            "the configured workspace's finished turns are quiet"
        );
        assert!(push.quiet_done.agent_prefixes.contains("impl-"));
        assert_eq!(
            settings.registry.quiet_done, push.quiet_done,
            "the list's flag and the pings follow the same agents"
        );
        assert!(
            Settings::from_config(&Config::parse("listen = \"127.0.0.1:0\"").expect("parses"))
                .expect("allowed")
                .push
                .is_none()
        );
    }

    #[test]
    fn tls_serves_off_loopback_once_the_leaf_loads() {
        let config =
            Config::parse("listen = \"192.0.2.10:8443\"\n[tls]\ncert = \"/nonexistent/c\"\nkey = \"/nonexistent/k\"")
                .expect("parses");
        assert_eq!(transport(&config).expect("allowed"), Transport::Tls);
        assert!(matches!(
            Settings::from_config(&config),
            Err(Error::Tls(tls::TlsError::Io { .. }))
        ));
    }

    #[test]
    fn the_example_deploy_config_is_valid() {
        let text = include_str!("../../../deploy/bridge/config.example.toml");
        let config = Config::parse(text).expect("parses");
        assert!(config.static_token().is_none(), "pairing only");
        assert!(config.listen.ip().is_loopback());
        let settings = Settings::from_config(&config).expect("allowed");
        assert!(!settings.insecure_http);
    }

    /// The fenced `toml` blocks in `text`, with their line numbers: from
    /// Markdown (fences may be indented, as in a numbered list) or from a
    /// Rust file's `//!` module docs.
    fn toml_blocks(text: &str) -> Vec<(usize, String)> {
        let mut blocks = Vec::new();
        let mut open: Option<(usize, usize, String)> = None;
        for (n, line) in text.lines().enumerate() {
            let line = line
                .strip_prefix("//!")
                .map_or(line, |doc| doc.strip_prefix(' ').unwrap_or(doc));
            let fence = line.trim_start();
            let indent = line.len() - fence.len();
            match open.take() {
                None if fence == "```toml" => open = Some((n + 1, indent, String::new())),
                None => {}
                Some((start, _, block)) if fence == "```" => blocks.push((start, block)),
                Some((start, indent, mut block)) => {
                    block.push_str(line.get(indent..).unwrap_or(line.trim_start()));
                    block.push('\n');
                    open = Some((start, indent, block));
                }
            }
        }
        blocks
    }

    /// A documented value in angle brackets (`"<64 hex characters…>"`)
    /// becomes a real one of the same kind, so the snippet is parsed as a
    /// reader would fill it in.
    fn fill_placeholders(block: &str) -> String {
        let mut out = String::new();
        let mut rest = block;
        while let Some(start) = rest.find("\"<") {
            let Some(len) = rest[start..].find(">\"") else {
                break;
            };
            out.push_str(&rest[..start]);
            out.push('"');
            out.push_str(&"0123456789abcdef".repeat(4));
            out.push('"');
            rest = &rest[start + len + 2..];
        }
        out.push_str(rest);
        out
    }

    /// Every example config in the docs parses as the bridge would read
    /// it: an example that the bridge refuses (a clash, a field that was
    /// renamed, two settings that exclude each other) teaches a broken
    /// config.
    #[test]
    fn every_documented_config_example_parses() {
        let sources = [
            ("docs/deploy.md", include_str!("../../../docs/deploy.md")),
            ("crates/bridge/src/config.rs", include_str!("config.rs")),
            (
                "crates/bridge/src/config/runbooks.rs",
                include_str!("config/runbooks.rs"),
            ),
        ];
        let mut seen = 0;
        let mut broken = Vec::new();
        for (file, text) in sources {
            for (line, block) in toml_blocks(text) {
                seen += 1;
                // The strictest policy, so the examples hold in every build.
                let parsed = Config::parse_with(
                    &fill_placeholders(&block),
                    config::StaticTokenBehindHttps::Refuse,
                );
                if let Err(error) = parsed {
                    broken.push(format!("{file}:{line}: {error}\n{block}"));
                }
            }
        }
        assert!(
            seen >= 9,
            "found only {seen} examples: is the extraction broken?"
        );
        assert!(
            broken.is_empty(),
            "examples the bridge refuses:\n{}",
            broken.join("\n")
        );
    }

    #[test]
    fn toml_blocks_are_found_in_markdown_and_module_docs() {
        let md = "x\n1. step\n   ```toml\n   a = 1\n   ```\n```toml\nb = 2\n```\n```sh\nc\n```\n";
        assert_eq!(
            toml_blocks(md),
            [(3, "a = 1\n".to_owned()), (6, "b = 2\n".to_owned())]
        );
        let rs = "//! Doc.\n//!\n//! ```toml\n//! k = \"v\"  # c\n//!\n//! ```\nfn f() {}\n";
        assert_eq!(toml_blocks(rs), [(3, "k = \"v\"  # c\n\n".to_owned())]);
        assert_eq!(
            fill_placeholders("t = \"<64 hex>\" # <not this>\n"),
            format!("t = \"{}\" # <not this>\n", "0123456789abcdef".repeat(4))
        );
    }
}
