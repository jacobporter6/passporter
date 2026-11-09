//! `POST /v1/actions`, the only mutating endpoint (design §5, protocol
//! `action` and `result`).
//!
//! Order: parse (400 `invalid_action`) → `origin` from the session's scope
//! ([`protocol::Scope::origin`]: a car session is always `car`, an app
//! session's claim can only narrow) → idempotency ledger (replay,
//! in flight or `idempotency_conflict`, none of which cost any budget) →
//! per-device rate limit for a fresh key (429 `rate_limited` with
//! `Retry-After`, the key released so it may be retried; design §8 least
//! privilege; an app `interrupt` is never limited, so Stop always works) → fresh `agent.get`, and for a blocked agent a
//! fresh screen parse ([`crate::pending`]) → stale guard (409: `expectedSeq`,
//! `pendingId` = the current content-hashed `screen-<seq>-<hex>` for
//! `choose` and for `keys` that carry one, and for `choose` the option must
//! be in the current parse) → policy ([`policy`]) → a prompt's attachments
//! must be the device's own and still stored (`unknown_attachment`
//! otherwise; [`crate::attachments`]) → audit `execute` entry →
//! for actions naming a pending id, one more read and parse that must give
//! the same `seq` and id (`recheck`) → herdr → audit `result` entry.
//!
//! Policy judges a hook-described dialog as hook-sourced
//! ([`Dialog::policy_input`]), so the car can answer it; the wire (and the
//! stale guard) keep the screen's input.
//!
//! [`car`] serves `POST /v1/agents/:id/car-reply` on top of this pipeline.
//!
//! `choose` sends the option's key (its digit, or Esc for an `(esc)`
//! option). With `feedbackText` (only on an option that accepts it: *No, and
//! tell Claude what to do differently*), the bridge then waits up to
//! [`PendingSettings::feedback_wait`](crate::pending::PendingSettings) for
//! herdr to report the agent `idle`/`done` at a new `seq` and sends the text
//! with `agent.prompt`. If Claude does not get there in time, or the prompt
//! fails, the result is `failed` (the option was chosen, the feedback was
//! not sent), never a guess. The work after the ledger runs in its own task, so a client
//! hanging up mid-request cannot leave an action half-recorded.
//!
//! HTTP status by result: `sent` and `failed` 200 (both final), `stale`
//! 409, `refused` 400 / 403 / 404 / 422 / 429 by reason, and 503 (nothing
//! sent, nothing recorded, retry the same key) when herdr or the audit log
//! is unavailable before sending.

pub(crate) mod audit;
pub(crate) mod car;
pub(crate) mod ledger;
mod policy;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use protocol::{
    Action, ActionKind, ActionResult, Agent, AgentId, ChoosePayload, Origin, PendingInput,
    RefusalReason, Seq,
};

use self::audit::{AuditLog, Entry, Phase};
use self::ledger::{Claim, Idempotent, Ledger};
use crate::attachments::{self, Attached};
use crate::auth::{RateLimit, RateLimiter};
use crate::control::{AgentControl, Resolved, resolve};
use crate::pending::Dialog;
use crate::registry::{Hub, protocol_status};
use crate::server::{AppState, ClientAddr, Principal, STATIC_DEVICE};

/// Longest parse error echoed back to the client.
const MAX_ERROR_CHARS: usize = 200;

/// Settings for the actions endpoint.
#[derive(Debug, Clone)]
pub struct ActionSettings {
    /// The audit log file.
    pub audit_log: PathBuf,
    /// Most finished results remembered for idempotent replay.
    pub idempotency_capacity: usize,
    /// Actions each device may post (design §8: per-device rate limits);
    /// static-token clients get a budget per client address.
    pub rate: RateLimit,
}

impl ActionSettings {
    /// Defaults: remember 1024 results; 60 actions per minute per device,
    /// far above anything a person taps or an outbox drains.
    #[must_use]
    pub fn new(audit_log: impl Into<PathBuf>) -> Self {
        Self {
            audit_log: audit_log.into(),
            idempotency_capacity: 1024,
            rate: RateLimit::per_minute(60),
        }
    }
}

