//! Terminal mode against `FakeHerdr`: `GET /v1/agents/:id/commands`,
//! `POST /v1/agents/:id/commands/run`, and the terminals a run opens
//! (`GET /v1/terminals/:pane/screen`, `POST /v1/terminals/:pane/keys`).

use std::path::PathBuf;
use std::time::Duration;

use bridge::auth::RateLimit;
use herdr::{AgentStatus, Call, Herdr, PaneId, Target};
use protocol::{
    CommandCandidate, CommandIntent, CommandList, CommandRefusal, CommandResult, TerminalScreen,
};
use serde_json::{Value, json};

use crate::harness::OTHER_TOKEN;
use crate::tldr::{Fixture, append, assistant, user};

const KEY1: &str = "00000000-0000-4000-8000-000000000001";
const KEY2: &str = "00000000-0000-4000-8000-000000000002";
const KEY3: &str = "00000000-0000-4000-8000-000000000003";
const KEY4: &str = "00000000-0000-4000-8000-000000000004";

/// A `!` command for here, and a decoy for another terminal.
const MESSAGE: &str = "I can't open a browser. Run `! gcloud auth login` here, or \
                       `gcloud auth login --no-browser` in another terminal.";

struct Commands {
    f: Fixture,
    audit: PathBuf,
    _audit_dir: tempfile::TempDir,
}

async fn fixture(message: &str) -> Commands {
    fixture_with(message, |_| {}).await
}

/// An idle Claude agent whose latest message is `message`.
async fn fixture_with(message: &str, adjust: impl FnOnce(&mut bridge::Settings)) -> Commands {
    let audit_dir = tempfile::tempdir().expect("tmp");
    let audit = audit_dir.path().join("audit.jsonl");
    let path = audit.clone();
    let f = Fixture::start_with(None, &(user("help me log in") + &assistant(message)), |s| {
        s.actions.audit_log = path;
        adjust(s);
    })
    .await;
    f.bridge.fake().set_status(&f.pane, AgentStatus::Idle);
    Commands {
        f,
        audit,
        _audit_dir: audit_dir,
    }
}

impl Commands {
    async fn list(&self) -> CommandList {
        let response = self
            .f
            .bridge
            .get_authed(&format!("/v1/agents/{}/commands", self.f.id))
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        serde_json::from_str(&response.body).expect("list")
    }

    async fn run_raw(&self, body: &Value) -> (u16, CommandResult, Option<String>) {
        let path = format!("/v1/agents/{}/commands/run", self.f.id);
        let response = self.f.bridge.post_authed(&path, &body.to_string()).await;
        let result = serde_json::from_str(&response.body)
            .unwrap_or_else(|e| panic!("{e}: {}", response.body));
        let retry = response.header("retry-after").map(ToOwned::to_owned);
        (response.status, result, retry)
    }

    async fn run(&self, body: &Value) -> (u16, CommandResult) {
        let (status, result, _) = self.run_raw(body).await;
        (status, result)
    }

    /// What reached herdr: prompts, typed lines and keys.
    fn sent(&self) -> Vec<Call> {
        self.f
            .bridge
            .fake()
            .calls()
            .into_iter()
            .filter(|c| {
                matches!(
                    c,
                    Call::Prompt { .. } | Call::Run { .. } | Call::Keys { .. }
                )
            })
            .collect()
    }

    fn audit_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.audit)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("json line"))
            .collect()
    }
}

fn body(key: &str, candidate: &CommandCandidate, target: &str, origin: &str) -> Value {
    json!({
        "idempotencyKey": key,
        "origin": origin,
        "createdAt": 1_762_500_000_000_u64,
        "candidate": candidate.id,
        "hash": candidate.hash.as_str(),
        "target": target,
    })
}

