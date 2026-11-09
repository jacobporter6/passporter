//! Opt-in integration test against a real herdr, in a throwaway **named
//! session** (`passporter-it-<pid>`), never the user's live session.
//!
//! Run with: `PASSPORTER_HERDR_IT=1 cargo nextest run -p herdr --test live_session`

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use herdr::{Event, EventStream, Herdr, ReadSource, SocketClient, SplitDirection, Subscription};

/// A `herdr` command that cannot reach the live session: herdr injects
/// `HERDR_SOCKET_PATH` and friends into every pane, and those must never
/// override `--session` for this test.
fn herdr_cmd() -> Command {
    let mut cmd = Command::new("herdr");
    for var in [
        "HERDR_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_ENV",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

/// A headless herdr server for a named session, stopped and deleted on drop.
struct TestSession {
    name: String,
    server: Child,
    socket: PathBuf,
}

impl TestSession {
    fn start() -> Self {
        let name = format!("passporter-it-{}", std::process::id());
        let server = herdr_cmd()
            .args(["--session", &name, "server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn herdr server");
        let mut session = Self {
            name,
            server,
            socket: PathBuf::new(),
        };
        session.socket = session.wait_for_socket(Duration::from_secs(15));
        if let Some(live) = std::env::var_os("HERDR_SOCKET_PATH") {
            assert_ne!(
                session.socket,
                PathBuf::from(live),
                "refusing to test against the live session"
            );
        }
        session
    }

    fn wait_for_socket(&self, within: Duration) -> PathBuf {
        let deadline = Instant::now() + within;
        loop {
            let out = herdr_cmd()
                .args(["session", "list", "--json"])
                .output()
                .expect("herdr session list");
            let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
            let found = list["sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|s| s["name"] == self.name.as_str() && s["running"] == true);
            if let Some(socket) = found.and_then(|s| s["socket_path"].as_str()) {
                return PathBuf::from(socket);
            }
            assert!(
                Instant::now() < deadline,
                "test session {} never came up",
                self.name
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        // Only ever this test's own session, by its unique name.
        let _ = herdr_cmd().args(["session", "stop", &self.name]).output();
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = herdr_cmd().args(["session", "delete", &self.name]).output();
    }
}

fn enabled() -> bool {
    std::env::var_os("PASSPORTER_HERDR_IT").is_some_and(|v| v == "1")
}

#[tokio::test(flavor = "multi_thread")]
async fn drives_a_real_herdr_session() {
    if !enabled() {
        eprintln!("skipped: set PASSPORTER_HERDR_IT=1 to run against a named test session");
        return;
    }
    let session = TestSession::start();
    assert_ne!(session.name, "default");
    let herdr = SocketClient::new(&session.socket);

    let info = herdr.ping().await.expect("ping");
    assert!(info.protocol >= 22, "protocol {}", info.protocol);
    assert_eq!(
        herdr.agents().await.expect("agent list"),
        Vec::new(),
        "fresh session has no agents"
    );

    let scratch = std::env::temp_dir();
    let created = herdr
        .create_workspace("passporter-it", Some(&scratch))
        .await
        .expect("workspace");
    let root = created.root_pane.pane_id.clone();
    let split = herdr
        .split_pane(&root, SplitDirection::Right, Some(&scratch))
        .await
        .expect("split");

    // Run a command and see its output come back through pane.read.
    herdr
        .run_in_pane(&root, "echo passporter-it-$((40 + 2))")
        .await
        .expect("run");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = herdr
            .read_pane(&root, ReadSource::RecentUnwrapped, Some(50))
            .await
            .expect("read");
        if screen.text.contains("passporter-it-42") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "command output never appeared:\n{}",
            screen.text
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Events: closing the split pane is reported on the subscription.
    let mut events = herdr
        .subscribe(&[Subscription::PaneClosed])
        .await
        .expect("subscribe");
    herdr.close_pane(&split.pane_id).await.expect("close pane");
    let event = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("event within 5s")
        .expect("stream open")
        .expect("decodes");
    assert!(
        matches!(&event, Event::PaneClosed { pane_id, .. } if *pane_id == split.pane_id),
        "{event:?}"
    );

    // Typed errors from the real server.
    let err = herdr
        .read_pane(&split.pane_id, ReadSource::Visible, None)
        .await
        .expect_err("closed pane");
    assert!(err.code().is_some(), "server error expected, got {err}");

    herdr
        .close_workspace(&created.workspace.workspace_id)
        .await
        .expect("close workspace");
}
