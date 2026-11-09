//! `passporter doctor` against temp dirs, `FakeHerdr`, a loopback stand-in
//! for whisper-server and a fake systemd. Each check is driven to pass and
//! to fail, and no report ever contains the static token.

use std::fs;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use bridge::config::{STATIC_TOKEN_BEHIND_HTTPS, StaticTokenBehindHttps};
use bridge::doctor::{
    self, Active, Check, Enabled, Finding, Report, Status, UnitError, UnitProbe, UnitState,
};
use bridge::push::VapidKey;
use herdr::{ErrorCode, FakeHerdr, Fault, Method, SocketClient};
use time::OffsetDateTime;
use tokio::net::TcpListener;

use crate::pki::{self, Ca};

/// A token-shaped secret that must never reach a report.
const TOKEN: &str = "c0ffee5ecre7c0ffee5ecre7c0ffee5ecre7c0ffee5ecre7c0ffee5ecre7beef";

const TIMEOUT: Duration = Duration::from_secs(2);

/// A systemd that answers with a fixed state.
enum FakeUnit {
    Answers(UnitState),
    NoBus,
}

impl FakeUnit {
    /// A unit that does not restrict writes nor name its config dir.
    fn with(enabled: Enabled, active: Active) -> Self {
        Self::Answers(UnitState {
            enabled,
            active,
            writable: Vec::new(),
            config_dir: None,
        })
    }

    /// The shipped unit: running, writes only in `writable`, serving `config_dir`.
    fn shipped(writable: &Path, config_dir: &Path) -> Self {
        Self::Answers(UnitState {
            enabled: Enabled::Enabled,
            active: Active::Active,
            writable: vec![writable.to_path_buf()],
            config_dir: Some(config_dir.to_path_buf()),
        })
    }

    fn running() -> Self {
        Self::with(Enabled::Enabled, Active::Active)
    }
}

impl UnitProbe for FakeUnit {
    fn state(
        &self,
        _timeout: Duration,
    ) -> impl Future<Output = Result<UnitState, UnitError>> + Send {
        let state = match self {
            Self::Answers(state) => Ok(state.clone()),
            Self::NoBus => Err(UnitError::NoAnswer("Failed to connect to bus".to_owned())),
        };
        std::future::ready(state)
    }
}

/// The switched-over transport: HTTPS behind the edge.
const HTTPS: &str = "listen = \"192.0.2.10:8443\"\ntrusted_proxy = \"192.0.2.46\"\n";

/// The `[tls]` table for the leaf [`Deployment::new`] installs.
const TLS: &str = "[tls]\ncert = \"tls/leaf.pem\"\nkey = \"tls/leaf.key\"\n";

/// A deployment in a temp dir: a private config dir and state dir, a
/// listening whisper stand-in, a leaf from a throwaway CA in
/// `<config>/tls`, and a config written by [`Self::config`].
struct Deployment {
    root: tempfile::TempDir,
    /// Issues the deployment's leaves.
    ca: Ca,
    /// The whisper stand-in, listening on 127.0.0.1 for the whole test.
    whisper: TcpListener,
    /// What the config points whisper at (normally `whisper`'s address).
    whisper_addr: SocketAddr,
}

impl Deployment {
    /// A healthy deployment: HTTPS with a valid leaf behind the edge,
    /// pairing only, push on, whisper up.
    async fn new() -> Self {
        let root = tempfile::tempdir().expect("tmp");
        let whisper = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let whisper_addr = whisper.local_addr().expect("addr");
        let deployment = Self {
            root,
            ca: Ca::new(),
            whisper,
            whisper_addr,
        };
        fs::create_dir(deployment.config_dir()).expect("mkdir config");
        chmod(&deployment.config_dir(), 0o700);
        fs::create_dir(deployment.state_dir()).expect("mkdir state");
        chmod(&deployment.state_dir(), 0o700);
        VapidKey::generate(&deployment.config_dir().join("vapid.pem")).expect("vapid key");
        fs::create_dir(deployment.tls_dir()).expect("mkdir tls");
        deployment.ca.leaf().install(&deployment.tls_dir());
        deployment.config(
            HTTPS,
            &format!("{TLS}[push]\nsubject = \"mailto:me@example.com\"\n"),
        );
        deployment
    }

