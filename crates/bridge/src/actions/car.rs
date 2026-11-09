//! `POST /v1/agents/:id/car-reply` (design §6.6): Android Auto's spoken
//! reply, mapped by the bridge so the phone stays a thin pipe.
//!
//! Order: parse (400) → idempotency ledger of its own, keyed by agent and
//! request (a replay answers the stored reply, sentence included) → the
//! per-device rate limit shared with `POST /v1/actions` →
//! [`CarIntent::interpret`] (an ambiguous or empty utterance is refused
//! with the reason) → a fresh read of the agent and its dialog → the
//! request's `pendingId`, if any, must still be the current one (409) →
//! the intent becomes one action:
//!
//! - **Allow**: the low-risk `yes` (*Allow once*) option of the current
//!   dialog, which a hook must have described; the request must name it.
//! - **Deny**: its `no` option, or `no_feedback` with the rest of the
//!   utterance as feedback; same conditions.
//! - **Prompt**: only while no dialog is open.
//!
//! Nothing else is reachable: no elevated option, keys, interrupt,
//! runbook, terminal command or attachment. The action then runs through
//! the same pipeline as `POST /v1/actions` with `origin: "car"` (stale
//! guard, car policy, audit before executing, recheck, herdr), so the car
//! policy is enforced twice.
//!
//! The answer is a [`CarReply`]: the result plus one short sentence for
//! the car to read aloud. Statuses are those of `POST /v1/actions`.

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use protocol::{
    Action, ActionKind, ActionResult, AgentId, CarIntent, CarRefusal, CarReply, CarReplyRequest,
    ChoosePayload, FeedbackText, IdempotencyKey, OptionId, Origin, PendingId, PendingInput,
    PendingOption, PendingSource, PromptPayload, PromptText, RefusalReason, Risk, Seq, Utterance,
    UtteranceRefusal,
};

use super::ledger::Idempotent;
use super::{
    Answered, Budget, Outcome, audit_result, decide_and_execute_on, only_feedback_lost, parse,
    read_current, refused, respond_with, run_once, stale,
};
use crate::one_line::one_line;
use crate::pending::Dialog;
use crate::server::{AppState, ClientAddr, Principal};

/// Longest part of a dialog's prompt read back after allowing it.
const MAX_SUMMARY_CHARS: usize = 80;

const SPEECH_NEEDS_PHONE: &str = "That needs the phone.";
const SPEECH_MOVED_ON: &str = "The agent has moved on. Please check your phone.";
const SPEECH_UNREACHABLE: &str = "I can't reach the agent right now. Try again in a moment.";
const SPEECH_GONE: &str = "That agent is gone.";
const SPEECH_BUSY: &str = "Too many replies at once. Try again in a moment.";

/// Why an understood utterance cannot become an action from the car.
/// `Display` is read aloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotFromTheCar {
    /// Allow or Deny with no pending input.
    NotAsking,
    /// Allow or Deny without the `pendingId` the car read aloud.
    Which,
    /// The dialog was not described by a hook (screen parse only).
    Unreliable,
    /// A prompt while a dialog is open.
    AnswerFirst,
    /// Deny with a note on a dialog whose deny takes none.
    NoNote,
    /// No low-risk option for the intent (only elevated ones).
    NeedsPhone,
}

impl std::fmt::Display for NotFromTheCar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAsking => f.write_str("The agent isn't asking anything right now."),
            Self::Which => f.write_str(
                "I'm not sure which question you're answering. Please check your phone.",
            ),
            Self::Unreliable => CarRefusal::UnreliableSource.fmt(f),
            Self::AnswerFirst => {
                f.write_str("The agent is waiting for an answer. Say yes or no, or use your phone.")
            }
            Self::NoNote => f.write_str(
                "That question doesn't take a message. Say just no, or answer on your phone.",
            ),
            Self::NeedsPhone => f.write_str(SPEECH_NEEDS_PHONE),
        }
    }
}

/// A car reply as its ledger keys it: the request and the agent it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CarCall {
    agent: AgentId,
    request: CarReplyRequest,
}

impl Idempotent for CarCall {
    type Outcome = CarReply;

    fn idempotency_key(&self) -> &IdempotencyKey {
        &self.request.idempotency_key
    }

    fn replayed(outcome: CarReply) -> CarReply {
        CarReply {
            result: Action::replayed(outcome.result),
            ..outcome
        }
    }

