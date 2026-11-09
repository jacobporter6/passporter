//! `GET /v1/agents` against herdr state.

use std::fs;
use std::path::Path;

use herdr::{AgentName, AgentSession, AgentStatus, Herdr, PaneId, StartAgent};
use protocol::{AgentKind, Lenient, Seq};

use crate::harness::{Bridge, CuttableHerdr};

const SESSION: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";

/// Starts a named agent in a new workspace rooted at `cwd`.
pub(crate) async fn start_agent(
    herdr: &CuttableHerdr,
    name: &str,
    kind: &str,
    cwd: &Path,
) -> PaneId {
    let created = herdr
        .fake
        .create_workspace(name, Some(cwd))
        .await
        .expect("workspace");
    let pane = created.root_pane.pane_id;
    herdr
        .fake
        .start_agent(&StartAgent {
            name: AgentName::new(name).expect("name"),
            kind: kind.to_owned(),
            pane: pane.clone(),
            args: Vec::new(),
            timeout: None,
        })
        .await
        .expect("start");
    pane
}

#[tokio::test]
async fn lists_agents_with_repo_worktree_and_session() {
    let repos = tempfile::tempdir().expect("tmp");
    let main = repos.path().join("gadget");
    fs::create_dir_all(main.join(".git/worktrees/login")).expect("admin dir");
    let worktree = repos.path().join("login");
    fs::create_dir_all(&worktree).expect("worktree");
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}", main.join(".git/worktrees/login").display()),
    )
    .expect("gitfile");

    let herdr = CuttableHerdr::new();
    let lead = start_agent(&herdr, "login-lead", "claude", &worktree).await;
    herdr.fake.set_session(
        &lead,
        AgentSession {
            source: "herdr:claude".to_owned(),
            agent: "claude".to_owned(),
            kind: "id".to_owned(),
            value: SESSION.to_owned(),
        },
    );
    herdr.fake.set_status(&lead, AgentStatus::Blocked);
    let docs = start_agent(&herdr, "docs", "codex", &main).await;
    herdr.fake.set_status(&docs, AgentStatus::Done);

    let bridge = Bridge::start(herdr).await;
    let agents = bridge.wait_for_agents("two agents", |a| a.len() == 2).await;

    let lead_agent = agents
        .iter()
        .find(|a| a.id.pane().as_str() == lead.as_str())
        .expect("lead listed");
    assert_eq!(lead_agent.id.to_string(), format!("{lead}~{SESSION}"));
    assert_eq!(lead_agent.title, "login-lead");
    assert_eq!(lead_agent.repo, "gadget");
    assert_eq!(lead_agent.worktree.as_deref(), Some("login"));
    assert_eq!(lead_agent.status, protocol::AgentStatus::Blocked);
    assert_eq!(lead_agent.seq, Seq(2));
    assert_eq!(lead_agent.kind, AgentKind::Claude);
    assert!(!lead_agent.unread);
    assert!(lead_agent.status_since.unix_millis() > 0);

    let docs_agent = agents
        .iter()
        .find(|a| a.id.pane().as_str() == docs.as_str())
        .expect("docs listed");
    assert_eq!(docs_agent.id.to_string(), docs.as_str(), "no session yet");
    assert_eq!(docs_agent.repo, "gadget");
    assert_eq!(docs_agent.worktree, None);
    assert_eq!(docs_agent.kind, AgentKind::Codex);
    assert!(docs_agent.unread, "done means not yet seen");

    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_list_is_a_bare_array_clients_can_parse_leniently() {
    let herdr = CuttableHerdr::new();
    herdr.fake.add_agent("solo", "pi", AgentStatus::Idle);
    let bridge = Bridge::start(herdr).await;
    bridge.wait_for_agents("one agent", |a| a.len() == 1).await;

    let response = bridge.get_authed("/v1/agents").await;
    assert_eq!(response.header("content-type"), Some("application/json"));
    let lenient: Vec<Lenient<protocol::Agent>> =
        serde_json::from_str(&response.body).expect("array");
    assert_eq!(lenient.len(), 1);
    assert!(lenient.into_iter().all(|a| a.into_option().is_some()));
    let raw: serde_json::Value = serde_json::from_str(&response.body).expect("json");
    assert_eq!(raw[0]["kind"], "pi");
    assert_eq!(raw[0]["title"], "solo");
    assert!(raw[0]["statusSince"].is_u64(), "camelCase wire names");

    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn an_empty_herdr_lists_nothing() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    assert_eq!(bridge.agents().await, Vec::new());
    bridge.stop().await.expect("clean stop");
}
