//! The `passporter-bridge` binary refuses unsafe configs before doing anything.
//! Every run points `--config-dir` and `HOME` at a temp dir, so the real
//! `~/.config/passporter` is never touched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn config_dir(contents: &str, dir_mode: u32, file_mode: u32) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tmp");
    let file = dir.path().join("config.toml");
    fs::write(&file, contents).expect("write");
    fs::set_permissions(&file, fs::Permissions::from_mode(file_mode)).expect("chmod file");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(dir_mode)).expect("chmod dir");
    dir
}

fn run_bridge(dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_passporter-bridge"))
        .arg("--config-dir")
        .arg(dir)
        .env("HOME", dir)
        .env_remove("PASSPORTER_CONFIG_DIR")
        .env_remove("HERDR_SOCKET_PATH")
        .env("PASSPORTER_LOG", "info")
        .output()
        .expect("runs")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn refuses_a_world_readable_config_file() {
    let dir = config_dir("listen = \"127.0.0.1:0\"", 0o700, 0o644);
    let output = run_bridge(dir.path());
    assert!(!output.status.success());
    let log = stderr(&output);
    assert!(log.contains("accessible by other users"), "{log}");
    assert!(log.contains("chmod 600"), "{log}");
}

#[test]
fn refuses_a_group_readable_config_dir() {
    let dir = config_dir("listen = \"127.0.0.1:0\"", 0o750, 0o600);
    let output = run_bridge(dir.path());
    assert!(!output.status.success());
    assert!(stderr(&output).contains("chmod 700"), "{}", stderr(&output));
}

#[test]
fn refuses_plain_http_off_loopback() {
    let dir = config_dir("listen = \"192.0.2.10:8443\"", 0o700, 0o600);
    let output = run_bridge(dir.path());
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("refusing plain HTTP"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn refuses_a_static_token_without_an_explicit_choice() {
    // What setup.sh wrote before `allow_static_token` existed.
    let token = "feedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedface";
    let config = format!(
        "listen = \"192.0.2.10:8443\"\ninsecure_http_over_wireguard = true\n\
         insecure_http_allow = [\"192.0.2.10\"]\napi_token = \"{token}\"\n"
    );
    let dir = config_dir(&config, 0o700, 0o600);
    let output = run_bridge(dir.path());
    assert!(!output.status.success());
    let log = stderr(&output);
    assert!(log.contains("`allow_static_token = true`"), "{log}");
    assert!(!log.contains(token), "never logs the token: {log}");
}

#[test]
fn rejects_unknown_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_passporter-bridge"))
        .arg("--frobnicate")
        .output()
        .expect("runs");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("usage"), "{}", stderr(&output));
}

fn keygen(dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_passporter-bridge"))
        .arg("push-keygen")
        .arg("--config-dir")
        .arg(dir)
        .env("HOME", dir)
        .env_remove("PASSPORTER_CONFIG_DIR")
        .output()
        .expect("runs")
}

fn run_cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_passporter"))
        .arg("--config-dir")
        .arg(dir)
        .args(args)
        .env("HOME", dir)
        .env_remove("PASSPORTER_CONFIG_DIR")
        .output()
        .expect("runs")
}

#[test]
fn push_keygen_writes_a_private_key_once() {
    let dir = config_dir("", 0o700, 0o600);
    let output = keygen(dir.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let public = String::from_utf8(output.stdout).expect("utf-8");
    // An uncompressed P-256 point, base64url without padding.
    assert_eq!(public.trim().len(), 87, "{public}");
    let key = dir.path().join("vapid.pem");
    let mode = fs::metadata(&key).expect("written").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let before = fs::read(&key).expect("read");

    let again = keygen(dir.path());
    assert!(!again.status.success());
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );
    assert_eq!(fs::read(&key).expect("read"), before, "never overwritten");
}

#[test]
fn refuses_push_without_a_vapid_key() {
    let dir = config_dir(
        "listen = \"127.0.0.1:0\"\n[push]\nsubject = \"mailto:me@example.com\"\n",
        0o700,
        0o600,
    );
    let output = run_bridge(dir.path());
    assert!(!output.status.success());
    assert!(stderr(&output).contains("vapid.pem"), "{}", stderr(&output));
}

