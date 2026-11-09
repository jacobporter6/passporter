//! `passporter-bridge [--config-dir <dir>]`
//!
//! Loads `<dir>/config.toml` (default `~/.config/passporter/`, or
//! `$PASSPORTER_CONFIG_DIR`), follows herdr and serves the API and the PWA
//! until SIGINT or SIGTERM: over HTTPS when the config has `[tls]`, then
//! reloading the leaf on SIGHUP or when its files change. Logs go to
//! stderr; `PASSPORTER_LOG` sets the filter (e.g. `info`, `bridge=debug,info`).
//!
//! Requests authenticate with device sessions ([`DeviceAuth`]); the static
//! `api_token` is also accepted when the config opts in. The admin socket
//! that `passporter pair` and `passporter devices` talk to lives in the
//! state dir.
//!
//! `passporter-bridge push-keygen [--config-dir <dir>]` makes the Web Push
//! VAPID key, `<dir>/vapid.pem` (mode 0600; never overwritten), and prints
//! its public key.
//!
//! `passporter-bridge runbook <spec>` is the runbook runner: the line the
//! bridge types into a runbook's herdr tab (see `bridge::runbooks::runner`).
//! Not for people to call.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use bridge::auth::admin::{self, ADMIN_SOCKET, AdminError};
use bridge::auth::{AuthError, AuthSettings, DeviceAuth};
use bridge::config::{Config, TldrConfig};
use bridge::push::VapidKey;
use bridge::push::vapid::VAPID_KEY_FILE;
use bridge::server::StaticToken;
use bridge::shutdown::{self, ShutdownTrigger};
use bridge::tldr::SharedSummarizer;
use bridge::tls::{self, TlsListener};
use bridge::version::BUILD;
use bridge::{Error, Settings};
use herdr::SocketClient;
use summarize::{ClaudeCli, ClaudeConfig};
use tokio::net::TcpListener;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Environment variable holding the log filter.
const LOG_ENV: &str = "PASSPORTER_LOG";

/// herdr calls the bridge makes are quick reads; don't wait on a hung server.
const HERDR_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

const USAGE: &str = "usage: passporter-bridge [--config-dir <dir>]\n       passporter-bridge push-keygen [--config-dir <dir>]\n       passporter-bridge runbook <spec>   (typed into a herdr tab by the bridge)";

/// Why the binary could not start.
#[derive(Debug, thiserror::Error)]
enum Startup {
    #[error("{0}\n{USAGE}")]
    Usage(String),
    #[error(transparent)]
    Bridge(#[from] Error),
    #[error("no herdr socket: set herdr_socket in the config, or HERDR_SOCKET_PATH or HOME")]
    NoHerdrSocket,
    #[error(transparent)]
    Vapid(#[from] bridge::push::VapidError),
    #[error("device auth: {0}")]
    Auth(#[from] AuthError),
    #[error("admin socket: {0}")]
    Admin(#[from] AdminError),
    #[error("TLS: {0}")]
    Tls(#[from] tls::TlsError),
    #[error("cannot watch for SIGHUP (TLS reloads): {0}")]
    Hangup(std::io::Error),
    #[error("binding {addr}: {source}")]
    Bind {
        addr: std::net::SocketAddr,
        source: std::io::Error,
    },
}

/// The listener the bridge serves on.
enum Serving {
    /// Plain HTTP (loopback, or the `WireGuard` opt-in).
    Plain(TcpListener),
    /// HTTPS.
    Https(tls::HttpsListener),
}

/// The listener to serve on: HTTPS when `settings` has a TLS leaf, with
/// the task that reloads it on SIGHUP and file changes; otherwise plain
/// HTTP, with a task that logs and ignores SIGHUP.
fn serving(
    listener: TcpListener,
    settings: &Settings,
    listen: std::net::SocketAddr,
    hangups: Signal,
    shutdown: &shutdown::Shutdown,
) -> Result<(Serving, tokio::task::JoinHandle<()>), Startup> {
    let Some(store) = &settings.tls else {
        return Ok((
            Serving::Plain(listener),
            tokio::spawn(ignore_hangups(hangups)),
        ));
    };
    let listener = TlsListener::new(listener, store.server_config()?)
        .map_err(|source| Startup::Bind {
            addr: listen,
            source,
        })?
        .into_https();
    tracing::info!(
        expires = %store.current().expiry_text(),
        "serving HTTPS; the leaf reloads on SIGHUP or when its files change"
    );
    let reloader = tls::spawn_reloader(
        Arc::clone(store),
        tls::RELOAD_POLL,
        hangups,
        shutdown.clone(),
    );
    Ok((Serving::Https(listener), reloader))
}

/// SIGHUP without `[tls]`: nothing to reload, so say so and keep serving.
async fn ignore_hangups(mut hangups: Signal) {
    while hangups.recv().await.is_some() {
        tracing::info!("SIGHUP: nothing to reload (no [tls] in the config)");
    }
}

/// What the command line asked for.
enum Command {
    Run { config_dir: Option<PathBuf> },
    PushKeygen { config_dir: Option<PathBuf> },
    Help,
}

/// How long the runtime waits for leftover tasks (e.g. a repository lookup
/// stuck on a hung file system) after the bridge has stopped.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    // The runner first: it runs in a herdr pane, with no runtime or logs.
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|arg| arg == "runbook") {
        if let (Some(spec), None) = (args.next(), args.next()) {
            return bridge::runbooks::runner::main(std::path::Path::new(&spec));
        }
        print_usage();
        return ExitCode::FAILURE;
    }
    init_logging();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot start the async runtime");
            return ExitCode::FAILURE;
        }
    };
    let outcome = runtime.block_on(start());
    // Never hang on exit: blocking-pool work is abandoned after the timeout.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{}", error_chain(&error));
            ExitCode::FAILURE
        }
    }
}

