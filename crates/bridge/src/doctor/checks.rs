//! The individual checks behind [`super::run`]. Each returns a [`Finding`]
//! and changes nothing.

use std::fs::{self, Metadata};
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use herdr::Herdr;
use tokio::net::TcpStream;

use super::{Check, Finding, UnitState, printable, shell_quote};
use crate::Transport;
use crate::config::{Config, ConfigError, is_private_mode as is_private};
use crate::push::{VapidError, VapidKey};
use crate::tls::{EXPIRY_WARNING, Leaf, TlsError};

/// herdr's default session socket, relative to `HOME`.
const DEFAULT_HERDR_SOCKET: &str = ".config/herdr/herdr.sock";

/// How to replace an expiring leaf.
const RENEW: &str = "renew the leaf from your CA and install it over the old files: the \
                     bridge picks it up within two minutes, or at once on \
                     systemctl --user reload passporter-bridge";

/// Where the switch-over to TLS is written up.
const SWITCH_OVER: &str = "docs/deploy.md, \"HTTPS\"";

fn mode_of(meta: &Metadata) -> u32 {
    meta.permissions().mode() & 0o7777
}

/// `path is 0700`, or a failure with the `chmod` that fixes it.
fn mode_finding(check: Check, path: &Path, mode: u32, want: u32) -> Finding {
    if is_private(mode) {
        Finding::ok(check, format!("{} is {mode:04o}", path.display()))
    } else {
        Finding::fail(
            check,
            format!(
                "{} is {mode:04o}: other users can access it",
                path.display()
            ),
            format!("chmod {want:o} {}", shell_quote(path)),
        )
    }
}

/// The config directory exists, is a directory and is `0700`.
pub(super) fn config_dir(dir: &Path) -> Finding {
    const CHECK: Check = Check::ConfigDir;
    match fs::metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Finding::fail(
            CHECK,
            format!("{} does not exist", dir.display()),
            "run deploy/bridge/setup.sh: it creates the dir (0700) and a pairing-only config",
        ),
        Err(error) => Finding::fail(
            CHECK,
            format!("cannot inspect {}: {error}", dir.display()),
            "run doctor as the user the bridge runs as",
        ),
        Ok(meta) if !meta.is_dir() => Finding::fail(
            CHECK,
            format!("{} is not a directory", dir.display()),
            "move it aside and re-run deploy/bridge/setup.sh",
        ),
        Ok(meta) => mode_finding(CHECK, dir, mode_of(&meta), 0o700),
    }
}

/// `config.toml`'s mode, then whether it loads, read and parsed by the same
/// steps as [`Config::load`]. Returns the config when it parses, even if its
/// mode is wrong (the bridge would refuse it), so the other checks still run.
pub(super) fn config_file(dir: &Path) -> (Vec<Finding>, Option<Config>) {
    let file = match Config::read_file(dir) {
        Ok(file) => file,
        Err(error) => return (vec![unreadable(&error)], None),
    };
    let path = file.path.clone();
    let mut findings = vec![mode_finding(Check::ConfigFile, &path, file.mode, 0o600)];
    match Config::from_file(dir, file) {
        Ok(config) => {
            findings.push(Finding::ok(
                Check::Config,
                format!("{} loads", path.display()),
            ));
            (findings, Some(config))
        }
        Err(error) => {
            // `ParseError` never quotes the file, so it cannot leak the token.
            findings.push(Finding::fail(
                Check::Config,
                error.to_string(),
                "fix the key named above; every key is documented in crates/bridge/src/config.rs",
            ));
            (findings, None)
        }
    }
}

/// The finding for a `config.toml` that [`Config::read_file`] refused.
fn unreadable(error: &ConfigError) -> Finding {
    const CHECK: Check = Check::ConfigFile;
    match error {
        ConfigError::Io { path, source } if source.kind() == io::ErrorKind::NotFound => {
            Finding::fail(
                CHECK,
                format!("{} does not exist", path.display()),
                "run deploy/bridge/setup.sh to write one (mode 0600, pairing only)",
            )
        }
        ConfigError::Io { .. } => Finding::fail(
            CHECK,
            error.to_string(),
            "run doctor as the user the bridge runs as",
        ),
        _ => Finding::fail(
            CHECK,
            error.to_string(),
            "move it aside and re-run deploy/bridge/setup.sh",
        ),
    }
}

