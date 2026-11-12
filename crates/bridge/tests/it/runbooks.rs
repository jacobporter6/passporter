//! `GET /v1/runbooks` and `POST /v1/runbooks/:name/run`: preconfigured
//! operations from the phone, against `FakeHerdr`. Where a run must really
//! happen, the test plays the tab's shell: it runs the line the bridge
//! typed with `sh -c`, which starts the real `passporter-bridge` runner.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bridge::auth::RateLimit;
use bridge::config::RunbookConfig;
use bridge::runbooks::RunbookSettings;
use herdr::{Call, PaneId};
use protocol::{RunState, RunbookList, RunbookName, RunbookRefusal, RunbookResult, RunbookRun};
use serde_json::{Value, json};

use crate::harness::{Bridge, CuttableHerdr, PATIENCE};

const KEY1: &str = "00000000-0000-4000-8000-000000000001";
const KEY2: &str = "00000000-0000-4000-8000-000000000002";
const KEY3: &str = "00000000-0000-4000-8000-000000000003";

const NO_CALLS: [Call; 0] = [];

struct Fixture {
    bridge: Bridge,
    audit: PathBuf,
    repo: PathBuf,
    _tmp: tempfile::TempDir,
}

fn runbook(name: &str, cwd: &Path, argv: &[&str]) -> RunbookConfig {
    RunbookConfig {
        name: RunbookName::new(name).expect("name"),
        aliases: vec![format!("please {}", name.to_lowercase())],
        cwd: cwd.to_path_buf(),
        argv: argv.iter().map(|a| (*a).to_owned()).collect(),
        confirm: true,
        timeout: Duration::from_secs(30),
    }
}

/// The real binary: its `runbook` mode is the runner.
const RUNNER: &str = env!("CARGO_BIN_EXE_passporter-bridge");

/// A bridge with harmless runbooks: `Hello` prints a line, `Fail` exits
/// 3, `Count` appends a line to `<repo>/count`, `Slow` sleeps 30 s.
async fn fixture() -> Fixture {
    fixture_with(|_| {}).await
}

async fn fixture_with(adjust: impl FnOnce(&mut RunbookSettings)) -> Fixture {
    fixture_in(tempfile::tempdir().expect("tmp"), adjust).await
}

/// A fixture keeping its state in `tmp`: a second one in the same place
/// is the bridge after a restart.
async fn fixture_in(tmp: tempfile::TempDir, adjust: impl FnOnce(&mut RunbookSettings)) -> Fixture {
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    let audit = tmp.path().join("state/audit.jsonl");
    let runs = tmp.path().join("state/runbooks");
    let runbooks = vec![
        runbook("Hello", &repo, &["printf", "hello from a runbook\\n"]),
        runbook("Fail", &repo, &["sh", "-c", "exit 3"]),
        runbook("Count", &repo, &["sh", "-c", "echo ran >> count"]),
        runbook("Slow", &repo, &["sleep", "30"]),
    ];
    let audit_path = audit.clone();
    let bridge = Bridge::start_with(CuttableHerdr::new(), move |settings| {
        settings.actions.audit_log = audit_path;
        settings.runbooks.runbooks = runbooks;
        settings.runbooks.runs_dir = runs;
        settings.runbooks.runner = PathBuf::from(RUNNER);
        adjust(&mut settings.runbooks);
    })
    .await;
    Fixture {
        bridge,
        audit,
        repo,
        _tmp: tmp,
    }
}

fn body(key: &str, origin: &str) -> String {
    json!({"idempotencyKey": key, "origin": origin, "createdAt": 1_762_500_000_000_u64}).to_string()
}

impl Fixture {
    async fn run(&self, name: &str, body: &str) -> (u16, RunbookResult) {
        let path = format!("/v1/runbooks/{name}/run");
        let response = self.bridge.post_authed(&path, body).await;
        let result = serde_json::from_str(&response.body)
            .unwrap_or_else(|e| panic!("{e}: {}", response.body));
        (response.status, result)
    }

    async fn list(&self) -> RunbookList {
        let response = self.bridge.get_authed("/v1/runbooks").await;
        assert_eq!(response.status, 200, "{}", response.body);
        serde_json::from_str(&response.body).expect("list")
    }