    fn is_transient(outcome: &CarReply) -> bool {
        Action::is_transient(&outcome.result)
    }
}

impl Answered for CarCall {
    const NOUN: &'static str = "reply";

    fn refusal(reason: RefusalReason, message: String) -> CarReply {
        let speech = match reason {
            RefusalReason::RateLimited => SPEECH_BUSY,
            _ => SPEECH_NEEDS_PHONE,
        };
        refusal(speech, reason, message)
    }

    fn result(outcome: &CarReply) -> &ActionResult {
        &outcome.result
    }
}

pub(crate) async fn post(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(principal): Extension<Principal>,
    Extension(client): Extension<ClientAddr>,
    body: Bytes,
) -> Response {
    let request: CarReplyRequest = match parse(&body) {
        Ok(request) => request,
        Err(message) => {
            let speech = "I couldn't send that. Please use your phone.";
            let reply = refusal(speech, RefusalReason::InvalidAction, message);
            return respond(&reply, true);
        }
    };
    let Ok(agent) = id.parse::<AgentId>() else {
        let reply = refusal(SPEECH_GONE, RefusalReason::UnknownAgent, "No such agent.");
        return respond(&reply, true);
    };
    let call = CarCall { agent, request };
    let actions = Arc::clone(&state.actions);
    let budget = Some(Budget::of(&principal, client));
    // The action it becomes reuses the key: one `POST /v1/actions` already
    // used is not a new car reply.
    let taken = actions.ledger.holds(&call.request.idempotency_key);
    run_once(
        &actions.limit,
        &actions.car,
        taken,
        call,
        budget,
        |call| async move { answer(&state, &principal.device, &call).await },
    )
    .await
}

/// Maps and runs one car reply: the reply, and whether it is final for
/// the key (false: herdr or the audit log was unavailable, nothing sent).
async fn answer(state: &AppState, device: &str, call: &CarCall) -> (CarReply, bool) {
    let utterance = &call.request.utterance;
    // An ambiguous utterance may still be meant for the agent itself
    // (`answering`): decided once the agent's state is read.
    let interpreted = match CarIntent::interpret(utterance) {
        Err(UtteranceRefusal::Empty) => {
            return refused_aloud(&call.agent, UtteranceRefusal::Empty);
        }
        interpreted => interpreted,
    };
    let (agent, dialog) = match read_current(state, &call.agent).await {
        Ok(current) => current,
        Err(outcome) => return spoken(None, *outcome, None),
    };
    let judged = dialog.as_ref().map(Dialog::policy_input);
    let named = call.request.pending_id.as_ref();
    let action = |kind| Action {
        idempotency_key: call.request.idempotency_key.clone(),
        agent_id: call.agent.clone(),
        expected_seq: Seq(agent.state_change_seq),
        created_at: crate::registry::now(),
        kind,
        origin: Origin::Car,
    };
    if let Some(named) = named.filter(|id| judged.as_ref().is_none_or(|pending| &pending.id != *id))
    {
        let wire = dialog.map(|dialog| dialog.input);
        let outcome = Outcome::decided(stale(&state.hub, &agent, wire));
        // Audited like a stale `POST /v1/actions`: what it would have sent.
        if let Some(kind) = interpreted
            .as_ref()
            .ok()
            .and_then(|intent| intended_kind(intent, named))
        {
            audit_result(state, device, &action(kind), &outcome.result).await;
        }
        return spoken(interpreted.as_ref().ok(), outcome, None);
    }
    let intent = match answering(interpreted, utterance, named, judged.as_ref()) {
        Ok(intent) => intent,
        Err(refused) => return refused_aloud(&call.agent, refused),
    };
    let kind = match action_kind(&intent, named, judged.as_ref()) {
        Ok(kind) => kind,
        Err(refused) => return refused_aloud(&call.agent, refused),
    };
    let action = action(kind);
    let summary = judged.as_ref().and_then(summary);
    // The stale guard re-checks this same read; a `choose` is re-read once
    // more after its audit entry, as for any action.
    let outcome = decide_and_execute_on(state, device, &action, agent, dialog, judged).await;
    audit_result(state, device, &action, &outcome.result).await;
    spoken(Some(&intent), outcome, summary)
}

