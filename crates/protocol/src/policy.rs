//! Action policy: whether an action can apply at all, which actions need
//! biometric step-up (design §8), and which are allowed from the car (design
//! §6.6). All are pure functions over one shared classification, so the
//! bridge enforces them and the app can pre-check them with the same code.
//!
//! # Order of checks on the bridge
//!
//! 1. **Stale guard** (HTTP 409): `agentId`, `expectedSeq` and `pendingId`
//!    (see [`Action::pending_is_current`]) must match the current state.
//! 2. [`check_answerable`]: a `choose` must name an option of the current
//!    pending input, and may carry `feedbackText` only if that option
//!    [accepts feedback](crate::PendingOption::accepts_feedback). Failure is
//!    a **permanent** refusal ([`Unanswerable::refusal_reason`]), never
//!    step-up. After step 1 it only fires when the client sent something the
//!    current input never offered.
//! 3. [`check_car_policy`] for `origin = car`.
//! 4. [`requires_step_up`] for every other origin; step-up proof travels
//!    out-of-band (see [`crate::action`]).
//!
//! `pending` is always the agent's *current* pending input. When herdr
//! reports the agent `blocked` but no structured input is known, the bridge
//! passes [`PendingInput::unparsed_dialog`]: for policy purposes that is a
//! pending input with options (a dialog), so prompts need step-up and nothing
//! can be chosen.
//!
//! # Step-up
//!
//! Required for: raw `keys` (they can select any option, or interrupt);
//! `interrupt`; `choose` of an elevated option, or with elevated feedback;
//! elevated prompts (a `!` shell, `/` slash command or `#` memory prefix; see
//! [`PromptText::is_elevated`](crate::PromptText::is_elevated)); and any
//! prompt while a [dialog](PendingInput::is_dialog) is open, since the text
//! would be typed into it. Nothing else.
//!
//! # Car
//!
//! The car refuses everything unanswerable or requiring step-up, plus one
//! car-only rule: while the pending input did not come from a hook (screen
//! or unknown source), both `choose` and prompts are refused, whatever the
//! input's kind or options. Screen parsing is too unreliable to act on
//! unseen: a misparsed permission dialog must not receive a spoken "2".
//! Photos never go from the car: a prompt with attachments is refused
//! there (nobody can look at a picture while driving, and an upload that
//! claims the car origin is suspect).
//! What remains: *Allow once*, *Deny* + feedback on hook input, and plain
//! prompts when nothing is pending or a hook-sourced question or free-text
//! request without options is. These are the safe defaults; a bridge setting
//! may only tighten them.

use core::fmt;

use crate::action::{Action, ActionKind, Origin};
use crate::pending::{PendingInput, PendingSource, Risk};
use crate::result::RefusalReason;
use crate::text::FeedbackText;

/// Why a `choose` cannot apply to the current state. Permanent: the same
/// action will never succeed, with or without step-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unanswerable {
    /// The agent has no pending input.
    NoPendingInput,
    /// The chosen option is not offered by the pending input.
    UnknownOption,
    /// `feedbackText` on an option that does not accept feedback.
    FeedbackNotAccepted,
}

impl Unanswerable {
    /// The refusal reason to return.
    #[must_use]
    pub const fn refusal_reason(self) -> RefusalReason {
        match self {
            Self::NoPendingInput => RefusalReason::NoPendingInput,
            Self::UnknownOption => RefusalReason::UnknownOption,
            Self::FeedbackNotAccepted => RefusalReason::FeedbackNotAccepted,
        }
    }
}

/// Checks that `action` can apply to the current pending input: a `choose`
/// must name one of its options, and carry feedback only if that option
/// accepts it. Other verbs always pass.
pub fn check_answerable(
    action: &Action,
    pending: Option<&PendingInput>,
) -> Result<(), Unanswerable> {
    let ActionKind::Choose(choose) = &action.kind else {
        return Ok(());
    };
    let pending = pending.ok_or(Unanswerable::NoPendingInput)?;
    let option = pending
        .options
        .get(&choose.option_id)
        .ok_or(Unanswerable::UnknownOption)?;
    if choose.feedback_text.is_some() && !option.accepts_feedback {
        return Err(Unanswerable::FeedbackNotAccepted);
    }
    Ok(())
}