/// The state dir is `0700`, and neither its entries nor the audit log
/// (wherever it lives) are open to group or other users. Under the systemd
/// unit (`unit`, when it runs this config), both must also be inside its
/// `ReadWritePaths`, and the dir must already exist: with
/// `ProtectSystem=strict` the bridge can neither write elsewhere nor create it.
pub(super) fn state_dir(config: &Config, unit: Option<&UnitState>) -> Finding {
    const CHECK: Check = Check::StateDir;
    let dir = &config.state_dir;
    if let Some(unit) = unit.filter(|unit| !unit.writable.is_empty()) {
        let outside: Vec<String> = [dir, &config.audit_log]
            .into_iter()
            .filter(|path| !unit.writable.iter().any(|w| path.starts_with(w)))
            .map(|path| path.display().to_string())
            .collect();
        if !outside.is_empty() {
            let writable: Vec<String> = unit
                .writable
                .iter()
                .map(|w| w.display().to_string())
                .collect();
            return Finding::fail(
                CHECK,
                format!(
                    "{} is outside the unit's ReadWritePaths ({}): the bridge cannot write there",
                    outside.join(" and "),
                    writable.join(" ")
                ),
                "keep state_dir and audit_log at their defaults (under ~/.local/state/passporter), \
                 or add the path to ReadWritePaths in the unit",
            );
        }
    }
    let meta = match fs::metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let create = format!("install -d -m 0700 {}", shell_quote(dir));
            return if unit.is_some() {
                Finding::fail(
                    CHECK,
                    format!(
                        "{} does not exist, and the unit cannot create it (ProtectSystem=strict)",
                        dir.display()
                    ),
                    format!("{create} (deploy/bridge/setup.sh does this), then restart the unit"),
                )
            } else {
                Finding::warn(
                    CHECK,
                    format!("{} does not exist yet", dir.display()),
                    format!(
                        "a bridge run by hand creates it (0700) on start; for the unit: {create}"
                    ),
                )
            };
        }
        Err(error) => {
            return Finding::fail(
                CHECK,
                format!("cannot inspect {}: {error}", dir.display()),
                "run doctor as the user the bridge runs as",
            );
        }
        Ok(meta) if !meta.is_dir() => {
            return Finding::fail(
                CHECK,
                format!("{} is not a directory", dir.display()),
                "point state_dir at a directory, or move this file aside",
            );
        }
        Ok(meta) => meta,
    };
    let dir_finding = mode_finding(CHECK, dir, mode_of(&meta), 0o700);
    if dir_finding.hint.is_some() {
        return dir_finding;
    }
    let mut open = match open_entries(dir) {
        Ok(open) => open,
        Err(error) => {
            return Finding::fail(
                CHECK,
                format!("cannot list {}: {error}", dir.display()),
                "run doctor as the user the bridge runs as; the dir must be theirs",
            );
        }
    };
    // The audit log may be configured anywhere, including deeper in the
    // state dir; judge the file itself, not a symlink to it.
    if fs::metadata(&config.audit_log).is_ok_and(|meta| !is_private(mode_of(&meta)))
        && !open.contains(&config.audit_log)
    {
        open.push(config.audit_log.clone());
    }
    if open.is_empty() {
        return Finding::ok(
            CHECK,
            format!("{} is 0700 and its files are private", dir.display()),
        );
    }
    let names: Vec<String> = open.iter().map(|p| p.display().to_string()).collect();
    let quoted: Vec<String> = open.iter().map(|p| shell_quote(p)).collect();
    Finding::fail(
        CHECK,
        format!("open to other users: {}", names.join(", ")),
        format!("chmod go= {}", quoted.join(" ")),
    )
}

/// Direct entries of `dir` with group or other bits. Symlinks are skipped:
/// their own mode means nothing, and `chmod` would not change it.
fn open_entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut open = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_symlink() && !is_private(mode_of(&meta)) {
            open.push(entry.path());
        }
    }
    open.sort();
    Ok(open)
}