    fn tls_dir(&self) -> PathBuf {
        self.config_dir().join("tls")
    }

    fn config_dir(&self) -> PathBuf {
        self.root.path().join("config")
    }

    fn state_dir(&self) -> PathBuf {
        self.root.path().join("state")
    }

    /// Writes `config.toml` (0600): `top` keys, the paths and whisper port
    /// of this deployment, then `tables`.
    fn config(&self, top: &str, tables: &str) {
        let text = format!(
            "{top}state_dir = \"{}\"\nherdr_socket = \"{}\"\n[audio]\nwhisper = \"{}\"\n{tables}",
            self.state_dir().display(),
            self.root.path().join("herdr.sock").display(),
            self.whisper_addr,
        );
        let file = self.config_dir().join("config.toml");
        fs::write(&file, text).expect("write config");
        chmod(&file, 0o600);
    }

    async fn doctor_with(&self, herdr: FakeHerdr, unit: &FakeUnit) -> Report {
        let report = doctor::run(&self.config_dir(), |_| herdr, unit, TIMEOUT).await;
        assert_no_secret(&report);
        report
    }

    async fn doctor(&self) -> Report {
        self.doctor_with(FakeHerdr::new(), &FakeUnit::running())
            .await
    }
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

fn assert_no_secret(report: &Report) {
    let shown = format!("{report}\n{report:?}");
    assert!(!shown.contains(TOKEN), "the token leaked: {shown}");
    assert!(
        !shown.contains("PRIVATE KEY"),
        "key material leaked: {shown}"
    );
}

fn finding(report: &Report, check: Check) -> &Finding {
    report
        .finding(check)
        .unwrap_or_else(|| panic!("{check:?} did not run:\n{report}"))
}

#[track_caller]
fn expect(report: &Report, check: Check, status: Status, needle: &str) {
    let found = finding(report, check);
    assert_eq!(found.status, status, "{check:?}:\n{report}");
    let shown = format!(
        "{} {}",
        found.detail,
        found.hint.as_deref().unwrap_or_default()
    );
    assert!(
        shown.contains(needle),
        "{check:?} should mention {needle:?}:\n{report}"
    );
    assert_eq!(
        found.hint.is_none(),
        status == Status::Ok,
        "a hint exactly when not ok:\n{report}"
    );
}

/// The MVP config `deploy/bridge/setup.sh` writes, with [`TOKEN`].
const MVP: &str = "listen = \"192.0.2.10:8443\"\ninsecure_http_over_wireguard = true\n\
                   insecure_http_allow = [\"192.0.2.10\"]\n";

fn with_token(top: &str, allow: bool) -> String {
    format!("{top}api_token = \"{TOKEN}\"\nallow_static_token = {allow}\n")
}

#[tokio::test]
async fn a_healthy_deployment_passes_every_check() {
    let deployment = Deployment::new().await;
    let report = deployment.doctor().await;
    let checks: Vec<Check> = report.findings.iter().map(|f| f.check).collect();
    assert_eq!(
        checks,
        [
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
        ],
        "{report}"
    );
    for found in &report.findings {
        assert_eq!(found.status, Status::Ok, "{report}");
    }
    assert!(report.healthy());
    assert!(
        report.to_string().ends_with("All checks passed.\n"),
        "{report}"
    );
}

#[tokio::test]
async fn config_dir_mode_and_existence() {
    let deployment = Deployment::new().await;
    expect(
        &deployment.doctor().await,
        Check::ConfigDir,
        Status::Ok,
        "0700",
    );

    chmod(&deployment.config_dir(), 0o750);
    let report = deployment.doctor().await;
    expect(&report, Check::ConfigDir, Status::Fail, "chmod 700");
    assert!(
        report.finding(Check::Unit).is_some(),
        "later checks still run"
    );
    chmod(&deployment.config_dir(), 0o700);

    let missing = doctor::run(
        &deployment.root.path().join("nowhere"),
        |_| FakeHerdr::new(),
        &FakeUnit::running(),
        TIMEOUT,
    )
    .await;
    expect(&missing, Check::ConfigDir, Status::Fail, "setup.sh");
    expect(&missing, Check::ConfigFile, Status::Fail, "does not exist");
    assert!(missing.finding(Check::Listen).is_none(), "{missing}");
    assert!(missing.to_string().contains("need a config that loads"));
    assert!(!missing.healthy());
}

#[tokio::test]
async fn config_file_mode() {
    let deployment = Deployment::new().await;
    expect(
        &deployment.doctor().await,
        Check::ConfigFile,
        Status::Ok,
        "0600",
    );
    let file = deployment.config_dir().join("config.toml");
    chmod(&file, 0o644);
    let report = deployment.doctor().await;
    expect(&report, Check::ConfigFile, Status::Fail, "chmod 600");
    // Still parsed, so the rest of the report is useful.
    expect(&report, Check::Config, Status::Ok, "loads");
    expect(&report, Check::Listen, Status::Ok, "HTTPS");
}

#[tokio::test]
async fn a_config_that_does_not_load_never_quotes_the_token() {
    let deployment = Deployment::new().await;
    // An unterminated string on the token line, then the pre-opt-in shape.
    for top in [
        format!("api_token = \"{TOKEN}\nlisten = \"127.0.0.1:1\"\n"),
        format!("{MVP}api_token = \"{TOKEN}\"\n"),
    ] {
        deployment.config(&top, "");
        let report = deployment.doctor().await;
        expect(&report, Check::Config, Status::Fail, "config.toml");
        assert!(report.finding(Check::Auth).is_none(), "{report}");
        assert!(
            report.to_string().contains("need a config that loads"),
            "{report}"
        );
    }
    let report = deployment.doctor().await;
    expect(
        &report,
        Check::Config,
        Status::Fail,
        "allow_static_token = true",
    );
}

#[tokio::test]
async fn state_dir_modes() {
    let deployment = Deployment::new().await;
    expect(
        &deployment.doctor().await,
        Check::StateDir,
        Status::Ok,
        "private",
    );

    let audit = deployment.state_dir().join("audit.jsonl");
    fs::write(&audit, "").expect("write");
    chmod(&audit, 0o644);
    expect(
        &deployment.doctor().await,
        Check::StateDir,
        Status::Fail,
        "audit.jsonl",
    );
    chmod(&audit, 0o600);

    chmod(&deployment.state_dir(), 0o755);
    expect(
        &deployment.doctor().await,
        Check::StateDir,
        Status::Fail,
        "chmod 700",
    );

    // Listable is part of the check: a dir doctor cannot read is not "private".
    chmod(&deployment.state_dir(), 0o300);
    let report = deployment.doctor().await;
    chmod(&deployment.state_dir(), 0o700);
    if !running_as_root() {
        expect(&report, Check::StateDir, Status::Fail, "cannot list");
    }

    // Missing: under the unit (ProtectSystem=strict) nothing will create it.
    fs::remove_file(&audit).expect("rm");
    fs::remove_dir(deployment.state_dir()).expect("rmdir");
    expect(
        &deployment.doctor().await,
        Check::StateDir,
        Status::Fail,
        "install -d -m 0700",
    );
    let no_unit = FakeUnit::with(Enabled::NotFound, Active::Inactive);
    let report = deployment.doctor_with(FakeHerdr::new(), &no_unit).await;
    expect(
        &report,
        Check::StateDir,
        Status::Warn,
        "run by hand creates it",
    );
}

#[tokio::test]
async fn under_the_unit_the_state_must_be_inside_its_read_write_paths() {
    let deployment = Deployment::new().await;
    let config_dir = deployment.config_dir();
    let inside = FakeUnit::shipped(deployment.root.path(), &config_dir);
    let report = deployment.doctor_with(FakeHerdr::new(), &inside).await;
    expect(&report, Check::StateDir, Status::Ok, "private");
    expect(&report, Check::Unit, Status::Ok, "enabled and active");

    let elsewhere = deployment.root.path().join("elsewhere");
    let outside = FakeUnit::shipped(&elsewhere, &config_dir);
    let report = deployment.doctor_with(FakeHerdr::new(), &outside).await;
    expect(
        &report,
        Check::StateDir,
        Status::Fail,
        "outside the unit's ReadWritePaths",
    );

    // A unit running another config dir sets no limits on this one.
    let other = FakeUnit::shipped(&elsewhere, &elsewhere);
    let report = deployment.doctor_with(FakeHerdr::new(), &other).await;
    expect(&report, Check::StateDir, Status::Ok, "private");
    expect(&report, Check::Unit, Status::Warn, "--config-dir");
}

fn running_as_root() -> bool {
    // Root reads any dir regardless of mode, so the unlistable case can't fail.
    fs::metadata("/proc/self").is_ok_and(|meta| std::os::unix::fs::MetadataExt::uid(&meta) == 0)
}

#[tokio::test]
async fn the_audit_log_is_checked_wherever_it_lives() {
    let deployment = Deployment::new().await;
    let push = "[push]\nsubject = \"mailto:me@example.com\"\n";

    // Nested below a subdirectory of the state dir.
    let logs = deployment.state_dir().join("logs");
    fs::create_dir(&logs).expect("mkdir");
    chmod(&logs, 0o700);
    let nested = logs.join("audit.jsonl");
    fs::write(&nested, "").expect("write");
    chmod(&nested, 0o644);
    deployment.config(
        &format!(
            "listen = \"127.0.0.1:8443\"\naudit_log = \"{}\"\n",
            nested.display()
        ),
        push,
    );
    expect(
        &deployment.doctor().await,
        Check::StateDir,
        Status::Fail,
        "logs/audit.jsonl",
    );
    chmod(&nested, 0o600);
    expect(
        &deployment.doctor().await,
        Check::StateDir,
        Status::Ok,
        "private",
    );

    // Outside it, through a symlink to a private file: judged by the file.
    let elsewhere = deployment.root.path().join("audit-target.jsonl");
    fs::write(&elsewhere, "").expect("write");
    chmod(&elsewhere, 0o600);
    let link = deployment.root.path().join("audit.jsonl");
    std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
    deployment.config(
        &format!(
            "listen = \"127.0.0.1:8443\"\naudit_log = \"{}\"\n",
            link.display()
        ),
        push,
    );
    expect(
        &deployment.doctor().await,
        Check::StateDir,
        Status::Ok,
        "private",
    );
    chmod(&elsewhere, 0o644);
    expect(
        &deployment.doctor().await,
        Check::StateDir,
        Status::Fail,
        "audit.jsonl",
    );

    // A symlink inside the state dir is skipped, not failed on its 0777.
    chmod(&elsewhere, 0o600);
    std::os::unix::fs::symlink(&elsewhere, deployment.state_dir().join("link")).expect("symlink");
    expect(
        &deployment.doctor().await,
        Check::StateDir,
        Status::Ok,
        "private",
    );
}

#[tokio::test]
async fn listen_against_the_allowlist_and_the_wireguard_opt_in() {
    let deployment = Deployment::new().await;
    // 0.0.0.0 can never be allowlisted, so the hint must not suggest it.
    deployment.config("listen = \"0.0.0.0:8443\"\n", "");
    let report = deployment.doctor().await;
    expect(
        &report,
        Check::Listen,
        Status::Fail,
        "never allowed on 0.0.0.0",
    );
    let hint = finding(&report, Check::Listen)
        .hint
        .clone()
        .unwrap_or_default();
    assert!(!hint.contains("[\"0.0.0.0\"]"), "{hint}");
    deployment.config("listen = \"127.0.0.1:8443\"\n", "");
    expect(
        &deployment.doctor().await,
        Check::Listen,
        Status::Ok,
        "plain HTTP on loopback",
    );

    deployment.config(&with_token(MVP, true), "");
    expect(
        &deployment.doctor().await,
        Check::Listen,
        Status::Warn,
        "only from your WireGuard subnet",
    );

    deployment.config("listen = \"192.0.2.10:8443\"\n", "");
    expect(
        &deployment.doctor().await,
        Check::Listen,
        Status::Fail,
        "insecure_http_over_wireguard = true",
    );

    deployment.config(
        "listen = \"192.0.2.11:8443\"\ninsecure_http_over_wireguard = true\n\
         insecure_http_allow = [\"192.0.2.10\"]\n",
        "",
    );
    expect(
        &deployment.doctor().await,
        Check::Listen,
        Status::Fail,
        "insecure_http_allow",
    );

    deployment.config(
        "listen = \"0.0.0.0:8443\"\ninsecure_http_over_wireguard = true\n\
         insecure_http_allow = [\"0.0.0.0\"]\n",
        "",
    );
    expect(
        &deployment.doctor().await,
        Check::Listen,
        Status::Fail,
        "never 0.0.0.0",
    );

    // HTTPS anywhere, and X-Forwarded-For only from the edge.
    deployment.config("listen = \"192.0.2.10:8443\"\n", TLS);
    expect(
        &deployment.doctor().await,
        Check::Listen,
        Status::Warn,
        "trusted_proxy = \"<proxy IP>\"",
    );
    deployment.config(
        "listen = \"192.0.2.10:8443\"\ntrusted_proxy = \"192.0.2.46\"\n",
        TLS,
    );
    expect(
        &deployment.doctor().await,
        Check::Listen,
        Status::Ok,
        "believed only from 192.0.2.46",
    );

    // A proxy on another host cannot reach loopback.
    deployment.config(
        "listen = \"127.0.0.1:8443\"\ntrusted_proxy = \"192.0.2.46\"\n",
        TLS,
    );
    expect(
        &deployment.doctor().await,
        Check::Listen,
        Status::Warn,
        "only a proxy on this machine",
    );

    // [tls] and the plain-HTTP opt-in are one or the other.
    deployment.config(MVP, TLS);
    expect(
        &deployment.doctor().await,
        Check::Config,
        Status::Fail,
        "insecure_http_over_wireguard",
    );
}

#[tokio::test]
async fn tls_leaf_key_name_and_expiry() {
    let deployment = Deployment::new().await;
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Ok,
        "leaf for passporter.internal, key matches",
    );
    let now = OffsetDateTime::now_utc();

