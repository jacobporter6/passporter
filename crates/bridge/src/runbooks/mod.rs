//! Runbooks: named, preconfigured operations the phone may run, by tap or
//! by voice (`GET /v1/runbooks`, `POST /v1/runbooks/:name/run`; protocol
//! `runbook`). Never free-form shell.
//!
//! Everything a run executes comes from `[[runbooks]]` in the config: the
//! phone names a runbook, and the request body has no field for anything
//! else (strict serde). Order: parse (400 `invalid_request`) → the name
//! (404 `unknown_runbook`) → idempotency ledger (replay, in flight or
//! `idempotency_conflict`) → origin: never the car (403 `car_forbidden`)
//! → the runbook (404) → one run per runbook at a time (409 `busy`) →
//! per-device rate limit (429 `rate_limited`, key released); a key whose
//! run files exist (an earlier try, before a bridge restart) replays that
//! run instead of running again → audit
//! `execute` line (with the exact `argv` and `cwd`) → herdr → audit
//! `result` line; when the run ends, an audit `finish` line.
//!
//! In herdr: a tab in the spawn workspace (the `[spawn] workspace`, made
//! on first use, unfocused) labelled with the runbook's name, its shell in
//! the runbook's `cwd`. The bridge writes the run's spec to a `0600` file
//! in `<state dir>/runbooks/` and types one line into that shell, which
//! starts the [`runner`] on it: the runner executes `argv` directly (no
//! shell), with its output in the pane like any other, and reports back
//! through a status file ([`files`]). A watcher ([`watch`]) follows that
//! file and the pane's last lines and publishes each change as
//! `runbook_updated` on the stream, until the run exits, times out, is
//! interrupted, or never starts. The files outlive the bridge: on startup
//! [`Runbooks::resume`] picks up runs still going (a runbook that
//! restarted the bridge) and the latest run of each runbook.

mod audit;
mod files;
pub mod runner;
mod watch;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use herdr::PaneId;
use protocol::{
    AgentEvent, IdempotencyKey, Origin, RunState, Runbook, RunbookList, RunbookName,
    RunbookRefusal, RunbookResult, RunbookRun, RunbookRunRequest,
};

use self::audit::{Asked, RunbookEntry};
use self::files::RunFiles;
use self::runner::{RunSpec, RunnerStatus};
use self::watch::{Ending, Watch};
use crate::actions::Budget;
use crate::actions::audit::AuditLog;
use crate::actions::ledger::{Claim, Idempotent, Ledger};
use crate::actions::retry_after_secs;
use crate::auth::{RateLimit, RateLimiter};
use crate::config::{RunbookConfig, shell_quote};
use crate::registry::Hub;
use crate::server::{AppState, ClientAddr, Principal};
use crate::spawn::SpawnControl;
use crate::spawn::tabs::Tabs;

/// Longest parse error echoed back to the client.
const MAX_ERROR_CHARS: usize = 200;

/// The runs directory inside the state dir.
const RUNS_DIR: &str = "runbooks";

/// Settings for runbooks.
#[derive(Debug, Clone)]
pub struct RunbookSettings {
    /// The `[[runbooks]]`; empty turns runbooks off (every name is unknown).
    pub runbooks: Vec<RunbookConfig>,
    /// Where run specs and status files go (`0700`).
    pub runs_dir: PathBuf,
    /// The runner: this very binary, run as `<runner> runbook <spec>`.
    pub runner: PathBuf,
    /// Most finished results remembered for idempotent replay.
    pub idempotency_capacity: usize,
    /// Runs each device may ask for (design §8: per-device rate limits).
    pub rate: RateLimit,
    /// How often the watcher looks at a run.
    pub poll: Duration,
    /// How long a fresh tab's shell may take to settle before the launch
    /// line is typed anyway (it is typed once, never again).
    pub shell_wait: Duration,
    /// How long the runner may take to claim the run before the bridge
    /// gives up on it (and claims it as failed, so it never runs).
    pub start_wait: Duration,
}

