//! The bridge's config file, `<config dir>/config.toml` (design §8).
//!
//! The config directory defaults to `~/.config/passporter/` and holds the
//! bridge's secrets as later tasks add them (device keys, TLS key paths), so
//! the bridge refuses to start unless the directory is `0700` and the file
//! `0600` (no group or other permission bits at all).
//!
//! Every field is optional; an empty file means "all defaults":
//!
//! ```toml
//! listen = "127.0.0.1:8443"         # default; loopback, behind an HTTPS proxy
//! static_dir = "/home/me/.local/share/passporter/web"
//! herdr_socket = "/home/me/.config/herdr/herdr.sock"
//! trusted_proxy = "127.0.0.1"        # your HTTPS proxy; only it may set X-Forwarded-For
//! state_dir = "/home/me/.local/state/passporter"   # audit log lives here
//! audit_log = "/home/me/.local/state/passporter/audit.jsonl"
//! claude_projects_dir = "/home/me/.claude/projects"
//! codex_sessions_dir = "/home/me/.codex/sessions"
//!
//! # Insecure MVP mode (crate README), off unless you opt in, and only
//! # without [tls] and trusted_proxy: a static bearer token, accepted
//! # alongside device sessions only with the explicit opt-in, and plain HTTP
//! # on an address reached only over WireGuard.
//! # api_token = "<64 hex chars from deploy/bridge/setup.sh --insecure-mvp>"
//! # allow_static_token = true
//! # insecure_http_over_wireguard = true
//! # insecure_http_allow = ["192.0.2.10"]
//!
//! [push]                              # Web Push pings (crate::push)
//! subject = "mailto:me@example.com"   # VAPID contact for push services
//! vapid_key = "vapid.pem"             # default; relative to the config dir
//! extra_hosts = ["push.example.org"]  # self-hosted push services (default none)
//! ntfy_host = "ntfy.example.org"     # a self-hosted ntfy server, exact name (default none)
//! car_announce_done = "decide"        # default; Android Auto announces a finished agent only
//!                                     # once its TL;DR has a Decide line ("always", "never")
//! quiet_done_workspaces = ["agent-swarm"]  # finishes here never ping
//! quiet_done_agent_prefixes = ["impl-", "rev-"]  # nor agents named so
//!
//! [audio]                             # dictation (design §6.3); all optional
//! whisper = "127.0.0.1:8178"          # whisper-server (deploy/whisper); loopback only
//! retention_hours = 168               # keep transcribed audio this long (0: delete at once)
//! max_upload_mib = 25                 # per clip
//! max_duration_secs = 300             # per clip
//!
//! [tldr]                              # TL;DRs of long turns (design §6.4)
//! enabled = true                      # false: never run the summarizer
//! claude = "claude"                   # the `claude` executable (PATH lookup)
//! timeout_secs = 90                   # limit on one summary, 1–600
//!
//! [spawn]                             # new agents from the phone (crate::spawn)
//! workspace = "passporter"            # default; the herdr workspace they start in
//! kinds = ["claude", "codex"]         # default; the agent kinds the phone may pick
//! projects = [                        # the only folders an agent may start in
//!   { name = "passporter", path = "/home/me/git/passporter" },
//! ]
//!
//! [[runbooks]]                        # operations the phone may run (crate::runbooks)
//! name = "Deploy bridge"
//! aliases = ["deploy the bridge"]     # more phrases that offer it by voice
//! cwd = "/home/me/git/passporter"
//! argv = ["deploy/bridge/setup.sh"]   # executed directly, no shell
//! confirm = true                      # default; a tap asks first (speech always does)
//! timeout_secs = 600                  # default
//!
//! [tls]                               # HTTPS served by the bridge itself (crate::tls)
//! cert = "tls/leaf.pem"               # chain, leaf first; relative to the config dir
//! key = "tls/leaf.key"                # mode 0600
//! name = "passporter.internal"        # default; the name your proxy verifies
//! ```
//!
//! With `[tls]` the bridge serves only HTTPS, so the plain-HTTP opt-ins
//! (`insecure_http_over_wireguard`, `insecure_http_allow`) are refused
//! alongside it: one or the other.

mod runbooks;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::{ProjectName, SpawnKind};
use rustls::pki_types::DnsName;

pub use self::runbooks::RunbookConfig;
use serde::Deserialize;

use crate::push::vapid::{Subject, VAPID_KEY_FILE};
use crate::push::{CarAnnounce, PushHost};
use crate::server::{ApiToken, InvalidToken};

/// The file name inside the config directory.
pub const CONFIG_FILE: &str = "config.toml";

/// Environment variable that overrides the config directory.
pub const CONFIG_DIR_ENV: &str = "PASSPORTER_CONFIG_DIR";

/// Larger config files are refused rather than read.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Permission bits that must be clear on the config dir and file.
const GROUP_OTHER_BITS: u32 = 0o077;

/// The default listen address: loopback only. Phones reach the bridge
/// through an HTTPS reverse proxy on the same machine (Tailscale Serve,
/// Caddy); listening anywhere else takes an explicit `listen`.
pub const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8443);

/// `config.toml` as read by [`Config::read_file`], not yet parsed.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ConfigFile {
    /// Where it was read from.
    pub(crate) path: PathBuf,
    /// Its permission bits.
    pub(crate) mode: u32,
    /// Its contents. May hold the token: never printed, not even by `Debug`.
    text: String,
}

impl std::fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigFile")
            .field("path", &self.path)
            .field("mode", &format_args!("{:04o}", self.mode))
            .field(
                "text",
                &format_args!("<{} bytes, redacted>", self.text.len()),
            )
            .finish()
    }
}

/// Whether permission bits give group and other users no access at all:
/// what the config dir, the config file and everything private must be.
#[must_use]
pub fn is_private_mode(mode: u32) -> bool {
    mode & GROUP_OTHER_BITS == 0
}

/// The bridge's configuration, as loaded from [`CONFIG_FILE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Address and port to listen on.
    pub listen: SocketAddr,
    /// The PWA's build output (`crates/web/dist`).
    pub static_dir: PathBuf,
    /// herdr's socket; `None` means herdr's own default resolution
    /// (`HERDR_SOCKET_PATH`, then `~/.config/herdr/herdr.sock`).
    pub herdr_socket: Option<PathBuf>,
    /// The only peer whose `X-Forwarded-For` header is believed (the HTTPS
    /// reverse proxy). `None` ignores the header from everyone.
    pub trusted_proxy: Option<IpAddr>,
    /// HTTPS: the leaf a proxy (or the phone) connects to. `None` serves
    /// plain HTTP (loopback, or the `WireGuard` opt-in); never both.
    pub tls: Option<TlsConfig>,
    /// MVP auth: a bearer token for clients that cannot use device keys
    /// (see [`crate::server::StaticToken`]). Only accepted when
    /// [`Self::allow_static_token`] opts in, and never alongside
    /// [`Self::tls`] or [`Self::trusted_proxy`]; see [`Self::static_token`].
    pub api_token: Option<ApiToken>,
    /// The explicit choice about [`Self::api_token`]: `true` accepts it,
    /// `false` keeps it in the file but refuses it. Device pairing needs a
    /// secure context, so plain-HTTP clients need the token until TLS lands.
    /// An `api_token` without either is a startup error
    /// ([`ParseError::StaticTokenUndecided`]), so a config from before the
    /// opt-in existed cannot silently lock its phone out.
    pub allow_static_token: bool,
    /// MVP transport opt-in: serve plain HTTP off loopback, because the
    /// only route in is `WireGuard` (which encrypts). Needs `listen`'s IP in
    /// [`Self::insecure_http_allow`] too. Refused with [`Self::tls`].
    pub insecure_http_over_wireguard: bool,
    /// The only non-loopback IPs plain HTTP may listen on.
    pub insecure_http_allow: Vec<IpAddr>,
    /// Where the bridge keeps its state (the audit log).
    pub state_dir: PathBuf,
    /// The append-only action audit log (JSON lines).
    pub audit_log: PathBuf,
    /// Claude Code's projects directory; `None` means Claude's own default
    /// (`$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects`).
    pub claude_projects_dir: Option<PathBuf>,
    /// Codex's sessions directory (rollouts only: nothing else under
    /// `~/.codex` is read); `None` means Codex's own default
    /// (`$CODEX_HOME/sessions`, else `~/.codex/sessions`).
    pub codex_sessions_dir: Option<PathBuf>,
    /// Web Push; `None` (no `[push]` table) turns it off.
    pub push: Option<PushConfig>,
    /// Dictation: the transcription backend, caps and retention.
    pub audio: AudioConfig,
    /// TL;DR generation.
    pub tldr: TldrConfig,
    /// Starting agents from the phone; `None` (no `[spawn]` table) turns
    /// it off.
    pub spawn: Option<SpawnConfig>,
    /// Operations the phone may run (`[[runbooks]]`), in config order.
    pub runbooks: Vec<RunbookConfig>,
}