/// Why an action requires biometric step-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepUpReason {
    /// Raw keys can select any option, including elevated ones, or interrupt.
    RawKeys,
    /// Interrupting a turn.
    Interrupt,
    /// The chosen option is elevated (e.g. *Always allow*).
    ElevatedOption,
    /// A prompt with a `!`, `/` or `#` command prefix.
    ElevatedPrompt,
    /// Feedback with a `!`, `/` or `#` command prefix.
    ElevatedFeedback,
    /// A prompt while a dialog is open: the text would be typed into it.
    PromptIntoDialog,
}

/// Classifies `action` against the agent's current pending input: `Some`
/// with the reason if it requires step-up, whatever its origin. Assumes
/// [`check_answerable`] passed (an unanswerable `choose` is classified only
/// by its feedback).
#[must_use]
pub fn step_up_reason(action: &Action, pending: Option<&PendingInput>) -> Option<StepUpReason> {
    match &action.kind {
        ActionKind::Prompt(prompt) => {
            if prompt.text.is_elevated() {
                Some(StepUpReason::ElevatedPrompt)
            } else if pending.is_some_and(PendingInput::is_dialog) {
                Some(StepUpReason::PromptIntoDialog)
            } else {
                None
            }
        }
        ActionKind::Keys(_) => Some(StepUpReason::RawKeys),
        ActionKind::Interrupt(_) => Some(StepUpReason::Interrupt),
        ActionKind::Choose(choose) => {
            let elevated_option = pending
                .and_then(|pending| pending.options.get(&choose.option_id))
                .is_some_and(|option| option.risk == Risk::Elevated);
            if elevated_option {
                Some(StepUpReason::ElevatedOption)
            } else if choose
                .feedback_text
                .as_ref()
                .is_some_and(FeedbackText::is_elevated)
            {
                Some(StepUpReason::ElevatedFeedback)
            } else {
                None
            }
        }
    }
}

/// Whether `action` requires biometric step-up (design §8), whatever its
/// origin. See [`step_up_reason`] for why.
#[must_use]
pub fn requires_step_up(action: &Action, pending: Option<&PendingInput>) -> bool {
    step_up_reason(action, pending).is_some()
}

/// Why the car policy refused an action. Return
/// [`CarRefusal::refusal_reason`] with this value's `Display` text as the
/// message; it is read aloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CarRefusal {
    /// The action cannot apply at all (permanent, not a car rule).
    Unanswerable(Unanswerable),
    /// The action requires step-up, which the car cannot do.
    StepUp(StepUpReason),
    /// `choose` or a prompt while the pending input did not come from a hook.
    UnreliableSource,
    /// A prompt carrying attachments.
    Attachments,
}

impl CarRefusal {
    /// The refusal reason to return: the unanswerable reason, or
    /// [`RefusalReason::CarPolicy`].
    #[must_use]
    pub const fn refusal_reason(self) -> RefusalReason {
        match self {
            Self::Unanswerable(unanswerable) => unanswerable.refusal_reason(),
            Self::StepUp(_) | Self::UnreliableSource | Self::Attachments => {
                RefusalReason::CarPolicy
            }
        }
    }
}

impl fmt::Display for CarRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unanswerable(Unanswerable::NoPendingInput) => {
                "The agent isn't asking anything right now, so there's nothing to answer."
            }
            Self::Unanswerable(Unanswerable::UnknownOption) => {
                "That isn't one of the options. Please check your phone."
            }
            Self::Unanswerable(Unanswerable::FeedbackNotAccepted) => {
                "That option doesn't take a message. Please choose it without one."
            }
            Self::StepUp(StepUpReason::RawKeys) => {
                "I can't send keys from the car. Please use your phone."
            }
            Self::StepUp(StepUpReason::Interrupt) => {
                "I can't interrupt an agent from the car. Please use your phone."
            }
            Self::StepUp(StepUpReason::ElevatedOption) => {
                "That option needs your phone. Please confirm it there."
            }
            Self::StepUp(StepUpReason::ElevatedPrompt | StepUpReason::ElevatedFeedback) => {
                "Commands like that need your phone. Please send it from there."
            }
            Self::StepUp(StepUpReason::PromptIntoDialog) => {
                "The agent is waiting for you to pick an option. Please choose one first."
            }
            Self::UnreliableSource => {
                "I'm not sure I read that question right. Please answer it on your phone."
            }
            Self::Attachments => "I can't send photos from the car. Please use your phone.",
        })
    }
}

impl core::error::Error for CarRefusal {}

