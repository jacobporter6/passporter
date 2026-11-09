//! The registry against herdr's recorded replies (see
//! `crates/herdr/tests/fixtures`), through the real socket client: what the
//! phone's agent list is built from when herdr reports agents the way it
//! really does, with and without sessions, titled only by their terminal,
//! some in the swarm's quiet workspace.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bridge::registry::{self, Hub, QuietDone, RegistrySettings};
use herdr::SocketClient;
use herdr::replay::{ReplaySocket, recorded_result};
use protocol::{Agent, AgentKind, AgentStatus};
use serde_json::json;

const AGENT_LIST: &str = include_str!("../../../herdr/tests/fixtures/agent-list.json");
const WORKSPACE_LIST: &str = include_str!("../../../herdr/tests/fixtures/workspace-list.json");

/// A herdr that answers from the recordings; the agents are `done`
/// instead when `all_done`, so every one has a finished turn to judge.
fn recorded_herdr(dir: &tempfile::TempDir, all_done: bool) -> ReplaySocket {
    let mut agents = recorded_result(AGENT_LIST);
    if all_done {
        for agent in agents["agents"].as_array_mut().expect("agents") {
            agent["agent_status"] = json!("done");
        }
    }
    let workspaces = recorded_result(WORKSPACE_LIST);
    ReplaySocket::serve(&dir.path().join("herdr.sock"), move |method, params| {
        let find = || {
            agents["agents"]
                .as_array()
                .and_then(|all| all.iter().find(|a| a["pane_id"] == params["target"]))
                .cloned()
                .ok_or_else(|| ("agent_not_found".to_owned(), "no such agent".to_owned()))
        };
        match method {
            "agent.list" => Ok(agents.clone()),
            "workspace.list" => Ok(workspaces.clone()),
            "agent.get" => find().map(|agent| json!({"type": "agent_info", "agent": agent})),
            "agent.read" => find().map(|agent| {
                json!({"type": "pane_read", "read": {
                    "pane_id": agent["pane_id"], "source": params["source"],
                    "text": "", "revision": agent["revision"], "truncated": false,
                }})
            }),
            "events.subscribe" => Ok(json!({"type": "subscription_started"})),
            other => Err((
                "invalid_request".to_owned(),
                format!("unknown method {other}"),
            )),
        }
    })
    .expect("binds the replay socket")
}

/// Runs the registry against `herdr` until the hub lists every recorded
/// agent with its workspace placement known, and returns the list.
async fn listed(herdr: &ReplaySocket) -> (Vec<Agent>, Arc<Hub>) {
    let hub = Arc::new(Hub::new(64));
    let (stop, shutdown) = bridge::shutdown::channel();
    let settings = RegistrySettings {
        quiet_done: QuietDone {
            workspaces: BTreeSet::from(["agent-swarm".to_owned()]),
            agent_prefixes: BTreeSet::from(["impl-".to_owned(), "rev-".to_owned()]),
        },
        ..RegistrySettings::default()
    };
    let task = registry::spawn(
        Arc::new(SocketClient::new(herdr.path())),
        Arc::clone(&hub),
        settings,
        shutdown,
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let agents = loop {
        let agents = hub.agents();
        let placed = agents.iter().all(|a| {
            hub.placement(a.id.pane())
                .is_some_and(|p| p.workspace.is_some())
        });
        if agents.len() == 7 && placed {
            break agents;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the registry listed {agents:?}; herdr was asked {:?}",
            herdr.methods()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    stop.trigger();
    let _ = task.await;
    (agents, hub)
}

#[tokio::test]
async fn the_registry_lists_every_recorded_agent() {
    let dir = tempfile::tempdir().expect("tmp");
    let herdr = recorded_herdr(&dir, false);
    let (agents, hub) = listed(&herdr).await;

    let by_pane = |pane: &str| {
        agents
            .iter()
            .find(|a| a.id.pane().as_str() == pane)
            .unwrap_or_else(|| panic!("{pane} listed: {agents:?}"))
    };
    assert!(agents.iter().all(|a| a.kind == AgentKind::Claude));

    // No session id: still listed, under its terminal title.
    let restarted = by_pane("w6:p4");
    assert_eq!(restarted.id.session(), None);
    assert_eq!(restarted.title, "Greenhouse controller brief");
    assert_eq!(restarted.status, AgentStatus::Working);
    // A named swarm agent: its name wins over the terminal title.
    let swarm = by_pane("wD:p7");
    assert_eq!(swarm.id.session(), None);
    assert_eq!(swarm.title, "impl-t41b");
    // With a session: carried in the id.
    assert!(by_pane("w5:p7").id.session().is_some());
    assert_eq!(by_pane("w5:p7").title, "Sprinkler valve wiring");
    assert_eq!(by_pane("w7:p1").title, "circus");

    let placement = hub.placement(swarm.id.pane()).expect("placed");
    assert_eq!(placement.workspace.as_deref(), Some("agent-swarm"));
    assert_eq!(placement.name.as_deref(), Some("impl-t41b"));
}

/// Finished turns of the swarm's agents are not news; everyone else's are,
/// including an unnamed agent and one with no session.
#[tokio::test]
async fn only_the_swarms_finished_turns_are_quiet() {
    let dir = tempfile::tempdir().expect("tmp");
    let herdr = recorded_herdr(&dir, true);
    let (agents, _) = listed(&herdr).await;
    let quiet: Vec<&str> = agents
        .iter()
        .filter(|a| a.quiet_done)
        .map(|a| a.id.pane().as_str())
        .collect();
    assert_eq!(quiet, ["wD:p7"], "{agents:?}");
    assert!(agents.iter().all(|a| a.unread), "done means unread");
}