/// The ledgers (actions, car replies), the audit log and the per-device
/// limiter both spend.
#[derive(Debug)]
pub struct Actions {
    ledger: Arc<Ledger<Action>>,
    car: Arc<Ledger<car::CarCall>>,
    audit: Arc<AuditLog>,
    limit: RateLimiter<Budget>,
}

/// Whose rate-limit budget a mutating request spends (actions here,
/// spawns in [`crate::spawn`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Budget {
    /// A paired device.
    Device(String),
    /// A static-token client: every one shares the same principal, so the
    /// budget is per client address instead (one client cannot use up the
    /// others').
    StaticToken(IpAddr),
}

impl Budget {
    pub(crate) fn of(principal: &Principal, client: ClientAddr) -> Self {
        if principal.device == STATIC_DEVICE {
            Self::StaticToken(client.0)
        } else {
            Self::Device(principal.device.clone())
        }
    }
}

/// `Retry-After` seconds for `wait`, rounded up (and at least 1), so a
/// client that waits as told is outside the window.
pub(crate) fn retry_after_secs(wait: Duration) -> u64 {
    (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1)
}

impl Actions {
    /// An empty ledger writing to `settings.audit_log`.
    #[must_use]
    pub fn new(settings: &ActionSettings) -> Self {
        Self {
            ledger: Ledger::new(settings.idempotency_capacity),
            car: Ledger::new(settings.idempotency_capacity),
            audit: Arc::new(AuditLog::new(&settings.audit_log)),
            limit: RateLimiter::new(settings.rate),
        }
    }

    /// The audit log, shared with [`crate::spawn`] so both write one file.
    pub(crate) fn audit(&self) -> Arc<AuditLog> {
        Arc::clone(&self.audit)
    }
}

/// An outcome plus whether it is final for the key.
struct Outcome {
    result: ActionResult,
    /// False when the bridge could not decide (herdr or the audit log
    /// unavailable before anything was sent): answered with 503, not
    /// recorded, so the same key may be retried.
    decided: bool,
}

impl Outcome {
    fn decided(result: ActionResult) -> Self {
        Self {
            result,
            decided: true,
        }
    }

    fn undecided(result: ActionResult) -> Self {
        Self {
            result,
            decided: false,
        }
    }
}

pub(crate) async fn post(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(client): Extension<ClientAddr>,
    body: Bytes,
) -> Response {
    let mut action: Action = match parse(&body) {
        Ok(action) => action,
        Err(message) => return respond(&refused(RefusalReason::InvalidAction, message), true),
    };
    // The token decides the origin, not the body (security.md D2).
    action.origin = principal.scope.origin(action.origin);
    // Stop must always work, but only the app can interrupt at all: the
    // car (and a notification) is refused by policy, so it pays like the rest.
    let exempt = matches!(action.kind, ActionKind::Interrupt(_)) && action.origin == Origin::App;
    let budget = (!exempt).then(|| Budget::of(&principal, client));
    let actions = Arc::clone(&state.actions);
    // A car reply's key names its action too: never run again from here.
    let taken = actions.car.holds(&action.idempotency_key);
    run_once(
        &actions.limit,
        &actions.ledger,
        taken,
        action,
        budget,
        |action| async move {
            let outcome = decide_and_execute(&state, &principal.device, &action).await;
            audit_result(&state, &principal.device, &action, &outcome.result).await;
            (outcome.result, outcome.decided)
        },
    )
    .await
}

/// `body` as a strict request, or the parse error (cut short) to refuse it
/// with.
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    serde_json::from_slice(body)
        .map_err(|error| error.to_string().chars().take(MAX_ERROR_CHARS).collect())
}

/// A request served at most once through an idempotency ledger, and how
/// its endpoint answers.
pub(crate) trait Answered: Idempotent + Send + Sync + 'static {
    /// What the request is called in refusals ("action").
    const NOUN: &'static str;

    /// A refusal decided before any work: by the ledger or the rate limit.
    fn refusal(reason: RefusalReason, message: String) -> Self::Outcome;

    /// The result inside an outcome, which sets the HTTP status.
    fn result(outcome: &Self::Outcome) -> &ActionResult;
}