/// The `[spawn]` table: which agents the phone may start, and where
/// (see [`crate::spawn`]). The phone only ever names a project and a kind
/// from here; paths and herdr arguments never come from a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnConfig {
    /// The herdr workspace (by label) new agents get a tab in; created on
    /// first use. Never one of `push.quiet_done_workspaces`: agents the
    /// user starts ping when they finish, like the user's own.
    pub workspace: String,
    /// The allowlist, in the order the phone shows it.
    pub projects: Vec<SpawnProject>,
    /// The kinds the phone may pick, in display order.
    pub kinds: Vec<SpawnKind>,
}

/// One allowlisted project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnProject {
    /// What the phone shows and sends.
    pub name: ProjectName,
    /// Where the agent starts: an absolute directory.
    pub path: PathBuf,
}

/// The default `[spawn] workspace`.
pub const DEFAULT_SPAWN_WORKSPACE: &str = "passporter";

/// Longest `[spawn] workspace` label, in characters.
const MAX_WORKSPACE_LABEL_CHARS: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpawn {
    workspace: Option<String>,
    #[serde(default)]
    projects: Vec<RawSpawnProject>,
    kinds: Option<Vec<SpawnKind>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpawnProject {
    name: ProjectName,
    path: PathBuf,
}

impl RawSpawn {
    fn validate(self, push: Option<&PushConfig>) -> Result<SpawnConfig, ParseError> {
        let workspace = self
            .workspace
            .unwrap_or_else(|| DEFAULT_SPAWN_WORKSPACE.to_owned());
        let label_ok = !workspace.trim().is_empty()
            && workspace.chars().count() <= MAX_WORKSPACE_LABEL_CHARS
            && !workspace.chars().any(char::is_control);
        if !label_ok {
            return Err(ParseError::SpawnWorkspace);
        }
        if push.is_some_and(|push| push.quiet_done_workspaces.contains(&workspace)) {
            return Err(ParseError::SpawnWorkspaceQuiet(workspace));
        }
        let mut seen = BTreeSet::new();
        let mut projects = Vec::with_capacity(self.projects.len());
        for project in self.projects {
            if !seen.insert(project.name.clone()) {
                return Err(ParseError::SpawnDuplicateProject(project.name.to_string()));
            }
            if !project.path.is_absolute() {
                return Err(ParseError::SpawnRelativePath(project.name.to_string()));
            }
            projects.push(SpawnProject {
                name: project.name,
                path: project.path,
            });
        }
        let mut kinds = self.kinds.unwrap_or_else(|| SpawnKind::ALL.to_vec());
        let mut unique = BTreeSet::new();
        kinds.retain(|kind| unique.insert(*kind));
        if kinds.is_empty() {
            return Err(ParseError::SpawnNoKinds);
        }
        Ok(SpawnConfig {
            workspace,
            projects,
            kinds,
        })
    }
}

/// The `[push]` table: Web Push wake pings (see [`crate::push`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushConfig {
    /// The VAPID contact push services see (`mailto:` or `https:`).
    pub subject: Subject,
    /// The VAPID private key (made by `passporter-bridge push-keygen`).
    /// Relative paths are resolved against the config directory by
    /// [`Config::load`].
    pub vapid_key: PathBuf,
    /// Push services allowed beyond the browsers' own (each with its
    /// subdomains), such as a self-hosted one. Empty by default.
    pub extra_hosts: Vec<PushHost>,
    /// The self-hosted ntfy server (`UnifiedPush` for the native app),
    /// allowed by its exact name. None by default.
    pub ntfy_host: Option<PushHost>,
    /// When Android Auto announces a finished agent; see [`CarAnnounce`].
    pub car_announce_done: CarAnnounce,
    /// herdr workspace labels whose agents ping only when blocked, never
    /// for finishing a turn (say, a workspace where automation runs agents
    /// that other agents wait on). Defaults to
    /// [`DEFAULT_QUIET_DONE_WORKSPACES`]; an explicit empty list pings for
    /// every finish.
    pub quiet_done_workspaces: BTreeSet<String>,
    /// herdr agent-name prefixes whose agents ping only when blocked, never
    /// for finishing a turn, in any workspace. Defaults to
    /// [`DEFAULT_QUIET_DONE_AGENT_PREFIXES`]; an explicit empty list turns
    /// this off. Never matches agents started from the phone.
    pub quiet_done_agent_prefixes: BTreeSet<String>,
}

/// The quiet workspaces when `quiet_done_workspaces` is not set: none, so
/// every finished turn pings.
pub const DEFAULT_QUIET_DONE_WORKSPACES: &[&str] = &[];

/// The quiet agent-name prefixes when `quiet_done_agent_prefixes` is not
/// set: none.
pub const DEFAULT_QUIET_DONE_AGENT_PREFIXES: &[&str] = &[];

fn owned_set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|&item| item.to_owned()).collect()
}

fn default_quiet_done_workspaces() -> BTreeSet<String> {
    owned_set(DEFAULT_QUIET_DONE_WORKSPACES)
}

fn default_quiet_done_agent_prefixes() -> BTreeSet<String> {
    owned_set(DEFAULT_QUIET_DONE_AGENT_PREFIXES)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPush {
    subject: Subject,
    vapid_key: Option<PathBuf>,
    #[serde(default)]
    extra_hosts: Vec<PushHost>,
    ntfy_host: Option<PushHost>,
    #[serde(default)]
    car_announce_done: CarAnnounce,
    #[serde(default = "default_quiet_done_workspaces")]
    quiet_done_workspaces: BTreeSet<String>,
    #[serde(default = "default_quiet_done_agent_prefixes")]
    quiet_done_agent_prefixes: BTreeSet<String>,
}

impl RawPush {
    fn validate(self) -> Result<PushConfig, ParseError> {
        if let Some(bad) = self
            .quiet_done_agent_prefixes
            .iter()
            .find(|prefix| prefix.is_empty() || crate::spawn::may_name(prefix))
        {
            return Err(ParseError::QuietDonePrefix(bad.clone()));
        }
        Ok(PushConfig {
            subject: self.subject,
            vapid_key: self
                .vapid_key
                .unwrap_or_else(|| PathBuf::from(VAPID_KEY_FILE)),
            extra_hosts: self.extra_hosts,
            ntfy_host: self.ntfy_host,
            car_announce_done: self.car_announce_done,
            quiet_done_workspaces: self.quiet_done_workspaces,
            quiet_done_agent_prefixes: self.quiet_done_agent_prefixes,
        })
    }
}

/// The default `whisper-server` address (deploy/whisper).
pub const DEFAULT_WHISPER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8178);