impl RunbookSettings {
    /// Defaults for `runbooks` with state in `state_dir`: the running
    /// binary as the runner; remember 256 results; 10 runs per 10 minutes
    /// per device; look every second; up to 5 s for a fresh shell to
    /// settle and 30 s for the runner to claim the run. The runs dir is
    /// made absolute (a relative state dir would otherwise put a path
    /// relative to the runbook's `cwd` on the launch line).
    #[must_use]
    pub fn new(runbooks: Vec<RunbookConfig>, state_dir: &Path) -> Self {
        Self {
            runbooks,
            runs_dir: std::path::absolute(state_dir.join(RUNS_DIR))
                .unwrap_or_else(|_| state_dir.join(RUNS_DIR)),
            runner: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("passporter-bridge")),
            idempotency_capacity: 256,
            rate: RateLimit {
                requests: 10,
                window: Duration::from_secs(600),
            },
            poll: Duration::from_secs(1),
            shell_wait: Duration::from_secs(5),
            start_wait: Duration::from_secs(30),
        }
    }
}

/// A run request as the ledger keys it: the name from the path and the
/// body, so one key used for two runbooks is a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunRequest {
    name: RunbookName,
    body: RunbookRunRequest,
}

impl Idempotent for RunRequest {
    type Outcome = RunbookResult;

    fn idempotency_key(&self) -> &IdempotencyKey {
        &self.body.idempotency_key
    }

    fn replayed(outcome: RunbookResult) -> RunbookResult {
        match outcome {
            RunbookResult::Started { run, .. } => RunbookResult::Started {
                run,
                replayed: true,
            },
            other => other,
        }
    }

    fn is_transient(outcome: &RunbookResult) -> bool {
        matches!(outcome, RunbookResult::Refused { reason, .. } if reason.is_transient())
    }
}

/// The ledger, limiter, latest runs and herdr seam behind the routes.
pub struct Runbooks {
    settings: RunbookSettings,
    control: Arc<dyn SpawnControl>,
    tabs: Arc<Tabs>,
    hub: Arc<Hub>,
    ledger: Arc<Ledger<RunRequest>>,
    limit: RateLimiter<Budget>,
    audit: Arc<AuditLog>,
    /// The latest run of each runbook since the bridge started.
    runs: Mutex<HashMap<RunbookName, RunbookRun>>,
    /// Runbooks with a run under way (claimed before anything starts).
    active: Mutex<HashSet<RunbookName>>,
    /// The watchers following runs.
    watchers: Mutex<tokio::task::JoinSet<()>>,
}

impl std::fmt::Debug for Runbooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runbooks")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// The services a [`Runbooks`] works with.
pub(crate) struct RunbookDeps {
    /// herdr.
    pub(crate) control: Arc<dyn SpawnControl>,
    /// Tabs in the spawn workspace (shared with [`crate::spawn`]).
    pub(crate) tabs: Arc<Tabs>,
    /// Where `runbook_updated` events go.
    pub(crate) hub: Arc<Hub>,
    /// The actions' audit log.
    pub(crate) audit: Arc<AuditLog>,
}

impl Runbooks {
    pub(crate) fn new(settings: RunbookSettings, deps: RunbookDeps) -> Self {
        Self {
            ledger: Ledger::new(settings.idempotency_capacity),
            limit: RateLimiter::new(settings.rate),
            settings,
            control: deps.control,
            tabs: deps.tabs,
            hub: deps.hub,
            audit: deps.audit,
            runs: Mutex::new(HashMap::new()),
            active: Mutex::new(HashSet::new()),
            watchers: Mutex::new(tokio::task::JoinSet::new()),
        }
    }

    fn config(&self, name: &RunbookName) -> Option<&RunbookConfig> {
        self.settings.runbooks.iter().find(|r| r.name == *name)
    }