/// `listen` under the bridge's own transport policy ([`crate::transport`]):
/// HTTPS (with the reverse proxy as `trusted_proxy`), loopback, or the allowlisted
/// `WireGuard` address.
pub(super) fn listen(config: &Config) -> Finding {
    const CHECK: Check = Check::Listen;
    let listen = config.listen;
    match crate::transport(config) {
        Ok(Transport::Tls) if listen.ip().is_loopback() => Finding::warn(
            CHECK,
            format!("HTTPS on {listen}: only a proxy on this machine can reach it"),
            "fine if your reverse proxy runs here; if it runs on another host, listen on an \
             address it can reach (listen = \"<this machine's address>:8443\")",
        ),
        Ok(Transport::Tls) => match config.trusted_proxy {
            Some(edge) => Finding::ok(
                CHECK,
                format!("HTTPS on {listen}; X-Forwarded-For is believed only from {edge}"),
            ),
            None => Finding::warn(
                CHECK,
                format!(
                    "HTTPS on {listen}, but no trusted_proxy: behind a reverse proxy every \
                     client looks like the proxy (rate limits, audit)"
                ),
                "set trusted_proxy to your reverse proxy's IP address \
                 (trusted_proxy = \"<proxy IP>\")",
            ),
        },
        Ok(Transport::Loopback) => Finding::ok(CHECK, format!("plain HTTP on loopback ({listen})")),
        Ok(Transport::PlainOverWireguard) => Finding::warn(
            CHECK,
            format!(
                "MVP mode: plain HTTP on {listen}, allowed by insecure_http_over_wireguard \
                 and insecure_http_allow"
            ),
            format!(
                "make sure only the tunnel reaches it: allow port {} only from your WireGuard \
                 subnet in your firewall, e.g. sudo ufw allow from <tunnel subnet> to any port {} \
                 proto tcp (HTTPS replaces this: {SWITCH_OVER})",
                listen.port(),
                listen.port()
            ),
        ),
        Err(crate::Error::PlaintextOffLoopback(_)) => Finding::fail(
            CHECK,
            format!(
                "{listen} is off loopback and insecure_http_over_wireguard is not set: \
                 the bridge refuses plain HTTP there"
            ),
            if crate::config::is_specific(listen.ip().to_canonical()) {
                format!(
                    "listen on 127.0.0.1 behind an HTTPS proxy (docs/deploy.md), add [tls], or (WireGuard only) set insecure_http_over_wireguard = \
                     true and insecure_http_allow = [\"{}\"]",
                    listen.ip().to_canonical()
                )
            } else {
                "listen on 127.0.0.1 behind an HTTPS proxy (docs/deploy.md), add [tls], or (WireGuard only) on this machine's own address with \
                 insecure_http_over_wireguard = true and that IP in insecure_http_allow; plain \
                 HTTP is never allowed on 0.0.0.0 or ::"
                    .to_owned()
            },
        ),
        Err(error) => Finding::fail(
            CHECK,
            error.to_string(),
            "set listen to this machine's own address and list exactly that IP in \
             insecure_http_allow (never 0.0.0.0 or ::)",
        ),
    }
}