/// What the utterance asks for in the state the car reply found. With no
/// dialog open and none named (the answer to an announced *Decide* line,
/// which the agent asked in its own words), a yes, a no, or anything
/// ambiguous about them ("yes, merge it") goes to the agent as said, as a
/// prompt. Otherwise it stays what [`CarIntent::interpret`] said.
fn answering(
    interpreted: Result<CarIntent, UtteranceRefusal>,
    utterance: &Utterance,
    named: Option<&PendingId>,
    pending: Option<&PendingInput>,
) -> Result<CarIntent, UtteranceRefusal> {
    let to_the_agent = named.is_none() && pending.is_none();
    match interpreted {
        Ok(CarIntent::Allow | CarIntent::Deny { .. }) | Err(UtteranceRefusal::Ambiguous)
            if to_the_agent =>
        {
            PromptText::new(utterance.as_str().trim())
                .map(CarIntent::Prompt)
                .map_err(|_| UtteranceRefusal::Empty)
        }
        interpreted => interpreted,
    }
}

/// The action a car reply meant for the dialog `named`, for the audit of
/// a reply refused as stale before it became one.
fn intended_kind(intent: &CarIntent, named: &PendingId) -> Option<ActionKind> {
    let choose = |option: &str, feedback: Option<&FeedbackText>| {
        Some(ActionKind::Choose(ChoosePayload {
            pending_id: named.clone(),
            option_id: OptionId::new(option).ok()?,
            feedback_text: feedback.cloned(),
        }))
    };
    match intent {
        CarIntent::Allow => choose("yes", None),
        CarIntent::Deny { feedback: None } => choose("no", None),
        CarIntent::Deny { feedback } => choose("no_feedback", feedback.as_ref()),
        CarIntent::Prompt(text) => Some(ActionKind::Prompt(PromptPayload::text(text.clone()))),
    }
}

/// A final car-policy refusal before any action exists (not understood,
/// or nothing the car may do; nothing was sent, so nothing is audited): logged without the utterance, with
/// `refused` as the sentence read aloud.
fn refused_aloud(
    agent: &AgentId,
    refused: impl std::fmt::Display + std::fmt::Debug,
) -> (CarReply, bool) {
    tracing::info!(%agent, reason = ?refused, "car reply refused");
    let speech = refused.to_string();
    (
        refusal(&speech, RefusalReason::CarPolicy, speech.clone()),
        true,
    )
}

/// The action `intent` means against `pending` (as policy sees it), or
/// why the car cannot do it. Only ever a prompt, or a
/// `choose` of a low-risk `yes`, `no` or `no_feedback` option.
fn action_kind(
    intent: &CarIntent,
    named: Option<&PendingId>,
    pending: Option<&PendingInput>,
) -> Result<ActionKind, NotFromTheCar> {
    let feedback = match intent {
        CarIntent::Prompt(text) => {
            if pending.is_some_and(PendingInput::is_dialog) {
                return Err(NotFromTheCar::AnswerFirst);
            }
            return Ok(ActionKind::Prompt(PromptPayload::text(text.clone())));
        }
        CarIntent::Allow => None,
        CarIntent::Deny { feedback } => feedback.as_ref(),
    };
    let pending = pending.ok_or(NotFromTheCar::NotAsking)?;
    // The caller checked that a named id is the current one.
    let pending_id = named.ok_or(NotFromTheCar::Which)?;
    if pending.source != PendingSource::Hook {
        return Err(NotFromTheCar::Unreliable);
    }
    let option = |id: &str| {
        let id = OptionId::new(id).ok()?;
        pending
            .options
            .get(&id)
            .filter(|option| option.risk == Risk::Low)
    };
    let chosen: Option<&PendingOption> = match (intent, feedback) {
        (CarIntent::Allow, _) => option("yes"),
        (_, Some(_)) => {
            let with_note = option("no_feedback").filter(|option| option.accepts_feedback);
            if with_note.is_none() && option("no").is_some() {
                return Err(NotFromTheCar::NoNote);
            }
            with_note
        }
        (_, None) => option("no").or_else(|| option("no_feedback")),
    };
    let chosen = chosen.ok_or(NotFromTheCar::NeedsPhone)?;
    Ok(ActionKind::Choose(ChoosePayload {
        pending_id: pending_id.clone(),
        option_id: chosen.id.clone(),
        feedback_text: feedback.cloned(),
    }))
}

