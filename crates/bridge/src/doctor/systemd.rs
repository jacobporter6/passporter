//! The bridge's systemd user unit, as `systemctl --user show` reports it.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use super::{Check, Finding, printable, shell_quote};

/// The unit `deploy/bridge/setup.sh` installs.
pub const UNIT: &str = "passporter-bridge.service";

/// Whether the unit starts with the user manager (`UnitFileState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enabled {
    /// Starts at login (or boot, with lingering).
    Enabled,
    /// Installed, but not started automatically.
    Disabled,
    /// The unit file is not installed (`LoadState=not-found`).
    NotFound,
    /// Any other state (`static`, `masked`, ...), printable.
    Other(String),
}

impl Enabled {
    /// Parses systemd's word for it.
    #[must_use]
    pub fn parse(word: &str) -> Self {
        match word {
            "enabled" => Self::Enabled,
            "disabled" => Self::Disabled,
            "not-found" => Self::NotFound,
            other => Self::Other(printable(other)),
        }
    }

    fn describe(&self) -> &str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::NotFound => "not installed",
            Self::Other(word) => word,
        }
    }
}

/// What the unit is doing now (`ActiveState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Active {
    /// Running.
    Active,
    /// Starting, stopping or reloading (as during a restart): give it a moment.
    Transitioning(String),
    /// Stopped.
    Inactive,
    /// Stopped after a failure.
    Failed,
    /// Any other state, printable.
    Other(String),
}

impl Active {
    /// Parses systemd's word for it.
    #[must_use]
    pub fn parse(word: &str) -> Self {
        match word {
            "active" => Self::Active,
            "activating" | "deactivating" | "reloading" | "refreshing" => {
                Self::Transitioning(word.to_owned())
            }
            "inactive" => Self::Inactive,
            "failed" => Self::Failed,
            other => Self::Other(printable(other)),
        }
    }

    fn describe(&self) -> &str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Failed => "failed",
            Self::Transitioning(word) | Self::Other(word) => word,
        }
    }
}

/// The unit's state and the parts of its definition doctor relies on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitState {
    /// Whether it starts automatically.
    pub enabled: Enabled,
    /// What it is doing now.
    pub active: Active,
    /// `ReadWritePaths=` (an optional entry's `-` dropped). With the shipped
    /// unit's `ProtectSystem=strict`, the only places the bridge can write.
    pub writable: Vec<PathBuf>,
    /// The `--config-dir` its `ExecStart=` passes, if any.
    pub config_dir: Option<PathBuf>,
}

impl UnitState {
    fn installed(&self) -> bool {
        self.enabled != Enabled::NotFound
    }

    /// Whether this unit runs the bridge with the config in `dir` (or does
    /// not say which config it uses).
    fn serves(&self, dir: &Path) -> bool {
        self.config_dir
            .as_deref()
            .is_none_or(|served| same_path(served, dir))
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Why systemd could not be asked.
#[derive(Debug, thiserror::Error)]
pub enum UnitError {
    /// `systemctl` could not be run.
    #[error("cannot run systemctl: {0}")]
    Spawn(#[source] io::Error),
    /// systemctl did not finish in time (a wedged user bus).
    #[error("systemctl --user did not answer within {}s", .0.as_secs_f32())]
    TimedOut(Duration),
    /// systemctl ran but reported no state (no user manager, no session bus).
    #[error("{0}")]
    NoAnswer(String),
}

/// Reports [`UNIT`]'s state.
pub trait UnitProbe: Send + Sync {
    /// The unit's current state, or why systemd could not be asked within
    /// `timeout`.
    fn state(&self, timeout: Duration)
    -> impl Future<Output = Result<UnitState, UnitError>> + Send;
}

/// Asks the real `systemctl --user`, killing it if it outlives the timeout.
#[derive(Debug, Clone, Copy, Default)]
pub struct Systemctl;

impl UnitProbe for Systemctl {
    async fn state(&self, timeout: Duration) -> Result<UnitState, UnitError> {
        let output = Command::new("systemctl")
            .args(["--user", "show", UNIT])
            .args([
                "-p",
                "LoadState",
                "-p",
                "UnitFileState",
                "-p",
                "ActiveState",
            ])
            .args(["-p", "ReadWritePaths", "-p", "ExecStart"])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(timeout, output)
            .await
            .map_err(|_| UnitError::TimedOut(timeout))?
            .map_err(UnitError::Spawn)?;
        parse_show(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
            let why = String::from_utf8_lossy(&output.stderr);
            let why = printable(why.lines().next().unwrap_or_default());
            UnitError::NoAnswer(if why.trim().is_empty() {
                "systemctl --user show printed no state".to_owned()
            } else {
                why.trim().to_owned()
            })
        })
    }
}

/// Parses `systemctl show` output (`Key=Value` lines); `None` without the
/// states.
fn parse_show(text: &str) -> Option<UnitState> {
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
    };
    let load = value("LoadState")?;
    let active = Active::parse(value("ActiveState")?);
    let enabled = if load == "not-found" {
        Enabled::NotFound
    } else {
        Enabled::parse(value("UnitFileState").unwrap_or_default())
    };
    let writable = value("ReadWritePaths")
        .unwrap_or_default()
        .split_whitespace()
        .map(|path| PathBuf::from(path.trim_start_matches('-')))
        .collect();
    // ExecStart={ path=... ; argv[]=/bin/x --config-dir /dir ; ... }
    let config_dir = value("ExecStart").and_then(|exec| {
        let argv = exec.split("argv[]=").nth(1)?.split(" ;").next()?;
        let mut words = argv.split_whitespace();
        words.find(|&word| word == "--config-dir")?;
        words.next().map(PathBuf::from)
    });
    Some(UnitState {
        enabled,
        active,
        writable,
        config_dir,
    })
}

/// The unit state, if it is installed and runs the bridge on the config
/// in `config_dir`: only then do its limits apply to that config.
pub(super) fn serving<'a>(
    state: &'a Result<UnitState, UnitError>,
    config_dir: &Path,
) -> Option<&'a UnitState> {
    state
        .as_ref()
        .ok()
        .filter(|state| state.installed() && state.serves(config_dir))
}

