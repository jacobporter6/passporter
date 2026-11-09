//! Contract tests: the client against replies recorded from a live herdr
//! 0.9.1 (`tests/fixtures/*.json`, scrubbed: paths, titles, labels and
//! session ids replaced, structure untouched).
//!
//! The fixtures are herdr's real shape, including what in-memory fakes
//! tend to leave out: agents with no `agent_session` (swarm agents run with
//! hooks off; some restarted sessions), no `title` but a
//! `terminal_title_stripped`, pane ids past `p9` (`w7:pC`), fields the
//! client does not model (`terminal_title`, `scroll`), and a quiet
//! workspace (`agent-swarm`). When herdr changes its replies, record
//! them again (see `fixtures/README.md`) and these tests say what broke.

use herdr::replay::{ReplaySocket, recorded_result};
use herdr::{AgentStatus, Herdr, Pane, SocketClient};
use serde_json::Value;

const AGENT_LIST: &str = include_str!("fixtures/agent-list.json");
const PANE_LIST: &str = include_str!("fixtures/pane-list.json");
const WORKSPACE_LIST: &str = include_str!("fixtures/workspace-list.json");

/// A replay socket answering `agent.list`, `agent.get` and
/// `workspace.list` from the fixtures, and refusing anything else as herdr
/// refuses an unknown method.
fn recorded_herdr(dir: &tempfile::TempDir) -> ReplaySocket {
    let agents = recorded_result(AGENT_LIST);
    let workspaces = recorded_result(WORKSPACE_LIST);
    ReplaySocket::serve(
        &dir.path().join("herdr.sock"),
        move |method, params| match method {
            "agent.list" => Ok(agents.clone()),
            "workspace.list" => Ok(workspaces.clone()),
            "agent.get" => agents["agents"]
                .as_array()
                .and_then(|all| all.iter().find(|a| a["pane_id"] == params["target"]))
                .map(|agent| serde_json::json!({"type": "agent_info", "agent": agent}))
                .ok_or_else(|| ("agent_not_found".to_owned(), "no such agent".to_owned())),
            other => Err((
                "invalid_request".to_owned(),
                format!("unknown method {other}"),
            )),
        },
    )
    .expect("binds the replay socket")
}

#[tokio::test]
async fn the_client_reads_a_recorded_agent_list() {
    let dir = tempfile::tempdir().expect("tmp");
    let herdr = recorded_herdr(&dir);
    let client = SocketClient::new(herdr.path());

    let agents = client.agents().await.expect("decodes herdr's agent list");
    assert_eq!(agents.len(), 7, "every recorded agent");
    assert!(agents.iter().all(|a| a.kind.as_deref() == Some("claude")));

    let without_session: Vec<&str> = agents
        .iter()
        .filter(|a| a.agent_session.is_none())
        .map(|a| a.pane_id.as_str())
        .collect();
    assert_eq!(
        without_session,
        ["w6:p4", "wD:p7"],
        "real herdr often reports no session: a swarm agent and a restarted one"
    );
    let swarm = agents
        .iter()
        .find(|a| a.pane_id.as_str() == "wD:p7")
        .expect("the swarm agent");
    assert_eq!(
        swarm.name.as_ref().map(herdr::AgentName::as_str),
        Some("impl-t41b")
    );
    assert_eq!(swarm.agent_status, AgentStatus::Working);
    assert!(swarm.interactive_ready);
    assert_eq!(
        swarm.terminal_title_stripped.as_deref(),
        Some("T41B: Load-robust integration tests")
    );
    // herdr 0.9.1 sends no `title`, only the terminal's.
    assert!(agents.iter().all(|a| a.title.is_none()));
    assert!(
        agents.iter().all(|a| a.terminal_title_stripped.is_some()),
        "every agent has a terminal title"
    );
    // Hex pane ids past p9.
    assert!(agents.iter().any(|a| a.pane_id.as_str() == "w7:pC"));
    assert!(agents.iter().all(|a| a.state_change_seq > 0));

    let one = client
        .agent(&herdr::Target::Pane(herdr::PaneId::new("w6:p4")))
        .await
        .expect("decodes herdr's agent info");
    assert_eq!(one.agent_session, None);
}

#[tokio::test]
async fn the_client_reads_recorded_workspaces_with_their_labels() {
    let dir = tempfile::tempdir().expect("tmp");
    let herdr = recorded_herdr(&dir);
    let workspaces = SocketClient::new(herdr.path())
        .workspaces()
        .await
        .expect("decodes herdr's workspace list");
    let labels: Vec<(&str, &str)> = workspaces
        .iter()
        .map(|w| (w.workspace_id.as_str(), w.label.as_str()))
        .collect();
    assert_eq!(
        labels,
        [
            ("w5", "garden"),
            ("w6", "greenhouse"),
            ("w7", "passporter"),
            ("wD", "agent-swarm"),
        ]
    );
}

#[tokio::test]
async fn an_unknown_method_comes_back_as_a_typed_server_error() {
    let dir = tempfile::tempdir().expect("tmp");
    let herdr = recorded_herdr(&dir);
    let err = SocketClient::new(herdr.path())
        .ping()
        .await
        .expect_err("the replay refuses ping");
    assert!(
        matches!(&err, herdr::Error::Server { code, .. } if code.as_str() == "invalid_request"),
        "{err}"
    );
    assert_eq!(herdr.methods(), ["ping"]);
}

/// `pane.list` is not part of the client yet, but [`Pane`] is: every
/// recorded pane, shell or agent, decodes.
#[test]
fn every_recorded_pane_decodes() {
    let result = recorded_result(PANE_LIST);
    assert_eq!(result["type"], "pane_list");
    let panes: Vec<Pane> =
        serde_json::from_value(result["panes"].clone()).expect("decodes herdr's panes");
    assert_eq!(panes.len(), 16);
    let shells = panes.iter().filter(|p| p.agent_kind.is_none()).count();
    assert_eq!(shells, 9, "plain shells report no agent");
    assert!(
        panes
            .iter()
            .filter(|p| p.agent_kind.is_none())
            .all(|p| p.agent_status == AgentStatus::Unknown)
    );
}

/// The fixtures stay scrubbed (see `fixtures/README.md`).
#[test]
fn the_fixtures_name_no_real_user_or_project() {
    for fixture in [AGENT_LIST, PANE_LIST, WORKSPACE_LIST] {
        let _: Value = serde_json::from_str(fixture).expect("JSON");
        assert_eq!(
            fixture.matches("/home/").count(),
            fixture.matches("/home/dev/").count(),
            "every home path is the scrubbed /home/dev"
        );
    }
}
