//! Terminal mode: commands an agent asks the user to run
//! (`GET /v1/agents/:id/commands`, `POST /v1/agents/:id/commands/run`),
//! and the terminals those runs open (`GET /v1/terminals/:pane/screen`,
//! `POST /v1/terminals/:pane/keys`). Protocol `command`.
//!
//! The candidates come from the agent's latest message, extracted by fixed
//! rules ([`extract`]); every one is listed, so decoys ("run this, or
//! this") are all visible and the user picks. They are agent-suggested
//! commands that run as the user: the phone's confirm, showing the exact
//! text, is the control (docs/security.md).
//!
//! A run never carries command text. Order: parse (400
//! `invalid_request`) → idempotency ledger (replay, in flight or
//! `idempotency_conflict`) → origin: never the car (403 `car_forbidden`) →
//! the agent now (404 `not_found`; another session in its pane is 409
//! `stale`) → extract its latest message again and find the candidate by
//! id **and** hash (409 `stale`: the message changed, or the pair never
//! matched) → where: in the session only a single-line command (422
//! `not_here`) and only while the agent is at its prompt (409
//! `agent_busy`); a new terminal needs the agent's folder (404) → per-device
//! rate limit (429 `rate_limited`, key released) → audit `execute` line
//! with the exact command → read and extract once more, same seq, same
//! candidate (409 `stale`) → herdr → audit `result` line.
//!
//! - **Here**: `agent.prompt` with `!<command>`. Claude Code and Codex
//!   both run a `!` line as a shell command in their own session, so the
//!   output lands in the agent's context.
//! - **New terminal**: a tab labelled `run: <agent>` in the spawn workspace
//!   (shared with [`crate::spawn`] and [`crate::runbooks`]), its shell in
//!   the agent's working directory; once that shell settles, the command
//!   is typed into it with Enter. The answer names the pane, which the
//!   phone may then watch (`GET /v1/terminals/:pane/screen`) and send
//!   allowlisted keys to (`POST /v1/terminals/:pane/keys`: audited, rate
//!   limited, never from the car). Only terminals opened this way, since
//!   the bridge started, by the same device, and only while the pane still
//!   runs the terminal herdr created for it (its terminal id, checked with
//!   `pane.get` on every read and key send), can be used; any other pane
//!   is 404.
//!
//! The work after the ledger runs in its own task, so a client hanging up
//! cannot leave a run half-recorded.

mod audit;
pub(crate) mod extract;

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use herdr::{PaneId, TerminalId};
use protocol::{
    AgentId, CommandCandidate, CommandHash, CommandList, CommandRefusal, CommandResult,
    CommandRunRequest, IdempotencyKey, MAX_SCREEN_LINES, Origin, RunTarget, Terminal,
    TerminalKeysRequest, TerminalScreen,
};
use sha2::{Digest, Sha256};

use self::audit::{CommandEntry, KeysEntry};
use crate::actions::Budget;
use crate::actions::audit::{AuditLog, Phase};
use crate::actions::ledger::{Claim, Idempotent, Ledger};
use crate::actions::retry_after_secs;
use crate::auth::{RateLimit, RateLimiter};
use crate::control::{Resolved, resolve};
use crate::server::{AppState, ClientAddr, Principal, current_agent, screen_tail};
use crate::spawn::SpawnControl;
use crate::spawn::tabs::{Tabs, wait_for_shell};
use crate::threads::{ThreadSource, Threads};

/// Longest parse error echoed back to the client.
const MAX_ERROR_CHARS: usize = 200;

/// Longest agent name in a terminal's tab label, in characters.
const MAX_LABEL_NAME_CHARS: usize = 32;

