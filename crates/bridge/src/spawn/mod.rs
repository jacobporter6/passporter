//! Starting a new agent from the phone: `GET /v1/projects` and
//! `POST /v1/agents` (protocol `spawn`).
//!
//! The phone names a project from the `[spawn]` allowlist and an agent
//! kind; everything herdr is asked to do comes from the config. Order:
//! parse (400 `invalid_request`) → off without `[spawn]` (404 `disabled`)
//! → idempotency ledger (replay, in flight or `idempotency_conflict`) →
//! per-device rate limit for a fresh key (429 `rate_limited`, key
//! released) → project and kind against the config (422) → policy: a first
//! prompt starting with `!`, `/` or `#` needs step-up, which the MVP does
//! not have (403 `step_up_required`) → the project folder must exist →
//! audit `execute` line → herdr → audit `result` line.
//!
//! In herdr: a tab in the `[spawn] workspace` (made on first use, unfocused)
//! with its shell in the project folder; once that shell is at its prompt,
//! `agent.start` with the name `pp-<key>`, the kind and **no
//! native arguments**, so Claude starts in its normal permission mode and
//! no client input ever reaches a command line. Then wait (boundedly) for
//! the agent to settle (idle, done or blocked), so its id carries Claude's
//! session, and with a first prompt send it with `agent.prompt`; a
//! prompt that cannot be sent (the agent asks something first, or is not
//! ready in time) is reported in the result, never retried blindly. Last,
//! wait briefly for the registry to list the agent, so the phone can open
//! it straight away, and answer with its id.
//!
//! The agent name comes from the idempotency key, so a retry the ledger
//! has forgotten (a bridge restart) finds herdr's `agent_name_taken`,
//! looks the agent up and replays instead of starting a second one. A tab
//! the spawn made and could not use is closed again. The work after the
//! ledger runs in its own task, so a client hanging up cannot leave a
//! spawn half-recorded. The handler answers within
//! [`SpawnSettings::answer_within`]: with the result, or, for a spawn still
//! under way, a transient "still starting" that the client retries with
//! the same key; the route's own timeout lies beyond that.

mod audit;
pub mod control;
pub(crate) mod tabs;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use herdr::{AgentName, ErrorCode, PaneId, ShellWait, StartAgent};
use protocol::{AgentId, SpawnOptions, SpawnRefusal, SpawnRequest, SpawnResult};

use self::audit::SpawnEntry;
pub use self::control::SpawnControl;
use self::tabs::Tabs;
use crate::actions::Budget;
use crate::actions::audit::{AuditLog, Phase};
use crate::actions::ledger::{Claim, Idempotent, Ledger};
use crate::actions::retry_after_secs;
use crate::auth::{RateLimit, RateLimiter};
use crate::config::{SpawnConfig, SpawnProject};
use crate::server::{AppState, ClientAddr, Principal};

/// Longest parse error echoed back to the client.
const MAX_ERROR_CHARS: usize = 200;

/// Prefix of every agent name the bridge gives herdr.
const NAME_PREFIX: &str = "pp";

/// How many hex digits of the idempotency key go into the agent name:
/// all herdr's 32-character limit leaves after the prefix (116 random bits
/// of a v4 key), so two spawns never share a name.
const NAME_KEY_CHARS: usize = AgentName::MAX_LEN - NAME_PREFIX.len() - 1;

