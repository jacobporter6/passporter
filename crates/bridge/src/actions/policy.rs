//! The MVP's action policy: protocol `policy` with one documented
//! exemption (crate README, "MVP mode").
//!
//! The MVP has no step-up mechanism yet (`WebAuthn` is planned), so
//! anything that requires step-up is refused with `step_up_required`,
//! except **raw keys and interrupt from the app**: the keypad is the MVP's
//! only way to answer a dialog, and an interrupt is exactly the keypad's
//! `ctrl+c`. The car always follows `check_car_policy` (no exemption).
//!
//! `choose` follows protocol policy exactly: a low-risk option of the
//! screen-parsed input may be chosen from the app or a notification; an
//! elevated one (*don't ask again*, *auto-accept*, anything unrecognized) is
//! refused with `step_up_required` until step-up exists; the car
//! refuses every screen-parsed `choose` (`car_policy`).

use protocol::{
    Action, ActionResult, Origin, PendingInput, RefusalReason, StepUpReason, check_answerable,
    check_car_policy, step_up_reason,
};

fn refused(reason: RefusalReason, message: impl Into<String>) -> ActionResult {
    ActionResult::Refused {
        reason,
        message: message.into(),
    }
}

/// The refusal to return if `action` may not execute against the current
/// `pending` input; `None` if it may.
pub(crate) fn refusal(action: &Action, pending: Option<&PendingInput>) -> Option<ActionResult> {
    if action.origin == Origin::Car {
        if let Err(refusal) = check_car_policy(action, pending) {
            return Some(refused(refusal.refusal_reason(), refusal.to_string()));
        }
    } else {
        if let Err(unanswerable) = check_answerable(action, pending) {
            return Some(refused(
                unanswerable.refusal_reason(),
                "Nothing on the agent's screen matches that answer; use the keypad.",
            ));
        }
        if let Some(reason) = step_up_reason(action, pending) {
            let mvp_exempt = action.origin == Origin::App
                && matches!(reason, StepUpReason::RawKeys | StepUpReason::Interrupt);
            if !mvp_exempt {
                return Some(refused(
                    RefusalReason::StepUpRequired,
                    step_up_message(reason),
                ));
            }
        }
    }
    None
}