async fn start() -> Result<(), Startup> {
    let config_dir = match parse_args(std::env::args_os().skip(1))? {
        Command::Help => {
            print_usage();
            return Ok(());
        }
        Command::PushKeygen { config_dir } => return push_keygen(config_dir),
        Command::Run { config_dir } => config_dir,
    };
    // First, so `systemctl --user reload` (ExecReload sends SIGHUP) can
    // never kill the bridge with the signal's default action, whether or
    // not there is a TLS leaf to reload.
    let hangups = signal(SignalKind::hangup()).map_err(Startup::Hangup)?;
    let dir = Config::locate_dir(config_dir).map_err(Error::from)?;
    let config = Config::load(&dir).map_err(Error::from)?;
    let mut settings = Settings::from_config(&config)?;
    settings.tldr.summarizer = summarizer(&config.tldr);
    let herdr = config
        .herdr_socket
        .clone()
        .map(SocketClient::new)
        .or_else(SocketClient::from_env)
        .ok_or(Startup::NoHerdrSocket)?
        .with_request_timeout(HERDR_REQUEST_TIMEOUT);
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|source| Startup::Bind {
            addr: config.listen,
            source,
        })?;
    tracing::info!(
        listen = %config.listen,
        scheme = if settings.tls.is_some() { "https" } else { "http" },
        herdr = %herdr.path().display(),
        static_dir = %settings.server.static_dir.display(),
        build = BUILD.sha,
        built = BUILD.built,
        "passporter-bridge starting"
    );
    if settings.insecure_http {
        tracing::warn!(
            listen = %config.listen,
            "MVP MODE: serving PLAIN HTTP off loopback (insecure_http_over_wireguard). \
             Traffic is only protected by WireGuard; make sure this address is \
             unreachable except through the tunnel"
        );
    }
    let static_token = config.static_token().cloned().map(StaticToken::new);
    if config.static_token_behind_https() {
        tracing::warn!(
            "the static api_token is accepted although the bridge is behind HTTPS ([tls] or \
             trusted_proxy), where phones can pair instead; builds that refuse this config \
             will not start. Pair every phone (`passporter pair`), then set \
             allow_static_token = false or remove api_token"
        );
    } else if static_token.is_some() {
        tracing::warn!(
            "MVP MODE: the static api_token is accepted alongside device sessions \
             (allow_static_token); remove it once every client pairs over HTTPS"
        );
    } else if config.api_token.is_some() {
        tracing::info!("api_token is refused (allow_static_token = false)");
    }
    let auth = Arc::new(DeviceAuth::open(
        &config.state_dir,
        AuthSettings::default(),
        static_token,
    )?);

    let (trigger, shutdown) = shutdown::channel();
    // Everything that can fail comes before the admin socket exists.
    let (serving, reloader) = serving(listener, &settings, config.listen, hangups, &shutdown)?;
    let admin = admin::spawn(
        Arc::clone(&auth),
        &config.state_dir.join(ADMIN_SOCKET),
        shutdown.clone(),
    )?;
    tokio::spawn(stop_on_signal(trigger));
    let herdr = Arc::new(herdr);
    // Two arms because `run` is generic over the listener type.
    let served = match serving {
        Serving::Https(listener) => bridge::run(herdr, auth, settings, listener, shutdown).await,
        Serving::Plain(listener) => bridge::run(herdr, auth, settings, listener, shutdown).await,
    };
    reloader.abort();
    // After a clean stop the admin socket has seen the same signal and
    // removes its file; after a failure nothing will signal it.
    if served.is_err() {
        admin.abort();
    }
    let _ = admin.await;
    served?;
    tracing::info!("passporter-bridge stopped");
    Ok(())
}