/// The `[audio]` table (design §6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioConfig {
    /// `whisper-server`'s address. Always loopback: audio never leaves the
    /// machine, and the connection is plain HTTP.
    pub whisper: SocketAddr,
    /// How long transcribed audio is kept for debugging; zero deletes it
    /// as soon as it is transcribed. (Failed clips keep theirs for a retry
    /// until their record expires.)
    pub retention: Duration,
    /// Largest clip, in bytes.
    pub max_upload_bytes: u64,
    /// Longest clip.
    pub max_duration: Duration,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            whisper: DEFAULT_WHISPER,
            retention: Duration::from_hours(7 * 24),
            max_upload_bytes: 25 * MIB,
            max_duration: Duration::from_secs(300),
        }
    }
}

/// The `[tldr]` table: whether and how the bridge runs the summarizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TldrConfig {
    /// Run the summarizer at all. When off, the TL;DR endpoint answers
    /// `null` and no model is ever started.
    pub enabled: bool,
    /// The `claude` executable: a path, or a name looked up on `PATH`.
    pub claude: PathBuf,
    /// Limit on one summary; the process is killed when it expires.
    pub timeout: Duration,
}

/// Longest allowed `[tldr] timeout_secs`.
pub const MAX_TLDR_TIMEOUT_SECS: u64 = 600;

impl Default for TldrConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            claude: PathBuf::from("claude"),
            timeout: summarize::DEFAULT_TIMEOUT,
        }
    }
}

const MIB: u64 = 1024 * 1024;
/// Caps on the `[audio]` knobs, so a typo cannot ask for absurd values.
const MAX_RETENTION_HOURS: u64 = 24 * 365;
const MAX_UPLOAD_MIB: u64 = 200;
const MAX_DURATION_SECS: u64 = 3600;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAudio {
    whisper: Option<SocketAddr>,
    retention_hours: Option<u64>,
    max_upload_mib: Option<u64>,
    max_duration_secs: Option<u64>,
}

impl RawAudio {
    fn validate(self) -> Result<AudioConfig, ParseError> {
        let defaults = AudioConfig::default();
        let whisper = self.whisper.unwrap_or(defaults.whisper);
        if !whisper.ip().is_loopback() {
            return Err(ParseError::WhisperNotLoopback(whisper));
        }
        let in_range = |key: &'static str, value: Option<u64>, min: u64, max: u64| match value {
            Some(v) if !(min..=max).contains(&v) => Err(ParseError::OutOfRange { key, min, max }),
            other => Ok(other),
        };
        let retention = in_range(
            "audio.retention_hours",
            self.retention_hours,
            0,
            MAX_RETENTION_HOURS,
        )?
        .map_or(defaults.retention, |h| Duration::from_secs(h * 3600));
        let max_upload_bytes = in_range(
            "audio.max_upload_mib",
            self.max_upload_mib,
            1,
            MAX_UPLOAD_MIB,
        )?
        .map_or(defaults.max_upload_bytes, |m| m * MIB);
        let max_duration = in_range(
            "audio.max_duration_secs",
            self.max_duration_secs,
            1,
            MAX_DURATION_SECS,
        )?
        .map_or(defaults.max_duration, Duration::from_secs);
        Ok(AudioConfig {
            whisper,
            retention,
            max_upload_bytes,
            max_duration,
        })
    }
}

/// The `[tldr]` table's shape.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTldr {
    enabled: Option<bool>,
    claude: Option<PathBuf>,
    timeout_secs: Option<u64>,
}

impl RawTldr {
    fn resolve(self) -> Result<TldrConfig, ParseError> {
        let defaults = TldrConfig::default();
        let timeout = match self.timeout_secs {
            None => defaults.timeout,
            Some(secs @ 1..=MAX_TLDR_TIMEOUT_SECS) => Duration::from_secs(secs),
            Some(_) => {
                return Err(ParseError::OutOfRange {
                    key: "tldr.timeout_secs",
                    min: 1,
                    max: MAX_TLDR_TIMEOUT_SECS,
                });
            }
        };
        Ok(TldrConfig {
            enabled: self.enabled.unwrap_or(defaults.enabled),
            claude: self.claude.unwrap_or(defaults.claude),
            timeout,
        })
    }
}

/// The name a proxy verifies the bridge's leaf against when `tls.name` is
/// not set. `.internal` is reserved for private use, so it never collides
/// with a public name; set `tls.name` to whatever your leaf is issued for.
pub const DEFAULT_TLS_NAME: &str = "passporter.internal";

/// The `[tls]` table: the bridge's own leaf (say, from a private CA that
/// your reverse proxy trusts), served by [`crate::tls`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    /// PEM certificate chain, leaf first. Relative paths are resolved
    /// against the config directory by [`Config::load`].
    pub cert: PathBuf,
    /// PEM private key (PKCS#8, SEC1 or PKCS#1), mode `0600`. Relative
    /// paths as for [`Self::cert`].
    pub key: PathBuf,
    /// The DNS name the leaf must be valid for: what the reverse proxy
    /// checks. The bridge refuses to start with, or to
    /// reload to, a leaf whose SANs do not cover it, and `passporter
    /// doctor` reports it; once loaded, the one leaf is served whatever
    /// name a client asks for.
    pub name: DnsName<'static>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    cert: PathBuf,
    key: PathBuf,
    name: Option<String>,
}

impl RawTls {
    fn resolve(self) -> Result<TlsConfig, ParseError> {
        let name = match self.name {
            None => DnsName::try_from(DEFAULT_TLS_NAME).map(|name| name.to_owned()),
            Some(name) => DnsName::try_from(name),
        }
        .map_err(|_| ParseError::TlsName)?;
        Ok(TlsConfig {
            cert: self.cert,
            key: self.key,
            name,
        })
    }
}

/// The file's shape: everything optional, unknown keys rejected (typos).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: Option<SocketAddr>,
    static_dir: Option<PathBuf>,
    herdr_socket: Option<PathBuf>,
    trusted_proxy: Option<IpAddr>,
    tls: Option<RawTls>,
    // Validated after parsing, so a parse error never quotes the token.
    api_token: Option<String>,
    allow_static_token: Option<bool>,
    #[serde(default)]
    insecure_http_over_wireguard: bool,
    #[serde(default)]
    insecure_http_allow: Vec<IpAddr>,
    state_dir: Option<PathBuf>,
    audit_log: Option<PathBuf>,
    claude_projects_dir: Option<PathBuf>,
    codex_sessions_dir: Option<PathBuf>,
    push: Option<RawPush>,
    #[serde(default)]
    audio: RawAudio,
    tldr: Option<RawTldr>,
    spawn: Option<RawSpawn>,
    #[serde(default)]
    runbooks: Vec<runbooks::RawRunbook>,
}

/// Why the config could not be loaded. Each variant names the path involved.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Neither a config dir was given nor `HOME` set.
    #[error("cannot locate the config directory: HOME is not set (pass --config-dir)")]
    NoHome,
    /// The directory or file could not be inspected or read.
    #[error("reading {}: {source}", path.display())]
    Io {
        /// What we tried to read.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The path exists but is the wrong kind of file.
    #[error("{} is not a {expected}", path.display())]
    NotA {
        /// The offending path.
        path: PathBuf,
        /// `"directory"` or `"regular file"`.
        expected: &'static str,
    },
    /// Group or other users could access the directory or file.
    #[error(
        "{} is accessible by other users (mode {mode:04o}); run `chmod {want:o} {}`",
        path.display(),
        shell_quote(path)
    )]
    TooOpen {
        /// The offending path.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
        /// The mode to set.
        want: u32,
    },
    /// The file is implausibly large for a config file.
    #[error("{} is larger than {MAX_CONFIG_BYTES} bytes", path.display())]
    TooLarge {
        /// The offending path.
        path: PathBuf,
    },
    /// The file is not valid TOML for [`Config`].
    #[error("parsing {}: {source}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// What was wrong.
        source: Box<ParseError>,
    },
}