    deployment
        .ca
        .issue(&[pki::NAME], now - time::Duration::days(1))
        .install(&deployment.tls_dir());
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Fail,
        "expired",
    );

    deployment
        .ca
        .issue_from(
            &[pki::NAME],
            now + time::Duration::days(1),
            now + time::Duration::days(90),
        )
        .install(&deployment.tls_dir());
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Fail,
        "not valid until",
    );

    deployment
        .ca
        .issue(&[pki::NAME], now + time::Duration::days(5))
        .install(&deployment.tls_dir());
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Warn,
        "renew the leaf",
    );

    deployment
        .ca
        .issue(&["devbox.example.test"], now + time::Duration::days(90))
        .install(&deployment.tls_dir());
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Fail,
        "not valid for passporter.internal",
    );
    // ... which is fine when the edge is set up to verify that name.
    deployment.config(HTTPS, &format!("{TLS}name = \"devbox.example.test\"\n"));
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Ok,
        "leaf for devbox.example.test",
    );
    deployment.config(HTTPS, TLS);

    let leaf = deployment.ca.leaf();
    leaf.install(&deployment.tls_dir());
    let key = deployment.tls_dir().join(pki::KEY_FILE);
    fs::write(&key, deployment.ca.leaf().key).expect("write key");
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Fail,
        "does not match",
    );

    leaf.install(&deployment.tls_dir());
    chmod(&key, 0o640);
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Fail,
        "chmod 600",
    );

    fs::remove_file(&key).expect("rm");
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Fail,
        "install the leaf",
    );
}