/// Decides whether `action` is permitted under the car policy.
///
/// Actions from other origins always pass (they are subject to
/// [`check_answerable`] and [`requires_step_up`] instead). Run the stale
/// guard first; see the [module docs](self).
pub fn check_car_policy(action: &Action, pending: Option<&PendingInput>) -> Result<(), CarRefusal> {
    if action.origin != Origin::Car {
        return Ok(());
    }
    check_answerable(action, pending).map_err(CarRefusal::Unanswerable)?;
    if let ActionKind::Prompt(prompt) = &action.kind
        && !prompt.attachments.is_empty()
    {
        return Err(CarRefusal::Attachments);
    }
    if let Some(reason) = step_up_reason(action, pending) {
        return Err(CarRefusal::StepUp(reason));
    }
    let unreliable = pending.is_some_and(|pending| pending.source != PendingSource::Hook);
    match &action.kind {
        ActionKind::Choose(_) | ActionKind::Prompt(_) if unreliable => {
            Err(CarRefusal::UnreliableSource)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{ChoosePayload, InterruptPayload, KeysPayload, PromptPayload};
    use crate::ids::{AgentId, IdempotencyKey, OptionId, PendingId, Seq};
    use crate::keys::{AllowedKey, KeySequence};
    use crate::pending::{
        Confidence, PendingKind, PendingOption, PendingOptions, PendingSource, ScreenTail,
    };
    use crate::text::{FeedbackText, PromptText};
    use crate::time::Timestamp;

    const PENDING: &str = "p1";

    fn screen() -> PendingSource {
        PendingSource::Screen {
            confidence: Confidence::new(95).unwrap(),
            screen_tail: ScreenTail::default(),
        }
    }

    fn pending(kind: PendingKind, with_options: bool, source: PendingSource) -> PendingInput {
        let option = |id: &str, risk| PendingOption {
            id: OptionId::new(id).unwrap(),
            label: id.into(),
            risk,
            accepts_feedback: id == "deny",
        };
        let options = if with_options {
            vec![
                option("allow_once", Risk::Low),
                option("always_allow", Risk::Elevated),
                option("deny", Risk::Low),
            ]
        } else {
            vec![]
        };
        PendingInput {
            id: PendingId::new(PENDING).unwrap(),
            kind,
            prompt: "Run `pnpm test`?".into(),
            detail: None,
            options: PendingOptions::new(options).unwrap(),
            suggestions: vec![],
            source,
        }
    }

    fn action(origin: Origin, kind: ActionKind) -> Action {
        Action {
            idempotency_key: IdempotencyKey::new("9ca09978-ef89-46fd-9a85-118b84bee7b4").unwrap(),
            agent_id: "w6:p1~abc".parse::<AgentId>().unwrap(),
            expected_seq: Seq(1),
            created_at: Timestamp::from_unix_millis(0),
            kind,
            origin,
        }
    }

    fn choose_in(pending_id: &str, id: &str, feedback: Option<&str>) -> ActionKind {
        ActionKind::Choose(ChoosePayload {
            pending_id: PendingId::new(pending_id).unwrap(),
            option_id: OptionId::new(id).unwrap(),
            feedback_text: feedback.map(|f| FeedbackText::new(f).unwrap()),
        })
    }

    fn choose(id: &str, feedback: Option<&str>) -> ActionKind {
        choose_in(PENDING, id, feedback)
    }

    fn prompt(text: &str) -> ActionKind {
        ActionKind::Prompt(PromptPayload::text(PromptText::new(text).unwrap()))
    }

    fn photo(text: &str) -> ActionKind {
        ActionKind::Prompt(PromptPayload {
            text: PromptText::new(text).unwrap(),
            attachments: crate::Attachments::new(vec![
                crate::AttachmentId::new("00000000-0000-4000-8000-0000000000a1").unwrap(),
            ])
            .unwrap(),
        })
    }

    fn keys() -> ActionKind {
        ActionKind::Keys(KeysPayload {
            pending_id: None,
            keys: KeySequence::new(vec![AllowedKey::Enter]).unwrap(),
        })
    }

    fn interrupt() -> ActionKind {
        ActionKind::Interrupt(InterruptPayload {})
    }

    /// One row: what each check must say about `kind` given `pending`.
    struct Case<'a> {
        name: &'a str,
        kind: ActionKind,
        pending: Option<&'a PendingInput>,
        answerable: Result<(), Unanswerable>,
        step_up: Option<StepUpReason>,
        car: Result<(), CarRefusal>,
    }

    fn row<'a>(
        name: &'a str,
        kind: ActionKind,
        pending: Option<&'a PendingInput>,
        answerable: Result<(), Unanswerable>,
        step_up: Option<StepUpReason>,
        car: Result<(), CarRefusal>,
    ) -> Case<'a> {
        Case {
            name,
            kind,
            pending,
            answerable,
            step_up,
            car,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::many_single_char_names)] // a table: one row per case, short names for the pending inputs
    fn policy_table() {
        use CarRefusal::{StepUp, UnreliableSource};
        use StepUpReason::{
            ElevatedFeedback, ElevatedOption, ElevatedPrompt, Interrupt, PromptIntoDialog, RawKeys,
        };
        use Unanswerable::{FeedbackNotAccepted, NoPendingInput, UnknownOption};
        let car_unanswerable = CarRefusal::Unanswerable;

        let hook = pending(PendingKind::Permission, true, PendingSource::Hook);
        let screen_dialog = pending(PendingKind::Permission, true, screen());
        let unknown_source = pending(PendingKind::Permission, true, PendingSource::Unknown);
        let question = pending(PendingKind::Question, false, PendingSource::Hook);
        let free_text = pending(PendingKind::FreeText, false, screen());
        let question_with_options = pending(PendingKind::Question, true, PendingSource::Hook);
        let screen_question = pending(PendingKind::Question, false, screen());
        let unknown_question = pending(PendingKind::Question, false, PendingSource::Unknown);
        let (sq, uq) = (Some(&screen_question), Some(&unknown_question));
        let blocked = PendingInput::unparsed_dialog(PendingId::new(PENDING).unwrap());
        let (h, s, u) = (Some(&hook), Some(&screen_dialog), Some(&unknown_source));
        let (q, f, qo, b) = (
            Some(&question),
            Some(&free_text),
            Some(&question_with_options),
            Some(&blocked),
        );

        let cases = vec![
            // Prompts: elevated prefix (rule 1), into a dialog (N2), answering
            // a question without options (N3).
            row(
                "plain prompt, idle",
                prompt("carry on"),
                None,
                Ok(()),
                None,
                Ok(()),
            ),
            row(
                "photo prompt, idle: the app only",
                photo("see this"),
                None,
                Ok(()),
                None,
                Err(CarRefusal::Attachments),
            ),
            row(
                "prompt into hook dialog",
                prompt("2"),
                h,
                Ok(()),
                Some(PromptIntoDialog),
                Err(StepUp(PromptIntoDialog)),
            ),
            row(
                "prompt into screen dialog",
                prompt("2"),
                s,
                Ok(()),
                Some(PromptIntoDialog),
                Err(StepUp(PromptIntoDialog)),
            ),
            row(
                "prompt into question with options",
                prompt("yes"),
                qo,
                Ok(()),
                Some(PromptIntoDialog),
                Err(StepUp(PromptIntoDialog)),
            ),
            row(
                "prompt while blocked, unparsed",
                prompt("2"),
                b,
                Ok(()),
                Some(PromptIntoDialog),
                Err(StepUp(PromptIntoDialog)),
            ),
            row(
                "prompt answers question",
                prompt("yes, open it"),
                q,
                Ok(()),
                None,
                Ok(()),
            ),
            row(
                "prompt answers screen free text",
                prompt("use nextest"),
                f,
                Ok(()),
                None,
                Err(UnreliableSource),
            ),
            row(
                "prompt answers misparsed screen question",
                prompt("2"),
                sq,
                Ok(()),
                None,
                Err(UnreliableSource),
            ),
            row(
                "prompt answers unknown-source question",
                prompt("yes"),
                uq,
                Ok(()),
                None,
                Err(UnreliableSource),
            ),
            row(
                "emoji-selector-hidden slash prompt",
                prompt("\u{200D}/clear"),
                None,
                Ok(()),
                Some(ElevatedPrompt),
                Err(StepUp(ElevatedPrompt)),
            ),
            row(
                "punctuation-hidden shell prompt",
                prompt("-- !ls"),
                q,
                Ok(()),
                Some(ElevatedPrompt),
                Err(StepUp(ElevatedPrompt)),
            ),
            row(
                "shell prompt",
                prompt("!rm -rf x"),
                None,
                Ok(()),
                Some(ElevatedPrompt),
                Err(StepUp(ElevatedPrompt)),
            ),
            row(
                "slash prompt",
                prompt("  /clear"),
                None,
                Ok(()),
                Some(ElevatedPrompt),
                Err(StepUp(ElevatedPrompt)),
            ),
            row(
                "memory prompt",
                prompt("# note"),
                None,
                Ok(()),
                Some(ElevatedPrompt),
                Err(StepUp(ElevatedPrompt)),
            ),
            row(
                "joiner-hidden shell prompt",
                prompt("\u{200D}!ls"),
                None,
                Ok(()),
                Some(ElevatedPrompt),
                Err(StepUp(ElevatedPrompt)),
            ),
            row(
                "shell prompt answering question",
                prompt("!ls"),
                q,
                Ok(()),
                Some(ElevatedPrompt),
                Err(StepUp(ElevatedPrompt)),
            ),
            row(
                "shell prompt into dialog",
                prompt("!ls"),
                h,
                Ok(()),
                Some(ElevatedPrompt),
                Err(StepUp(ElevatedPrompt)),
            ),
            // Choose: risk (rule 3), hook-only from the car (rule 5),
            // unanswerable is permanent and not step-up (N5), feedback only
            // where accepted and subject to the elevated rule.
            row(
                "feedback on allow once",
                choose("allow_once", Some("ok")),
                h,
                Err(FeedbackNotAccepted),
                None,
                Err(car_unanswerable(FeedbackNotAccepted)),
            ),
            row(
                "elevated feedback on deny",
                choose("deny", Some("/clear")),
                h,
                Ok(()),
                Some(ElevatedFeedback),
                Err(StepUp(ElevatedFeedback)),
            ),
            row(
                "hidden elevated feedback on deny",
                choose("deny", Some("\u{200D}!rm -rf x")),
                h,
                Ok(()),
                Some(ElevatedFeedback),
                Err(StepUp(ElevatedFeedback)),
            ),
            row(
                "allow once, hook",
                choose("allow_once", None),
                h,
                Ok(()),
                None,
                Ok(()),
            ),
            row(
                "deny + feedback, hook",
                choose("deny", Some("use nextest")),
                h,
                Ok(()),
                None,
                Ok(()),
            ),
            row(
                "always allow, hook",
                choose("always_allow", None),
                h,
                Ok(()),
                Some(ElevatedOption),
                Err(StepUp(ElevatedOption)),
            ),
            row(
                "unknown option",
                choose("yolo", None),
                h,
                Err(UnknownOption),
                None,
                Err(car_unanswerable(UnknownOption)),
            ),
            row(
                "nothing pending",
                choose("allow_once", None),
                None,
                Err(NoPendingInput),
                None,
                Err(car_unanswerable(NoPendingInput)),
            ),
            row(
                "choose while blocked, unparsed",
                choose("allow_once", None),
                b,
                Err(UnknownOption),
                None,
                Err(car_unanswerable(UnknownOption)),
            ),
            row(
                "allow once, screen",
                choose("allow_once", None),
                s,
                Ok(()),
                None,
                Err(UnreliableSource),
            ),
            row(
                "allow once, unknown source",
                choose("allow_once", None),
                u,
                Ok(()),
                None,
                Err(UnreliableSource),
            ),
            row(
                "always allow, screen",
                choose("always_allow", None),
                s,
                Ok(()),
                Some(ElevatedOption),
                Err(StepUp(ElevatedOption)),
            ),
            // Keys and interrupt (rule 3).
            row(
                "keys, idle",
                keys(),
                None,
                Ok(()),
                Some(RawKeys),
                Err(StepUp(RawKeys)),
            ),
            row(
                "keys, pending",
                keys(),
                h,
                Ok(()),
                Some(RawKeys),
                Err(StepUp(RawKeys)),
            ),
            row(
                "interrupt",
                interrupt(),
                None,
                Ok(()),
                Some(Interrupt),
                Err(StepUp(Interrupt)),
            ),
        ];

        for case in cases {
            let name = case.name;
            for origin in [Origin::App, Origin::Notification, Origin::Car] {
                let action = action(origin, case.kind.clone());
                assert_eq!(
                    check_answerable(&action, case.pending),
                    case.answerable,
                    "{name} from {origin:?}"
                );
                assert_eq!(
                    step_up_reason(&action, case.pending),
                    case.step_up,
                    "{name} from {origin:?}"
                );
                assert_eq!(
                    requires_step_up(&action, case.pending),
                    case.step_up.is_some(),
                    "{name} from {origin:?}"
                );
                let want = if origin == Origin::Car {
                    case.car
                } else {
                    Ok(())
                };
                assert_eq!(
                    check_car_policy(&action, case.pending),
                    want,
                    "{name} from {origin:?}"
                );
            }
        }
    }

    /// N1: a dialog replaced by another without `seq` moving is caught by the
    /// stale guard's `pendingId` check, before any policy runs.
    #[test]
    fn replaced_dialog_is_stale_not_a_policy_question() {
        let current = pending(PendingKind::Permission, true, PendingSource::Hook);
        let answered_old = action(Origin::Car, choose_in("p0", "allow_once", None));
        assert!(!answered_old.pending_is_current(Some(&current)));
        let answered_current = action(Origin::Car, choose("allow_once", None));
        assert!(answered_current.pending_is_current(Some(&current)));
        assert_eq!(check_car_policy(&answered_current, Some(&current)), Ok(()));
    }

    /// Review round 3: invisible characters before a command prefix. Each one
    /// is either rejected by `PromptText` (so no action can carry it) or
    /// accepted and classified elevated, never a plain prompt.
    #[test]
    fn hidden_command_prefixes_never_pass_as_plain_prompts() {
        let rejected = [
            "\u{E0100}!ls",
            "\u{17B4}/clear",
            "\u{2065}/clear",
            "\u{E0002}/clear",
            "\u{FE0F}/clear", // a variation selector must follow a visible character
        ];
        for text in rejected {
            assert!(PromptText::new(text).is_err(), "{text:?} must be rejected");
        }
        let accepted_but_elevated = ["\u{200D}/clear"];
        for text in accepted_but_elevated {
            for origin in [Origin::App, Origin::Notification, Origin::Car] {
                let action = action(origin, prompt(text));
                assert_eq!(
                    step_up_reason(&action, None),
                    Some(StepUpReason::ElevatedPrompt),
                    "{text:?}"
                );
            }
            assert_eq!(
                check_car_policy(&action(Origin::Car, prompt(text)), None),
                Err(CarRefusal::StepUp(StepUpReason::ElevatedPrompt))
            );
        }
    }

    #[test]
    fn refusal_reasons() {
        assert_eq!(
            Unanswerable::FeedbackNotAccepted.refusal_reason(),
            RefusalReason::FeedbackNotAccepted
        );
        assert!(!RefusalReason::FeedbackNotAccepted.is_transient());
        assert_eq!(
            Unanswerable::NoPendingInput.refusal_reason(),
            RefusalReason::NoPendingInput
        );
        assert_eq!(
            Unanswerable::UnknownOption.refusal_reason(),
            RefusalReason::UnknownOption
        );
        assert!(!RefusalReason::NoPendingInput.is_transient());
        assert!(!RefusalReason::UnknownOption.is_transient());
        assert_eq!(
            CarRefusal::Unanswerable(Unanswerable::UnknownOption).refusal_reason(),
            RefusalReason::UnknownOption
        );
        assert_eq!(
            CarRefusal::StepUp(StepUpReason::RawKeys).refusal_reason(),
            RefusalReason::CarPolicy
        );
        assert_eq!(
            CarRefusal::UnreliableSource.refusal_reason(),
            RefusalReason::CarPolicy
        );
        assert_eq!(
            CarRefusal::Attachments.refusal_reason(),
            RefusalReason::CarPolicy
        );
    }

    #[test]
    fn refusals_are_spoken_sentences() {
        let all = [
            CarRefusal::Unanswerable(Unanswerable::NoPendingInput),
            CarRefusal::Unanswerable(Unanswerable::UnknownOption),
            CarRefusal::StepUp(StepUpReason::RawKeys),
            CarRefusal::StepUp(StepUpReason::Interrupt),
            CarRefusal::StepUp(StepUpReason::ElevatedOption),
            CarRefusal::StepUp(StepUpReason::ElevatedPrompt),
            CarRefusal::StepUp(StepUpReason::PromptIntoDialog),
            CarRefusal::StepUp(StepUpReason::ElevatedFeedback),
            CarRefusal::Unanswerable(Unanswerable::FeedbackNotAccepted),
            CarRefusal::UnreliableSource,
            CarRefusal::Attachments,
        ];
        for refusal in all {
            let text = refusal.to_string();
            assert!(text.starts_with(|c: char| c.is_ascii_uppercase()), "{text}");
            assert!(text.ends_with('.'), "{text}");
            assert!(!text.contains(';'), "{text}");
        }
    }
}
