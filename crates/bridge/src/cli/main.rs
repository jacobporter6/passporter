//! `passporter`: the operator's side of device auth (design §8).
//!
//! ```text
//! passporter [--config-dir <dir>] pair [--name <name>]
//! passporter [--config-dir <dir>] devices list
//! passporter [--config-dir <dir>] devices revoke <device-id>
//! passporter [--config-dir <dir>] doctor
//! ```
//!
//! `pair` and `devices` talk to the running `passporter-bridge` over its
//! admin socket in the state dir (see `bridge::auth::admin`), so the config
//! is read only to find that dir. `doctor` is a read-only self-check of the
//! deployment (see `bridge::doctor`); it exits non-zero if a check fails.

mod args;
mod render;

use std::io::{self, Write};
use std::process::ExitCode;

use bridge::auth::admin::{self, ADMIN_SOCKET, AdminError, AdminRequest, AdminResponse};
use bridge::config::{Config, ConfigError};
use bridge::doctor::{self, PROBE_TIMEOUT, Systemctl};
use herdr::SocketClient;

use self::args::{Command, USAGE};

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error("{0}\n{USAGE}")]
    Usage(String),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{0} (is passporter-bridge running?)")]
    Admin(#[from] AdminError),
    #[error("the bridge refused: {0}")]
    Refused(String),
    #[error("unexpected answer from the bridge")]
    Unexpected,
    #[error("{0} doctor check(s) failed")]
    Unhealthy(usize),
    #[error("writing output: {0}")]
    Output(#[from] io::Error),
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error),
    };
    match runtime.block_on(run(std::env::args_os().skip(1))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}

fn fail(error: &dyn std::error::Error) -> ExitCode {
    let _ = writeln!(io::stderr(), "passporter: {error}");
    ExitCode::FAILURE
}

async fn run(args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), Failure> {
    let (config_dir, command) = args::parse(args).map_err(Failure::Usage)?;
    let mut out = io::stdout().lock();
    let request = match command {
        Command::Help => {
            writeln!(out, "{USAGE}")?;
            return Ok(());
        }
        Command::Pair { name } => AdminRequest::Pair { name },
        Command::List => AdminRequest::Devices {},
        Command::Revoke { id } => AdminRequest::Revoke { id },
        Command::Doctor => {
            let dir = Config::locate_dir(config_dir)?;
            let connect = |socket: &std::path::Path| {
                SocketClient::new(socket).with_request_timeout(PROBE_TIMEOUT)
            };
            let report = doctor::run(&dir, connect, &Systemctl, PROBE_TIMEOUT).await;
            write!(out, "{report}")?;
            out.flush()?;
            return if report.healthy() {
                Ok(())
            } else {
                Err(Failure::Unhealthy(report.failures()))
            };
        }
    };
    let dir = Config::locate_dir(config_dir)?;
    let config = Config::load(&dir)?;
    let socket = config.state_dir.join(ADMIN_SOCKET);
    match admin::send(&socket, &request).await? {
        AdminResponse::Error { message } => Err(Failure::Refused(message)),
        AdminResponse::Paired {
            code,
            expires_in_secs,
        } if matches!(request, AdminRequest::Pair { .. }) => {
            out.write_all(render::pairing(&code, expires_in_secs).as_bytes())?;
            Ok(())
        }
        AdminResponse::Devices { devices } if request == (AdminRequest::Devices {}) => {
            out.write_all(render::devices(&devices, render::now_millis()).as_bytes())?;
            Ok(())
        }
        AdminResponse::Revoked { sessions } => {
            if let AdminRequest::Revoke { id } = &request {
                writeln!(
                    out,
                    "Revoked {id}: {sessions} session(s) ended, open streams closed."
                )?;
                Ok(())
            } else {
                Err(Failure::Unexpected)
            }
        }
        AdminResponse::Paired { .. } | AdminResponse::Devices { .. } => Err(Failure::Unexpected),
    }
}