/// The finding for the unit's state, given the config dir doctor checked.
pub(super) fn check(state: &Result<UnitState, UnitError>, config_dir: &Path) -> Finding {
    const CHECK: Check = Check::Unit;
    let state = match state {
        Ok(state) => state,
        Err(error) => {
            return Finding::warn(
                CHECK,
                printable(&format!("cannot ask systemd: {error}")),
                "run doctor as the bridge's user, in a session with a user manager \
                 (systemctl --user status passporter-bridge)",
            );
        }
    };
    let (is, doing) = (state.enabled.describe(), state.active.describe());
    if !state.installed() {
        return Finding::warn(
            CHECK,
            format!("{UNIT} is not installed (fine only if you run the bridge another way)"),
            "deploy/bridge/setup.sh installs, enables and starts it",
        );
    }
    if let Some(served) = state
        .config_dir
        .as_deref()
        .filter(|_| !state.serves(config_dir))
    {
        return Finding::warn(
            CHECK,
            format!(
                "{UNIT} runs the bridge with {}, not the config dir checked here",
                served.display()
            ),
            format!(
                "check that one instead: passporter --config-dir {} doctor",
                shell_quote(served)
            ),
        );
    }
    match (&state.enabled, &state.active) {
        (Enabled::Enabled, Active::Active) => {
            Finding::ok(CHECK, format!("{UNIT} is enabled and active"))
        }
        (_, Active::Transitioning(_)) => Finding::warn(
            CHECK,
            format!("{UNIT} is {is} and {doing}"),
            "wait a few seconds and run doctor again; if it stays there, \
             journalctl --user -u passporter-bridge -e",
        ),
        (_, Active::Active) => Finding::warn(
            CHECK,
            format!("{UNIT} is active but {is}: it may not come back after a reboot"),
            "systemctl --user enable passporter-bridge (and loginctl enable-linger \"$USER\" \
             to run while logged out)",
        ),
        _ => Finding::fail(
            CHECK,
            format!("{UNIT} is {is} and {doing}"),
            "journalctl --user -u passporter-bridge -e shows why; then \
             systemctl --user enable --now passporter-bridge",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_words_parse() {
        assert_eq!(Active::parse("active"), Active::Active);
        assert_eq!(
            Active::parse("reloading"),
            Active::Transitioning("reloading".to_owned())
        );
        assert_eq!(Active::parse("failed"), Active::Failed);
        assert_eq!(Enabled::parse("not-found"), Enabled::NotFound);
        assert_eq!(
            Enabled::parse("mas\u{202e}ked"),
            Enabled::Other("masked".to_owned()),
            "bidi controls are dropped"
        );
    }

    #[test]
    fn parses_systemctl_show() {
        let shown = "LoadState=loaded\nUnitFileState=enabled\nActiveState=active\n\
            ReadWritePaths=-/home/me/.local/state/passporter /srv/x\n\
            ExecStart={ path=/home/me/.local/bin/passporter-bridge ; \
            argv[]=/home/me/.local/bin/passporter-bridge --config-dir /home/me/.config/passporter ; \
            ignore_errors=no ; start_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }\n";
        let state = parse_show(shown).expect("parses");
        assert_eq!(state.enabled, Enabled::Enabled);
        assert_eq!(state.active, Active::Active);
        assert_eq!(
            state.writable,
            [
                PathBuf::from("/home/me/.local/state/passporter"),
                PathBuf::from("/srv/x")
            ]
        );
        assert_eq!(
            state.config_dir,
            Some(PathBuf::from("/home/me/.config/passporter"))
        );

        let missing = parse_show(
            "LoadState=not-found\nUnitFileState=\nActiveState=inactive\nReadWritePaths=\nExecStart=\n",
        )
        .expect("parses");
        assert_eq!(missing.enabled, Enabled::NotFound);
        assert!(missing.writable.is_empty() && missing.config_dir.is_none());

        assert_eq!(parse_show(""), None, "no bus: no state");
    }
}