/// The `[tls]` leaf, judged at `now`: it loads (key private and matching
/// the leaf), covers `tls.name` (what the proxy verifies) and is not
/// expired or about to. Without `[tls]`: fine on loopback behind a trusted
/// HTTPS proxy on this machine, else a warning (phones cannot pair).
pub(super) fn tls(config: &Config, now: SystemTime) -> Finding {
    const CHECK: Check = Check::Tls;
    let Some(tls) = &config.tls else {
        let loopback = matches!(crate::transport(config), Ok(Transport::Loopback));
        return match config.trusted_proxy {
            Some(proxy) if loopback => Finding::ok(
                CHECK,
                format!("no [tls]: plain HTTP on loopback, HTTPS ends at the proxy ({proxy})"),
            ),
            None if loopback => Finding::warn(
                CHECK,
                "no [tls] and no trusted_proxy: phones need HTTPS to pair",
                format!(
                    "put an HTTPS proxy in front (tailscale serve, Caddy) and set \
                     trusted_proxy = \"127.0.0.1\": {SWITCH_OVER}"
                ),
            ),
            _ => Finding::warn(
                CHECK,
                "no [tls]: plain HTTP off loopback, so phones cannot pair",
                format!("switch to HTTPS: {SWITCH_OVER}"),
            ),
        };
    };
    // The same checks the bridge makes before serving a leaf.
    let expiry = match Leaf::load_servable(tls, now) {
        Ok((_, validity)) => validity.not_after,
        Err(error) => {
            let name = tls.name.as_ref();
            let hint = match &error {
                TlsError::TooOpen { path, .. } => format!("chmod 600 {}", shell_quote(path)),
                TlsError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                    format!("install the leaf and its key from your CA ({SWITCH_OVER})")
                }
                TlsError::KeyMismatch { .. } => {
                    "install the key the leaf was issued for (renew both together); the \
                     bridge refuses to start with this pair and keeps serving the old one on reload"
                        .to_owned()
                }
                TlsError::WrongName { .. } => format!(
                    "issue the leaf with DNS SAN {name}, or set tls.name to the name your \
                     reverse proxy verifies"
                ),
                TlsError::Expired { .. } => RENEW.to_owned(),
                TlsError::NotYetValid { .. } => {
                    "check the clocks of the CA and this host (timedatectl), or re-issue the leaf"
                        .to_owned()
                }
                _ => format!(
                    "tls.cert must hold the PEM chain (leaf first) and tls.key its PEM private key \
                     ({SWITCH_OVER})"
                ),
            };
            return Finding::fail(CHECK, error.to_string(), hint);
        }
    };
    let name = tls.name.as_ref();
    let left = expiry.remaining(now).unwrap_or_default();
    if left < EXPIRY_WARNING {
        Finding::warn(
            CHECK,
            format!("the leaf for {name} expires at {expiry}, in {}", days(left)),
            RENEW,
        )
    } else {
        Finding::ok(
            CHECK,
            format!(
                "leaf for {name}, key matches, expires at {expiry} (in {})",
                days(left)
            ),
        )
    }
}

/// `left` in whole days, or hours under two days.
fn days(left: Duration) -> String {
    let hours = left.as_secs() / 3600;
    if hours < 48 {
        format!("{hours} h")
    } else {
        format!("{} days", hours / 24)
    }
}

/// Which clients can get in: the static token (MVP) or paired devices only.
/// Only ever tests the token for presence.
pub(super) fn auth(config: &Config) -> Finding {
    const CHECK: Check = Check::Auth;
    if config.static_token_behind_https() {
        return Finding::warn(
            CHECK,
            "the static api_token is accepted behind HTTPS ([tls] or trusted_proxy), where \
             phones can pair instead; builds that refuse this config will not start",
            "pair every phone (`passporter pair`), then set allow_static_token = false or \
             remove api_token",
        );
    }
    if config.static_token().is_some() {
        return Finding::warn(
            CHECK,
            "MVP mode: the static api_token is accepted alongside paired devices",
            "once every phone has paired over HTTPS, set allow_static_token = false or remove api_token",
        );
    }
    let detail = if config.api_token.is_some() {
        "paired devices only (api_token is kept but refused: allow_static_token = false)"
    } else {
        "paired devices only"
    };
    if matches!(crate::transport(config), Ok(Transport::PlainOverWireguard)) {
        Finding::warn(
            CHECK,
            format!("{detail}, but over plain HTTP the PWA cannot pair (no secure context)"),
            "until HTTPS is set up, phones on plain HTTP need api_token with allow_static_token = true \
             (deploy/bridge/setup.sh --insecure-mvp writes both)",
        )
    } else {
        Finding::ok(CHECK, format!("{detail}; `passporter pair` adds a phone"))
    }
}