#[tokio::test]
async fn without_tls_only_loopback_behind_a_local_proxy_is_fine() {
    let deployment = Deployment::new().await;
    deployment.config("listen = \"127.0.0.1:8443\"\n", "");
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Warn,
        "tailscale serve",
    );
    // Loopback behind a local HTTPS proxy (tailscale serve, Caddy) needs no [tls].
    deployment.config(
        "listen = \"127.0.0.1:8443\"\ntrusted_proxy = \"127.0.0.1\"\n",
        "",
    );
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Ok,
        "HTTPS ends at the proxy",
    );
    deployment.config(&with_token(MVP, true), "");
    expect(
        &deployment.doctor().await,
        Check::Tls,
        Status::Warn,
        "phones cannot pair",
    );
}

#[tokio::test]
async fn auth_mode_never_shows_the_token() {
    let deployment = Deployment::new().await;
    expect(
        &deployment.doctor().await,
        Check::Auth,
        Status::Ok,
        "passporter pair",
    );

    deployment.config(&with_token(MVP, true), "");
    expect(
        &deployment.doctor().await,
        Check::Auth,
        Status::Warn,
        "static api_token is accepted",
    );

    deployment.config(&with_token("listen = \"127.0.0.1:8443\"\n", false), "");
    expect(
        &deployment.doctor().await,
        Check::Auth,
        Status::Ok,
        "kept but refused",
    );

    // Pairing only, but on plain HTTP off loopback the PWA cannot pair.
    deployment.config(MVP, "");
    expect(
        &deployment.doctor().await,
        Check::Auth,
        Status::Warn,
        "cannot pair",
    );
}