/// The hardened headless `claude -p` summarizer (design §6.4), one run at
/// a time, or `None` when `[tldr] enabled = false`.
fn summarizer(config: &TldrConfig) -> Option<SharedSummarizer> {
    if !config.enabled {
        tracing::info!("TL;DRs disabled ([tldr] enabled = false)");
        return None;
    }
    let cli = ClaudeCli::new(ClaudeConfig {
        program: config.claude.clone(),
        timeout: config.timeout,
        concurrency: 1,
        ..ClaudeConfig::default()
    });
    Some(SharedSummarizer::new(Arc::new(cli)))
}

#[expect(clippy::print_stdout, reason = "--help output belongs on stdout")]
fn print_usage() {
    println!("{USAGE}");
}

/// Writes a new VAPID key into the config dir and prints its public half.
#[expect(
    clippy::print_stdout,
    reason = "the public key is the command's output"
)]
fn push_keygen(config_dir: Option<PathBuf>) -> Result<(), Startup> {
    let dir = Config::locate_dir(config_dir).map_err(Error::from)?;
    let path = dir.join(VAPID_KEY_FILE);
    let key = VapidKey::generate(&path)?;
    tracing::info!(path = %path.display(), "VAPID key written; add a [push] table with a subject to config.toml");
    println!("{}", key.public_key());
    Ok(())
}

fn parse_args(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<Command, Startup> {
    let mut config_dir = None;
    let mut keygen = false;
    let mut first = true;
    while let Some(arg) = args.next() {
        let at_start = std::mem::replace(&mut first, false);
        match arg.to_str() {
            Some("-h" | "--help") => return Ok(Command::Help),
            Some("push-keygen") if at_start => keygen = true,
            Some("--config-dir") => {
                let dir = args
                    .next()
                    .ok_or_else(|| Startup::Usage("--config-dir needs a value".to_owned()))?;
                config_dir = Some(PathBuf::from(dir));
            }
            _ => {
                return Err(Startup::Usage(format!(
                    "unexpected argument {}",
                    arg.to_string_lossy()
                )));
            }
        }
    }
    Ok(if keygen {
        Command::PushKeygen { config_dir }
    } else {
        Command::Run { config_dir }
    })
}

/// Triggers shutdown on SIGINT or SIGTERM. A signal whose handler cannot be
/// installed is logged and ignored, never taken as a request to stop.
async fn stop_on_signal(trigger: ShutdownTrigger) {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "cannot listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
    trigger.trigger();
}

fn init_logging() {
    let filter = std::env::var(LOG_ENV)
        .ok()
        .and_then(|spec| spec.parse::<Targets>().ok())
        .unwrap_or_else(|| Targets::new().with_default(tracing::Level::INFO));
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(filter)
        .init();
}

/// An error and its sources, one per clause.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}