/// Settings for terminal mode.
#[derive(Debug, Clone)]
pub struct CommandSettings {
    /// Most finished results remembered for idempotent replay (runs and
    /// keys each).
    pub idempotency_capacity: usize,
    /// Command runs each device may ask for (design §8: per-device rate
    /// limits).
    pub rate: RateLimit,
    /// Key sends each device may make to the terminals.
    pub keys_rate: RateLimit,
    /// How long a fresh tab's shell may take to settle before the command
    /// is typed anyway (it is typed once, never again).
    pub shell_wait: Duration,
    /// Most terminals remembered for watching; the oldest is forgotten
    /// first.
    pub max_terminals: usize,
}

impl Default for CommandSettings {
    /// Remember 256 results; 20 runs per 10 minutes and 60 key sends per
    /// minute per device; up to 5 s for a fresh shell; 32 terminals.
    fn default() -> Self {
        Self {
            idempotency_capacity: 256,
            rate: RateLimit {
                requests: 20,
                window: Duration::from_secs(600),
            },
            keys_rate: RateLimit::per_minute(60),
            shell_wait: Duration::from_secs(5),
            max_terminals: 32,
        }
    }
}

/// A run request as the ledger keys it: the agent from the path and the
/// body, so one key used for two agents is a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunRequest {
    agent: AgentId,
    body: CommandRunRequest,
}

impl Idempotent for RunRequest {
    type Outcome = CommandResult;

    fn idempotency_key(&self) -> &IdempotencyKey {
        &self.body.idempotency_key
    }

    fn replayed(outcome: CommandResult) -> CommandResult {
        replayed(outcome)
    }

    fn is_transient(outcome: &CommandResult) -> bool {
        is_transient(outcome)
    }
}

/// A keys request as the ledger keys it, with the pane from the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeysRequest {
    pane: PaneId,
    body: TerminalKeysRequest,
}

impl Idempotent for KeysRequest {
    type Outcome = CommandResult;

    fn idempotency_key(&self) -> &IdempotencyKey {
        &self.body.idempotency_key
    }

    fn replayed(outcome: CommandResult) -> CommandResult {
        replayed(outcome)
    }

    fn is_transient(outcome: &CommandResult) -> bool {
        is_transient(outcome)
    }
}

fn replayed(outcome: CommandResult) -> CommandResult {
    match outcome {
        CommandResult::Sent { .. } => CommandResult::Sent { replayed: true },
        CommandResult::Started { terminal, .. } => CommandResult::Started {
            terminal,
            replayed: true,
        },
        other => other,
    }
}

fn is_transient(outcome: &CommandResult) -> bool {
    matches!(outcome, CommandResult::Refused { reason, .. } if reason.is_transient())
}

/// The ledgers, limiters, terminals and herdr seam behind the routes.
pub struct Commands {
    settings: CommandSettings,
    control: Arc<dyn SpawnControl>,
    tabs: Arc<Tabs>,
    audit: Arc<AuditLog>,
    runs: Arc<Ledger<RunRequest>>,
    keys: Arc<Ledger<KeysRequest>>,
    run_limit: RateLimiter<Budget>,
    keys_limit: RateLimiter<Budget>,
    /// Terminals opened by runs, newest last.
    terminals: Mutex<VecDeque<Opened>>,
}

/// A terminal a run opened: for which device, and which herdr terminal
/// its pane ran then.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Opened {
    device: String,
    pane: PaneId,
    terminal: TerminalId,
}

/// Whether a pane is a terminal the asking device may use now.
#[derive(Debug)]
enum Live {
    /// It is: the device's run opened it and it still runs that terminal.
    Yes,
    /// It is not (another device's, never opened, closed, or its id now
    /// names another terminal).
    No,
    /// herdr did not say.
    Unknown(herdr::Error),
}

impl std::fmt::Debug for Commands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Commands")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// The services [`Commands`] works with.
pub(crate) struct CommandDeps {
    /// herdr, for tabs and terminals.
    pub(crate) control: Arc<dyn SpawnControl>,
    /// Tabs in the spawn workspace.
    pub(crate) tabs: Arc<Tabs>,
    /// The actions' audit log.
    pub(crate) audit: Arc<AuditLog>,
}