#[tokio::test]
async fn a_static_token_behind_https_is_warned_about_or_refused() {
    let deployment = Deployment::new().await;
    deployment.config(
        &with_token(
            "listen = \"127.0.0.1:8443\"\ntrusted_proxy = \"127.0.0.1\"\n",
            true,
        ),
        "",
    );
    let report = deployment.doctor().await;
    match STATIC_TOKEN_BEHIND_HTTPS {
        StaticTokenBehindHttps::Warn => {
            expect(&report, Check::Auth, Status::Warn, "behind HTTPS");
        }
        StaticTokenBehindHttps::Refuse => {
            expect(&report, Check::Config, Status::Fail, "refused with [tls]");
        }
    }
    assert!(!report.to_string().contains(TOKEN), "{report}");
}

#[tokio::test]
async fn herdr_socket_reachability() {
    let deployment = Deployment::new().await;
    expect(
        &deployment.doctor().await,
        Check::Herdr,
        Status::Ok,
        "herdr fake, protocol 22",
    );

    let herdr = FakeHerdr::new();
    herdr.fail_next(Method::Ping, Fault::Reject(ErrorCode::ServerUnavailable));
    let report = deployment.doctor_with(herdr, &FakeUnit::running()).await;
    expect(&report, Check::Herdr, Status::Fail, "herdr_socket");

    // The real client on a socket nobody listens on.
    let report = doctor::run(
        &deployment.config_dir(),
        |socket: &Path| SocketClient::new(socket).with_request_timeout(TIMEOUT),
        &FakeUnit::running(),
        TIMEOUT,
    )
    .await;
    expect(&report, Check::Herdr, Status::Fail, "herdr.sock");
}