    fn latest(&self, name: &RunbookName) -> Option<RunbookRun> {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// Stores `run` as its runbook's latest and tells the stream. The lock
    /// is held while publishing, so the stream sees runs in the order they
    /// are stored.
    fn record(&self, run: &RunbookRun) {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        runs.insert(run.runbook.clone(), run.clone());
        self.hub.publish(AgentEvent::RunbookUpdated {
            run: Some(run.clone()),
        });
    }

    /// Records a run's end, published by its watcher after it released the
    /// runbook: only while this run is still the runbook's latest. The
    /// next run may have started (and been recorded) in between, and a late
    /// end must not hide it. Runs are told apart by `started_at` being
    /// equal, never by which is larger: the wall clock may step back.
    fn record_end(&self, run: &RunbookRun) {
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        if runs
            .get(&run.runbook)
            .is_some_and(|known| known.started_at != run.started_at)
        {
            return;
        }
        runs.insert(run.runbook.clone(), run.clone());
        self.hub.publish(AgentEvent::RunbookUpdated {
            run: Some(run.clone()),
        });
    }

    /// Picks up what the run files say (after a restart), before the
    /// bridge serves requests:
    ///
    /// - a run that ended while no bridge watched it gets its audit
    ///   `finish` line now (once: see [`files`]);
    /// - a spec its runner never claimed (the bridge stopped in between)
    ///   is given up on, never launched again: herdr may have reused its
    ///   pane for something else;
    /// - a run still going is followed, which holds its runbook busy;
    /// - each runbook's latest run is shown again.
    pub(crate) async fn resume(self: &Arc<Self>) {
        for (files, spec) in files::all(&self.settings.runs_dir).await {
            let Ok(name) = RunbookName::new(spec.runbook.clone()) else {
                continue;
            };
            let mut status = files.status().await;
            if status.is_none() {
                let never = || RunnerStatus::Failed {
                    message: watch::NEVER_STARTED.to_owned(),
                };
                if !files.give_up(never()).await && files.status().await.is_none() {
                    // A claim the runner never finished writing.
                    files.overwrite(never()).await;
                }
                status = files.status().await;
            }
            let run = run_from(&name, &spec, status.as_ref());
            let asked = Asked {
                device: &spec.device,
                key: files.key(),
                origin: spec.origin,
                runbook: &name,
            };
            if run.is_finished() && !files.is_audited().await {
                self.audit_finish(&files, asked, &run).await;
            }
            let newer = self
                .latest(&name)
                .is_none_or(|known| known.started_at <= run.started_at);
            if newer && self.config(&name).is_some() {
                self.record(&run);
            }
            if run.is_finished() {
                continue;
            }
            let Some(active) = self.claim(&name) else {
                continue;
            };
            tracing::info!(runbook = %name, pane = %spec.pane, "following a runbook run started before this bridge");
            let key = files.key().to_owned();
            self.follow(Watch {
                active,
                pane: PaneId::new(spec.pane.clone()),
                files,
                run,
                timeout: Duration::from_secs(spec.timeout_secs),
                launch: None,
                device: spec.device.clone(),
                key,
                origin: spec.origin,
            });
        }
    }

    /// Follows a run in the background, until it ends or [`Self::stop`].
    fn follow(&self, watch: Watch) {
        self.watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawn(watch.follow());
    }

    /// Stops following runs (the bridge is stopping). The runs go on; the
    /// next bridge picks them up from their files.
    pub(crate) fn stop(&self) {
        self.watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .abort_all();
    }

    /// Writes the run's audit `finish` line and notes it in its files, so
    /// no later bridge writes it again.
    async fn audit_finish(&self, files: &RunFiles, asked: Asked<'_>, run: &RunbookRun) {
        audit_line(&self.audit, RunbookEntry::finish(asked, run)).await;
        files.mark_audited().await;
    }

    /// Claims `name` for one run; `None` if it has one under way.
    fn claim(self: &Arc<Self>, name: &RunbookName) -> Option<Active> {
        let fresh = self
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.clone());
        fresh.then(|| Active {
            runbooks: Arc::clone(self),
            name: name.clone(),
        })
    }