    /// Every line typed into a pane, with the pane.
    fn typed(&self) -> Vec<(PaneId, String)> {
        self.bridge
            .fake()
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Run { pane, command } => Some((pane, command)),
                _ => None,
            })
            .collect()
    }

    /// The typed lines once there are at least `n`.
    async fn typed_at_least(&self, n: usize) -> Vec<(PaneId, String)> {
        let poll = async {
            loop {
                let typed = self.typed();
                if typed.len() >= n {
                    return typed;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(PATIENCE, poll)
            .await
            .expect("the launch line is typed")
    }

    /// Plays the tab's shell for the `n`th typed line: runs it with
    /// `sh -c` in the repo, to its end. Returns the pane.
    async fn shell_runs(&self, n: usize) -> PaneId {
        let (pane, line) = self.typed_at_least(n + 1).await.swap_remove(n);
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&line)
            .current_dir(&self.repo)
            .output()
            .await
            .expect("sh");
        assert!(
            !out.stdout.is_empty() || !out.stderr.is_empty(),
            "the runner said something"
        );
        pane
    }

    /// Starts the runner for the `n`th typed line without waiting for it.
    async fn shell_starts(&self, n: usize) -> tokio::process::Child {
        let (_, line) = self.typed_at_least(n + 1).await.swap_remove(n);
        let spec = line.trim().rsplit(' ').next().expect("spec").to_owned();
        tokio::process::Command::new(RUNNER)
            .arg("runbook")
            .arg(spec)
            .current_dir(&self.repo)
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("runner")
    }

    /// How many times `Count` really ran.
    fn counted(&self) -> usize {
        std::fs::read_to_string(self.repo.join("count")).map_or(0, |text| text.lines().count())
    }

    /// The status file of the run for `key`, once its runner reports `pid`.
    async fn running_pid(&self, key: &str) -> u32 {
        let path = self
            .audit
            .with_file_name(format!("runbooks/{key}.status.json"));
        let poll = async {
            loop {
                if let Ok(text) = std::fs::read(&path)
                    && let Ok(status) = serde_json::from_slice::<Value>(&text)
                    && let Some(pid) = status["pid"].as_u64()
                {
                    return u32::try_from(pid).expect("pid");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(PATIENCE, poll)
            .await
            .expect("the runner reports its program")
    }

    /// The runbook's latest run once `done` holds for it.
    async fn run_where(&self, name: &str, done: impl Fn(&RunbookRun) -> bool) -> RunbookRun {
        let poll = async {
            loop {
                let list = self.list().await;
                if let Some(run) = list
                    .runbooks
                    .into_iter()
                    .find(|r| r.name.as_str() == name)
                    .and_then(|r| r.run)
                    .filter(|run| done(run))
                {
                    return run;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(PATIENCE, poll)
            .await
            .expect("the run gets there")
    }

    fn audit_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.audit)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("json line"))
            .collect()
    }

    fn runbook_lines(&self) -> Vec<Value> {
        self.audit_lines()
            .into_iter()
            .filter(|l| l["kind"] == "runbook")
            .collect()
    }
}

fn refusal(result: &RunbookResult) -> Option<RunbookRefusal> {
    match result {
        RunbookResult::Refused { reason, .. } => Some(*reason),
        _ => None,
    }
}

#[tokio::test]
async fn lists_each_runbook_with_its_exact_command() {
    let f = fixture().await;
    let list = f.list().await;
    let names: Vec<&str> = list.runbooks.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["Hello", "Fail", "Count", "Slow"], "config order");
    let hello = &list.runbooks[0];
    assert_eq!(hello.argv, ["printf", "hello from a runbook\\n"]);
    assert_eq!(Path::new(&hello.cwd), f.repo);
    assert_eq!(hello.aliases, ["please hello"]);
    assert!(hello.confirm);
    assert_eq!(hello.run, None);
    let anonymous = f.bridge.http("GET", "/v1/runbooks", &[]).await;
    assert_eq!(anonymous.status, 401);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn unknown_runbooks_are_404_and_touch_nothing() {
    let f = fixture().await;
    for (name, key) in [("Nope", KEY1), ("hello", KEY2), ("%20Hello", KEY3)] {
        let (status, result) = f.run(name, &body(key, "app")).await;
        assert_eq!(status, 404, "{name}: {result:?}");
        assert_eq!(refusal(&result), Some(RunbookRefusal::UnknownRunbook));
    }
    assert_eq!(f.bridge.fake().calls(), NO_CALLS);
    assert!(f.runbook_lines().iter().all(|l| l["phase"] != "execute"));
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn clients_cannot_supply_arguments() {
    let f = fixture().await;
    for extra in [
        json!({"argv": ["rm", "-rf", "/"]}),
        json!({"args": ["--force"]}),
        json!({"cwd": "/"}),
        json!({"env": {"PATH": "/tmp"}}),
        json!({"command": "id"}),
    ] {
        let mut request: Value = serde_json::from_str(&body(KEY1, "app")).expect("json");
        request
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let (status, result) = f.run("Hello", &request.to_string()).await;
        assert_eq!(status, 400, "{extra}: {result:?}");
        assert_eq!(refusal(&result), Some(RunbookRefusal::InvalidRequest));
    }
    // Query strings are not read either.
    let response = f
        .bridge
        .post_authed("/v1/runbooks/Hello/run?argv=id&cwd=/", &body(KEY1, "app"))
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    // What runs is the config's argv and cwd, through a bridge-made spec;
    // the typed line holds the runner and that spec's path, nothing else.
    let [(_, line)] = f
        .typed_at_least(1)
        .await
        .try_into()
        .expect("one line typed");
    assert_eq!(f.bridge.fake().calls().len(), 1, "only the launch line");
    let spec_path = line.trim().rsplit(' ').next().expect("spec").to_owned();
    assert_eq!(line, format!(" {RUNNER} runbook {spec_path}"));
    let spec: Value =
        serde_json::from_slice(&std::fs::read(&spec_path).expect("spec file")).expect("json");
    assert_eq!(spec["argv"], json!(["printf", "hello from a runbook\\n"]));
    assert_eq!(Path::new(spec["cwd"].as_str().expect("cwd")), f.repo);
    let mode = std::fs::metadata(&spec_path)
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "spec is private: {mode:o}");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn runs_in_a_tab_and_reports_exit_code_and_last_lines() {
    let f = fixture().await;
    let (status, result) = f.run("Hello", &body(KEY1, "app")).await;
    assert_eq!(status, 200, "{result:?}");
    let RunbookResult::Started {
        run,
        replayed: false,
    } = result
    else {
        panic!("not started: {result:?}");
    };
    assert_eq!(run.state, RunState::Running);
    assert_eq!(
        f.list().await.runbooks[0].run.as_ref().map(|r| r.state),
        Some(RunState::Running)
    );
    // The pane shows the output before the runner reports the end (as a
    // real terminal does): the watcher stops reading the pane once the run
    // has ended, so a screen set afterwards might never be seen.
    let [(pane, _)] = f
        .typed_at_least(1)
        .await
        .try_into()
        .expect("one line typed");
    f.bridge.fake().set_screen(
        &pane,
        "$ passporter-bridge runbook …\n▶ Hello\nhello from a runbook\n■ Hello: exit 0\n\n",
    );
    f.shell_runs(0).await;
    let done = f.run_where("Hello", RunbookRun::is_finished).await;
    assert!(done.succeeded(), "{done:?}");
    assert!(done.finished_at.is_some());
    assert!(
        done.last_lines.iter().any(|l| l == "hello from a runbook"),
        "{done:?}"
    );
    assert_eq!(
        done.last_lines.last().map(String::as_str),
        Some("■ Hello: exit 0")
    );

    // A failing runbook reports its code.
    let (_, result) = f.run("Fail", &body(KEY2, "app")).await;
    assert!(
        matches!(result, RunbookResult::Started { .. }),
        "{result:?}"
    );
    f.shell_runs(1).await;
    let failed = f.run_where("Fail", RunbookRun::is_finished).await;
    assert_eq!(
        (failed.state, failed.exit_code),
        (RunState::Exited, Some(3))
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn replays_the_same_key_without_running_twice() {
    let f = fixture().await;
    let (_, first) = f.run("Hello", &body(KEY1, "app")).await;
    assert!(matches!(
        first,
        RunbookResult::Started {
            replayed: false,
            ..
        }
    ));
    let (status, again) = f.run("Hello", &body(KEY1, "app")).await;
    assert_eq!(status, 200);
    let RunbookResult::Started { replayed: true, .. } = again else {
        panic!("not a replay: {again:?}");
    };
    assert_eq!(f.typed_at_least(1).await.len(), 1, "ran once");
    // The same key for another runbook is a conflict, not a second run.
    let (status, other) = f.run("Fail", &body(KEY1, "app")).await;
    assert_eq!(status, 422, "{other:?}");
    assert_eq!(refusal(&other), Some(RunbookRefusal::IdempotencyConflict));
    // A replay after the run ended says how it ended.
    f.shell_runs(0).await;
    f.run_where("Hello", RunbookRun::is_finished).await;
    let (_, late) = f.run("Hello", &body(KEY1, "app")).await;
    let RunbookResult::Started {
        run,
        replayed: true,
    } = late
    else {
        panic!("not a replay: {late:?}");
    };
    assert!(run.succeeded(), "{run:?}");
    assert_eq!(f.typed_at_least(1).await.len(), 1, "still once");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn one_run_per_runbook_at_a_time() {
    let f = fixture().await;
    let (_, first) = f.run("Hello", &body(KEY1, "app")).await;
    assert!(matches!(first, RunbookResult::Started { .. }), "{first:?}");
    let (status, second) = f.run("Hello", &body(KEY2, "app")).await;
    assert_eq!(status, 409, "{second:?}");
    assert_eq!(refusal(&second), Some(RunbookRefusal::Busy));
    // Another runbook may run meanwhile.
    let (_, other) = f.run("Fail", &body(KEY3, "app")).await;
    assert!(matches!(other, RunbookResult::Started { .. }), "{other:?}");
    assert_eq!(f.typed_at_least(2).await.len(), 2);
    // Once the first ends, Hello may run again.
    f.shell_runs(0).await;
    f.run_where("Hello", RunbookRun::is_finished).await;
    let fresh = "00000000-0000-4000-8000-000000000004";
    let (status, third) = f.run("Hello", &body(fresh, "app")).await;
    assert_eq!(status, 200, "{third:?}");
    assert_eq!(f.typed_at_least(3).await.len(), 3);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_run_that_never_starts_fails_and_can_never_start_late() {
    let f = fixture_with(|settings| settings.start_wait = Duration::from_millis(100)).await;
    let (_, first) = f.run("Count", &body(KEY1, "app")).await;
    assert!(matches!(first, RunbookResult::Started { .. }));
    // Nobody plays the shell: the runner never claims the run.
    let failed = f.run_where("Count", RunbookRun::is_finished).await;
    assert_eq!(failed.state, RunState::Failed);
    assert!(failed.message.is_some());
    // A shell that finally runs the line finds the bridge's claim.
    f.shell_runs(0).await;
    assert_eq!(f.counted(), 0, "a run given up on never starts");
    let (status, _) = f.run("Count", &body(KEY2, "app")).await;
    assert_eq!(status, 200, "the runbook is free again");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_launch_line_is_typed_once_and_runs_once() {
    let f = fixture().await;
    f.run("Count", &body(KEY1, "app")).await;
    f.typed_at_least(1).await;
    // Unclaimed for a while: never typed again (a slow shell would hand a
    // second copy to the runbook's program as input).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(f.typed().len(), 1);
    // The same line run twice (a shell replaying it) still runs once.
    f.shell_runs(0).await;
    f.shell_runs(0).await;
    let done = f.run_where("Count", RunbookRun::is_finished).await;
    assert!(done.succeeded(), "{done:?}");
    assert_eq!(f.counted(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn an_unreadable_claim_ends_as_never_started() {
    let f = fixture_with(|settings| settings.start_wait = Duration::from_millis(100)).await;
    f.run("Count", &body(KEY1, "app")).await;
    f.typed_at_least(1).await;
    // A runner that died between creating its claim and writing it.
    let status = f
        .audit
        .with_file_name(format!("runbooks/{KEY1}.status.json"));
    std::fs::write(&status, b"").expect("empty claim");
    let ended = f.run_where("Count", RunbookRun::is_finished).await;
    assert_eq!(ended.state, RunState::Failed);
    let (status, _) = f.run("Count", &body(KEY2, "app")).await;
    assert_eq!(status, 200, "the runbook is free again");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_run_that_ends_between_bridges_is_audited_once() {
    let f = fixture().await;
    f.run("Count", &body(KEY1, "app")).await;
    f.typed_at_least(1).await;
    // The bridge stops (a runbook restarting it), then the run ends.
    let Fixture {
        bridge, _tmp: tmp, ..
    } = f;
    bridge.stop().await.expect("clean stop");
    // No bridge watches: the shell runs the line now.
    let out = tokio::process::Command::new(RUNNER)
        .arg("runbook")
        .arg(tmp.path().join(format!("state/runbooks/{KEY1}.spec.json")))
        .current_dir(tmp.path().join("repo"))
        .output()
        .await
        .expect("runner");
    assert!(out.status.success());
    let audit = tmp.path().join("state/audit.jsonl");
    let finishes = |audit: &Path| {
        std::fs::read_to_string(audit)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("\"phase\":\"finish\""))
            .count()
    };
    assert_eq!(finishes(&audit), 0, "nobody watched the end");
    let f = fixture_in(tmp, |_| {}).await;
    let done = f.run_where("Count", RunbookRun::is_finished).await;
    assert!(done.succeeded(), "{done:?}");
    let finish = f
        .runbook_lines()
        .into_iter()
        .filter(|l| l["phase"] == "finish")
        .collect::<Vec<_>>();
    assert_eq!(finish.len(), 1, "{finish:?}");
    assert_eq!(finish[0]["outcome"], "exit 0");
    assert_eq!(finish[0]["idempotencyKey"], KEY1);
    // Another restart writes no second line.
    let Fixture {
        bridge, _tmp: tmp, ..
    } = f;
    bridge.stop().await.expect("clean stop");
    let f = fixture_in(tmp, |_| {}).await;
    f.run_where("Count", RunbookRun::is_finished).await;
    assert_eq!(
        f.runbook_lines()
            .iter()
            .filter(|l| l["phase"] == "finish")
            .count(),
        1
    );
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_restart_gives_up_on_unclaimed_runs_and_types_nothing() {
    let f = fixture().await;
    f.run("Count", &body(KEY1, "app")).await;
    f.typed_at_least(1).await;
    let Fixture {
        bridge, _tmp: tmp, ..
    } = f;
    bridge.stop().await.expect("clean stop");
    // The shell never ran the line; the new bridge must not retype it into
    // a pane herdr may have reused.
    let f = fixture_in(tmp, |_| {}).await;
    let ended = f.run_where("Count", RunbookRun::is_finished).await;
    assert_eq!(ended.state, RunState::Failed);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.typed().len(), 0);
    // A late runner finds the claim.
    let spec = f.audit.with_file_name(format!("runbooks/{KEY1}.spec.json"));
    let out = tokio::process::Command::new(RUNNER)
        .arg("runbook")
        .arg(&spec)
        .current_dir(&f.repo)
        .output()
        .await
        .expect("runner");
    assert!(!out.status.success());
    assert_eq!(f.counted(), 0);
    let (status, _) = f.run("Count", &body(KEY2, "app")).await;
    assert_eq!(status, 200);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn an_interrupted_runner_ends_the_run_at_once() {
    let f = fixture().await;
    f.run("Slow", &body(KEY1, "app")).await;
    let mut runner = f.shell_starts(0).await;
    let program = f.running_pid(KEY1).await;
    // Ctrl-C in the tab, or the tab closed: the runner dies without a word.
    runner.kill().await.expect("kill the runner");
    let _ = std::process::Command::new("kill")
        .arg(program.to_string())
        .status();
    let ended = f.run_where("Slow", RunbookRun::is_finished).await;
    assert_eq!(ended.state, RunState::Failed);
    assert!(
        ended
            .message
            .as_deref()
            .is_some_and(|m| m.contains("stopped")),
        "{ended:?}"
    );
    let (status, _) = f.run("Slow", &body(KEY2, "app")).await;
    assert_eq!(status, 200, "the runbook is free again");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_retry_after_a_restart_never_runs_twice() {
    let f = fixture().await;
    f.run("Count", &body(KEY1, "app")).await;
    f.shell_runs(0).await;
    f.run_where("Count", RunbookRun::is_finished).await;
    // The bridge restarts (a runbook may do that): its ledger is gone.
    let Fixture {
        bridge, _tmp: tmp, ..
    } = f;
    bridge.stop().await.expect("clean stop");
    let f = fixture_in(tmp, |_| {}).await;
    let finished = f.run_where("Count", RunbookRun::is_finished).await;
    assert!(
        finished.succeeded(),
        "the latest run survives: {finished:?}"
    );
    let (status, again) = f.run("Count", &body(KEY1, "app")).await;
    assert_eq!(status, 200, "{again:?}");
    let RunbookResult::Started {
        run,
        replayed: true,
    } = again
    else {
        panic!("not a replay: {again:?}");
    };
    assert!(run.succeeded(), "{run:?}");
    let (status, other) = f.run("Hello", &body(KEY1, "app")).await;
    assert_eq!(
        (status, refusal(&other)),
        (422, Some(RunbookRefusal::IdempotencyConflict))
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.typed().len(), 0, "nothing typed after the restart");
    assert_eq!(f.counted(), 1);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_restarted_bridge_follows_runs_still_going() {
    let f = fixture().await;
    f.run("Slow", &body(KEY1, "app")).await;
    let mut runner = f.shell_starts(0).await;
    let program = f.running_pid(KEY1).await;
    let Fixture {
        bridge, _tmp: tmp, ..
    } = f;
    bridge.stop().await.expect("clean stop");
    let f = fixture_in(tmp, |_| {}).await;
    // The run still goes: the runbook is busy, and the run is followed.
    let running = f
        .run_where("Slow", |run| run.state == RunState::Running)
        .await;
    assert!(!running.is_finished());
    let (status, busy) = f.run("Slow", &body(KEY2, "app")).await;
    assert_eq!((status, refusal(&busy)), (409, Some(RunbookRefusal::Busy)));
    // It ends (here: the program is killed) and the new bridge says how.
    let _ = std::process::Command::new("kill")
        .arg(program.to_string())
        .status();
    let ended = f.run_where("Slow", RunbookRun::is_finished).await;
    assert_eq!(
        (ended.state, ended.exit_code),
        (RunState::Exited, None),
        "{ended:?}"
    );
    let _ = runner.wait().await;
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn audits_before_executing_and_when_the_run_ends() {
    let f = fixture().await;
    f.run("Hello", &body(KEY1, "app")).await;
    let lines = f.runbook_lines();
    let [execute, result] = lines.as_slice() else {
        panic!("{lines:?}");
    };
    assert_eq!(execute["phase"], "execute");
    assert_eq!(execute["device"], "test-device");
    assert_eq!(execute["runbook"], "Hello");
    assert_eq!(execute["idempotencyKey"], KEY1);
    assert_eq!(execute["origin"], "app");
    assert_eq!(
        execute["argv"],
        json!(["printf", "hello from a runbook\\n"])
    );
    assert_eq!(Path::new(execute["cwd"].as_str().expect("cwd")), f.repo);
    assert_eq!(result["phase"], "result");
    assert_eq!(result["outcome"], "started");
    f.shell_runs(0).await;
    f.run_where("Hello", RunbookRun::is_finished).await;
    let finish = tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(line) = f
                .runbook_lines()
                .into_iter()
                .find(|l| l["phase"] == "finish")
            {
                return line;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a finish line");
    assert_eq!(finish["outcome"], "exit 0");
    // Refusals are audited too, never with an execute line.
    f.run("Hello", &body(KEY2, "car")).await;
    let car: Vec<Value> = f
        .runbook_lines()
        .into_iter()
        .filter(|l| l["idempotencyKey"] == KEY2)
        .collect();
    assert_eq!(car.len(), 1, "{car:?}");
    assert_eq!(car[0]["outcome"], "refused: car_forbidden");
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_car_can_never_run_runbooks() {
    let f = fixture().await;
    let (status, result) = f.run("Hello", &body(KEY1, "car")).await;
    assert_eq!(status, 403, "{result:?}");
    assert_eq!(refusal(&result), Some(RunbookRefusal::CarForbidden));
    assert_eq!(f.bridge.fake().calls(), NO_CALLS);
    // Notifications are not the car: allowed.
    let (status, _) = f.run("Hello", &body(KEY2, "notification")).await;
    assert_eq!(status, 200);
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn runs_are_rate_limited_per_device() {
    let f = fixture_with(|settings| {
        settings.rate = RateLimit {
            requests: 1,
            window: Duration::from_secs(600),
        };
    })
    .await;
    let (status, _) = f.run("Hello", &body(KEY1, "app")).await;
    assert_eq!(status, 200);
    let path = "/v1/runbooks/Fail/run";
    let response = f.bridge.post_authed(path, &body(KEY2, "app")).await;
    assert_eq!(response.status, 429, "{}", response.body);
    assert!(response.header("retry-after").is_some());
    assert!(response.body.contains("rate_limited"));
    // Transient: the key is not spent, and nothing ran.
    let again = f.bridge.post_authed(path, &body(KEY2, "app")).await;
    assert_eq!(again.status, 429);
    assert_eq!(f.typed_at_least(1).await.len(), 1);
    // Refusals do not spend the budget: a busy Hello is still busy, not limited.
    let (status, busy) = f.run("Hello", &body(KEY3, "app")).await;
    assert_eq!((status, refusal(&busy)), (409, Some(RunbookRefusal::Busy)));
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn herdr_down_runs_nothing_and_keeps_the_key() {
    // A budget of one: a "nothing ran" answer must not spend it.
    let f = fixture_with(|settings| {
        settings.rate = RateLimit {
            requests: 1,
            window: Duration::from_secs(600),
        };
    })
    .await;
    f.bridge.fake().fail_next(
        herdr::Method::WorkspaceCreate,
        herdr::Fault::Reject(herdr::ErrorCode::ServerUnavailable),
    );
    let response = f
        .bridge
        .post_authed("/v1/runbooks/Hello/run", &body(KEY1, "app"))
        .await;
    assert_eq!(response.status, 503, "{}", response.body);
    // The same key may be retried, and runs once herdr is back.
    let (status, result) = f.run("Hello", &body(KEY1, "app")).await;
    assert_eq!(status, 200, "{result:?}");
    assert!(matches!(
        result,
        RunbookResult::Started {
            replayed: false,
            ..
        }
    ));
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn the_stream_carries_each_change_of_a_run() {
    let f = fixture().await;
    let mut stream = f.bridge.stream().await;
    assert!(matches!(
        stream.next_event().await,
        protocol::AgentEvent::Snapshot { .. }
    ));
    f.run("Hello", &body(KEY1, "app")).await;
    f.shell_runs(0).await;
    let mut states = Vec::new();
    while states.last() != Some(&RunState::Exited) {
        if let protocol::AgentEvent::RunbookUpdated { run: Some(run) } = stream.next_event().await {
            assert_eq!(run.runbook.as_str(), "Hello");
            states.push(run.state);
        }
    }
    assert_eq!(states.first(), Some(&RunState::Running));
    f.bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn a_run_seen_ending_may_run_again_at_once() {
    let f = fixture().await;
    let mut stream = f.bridge.stream().await;
    assert!(matches!(
        stream.next_event().await,
        protocol::AgentEvent::Snapshot { .. }
    ));
    f.run("Hello", &body(KEY1, "app")).await;
    f.shell_runs(0).await;
    loop {
        if let protocol::AgentEvent::RunbookUpdated { run: Some(run) } = stream.next_event().await
            && run.is_finished()
        {
            break;
        }
    }
    // The end is published only once the runbook is free (and the end
    // audited): a tap the moment the phone shows it is not refused as busy.
    let (status, again) = f.run("Hello", &body(KEY2, "app")).await;
    assert_eq!(status, 200, "{again:?}");
    assert!(
        f.runbook_lines().iter().any(|l| l["phase"] == "finish"),
        "audited before it was published"
    );
    f.bridge.stop().await.expect("clean stop");
}