#[tokio::test]
async fn whisper_server_reachability_on_loopback() {
    let mut deployment = Deployment::new().await;
    expect(
        &deployment.doctor().await,
        Check::Whisper,
        Status::Ok,
        "listening on 127.0.0.1",
    );

    // The same port on another loopback address: while the stand-in holds
    // 127.0.0.1:<port>, nothing can bind the wildcard there, so this is
    // refused no matter what parallel tests do.
    let port = deployment.whisper.local_addr().expect("addr").port();
    deployment.whisper_addr = SocketAddr::from(([127, 0, 0, 2], port));
    deployment.config("listen = \"127.0.0.1:8443\"\n", "");
    expect(
        &deployment.doctor().await,
        Check::Whisper,
        Status::Fail,
        "build.sh",
    );
}

#[tokio::test]
async fn push_configuration() {
    let deployment = Deployment::new().await;
    expect(
        &deployment.doctor().await,
        Check::Push,
        Status::Ok,
        "vapid.pem loads",
    );

    let key = deployment.config_dir().join("vapid.pem");
    chmod(&key, 0o644);
    expect(
        &deployment.doctor().await,
        Check::Push,
        Status::Fail,
        "chmod 600",
    );

    fs::write(&key, "not a key").expect("write");
    chmod(&key, 0o600);
    expect(
        &deployment.doctor().await,
        Check::Push,
        Status::Fail,
        "move",
    );

    fs::remove_file(&key).expect("rm");
    expect(
        &deployment.doctor().await,
        Check::Push,
        Status::Fail,
        "push-keygen",
    );

    deployment.config("listen = \"127.0.0.1:8443\"\n", "");
    let report = deployment.doctor().await;
    expect(&report, Check::Push, Status::Off, "[push]");
    assert!(report.healthy(), "push off is not a failure:\n{report}");
}