impl Commands {
    pub(crate) fn new(settings: CommandSettings, deps: CommandDeps) -> Self {
        Self {
            runs: Ledger::new(settings.idempotency_capacity),
            keys: Ledger::new(settings.idempotency_capacity),
            run_limit: RateLimiter::new(settings.rate),
            keys_limit: RateLimiter::new(settings.keys_rate),
            settings,
            control: deps.control,
            tabs: deps.tabs,
            audit: deps.audit,
            terminals: Mutex::new(VecDeque::new()),
        }
    }

    fn terminals(&self) -> MutexGuard<'_, VecDeque<Opened>> {
        self.terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn remember(&self, opened: Opened) {
        let mut terminals = self.terminals();
        terminals.retain(|known| known.pane != opened.pane);
        terminals.push_back(opened);
        while terminals.len() > self.settings.max_terminals.max(1) {
            terminals.pop_front();
        }
    }

    /// Whether `pane` is a terminal `device`'s run opened that still runs
    /// the terminal it was opened with (`pane.get`): a pane id herdr has
    /// handed to another terminal is not it, and is forgotten.
    async fn live(&self, device: &str, pane: &PaneId) -> Live {
        let known = self
            .terminals()
            .iter()
            .find(|known| known.device == device && known.pane == *pane)
            .map(|known| known.terminal.clone());
        let Some(terminal) = known else {
            return Live::No;
        };
        let same = match self.control.pane(pane).await {
            Ok(now) => now.terminal_id == terminal,
            Err(error) if error.code().is_some_and(herdr::ErrorCode::is_not_found) => false,
            Err(error) => return Live::Unknown(error),
        };
        if same {
            Live::Yes
        } else {
            self.terminals()
                .retain(|known| !(known.pane == *pane && known.terminal == terminal));
            Live::No
        }
    }
}

/// The candidates of `source`'s latest message, with their hashes; empty
/// without a transcript or a message.
async fn candidates(threads: &Arc<Threads>, source: ThreadSource) -> CommandList {
    let Some(message) = threads.latest_message(source).await else {
        return CommandList::default();
    };
    let extraction = extract::extract(&message.text);
    let candidates = extraction
        .candidates
        .into_iter()
        .enumerate()
        .filter_map(|(id, found)| {
            let id = u16::try_from(id).ok()?;
            Some(CommandCandidate {
                id,
                hash: hash(&message.text, id, &found.text),
                text: found.text,
                intent: found.intent,
                source: found.source,
            })
        })
        .collect();
    CommandList {
        candidates,
        omitted: extraction.omitted,
    }
}