/// herdr answers a ping on the socket the bridge under the systemd unit
/// would use: `herdr_socket`, else herdr's default session socket. The
/// unit has no `HERDR_SOCKET_PATH` (herdr sets it only inside its panes), so
/// doctor ignores the one in its own environment, and says so.
pub(super) async fn herdr<H: Herdr>(
    config: &Config,
    connect: impl FnOnce(&Path) -> H,
    timeout: Duration,
) -> Finding {
    const CHECK: Check = Check::Herdr;
    const HINT: &str =
        "start herdr as this user, or point herdr_socket in config.toml at its socket";
    // herdr's default session socket, as `SocketClient::from_env` finds it
    // without HERDR_SOCKET_PATH.
    let default = std::env::var_os("HOME").map(|home| Path::new(&home).join(DEFAULT_HERDR_SOCKET));
    let Some(socket) = config.herdr_socket.clone().or(default) else {
        return Finding::fail(
            CHECK,
            "no herdr socket: herdr_socket is unset and HOME is not set",
            "set herdr_socket in config.toml",
        );
    };
    let ignored = std::env::var_os("HERDR_SOCKET_PATH")
        .map(PathBuf::from)
        .filter(|env| config.herdr_socket.is_none() && *env != socket);
    let path = match ignored {
        Some(env) => format!(
            "{} (not this shell's HERDR_SOCKET_PATH {}: the unit does not see it)",
            socket.display(),
            env.display()
        ),
        None => socket.display().to_string(),
    };
    let client = connect(&socket);
    match tokio::time::timeout(timeout, client.ping()).await {
        Ok(Ok(info)) => Finding::ok(
            CHECK,
            printable(&format!(
                "{path}: herdr {}, protocol {}",
                info.version, info.protocol
            )),
        ),
        Ok(Err(error)) => {
            // Connection errors already name the socket; server errors do not.
            let error = error.to_string();
            let detail = if error.contains(&path) {
                error
            } else {
                format!("{path}: {error}")
            };
            Finding::fail(CHECK, printable(&detail), HINT)
        }
        Err(_) => Finding::fail(
            CHECK,
            printable(&format!(
                "{path}: no answer within {}s",
                timeout.as_secs_f32()
            )),
            HINT,
        ),
    }
}

/// whisper-server accepts a TCP connection on its (loopback) address. The
/// connection is dropped at once; nothing is sent.
pub(super) async fn whisper(addr: SocketAddr, timeout: Duration) -> Finding {
    const CHECK: Check = Check::Whisper;
    const HINT: &str = "deploy/whisper/build.sh installs it; then systemctl --user status whisper-server \
                        (audio.whisper in config.toml must match its --port)";
    match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Ok(Ok(_)) => Finding::ok(CHECK, format!("whisper-server is listening on {addr}")),
        Ok(Err(error)) => Finding::fail(
            CHECK,
            format!("nothing accepts connections on {addr} ({error}): dictation will fail"),
            HINT,
        ),
        Err(_) => Finding::fail(
            CHECK,
            format!(
                "{addr} did not accept a connection within {}s: dictation will fail",
                timeout.as_secs_f32()
            ),
            HINT,
        ),
    }
}

/// Web Push: off without a `[push]` table; otherwise the VAPID key must load
/// (it is loaded, never shown). Pings go out to the browsers' push services,
/// so there is no local push service to probe.
pub(super) fn push(config: &Config) -> Finding {
    const CHECK: Check = Check::Push;
    let Some(push) = &config.push else {
        return Finding::off(
            CHECK,
            "off (no [push] table): the phone gets no ping when an agent needs you",
            "passporter-bridge push-keygen, then add [push] with subject = \"mailto:<you>\" \
             (docs/deploy.md, \"Push\")",
        );
    };
    let key = push.vapid_key.display();
    match VapidKey::load(&push.vapid_key) {
        Ok(_) => Finding::ok(
            CHECK,
            format!("on: VAPID key {key} loads; pings go to the browsers' push services"),
        ),
        Err(error) => {
            let hint = match &error {
                VapidError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                    "make the key: passporter-bridge push-keygen".to_owned()
                }
                VapidError::TooOpen { .. } => format!("chmod 600 {}", shell_quote(&push.vapid_key)),
                _ => format!(
                    "move {key} aside and run passporter-bridge push-keygen \
                     (every device must then subscribe again)"
                ),
            };
            Finding::fail(CHECK, error.to_string(), hint)
        }
    }
}