/// Settings for starting agents.
#[derive(Debug, Clone)]
pub struct SpawnSettings {
    /// The `[spawn]` table; `None` turns spawning off.
    pub config: Option<SpawnConfig>,
    /// Most finished results remembered for idempotent replay.
    pub idempotency_capacity: usize,
    /// Spawns each device may ask for (design §8: per-device rate limits).
    pub rate: RateLimit,
    /// How long a new tab's shell may take to reach its prompt.
    pub shell: ShellWait,
    /// herdr's startup timeout for the agent (`agent.start`).
    pub start_timeout: Duration,
    /// How long the new agent may take to settle before the first prompt.
    pub ready_wait: Duration,
    /// How long to wait for the registry to list the new agent.
    pub listed_wait: Duration,
    /// How often to look again while waiting (not-ready prompts, listing).
    pub poll: Duration,
    /// How long `POST /v1/agents` waits for the spawn before answering
    /// "still starting" (transient: the client asks again with the same
    /// key, and gets the stored result once there is one). [`Self::new`]
    /// makes it every configured wait plus slack, so a typical spawn
    /// answers with its result; a slow herdr (its per-call timeouts come
    /// on top) only costs one more round trip, never a second agent.
    pub answer_within: Duration,
}

impl SpawnSettings {
    /// Defaults: remember 256 results; 6 spawns per 10 minutes per device
    /// (far above what a person starts, far below a runaway client); a
    /// 30 s shell wait, herdr's 30 s startup timeout, 60 s for the agent to
    /// settle and 5 s for the registry.
    #[must_use]
    pub fn new(config: Option<SpawnConfig>) -> Self {
        let shell = ShellWait::default();
        let start_timeout = Duration::from_secs(30);
        let ready_wait = Duration::from_secs(60);
        let listed_wait = Duration::from_secs(5);
        Self {
            config,
            idempotency_capacity: 256,
            rate: RateLimit {
                requests: 6,
                window: Duration::from_secs(600),
            },
            shell,
            start_timeout,
            ready_wait,
            listed_wait,
            poll: Duration::from_millis(250),
            answer_within: shell.timeout
                + start_timeout
                + ready_wait
                + listed_wait
                + Duration::from_secs(10),
        }
    }

    /// The route's request timeout: comfortably past
    /// [`answer_within`](Self::answer_within), so the handler always
    /// answers first and the client never sees a bare 408.
    #[must_use]
    pub fn route_timeout(&self) -> Duration {
        self.answer_within + Duration::from_secs(10)
    }
}

impl Idempotent for SpawnRequest {
    type Outcome = SpawnResult;

    fn idempotency_key(&self) -> &protocol::IdempotencyKey {
        &self.idempotency_key
    }

    fn replayed(outcome: SpawnResult) -> SpawnResult {
        match outcome {
            SpawnResult::Spawned {
                agent_id,
                prompt_error,
                ..
            } => SpawnResult::Spawned {
                agent_id,
                replayed: true,
                prompt_error,
            },
            other => other,
        }
    }

    fn is_transient(outcome: &SpawnResult) -> bool {
        matches!(outcome, SpawnResult::Refused { reason, .. } if reason.is_transient())
    }
}

/// The ledger, limiter and herdr seam behind the spawn routes.
pub struct Spawner {
    settings: SpawnSettings,
    control: Arc<dyn SpawnControl>,
    ledger: Arc<Ledger<SpawnRequest>>,
    limit: RateLimiter<Budget>,
    audit: Arc<AuditLog>,
    /// Tabs in the spawn workspace, shared with runbook runs.
    tabs: Arc<Tabs>,
}

impl std::fmt::Debug for Spawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spawner")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Spawner {
    /// A spawner writing to `audit` (the actions' log, see
    /// [`crate::actions::Actions`]), opening tabs with `tabs` (whose
    /// workspace is the `[spawn]` one).
    pub(crate) fn new(
        settings: SpawnSettings,
        control: Arc<dyn SpawnControl>,
        audit: Arc<AuditLog>,
        tabs: Arc<Tabs>,
    ) -> Self {
        Self {
            ledger: Ledger::new(settings.idempotency_capacity),
            limit: RateLimiter::new(settings.rate),
            settings,
            control,
            audit,
            tabs,
        }
    }

    /// The settings it runs with.
    #[must_use]
    pub fn settings(&self) -> &SpawnSettings {
        &self.settings
    }

    fn options(&self) -> SpawnOptions {
        self.settings
            .config
            .as_ref()
            .map_or_else(SpawnOptions::default, |config| SpawnOptions {
                projects: config.projects.iter().map(|p| p.name.clone()).collect(),
                kinds: config.kinds.clone(),
            })
    }
}

