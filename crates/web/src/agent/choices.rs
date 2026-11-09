//! Option chips for a dialog (design §6.2): the pure rules, tested on the
//! host. The view is in [`super::reply_view`], the fetching in
//! [`super::pending`].
//!
//! Chips answer the [`PendingInput`] the reply bar *shows*, so they are
//! gated like the keypad: enabled only while that input is the latest one
//! for the agent's current seq ([`gate`]). A `choose` names the shown
//! input's id; if the dialog changed meanwhile, the bridge answers 409 and
//! the user looks again (never "Send anyway").

use protocol::{
    FeedbackText, OptionId, PendingId, PendingInput, PendingKind, PendingOption, PendingSource,
    PromptText, Risk, Seq, ValidationError,
};

use super::reply::Draft;
use super::screen::keypad_seq;

/// The pending input as last read for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PendingRead {
    /// What the bridge answered (`None`: nothing pending). After a failed
    /// read, the input last read, if any.
    pub pending: Option<PendingInput>,
    /// The agent's listed seq when the read started.
    pub seq: Option<Seq>,
    /// The agent's pending generation (one per `pending_changed` event)
    /// when the read started.
    pub generation: u64,
    /// The bridge gave no answer: whether `pending` is still asked is
    /// unknown.
    pub failed: bool,
}

/// Whether the chips may be used now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// The shown input is current: a `choose` carries this seq.
    Ready(Seq),
    /// The agent or its dialog changed since the read, or the read failed;
    /// a fresh one is on its way.
    Updating,
    /// The shown input was just answered from here; wait for the next one.
    Answered,
}

/// Whether the chips of `read` may be used, given the agent's `listed` seq,
/// its current pending `generation` and the input last answered from here.
/// A failed read is never current: what it shows is the input from before.
#[must_use]
pub fn gate(
    read: &PendingRead,
    listed: Option<Seq>,
    generation: u64,
    answered: Option<&PendingId>,
) -> Gate {
    if read.failed || read.generation != generation {
        return Gate::Updating;
    }
    let Some(seq) = keypad_seq(listed, read.seq) else {
        return Gate::Updating;
    };
    match (&read.pending, answered) {
        (Some(pending), Some(answered)) if pending.id == *answered => Gate::Answered,
        _ => Gate::Ready(seq),
    }
}

/// What keys answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyDialog {
    /// Not blocked: keys name no dialog.
    None,
    /// Blocked: keys name the shown pending input, if one is shown (else,
    /// e.g. an older bridge, they name none, as before).
    Shown(Option<PendingId>),
    /// Blocked, and the shown input is being re-read: keys wait.
    Rereading,
}

impl KeyDialog {
    /// The `pendingId` keys carry, or `None` (outer) while they must wait.
    #[must_use]
    pub fn pending_id(&self) -> Option<Option<&PendingId>> {
        match self {
            Self::None => Some(None),
            Self::Shown(id) => Some(id.as_ref()),
            Self::Rereading => None,
        }
    }
}

/// What keys answer, given whether the agent is `blocked`, the chips'
/// [`Gate`] and the shown read: while blocked, keys are gated on the shown
/// pending input being current, like the chips (an answered input still
/// takes keys: it is what the screen shows). A failed read while blocked
/// is not "nothing pending": the gate holds keys for a successful one.
#[must_use]
pub fn key_dialog(blocked: bool, gate: Gate, read: &PendingRead) -> KeyDialog {
    match (blocked, gate) {
        (false, _) => KeyDialog::None,
        (true, Gate::Updating) => KeyDialog::Rereading,
        (true, Gate::Ready(_) | Gate::Answered) => {
            KeyDialog::Shown(read.pending.as_ref().map(|p| p.id.clone()))
        }
    }
}

/// The options to show as chips, if any. With none (e.g. a low-confidence
/// screen parse, where the bridge sends no options), the keypad is the way
/// to answer.
#[must_use]
pub fn chips(pending: Option<&PendingInput>) -> &[PendingOption] {
    pending.map_or(&[], |p| p.options.as_slice())
}