#[tokio::test]
async fn every_candidate_of_the_latest_message_is_listed_decoys_included() {
    let c = fixture(MESSAGE).await;
    let list = c.list().await;
    let found: Vec<(&str, CommandIntent)> = list
        .candidates
        .iter()
        .map(|c| (c.text.as_str(), c.intent))
        .collect();
    assert_eq!(
        found,
        [
            ("gcloud auth login", CommandIntent::InSession),
            ("gcloud auth login --no-browser", CommandIntent::NewTerminal),
        ]
    );
    assert_eq!(list.omitted, 0);
    // Once the user has spoken again, the old message offers nothing.
    append(&c.f.transcript, &user("done"));
    assert_eq!(c.list().await.candidates, []);
    c.f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn here_types_the_bang_command_into_the_agent_audited_first_and_replays() {
    let c = fixture(MESSAGE).await;
    let list = c.list().await;
    let here = &list.candidates[0];
    let (status, result) = c.run(&body(KEY1, here, "here", "app")).await;
    assert_eq!(status, 200, "{result:?}");
    assert_eq!(result, CommandResult::Sent { replayed: false });
    assert_eq!(
        c.sent(),
        [Call::Prompt {
            pane: c.f.pane.clone(),
            text: "!gcloud auth login".into(),
        }]
    );
    // Audited before it ran, with the exact command; then the result.
    let lines = c.audit_lines();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["kind"], "command");
    assert_eq!(lines[0]["phase"], "execute");
    assert_eq!(lines[0]["command"], "gcloud auth login");
    assert_eq!(lines[0]["target"], "here");
    assert_eq!(lines[0]["agentId"], c.f.id.as_str());
    assert_eq!(lines[1]["phase"], "result");
    assert_eq!(lines[1]["outcome"], "sent");

    // A replay returns the stored result and types nothing more.
    let (status, again) = c.run(&body(KEY1, here, "here", "app")).await;
    assert_eq!(status, 200);
    assert_eq!(again, CommandResult::Sent { replayed: true });
    assert_eq!(c.sent().len(), 1);
    // The same key for another request is a conflict.
    let (status, conflict) = c.run(&body(KEY1, here, "new_terminal", "app")).await;
    assert_eq!(status, 422);
    assert!(matches!(
        conflict,
        CommandResult::Refused {
            reason: CommandRefusal::IdempotencyConflict,
            ..
        }
    ));
    c.f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_new_terminal_opens_in_the_agents_folder_and_types_the_command() {
    let c = fixture(MESSAGE).await;
    let list = c.list().await;
    let decoy = &list.candidates[1];
    let (status, result) = c.run(&body(KEY1, decoy, "new_terminal", "app")).await;
    assert_eq!(status, 200, "{result:?}");
    let CommandResult::Started {
        terminal,
        replayed: false,
    } = result
    else {
        panic!("started: {result:?}");
    };
    assert_eq!(terminal.label, "run: worker");
    let pane = PaneId::new(terminal.pane.as_str());
    assert_ne!(pane, c.f.pane);
    assert_eq!(
        c.sent(),
        [Call::Run {
            pane: pane.clone(),
            command: "gcloud auth login --no-browser".into(),
        }]
    );
    // A tab in the spawn workspace, its shell in the agent's folder.
    let fake = c.f.bridge.fake();
    let workspaces = fake.workspaces().await.expect("workspaces");
    assert!(
        workspaces.iter().any(|w| w.label == "passporter"),
        "{workspaces:?}"
    );
    let agent_cwd = fake
        .agent(&Target::from(c.f.pane.clone()))
        .await
        .expect("agent")
        .cwd;
    fake.detect_agent(&pane, "shell", AgentStatus::Idle);
    let terminal_cwd = fake
        .agent(&Target::from(pane.clone()))
        .await
        .expect("pane")
        .cwd;
    assert!(agent_cwd.is_some());
    assert_eq!(terminal_cwd, agent_cwd);
    let execute = &c.audit_lines()[0];
    assert_eq!(execute["command"], "gcloud auth login --no-browser");
    assert_eq!(execute["target"], "new_terminal");
    assert_eq!(c.audit_lines()[1]["pane"], pane.as_str());

    // The phone can watch it, and send it allowlisted keys.
    fake.set_screen(
        &pane,
        "$ gcloud auth login --no-browser\nGo to the link below",
    );
    let screen =
        c.f.bridge
            .get_authed(&format!("/v1/terminals/{pane}/screen"))
            .await;
    assert_eq!(screen.status, 200, "{}", screen.body);
    let screen: TerminalScreen = serde_json::from_str(&screen.body).expect("screen");
    assert!(screen.text.contains("Go to the link below"), "{screen:?}");
    let keys = json!({"idempotencyKey": KEY2, "origin": "app", "createdAt": 1, "keys": ["ctrl+c"]});
    let sent =
        c.f.bridge
            .post_authed(&format!("/v1/terminals/{pane}/keys"), &keys.to_string())
            .await;
    assert_eq!(sent.status, 200, "{}", sent.body);
    assert!(c.sent().contains(&Call::Keys {
        pane: pane.clone(),
        keys: vec!["ctrl+c".into()],
    }));
    let audit = c.audit_lines();
    assert!(
        audit
            .iter()
            .any(|l| l["kind"] == "terminal_keys" && l["phase"] == "execute"),
        "{audit:?}"
    );
    // Keys from the car: refused.
    let car = json!({"idempotencyKey": KEY3, "origin": "car", "createdAt": 1, "keys": ["enter"]});
    let refused =
        c.f.bridge
            .post_authed(&format!("/v1/terminals/{pane}/keys"), &car.to_string())
            .await;
    assert_eq!(refused.status, 403, "{}", refused.body);

    // Any other pane (the agent's own included) is not a terminal.
    let other =
        c.f.bridge
            .get_authed(&format!("/v1/terminals/{}/screen", c.f.pane))
            .await;
    assert_eq!(other.status, 404);
    let keys = json!({"idempotencyKey": KEY4, "origin": "app", "createdAt": 1, "keys": ["enter"]});
    let other =
        c.f.bridge
            .post_authed(
                &format!("/v1/terminals/{}/keys", c.f.pane),
                &keys.to_string(),
            )
            .await;
    assert_eq!(other.status, 404, "{}", other.body);
    c.f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_terminal_answers_only_the_device_that_opened_it_while_it_runs_its_terminal() {
    let c = fixture(MESSAGE).await;
    let list = c.list().await;
    let (status, result) = c
        .run(&body(KEY1, &list.candidates[1], "new_terminal", "app"))
        .await;
    assert_eq!(status, 200, "{result:?}");
    let CommandResult::Started { terminal, .. } = result else {
        panic!("started: {result:?}");
    };
    let pane = PaneId::new(terminal.pane.as_str());
    let screen = format!("/v1/terminals/{pane}/screen");
    let keys_path = format!("/v1/terminals/{pane}/keys");
    let keys = |key: &str| {
        json!({"idempotencyKey": key, "origin": "app", "createdAt": 1, "keys": ["enter"]})
            .to_string()
    };
    let json = [("content-type", "application/json")];
    let bridge = &c.f.bridge;

    // Another paired device knows nothing of it.
    let other = bridge.send_as(OTHER_TOKEN, "GET", &screen, &[], "").await;
    assert_eq!(other.status, 404, "{}", other.body);
    let other = bridge
        .send_as(OTHER_TOKEN, "POST", &keys_path, &json, &keys(KEY2))
        .await;
    assert_eq!(other.status, 404, "{}", other.body);
    assert_eq!(bridge.get_authed(&screen).await.status, 200);

    // herdr hands the pane's id to another terminal: it is no longer the
    // one the run opened, so it is neither read nor sent keys.
    bridge.fake().renew_terminal(&pane);
    assert_eq!(bridge.get_authed(&screen).await.status, 404);
    let reused = bridge.post_authed(&keys_path, &keys(KEY3)).await;
    assert_eq!(reused.status, 404, "{}", reused.body);
    assert!(
        !c.sent()
            .iter()
            .any(|call| matches!(call, Call::Keys { .. })),
        "{:?}",
        c.sent()
    );
    c.f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_stale_or_forged_candidate_never_runs() {
    let c = fixture(MESSAGE).await;
    let list = c.list().await;
    let (here, decoy) = (&list.candidates[0], &list.candidates[1]);

    // The id of one with the hash of the other.
    let forged = json!({
        "idempotencyKey": KEY1, "origin": "app", "createdAt": 1,
        "candidate": here.id, "hash": decoy.hash.as_str(), "target": "here",
    });
    let (status, result) = c.run(&forged).await;
    assert_eq!(status, 409, "{result:?}");
    assert!(matches!(
        result,
        CommandResult::Refused {
            reason: CommandRefusal::Stale,
            ..
        }
    ));
    // Free-form text has nowhere to go.
    let mut smuggled = body(KEY2, here, "here", "app");
    smuggled["text"] = json!("rm -rf ~");
    let (status, _) = c.run(&smuggled).await;
    assert_eq!(status, 400);

    // The message changes: the same id and hash are stale.
    append(
        &c.f.transcript,
        &assistant("Actually, run `! gcloud auth login --update-adc` instead."),
    );
    let (status, result) = c.run(&body(KEY3, here, "here", "app")).await;
    assert_eq!(status, 409, "{result:?}");
    assert!(matches!(
        result,
        CommandResult::Refused {
            reason: CommandRefusal::Stale,
            ..
        }
    ));
    assert_eq!(c.sent(), []);
    // Refusals before the audit write leave only a result line.
    assert!(
        c.audit_lines().iter().all(|l| l["phase"] == "result"),
        "{:?}",
        c.audit_lines()
    );
    c.f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_car_never_runs_commands() {
    let c = fixture(MESSAGE).await;
    let list = c.list().await;
    for (key, target) in [(KEY1, "here"), (KEY2, "new_terminal")] {
        let (status, result) = c.run(&body(key, &list.candidates[0], target, "car")).await;
        assert_eq!(status, 403, "{result:?}");
        assert!(matches!(
            result,
            CommandResult::Refused {
                reason: CommandRefusal::CarForbidden,
                ..
            }
        ));
    }
    assert_eq!(c.sent(), []);
    let audit = c.audit_lines();
    assert_eq!(audit.len(), 2);
    assert!(
        audit
            .iter()
            .all(|l| l["outcome"] == "refused: car_forbidden")
    );
    c.f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn here_needs_a_single_line_and_an_agent_at_its_prompt() {
    let message = "In this session run `! git status`. Then, in another terminal:\n\n\
                   ```bash\nnpm ci\nnpm run dev\n```";
    let c = fixture(message).await;
    let list = c.list().await;
    assert_eq!(list.candidates.len(), 2, "{list:?}");
    let block = &list.candidates[1];
    assert!(block.is_multiline());
    let (status, result) = c.run(&body(KEY1, block, "here", "app")).await;
    assert_eq!(status, 422, "{result:?}");
    assert!(matches!(
        result,
        CommandResult::Refused {
            reason: CommandRefusal::NotHere,
            ..
        }
    ));
    c.f.bridge
        .fake()
        .set_status(&c.f.pane, AgentStatus::Working);
    let (status, result) = c.run(&body(KEY2, &list.candidates[0], "here", "app")).await;
    assert_eq!(status, 409, "{result:?}");
    assert!(matches!(
        result,
        CommandResult::Refused {
            reason: CommandRefusal::AgentBusy,
            ..
        }
    ));
    // A multi-line block runs whole in a new terminal, working agent or not.
    let (status, result) = c.run(&body(KEY3, block, "new_terminal", "app")).await;
    assert_eq!(status, 200, "{result:?}");
    assert!(matches!(result, CommandResult::Started { .. }));
    let typed: Vec<String> = c
        .sent()
        .into_iter()
        .filter_map(|call| match call {
            Call::Run { command, .. } => Some(command),
            _ => None,
        })
        .collect();
    assert_eq!(typed, ["npm ci\nnpm run dev"]);
    c.f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn runs_are_rate_limited_per_device_but_replays_are_free() {
    let c = fixture_with(MESSAGE, |s| {
        s.commands.rate = RateLimit {
            requests: 2,
            window: Duration::from_secs(600),
        };
    })
    .await;
    let list = c.list().await;
    let here = &list.candidates[0];
    for key in [KEY1, KEY2] {
        let (status, result) = c.run(&body(key, here, "here", "app")).await;
        assert_eq!(status, 200, "{result:?}");
    }
    // A replay costs nothing.
    let (status, _) = c.run(&body(KEY1, here, "here", "app")).await;
    assert_eq!(status, 200);
    let (status, result, retry) = c.run_raw(&body(KEY3, here, "here", "app")).await;
    assert_eq!(status, 429, "{result:?}");
    assert!(retry.is_some());
    assert!(matches!(
        result,
        CommandResult::Refused {
            reason: CommandRefusal::RateLimited,
            ..
        }
    ));
    assert_eq!(c.sent().len(), 2);
    c.f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn nothing_runs_without_an_audit_entry() {
    let c = fixture(MESSAGE).await;
    let list = c.list().await;
    // A directory where the log should be: the write fails.
    std::fs::create_dir_all(&c.audit).expect("block the log");
    let (status, result) = c.run(&body(KEY1, &list.candidates[0], "here", "app")).await;
    assert_eq!(status, 503, "{result:?}");
    assert_eq!(c.sent(), []);
    c.f.bridge.stop().await.expect("clean stop");
}