/// Why config text is unusable.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Not valid TOML for [`Config`]. Carries only the message and line,
    /// never the quoted source line, which could hold the token.
    #[error("invalid config TOML{}: {message}", line.map(|l| format!(" at line {l}")).unwrap_or_default())]
    Toml {
        message: String,
        line: Option<usize>,
    },
    /// `api_token` is malformed. Never quotes the token.
    #[error(transparent)]
    Token(#[from] InvalidToken),
    /// `allow_static_token` is set without an `api_token` to allow.
    #[error("allow_static_token = true needs an api_token")]
    NoStaticToken,
    /// `api_token` is set, but `allow_static_token` is not: a config from
    /// before the opt-in existed. Ignoring the token would lock out the
    /// phone that uses it, so the bridge refuses to start instead.
    #[error(
        "api_token is set but allow_static_token is not. To keep using the token \
         (only phones on plain HTTP over WireGuard need it), add this line right \
         after the api_token line: `allow_static_token = true`. To refuse it, add \
         `allow_static_token = false` or remove api_token"
    )]
    StaticTokenUndecided,
    /// `allow_static_token = true` alongside `[tls]` or `trusted_proxy`,
    /// when [`STATIC_TOKEN_BEHIND_HTTPS`] refuses it: once the bridge is
    /// behind HTTPS, phones can pair, so a permanent, unrevocable bearer
    /// token has no reason to exist.
    #[error(
        "allow_static_token = true is refused with [tls] or trusted_proxy: behind HTTPS, \
         phones pair instead (`passporter pair`). Set allow_static_token = false or remove \
         api_token"
    )]
    StaticTokenWithHttps,
    /// `[tls]` and a plain-HTTP opt-in are both set.
    #[error(
        "[tls] serves HTTPS only, so insecure_http_over_wireguard and insecure_http_allow \
         must go: remove them, or remove [tls] to stay on plain HTTP over WireGuard"
    )]
    TlsWithPlainHttp,
    /// `tls.name` is not a DNS name.
    #[error("tls.name must be a DNS name, such as \"{DEFAULT_TLS_NAME}\"")]
    TlsName,
    /// `audio.whisper` is not a loopback address.
    #[error("audio.whisper = {0} is not loopback: whisper-server must listen on 127.0.0.1")]
    WhisperNotLoopback(SocketAddr),
    /// `spawn.workspace` is blank, too long or holds control characters.
    #[error(
        "spawn.workspace must be a herdr workspace label of 1 to {MAX_WORKSPACE_LABEL_CHARS} \
         printable characters"
    )]
    SpawnWorkspace,
    /// `spawn.workspace` is one whose finished turns never ping.
    #[error(
        "spawn.workspace = {0:?} is in push.quiet_done_workspaces, so agents started from the \
         phone would never ping when they finish: pick another workspace, or take it off that list"
    )]
    SpawnWorkspaceQuiet(String),
    /// A `push.quiet_done_agent_prefixes` entry is empty or would match the
    /// agents started from the phone.
    #[error(
        "push.quiet_done_agent_prefixes entry {0:?} is empty or matches the names the bridge \
         gives agents started from the phone (pp-...), which must ping when they finish: \
         remove it"
    )]
    QuietDonePrefix(String),
    /// Two `[spawn]` projects share a name.
    #[error("spawn.projects lists {0:?} twice: project names must be unique")]
    SpawnDuplicateProject(String),
    /// A `[spawn]` project path is relative.
    #[error("spawn.projects {0:?}: path must be absolute")]
    SpawnRelativePath(String),
    /// `spawn.kinds` is empty.
    #[error("spawn.kinds must name at least one kind (\"claude\", \"codex\"), or be left out")]
    SpawnNoKinds,
    /// More `[[runbooks]]` than allowed.
    #[error("at most {0} [[runbooks]] are allowed")]
    TooManyRunbooks(usize),
    /// A `[[runbooks]]` entry is unusable.
    #[error("runbook {name:?}: {problem}")]
    Runbook {
        /// The runbook's name.
        name: String,
        /// What is wrong.
        problem: &'static str,
    },
    /// Two runbook names or aliases sound alike: equal once normalised as
    /// the phone matches speech (case, punctuation and filler words).
    #[error(
        "runbook name or alias {0:?} sounds like another one (case, punctuation and filler \
         words like \"um\" or \"please\" are ignored): each phrase must offer one runbook"
    )]
    RunbookPhraseTaken(String),
    /// A runbook name or alias is only punctuation and filler words.
    #[error(
        "runbook name or alias {0:?} has no words left once punctuation and filler words are \
         ignored, so it can never be said"
    )]
    RunbookPhraseUnsayable(String),
    /// A number outside its allowed range.
    #[error("{key} must be between {min} and {max}")]
    OutOfRange {
        /// The offending key.
        key: &'static str,
        /// Smallest allowed value.
        min: u64,
        /// Largest allowed value.
        max: u64,
    },
}