/// How one option's chip behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    /// Low risk: a tap chooses it.
    Choose,
    /// Low risk and takes feedback: a tap opens the reason box first.
    Feedback,
    /// A permission dialog's low-risk "No" that takes no feedback itself,
    /// read off the screen: a tap opens the reason box first, and a note
    /// typed there is sent as a prompt once the "No" is through
    /// ([`deny_with_note`]). Only for a dialog on the screen, which a "No"
    /// sends back to the prompt; after a hook's denial Claude may carry on
    /// working instead, so the note could not follow.
    Note,
    /// Elevated: shown, but disabled until step-up exists; a tap explains.
    Desktop,
}

/// The chip for `option` of `pending`. Elevated (or unknown, which parses
/// as elevated) risk is never choosable from the app for now.
#[must_use]
pub fn chip(pending: &PendingInput, option: &PendingOption) -> Chip {
    match (option.risk, option.accepts_feedback) {
        (Risk::Elevated, _) => Chip::Desktop,
        (Risk::Low, true) => Chip::Feedback,
        (Risk::Low, false)
            if pending.kind == PendingKind::Permission && from_screen(pending) && is_no(option) =>
        {
            Chip::Note
        }
        (Risk::Low, false) => Chip::Choose,
    }
}

/// Whether `option` is the plain "No" (the parser's stable `no` id; never
/// guessed from the label, so "No, and tell Claude what to do differently"
/// without its Esc hint, which opens Claude's own feedback field, is not).
fn is_no(option: &PendingOption) -> bool {
    option.id.as_str() == "no"
}

/// Why an elevated chip is disabled.
pub const DESKTOP_REASON: &str = "Needs confirmation on the desktop for now.";

/// Whether the input was read off the terminal (best effort), so the user
/// should glance at the screen before choosing.
#[must_use]
pub const fn from_screen(pending: &PendingInput) -> bool {
    matches!(pending.source, PendingSource::Screen { .. })
}

/// Why a choice can't be sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChooseError {
    /// The option is not one of the shown input's.
    #[error("That isn't one of the options any more.")]
    UnknownOption,
    /// The option is elevated.
    #[error("{DESKTOP_REASON}")]
    NeedsDesktop,
    /// Feedback on an option that takes none.
    #[error("That option doesn't take a message.")]
    FeedbackNotAccepted,
    /// Feedback starting with `!`, `/` or `#` needs step-up.
    #[error("Messages starting with !, / or # need the desktop for now.")]
    ElevatedFeedback,
    /// The feedback text is not acceptable.
    #[error("Can't send this reason: {0}")]
    InvalidFeedback(ValidationError),
    /// The note is not acceptable as a prompt.
    #[error("Can't send this note: {0}")]
    InvalidNote(ValidationError),
}

/// The draft for choosing `option` of `pending`, with an optional typed
/// `feedback` (blank means none). Feedback is single-line on the wire, so
/// line breaks become spaces.
pub fn choose(
    pending: &PendingInput,
    option: &OptionId,
    feedback: Option<&str>,
) -> Result<Draft, ChooseError> {
    let found = pending
        .options
        .get(option)
        .ok_or(ChooseError::UnknownOption)?;
    if found.risk != Risk::Low {
        return Err(ChooseError::NeedsDesktop);
    }
    let feedback = feedback
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty());
    let feedback = match feedback {
        None => None,
        Some(_) if !found.accepts_feedback => return Err(ChooseError::FeedbackNotAccepted),
        Some(text) => {
            let text = FeedbackText::new(text).map_err(ChooseError::InvalidFeedback)?;
            if text.is_elevated() {
                return Err(ChooseError::ElevatedFeedback);
            }
            Some(text)
        }
    };
    Ok(Draft::Choose {
        pending: pending.id.clone(),
        option: found.id.clone(),
        label: found.label.clone(),
        feedback,
    })
}

/// The draft for a [`Chip::Note`] option ("No") and the note to send as a
/// prompt once it is through (`None` when blank). The note goes as its own
/// prompt, after the "No" (see [`super::reply::FollowUp`]): Claude's plain
/// "No" takes no feedback of its own.
pub fn deny_with_note(
    pending: &PendingInput,
    option: &OptionId,
    note: &str,
) -> Result<(Draft, Option<PromptText>), ChooseError> {
    let draft = choose(pending, option, None)?;
    let note = note.trim();
    if note.is_empty() {
        return Ok((draft, None));
    }
    let note = PromptText::new(note).map_err(ChooseError::InvalidNote)?;
    if note.is_elevated() {
        return Err(ChooseError::ElevatedFeedback);
    }
    Ok((draft, Some(note)))
}