/// `GET /v1/projects`: the projects and kinds this device may start; both
/// empty when spawning is off.
pub(crate) async fn options(State(state): State<AppState>) -> Json<SpawnOptions> {
    Json(state.spawner.options())
}

/// An outcome plus whether it is final for the key.
struct Outcome {
    result: SpawnResult,
    /// False when nothing happened and nothing was decided (herdr or the
    /// audit log unavailable first): 503, the same key may be retried.
    decided: bool,
    /// For a rate-limited request: when to try again (`Retry-After`).
    retry_after: Option<Duration>,
}

impl Outcome {
    fn decided(result: SpawnResult) -> Self {
        Self {
            result,
            decided: true,
            retry_after: None,
        }
    }

    fn undecided(message: &str) -> Self {
        Self {
            result: SpawnResult::Failed {
                message: message.to_owned(),
            },
            decided: false,
            retry_after: None,
        }
    }

    fn rate_limited(message: &str, wait: Duration) -> Self {
        Self {
            result: refused(SpawnRefusal::RateLimited, message),
            decided: true,
            retry_after: Some(wait),
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

/// How long the client is told to wait before asking again about a spawn
/// still under way.
const STILL_STARTING_RETRY: Duration = Duration::from_secs(3);

/// `POST /v1/agents`.
pub(crate) async fn post(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(client): Extension<ClientAddr>,
    body: Bytes,
) -> Response {
    let request: SpawnRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            let message: String = error.to_string().chars().take(MAX_ERROR_CHARS).collect();
            return respond(&refused(SpawnRefusal::InvalidRequest, message), true);
        }
    };
    let Some(config) = state.spawner.settings.config.clone() else {
        let result = refused(
            SpawnRefusal::Disabled,
            "Starting agents is off: the bridge config has no [spawn] table.",
        );
        return respond(&result, true);
    };
    let guard = match state.spawner.ledger.claim(&request) {
        Claim::Fresh(guard) => guard,
        Claim::Replay(result) => return respond(&result, true),
        Claim::Conflict => {
            let result = refused(
                SpawnRefusal::IdempotencyConflict,
                "This idempotency key was already used for a different request.",
            );
            return respond(&result, true);
        }
        Claim::InFlight => return still_starting(),
    };
    let budget = Budget::of(&principal, client);
    let answer_within = state.spawner.settings.answer_within;
    let mut work = tokio::spawn(async move {
        let outcome = decide_and_spawn(&state, &principal.device, &budget, &config, &request).await;
        audit_result(&state.spawner, &principal.device, &request, &outcome.result).await;
        guard.finish(&outcome.result, outcome.decided);
        outcome
    });
    // Always answer before the route's timeout: a spawn still under way
    // keeps its key in flight, and the client asks again with it until
    // the stored result replays (a 408 would leave it guessing).
    match tokio::time::timeout(answer_within, &mut work).await {
        Ok(Ok(outcome)) => outcome.respond(),
        Ok(Err(error)) => {
            tracing::error!(%error, "spawn task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(_) => {
            tracing::info!("spawn still under way; the client will ask again");
            still_starting()
        }
    }
}

/// The answer for a key whose spawn is still under way: transient, so the
/// client retries the same key and gets the result once it is stored.
fn still_starting() -> Response {
    Outcome::rate_limited(
        "This agent is still starting; retry shortly.",
        STILL_STARTING_RETRY,
    )
    .respond()
}

/// Checks `request` against the config and policy, spends the device's
/// rate-limit budget only for a request that passes (so refusals never
/// lock out good spawns), then audits and spawns.
async fn decide_and_spawn(
    state: &AppState,
    device: &str,
    budget: &Budget,
    config: &SpawnConfig,
    request: &SpawnRequest,
) -> Outcome {
    let spawner = &state.spawner;
    let Some(project) = config.projects.iter().find(|p| p.name == request.project) else {
        return Outcome::decided(refused(
            SpawnRefusal::UnknownProject,
            "The bridge has no project by that name.",
        ));
    };
    if !config.kinds.contains(&request.kind) {
        return Outcome::decided(refused(
            SpawnRefusal::UnknownKind,
            format!("The bridge does not start {} agents.", request.kind.label()),
        ));
    }
    if request
        .prompt
        .as_ref()
        .is_some_and(protocol::PromptText::is_elevated)
    {
        return Outcome::decided(refused(
            SpawnRefusal::StepUpRequired,
            "First prompts starting with !, / or # need confirmation, which this version \
             can't do yet; send it once the agent is open.",
        ));
    }
    if !tokio::fs::metadata(&project.path)
        .await
        .is_ok_and(|meta| meta.is_dir())
    {
        tracing::warn!(project = %project.name, path = %project.path.display(), "spawn project folder missing");
        return Outcome::decided(SpawnResult::Failed {
            message: format!("The folder for {} is missing on the bridge.", project.name),
        });
    }
    if let Err(wait) = spawner.limit.check(budget) {
        // Transient: the ledger releases the key for a later retry.
        return Outcome::rate_limited(
            "Too many new agents from this device; retry in a few minutes.",
            wait,
        );
    }
    if let Err(error) = audit_execute(spawner, device, request).await {
        tracing::error!(%error, "cannot write the audit log; refusing to spawn");
        return Outcome::undecided("The audit log is unavailable; nothing was started.");
    }
    spawn_agent(state, project, request).await
}

/// herdr's part: tab, agent, first prompt, listing.
async fn spawn_agent(state: &AppState, project: &SpawnProject, request: &SpawnRequest) -> Outcome {
    let spawner = &state.spawner;
    let name = match agent_name(&request.idempotency_key) {
        Ok(name) => name,
        Err(error) => {
            tracing::error!(%error, "cannot name a new agent");
            return Outcome::decided(SpawnResult::Failed {
                message: "The bridge could not name the agent; nothing was started.".to_owned(),
            });
        }
    };
    let (tab, pane) = match spawner
        .tabs
        .open(project.name.as_str(), &project.path)
        .await
    {
        Ok(opened) => opened,
        Err(error) if !error.may_have_taken_effect() => {
            tracing::warn!(%error, "herdr could not open a tab for a new agent");
            return Outcome::undecided("herdr is unavailable; nothing was started.");
        }
        Err(error) => {
            // herdr may have made the tab without saying so, and it has no
            // tab listing to find it by: say so rather than guess.
            tracing::warn!(%error, "opening a tab for a new agent failed");
            return Outcome::decided(SpawnResult::Failed {
                message: format!(
                    "herdr: {error}. No agent was started, but herdr may have opened an empty \
                     {} tab in the {} workspace; close it if so.",
                    project.name,
                    spawner.tabs.workspace()
                ),
            });
        }
    };
    let spec = StartAgent {
        name: name.clone(),
        kind: request.kind.as_str().to_owned(),
        pane,
        // Fixed: no native arguments, ever. Claude starts in its normal
        // permission mode, and nothing from the client reaches a command.
        args: Vec::new(),
        timeout: Some(spawner.settings.start_timeout),
    };
    let started = match spawner.control.start(&spec, spawner.settings.shell).await {
        Ok(agent) => agent,
        Err(error) if error.code() == Some(&ErrorCode::AgentNameTaken) => {
            // An earlier try of this same key started it (the ledger forgot
            // across a restart): open that one, start nothing new.
            spawner.tabs.close(&tab).await;
            return replay_started(state, &name, project, request).await;
        }
        Err(error) if error.may_have_taken_effect() => {
            match spawner.control.agent_named(&name).await {
                Ok(agent) => agent,
                Err(lookup) => {
                    tracing::warn!(%error, %lookup, "agent start was ambiguous and the agent is not there");
                    if lookup.code().is_some_and(ErrorCode::is_not_found) {
                        spawner.tabs.close(&tab).await;
                    }
                    return Outcome::decided(SpawnResult::Failed {
                        message: format!("herdr: {error}"),
                    });
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "agent start failed");
            spawner.tabs.close(&tab).await;
            return Outcome::decided(SpawnResult::Failed {
                message: format!("herdr: {error}"),
            });
        }
    };
    tracing::info!(agent = %name, pane = %started.pane_id, project = %project.name, kind = request.kind.as_str(), "agent started from the phone");
    // Settled, a Claude agent has its session, which its id carries: the
    // id the phone opens is the one its list keeps. One deadline covers
    // settling and the first prompt, so the route's budget holds.
    let ready_by = tokio::time::Instant::now() + spawner.settings.ready_wait;
    let settled = spawner
        .control
        .wait_settled(&started.pane_id, spawner.settings.ready_wait)
        .await;
    let prompt_error = match &request.prompt {
        Some(prompt) => first_prompt(state, &started.pane_id, prompt.as_str(), settled, ready_by)
            .await
            .err(),
        None => None,
    };
    let agent = state
        .control
        .agent(&started.pane_id)
        .await
        .unwrap_or(started);
    spawned(state, &agent, false, prompt_error).await
}

/// The `spawned` result for `agent`, by the id the phone's list shows.
async fn spawned(
    state: &AppState,
    agent: &herdr::Agent,
    replayed: bool,
    prompt_error: Option<String>,
) -> Outcome {
    match listed_id(state, agent).await {
        Some(agent_id) => Outcome::decided(SpawnResult::Spawned {
            agent_id,
            replayed,
            prompt_error,
        }),
        None => Outcome::decided(SpawnResult::Failed {
            message: format!(
                "The agent started in herdr pane {}, which the app cannot open.",
                agent.pane_id
            ),
        }),
    }
}

/// `pp-<the key's last 29 hex digits>`: unique per spawn request, and the
/// same for every retry of it (the tab carries the project's name). Always
/// a valid herdr name: the key is lowercase hex.
fn agent_name(key: &protocol::IdempotencyKey) -> Result<AgentName, herdr::InvalidAgentName> {
    let hex: Vec<char> = key.as_str().chars().filter(|c| *c != '-').collect();
    let tail: String = hex[hex.len().saturating_sub(NAME_KEY_CHARS)..]
        .iter()
        .collect();
    AgentName::new(format!("{NAME_PREFIX}-{tail}"))
}

/// Whether some agent started from the phone could have a name starting
/// with `prefix`: every such name is `pp-` and lowercase hex
/// ([`agent_name`]). The push config refuses quiet name prefixes that do.
pub(crate) fn may_name(prefix: &str) -> bool {
    let fixed = format!("{NAME_PREFIX}-");
    match prefix.strip_prefix(fixed.as_str()) {
        Some(key) => {
            key.len() <= NAME_KEY_CHARS
                && key
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        }
        None => fixed.starts_with(prefix),
    }
}

/// Sends the first prompt to the agent in `pane`, given how waiting for
/// it to settle went, retrying a not-yet-ready agent until `deadline`.
/// `Err` says, for the user, why it was not (or may not have been) sent.
async fn first_prompt(
    state: &AppState,
    pane: &PaneId,
    text: &str,
    settled: herdr::Result<herdr::Agent>,
    deadline: tokio::time::Instant,
) -> Result<(), String> {
    let settings = &state.spawner.settings;
    let asked = || {
        "The agent asked something before it was ready for your prompt: answer it, then send the prompt.".to_owned()
    };
    let not_ready = || {
        "The agent was not ready for your prompt in time; send it from the agent screen.".to_owned()
    };
    match settled {
        Ok(agent) if agent.agent_status == herdr::AgentStatus::Blocked => return Err(asked()),
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "waiting for a new agent to settle failed");
            return Err(not_ready());
        }
    }
    loop {
        match state.control.prompt(pane, text).await {
            Ok(_) => return Ok(()),
            Err(error) if error.code() == Some(&ErrorCode::AgentBlocked) => return Err(asked()),
            Err(error) if error.may_have_taken_effect() => {
                tracing::warn!(%error, "first prompt delivery is uncertain");
                return Err(format!(
                    "herdr could not confirm your prompt arrived ({error}); check the agent \
                     before sending it again."
                ));
            }
            Err(error)
                if matches!(
                    error.code(),
                    Some(ErrorCode::AgentNotReady | ErrorCode::AgentLaunchPending)
                ) && tokio::time::Instant::now() + settings.poll < deadline =>
            {
                tokio::time::sleep(settings.poll).await;
            }
            Err(error) => {
                tracing::warn!(%error, "first prompt failed");
                return Err(not_ready());
            }
        }
    }
}

/// The answer for a key whose agent an earlier try already started (its
/// name is taken in herdr, and the ledger forgot the result): that agent,
/// replayed, if it fits the request; else an idempotency conflict.
async fn replay_started(
    state: &AppState,
    name: &AgentName,
    project: &SpawnProject,
    request: &SpawnRequest,
) -> Outcome {
    match state.spawner.control.agent_named(name).await {
        Ok(agent) if !started_for(&agent, project, request.kind) => Outcome::decided(refused(
            SpawnRefusal::IdempotencyConflict,
            "This idempotency key already started an agent for a different request.",
        )),
        Ok(agent) => {
            // Whether that try's first prompt went out died with the
            // ledger: never claim it did, never resend it.
            let prompt_error = request.prompt.as_ref().map(|_| {
                "An earlier try of this request started the agent; whether its first prompt \
                 was sent is unknown, so check the agent before sending it again."
                    .to_owned()
            });
            spawned(state, &agent, true, prompt_error).await
        }
        Err(error) => {
            tracing::warn!(%error, "looking up an already started agent failed");
            Outcome::decided(SpawnResult::Failed {
                message: format!("herdr: {error}"),
            })
        }
    }
}

/// Whether `agent`, found under the name an earlier try of this key gave,
/// fits this request: the same kind and a pane in the project's folder.
/// (The first prompt cannot be compared: herdr does not keep it.)
fn started_for(agent: &herdr::Agent, project: &SpawnProject, kind: protocol::SpawnKind) -> bool {
    let kind_fits = agent.kind.as_deref().is_none_or(|k| k == kind.as_str());
    let place_fits = agent
        .cwd
        .as_ref()
        .or(agent.foreground_cwd.as_ref())
        .is_none_or(|cwd| cwd.starts_with(&project.path));
    kind_fits && place_fits
}

/// The new agent's id as the phone's list will know it. Waits (briefly)
/// for the registry to list the pane under the id herdr's answer gives
/// (with Claude's session, once it has one); at the deadline, the id the
/// registry lists the pane under, if any, else the derived one. `None`
/// for a pane id outside the protocol's charset.
async fn listed_id(state: &AppState, agent: &herdr::Agent) -> Option<AgentId> {
    let settings = &state.spawner.settings;
    let derived = crate::registry::agent_id(agent).ok()?;
    let deadline = tokio::time::Instant::now() + settings.listed_wait;
    loop {
        let listed = state.hub.agent_in(derived.pane()).map(|listed| listed.id);
        if listed.as_ref() == Some(&derived) {
            return Some(derived);
        }
        if tokio::time::Instant::now() >= deadline {
            return Some(listed.unwrap_or(derived));
        }
        tokio::time::sleep(settings.poll.min(Duration::from_millis(50))).await;
    }
}

fn refused(reason: SpawnRefusal, message: impl Into<String>) -> SpawnResult {
    SpawnResult::Refused {
        reason,
        message: message.into(),
    }
}

async fn audit_execute(
    spawner: &Spawner,
    device: &str,
    request: &SpawnRequest,
) -> std::io::Result<()> {
    let audit = Arc::clone(&spawner.audit);
    let device = device.to_owned();
    let request = request.clone();
    tokio::task::spawn_blocking(move || {
        audit.append(&SpawnEntry::new(Phase::Execute, &device, &request, None))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Best effort: the spawn already happened (or was refused).
async fn audit_result(
    spawner: &Spawner,
    device: &str,
    request: &SpawnRequest,
    result: &SpawnResult,
) {
    let audit = Arc::clone(&spawner.audit);
    let device = device.to_owned();
    let request = request.clone();
    let result = result.clone();
    let written = tokio::task::spawn_blocking(move || {
        audit.append(&SpawnEntry::new(
            Phase::Result,
            &device,
            &request,
            Some(&result),
        ))
    })
    .await
    .map_err(std::io::Error::other)
    .and_then(|written| written);
    if let Err(error) = written {
        tracing::error!(%error, "cannot write a spawn audit result line");
    }
}

fn respond(result: &SpawnResult, decided: bool) -> Response {
    let status = if decided {
        status_for(result)
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(result)).into_response()
}

fn status_for(result: &SpawnResult) -> StatusCode {
    match result {
        SpawnResult::Spawned { .. } | SpawnResult::Failed { .. } => StatusCode::OK,
        SpawnResult::Refused { reason, .. } => match reason {
            SpawnRefusal::InvalidRequest => StatusCode::BAD_REQUEST,
            SpawnRefusal::Disabled => StatusCode::NOT_FOUND,
            SpawnRefusal::StepUpRequired => StatusCode::FORBIDDEN,
            SpawnRefusal::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        },
        SpawnResult::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use protocol::IdempotencyKey;

    use super::*;

    #[test]
    fn may_name_matches_exactly_the_prefixes_of_phone_agent_names() {
        let key = IdempotencyKey::new("3f2b8c1e-9d4a-4b7f-8e6c-5a1d2f3e4b5c").expect("key");
        let name = agent_name(&key).expect("name");
        for end in 0..=name.as_str().len() {
            assert!(may_name(&name.as_str()[..end]), "{}", &name.as_str()[..end]);
        }
        for other in [
            "impl-",
            "rev-",
            "ppx",
            "pp-z",
            "pp-A",
            "p-",
            &format!("{name}0"),
        ] {
            assert!(!may_name(other), "{other}");
        }
    }

    #[test]
    fn agent_names_are_valid_and_stable_per_key() {
        let key = IdempotencyKey::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").expect("key");
        let name = agent_name(&key).expect("valid");
        assert_eq!(name.as_str(), "pp-09978ef8946fd9a85118b84bee7b4");
        assert_eq!(name.as_str().len(), AgentName::MAX_LEN);
        assert_eq!(agent_name(&key), Ok(name), "the same for a retry");
        let other = IdempotencyKey::new("9ca09978-ef89-46fd-9a85-118b84bee7b5").expect("key");
        assert_ne!(agent_name(&other), agent_name(&key));
    }

    #[test]
    fn the_handler_answers_before_the_route_times_out() {
        let settings = SpawnSettings::new(None);
        assert!(
            settings.answer_within
                > settings.shell.timeout
                    + settings.start_timeout
                    + settings.ready_wait
                    + settings.listed_wait
        );
        assert!(settings.route_timeout() > settings.answer_within);
    }

    #[test]
    fn replays_keep_the_agent_and_say_so() {
        let spawned = SpawnResult::Spawned {
            agent_id: "w1:p2".parse().expect("id"),
            replayed: false,
            prompt_error: None,
        };
        assert!(matches!(
            SpawnRequest::replayed(spawned),
            SpawnResult::Spawned { replayed: true, .. }
        ));
        let limited = refused(SpawnRefusal::RateLimited, "");
        assert!(SpawnRequest::is_transient(&limited));
        assert!(!SpawnRequest::is_transient(&refused(
            SpawnRefusal::UnknownProject,
            ""
        )));
    }
}