impl Config {
    /// The config directory: `explicit` if given, else `$PASSPORTER_CONFIG_DIR`,
    /// else `~/.config/passporter`.
    pub fn locate_dir(explicit: Option<PathBuf>) -> Result<PathBuf, ConfigError> {
        if let Some(dir) = explicit {
            return Ok(dir);
        }
        if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
            return Ok(PathBuf::from(dir));
        }
        let home = std::env::var_os("HOME").ok_or(ConfigError::NoHome)?;
        Ok(Path::new(&home).join(".config/passporter"))
    }

    /// Loads `<dir>/config.toml`, refusing a directory or file that group or
    /// other users can access.
    pub fn load(dir: &Path) -> Result<Self, ConfigError> {
        let dir_meta = std::fs::metadata(dir).map_err(|source| ConfigError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        if !dir_meta.is_dir() {
            return Err(ConfigError::NotA {
                path: dir.to_path_buf(),
                expected: "directory",
            });
        }
        check_private(dir, dir_meta.permissions().mode() & 0o7777, 0o700)?;

        let file = Self::read_file(dir)?;
        check_private(&file.path, file.mode, 0o600)?;
        Self::from_file(dir, file)
    }

    /// Opens and reads `<dir>/config.toml` (following symlinks, bounded),
    /// refusing anything but a regular file. Checks no permissions:
    /// [`Self::load`] does, and `passporter doctor` reports them; crate
    /// private so nothing else can skip them.
    pub(crate) fn read_file(dir: &Path) -> Result<ConfigFile, ConfigError> {
        let path = dir.join(CONFIG_FILE);
        let io_err = |source| ConfigError::Io {
            path: path.clone(),
            source,
        };
        // Check the opened file, not the path, so it can't be swapped in between.
        let file = File::open(&path).map_err(io_err)?;
        let meta = file.metadata().map_err(io_err)?;
        if !meta.is_file() {
            return Err(ConfigError::NotA {
                path,
                expected: "regular file",
            });
        }
        // Bounded read: the file could grow after the metadata check.
        let mut text = String::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(io_err)?;
        if text.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge { path });
        }
        Ok(ConfigFile {
            mode: meta.permissions().mode() & 0o7777,
            path,
            text,
        })
    }

    /// Parses a file read by [`Self::read_file`] from `dir`, resolving
    /// relative paths (the VAPID key, the TLS files) against `dir`.
    pub(crate) fn from_file(dir: &Path, file: ConfigFile) -> Result<Self, ConfigError> {
        let mut config = Self::parse(&file.text).map_err(|source| ConfigError::Parse {
            path: file.path,
            source: Box::new(source),
        })?;
        if let Some(push) = &mut config.push {
            push.vapid_key = dir.join(&push.vapid_key);
        }
        if let Some(tls) = &mut config.tls {
            tls.cert = dir.join(&tls.cert);
            tls.key = dir.join(&tls.key);
        }
        Ok(config)
    }

    /// Parses config text, filling in defaults. No permission checks.
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        Self::parse_with(text, STATIC_TOKEN_BEHIND_HTTPS)
    }

    /// [`Self::parse`] under an explicit static-token policy.
    pub(crate) fn parse_with(
        text: &str,
        behind_https: StaticTokenBehindHttps,
    ) -> Result<Self, ParseError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ParseError::Toml {
            message: e.message().to_owned(),
            line: e
                .span()
                .map(|span| text[..span.start.min(text.len())].lines().count().max(1)),
        })?;
        let api_token = raw.api_token.map(ApiToken::new).transpose()?;
        let allow_static_token = match (raw.allow_static_token, &api_token) {
            (Some(true), None) => return Err(ParseError::NoStaticToken),
            (None, Some(_)) => return Err(ParseError::StaticTokenUndecided),
            (allow, _) => allow.unwrap_or(false),
        };
        let audio = raw.audio.validate()?;
        let tls = raw.tls.map(RawTls::resolve).transpose()?;
        if allow_static_token
            && (tls.is_some() || raw.trusted_proxy.is_some())
            && behind_https == StaticTokenBehindHttps::Refuse
        {
            return Err(ParseError::StaticTokenWithHttps);
        }
        if tls.is_some()
            && (raw.insecure_http_over_wireguard || !raw.insecure_http_allow.is_empty())
        {
            return Err(ParseError::TlsWithPlainHttp);
        }
        let state_dir = raw.state_dir.unwrap_or_else(default_state_dir);
        let tldr = raw.tldr.unwrap_or_default().resolve()?;
        let push = raw.push.map(RawPush::validate).transpose()?;
        let spawn = raw
            .spawn
            .map(|spawn| spawn.validate(push.as_ref()))
            .transpose()?;
        let runbooks = runbooks::validate(raw.runbooks)?;
        Ok(Self {
            listen: raw.listen.unwrap_or(DEFAULT_LISTEN),
            static_dir: raw.static_dir.unwrap_or_else(default_static_dir),
            herdr_socket: raw.herdr_socket,
            trusted_proxy: raw.trusted_proxy,
            tls,
            api_token,
            allow_static_token,
            insecure_http_over_wireguard: raw.insecure_http_over_wireguard,
            insecure_http_allow: raw.insecure_http_allow,
            audit_log: raw
                .audit_log
                .unwrap_or_else(|| state_dir.join(AUDIT_LOG_FILE)),
            state_dir,
            claude_projects_dir: raw.claude_projects_dir,
            codex_sessions_dir: raw.codex_sessions_dir,
            push,
            audio,
            tldr,
            spawn,
            runbooks,
        })
    }
}

impl Config {
    /// The static token, if the config opts in to it. An `api_token` with
    /// `allow_static_token = false` is refused (the binary says so).
    #[must_use]
    pub fn static_token(&self) -> Option<&ApiToken> {
        self.api_token.as_ref().filter(|_| self.allow_static_token)
    }

    /// Whether the static token is accepted although the bridge is behind
    /// HTTPS (`[tls]` or `trusted_proxy`), where phones could pair instead.
    /// Only possible while [`STATIC_TOKEN_BEHIND_HTTPS`] warns; the bridge
    /// logs it at startup and `passporter doctor` reports it.
    #[must_use]
    pub fn static_token_behind_https(&self) -> bool {
        self.static_token().is_some() && (self.tls.is_some() || self.trusted_proxy.is_some())
    }
}

/// What the bridge does with `allow_static_token = true` alongside `[tls]`
/// or `trusted_proxy`, where phones can pair and a permanent bearer token
/// has no reason to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticTokenBehindHttps {
    /// Load the config; the bridge logs a warning at startup and
    /// `passporter doctor` warns.
    Warn,
    /// Refuse the config ([`ParseError::StaticTokenWithHttps`]).
    Refuse,
}

/// This build's policy: refuse.
pub const STATIC_TOKEN_BEHIND_HTTPS: StaticTokenBehindHttps = StaticTokenBehindHttps::Refuse;

/// The audit log's file name inside the state dir.
pub const AUDIT_LOG_FILE: &str = "audit.jsonl";

/// `$XDG_STATE_HOME/passporter`, else `~/.local/state/passporter`, else a
/// relative `state` without `HOME`.
fn default_state_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Path::new(&xdg).join("passporter");
    }
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("state"),
        |home| Path::new(&home).join(".local/state/passporter"),
    )
}

/// `~/.local/share/passporter/web`, or a relative `web` without `HOME`.
fn default_static_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("web"),
        |home| Path::new(&home).join(".local/share/passporter/web"),
    )
}

/// `path` as one shell word, for copy-paste fix hints: unchanged when it
/// is made of plain characters, else single-quoted.
#[must_use]
pub fn shell_quote(path: &Path) -> String {
    let text = path.display().to_string();
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c));
    if plain {
        text
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// Why [`read_bounded`] or [`read_private`] refused a file.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// It could not be opened or read.
    Io(std::io::Error),
    /// Group or other users can access it ([`read_private`] only).
    TooOpen {
        /// Its permission bits.
        mode: u32,
    },
    /// It is longer than the caller's limit.
    TooLarge,
}

/// Reads all of `file` if it is at most `max` bytes, refusing a longer
/// one (it may grow while being read, so the limit is on what is read).
fn read_to_limit(file: File, max: u64) -> Result<Vec<u8>, ReadError> {
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() as u64 > max {
        return Err(ReadError::TooLarge);
    }
    Ok(bytes)
}

/// Reads the file at `path` if it is at most `max` bytes.
pub(crate) fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>, ReadError> {
    read_to_limit(File::open(path).map_err(ReadError::Io)?, max)
}

/// Reads a secret file: opens it, refuses it if group or other users can
/// access it (judging the opened file, not the path, so it cannot be
/// swapped in between), and reads at most `max` bytes, refusing a longer
/// file. The VAPID and TLS key loaders share it.
pub(crate) fn read_private(path: &Path, max: u64) -> Result<Vec<u8>, ReadError> {
    let file = File::open(path).map_err(ReadError::Io)?;
    let mode = file.metadata().map_err(ReadError::Io)?.permissions().mode() & 0o7777;
    if !is_private_mode(mode) {
        return Err(ReadError::TooOpen { mode });
    }
    read_to_limit(file, max)
}

