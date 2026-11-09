//! `GET /v1/projects` and `POST /v1/agents`: new agents from the phone's
//! project allowlist, against `FakeHerdr`.

use std::path::PathBuf;
use std::time::Duration;

use bridge::auth::RateLimit;
use bridge::config::{SpawnConfig, SpawnProject};
use bridge::spawn::SpawnSettings;
use herdr::{AgentSession, AgentStatus, Call, Herdr, PaneId, Target};
use protocol::{ProjectName, SpawnKind, SpawnOptions, SpawnRefusal, SpawnResult};
use serde_json::{Value, json};

use crate::harness::{Bridge, CuttableHerdr, HttpResponse, PATIENCE};

const KEY1: &str = "00000000-0000-4000-8000-000000000001";
const KEY2: &str = "00000000-0000-4000-8000-000000000002";
const KEY3: &str = "00000000-0000-4000-8000-000000000003";

const NO_CALLS: [Call; 0] = [];
const NO_WORKSPACES: [herdr::Workspace; 0] = [];

struct Fixture {
    bridge: Bridge,
    audit: PathBuf,
    _tmp: tempfile::TempDir,
}

/// A bridge whose allowlist has `web` and `api` (real folders) and `gone`
/// (a missing one), and allows Claude only.
async fn fixture() -> Fixture {
    fixture_with(|_| {}, |_| {}).await
}

async fn fixture_with(
    prepare: impl FnOnce(&herdr::FakeHerdr),
    adjust: impl FnOnce(&mut SpawnSettings),
) -> Fixture {
    let tmp = tempfile::tempdir().expect("tmp");
    let web = tmp.path().join("repos/web");
    std::fs::create_dir_all(&web).expect("project dir");
    let api = tmp.path().join("repos/api");
    std::fs::create_dir_all(&api).expect("project dir");
    let audit = tmp.path().join("state/audit.jsonl");
    let config = SpawnConfig {
        workspace: "passporter".to_owned(),
        projects: vec![
            SpawnProject {
                name: ProjectName::new("web").expect("name"),
                path: web,
            },
            SpawnProject {
                name: ProjectName::new("gone").expect("name"),
                path: tmp.path().join("repos/gone"),
            },
            SpawnProject {
                name: ProjectName::new("api").expect("name"),
                path: api,
            },
        ],
        kinds: vec![SpawnKind::Claude],
    };
    let herdr = CuttableHerdr::new();
    prepare(&herdr.fake);
    let audit_path = audit.clone();
    let bridge = Bridge::start_with(herdr, move |settings| {
        settings.actions.audit_log = audit_path;
        settings.spawn.config = Some(config);
        adjust(&mut settings.spawn);
    })
    .await;
    Fixture {
        bridge,
        audit,
        _tmp: tmp,
    }
}

fn body(key: &str, project: &str, kind: &str, prompt: Option<&str>) -> String {
    let mut body = json!({
        "idempotencyKey": key,
        "project": project,
        "kind": kind,
        "createdAt": 1_762_500_000_000_u64,
    });
    if let Some(prompt) = prompt {
        body["prompt"] = json!(prompt);
    }
    body.to_string()
}

impl Fixture {
    async fn post_raw(&self, body: &str) -> HttpResponse {
        self.bridge.post_authed("/v1/agents", body).await
    }

    async fn post(&self, body: &str) -> (u16, SpawnResult) {
        let response = self.post_raw(body).await;
        let result = serde_json::from_str(&response.body)
            .unwrap_or_else(|e| panic!("{e}: {}", response.body));
        (response.status, result)
    }

    fn starts(&self) -> Vec<Call> {
        self.bridge
            .fake()
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Start { .. }))
            .collect()
    }

    fn prompts(&self) -> Vec<Call> {
        self.bridge
            .fake()
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Prompt { .. }))
            .collect()
    }

    /// The pane of the first agent started, once there is one.
    async fn started_pane(&self) -> PaneId {
        let poll = async {
            loop {
                if let Some(Call::Start { pane, .. }) = self.starts().into_iter().next() {
                    return pane;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(PATIENCE, poll)
            .await
            .expect("an agent starts")
    }

    fn audit_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.audit)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("json line"))
            .collect()
    }
}

