//! `deploy/bridge/setup.sh --config-only`: the config it writes for a new
//! install, under a temp `HOME`. Nothing is built, installed or started.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use bridge::config::Config;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/bridge/setup.sh")
}

fn setup(home: &Path, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(script())
        .arg("--config-only")
        .args(args)
        .env("HOME", home)
        .output()
        .expect("runs")
}

fn config_path(home: &Path) -> PathBuf {
    home.join(".config/passporter/config.toml")
}

/// Runs setup.sh in a fresh `HOME` and loads what it wrote.
fn written(args: &[&str]) -> (tempfile::TempDir, String, Config) {
    let home = tempfile::tempdir().expect("tmp");
    let output = setup(home.path(), args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = config_path(home.path());
    let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir = path.parent().expect("dir");
    let dir_mode = fs::metadata(dir).expect("meta").permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);
    let text = fs::read_to_string(&path).expect("read");
    let config = Config::load(dir).expect("the bridge starts with it");
    (home, text, config)
}

#[test]
fn a_new_install_is_pairing_only_on_loopback_behind_a_local_proxy() {
    let (_home, _, config) = written(&[]);
    assert!(config.listen.ip().is_loopback());
    assert_eq!(config.listen.port(), 8443);
    assert_eq!(config.trusted_proxy, Some("127.0.0.1".parse().expect("ip")));
    assert!(config.api_token.is_none(), "no static token");
    assert!(!config.insecure_http_over_wireguard);
    assert!(bridge::Settings::from_config(&config).is_ok());
}

#[test]
fn listen_and_trusted_proxy_are_flags() {
    let (_home, _, config) = written(&["--listen", "192.0.2.10:8443"]);
    assert_eq!(config.listen, "192.0.2.10:8443".parse().expect("addr"));
    assert_eq!(config.trusted_proxy, None, "no local proxy off loopback");

    let (_home, _, config) = written(&[
        "--listen",
        "192.0.2.10:9443",
        "--trusted-proxy",
        "192.0.2.46",
    ]);
    assert_eq!(config.listen.port(), 9443);
    assert_eq!(
        config.trusted_proxy,
        Some("192.0.2.46".parse().expect("ip"))
    );
}

#[test]
fn insecure_mvp_mode_is_an_explicit_flag() {
    let (_home, text, config) = written(&["--insecure-mvp", "192.0.2.10"]);
    assert!(text.contains("INSECURE MVP MODE"), "{text}");
    assert!(config.static_token().is_some());
    assert!(config.insecure_http_over_wireguard);
    assert_eq!(config.trusted_proxy, None);
    assert!(!config.static_token_behind_https());

    let home = tempfile::tempdir().expect("tmp");
    let refused = setup(
        home.path(),
        &[
            "--insecure-mvp",
            "192.0.2.10",
            "--trusted-proxy",
            "127.0.0.1",
        ],
    );
    assert!(!refused.status.success(), "MVP mode never with a proxy");
    assert!(!config_path(home.path()).exists());
}

#[test]
fn an_existing_config_is_never_touched() {
    let home = tempfile::tempdir().expect("tmp");
    let path = config_path(home.path());
    fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
    let mine = "listen = \"192.0.2.10:8443\"\n# my own settings\n";
    fs::write(&path, mine).expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");

    for args in [&[][..], &["--insecure-mvp", "192.0.2.10"][..]] {
        let output = setup(home.path(), args);
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Keeping the existing"));
        assert_eq!(fs::read_to_string(&path).expect("read"), mine);
    }
}