#[cfg(test)]
mod tests {
    use protocol::{Confidence, PendingKind, PendingOptions, ScreenTail};

    use super::*;

    fn option(id: &str, risk: Risk, accepts_feedback: bool) -> PendingOption {
        PendingOption {
            id: OptionId::new(id).unwrap(),
            label: format!("label {id}"),
            risk,
            accepts_feedback,
        }
    }

    fn permission(id: &str) -> PendingInput {
        PendingInput {
            id: PendingId::new(id).unwrap(),
            kind: PendingKind::Permission,
            prompt: "Run `pnpm test`?".into(),
            detail: Some("pnpm test".into()),
            options: PendingOptions::new(vec![
                option("allow_once", Risk::Low, false),
                option("always", Risk::Elevated, false),
                option("deny", Risk::Low, true),
            ])
            .unwrap(),
            suggestions: vec![],
            source: PendingSource::Hook,
        }
    }

    fn read(pending: Option<PendingInput>, seq: u64, generation: u64) -> PendingRead {
        PendingRead {
            pending,
            seq: Some(Seq(seq)),
            generation,
            failed: false,
        }
    }

    fn oid(id: &str) -> OptionId {
        OptionId::new(id).unwrap()
    }

    #[test]
    fn chips_are_ready_only_for_the_current_input() {
        let shown = read(Some(permission("p1")), 5, 2);
        assert_eq!(gate(&shown, Some(Seq(5)), 2, None), Gate::Ready(Seq(5)));
        // The agent moved on since the read.
        assert_eq!(gate(&shown, Some(Seq(6)), 2, None), Gate::Updating);
        // A pending_changed arrived since the read.
        assert_eq!(gate(&shown, Some(Seq(5)), 3, None), Gate::Updating);
        // Never read yet.
        assert_eq!(
            gate(&PendingRead::default(), Some(Seq(5)), 0, None),
            Gate::Updating
        );
        // A read newer than the list is fine (the list lags).
        let newer = read(Some(permission("p1")), 7, 2);
        assert_eq!(gate(&newer, Some(Seq(5)), 2, None), Gate::Ready(Seq(7)));
    }

    #[test]
    fn an_answered_input_waits_for_the_next() {
        let shown = read(Some(permission("p1")), 5, 2);
        let p1 = PendingId::new("p1").unwrap();
        let p0 = PendingId::new("p0").unwrap();
        assert_eq!(gate(&shown, Some(Seq(5)), 2, Some(&p1)), Gate::Answered);
        assert_eq!(
            gate(&shown, Some(Seq(5)), 2, Some(&p0)),
            Gate::Ready(Seq(5))
        );
        let next = read(Some(permission("p2")), 6, 3);
        assert_eq!(gate(&next, Some(Seq(6)), 3, Some(&p1)), Gate::Ready(Seq(6)));
    }

    #[test]
    fn chip_kinds_follow_risk_and_feedback() {
        let p = permission("p1");
        let kinds: Vec<Chip> = chips(Some(&p)).iter().map(|o| chip(&p, o)).collect();
        assert_eq!(kinds, [Chip::Choose, Chip::Desktop, Chip::Feedback]);
        assert_eq!(chip(&p, &option("x", Risk::Elevated, true)), Chip::Desktop);
        assert_eq!(chips(None), []);
        let mut low_confidence = permission("p2");
        low_confidence.options = PendingOptions::default();
        low_confidence.source = PendingSource::Screen {
            confidence: Confidence::new(30).unwrap(),
            screen_tail: ScreenTail::default(),
        };
        assert_eq!(chips(Some(&low_confidence)), []);
        assert!(from_screen(&low_confidence));
        assert!(!from_screen(&p));
    }