/// The reply for `outcome`, with the sentence to read aloud. `summary` is
/// what was allowed, read back after a sent Allow. `intent` is `None` when
/// the reply ended before one was settled (only stale, gone or
/// unavailable outcomes then).
fn spoken(
    intent: Option<&CarIntent>,
    outcome: Outcome,
    summary: Option<String>,
) -> (CarReply, bool) {
    let speech = if outcome.decided {
        speech(intent, &outcome.result, summary)
    } else {
        SPEECH_UNREACHABLE.to_owned()
    };
    (
        CarReply {
            speech,
            result: outcome.result,
        },
        outcome.decided,
    )
}

fn speech(intent: Option<&CarIntent>, result: &ActionResult, summary: Option<String>) -> String {
    match result {
        ActionResult::Sent { .. } => match intent {
            Some(CarIntent::Allow) => summary.map_or_else(
                || "Allowed.".to_owned(),
                |summary| format!("Allowed: {summary}"),
            ),
            Some(CarIntent::Deny { feedback: None }) => "Denied.".to_owned(),
            Some(CarIntent::Deny { feedback: Some(_) }) => "Denied, and sent your note.".to_owned(),
            Some(CarIntent::Prompt(_)) | None => "Sent.".to_owned(),
        },
        ActionResult::Stale { .. } => SPEECH_MOVED_ON.to_owned(),
        // These messages come from the car policy and are written to be
        // read aloud.
        ActionResult::Refused {
            reason:
                RefusalReason::CarPolicy
                | RefusalReason::NoPendingInput
                | RefusalReason::UnknownOption
                | RefusalReason::FeedbackNotAccepted,
            message,
        } => message.clone(),
        ActionResult::Refused {
            reason: RefusalReason::UnknownAgent,
            ..
        } => SPEECH_GONE.to_owned(),
        ActionResult::Refused {
            reason: RefusalReason::RateLimited,
            ..
        } => SPEECH_BUSY.to_owned(),
        // Denied, only the note was lost; anything else failed before the
        // dialog was answered, so nothing was denied.
        ActionResult::Failed { .. } if only_feedback_lost(result) => {
            "Denied, but your note didn't reach the agent. Please check your phone.".to_owned()
        }
        ActionResult::Failed { .. } => {
            "That didn't go through. Please check your phone.".to_owned()
        }
        ActionResult::Refused { .. } | ActionResult::Unknown => SPEECH_NEEDS_PHONE.to_owned(),
    }
}

/// What a dialog asks to allow, short enough to read back: the command of
/// a ``Run `…`?`` prompt, otherwise the prompt without backticks or its
/// question mark, on one clean line of at most [`MAX_SUMMARY_CHARS`]
/// ([`one_line`], as for notices).
fn summary(pending: &PendingInput) -> Option<String> {
    let prompt = pending.prompt.trim();
    let inner = prompt
        .strip_prefix("Run `")
        .and_then(|rest| rest.strip_suffix("`?"))
        .unwrap_or(prompt);
    let plain: String = inner.chars().filter(|&c| c != '`').collect();
    one_line(plain.trim().trim_end_matches('?'), MAX_SUMMARY_CHARS)
}

fn refusal(speech: &str, reason: RefusalReason, message: impl Into<String>) -> CarReply {
    CarReply {
        speech: speech.to_owned(),
        result: refused(reason, message),
    }
}

fn respond(reply: &CarReply, decided: bool) -> Response {
    respond_with(reply, &reply.result, decided)
}

#[cfg(test)]
mod tests {
    use protocol::{FeedbackText, PendingKind, PendingOptions, PromptText, Utterance};

    use super::*;

    const ID: &str = "screen-4-0a1b2c3d4e5f";

    fn dialog(source: PendingSource, options: &[(&str, Risk, bool)]) -> PendingInput {
        PendingInput {
            id: PendingId::new(ID).expect("id"),
            kind: PendingKind::Permission,
            prompt: "Run `rm -rf build`?".to_owned(),
            detail: None,
            options: PendingOptions::new(
                options
                    .iter()
                    .map(|&(id, risk, accepts_feedback)| PendingOption {
                        id: OptionId::new(id).expect("id"),
                        label: id.to_owned(),
                        risk,
                        accepts_feedback,
                    })
                    .collect(),
            )
            .expect("options"),
            suggestions: Vec::new(),
            source,
        }
    }

    fn claude(source: PendingSource) -> PendingInput {
        dialog(
            source,
            &[
                ("yes", Risk::Low, false),
                ("yes_always", Risk::Elevated, false),
                ("no_feedback", Risk::Low, true),
            ],
        )
    }