    fn list(&self) -> RunbookList {
        let runbooks = self
            .settings
            .runbooks
            .iter()
            .map(|config| Runbook {
                name: config.name.clone(),
                aliases: config.aliases.clone(),
                argv: config.argv.clone(),
                cwd: config.cwd.to_string_lossy().into_owned(),
                confirm: config.confirm,
                run: self.latest(&config.name),
            })
            .collect();
        RunbookList { runbooks }
    }

    /// A stored `started` result with its run brought up to date (a replay
    /// should say whether the run has ended since).
    fn refreshed(&self, result: RunbookResult) -> RunbookResult {
        match result {
            RunbookResult::Started { run, replayed } => {
                let latest = self
                    .latest(&run.runbook)
                    .filter(|latest| latest.started_at == run.started_at);
                RunbookResult::Started {
                    run: latest.unwrap_or(run),
                    replayed,
                }
            }
            other => other,
        }
    }
}

/// A runbook's claim on its one run; released when dropped.
struct Active {
    runbooks: Arc<Runbooks>,
    name: RunbookName,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.runbooks
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.name);
    }
}

/// `GET /v1/runbooks`: every runbook with its exact command, and its
/// latest run.
pub(crate) async fn list(State(state): State<AppState>) -> Json<RunbookList> {
    Json(state.runbooks.list())
}

/// An outcome plus whether it is final for the key.
struct Outcome {
    result: RunbookResult,
    /// False when nothing ran and nothing was decided (herdr or the audit
    /// log unavailable first): 503, the same key may be retried.
    decided: bool,
    /// For a rate-limited request: when to try again (`Retry-After`).
    retry_after: Option<Duration>,
}

impl Outcome {
    fn decided(result: RunbookResult) -> Self {
        Self {
            result,
            decided: true,
            retry_after: None,
        }
    }

    fn refused(reason: RunbookRefusal, message: impl Into<String>) -> Self {
        Self::decided(refused(reason, message))
    }

    fn failed(message: impl Into<String>) -> Self {
        Self::decided(RunbookResult::Failed {
            message: message.into(),
        })
    }

    fn undecided(message: &str) -> Self {
        Self {
            decided: false,
            ..Self::failed(message)
        }
    }

    fn respond(&self) -> Response {
        let mut response = respond(&self.result, self.decided);
        if let Some(wait) = self.retry_after {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from(retry_after_secs(wait)),
            );
        }
        response
    }
}