fn check_private(path: &Path, mode: u32, want: u32) -> Result<(), ConfigError> {
    if is_private_mode(mode) {
        Ok(())
    } else {
        Err(ConfigError::TooOpen {
            path: path.to_path_buf(),
            mode,
            want,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn private_dir(contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("chmod dir");
        let file = dir.path().join(CONFIG_FILE);
        fs::write(&file, contents).expect("write");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod file");
        dir
    }

    #[test]
    fn shell_quote_keeps_plain_paths_and_quotes_the_rest() {
        assert_eq!(
            shell_quote(Path::new("/home/me/.config/x")),
            "/home/me/.config/x"
        );
        assert_eq!(shell_quote(Path::new("/a b/c")), "'/a b/c'");
        assert_eq!(shell_quote(Path::new("/it's;rm")), r"'/it'\''s;rm'");
    }

    #[test]
    fn read_private_refuses_open_and_oversized_files() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("secret");
        fs::write(&path, "12345").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
        assert_eq!(read_private(&path, 5).expect("reads"), b"12345");
        assert!(matches!(read_private(&path, 4), Err(ReadError::TooLarge)));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");
        assert!(matches!(
            read_private(&path, 5),
            Err(ReadError::TooOpen { mode: 0o640 })
        ));
        assert!(matches!(
            read_private(&dir.path().join("missing"), 5),
            Err(ReadError::Io(_))
        ));
    }

    #[test]
    fn empty_file_means_defaults() {
        let config = Config::parse("").expect("parses");
        assert_eq!(config.listen, DEFAULT_LISTEN);
        assert_eq!(config.trusted_proxy, None);
        assert_eq!(config.herdr_socket, None);
        assert_eq!(config.tls, None);
    }

    #[test]
    fn parses_every_field() {
        let config = Config::parse(
            r#"
            listen = "127.0.0.1:0"
            static_dir = "/srv/web"
            herdr_socket = "/run/herdr.sock"
            trusted_proxy = "192.0.2.46"
            codex_sessions_dir = "/srv/codex/sessions"
            [tls]
            cert = "/c.pem"
            key = "/k.pem"
            "#,
        )
        .expect("parses");
        assert_eq!(config.listen, "127.0.0.1:0".parse().expect("addr"));
        assert_eq!(config.static_dir, PathBuf::from("/srv/web"));
        assert_eq!(config.herdr_socket, Some(PathBuf::from("/run/herdr.sock")));
        assert_eq!(
            config.codex_sessions_dir,
            Some(PathBuf::from("/srv/codex/sessions"))
        );
        assert_eq!(
            config.trusted_proxy,
            Some("192.0.2.46".parse().expect("ip"))
        );
        assert_eq!(
            config.tls,
            Some(TlsConfig {
                cert: "/c.pem".into(),
                key: "/k.pem".into(),
                name: DnsName::try_from(DEFAULT_TLS_NAME)
                    .expect("name")
                    .to_owned(),
            })
        );
    }

    #[test]
    fn tls_and_plain_http_are_one_or_the_other() {
        let tls = "listen = \"192.0.2.10:8443\"\n[tls]\ncert = \"c\"\nkey = \"k\"\n";
        assert!(Config::parse(tls).is_ok());
        for opt_in in [
            "insecure_http_over_wireguard = true\n",
            "insecure_http_allow = [\"192.0.2.10\"]\n",
            "insecure_http_over_wireguard = true\ninsecure_http_allow = [\"192.0.2.10\"]\n",
        ] {
            let err = Config::parse(&format!("{opt_in}{tls}")).expect_err(opt_in);
            assert!(matches!(err, ParseError::TlsWithPlainHttp), "{err}");
        }
        // Spelling out the default (off) is fine.
        assert!(Config::parse(&format!("insecure_http_over_wireguard = false\n{tls}")).is_ok());
    }

    #[test]
    fn tls_name_defaults_and_is_validated() {
        let config = Config::parse("[tls]\ncert = \"c\"\nkey = \"k\"\nname = \"bridge.lan\"")
            .expect("parses");
        assert_eq!(config.tls.expect("tls").name.as_ref(), "bridge.lan");
        let err = Config::parse("[tls]\ncert = \"c\"\nkey = \"k\"\nname = \"not a name\"")
            .expect_err("invalid");
        assert!(matches!(err, ParseError::TlsName), "{err}");
    }

    #[test]
    fn relative_tls_paths_resolve_against_the_config_dir() {
        let dir = private_dir("[tls]\ncert = \"tls/leaf.pem\"\nkey = \"/abs/leaf.key\"");
        let tls = Config::load(dir.path()).expect("loads").tls.expect("tls");
        assert_eq!(tls.cert, dir.path().join("tls/leaf.pem"));
        assert_eq!(tls.key, PathBuf::from("/abs/leaf.key"));
    }

    #[test]
    fn toml_errors_never_quote_the_token_line() {
        let secret = "a".repeat(40) + "SECRETPART";
        let text = format!("api_token = \"{secret}\nlisten = \"127.0.0.1:1\"\n");
        let err = Config::parse(&text).expect_err("unterminated string");
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains("SECRETPART"), "{shown}");
        assert!(shown.contains("line"), "{shown}");
    }

    #[test]
    fn parses_the_mvp_fields() {
        let config = Config::parse(
            r#"
            api_token = "0123456789abcdef0123456789abcdef"
            allow_static_token = true
            insecure_http_over_wireguard = true
            insecure_http_allow = ["192.0.2.10"]
            state_dir = "/var/lib/pp"
            "#,
        )
        .expect("parses");
        assert!(config.api_token.is_some());
        assert!(config.insecure_http_over_wireguard);
        assert_eq!(
            config.insecure_http_allow,
            vec!["192.0.2.10".parse::<IpAddr>().expect("ip")]
        );
        assert_eq!(config.audit_log, PathBuf::from("/var/lib/pp/audit.jsonl"));
        let defaults = Config::parse("").expect("parses");
        assert!(defaults.api_token.is_none());
        assert!(!defaults.insecure_http_over_wireguard);
    }

    #[test]
    fn parses_the_push_table() {
        assert_eq!(Config::parse("").expect("parses").push, None);
        let config = Config::parse("[push]\nsubject = \"mailto:me@example.com\"").expect("parses");
        let push = config.push.expect("push");
        assert_eq!(push.vapid_key, PathBuf::from(VAPID_KEY_FILE));
        assert_eq!(push.extra_hosts, Vec::new(), "none by default");
        assert_eq!(push.ntfy_host, None, "none by default");
        assert_eq!(push.car_announce_done, CarAnnounce::Decide);
        assert_eq!(
            push.quiet_done_workspaces,
            owned_set(DEFAULT_QUIET_DONE_WORKSPACES),
            "the build's default"
        );
        let config =
            Config::parse("[push]\nsubject = \"mailto:a@b\"\nextra_hosts = [\"Push.Example.org\"]")
                .expect("parses");
        assert_eq!(
            config.push.expect("push").extra_hosts,
            ["push.example.org".parse::<PushHost>().expect("host")]
        );
        for bad in [
            "127.0.0.1",
            "https://push.example.org",
            "push.example.org:8443",
        ] {
            let text = format!("[push]\nsubject = \"mailto:a@b\"\nextra_hosts = [\"{bad}\"]");
            assert!(Config::parse(&text).is_err(), "{bad}");
        }
        assert!(Config::parse("[push]\nsubject = \"me@example.com\"").is_err());
        assert!(Config::parse("[push]\nsubject = \"mailto:a@b\"\nkey = \"x\"").is_err());
        assert!(Config::parse("[push]").is_err(), "subject is required");
    }

    #[test]
    fn the_ntfy_host_and_car_announcements_are_configurable() {
        let push = |lines: &str| {
            Config::parse(&format!("[push]\nsubject = \"mailto:a@b\"\n{lines}"))
                .map(|config| config.push.expect("push"))
        };
        let config = push("ntfy_host = \"Push.App.Example.org\"\ncar_announce_done = \"always\"")
            .expect("parses");
        assert_eq!(
            config.ntfy_host,
            Some("push.app.example.org".parse().expect("host"))
        );
        assert_eq!(config.car_announce_done, CarAnnounce::Always);
        assert_eq!(
            push("car_announce_done = \"never\"")
                .expect("parses")
                .car_announce_done,
            CarAnnounce::Never
        );
        assert!(push("car_announce_done = \"sometimes\"").is_err());
        assert!(push("ntfy_host = \"https://push.app.example.org\"").is_err());
        assert!(push("ntfy_host = \"10.0.0.5\"").is_err());
    }

    #[test]
    fn quiet_done_workspaces_can_be_listed_or_emptied() {
        let quiet = |list: &str| {
            let text = format!("[push]\nsubject = \"mailto:a@b\"\nquiet_done_workspaces = {list}");
            Config::parse(&text)
                .expect("parses")
                .push
                .expect("push")
                .quiet_done_workspaces
        };
        assert_eq!(
            quiet(r#"["swarm", "night-shift"]"#),
            BTreeSet::from(["swarm".to_owned(), "night-shift".to_owned()])
        );
        assert_eq!(quiet("[]"), BTreeSet::new(), "empty: every finish pings");
    }

    #[test]
    fn quiet_done_agent_prefixes_default_to_the_builds_and_can_be_listed_or_emptied() {
        let prefixes = |line: &str| {
            let text = format!("[push]\nsubject = \"mailto:a@b\"\n{line}");
            Config::parse(&text).map(|config| config.push.expect("push").quiet_done_agent_prefixes)
        };
        let set = |list: &[&str]| list.iter().map(|&p| p.to_owned()).collect::<BTreeSet<_>>();
        assert_eq!(
            prefixes("").expect("parses"),
            owned_set(DEFAULT_QUIET_DONE_AGENT_PREFIXES),
            "the build's default"
        );
        assert_eq!(
            prefixes(r#"quiet_done_agent_prefixes = ["impl-", "rev-"]"#).expect("parses"),
            set(&["impl-", "rev-"])
        );
        assert_eq!(
            prefixes("quiet_done_agent_prefixes = []").expect("parses"),
            BTreeSet::new(),
            "empty: names quiet nothing"
        );
        assert_eq!(
            prefixes(r#"quiet_done_agent_prefixes = ["night-"]"#).expect("parses"),
            set(&["night-"])
        );
        for bad in ["", "p", "pp", "pp-", "pp-0a"] {
            let err = prefixes(&format!("quiet_done_agent_prefixes = [{bad:?}]")).expect_err(bad);
            assert!(
                matches!(err, ParseError::QuietDonePrefix(_)),
                "{bad}: {err}"
            );
        }
        prefixes(r#"quiet_done_agent_prefixes = ["ppx", "pp-z"]"#).expect("never a phone name");
    }

    #[test]
    fn load_resolves_the_vapid_key_against_the_config_dir() {
        let dir = private_dir("[push]\nsubject = \"mailto:me@example.com\"\n");
        let push = Config::load(dir.path()).expect("loads").push.expect("push");
        assert_eq!(push.vapid_key, dir.path().join(VAPID_KEY_FILE));
        let dir = private_dir(
            "[push]\nsubject = \"mailto:me@example.com\"\nvapid_key = \"/etc/pp/k.pem\"\n",
        );
        let push = Config::load(dir.path()).expect("loads").push.expect("push");
        assert_eq!(push.vapid_key, PathBuf::from("/etc/pp/k.pem"));
    }

    #[test]
    fn the_static_token_needs_an_explicit_choice() {
        let token = "api_token = \"0123456789abcdef0123456789abcdef\"\n";
        let refused = Config::parse(&format!("{token}allow_static_token = false")).expect("parses");
        assert!(refused.api_token.is_some());
        assert!(refused.static_token().is_none());
        let opted_in = Config::parse(&format!("{token}allow_static_token = true")).expect("parses");
        assert!(opted_in.static_token().is_some());
        let undecided = Config::parse(token).expect_err("undecided");
        assert!(matches!(undecided, ParseError::StaticTokenUndecided));
        assert!(
            undecided
                .to_string()
                .contains("`allow_static_token = true`"),
            "{undecided}"
        );
        assert!(!undecided.to_string().contains("0123456789abcdef"));
        assert!(matches!(
            Config::parse("allow_static_token = true"),
            Err(ParseError::NoStaticToken)
        ));
    }

    #[test]
    fn the_static_token_behind_https_is_refused_or_warned_about() {
        let token = "api_token = \"0123456789abcdef0123456789abcdef\"\n";
        for https in [
            "trusted_proxy = \"127.0.0.1\"\n",
            "[tls]\ncert = \"c\"\nkey = \"k\"\n",
        ] {
            let opted_in = format!("{token}allow_static_token = true\n{https}");
            let err =
                Config::parse_with(&opted_in, StaticTokenBehindHttps::Refuse).expect_err(https);
            assert!(matches!(err, ParseError::StaticTokenWithHttps), "{err}");
            assert!(!err.to_string().contains("0123456789abcdef"));
            let warned = Config::parse_with(&opted_in, StaticTokenBehindHttps::Warn)
                .expect("loads under the warning");
            assert!(warned.static_token_behind_https(), "{https}");
            let refused = format!("{token}allow_static_token = false\n{https}");
            for policy in [StaticTokenBehindHttps::Warn, StaticTokenBehindHttps::Refuse] {
                let config = Config::parse_with(&refused, policy).expect("parses");
                assert!(!config.static_token_behind_https(), "{https}");
            }
        }
        let mvp = format!("{token}allow_static_token = true\n");
        let config = Config::parse_with(&mvp, StaticTokenBehindHttps::Refuse).expect("parses");
        assert!(
            !config.static_token_behind_https(),
            "plain HTTP is not behind HTTPS"
        );
    }

    /// An MVP-mode config with a token but no `allow_static_token` choice
    /// (an obviously fake token).
    const PRE_OPT_IN_SETUP_CONFIG: &str = r#"# passporter-bridge config (MVP mode; see crates/bridge/README.md).
listen = "192.0.2.10:8443"
insecure_http_over_wireguard = true
insecure_http_allow = ["192.0.2.10"]
api_token = "feedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedface"
static_dir = "/home/me/git/passporter/crates/web/dist"
state_dir = "/home/me/.local/state/passporter"
"#;

    #[test]
    fn the_pre_opt_in_setup_config_fails_loudly_and_says_how_to_fix_it() {
        let err = Config::parse(PRE_OPT_IN_SETUP_CONFIG).expect_err("must not start");
        assert!(matches!(err, ParseError::StaticTokenUndecided), "{err}");
        let shown = err.to_string();
        assert!(shown.contains("allow_static_token = true"), "{shown}");
        assert!(
            !shown.contains("feedface"),
            "never quotes the token: {shown}"
        );
        // With the explicit opt-in, it loads and keeps the token.
        let migrated = PRE_OPT_IN_SETUP_CONFIG.replace(
            "api_token = \"feed",
            "allow_static_token = true\napi_token = \"feed",
        );
        let config = Config::parse(&migrated).expect("migrated config loads");
        assert!(config.static_token().is_some());
        assert!(config.insecure_http_over_wireguard);
    }

    #[test]
    fn parses_the_tldr_table() {
        let defaults = Config::parse("").expect("parses");
        assert_eq!(defaults.tldr, TldrConfig::default());
        assert!(defaults.tldr.enabled);

        let config =
            Config::parse("[tldr]\nenabled = false\nclaude = \"/opt/claude\"\ntimeout_secs = 30")
                .expect("parses");
        assert_eq!(
            config.tldr,
            TldrConfig {
                enabled: false,
                claude: PathBuf::from("/opt/claude"),
                timeout: Duration::from_secs(30),
            }
        );
        assert!(Config::parse("[tldr]\ntimeout_secs = 0").is_err());
        assert!(Config::parse("[tldr]\ntimeout_secs = 601").is_err());
        assert!(Config::parse("[tldr]\nmodel = \"x\"").is_err());
    }

    #[test]
    fn parses_the_spawn_table() {
        assert_eq!(Config::parse("").expect("parses").spawn, None);
        let spawn = Config::parse(
            r#"
            [spawn]
            projects = [
              { name = "passporter", path = "/home/me/git/passporter" },
              { name = "web", path = "/srv/web" },
            ]
            "#,
        )
        .expect("parses")
        .spawn
        .expect("spawn");
        assert_eq!(spawn.workspace, DEFAULT_SPAWN_WORKSPACE);
        assert_eq!(spawn.kinds, SpawnKind::ALL);
        let names: Vec<&str> = spawn.projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["passporter", "web"], "config order");
        assert_eq!(spawn.projects[1].path, PathBuf::from("/srv/web"));

        let only_claude =
            Config::parse("[spawn]\nworkspace = \"phone\"\nkinds = [\"claude\", \"claude\"]")
                .expect("parses")
                .spawn
                .expect("spawn");
        assert_eq!(only_claude.workspace, "phone");
        assert_eq!(only_claude.kinds, [SpawnKind::Claude], "deduplicated");
    }

    #[test]
    fn spawn_values_are_checked() {
        let cases = [
            ("[spawn]\nkinds = []", "no kinds"),
            ("[spawn]\nkinds = [\"pi\"]", "unknown kind"),
            ("[spawn]\nworkspace = \" \"", "blank workspace"),
            (
                "[spawn]\nprojects = [{ name = \"a\", path = \"rel\" }]",
                "relative path",
            ),
            (
                "[spawn]\nprojects = [{ name = \"../x\", path = \"/x\" }]",
                "bad name",
            ),
            (
                "[spawn]\nprojects = [{ name = \"a\", path = \"/a\" }, { name = \"a\", path = \"/b\" }]",
                "duplicate",
            ),
            (
                "[spawn]\nprojects = [{ name = \"a\", path = \"/a\", args = [\"--yolo\"] }]",
                "no args",
            ),
            (
                "[spawn]\nargs = [\"--dangerously-skip-permissions\"]",
                "no args",
            ),
        ];
        for (text, what) in cases {
            assert!(Config::parse(text).is_err(), "{what}");
        }
    }

    #[test]
    fn the_spawn_workspace_is_never_a_quiet_one() {
        let quiet = "[push]\nsubject = \"mailto:a@b\"\nquiet_done_workspaces = [\"agent-swarm\"]\n\
                     [spawn]\nworkspace = \"agent-swarm\"";
        let err = Config::parse(quiet).expect_err("quiet");
        assert!(matches!(err, ParseError::SpawnWorkspaceQuiet(_)), "{err}");
        // The default workspace pings with the default push settings.
        let config = Config::parse("[push]\nsubject = \"mailto:a@b\"\n[spawn]").expect("parses");
        let spawn = config.spawn.expect("spawn");
        assert!(
            !config
                .push
                .expect("push")
                .quiet_done_workspaces
                .contains(&spawn.workspace)
        );
    }

    #[test]
    fn a_short_token_is_rejected_without_echoing_it() {
        let err = Config::parse(r#"api_token = "hunter2""#).expect_err("too short");
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::parse("lisen = \"127.0.0.1:1\"").is_err());
        assert!(Config::parse("[tls]\ncert = \"a\"\nkey = \"b\"\nca = \"c\"").is_err());
    }

    #[test]
    fn loads_a_private_config() {
        let dir = private_dir("listen = \"127.0.0.1:9\"");
        let config = Config::load(dir.path()).expect("loads");
        assert_eq!(config.listen.port(), 9);
    }

    #[test]
    fn refuses_a_group_or_world_readable_file() {
        for mode in [0o640, 0o604, 0o660, 0o666] {
            let dir = private_dir("");
            let file = dir.path().join(CONFIG_FILE);
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).expect("chmod");
            let err = Config::load(dir.path()).expect_err("too open");
            assert!(
                matches!(err, ConfigError::TooOpen { mode: m, want: 0o600, .. } if m == mode),
                "{mode:o}: {err}"
            );
        }
    }

    #[test]
    fn refuses_an_open_directory() {
        let dir = private_dir("");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o750)).expect("chmod");
        let err = Config::load(dir.path()).expect_err("too open");
        assert!(
            matches!(err, ConfigError::TooOpen { want: 0o700, .. }),
            "{err}"
        );
        assert!(err.to_string().contains("chmod 700"), "{err}");
    }

    #[test]
    fn stricter_modes_are_fine() {
        let dir = private_dir("");
        let file = dir.path().join(CONFIG_FILE);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).expect("chmod");
        Config::load(dir.path()).expect("0400 is private");
    }

    #[test]
    fn missing_file_is_an_io_error_naming_it() {
        let dir = private_dir("");
        fs::remove_file(dir.path().join(CONFIG_FILE)).expect("rm");
        let err = Config::load(dir.path()).expect_err("missing");
        assert!(matches!(err, ConfigError::Io { .. }));
        assert!(err.to_string().contains(CONFIG_FILE), "{err}");
    }

    #[test]
    fn a_directory_in_place_of_the_file_is_refused() {
        let dir = private_dir("");
        let file = dir.path().join(CONFIG_FILE);
        fs::remove_file(&file).expect("rm");
        fs::create_dir(&file).expect("mkdir");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).expect("chmod");
        let err = Config::load(dir.path()).expect_err("not a file");
        assert!(matches!(err, ConfigError::NotA { .. }), "{err}");
    }

    #[test]
    fn oversized_file_is_refused() {
        let big = format!(
            "# {}\n",
            "x".repeat(usize::try_from(MAX_CONFIG_BYTES).expect("small"))
        );
        let dir = private_dir(&big);
        let err = Config::load(dir.path()).expect_err("too large");
        assert!(matches!(err, ConfigError::TooLarge { .. }), "{err}");
    }

    #[test]
    fn invalid_toml_names_the_file() {
        let dir = private_dir("listen = 5");
        let err = Config::load(dir.path()).expect_err("bad type");
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(err.to_string().contains(CONFIG_FILE), "{err}");
    }

    #[test]
    fn audio_defaults_and_overrides() {
        assert_eq!(
            Config::parse("").expect("parses").audio,
            AudioConfig::default()
        );
        let audio = Config::parse(
            "[audio]\nwhisper = \"127.0.0.1:9000\"\nretention_hours = 0\n\
             max_upload_mib = 10\nmax_duration_secs = 60",
        )
        .expect("parses")
        .audio;
        assert_eq!(audio.whisper.port(), 9000);
        assert_eq!(audio.retention, Duration::ZERO);
        assert_eq!(audio.max_upload_bytes, 10 * MIB);
        assert_eq!(audio.max_duration, Duration::from_secs(60));
    }

    #[test]
    fn audio_values_are_checked() {
        let err = Config::parse("[audio]\nwhisper = \"192.0.2.10:8178\"").expect_err("lan");
        assert!(matches!(err, ParseError::WhisperNotLoopback(_)), "{err}");
        for bad in [
            "max_upload_mib = 0",
            "max_duration_secs = 99999",
            "retention_hours = 100000",
        ] {
            let err = Config::parse(&format!("[audio]\n{bad}")).expect_err(bad);
            assert!(matches!(err, ParseError::OutOfRange { .. }), "{bad}: {err}");
        }
        assert!(Config::parse("[audio]\nmodel = \"x\"").is_err());
    }

    #[test]
    fn a_read_config_file_never_debug_prints_its_text() {
        let dir = private_dir("api_token = \"0123456789abcdef0123456789abcdefSECRET\"\n");
        let file = Config::read_file(dir.path()).expect("reads");
        let shown = format!("{file:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(
            shown.contains("redacted") && shown.contains("0600"),
            "{shown}"
        );
    }

    #[test]
    fn explicit_dir_wins() {
        let dir = Config::locate_dir(Some("/x/y".into())).expect("explicit");
        assert_eq!(dir, PathBuf::from("/x/y"));
    }
}
