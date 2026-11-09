//! `passporter doctor`: a read-only self-check of a deployment (design §8).
//!
//! [`run`] inspects, in order: the config dir and file modes, whether the
//! config loads, the state dir's modes, the listen address (HTTPS with a
//! trusted proxy, or plain HTTP by the `WireGuard` opt-in), the TLS leaf
//! (key match, expiry, the name the proxy verifies), the auth mode (static
//! token or device pairing), herdr's socket, whisper-server and Web Push,
//! and the systemd user unit. Each [`Finding`] that is not [`Status::Ok`]
//! carries a fix hint. The report opens with the build of the `passporter`
//! binary running it (the running bridge's is on `GET /v1/version`).
//!
//! It changes nothing: it reads files and metadata, pings herdr, opens (and
//! drops) a TCP connection to whisper-server and asks `systemctl --user`.
//! It never prints a secret: the static token is only ever tested for
//! presence, config errors never quote source lines (see
//! [`crate::config::ParseError`]), and key files are loaded, never shown.

mod checks;
mod systemd;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use herdr::Herdr;

use crate::config::shell_quote;
use crate::version::BUILD;

pub use self::systemd::{Active, Enabled, Systemctl, UNIT, UnitError, UnitProbe, UnitState};

/// How long each network probe (herdr, whisper-server) may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// As it should be.
    Ok,
    /// An optional feature is not configured.
    Off,
    /// Works, but is a deliberate MVP compromise or likely to bite later.
    Warn,
    /// Broken: the bridge will not start, or a feature will not work.
    Fail,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
        }
    }
}

/// What a [`Finding`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The config directory exists and is private (`0700`).
    ConfigDir,
    /// `config.toml` exists and is private (`0600`).
    ConfigFile,
    /// `config.toml` parses and validates.
    Config,
    /// The state directory and the files in it are private.
    StateDir,
    /// `listen` under the transport policy: HTTPS, loopback, or the
    /// `WireGuard` opt-in and its allowlist.
    Listen,
    /// The `[tls]` leaf loads, its key matches, it covers `tls.name` and
    /// has not expired.
    Tls,
    /// Static token or device pairing.
    Auth,
    /// herdr answers a ping on its socket.
    Herdr,
    /// whisper-server accepts connections on its loopback address.
    Whisper,
    /// Web Push is configured and its VAPID key loads.
    Push,
    /// The `passporter-bridge` systemd user unit is enabled and active.
    Unit,
}

impl Check {
    /// A short, stable label for the report.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ConfigDir => "config dir",
            Self::ConfigFile => "config file",
            Self::Config => "config",
            Self::StateDir => "state dir",
            Self::Listen => "listen",
            Self::Tls => "tls",
            Self::Auth => "auth",
            Self::Herdr => "herdr",
            Self::Whisper => "whisper",
            Self::Push => "push",
            Self::Unit => "systemd unit",
        }
    }
}

/// The outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What was checked.
    pub check: Check,
    /// How it came out.
    pub status: Status,
    /// What was found, in one line.
    pub detail: String,
    /// How to fix it; `None` only when the status is [`Status::Ok`].
    pub hint: Option<String>,
}

impl Finding {
    fn ok(check: Check, detail: impl Into<String>) -> Self {
        Self {
            check,
            status: Status::Ok,
            detail: detail.into(),
            hint: None,
        }
    }

    fn with_hint(
        check: Check,
        status: Status,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            check,
            status,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn off(check: Check, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::with_hint(check, Status::Off, detail, hint)
    }

    fn warn(check: Check, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::with_hint(check, Status::Warn, detail, hint)
    }

    fn fail(check: Check, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::with_hint(check, Status::Fail, detail, hint)
    }
}

/// Everything [`run`] found. `Display` renders the operator's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The config directory that was checked.
    pub config_dir: PathBuf,
    /// One finding per check that could run, in report order. When the
    /// config does not load, the checks that need it are left out.
    pub findings: Vec<Finding>,
}

impl Report {
    /// How many checks failed.
    #[must_use]
    pub fn failures(&self) -> usize {
        self.count(Status::Fail)
    }

    /// No check failed (warnings and features turned off are fine).
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.failures() == 0
    }

    /// The finding for `check`, if that check ran.
    #[must_use]
    pub fn finding(&self, check: Check) -> Option<&Finding> {
        self.findings.iter().find(|finding| finding.check == check)
    }

    fn count(&self, status: Status) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.status == status)
            .count()
    }
}