/// `POST /v1/runbooks/:name/run`.
pub(crate) async fn run(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(client): Extension<ClientAddr>,
    UrlPath(raw): UrlPath<String>,
    body: Bytes,
) -> Response {
    let body: RunbookRunRequest = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => {
            let message: String = error.to_string().chars().take(MAX_ERROR_CHARS).collect();
            return respond(&refused(RunbookRefusal::InvalidRequest, message), true);
        }
    };
    let Ok(name) = RunbookName::new(raw) else {
        return respond(&unknown_runbook(), true);
    };
    let request = RunRequest { name, body };
    let runbooks = Arc::clone(&state.runbooks);
    let guard = match runbooks.ledger.claim(&request) {
        Claim::Fresh(guard) => guard,
        Claim::Replay(result) => return respond(&runbooks.refreshed(result), true),
        Claim::Conflict => {
            let result = refused(
                RunbookRefusal::IdempotencyConflict,
                "This idempotency key was already used for a different request.",
            );
            return respond(&result, true);
        }
        // Starting a run takes a few herdr calls: tell the retry to wait.
        Claim::InFlight => {
            return rate_limited(
                "This run is starting; retry shortly.",
                Duration::from_secs(1),
            )
            .respond();
        }
    };
    let budget = Budget::of(&principal, client);
    // Its own task: a client hanging up cannot leave a run half-recorded.
    let work = tokio::spawn(async move {
        let outcome = decide_and_run(&runbooks, &principal.device, &budget, &request).await;
        let asked = asked(&principal.device, &request);
        audit_line(
            &runbooks.audit,
            RunbookEntry::result(asked, &outcome.result),
        )
        .await;
        guard.finish(&outcome.result, outcome.decided);
        outcome
    });
    match work.await {
        Ok(outcome) => outcome.respond(),
        Err(error) => {
            tracing::error!(%error, "runbook task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn asked<'a>(device: &'a str, request: &'a RunRequest) -> Asked<'a> {
    Asked {
        device,
        key: request.body.idempotency_key.as_str(),
        origin: request.body.origin,
        runbook: &request.name,
    }
}

/// Checks `request` against policy and the config, spends the device's
/// rate-limit budget only for a request that passes, then audits and
/// starts the run.
async fn decide_and_run(
    runbooks: &Arc<Runbooks>,
    device: &str,
    budget: &Budget,
    request: &RunRequest,
) -> Outcome {
    if request.body.origin == Origin::Car {
        return Outcome::refused(
            RunbookRefusal::CarForbidden,
            "Runbooks never run from the car.",
        );
    }
    let Some(config) = runbooks.config(&request.name) else {
        return Outcome::decided(unknown_runbook());
    };
    // An earlier try of this key may have started the run before a bridge
    // restart (which a runbook can cause) wiped the ledger: its files say.
    let files = RunFiles::new(&runbooks.settings.runs_dir, &request.body.idempotency_key);
    if let Some(spec) = files.read_spec().await {
        if spec.runbook != config.name.as_str() || spec.origin != request.body.origin {
            return Outcome::refused(
                RunbookRefusal::IdempotencyConflict,
                "This idempotency key already started a different run.",
            );
        }
        let run = runbooks
            .latest(&config.name)
            .filter(|latest| latest.started_at == spec.started_at)
            .unwrap_or_else(|| run_from(&config.name, &spec, None));
        return Outcome::decided(RunbookResult::Started {
            run,
            replayed: true,
        });
    }
    let Some(active) = runbooks.claim(&config.name) else {
        return Outcome::refused(
            RunbookRefusal::Busy,
            format!("{} is running already; wait for it to finish.", config.name),
        );
    };
    if let Err(wait) = runbooks.limit.check(budget) {
        // Transient: the ledger releases the key for a later retry.
        return rate_limited(
            "Too many runbook runs from this device; retry in a few minutes.",
            wait,
        );
    }
    let asked = asked(device, request);
    let execute = RunbookEntry::execute(asked, &config.argv, &config.cwd);
    let outcome = if let Err(error) = append(&runbooks.audit, &execute).await {
        tracing::error!(%error, "cannot write the audit log; refusing to run");
        Outcome::undecided("The audit log is unavailable; nothing ran.")
    } else {
        start(runbooks, config, request, active, device, files).await
    };
    if !outcome.decided {
        // Nothing ran: the retry this invites must not cost budget.
        runbooks.limit.refund(budget);
    }
    outcome
}

/// herdr's part: the tab and the spec; then the watcher, which types the
/// launch line once the tab's shell is ready and follows the run.
async fn start(
    runbooks: &Arc<Runbooks>,
    config: &RunbookConfig,
    request: &RunRequest,
    active: Active,
    device: &str,
    files: RunFiles,
) -> Outcome {
    let label = config.name.as_str();
    let (tab, pane) = match runbooks.tabs.open(label, &config.cwd).await {
        Ok((tab, pane)) => (tab, pane.pane_id),
        Err(error) if !error.may_have_taken_effect() => {
            tracing::warn!(%error, "herdr could not open a tab for a runbook");
            return Outcome::undecided("herdr is unavailable; nothing ran.");
        }
        Err(error) => {
            tracing::warn!(%error, "opening a tab for a runbook failed");
            return Outcome::failed(format!(
                "herdr: {error}. Nothing ran, but herdr may have opened an empty {label} tab \
                 in the {} workspace; close it if so.",
                runbooks.tabs.workspace()
            ));
        }
    };
    let started_at = crate::registry::now();
    let spec = RunSpec {
        runbook: config.name.to_string(),
        cwd: config.cwd.clone(),
        argv: config.argv.clone(),
        timeout_secs: config.timeout.as_secs(),
        status: files.status.clone(),
        started_at,
        pane: pane.to_string(),
        device: device.to_owned(),
        origin: request.body.origin,
    };
    if let Err(error) = files.write_spec(&spec).await {
        tracing::error!(%error, path = %files.spec.display(), "cannot write a runbook spec");
        runbooks.tabs.close(&tab).await;
        return Outcome::failed("The bridge could not prepare the run; nothing ran.");
    }
    let run = run_from(&config.name, &spec, None);
    tracing::info!(runbook = %config.name, %pane, "runbook started from the phone");
    runbooks.record(&run);
    let watch = Watch {
        active,
        pane,
        launch: Some(launch_line(&runbooks.settings.runner, &files.spec)),
        files,
        run: run.clone(),
        timeout: config.timeout,
        device: device.to_owned(),
        key: request.body.idempotency_key.to_string(),
        origin: request.body.origin,
    };
    runbooks.follow(watch);
    Outcome::decided(RunbookResult::Started {
        run,
        replayed: false,
    })
}

/// The run `spec` records, as `status` (if any) says it stands. Output
/// lines and the end time are not kept in the files.
fn run_from(name: &RunbookName, spec: &RunSpec, status: Option<&RunnerStatus>) -> RunbookRun {
    let timeout = Duration::from_secs(spec.timeout_secs);
    let ending = status.and_then(|status| Ending::of(status, timeout));
    RunbookRun {
        runbook: name.clone(),
        state: ending.as_ref().map_or(RunState::Running, |e| e.state),
        exit_code: ending.as_ref().and_then(|e| e.exit_code),
        message: ending.and_then(|e| e.message),
        last_lines: Vec::new(),
        started_at: spec.started_at,
        finished_at: None,
    }
}

/// The line typed into the tab's shell: the runner on the spec, both
/// bridge-made paths quoted for the shell (single quotes unless plain).
/// The leading space keeps it out of shell history where the shell
/// honours that (`HISTCONTROL`, `hist_ignore_space`).
fn launch_line(runner: &Path, spec: &Path) -> String {
    format!(" {} runbook {}", shell_quote(runner), shell_quote(spec))
}

fn refused(reason: RunbookRefusal, message: impl Into<String>) -> RunbookResult {
    RunbookResult::Refused {
        reason,
        message: message.into(),
    }
}

fn unknown_runbook() -> RunbookResult {
    refused(
        RunbookRefusal::UnknownRunbook,
        "The bridge has no runbook by that name.",
    )
}

fn rate_limited(message: &str, wait: Duration) -> Outcome {
    Outcome {
        retry_after: Some(wait),
        ..Outcome::refused(RunbookRefusal::RateLimited, message)
    }
}

async fn append(audit: &Arc<AuditLog>, entry: &RunbookEntry<'_>) -> std::io::Result<()> {
    let line = serde_json::to_value(entry).map_err(std::io::Error::other)?;
    let audit = Arc::clone(audit);
    tokio::task::spawn_blocking(move || audit.append(&line))
        .await
        .map_err(std::io::Error::other)?
}

/// Best effort: what it records has already happened.
async fn audit_line(audit: &Arc<AuditLog>, entry: RunbookEntry<'_>) {
    if let Err(error) = append(audit, &entry).await {
        tracing::error!(%error, "cannot write a runbook audit line");
    }
}

fn respond(result: &RunbookResult, decided: bool) -> Response {
    let status = if decided {
        status_for(result)
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(result)).into_response()
}

fn status_for(result: &RunbookResult) -> StatusCode {
    match result {
        RunbookResult::Started { .. } | RunbookResult::Failed { .. } => StatusCode::OK,
        RunbookResult::Refused { reason, .. } => match reason {
            RunbookRefusal::InvalidRequest => StatusCode::BAD_REQUEST,
            RunbookRefusal::UnknownRunbook => StatusCode::NOT_FOUND,
            RunbookRefusal::CarForbidden => StatusCode::FORBIDDEN,
            RunbookRefusal::Busy => StatusCode::CONFLICT,
            RunbookRefusal::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            RunbookRefusal::IdempotencyConflict | RunbookRefusal::Other => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
        },
        RunbookResult::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use herdr::FakeHerdr;
    use protocol::Timestamp;

    use super::*;
    use crate::control::HerdrControl;

    fn hello_run(started_at: u64, state: RunState) -> RunbookRun {
        RunbookRun {
            runbook: RunbookName::new("Hello").expect("name"),
            state,
            exit_code: None,
            last_lines: Vec::new(),
            message: None,
            started_at: Timestamp::from_unix_millis(started_at),
            finished_at: None,
        }
    }

    #[tokio::test]
    async fn a_late_end_never_hides_the_next_run() {
        let tmp = tempfile::tempdir().expect("tmp");
        let control: Arc<dyn SpawnControl> = Arc::new(HerdrControl(Arc::new(FakeHerdr::new())));
        let hub = Arc::new(Hub::new(16));
        let runbooks = Runbooks::new(
            RunbookSettings::new(Vec::new(), tmp.path()),
            RunbookDeps {
                tabs: Arc::new(Tabs::new(Arc::clone(&control), "spawn")),
                control,
                hub: Arc::clone(&hub),
                audit: Arc::new(AuditLog::new(tmp.path().join("audit.jsonl"))),
            },
        );
        let name = RunbookName::new("Hello").expect("name");
        let mut feed = hub.subscribe();
        assert!(matches!(feed.next().await, AgentEvent::Snapshot { .. }));
        // The next run starts before the earlier run's watcher publishes
        // that run's end; the clock stepped back in between.
        runbooks.record(&hello_run(5, RunState::Running));
        let next = hello_run(2, RunState::Running);
        runbooks.record(&next);
        runbooks.record_end(&hello_run(5, RunState::Exited));
        assert_eq!(runbooks.latest(&name), Some(next));
        // The next run's own end is recorded.
        let ended = hello_run(2, RunState::Exited);
        runbooks.record_end(&ended);
        assert_eq!(runbooks.latest(&name), Some(ended));
        let mut published = Vec::new();
        for _ in 0..3 {
            match feed.next().await {
                AgentEvent::RunbookUpdated { run: Some(run) } => {
                    published.push((run.started_at.unix_millis(), run.state));
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            published,
            [
                (5, RunState::Running),
                (2, RunState::Running),
                (2, RunState::Exited)
            ],
            "the stale end is not published"
        );
    }

    #[test]
    fn the_launch_line_holds_only_shell_quoted_paths() {
        let line = launch_line(
            Path::new("/home/me/.local/bin/passporter-bridge"),
            Path::new("/state/runbooks/k.spec.json"),
        );
        assert_eq!(
            line,
            " /home/me/.local/bin/passporter-bridge runbook /state/runbooks/k.spec.json"
        );
        let odd = launch_line(Path::new("/it's/$(x)"), Path::new("/s"));
        assert_eq!(odd, r" '/it'\''s/$(x)' runbook /s");
    }

    #[test]
    fn replays_say_so_and_rate_limits_are_transient() {
        let run = hello_run(3, RunState::Running);
        let replay = RunRequest::replayed(RunbookResult::Started {
            run,
            replayed: false,
        });
        assert!(matches!(
            replay,
            RunbookResult::Started { replayed: true, .. }
        ));
        assert!(RunRequest::is_transient(&refused(
            RunbookRefusal::RateLimited,
            ""
        )));
        assert!(!RunRequest::is_transient(&refused(
            RunbookRefusal::Busy,
            ""
        )));
    }
}