/// A candidate's hash: SHA-256 over the whole message, the candidate's id
/// and its text, cut to [`protocol::COMMAND_HASH_LEN`] hex digits. Any
/// change to the message changes every hash.
fn hash(message: &str, id: u16, text: &str) -> CommandHash {
    let mut digest = Sha256::new();
    digest.update(b"passporter-command-v1\0");
    digest.update(message.as_bytes());
    digest.update(b"\0");
    digest.update(id.to_be_bytes());
    digest.update(b"\0");
    digest.update(text.as_bytes());
    let hex = digest
        .finalize()
        .iter()
        .take(protocol::COMMAND_HASH_LEN / 2)
        .fold(String::new(), |mut hex, byte| {
            // Writing to a String cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    // Always 32 lowercase hex digits.
    CommandHash::new(hex).unwrap_or_else(|_| unreachable!("a SHA-256 prefix is valid hex"))
}

/// `GET /v1/agents/:id/commands`: the candidates of the agent's latest
/// message. 400 for a malformed id, 404 if the agent is gone, 502 if herdr
/// fails; an agent without a transcript gets an empty list.
pub(crate) async fn list(State(state): State<AppState>, UrlPath(raw): UrlPath<String>) -> Response {
    let (id, agent) = match current_agent(&state, &raw).await {
        Ok(found) => found,
        Err(status) => return status.into_response(),
    };
    let list = match ThreadSource::for_agent(id, &agent) {
        Some(source) => candidates(&state.threads, source).await,
        None => CommandList::default(),
    };
    Json(list).into_response()
}

/// An outcome plus whether it is final for the key.
struct Outcome {
    result: CommandResult,
    /// False when nothing ran and nothing was decided (herdr or the audit
    /// log unavailable first): 503, the same key may be retried.
    decided: bool,
    /// For a rate-limited request: when to try again (`Retry-After`).
    retry_after: Option<Duration>,
}

impl Outcome {
    fn decided(result: CommandResult) -> Self {
        Self {
            result,
            decided: true,
            retry_after: None,
        }
    }

    fn refused(reason: CommandRefusal, message: impl Into<String>) -> Self {
        Self::decided(refused(reason, message))
    }

    fn failed(message: impl Into<String>) -> Self {
        Self::decided(CommandResult::Failed {
            message: message.into(),
        })
    }

    fn undecided(message: &str) -> Self {
        Self {
            decided: false,
            ..Self::failed(message)
        }
    }

    fn rate_limited(message: &str, wait: Duration) -> Self {
        Self {
            retry_after: Some(wait),
            ..Self::refused(CommandRefusal::RateLimited, message)
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

/// `POST /v1/agents/:id/commands/run`.
pub(crate) async fn run(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(client): Extension<ClientAddr>,
    UrlPath(raw): UrlPath<String>,
    body: Bytes,
) -> Response {
    let body: CommandRunRequest = match parse(&body) {
        Ok(body) => body,
        Err(refusal) => return respond(&refusal, true),
    };
    let Ok(agent) = raw.parse::<AgentId>() else {
        return respond(&invalid("malformed agent id"), true);
    };
    let request = RunRequest { agent, body };
    let commands = Arc::clone(&state.commands);
    let guard = match commands.runs.claim(&request) {
        Claim::Fresh(guard) => guard,
        Claim::Replay(result) => return respond(&result, true),
        Claim::Conflict => return respond(&conflict(), true),
        Claim::InFlight => {
            return Outcome::rate_limited(
                "This command is starting; retry shortly.",
                Duration::from_secs(1),
            )
            .respond();
        }
    };
    let budget = Budget::of(&principal, client);
    let work = tokio::spawn(async move {
        let outcome = decide_and_run(&state, &principal.device, &budget, &request).await;
        let line = CommandEntry::result(
            &principal.device,
            &request.agent,
            &request.body,
            &outcome.result,
        );
        audit_line(&state.commands.audit, &line).await;
        guard.finish(&outcome.result, outcome.decided);
        outcome
    });
    finish(work.await)
}

/// The candidate `request` names, if `source`'s latest message still has
/// it: same id, same hash.
async fn current(
    threads: &Arc<Threads>,
    source: ThreadSource,
    request: &CommandRunRequest,
) -> Option<CommandCandidate> {
    candidates(threads, source)
        .await
        .candidates
        .into_iter()
        .find(|c| c.id == request.candidate && c.hash == request.hash)
}

/// The agent as herdr has it now, or the outcome to answer instead.
async fn agent_now(state: &AppState, id: &AgentId) -> Result<herdr::Agent, Box<Outcome>> {
    match resolve(state.control.as_ref(), id).await {
        Ok(Resolved::Current(agent)) => Ok(agent),
        Ok(Resolved::Moved(_)) => Err(Box::new(stale())),
        Ok(Resolved::Gone) => Err(Box::new(Outcome::refused(
            CommandRefusal::NotFound,
            "That agent is gone.",
        ))),
        Err(error) => {
            tracing::warn!(%error, "herdr agent lookup failed");
            Err(Box::new(Outcome::undecided(
                "herdr is unavailable; nothing ran.",
            )))
        }
    }
}

/// Checks `request` against the agent's message now, spends the device's
/// budget only for a request that passes, audits, checks once more and
/// runs.
async fn decide_and_run(
    state: &AppState,
    device: &str,
    budget: &Budget,
    request: &RunRequest,
) -> Outcome {
    let commands = &state.commands;
    let body = &request.body;
    if body.origin == Origin::Car {
        return Outcome::refused(
            CommandRefusal::CarForbidden,
            "Commands never run from the car.",
        );
    }
    let agent = match agent_now(state, &request.agent).await {
        Ok(agent) => agent,
        Err(outcome) => return *outcome,
    };
    let Some(source) = ThreadSource::for_agent(request.agent.clone(), &agent) else {
        return stale();
    };
    let Some(candidate) = current(&state.threads, source.clone(), body).await else {
        return stale();
    };
    match body.target {
        RunTarget::Here if candidate.is_multiline() => {
            return Outcome::refused(
                CommandRefusal::NotHere,
                "A multi-line command only runs in a new terminal.",
            );
        }
        RunTarget::Here if !agent.agent_status.is_ready() => {
            return Outcome::refused(
                CommandRefusal::AgentBusy,
                "The agent is not at its prompt; run it once it is, or in a new terminal.",
            );
        }
        RunTarget::NewTerminal if !is_dir(&source.cwd).await => {
            return Outcome::refused(
                CommandRefusal::NotFound,
                "The agent's folder is gone, so there is nowhere to open a terminal.",
            );
        }
        _ => {}
    }
    if let Err(wait) = commands.run_limit.check(budget) {
        return Outcome::rate_limited(
            "Too many commands from this device; retry in a few minutes.",
            wait,
        );
    }
    let execute = CommandEntry::execute(device, &request.agent, body, &candidate.text);
    if let Err(error) = append(&commands.audit, &execute).await {
        tracing::error!(%error, "cannot write the audit log; refusing to run");
        commands.run_limit.refund(budget);
        return Outcome::undecided("The audit log is unavailable; nothing ran.");
    }
    // The message may have moved on while this was checked and audited.
    let again = match agent_now(state, &request.agent).await {
        Ok(again) => again,
        Err(outcome) => return *outcome,
    };
    let still = match ThreadSource::for_agent(request.agent.clone(), &again) {
        Some(source) if again.state_change_seq == agent.state_change_seq => {
            current(&state.threads, source, body).await
        }
        _ => None,
    };
    if still.as_ref() != Some(&candidate) {
        return stale();
    }
    match body.target {
        RunTarget::Here => run_here(state, &again, &candidate).await,
        RunTarget::NewTerminal => {
            run_in_terminal(commands, device, &again, &source.cwd, &candidate).await
        }
    }
}

/// Types `!<command>` into the agent's prompt.
async fn run_here(state: &AppState, agent: &herdr::Agent, candidate: &CommandCandidate) -> Outcome {
    let line = format!("!{}", candidate.text);
    match state.control.prompt(&agent.pane_id, &line).await {
        Ok(_) => Outcome::decided(CommandResult::Sent { replayed: false }),
        Err(error)
            if error.code() == Some(&herdr::ErrorCode::AgentBlocked)
                && !error.may_have_taken_effect() =>
        {
            Outcome::refused(
                CommandRefusal::AgentBusy,
                "The agent is waiting at a dialog; nothing was typed.",
            )
        }
        Err(error) => {
            tracing::warn!(%error, "typing a command into an agent failed");
            Outcome::failed(format!("herdr: {error}"))
        }
    }
}

/// Opens a tab in the agent's folder and types the command into its shell.
/// Only `device` may then watch it or send it keys.
async fn run_in_terminal(
    commands: &Commands,
    device: &str,
    agent: &herdr::Agent,
    cwd: &Path,
    candidate: &CommandCandidate,
) -> Outcome {
    let label = label(agent);
    let (pane, terminal) = match commands.tabs.open(&label, cwd).await {
        Ok((_, root)) => (root.pane_id, root.terminal_id),
        Err(error) if !error.may_have_taken_effect() => {
            tracing::warn!(%error, "herdr could not open a tab for a command");
            return Outcome::undecided("herdr is unavailable; nothing ran.");
        }
        Err(error) => {
            tracing::warn!(%error, "opening a tab for a command failed");
            return Outcome::failed(format!(
                "herdr: {error}. Nothing ran, but herdr may have opened an empty \"{label}\" tab \
                 in the {} workspace; close it if so.",
                commands.tabs.workspace()
            ));
        }
    };
    commands.remember(Opened {
        device: device.to_owned(),
        pane: pane.clone(),
        terminal,
    });
    wait_for_shell(
        commands.control.as_ref(),
        &pane,
        commands.settings.shell_wait,
    )
    .await;
    if let Err(error) = commands.control.type_line(&pane, &candidate.text).await {
        tracing::warn!(%error, %pane, "typing a command into its terminal failed");
        return Outcome::failed(format!(
            "herdr: {error}. The \"{label}\" tab is open, but the command may not have been \
             typed; check it in herdr."
        ));
    }
    tracing::info!(%pane, "command started in a new terminal from the phone");
    let Ok(pane) = protocol::PaneId::new(pane.as_str()) else {
        return Outcome::failed("herdr named the terminal's pane oddly; find it in herdr.");
    };
    Outcome::decided(CommandResult::Started {
        terminal: Terminal { pane, label },
        replayed: false,
    })
}

/// A terminal's tab label: `run: ` and the agent's name (or its kind and
/// pane, without one).
fn label(agent: &herdr::Agent) -> String {
    let name = agent.name.as_ref().map_or_else(
        || {
            format!(
                "{} {}",
                agent.kind.as_deref().unwrap_or("agent"),
                agent.pane_id
            )
        },
        ToString::to_string,
    );
    let name: String = name.chars().take(MAX_LABEL_NAME_CHARS).collect();
    format!("run: {name}")
}

async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_dir())
}

/// `GET /v1/terminals/:pane/screen`: the last lines of a terminal this
/// device's run opened, while its pane still runs that terminal. 404 for
/// any other pane, 502 if herdr fails.
pub(crate) async fn screen(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    UrlPath(raw): UrlPath<String>,
) -> Response {
    let Ok(pane) = protocol::PaneId::new(raw) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let pane = PaneId::new(pane.as_str());
    match state.commands.live(&principal.device, &pane).await {
        Live::Yes => {}
        Live::No => return StatusCode::NOT_FOUND.into_response(),
        Live::Unknown(error) => {
            tracing::warn!(%error, "herdr pane lookup failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    }
    let lines = u32::try_from(MAX_SCREEN_LINES).unwrap_or(u32::MAX);
    match state.commands.control.read_recent(&pane, lines).await {
        Ok(text) => {
            let text = screen_tail(&text, MAX_SCREEN_LINES);
            let revision = revision(&text);
            Json(TerminalScreen { text, revision }).into_response()
        }
        Err(error) if error.code().is_some_and(herdr::ErrorCode::is_not_found) => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "herdr terminal read failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// A revision for a terminal's text: changes when the text does.
fn revision(text: &str) -> u64 {
    let digest = Sha256::digest(text.as_bytes());
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first)
}

/// `POST /v1/terminals/:pane/keys`: allowlisted keys into a terminal this
/// device's run opened. Order: parse (400) → ledger → never the car (403)
/// → a terminal of this device's that still runs (404; 503 if herdr does
/// not say) → per-device rate limit (429) → audit `execute` → herdr →
/// audit `result`.
pub(crate) async fn keys(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(client): Extension<ClientAddr>,
    UrlPath(raw): UrlPath<String>,
    body: Bytes,
) -> Response {
    let body: TerminalKeysRequest = match parse(&body) {
        Ok(body) => body,
        Err(refusal) => return respond(&refusal, true),
    };
    let Ok(pane) = protocol::PaneId::new(raw) else {
        return respond(&invalid("malformed pane id"), true);
    };
    let request = KeysRequest {
        pane: PaneId::new(pane.as_str()),
        body,
    };
    let commands = Arc::clone(&state.commands);
    let guard = match commands.keys.claim(&request) {
        Claim::Fresh(guard) => guard,
        Claim::Replay(result) => return respond(&result, true),
        Claim::Conflict => return respond(&conflict(), true),
        Claim::InFlight => {
            return Outcome::rate_limited(
                "These keys are being sent; retry shortly.",
                Duration::from_secs(1),
            )
            .respond();
        }
    };
    let budget = Budget::of(&principal, client);
    let work = tokio::spawn(async move {
        let outcome = send_keys(&commands, &principal.device, &budget, &request).await;
        let line = KeysEntry::new(
            Phase::Result,
            &principal.device,
            &request.pane,
            &request.body,
            Some(&outcome.result),
        );
        audit_line(&commands.audit, &line).await;
        guard.finish(&outcome.result, outcome.decided);
        outcome
    });
    finish(work.await)
}

async fn send_keys(
    commands: &Commands,
    device: &str,
    budget: &Budget,
    request: &KeysRequest,
) -> Outcome {
    if request.body.origin == Origin::Car {
        return Outcome::refused(
            CommandRefusal::CarForbidden,
            "Terminals take no keys from the car.",
        );
    }
    match commands.live(device, &request.pane).await {
        Live::Yes => {}
        Live::No => {
            return Outcome::refused(
                CommandRefusal::NotFound,
                "No terminal this phone opened runs in that pane.",
            );
        }
        Live::Unknown(error) => {
            tracing::warn!(%error, "herdr pane lookup failed");
            return Outcome::undecided("herdr is unavailable; nothing was sent.");
        }
    }
    if let Err(wait) = commands.keys_limit.check(budget) {
        return Outcome::rate_limited("Too many keys from this device; retry shortly.", wait);
    }
    let line = KeysEntry::new(Phase::Execute, device, &request.pane, &request.body, None);
    if let Err(error) = append(&commands.audit, &line).await {
        tracing::error!(%error, "cannot write the audit log; refusing to send keys");
        commands.keys_limit.refund(budget);
        return Outcome::undecided("The audit log is unavailable; nothing was sent.");
    }
    let names: Vec<&str> = request
        .body
        .keys
        .as_slice()
        .iter()
        .map(|k| k.as_str())
        .collect();
    match commands.control.send_keys(&request.pane, &names).await {
        Ok(()) => Outcome::decided(CommandResult::Sent { replayed: false }),
        Err(error) if error.code().is_some_and(herdr::ErrorCode::is_not_found) => {
            Outcome::refused(CommandRefusal::NotFound, "That terminal is closed.")
        }
        Err(error) => {
            tracing::warn!(%error, "sending keys to a terminal failed");
            Outcome::failed(format!("herdr: {error}"))
        }
    }
}

/// A strict body, or the `invalid_request` refusal to answer (400).
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, CommandResult> {
    serde_json::from_slice(body).map_err(|error| {
        let message: String = error.to_string().chars().take(MAX_ERROR_CHARS).collect();
        invalid(message)
    })
}

fn finish(work: Result<Outcome, tokio::task::JoinError>) -> Response {
    match work {
        Ok(outcome) => outcome.respond(),
        Err(error) => {
            tracing::error!(%error, "command task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn refused(reason: CommandRefusal, message: impl Into<String>) -> CommandResult {
    CommandResult::Refused {
        reason,
        message: message.into(),
    }
}

fn invalid(message: impl Into<String>) -> CommandResult {
    refused(CommandRefusal::InvalidRequest, message)
}

fn conflict() -> CommandResult {
    refused(
        CommandRefusal::IdempotencyConflict,
        "This idempotency key was already used for a different request.",
    )
}

fn stale() -> Outcome {
    Outcome::refused(
        CommandRefusal::Stale,
        "That command is no longer in the agent's latest message; nothing ran.",
    )
}

async fn append(audit: &Arc<AuditLog>, entry: &impl serde::Serialize) -> std::io::Result<()> {
    let line = serde_json::to_value(entry).map_err(std::io::Error::other)?;
    let audit = Arc::clone(audit);
    tokio::task::spawn_blocking(move || audit.append(&line))
        .await
        .map_err(std::io::Error::other)?
}

/// Best effort: what it records has already happened.
async fn audit_line(audit: &Arc<AuditLog>, entry: &impl serde::Serialize) {
    if let Err(error) = append(audit, entry).await {
        tracing::error!(%error, "cannot write a command audit line");
    }
}

fn respond(result: &CommandResult, decided: bool) -> Response {
    let status = if decided {
        status_for(result)
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(result)).into_response()
}

fn status_for(result: &CommandResult) -> StatusCode {
    match result {
        CommandResult::Sent { .. }
        | CommandResult::Started { .. }
        | CommandResult::Failed { .. } => StatusCode::OK,
        CommandResult::Refused { reason, .. } => match reason {
            CommandRefusal::InvalidRequest => StatusCode::BAD_REQUEST,
            CommandRefusal::NotFound => StatusCode::NOT_FOUND,
            CommandRefusal::CarForbidden => StatusCode::FORBIDDEN,
            CommandRefusal::Stale | CommandRefusal::AgentBusy => StatusCode::CONFLICT,
            CommandRefusal::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            CommandRefusal::NotHere
            | CommandRefusal::IdempotencyConflict
            | CommandRefusal::Other => StatusCode::UNPROCESSABLE_ENTITY,
        },
        CommandResult::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_bind_the_message_the_position_and_the_text() {
        let base = hash("run `ls`", 0, "ls");
        assert_eq!(base.as_str().len(), protocol::COMMAND_HASH_LEN);
        assert_eq!(base, hash("run `ls`", 0, "ls"));
        assert_ne!(base, hash("run `ls` now", 0, "ls"));
        assert_ne!(base, hash("run `ls`", 1, "ls"));
        assert_ne!(base, hash("run `ls`", 0, "ls -la"));
    }

    #[test]
    fn replays_say_so_and_only_rate_limits_are_transient() {
        let terminal = Terminal {
            pane: protocol::PaneId::new("w1:p1").expect("pane"),
            label: "run: x".into(),
        };
        assert_eq!(
            replayed(CommandResult::Started {
                terminal: terminal.clone(),
                replayed: false
            }),
            CommandResult::Started {
                terminal,
                replayed: true
            }
        );
        assert_eq!(
            replayed(CommandResult::Sent { replayed: false }),
            CommandResult::Sent { replayed: true }
        );
        assert!(is_transient(&refused(CommandRefusal::RateLimited, "")));
        assert!(!is_transient(&refused(CommandRefusal::Stale, "")));
    }

    #[test]
    fn labels_name_the_agent_briefly() {
        let agent: herdr::Agent = serde_json::from_value(serde_json::json!({
            "terminal_id": "t1", "pane_id": "w1:p2", "tab_id": "w1:t1", "workspace_id": "w1",
            "agent": "claude", "agent_status": "idle", "revision": 1, "focused": false,
        }))
        .expect("agent");
        assert_eq!(label(&agent), "run: claude w1:p2");
        let named = herdr::Agent {
            name: Some(herdr::AgentName::new("api-fix").expect("name")),
            ..agent
        };
        assert_eq!(label(&named), "run: api-fix");
    }
}