    #[test]
    fn unknown_risk_is_never_choosable() {
        let json = r#"{"id":"p","kind":"permission","prompt":"?","source":"hook",
            "options":[{"id":"x","label":"X","risk":"spicy"}]}"#;
        let pending: PendingInput = serde_json::from_str(json).unwrap();
        assert_eq!(
            chip(&pending, &pending.options.as_slice()[0]),
            Chip::Desktop
        );
        assert_eq!(
            choose(&pending, &oid("x"), None),
            Err(ChooseError::NeedsDesktop)
        );
    }

    #[test]
    fn choose_names_the_shown_input_and_option() {
        let p = permission("p1");
        let draft = choose(&p, &oid("allow_once"), None).unwrap();
        assert_eq!(
            draft,
            Draft::Choose {
                pending: PendingId::new("p1").unwrap(),
                option: oid("allow_once"),
                label: "label allow_once".into(),
                feedback: None,
            }
        );
        assert_eq!(
            choose(&p, &oid("nope"), None),
            Err(ChooseError::UnknownOption)
        );
        assert_eq!(
            choose(&p, &oid("always"), None),
            Err(ChooseError::NeedsDesktop)
        );
    }

    #[test]
    fn feedback_only_where_accepted_and_single_line() {
        let p = permission("p1");
        let Draft::Choose { feedback, .. } =
            choose(&p, &oid("deny"), Some("  use pnpm\n  instead  ")).unwrap()
        else {
            panic!("not a choice")
        };
        assert_eq!(feedback.unwrap().as_str(), "use pnpm instead");
        // Blank feedback is no feedback.
        let Draft::Choose { feedback, .. } = choose(&p, &oid("deny"), Some(" \n ")).unwrap() else {
            panic!("not a choice")
        };
        assert_eq!(feedback, None);
        assert_eq!(
            choose(&p, &oid("allow_once"), Some("why")),
            Err(ChooseError::FeedbackNotAccepted)
        );
        assert!(choose(&p, &oid("allow_once"), Some("   ")).is_ok());
        assert_eq!(
            choose(&p, &oid("deny"), Some("/clear")),
            Err(ChooseError::ElevatedFeedback)
        );
        assert!(matches!(
            choose(&p, &oid("deny"), Some(&"x".repeat(5000))),
            Err(ChooseError::InvalidFeedback(_))
        ));
        assert!(matches!(
            choose(&p, &oid("deny"), Some("hidden\u{202E}text")),
            Err(ChooseError::InvalidFeedback(_))
        ));
    }

    /// Claude Code 2.1.317's permission dialog, read off the screen: a
    /// plain "No".
    fn plain_no(id: &str) -> PendingInput {
        let mut no = option("no", Risk::Low, false);
        no.label = "No".into();
        PendingInput {
            options: PendingOptions::new(vec![
                option("yes", Risk::Low, false),
                option("yes_always", Risk::Elevated, false),
                no,
            ])
            .unwrap(),
            source: PendingSource::Screen {
                confidence: Confidence::new(90).unwrap(),
                screen_tail: ScreenTail::default(),
            },
            ..permission(id)
        }
    }

    #[test]
    fn a_plain_no_on_a_permission_dialog_takes_a_note() {
        let p = plain_no("p1");
        let kinds: Vec<Chip> = chips(Some(&p)).iter().map(|o| chip(&p, o)).collect();
        assert_eq!(kinds, [Chip::Choose, Chip::Desktop, Chip::Note]);
        // Not on a plan approval or question: there "No…" is just a choice.
        let plan = PendingInput {
            kind: PendingKind::PlanApproval,
            ..plain_no("p2")
        };
        assert_eq!(chip(&plan, &plan.options.as_slice()[2]), Chip::Choose);
        // Matched by id, not label: Claude's own "tell Claude what to do
        // differently" option is not a note chip, whatever it says.
        let mut tell = option("no_feedback", Risk::Low, false);
        tell.label = "No, and tell Claude what to do differently".into();
        let p = PendingInput {
            options: PendingOptions::new(vec![tell.clone()]).unwrap(),
            ..plain_no("p3")
        };
        assert_eq!(chip(&p, &tell), Chip::Choose);
        // A hook's dialog: after its "No" Claude may keep working, so the
        // note could not follow; the "No" is just a choice.
        let hook = PendingInput {
            source: PendingSource::Hook,
            ..plain_no("p4")
        };
        assert_eq!(chip(&hook, &hook.options.as_slice()[2]), Chip::Choose);
    }

    #[test]
    fn deny_with_note_chooses_no_and_keeps_the_note_for_after() {
        let p = plain_no("p1");
        let (draft, note) = deny_with_note(&p, &oid("no"), "  use pnpm instead\n").unwrap();
        assert_eq!(
            draft,
            Draft::Choose {
                pending: PendingId::new("p1").unwrap(),
                option: oid("no"),
                label: "No".into(),
                feedback: None,
            }
        );
        assert_eq!(note.unwrap().as_str(), "use pnpm instead");
        let (_, blank) = deny_with_note(&p, &oid("no"), " \n ").unwrap();
        assert_eq!(blank, None);
        assert_eq!(
            deny_with_note(&p, &oid("no"), "/clear"),
            Err(ChooseError::ElevatedFeedback)
        );
        assert!(matches!(
            deny_with_note(&p, &oid("no"), "bad\u{202E}"),
            Err(ChooseError::InvalidNote(_))
        ));
        // Elevated options stay locked.
        assert_eq!(
            deny_with_note(&p, &oid("yes_always"), "fine"),
            Err(ChooseError::NeedsDesktop)
        );
    }

    #[test]
    fn keys_name_the_shown_dialog_only_while_blocked_and_current() {
        let shown = read(Some(permission("screen-5-0123456789ab")), 5, 1);
        let id = PendingId::new("screen-5-0123456789ab").unwrap();
        assert_eq!(key_dialog(false, Gate::Updating, &shown), KeyDialog::None);
        assert_eq!(
            key_dialog(true, Gate::Ready(Seq(5)), &shown),
            KeyDialog::Shown(Some(id.clone()))
        );
        assert_eq!(
            key_dialog(true, Gate::Answered, &shown),
            KeyDialog::Shown(Some(id.clone()))
        );
        assert_eq!(
            key_dialog(true, Gate::Updating, &shown),
            KeyDialog::Rereading
        );
        assert_eq!(KeyDialog::Rereading.pending_id(), None);
        assert_eq!(KeyDialog::None.pending_id(), Some(None));
        assert_eq!(
            KeyDialog::Shown(Some(id.clone())).pending_id(),
            Some(Some(&id))
        );
        // Blocked with nothing parsed: keys go without a pendingId.
        let nothing = read(None, 5, 1);
        assert_eq!(
            key_dialog(true, Gate::Ready(Seq(5)), &nothing),
            KeyDialog::Shown(None)
        );
    }

    #[test]
    fn a_replaced_dialog_at_the_same_seq_is_not_current() {
        // Same seq, new content: a pending_changed bumps the generation.
        let shown = read(Some(permission("screen-5-aaaaaaaaaaaa")), 5, 1);
        assert_eq!(gate(&shown, Some(Seq(5)), 2, None), Gate::Updating);
        let fresh = read(Some(permission("screen-5-bbbbbbbbbbbb")), 5, 2);
        assert_eq!(gate(&fresh, Some(Seq(5)), 2, None), Gate::Ready(Seq(5)));
        let Draft::Choose { pending, .. } =
            choose(fresh.pending.as_ref().unwrap(), &oid("allow_once"), None).unwrap()
        else {
            panic!("not a choice")
        };
        assert_eq!(pending.to_string(), "screen-5-bbbbbbbbbbbb");
    }

    #[test]
    fn a_failed_read_while_blocked_holds_the_keys() {
        let failed = PendingRead {
            failed: true,
            ..read(None, 5, 1)
        };
        let gated = gate(&failed, Some(Seq(5)), 1, None);
        assert_eq!(gated, Gate::Updating);
        assert_eq!(key_dialog(true, gated, &failed), KeyDialog::Rereading);
        assert_eq!(key_dialog(false, gated, &failed), KeyDialog::None);
    }

    #[test]
    fn a_failed_read_keeps_the_dialog_shown_but_waiting() {
        // The phone came back and its re-read failed: the dialog read
        // before stays (its chips with it), but nothing may answer it
        // until a read succeeds.
        let kept = PendingRead {
            failed: true,
            ..read(Some(permission("p1")), 5, 2)
        };
        assert_eq!(chips(kept.pending.as_ref()).len(), 3);
        assert_eq!(gate(&kept, Some(Seq(5)), 2, None), Gate::Updating);
        assert_eq!(
            key_dialog(true, Gate::Updating, &kept),
            KeyDialog::Rereading
        );
        let again = read(Some(permission("p1")), 5, 3);
        assert_eq!(gate(&again, Some(Seq(5)), 3, None), Gate::Ready(Seq(5)));
    }
}