    fn screen() -> PendingSource {
        PendingSource::Screen {
            confidence: protocol::Confidence::new(100).expect("confidence"),
            screen_tail: protocol::ScreenTail::default(),
        }
    }

    fn said(text: &str) -> CarIntent {
        CarIntent::interpret(&Utterance::new(text).expect("utterance")).expect("understood")
    }

    fn chosen(kind: &Result<ActionKind, NotFromTheCar>) -> Option<(&str, Option<&str>)> {
        match kind {
            Ok(ActionKind::Choose(choose)) => Some((
                choose.option_id.as_str(),
                choose.feedback_text.as_ref().map(FeedbackText::as_str),
            )),
            _ => None,
        }
    }

    #[test]
    fn allow_and_deny_choose_low_risk_options_of_a_hook_dialog() {
        let hook = claude(PendingSource::Hook);
        let id = PendingId::new(ID).expect("id");
        let named = Some(&id);
        let p = Some(&hook);
        assert_eq!(
            chosen(&action_kind(&said("yes"), named, p)),
            Some(("yes", None))
        );
        assert_eq!(
            chosen(&action_kind(&said("no"), named, p)),
            Some(("no_feedback", None))
        );
        assert_eq!(
            chosen(&action_kind(&said("no, use pnpm"), named, p)),
            Some(("no_feedback", Some("use pnpm")))
        );
        // Codex-style: a plain `no` wins when there is no message.
        let plain = dialog(
            PendingSource::Hook,
            &[("yes", Risk::Low, false), ("no", Risk::Low, false)],
        );
        assert_eq!(
            chosen(&action_kind(&said("deny"), named, Some(&plain))),
            Some(("no", None))
        );
        assert_eq!(
            action_kind(&said("no, use pnpm"), named, Some(&plain)).err(),
            Some(NotFromTheCar::NoNote)
        );
    }

    #[test]
    fn elevated_screen_and_unnamed_dialogs_are_refused() {
        let id = PendingId::new(ID).expect("id");
        let hook = claude(PendingSource::Hook);
        assert_eq!(
            action_kind(&said("yes"), None, Some(&hook)).err(),
            Some(NotFromTheCar::Which),
            "Allow must name the dialog it read"
        );
        assert_eq!(
            action_kind(&said("yes"), Some(&id), Some(&claude(screen()))).err(),
            Some(NotFromTheCar::Unreliable)
        );
        assert_eq!(
            action_kind(
                &said("yes"),
                Some(&id),
                Some(&claude(PendingSource::Unknown))
            )
            .err(),
            Some(NotFromTheCar::Unreliable)
        );
        // Only an elevated allow on offer: never chosen.
        let elevated = dialog(
            PendingSource::Hook,
            &[
                ("yes_always", Risk::Elevated, false),
                ("no", Risk::Low, false),
            ],
        );
        assert_eq!(
            action_kind(&said("approve"), Some(&id), Some(&elevated)).err(),
            Some(NotFromTheCar::NeedsPhone)
        );
        let risky_yes = dialog(PendingSource::Hook, &[("yes", Risk::Elevated, false)]);
        assert_eq!(
            action_kind(&said("allow"), Some(&id), Some(&risky_yes)).err(),
            Some(NotFromTheCar::NeedsPhone)
        );
        assert_eq!(
            action_kind(&said("yes"), Some(&id), None).err(),
            Some(NotFromTheCar::NotAsking)
        );
    }

    #[test]
    fn a_yes_or_no_with_nothing_open_is_said_to_the_agent() {
        let heard = |text: &str| {
            let utterance = Utterance::new(text).expect("utterance");
            (CarIntent::interpret(&utterance), utterance)
        };
        let to_agent = |text: &str| {
            let (interpreted, utterance) = heard(text);
            match answering(interpreted, &utterance, None, None) {
                Ok(CarIntent::Prompt(prompt)) => Some(prompt.as_str().to_owned()),
                _ => None,
            }
        };
        // Nothing open, nothing named: the agent asked in its own words,
        // including what `interpret` alone finds ambiguous.
        for said in ["Yes.", "yes, merge it", "no", "No problem, go ahead"] {
            assert_eq!(to_agent(said).as_deref(), Some(said), "{said:?}");
        }
        // A dialog open, or one named: still an answer to it, or refused.
        let hook = claude(PendingSource::Hook);
        let id = PendingId::new(ID).expect("id");
        let (interpreted, utterance) = heard("yes, merge it");
        assert_eq!(
            answering(interpreted, &utterance, None, Some(&hook)),
            Err(UtteranceRefusal::Ambiguous)
        );
        let (interpreted, utterance) = heard("yes");
        assert_eq!(
            answering(interpreted, &utterance, Some(&id), None),
            Ok(CarIntent::Allow)
        );
    }