fn step_up_message(reason: StepUpReason) -> &'static str {
    match reason {
        StepUpReason::PromptIntoDialog => {
            "The agent is showing a dialog; answer it with the keypad first."
        }
        StepUpReason::ElevatedPrompt => {
            "Prompts starting with !, / or # need confirmation, which this version can't do yet."
        }
        StepUpReason::RawKeys | StepUpReason::Interrupt => {
            "Keys and interrupts can only be sent from the app."
        }
        StepUpReason::ElevatedOption => {
            "That option needs confirmation, which this version can't do yet; use the keypad."
        }
        StepUpReason::ElevatedFeedback => {
            "Feedback starting with !, / or # needs confirmation, which this version can't do yet."
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::{
        Confidence, OptionId, PendingId, PendingKind, PendingOption, PendingOptions, PendingSource,
        Risk, ScreenTail,
    };

    use super::*;

    fn action(kind_and_payload: &serde_json::Value, origin: &str) -> Action {
        let mut body = serde_json::json!({
            "idempotencyKey": "00000000-0000-4000-8000-000000000001",
            "agentId": "w1:p1",
            "expectedSeq": 1,
            "createdAt": 0,
            "origin": origin,
        });
        let fields = body.as_object_mut().expect("object");
        for (k, v) in kind_and_payload.as_object().expect("object") {
            fields.insert(k.clone(), v.clone());
        }
        serde_json::from_value(body).expect("valid action")
    }

    fn reason(result: Option<ActionResult>) -> Option<RefusalReason> {
        match result {
            None => None,
            Some(ActionResult::Refused { reason, .. }) => Some(reason),
            Some(other) => panic!("unexpected {other:?}"),
        }
    }

    fn dialog() -> PendingInput {
        PendingInput::unparsed_dialog(PendingId::new("screen-1").expect("id"))
    }

    #[test]
    fn app_may_send_keys_and_interrupt_but_car_may_not() {
        let keys = serde_json::json!({"kind": "keys", "payload": {"keys": ["2", "enter"]}});
        let interrupt = serde_json::json!({"kind": "interrupt", "payload": {}});
        for kind in [&keys, &interrupt] {
            assert_eq!(reason(refusal(&action(kind, "app"), Some(&dialog()))), None);
            assert_eq!(
                reason(refusal(&action(kind, "notification"), None)),
                Some(RefusalReason::StepUpRequired)
            );
            assert_eq!(
                reason(refusal(&action(kind, "car"), Some(&dialog()))),
                Some(RefusalReason::CarPolicy)
            );
        }
    }

    #[test]
    fn prompts_follow_the_step_up_rules() {
        let plain = serde_json::json!({"kind": "prompt", "payload": {"text": "go on"}});
        let slash = serde_json::json!({"kind": "prompt", "payload": {"text": "/clear"}});
        assert_eq!(reason(refusal(&action(&plain, "app"), None)), None);
        assert_eq!(reason(refusal(&action(&plain, "car"), None)), None);
        assert_eq!(
            reason(refusal(&action(&plain, "app"), Some(&dialog()))),
            Some(RefusalReason::StepUpRequired)
        );
        assert_eq!(
            reason(refusal(&action(&slash, "app"), None)),
            Some(RefusalReason::StepUpRequired)
        );
    }

    fn parsed() -> PendingInput {
        let option = |id: &str, risk, accepts_feedback| PendingOption {
            id: OptionId::new(id).expect("id"),
            label: id.to_owned(),
            risk,
            accepts_feedback,
        };
        PendingInput {
            options: PendingOptions::new(vec![
                option("yes", Risk::Low, false),
                option("yes_always", Risk::Elevated, false),
                option("no_feedback", Risk::Low, true),
            ])
            .expect("options"),
            source: PendingSource::Screen {
                confidence: Confidence::new(100).expect("confidence"),
                screen_tail: ScreenTail::default(),
            },
            kind: PendingKind::Permission,
            ..dialog()
        }
    }

    fn choose(option: &str, feedback: Option<&str>) -> serde_json::Value {
        let mut payload = serde_json::json!({"pendingId": "screen-1", "optionId": option});
        if let Some(feedback) = feedback {
            payload["feedbackText"] = feedback.into();
        }
        serde_json::json!({"kind": "choose", "payload": payload})
    }

    #[test]
    fn choose_follows_risk() {
        let pending = parsed();
        let p = Some(&pending);
        assert_eq!(
            reason(refusal(&action(&choose("yes", None), "app"), p)),
            None
        );
        assert_eq!(
            reason(refusal(&action(&choose("yes", None), "notification"), p)),
            None
        );
        assert_eq!(
            reason(refusal(&action(&choose("yes_always", None), "app"), p)),
            Some(RefusalReason::StepUpRequired)
        );
        assert_eq!(
            reason(refusal(
                &action(&choose("no_feedback", Some("use pnpm")), "app"),
                p
            )),
            None
        );
        assert_eq!(
            reason(refusal(
                &action(&choose("no_feedback", Some("/clear")), "app"),
                p
            )),
            Some(RefusalReason::StepUpRequired)
        );
        assert_eq!(
            reason(refusal(&action(&choose("yes", Some("and more")), "app"), p)),
            Some(RefusalReason::FeedbackNotAccepted)
        );
        // Screen-parsed input is never answered from the car.
        assert_eq!(
            reason(refusal(&action(&choose("yes", None), "car"), p)),
            Some(RefusalReason::CarPolicy)
        );
    }

    #[test]
    fn choose_needs_a_matching_option() {
        assert_eq!(
            reason(refusal(&action(&choose("yes", None), "app"), None)),
            Some(RefusalReason::NoPendingInput)
        );
        assert_eq!(
            reason(refusal(
                &action(&choose("yes", None), "app"),
                Some(&dialog())
            )),
            Some(RefusalReason::UnknownOption)
        );
    }
}