/// Width of the label column: the longest [`Check::label`].
const LABEL_WIDTH: usize = 12;

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "passporter doctor: {}", self.config_dir.display())?;
        writeln!(
            f,
            "  {:<4}  {:<LABEL_WIDTH$}  passporter {BUILD}",
            "", "build"
        )?;
        for finding in &self.findings {
            writeln!(
                f,
                "  {:<4}  {:<LABEL_WIDTH$}  {}",
                finding.status.tag(),
                finding.check.label(),
                finding.detail
            )?;
            if let Some(hint) = &finding.hint {
                writeln!(f, "  {:<4}  {:<LABEL_WIDTH$}  fix: {hint}", "", "")?;
            }
        }
        if self
            .finding(Check::Config)
            .is_none_or(|config| config.status != Status::Ok)
        {
            writeln!(f, "(the other checks need a config that loads)")?;
        }
        let (failed, warned) = (self.failures(), self.count(Status::Warn));
        if failed == 0 && warned == 0 {
            writeln!(f, "All checks passed.")
        } else {
            writeln!(f, "{failed} failed, {warned} warning(s).")
        }
    }
}

/// Runs every check against the config in `config_dir`.
///
/// `connect` makes a herdr client for the socket the bridge would use (the
/// binary passes [`herdr::SocketClient::new`], tests a `FakeHerdr`); `unit`
/// reports the systemd unit (the binary passes [`Systemctl`]); `timeout`
/// bounds each probe. The probes (herdr, whisper-server, systemd) run
/// concurrently, so the worst case is one `timeout`.
pub async fn run<H: Herdr, U: UnitProbe>(
    config_dir: &Path,
    connect: impl FnOnce(&Path) -> H,
    unit: &U,
    timeout: Duration,
) -> Report {
    let mut findings = vec![checks::config_dir(config_dir)];
    let (file, config) = checks::config_file(config_dir);
    findings.extend(file);
    let Some(config) = config else {
        let unit = unit.state(timeout).await;
        findings.push(systemd::check(&unit, config_dir));
        return Report {
            config_dir: config_dir.to_path_buf(),
            findings,
        };
    };
    let (herdr, whisper, unit) = tokio::join!(
        checks::herdr(&config, connect, timeout),
        checks::whisper(config.audio.whisper, timeout),
        unit.state(timeout),
    );
    findings.extend([
        checks::state_dir(&config, systemd::serving(&unit, config_dir)),
        checks::listen(&config),
        checks::tls(&config, SystemTime::now()),
        checks::auth(&config),
        herdr,
        whisper,
        checks::push(&config),
        systemd::check(&unit, config_dir),
    ]);
    Report {
        config_dir: config_dir.to_path_buf(),
        findings,
    }
}

/// `text` without control, bidi or zero-width characters (the same rule
/// as untrusted text bound for a prompt), so what herdr or systemctl says
/// cannot drive or disguise the terminal.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|&c| !protocol::text::is_disallowed(c, false))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_shows_hints_and_a_summary() {
        let report = Report {
            config_dir: "/c".into(),
            findings: vec![
                Finding::ok(Check::ConfigDir, "/c is 0700"),
                Finding::fail(Check::ConfigFile, "/c/config.toml is 0644", "chmod 600 x"),
                Finding::warn(Check::Unit, "cannot ask", "run it"),
            ],
        };
        let text = report.to_string();
        assert!(
            text.contains("  ok    config dir    /c is 0700\n"),
            "{text}"
        );
        assert!(text.contains("  FAIL  config file"), "{text}");
        assert!(text.contains("fix: chmod 600 x"), "{text}");
        assert!(text.contains("need a config that loads"), "{text}");
        assert!(
            text.contains(&format!(
                "        build         passporter {}, built ",
                BUILD.sha
            )),
            "{text}"
        );
        assert!(text.ends_with("1 failed, 1 warning(s).\n"), "{text}");
        assert!(!report.healthy());
        assert_eq!(report.failures(), 1);
    }

    #[test]
    fn printable_drops_controls_bidi_and_zero_width() {
        assert_eq!(printable("a\u{1b}[2Jb\u{202e}c\u{200b}d"), "a[2Jbcd");
    }

    #[test]
    fn a_config_that_does_not_parse_says_why_checks_are_missing() {
        let report = Report {
            config_dir: "/c".into(),
            findings: vec![Finding::fail(Check::Config, "bad", "fix")],
        };
        assert!(report.to_string().contains("need a config that loads"));
    }

    #[test]
    fn labels_fit_the_column() {
        for check in [
            Check::ConfigDir,
            Check::ConfigFile,
            Check::Config,
            Check::StateDir,
            Check::Listen,
            Check::Tls,
            Check::Auth,
            Check::Herdr,
            Check::Whisper,
            Check::Push,
            Check::Unit,
        ] {
            assert!(check.label().len() <= LABEL_WIDTH, "{check:?}");
        }
    }
}