    #[test]
    fn refusals_are_spoken_sentences() {
        assert_eq!(
            NotFromTheCar::Unreliable.to_string(),
            CarRefusal::UnreliableSource.to_string()
        );
        assert_eq!(NotFromTheCar::NeedsPhone.to_string(), SPEECH_NEEDS_PHONE);
    }

    #[test]
    fn prompts_go_only_while_no_dialog_is_open() {
        let hook = claude(PendingSource::Hook);
        assert_eq!(
            action_kind(&said("carry on"), None, Some(&hook)).err(),
            Some(NotFromTheCar::AnswerFirst)
        );
        let Ok(ActionKind::Prompt(prompt)) = action_kind(&said("carry on"), None, None) else {
            panic!("a prompt")
        };
        assert_eq!(prompt.text, PromptText::new("carry on").expect("text"));
        assert!(prompt.attachments.is_empty(), "never attachments");
        let question = PendingInput {
            kind: PendingKind::Question,
            ..dialog(PendingSource::Hook, &[])
        };
        assert!(matches!(
            action_kind(&said("postgres"), None, Some(&question)),
            Ok(ActionKind::Prompt(_))
        ));
    }

    #[test]
    fn the_spoken_result_is_short() {
        let hook = claude(PendingSource::Hook);
        let sent = ActionResult::Sent { replayed: false };
        assert_eq!(
            speech(Some(&said("yes")), &sent, summary(&hook)),
            "Allowed: rm -rf build"
        );
        assert_eq!(speech(Some(&said("carry on")), &sent, None), "Sent.");
        assert_eq!(speech(Some(&said("no")), &sent, None), "Denied.");
        assert_eq!(
            speech(Some(&said("no, wait")), &sent, None),
            "Denied, and sent your note."
        );
        let stale = ActionResult::Stale {
            current: None,
            pending: None,
        };
        assert_eq!(speech(Some(&said("yes")), &stale, None), SPEECH_MOVED_ON);
        let step_up = refused(RefusalReason::StepUpRequired, "elevated");
        assert_eq!(
            speech(Some(&said("yes")), &step_up, None),
            SPEECH_NEEDS_PHONE
        );
        let car = refused(
            RefusalReason::CarPolicy,
            "Commands like that need your phone.",
        );
        assert_eq!(
            speech(Some(&said("/clear")), &car, None),
            "Commands like that need your phone."
        );
        let lost = ActionResult::Failed {
            message: format!("{} the agent did not return.", super::super::OPTION_CHOSEN),
        };
        assert_eq!(
            speech(Some(&said("no, use pnpm")), &lost, None),
            "Denied, but your note didn't reach the agent. Please check your phone."
        );
        let not_sent = ActionResult::Failed {
            message: "herdr: connection refused".into(),
        };
        assert_eq!(
            speech(Some(&said("no, use pnpm")), &not_sent, None),
            "That didn't go through. Please check your phone.",
            "the deny key itself failed: nothing was denied"
        );
        let (reply, decided) = spoken(
            Some(&said("yes")),
            Outcome::undecided(ActionResult::Failed {
                message: "herdr".into(),
            }),
            None,
        );
        assert!(!decided);
        assert_eq!(reply.speech, SPEECH_UNREACHABLE);
    }

    #[test]
    fn summaries_name_the_command_and_stay_short() {
        let mut pending = claude(PendingSource::Hook);
        assert_eq!(summary(&pending).as_deref(), Some("rm -rf build"));
        pending.prompt = "Approve the plan?".to_owned();
        assert_eq!(summary(&pending).as_deref(), Some("Approve the plan"));
        pending.prompt = format!("Run `{}`?", "x".repeat(200));
        let long = summary(&pending).expect("summary");
        assert_eq!(long.chars().count(), MAX_SUMMARY_CHARS);
        assert!(long.ends_with('…'));
        pending.prompt = "  ".to_owned();
        assert_eq!(summary(&pending), None);
        pending.prompt = "Run `rm\u{202e} -rf\nbuild`?".to_owned();
        assert_eq!(
            summary(&pending).as_deref(),
            Some("rm -rf build"),
            "cleaned like a notice"
        );
    }
}