fn refusal(result: &SpawnResult) -> Option<SpawnRefusal> {
    match result {
        SpawnResult::Refused { reason, .. } => Some(*reason),
        _ => None,
    }
}

#[tokio::test]
async fn projects_lists_names_and_kinds_but_never_paths() {
    let f = fixture().await;
    let response = f.bridge.get_authed("/v1/projects").await;
    assert_eq!(response.status, 200);
    assert!(!response.body.contains("repos"), "{}", response.body);
    let options: SpawnOptions = serde_json::from_str(&response.body).expect("options");
    let names: Vec<&str> = options.projects.iter().map(ProjectName::as_str).collect();
    assert_eq!(names, ["web", "gone", "api"]);
    assert_eq!(options.kinds, [SpawnKind::Claude]);
    let anonymous = f.bridge.http("GET", "/v1/projects", &[]).await;
    assert_eq!(anonymous.status, 401);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn spawning_is_off_without_a_spawn_table() {
    let bridge = Bridge::start(CuttableHerdr::new()).await;
    let options: SpawnOptions =
        serde_json::from_str(&bridge.get_authed("/v1/projects").await.body).expect("options");
    assert_eq!(options, SpawnOptions::default());
    let response = bridge
        .post_authed("/v1/agents", &body(KEY1, "web", "claude", None))
        .await;
    assert_eq!(response.status, 404, "{}", response.body);
    assert!(response.body.contains("disabled"), "{}", response.body);
    assert_eq!(bridge.fake().calls(), NO_CALLS);
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn spawns_in_an_allowlisted_project_with_fixed_arguments() {
    // A fresh tab's shell takes a few tries to reach its prompt.
    let f = fixture_with(|fake| fake.set_shell_startup(3), |_| {}).await;
    let (status, result) = f.post(&body(KEY1, "web", "claude", None)).await;
    assert_eq!(status, 200, "{result:?}");
    let SpawnResult::Spawned {
        agent_id,
        replayed: false,
        prompt_error: None,
    } = result
    else {
        panic!("not spawned: {result:?}");
    };
    let starts = f.starts();
    let [
        Call::Start {
            pane,
            name,
            kind,
            args,
        },
    ] = starts.as_slice()
    else {
        panic!("one start: {starts:?}");
    };
    assert_eq!(name.as_str(), "pp-00000000040008000000000000001");
    assert_eq!(kind, "claude");
    assert!(args.is_empty(), "no native arguments, ever: {args:?}");
    assert_eq!(agent_id.pane().as_str(), pane.as_str());

    // In the spawn workspace, in the project folder.
    let fake = f.bridge.fake();
    let workspaces = fake.workspaces().await.expect("workspaces");
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].label, "passporter");
    let agent = fake.agent(&Target::from(pane)).await.expect("agent");
    assert!(
        agent
            .cwd
            .as_ref()
            .is_some_and(|cwd| cwd.ends_with("repos/web")),
        "{:?}",
        agent.cwd
    );
    // The phone can open it at once: the list already has it.
    let listed = f.bridge.agents().await;
    assert!(listed.iter().any(|a| a.id == agent_id), "{listed:?}");

    // The next one gets a tab of its own in the same workspace.
    let (_, second) = f.post(&body(KEY2, "web", "claude", None)).await;
    assert!(matches!(second, SpawnResult::Spawned { .. }), "{second:?}");
    let workspaces = fake.workspaces().await.expect("workspaces");
    assert_eq!(workspaces.len(), 1, "one spawn workspace");
    assert_eq!(workspaces[0].tab_count, 2);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn refuses_unknown_projects_kinds_and_client_flags() {
    let f = fixture().await;
    let (status, result) = f.post(&body(KEY1, "nope", "claude", None)).await;
    assert_eq!(
        (status, refusal(&result)),
        (422, Some(SpawnRefusal::UnknownProject))
    );
    // Codex is a protocol kind this bridge does not allow.
    let (status, result) = f.post(&body(KEY2, "web", "codex", None)).await;
    assert_eq!(
        (status, refusal(&result)),
        (422, Some(SpawnRefusal::UnknownKind))
    );
    // Not a protocol kind at all, a path for a project, or flags.
    for bad in [
        body(KEY3, "web", "pi", None),
        body(KEY3, "/etc", "claude", None),
        json!({
            "idempotencyKey": KEY3, "project": "web", "kind": "claude", "createdAt": 0,
            "args": ["--dangerously-skip-permissions"],
        })
        .to_string(),
        json!({
            "idempotencyKey": KEY3, "project": "web", "kind": "claude", "createdAt": 0,
            "path": "/home",
        })
        .to_string(),
    ] {
        let (status, result) = f.post(&bad).await;
        assert_eq!(
            (status, refusal(&result)),
            (400, Some(SpawnRefusal::InvalidRequest)),
            "{bad}"
        );
    }
    // A prompt that would run a command needs step-up first.
    let (status, result) = f
        .post(&body(KEY3, "web", "claude", Some("!rm -rf .")))
        .await;
    assert_eq!(
        (status, refusal(&result)),
        (403, Some(SpawnRefusal::StepUpRequired))
    );
    assert!(f.bridge.fake().calls().is_empty(), "nothing started");
    assert!(
        f.bridge.fake().workspaces().await.expect("list").is_empty(),
        "no tab opened"
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_missing_project_folder_starts_nothing() {
    let f = fixture().await;
    let (status, result) = f.post(&body(KEY1, "gone", "claude", None)).await;
    assert_eq!(status, 200);
    assert!(
        matches!(&result, SpawnResult::Failed { message } if message.contains("missing")),
        "{result:?}"
    );
    assert_eq!(f.bridge.fake().calls(), NO_CALLS);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn an_idempotent_retry_replays_and_never_starts_twice() {
    let f = fixture().await;
    let request = body(KEY1, "web", "claude", Some("hello"));
    let (_, first) = f.post(&request).await;
    let SpawnResult::Spawned { agent_id, .. } = &first else {
        panic!("not spawned: {first:?}");
    };
    let (status, again) = f.post(&request).await;
    assert_eq!(status, 200);
    assert_eq!(
        again,
        SpawnResult::Spawned {
            agent_id: agent_id.clone(),
            replayed: true,
            prompt_error: None,
        }
    );
    assert_eq!(f.starts().len(), 1);
    assert_eq!(f.prompts().len(), 1, "the prompt is not sent twice");

    // Same key, different request.
    let (status, conflict) = f.post(&body(KEY1, "web", "claude", Some("bye"))).await;
    assert_eq!(
        (status, refusal(&conflict)),
        (422, Some(SpawnRefusal::IdempotencyConflict))
    );
    assert_eq!(f.starts().len(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_retry_after_the_ledger_forgot_finds_the_agent_by_name() {
    // A remembered-results capacity of one: the second spawn evicts the
    // first, as a bridge restart would.
    let f = fixture_with(|_| {}, |spawn| spawn.idempotency_capacity = 1).await;
    let request = body(KEY1, "web", "claude", Some("hello"));
    let (_, first) = f.post(&request).await;
    let (_, _) = f.post(&body(KEY2, "web", "claude", None)).await;
    let (status, again) = f.post(&request).await;
    assert_eq!(status, 200);
    match (&first, &again) {
        (
            SpawnResult::Spawned { agent_id: a, .. },
            SpawnResult::Spawned {
                agent_id: b,
                replayed: true,
                prompt_error: Some(why),
            },
        ) => {
            assert_eq!(a, b);
            assert!(
                why.contains("unknown"),
                "never claims the prompt went: {why}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(f.starts().len(), 2, "no third agent");
    assert_eq!(f.prompts().len(), 1, "the prompt is not sent again");
    let workspaces = f.bridge.fake().workspaces().await.expect("list");
    assert_eq!(workspaces[0].tab_count, 2, "the spare tab was closed");

    // The same key for another project is a conflict, not that agent.
    let (status, conflict) = f.post(&body(KEY1, "api", "claude", None)).await;
    assert_eq!(
        (status, refusal(&conflict)),
        (422, Some(SpawnRefusal::IdempotencyConflict)),
        "{conflict:?}"
    );
    assert_eq!(f.starts().len(), 2);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_slow_spawn_answers_still_starting_and_the_same_key_gets_the_result() {
    let f = fixture_with(
        |fake| fake.set_start_status(AgentStatus::Working),
        |spawn| {
            spawn.answer_within = Duration::from_millis(200);
            spawn.ready_wait = PATIENCE;
        },
    )
    .await;
    let request = body(KEY1, "web", "claude", Some("go"));
    let response = f.post_raw(&request).await;
    assert_eq!(response.status, 429, "{}", response.body);
    assert!(response.header("retry-after").is_some());
    let still: SpawnResult = serde_json::from_str(&response.body).expect("result");
    assert_eq!(
        refusal(&still),
        Some(SpawnRefusal::RateLimited),
        "transient"
    );
    // Asking again while it runs: still starting, nothing new started.
    assert_eq!(f.post_raw(&request).await.status, 429);
    let pane = f.started_pane().await;
    f.bridge.fake().set_status(&pane, AgentStatus::Idle);
    let poll = async {
        loop {
            let (status, result) = f.post(&request).await;
            if status == 200 {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let result = tokio::time::timeout(PATIENCE, poll).await.expect("settles");
    assert!(
        matches!(
            result,
            SpawnResult::Spawned {
                replayed: true,
                prompt_error: None,
                ..
            }
        ),
        "{result:?}"
    );
    assert_eq!(f.starts().len(), 1, "one agent");
    assert_eq!(f.prompts().len(), 1, "one prompt");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn refused_requests_never_spend_the_rate_budget() {
    let f = fixture_with(|_| {}, |spawn| spawn.rate = RateLimit::per_minute(1)).await;
    let _ = f.post(&body(KEY1, "nope", "claude", None)).await;
    let _ = f.post(&body(KEY2, "web", "claude", Some("/clear"))).await;
    let _ = f.post(&body(KEY3, "gone", "claude", None)).await;
    let (status, result) = f
        .post(&body(
            "00000000-0000-4000-8000-000000000004",
            "web",
            "claude",
            None,
        ))
        .await;
    assert_eq!(status, 200, "{result:?}");
    assert!(matches!(result, SpawnResult::Spawned { .. }), "{result:?}");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn spawns_are_rate_limited_per_device() {
    let f = fixture_with(|_| {}, |spawn| spawn.rate = RateLimit::per_minute(2)).await;
    for key in [KEY1, KEY2] {
        let (status, _) = f.post(&body(key, "web", "claude", None)).await;
        assert_eq!(status, 200);
    }
    let response = f.post_raw(&body(KEY3, "web", "claude", None)).await;
    assert_eq!(response.status, 429, "{}", response.body);
    assert!(response.header("retry-after").is_some());
    assert!(response.body.contains("rate_limited"));
    assert_eq!(f.starts().len(), 2);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn every_spawn_is_audited_before_it_executes() {
    let f = fixture().await;
    let (_, result) = f
        .post(&body(KEY1, "web", "claude", Some("fix the flaky test")))
        .await;
    let SpawnResult::Spawned { agent_id, .. } = result else {
        panic!("not spawned: {result:?}");
    };
    let _ = f.post(&body(KEY2, "nope", "claude", None)).await;
    let lines = f.audit_lines();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0]["phase"], "execute");
    assert_eq!(lines[0]["kind"], "spawn");
    assert_eq!(lines[0]["device"], "test-device");
    assert_eq!(lines[0]["project"], "web");
    assert_eq!(lines[0]["agentKind"], "claude");
    assert_eq!(lines[0]["prompt"], "fix the flaky test");
    assert_eq!(lines[1]["phase"], "result");
    assert_eq!(lines[1]["outcome"], "spawned");
    assert_eq!(lines[1]["agentId"], agent_id.to_string());
    // A refusal executes nothing, so it has only its result.
    assert_eq!(lines[2]["phase"], "result");
    assert_eq!(lines[2]["outcome"], "refused: unknown_project");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn nothing_starts_when_the_audit_log_is_unwritable() {
    let f = fixture().await;
    // A directory where the log should be: every append fails.
    std::fs::create_dir_all(&f.audit).expect("block the log");
    let request = body(KEY1, "web", "claude", None);
    let (status, result) = f.post(&request).await;
    assert_eq!(status, 503, "{result:?}");
    assert_eq!(f.bridge.fake().calls(), NO_CALLS);
    assert_eq!(
        f.bridge.fake().workspaces().await.expect("list"),
        NO_WORKSPACES
    );
    // Undecided: the same key may be retried once the log is back.
    std::fs::remove_dir(&f.audit).expect("unblock");
    let (status, result) = f.post(&request).await;
    assert_eq!(status, 200, "{result:?}");
    assert_eq!(f.starts().len(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_first_prompt_is_sent_only_once_the_agent_is_ready() {
    // Claude boots for a while after herdr starts it.
    let f = fixture_with(|fake| fake.set_start_status(AgentStatus::Working), |_| {}).await;
    let request = body(KEY1, "web", "claude", Some("fix the flaky test"));
    let session = AgentSession {
        source: "herdr:claude".into(),
        agent: "claude".into(),
        kind: "id".into(),
        value: "0e2e0000-0000-4000-8000-0000000000aa".into(),
    };
    let boot = async {
        let pane = f.started_pane().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(f.prompts().is_empty(), "no prompt while it boots");
        f.bridge.fake().set_session(&pane, session.clone());
        f.bridge.fake().set_status(&pane, AgentStatus::Idle);
        pane
    };
    let ((status, result), pane) = tokio::join!(f.post(&request), boot);
    assert_eq!(status, 200, "{result:?}");
    assert_eq!(
        f.prompts(),
        [Call::Prompt {
            pane: pane.clone(),
            text: "fix the flaky test".into()
        }]
    );
    let SpawnResult::Spawned {
        agent_id,
        prompt_error: None,
        ..
    } = result
    else {
        panic!("{result:?}");
    };
    assert_eq!(
        agent_id.session().map(protocol::SessionId::as_str),
        Some(session.value.as_str()),
        "the id carries the session once Claude has one"
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_first_prompt_waits_out_a_dialog_instead_of_answering_it() {
    let f = fixture_with(|fake| fake.set_start_status(AgentStatus::Blocked), |_| {}).await;
    let (status, result) = f.post(&body(KEY1, "web", "claude", Some("go"))).await;
    assert_eq!(status, 200);
    assert!(
        matches!(&result, SpawnResult::Spawned { prompt_error: Some(why), .. } if why.contains("asked")),
        "{result:?}"
    );
    assert!(f.prompts().is_empty(), "never typed into a dialog");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_first_prompt_gives_up_when_the_agent_never_settles() {
    let f = fixture_with(
        |fake| fake.set_start_status(AgentStatus::Working),
        |spawn| spawn.ready_wait = Duration::from_millis(100),
    )
    .await;
    let (status, result) = f.post(&body(KEY1, "web", "claude", Some("go"))).await;
    assert_eq!(status, 200);
    assert!(
        matches!(&result, SpawnResult::Spawned { prompt_error: Some(why), .. } if why.contains("not ready")),
        "{result:?}"
    );
    assert_eq!(f.prompts(), NO_CALLS);
    f.bridge.stop().await.expect("clean stop");
}