#[test]
fn the_cli_says_when_the_bridge_is_not_running() {
    let dir = config_dir("", 0o700, 0o600);
    let state = dir.path().join("state");
    fs::write(
        dir.path().join("config.toml"),
        format!("state_dir = \"{}\"", state.display()),
    )
    .expect("write");
    let output = run_cli(dir.path(), &["devices", "list"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("is passporter-bridge running?"),
        "{}",
        stderr(&output)
    );
}

#[tokio::test]
async fn the_cli_pairs_lists_and_revokes_through_the_admin_socket() {
    use std::sync::Arc;

    use bridge::auth::admin::{self, ADMIN_SOCKET};
    use bridge::auth::{AuthSettings, DeviceAuth};

    let dir = config_dir("", 0o700, 0o600);
    let state = dir.path().join("state");
    fs::write(
        dir.path().join("config.toml"),
        format!("state_dir = \"{}\"", state.display()),
    )
    .expect("write");
    let auth = Arc::new(DeviceAuth::open(&state, AuthSettings::default(), None).expect("auth"));
    let (_trigger, shutdown) = bridge::shutdown::channel();
    let _admin = admin::spawn(auth, &state.join(ADMIN_SOCKET), shutdown).expect("admin");

    let cli = |args: &'static [&'static str]| {
        let dir = dir.path().to_path_buf();
        tokio::task::spawn_blocking(move || run_cli(&dir, args))
    };
    let paired = cli(&["pair", "--name", "Pixel"]).await.expect("joins");
    assert!(paired.status.success(), "{}", stderr(&paired));
    let shown = String::from_utf8_lossy(&paired.stdout);
    assert!(shown.contains("Pairing code: "), "{shown}");
    assert!(shown.contains("single use"), "{shown}");

    let listed = cli(&["devices", "list"]).await.expect("joins");
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(String::from_utf8_lossy(&listed.stdout).contains("No paired devices"));

    let revoked = cli(&["devices", "revoke", "dev-0000000000000001"])
        .await
        .expect("joins");
    assert!(!revoked.status.success());
    assert!(
        stderr(&revoked).contains("no paired device"),
        "{}",
        stderr(&revoked)
    );
}

#[test]
fn refuses_tls_with_an_open_key_or_a_plain_http_opt_in() {
    let dir = config_dir(
        "listen = \"127.0.0.1:0\"\n[tls]\ncert = \"leaf.pem\"\nkey = \"leaf.key\"\n",
        0o700,
        0o600,
    );
    let tls = crate::pki::Ca::new().leaf().install(dir.path());
    fs::set_permissions(&tls.key, fs::Permissions::from_mode(0o644)).expect("chmod key");
    let output = run_bridge(dir.path());
    assert!(!output.status.success());
    let log = stderr(&output);
    assert!(
        log.contains("leaf.key is accessible by other users"),
        "{log}"
    );
    assert!(log.contains("chmod 600"), "{log}");
    assert!(
        !log.contains("PRIVATE KEY"),
        "never logs key material: {log}"
    );

    fs::write(
        dir.path().join("config.toml"),
        "listen = \"192.0.2.10:8443\"\ninsecure_http_over_wireguard = true\n\
         insecure_http_allow = [\"192.0.2.10\"]\n[tls]\ncert = \"leaf.pem\"\nkey = \"leaf.key\"\n",
    )
    .expect("write");
    let output = run_bridge(dir.path());
    assert!(!output.status.success());
    let log = stderr(&output);
    assert!(log.contains("[tls] serves HTTPS only"), "{log}");
}

/// `systemctl --user reload` sends SIGHUP whatever the config: without
/// `[tls]` there is nothing to reload, and the bridge must keep running.
#[test]
fn sighup_without_tls_does_not_stop_the_bridge() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::time::Duration;

    let dir = config_dir("", 0o700, 0o600);
    fs::write(
        dir.path().join("config.toml"),
        format!(
            "listen = \"127.0.0.1:0\"\nstate_dir = \"{0}/state\"\nstatic_dir = \"{0}/web\"\n\
             herdr_socket = \"{0}/herdr.sock\"\n[tldr]\nenabled = false\n",
            dir.path().display()
        ),
    )
    .expect("write");
    let mut bridge = Command::new(env!("CARGO_BIN_EXE_passporter-bridge"))
        .arg("--config-dir")
        .arg(dir.path())
        .env("HOME", dir.path())
        .env_remove("PASSPORTER_CONFIG_DIR")
        .env_remove("HERDR_SOCKET_PATH")
        .env("PASSPORTER_LOG", "info")
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawns");
    let mut log = BufReader::new(bridge.stderr.take().expect("stderr"));
    let mut line = String::new();
    while !line.contains("passporter-bridge starting") {
        line.clear();
        assert!(log.read_line(&mut line).expect("log") > 0, "exited early");
    }
    let pid = bridge.id().to_string();
    let signal = |name: &str| {
        let status = Command::new("kill")
            .args([name, &pid])
            .status()
            .expect("kill runs");
        assert!(status.success());
    };
    signal("-HUP");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        bridge.try_wait().expect("wait").is_none(),
        "SIGHUP stopped the bridge"
    );
    signal("-TERM");
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut log, &mut rest).expect("log");
    assert!(bridge.wait().expect("wait").success(), "{rest}");
    assert!(rest.contains("nothing to reload"), "{rest}");
}