impl Answered for Action {
    const NOUN: &'static str = "action";

    fn refusal(reason: RefusalReason, message: String) -> ActionResult {
        refused(reason, message)
    }

    fn result(outcome: &ActionResult) -> &ActionResult {
        outcome
    }
}

/// Serves `request` once: a known key replays its outcome (a different
/// body, or a key `taken_elsewhere` by the other ledger, is
/// `idempotency_conflict`, one still running is answered
/// `rate_limited`); a fresh key spends `budget`, if any (429 with
/// `Retry-After`, the key released), then runs `work` on the request in
/// its own task, so
/// a client hanging up cannot leave it half done. `work` says whether its
/// outcome is final for the key (false: 503, retry the same key).
async fn run_once<R, F, W>(
    limit: &RateLimiter<Budget>,
    ledger: &Arc<Ledger<R>>,
    taken_elsewhere: bool,
    request: R,
    budget: Option<Budget>,
    work: F,
) -> Response
where
    R: Answered,
    R::Outcome: serde::Serialize + Send + 'static,
    F: FnOnce(R) -> W,
    W: Future<Output = (R::Outcome, bool)> + Send + 'static,
{
    let answer = |outcome: &R::Outcome, decided| respond_with(outcome, R::result(outcome), decided);
    let noun = R::NOUN;
    let claim = if taken_elsewhere {
        Claim::Conflict
    } else {
        ledger.claim(&request)
    };
    let guard = match claim {
        Claim::Fresh(guard) => guard,
        Claim::Replay(outcome) => return answer(&outcome, true),
        Claim::Conflict => {
            let message = format!("This idempotency key was already used for a different {noun}.");
            return answer(
                &R::refusal(RefusalReason::IdempotencyConflict, message),
                true,
            );
        }
        Claim::InFlight => {
            let message = format!("This {noun} is still being sent; retry shortly.");
            return answer(&R::refusal(RefusalReason::RateLimited, message), true);
        }
    };
    if let Some(budget) = budget
        && let Err(wait) = limit.check(&budget)
    {
        // Dropping the guard releases the key: nothing was decided.
        drop(guard);
        let message = "Too many actions from this device; retry shortly.".to_owned();
        let mut response = answer(&R::refusal(RefusalReason::RateLimited, message), true);
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from(retry_after_secs(wait)),
        );
        return response;
    }
    let work = work(request);
    let task = tokio::spawn(async move {
        let (outcome, decided) = work.await;
        guard.finish(&outcome, decided);
        (outcome, decided)
    });
    match task.await {
        Ok((outcome, decided)) => answer(&outcome, decided),
        Err(error) => {
            tracing::error!(%error, "{noun} task failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn decide_and_execute(state: &AppState, device: &str, action: &Action) -> Outcome {
    match read_current(state, &action.agent_id).await {
        Ok((agent, dialog)) => {
            let judged = dialog.as_ref().map(Dialog::policy_input);
            decide_and_execute_on(state, device, action, agent, dialog, judged).await
        }
        Err(outcome) => *outcome,
    }
}

/// [`decide_and_execute`] against `agent` and its `dialog` as just read
/// by the caller ([`read_current`]), and `judged`, the dialog as policy
/// sees it ([`Dialog::policy_input`]), so a caller that already needed
/// them (a car reply, to map its utterance) reads herdr and copies the
/// input once.
async fn decide_and_execute_on(
    state: &AppState,
    device: &str,
    action: &Action,
    agent: herdr::Agent,
    dialog: Option<Dialog>,
    judged: Option<PendingInput>,
) -> Outcome {
    let pending = dialog.as_ref().map(|dialog| &dialog.input);
    if action.expected_seq != Seq(agent.state_change_seq)
        || !action.pending_is_current(pending)
        || !option_is_current(action, pending)
    {
        return Outcome::decided(stale(&state.hub, &agent, pending.cloned()));
    }
    if let Some(refusal) = policy::refusal(action, judged.as_ref()) {
        return Outcome::decided(refusal);
    }
    let attached = match &action.kind {
        ActionKind::Prompt(prompt) => {
            match state
                .attachments
                .resolve(device, prompt.attachments.as_slice())
            {
                Ok(attached) => attached,
                Err(missing) => {
                    return Outcome::decided(refused(
                        RefusalReason::UnknownAttachment,
                        format!(
                            "Attachment {missing} is not available (expired, or never uploaded \
                             from this device); attach the photo again."
                        ),
                    ));
                }
            }
        }
        _ => Vec::new(),
    };
    if let Err(error) = audit_execute(state, device, action, &attached).await {
        tracing::error!(%error, "cannot write the audit log; refusing to execute");
        return Outcome::undecided(refused(
            RefusalReason::Other,
            "The audit log is unavailable; nothing was sent.",
        ));
    }
    if action.pending_id().is_none() {
        return Outcome::decided(execute(state, &agent, action, dialog.as_ref(), &attached).await);
    }
    match recheck(state, action, &agent).await {
        Ok((agent, dialog)) => {
            Outcome::decided(execute(state, &agent, action, dialog.as_ref(), &attached).await)
        }
        Err(outcome) => *outcome,
    }
}

/// The last check before sending an action that answers a specific
/// dialog (`choose`, or `keys` with a `pendingId`): after the audit entry,
/// read the agent and its screen once more and require the same `seq` and
/// the same content-hashed pending id, so a dialog that changed while the
/// action was being checked and audited gets nothing (409 with the new
/// pending). What remains is the time between this read and herdr
/// delivering the key, a few milliseconds on the local socket; the
/// protocol cannot close it without herdr support for conditional sends.
async fn recheck(
    state: &AppState,
    action: &Action,
    checked: &herdr::Agent,
) -> Result<(herdr::Agent, Option<Dialog>), Box<Outcome>> {
    let (agent, dialog) = read_current(state, &action.agent_id).await?;
    let pending = dialog.as_ref().map(|dialog| &dialog.input);
    if agent.state_change_seq != checked.state_change_seq || !action.pending_is_current(pending) {
        return Err(Box::new(Outcome::decided(stale(
            &state.hub,
            &agent,
            pending.cloned(),
        ))));
    }
    Ok((agent, dialog))
}

/// A fresh `agent.get` for agent `id` and, if it is blocked, a fresh
/// parse of its dialog. The agent gone or replaced, or herdr unavailable,
/// is the outcome to answer instead.
async fn read_current(
    state: &AppState,
    id: &AgentId,
) -> Result<(herdr::Agent, Option<Dialog>), Box<Outcome>> {
    let unavailable = || {
        Box::new(Outcome::undecided(ActionResult::Failed {
            message: "herdr is unavailable; nothing was sent.".to_owned(),
        }))
    };
    let agent = match resolve(state.control.as_ref(), id).await {
        Ok(Resolved::Current(agent)) => agent,
        Ok(Resolved::Moved(agent)) => {
            return Err(Box::new(Outcome::decided(stale(&state.hub, &agent, None))));
        }
        Ok(Resolved::Gone) => {
            return Err(Box::new(Outcome::decided(refused(
                RefusalReason::UnknownAgent,
                "That agent is gone.",
            ))));
        }
        Err(error) => {
            tracing::warn!(%error, "herdr agent lookup failed");
            return Err(unavailable());
        }
    };
    match state.pending.current(&agent).await {
        Ok(dialog) => Ok((agent, dialog)),
        Err(error) => {
            tracing::warn!(%error, "herdr screen read failed");
            Err(unavailable())
        }
    }
}

/// The option part of the stale guard: a `choose` must name an option of
/// the screen as parsed just now. The content-hashed pending id already
/// changes with the options; this also covers a client that names an
/// option the current dialog never offered (stale, not wrong).
fn option_is_current(action: &Action, pending: Option<&PendingInput>) -> bool {
    match &action.kind {
        ActionKind::Choose(choose) => {
            pending.is_some_and(|pending| pending.options.get(&choose.option_id).is_some())
        }
        _ => true,
    }
}

/// Sends `action`; a prompt's `attached` images are named after its text
/// ([`attachments::prompt_text`]).
async fn execute(
    state: &AppState,
    agent: &herdr::Agent,
    action: &Action,
    dialog: Option<&Dialog>,
    attached: &[Attached],
) -> ActionResult {
    let control = state.control.as_ref();
    let hub = &state.hub;
    let pane = &agent.pane_id;
    let sent = match &action.kind {
        ActionKind::Prompt(prompt) => {
            let text =
                attachments::prompt_text(prompt.text.as_str(), agent.kind.as_deref(), attached);
            control.prompt(pane, &text).await.map(drop)
        }
        ActionKind::Keys(keys) => {
            let names: Vec<&str> = keys.keys.as_slice().iter().map(|k| k.as_str()).collect();
            control.send_keys(pane, &names).await
        }
        ActionKind::Interrupt(_) => control.send_keys(pane, &["ctrl+c"]).await,
        ActionKind::Choose(choose) => {
            // The stale guard checked the option is in this parse.
            let Some(key) = dialog.and_then(|dialog| dialog.key_for(&choose.option_id)) else {
                return stale(hub, agent, dialog.map(|dialog| dialog.input.clone()));
            };
            let sent = control.send_keys(pane, &[key]).await;
            if sent.is_ok() && choose.feedback_text.is_some() {
                return send_feedback(state, agent, choose).await;
            }
            sent
        }
    };
    match sent {
        Ok(()) => ActionResult::Sent { replayed: false },
        // herdr refused before typing anything: the agent is at a dialog now.
        Err(error)
            if error.code() == Some(&herdr::ErrorCode::AgentBlocked)
                && !error.may_have_taken_effect() =>
        {
            let mut now = agent.clone();
            now.agent_status = herdr::AgentStatus::Blocked;
            stale(hub, &now, None)
        }
        Err(error) => {
            tracing::warn!(%error, "herdr action failed");
            ActionResult::Failed {
                message: format!("herdr: {error}"),
            }
        }
    }
}

/// After choosing an option that takes feedback: waits for Claude to leave
/// the dialog for its prompt (herdr `idle`/`done` at a newer `seq`), then
/// sends the feedback as a prompt. Anything else is a `failed` result: the
/// option was chosen, the feedback was not sent.
async fn send_feedback(
    state: &AppState,
    agent: &herdr::Agent,
    choose: &ChoosePayload,
) -> ActionResult {
    let Some(feedback) = &choose.feedback_text else {
        return ActionResult::Sent { replayed: false };
    };
    let settings = state.pending.settings();
    let control = state.control.as_ref();
    let ready = tokio::time::timeout(
        settings.feedback_wait,
        wait_for_prompt(control, agent, settings.feedback_poll),
    )
    .await;
    if !matches!(ready, Ok(true)) {
        return ActionResult::Failed {
            message: format!(
                "{OPTION_CHOSEN} the agent did not return to its prompt in time, so the feedback \
                 was not sent. Send it as a prompt."
            ),
        };
    }
    match control.prompt(&agent.pane_id, feedback.as_str()).await {
        Ok(_) => ActionResult::Sent { replayed: false },
        Err(error) => {
            tracing::warn!(%error, "sending choose feedback failed");
            ActionResult::Failed {
                message: format!(
                    "{OPTION_CHOSEN} sending the feedback failed (herdr: {error}). Check the \
                     agent before sending it again."
                ),
            }
        }
    }
}

/// How every `failed` result of [`send_feedback`] starts: the option's
/// key went out, only the feedback did not.
const OPTION_CHOSEN: &str = "The option was chosen, but";

/// Whether `result` says a `choose` with feedback chose its option and
/// only the feedback was lost ([`send_feedback`]), rather than failing
/// before anything was sent.
pub(crate) fn only_feedback_lost(result: &ActionResult) -> bool {
    matches!(result, ActionResult::Failed { message } if message.starts_with(OPTION_CHOSEN))
}

/// Polls herdr until the agent in `before`'s pane is `idle` or `done` at a
/// newer `seq` (true), or is gone or replaced (false).
async fn wait_for_prompt(
    control: &dyn AgentControl,
    before: &herdr::Agent,
    every: Duration,
) -> bool {
    loop {
        tokio::time::sleep(every).await;
        match control.agent(&before.pane_id).await {
            Ok(now) => {
                if now.agent_session != before.agent_session {
                    return false;
                }
                let at_prompt = matches!(
                    now.agent_status,
                    herdr::AgentStatus::Idle | herdr::AgentStatus::Done
                );
                if at_prompt && now.state_change_seq > before.state_change_seq {
                    return true;
                }
            }
            Err(error) if error.code().is_some_and(herdr::ErrorCode::is_not_found) => {
                return false;
            }
            Err(error) => tracing::debug!(%error, "agent lookup during feedback wait failed"),
        }
    }
}

/// A 409 with the agent's current state (the registry's view of the pane,
/// updated with what herdr just said) and its current pending input.
fn stale(hub: &Hub, agent: &herdr::Agent, pending: Option<PendingInput>) -> ActionResult {
    let current = crate::registry::agent_id(agent).ok().and_then(|id| {
        hub.agent_in(id.pane()).map(|known| Agent {
            id,
            status: protocol_status(agent.agent_status),
            seq: Seq(agent.state_change_seq),
            ..known
        })
    });
    ActionResult::Stale {
        current: current.map(Box::new),
        pending,
    }
}

fn refused(reason: RefusalReason, message: impl Into<String>) -> ActionResult {
    ActionResult::Refused {
        reason,
        message: message.into(),
    }
}

async fn audit_execute(
    state: &AppState,
    device: &str,
    action: &Action,
    attached: &[Attached],
) -> std::io::Result<()> {
    let audit = Arc::clone(&state.actions.audit);
    let device = device.to_owned();
    let action = action.clone();
    let attached = attached.to_vec();
    tokio::task::spawn_blocking(move || {
        audit.append(&Entry::new(Phase::Execute, &device, &action, None).attached(&attached))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Best effort: the action already happened (or was refused).
async fn audit_result(state: &AppState, device: &str, action: &Action, result: &ActionResult) {
    let audit = Arc::clone(&state.actions.audit);
    let device = device.to_owned();
    let action = action.clone();
    let result = result.clone();
    let written = tokio::task::spawn_blocking(move || {
        audit.append(&Entry::new(Phase::Result, &device, &action, Some(&result)))
    })
    .await
    .map_err(std::io::Error::other)
    .and_then(|written| written);
    if let Err(error) = written {
        tracing::error!(%error, "cannot write an audit result entry");
    }
}

fn respond(result: &ActionResult, decided: bool) -> Response {
    respond_with(result, result, decided)
}

/// `body` with the status of `result`, or 503 when not `decided`.
fn respond_with<B: serde::Serialize>(body: &B, result: &ActionResult, decided: bool) -> Response {
    let status = if decided {
        status_for(result)
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

fn status_for(result: &ActionResult) -> StatusCode {
    match result {
        ActionResult::Sent { .. } | ActionResult::Failed { .. } => StatusCode::OK,
        ActionResult::Stale { .. } => StatusCode::CONFLICT,
        ActionResult::Refused { reason, .. } => match reason {
            RefusalReason::InvalidAction => StatusCode::BAD_REQUEST,
            RefusalReason::UnknownAgent => StatusCode::NOT_FOUND,
            RefusalReason::StepUpRequired | RefusalReason::CarPolicy => StatusCode::FORBIDDEN,
            RefusalReason::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        },
        ActionResult::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_rounds_up() {
        assert_eq!(retry_after_secs(Duration::from_millis(59_001)), 60);
        assert_eq!(retry_after_secs(Duration::from_secs(60)), 60);
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::ZERO), 1);
    }

    #[test]
    fn static_token_clients_get_a_budget_each() {
        let phone = ClientAddr(IpAddr::from([10, 77, 70, 2]));
        let car = ClientAddr(IpAddr::from([10, 77, 70, 3]));
        let shared = Principal::new(STATIC_DEVICE);
        assert_ne!(Budget::of(&shared, phone), Budget::of(&shared, car));
        let device = Principal::new("dev-0000000000000001");
        assert_eq!(Budget::of(&device, phone), Budget::of(&device, car));
    }
}