#[tokio::test]
async fn systemd_unit_state() {
    let deployment = Deployment::new().await;
    let cases = [
        (FakeUnit::running(), Status::Ok, "enabled and active"),
        (
            FakeUnit::with(Enabled::NotFound, Active::Inactive),
            Status::Warn,
            "setup.sh",
        ),
        (
            FakeUnit::with(Enabled::Enabled, Active::Failed),
            Status::Fail,
            "journalctl",
        ),
        (
            FakeUnit::with(Enabled::Disabled, Active::Active),
            Status::Warn,
            "enable passporter-bridge",
        ),
        (
            FakeUnit::with(
                Enabled::Enabled,
                Active::Transitioning("activating".to_owned()),
            ),
            Status::Warn,
            "run doctor again",
        ),
        (FakeUnit::NoBus, Status::Warn, "Failed to connect to bus"),
    ];
    for (unit, status, needle) in cases {
        let report = deployment.doctor_with(FakeHerdr::new(), &unit).await;
        expect(&report, Check::Unit, status, needle);
    }
}

#[tokio::test]
async fn the_mvp_deployment_reports_its_compromises_without_the_token() {
    let deployment = Deployment::new().await;
    deployment.config(
        &with_token(MVP, true),
        "[push]\nsubject = \"mailto:me@example.com\"\n",
    );
    let report = deployment.doctor().await;
    assert!(report.healthy(), "{report}");
    expect(&report, Check::Listen, Status::Warn, "MVP mode");
    expect(&report, Check::Auth, Status::Warn, "MVP mode");
    expect(&report, Check::Tls, Status::Warn, "no [tls]");
    assert!(
        report.to_string().contains("0 failed, 3 warning(s)."),
        "{report}"
    );
}

/// Run from a herdr pane, doctor must still check the socket the unit uses
/// (herdr's default under HOME), not the pane's `HERDR_SOCKET_PATH`.
#[test]
fn the_herdr_check_ignores_this_shells_herdr_socket_path() {
    let root = tempfile::tempdir().expect("tmp");
    let config_dir = root.path().join("config");
    fs::create_dir(&config_dir).expect("mkdir");
    chmod(&config_dir, 0o700);
    let file = config_dir.join("config.toml");
    fs::write(&file, "listen = \"127.0.0.1:8443\"\n").expect("write");
    chmod(&file, 0o600);
    let pane_socket = root.path().join("pane.sock");
    let output = Command::new(env!("CARGO_BIN_EXE_passporter"))
        .arg("--config-dir")
        .arg(&config_dir)
        .arg("doctor")
        .env("HOME", root.path())
        .env("HERDR_SOCKET_PATH", &pane_socket)
        .env("XDG_RUNTIME_DIR", root.path())
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .env_remove("PASSPORTER_CONFIG_DIR")
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let herdr = stdout
        .lines()
        .find(|line| line.contains("FAIL  herdr"))
        .unwrap_or_else(|| panic!("{stdout}"));
    let default = root.path().join(".config/herdr/herdr.sock");
    assert!(herdr.contains(&default.display().to_string()), "{herdr}");
    assert!(herdr.contains("the unit does not see it"), "{herdr}");
}

/// The `passporter doctor` binary end to end: real systemctl and herdr
/// client, both pointed at nothing, so it fails, and says so on stdout.
#[tokio::test]
async fn the_cli_prints_the_report_and_fails_on_a_failed_check() {
    let deployment = Deployment::new().await;
    deployment.config(&with_token(MVP, true), "");
    let output = Command::new(env!("CARGO_BIN_EXE_passporter"))
        .arg("--config-dir")
        .arg(deployment.config_dir())
        .arg("doctor")
        .env("HOME", deployment.root.path())
        .env("XDG_RUNTIME_DIR", deployment.root.path())
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .env_remove("PASSPORTER_CONFIG_DIR")
        .env_remove("HERDR_SOCKET_PATH")
        .output()
        .expect("runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stdout}");
    assert!(stdout.contains("FAIL  herdr"), "{stdout}");
    assert!(stdout.contains("ok    config dir"), "{stdout}");
    assert!(stderr.contains("doctor check(s) failed"), "{stderr}");
    assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
}
