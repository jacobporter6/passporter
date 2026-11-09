//! The body of `POST /v1/actions`, the bridge's only mutating endpoint
//! (design §5). Inbound: strict.
//!
//! Wire shape:
//!
//! ```json
//! {
//!   "idempotencyKey": "9ca09978-ef89-46fd-9a85-118b84bee7b4",
//!   "agentId": "w6:p1~583dc90c-15e5-4f5c-aeea-2d4701cb9d28",
//!   "expectedSeq": 136,
//!   "createdAt": 1762500000000,
//!   "kind": "choose",
//!   "payload": { "pendingId": "toolu_01", "optionId": "deny", "feedbackText": "use nextest" },
//!   "origin": "car"
//! }
//! ```
//!
//! Unknown fields are rejected at every level, and every field is a validated
//! type, so a deserialized [`Action`] is fully valid.
//!
//! # Stale guard
//!
//! Before any policy check the bridge compares `agentId` (pane and session),
//! `expectedSeq` against herdr's current `state_change_seq`, and, for
//! actions that carry one, `pendingId` against the current
//! [`PendingInput::id`](crate::PendingInput::id). Any mismatch (including a
//! `pendingId` when nothing is pending) is a 409
//! [`ActionResult::Stale`](crate::ActionResult::Stale): a dialog replaced by
//! another without `seq` moving is caught by `pendingId`. See
//! [`Action::pending_is_current`].
//!
//! # Step-up proof
//!
//! There is no wire field for step-up. The proof (a `WebAuthn` / biometric
//! assertion) travels out-of-band in a request header bound to the
//! `idempotencyKey`, defined by the auth protocol. Retrying the same key
//! and body with the proof attached, after a
//! [`StepUpRequired`](crate::RefusalReason::StepUpRequired) refusal, is a
//! normal retry, not an
//! [`IdempotencyConflict`](crate::RefusalReason::IdempotencyConflict).
//!
//! # Notes for reviewers
//!
//! - serde's derived struct deserializers also accept the *sequence form* of
//!   a payload, e.g. `"payload": ["go"]` for `{"text": "go"}`. That is not a
//!   bypass: elements go through the same validated types, and extra elements
//!   are rejected. Clients should send the object form; serialization always
//!   produces it.
//! - Strings and lists are fully deserialized before their size checks run,
//!   so the bridge's HTTP body limit is what bounds allocation per request.
//! - Whether an action needs step-up, or is allowed from the car, is decided
//!   by [`crate::policy`].

use std::collections::BTreeMap;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::attachment::Attachments;
use crate::ids::{AgentId, IdempotencyKey, OptionId, PendingId, Seq};
use crate::keys::KeySequence;
use crate::pending::PendingInput;
use crate::text::{FeedbackText, PromptText};
use crate::time::Timestamp;

/// A user decision to be applied to one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "ActionWire")]
pub struct Action {
    /// Generated on the device when the action is created (offline too).
    pub idempotency_key: IdempotencyKey,
    /// The agent the decision was made for.
    pub agent_id: AgentId,
    /// The agent's `state_change_seq` when the user decided.
    pub expected_seq: Seq,
    /// When the user decided (device clock).
    pub created_at: Timestamp,
    /// The verb and its payload, as `"kind"` + `"payload"` on the wire.
    #[serde(flatten)]
    pub kind: ActionKind,
    /// Which surface the action came from; drives the car policy and audit.
    pub origin: Origin,
}

/// The fixed verbs (design §5, §8), adjacently tagged as `"kind"` +
/// `"payload"`. There is no arbitrary-command verb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ActionKind {
    /// Send a new prompt.
    Prompt(PromptPayload),
    /// Choose an option of the agent's current [`PendingInput`].
    Choose(ChoosePayload),
    /// Send allowlisted keys.
    Keys(KeysPayload),
    /// Interrupt the running turn.
    Interrupt(InterruptPayload),
}

/// Payload of [`ActionKind::Prompt`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptPayload {
    /// The prompt text.
    pub text: PromptText,
    /// Images uploaded by the same device (`POST /v1/attachments`) to send
    /// with the text; the bridge names each one's file in the prompt.
    #[serde(default, skip_serializing_if = "Attachments::is_empty")]
    pub attachments: Attachments,
}

impl PromptPayload {
    /// A prompt of `text` alone.
    #[must_use]
    pub fn text(text: PromptText) -> Self {
        Self {
            text,
            attachments: Attachments::default(),
        }
    }
}

/// Payload of [`ActionKind::Choose`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChoosePayload {
    /// The pending input being answered; must match the current one.
    pub pending_id: PendingId,
    /// The chosen option's id.
    pub option_id: OptionId,
    /// Optional feedback, e.g. why a permission was denied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_text: Option<FeedbackText>,
}

/// Payload of [`ActionKind::Keys`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeysPayload {
    /// The pending input the keys answer, if any; when present it must match
    /// the current one (the keypad fallback sets it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<PendingId>,
    /// The keys to send, in order.
    pub keys: KeySequence,
}

impl Action {
    /// The pending input this action answers, if it names one.
    #[must_use]
    pub fn pending_id(&self) -> Option<&PendingId> {
        match &self.kind {
            ActionKind::Choose(choose) => Some(&choose.pending_id),
            ActionKind::Keys(keys) => keys.pending_id.as_ref(),
            ActionKind::Prompt(_) | ActionKind::Interrupt(_) => None,
        }
    }

    /// The `pendingId` part of the stale guard: false when the action names
    /// a pending input that is not the current one (or nothing is pending).
    /// Actions that name none always pass this part.
    #[must_use]
    pub fn pending_is_current(&self, current: Option<&PendingInput>) -> bool {
        match self.pending_id() {
            None => true,
            Some(id) => current.is_some_and(|pending| &pending.id == id),
        }
    }
}

/// Payload of [`ActionKind::Interrupt`]: always `{}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptPayload {}

/// The surface an action came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The app (PWA or APK), with a full screen and step-up available.
    App,
    /// A notification action or inline reply.
    Notification,
    /// Android Auto: subject to the car policy (design §6.6).
    Car,
}

/// Deserialization shadow of [`Action`].
///
/// `deny_unknown_fields` does not work together with `flatten`, so unknown
/// top-level keys are collected into `unknown` and rejected in `TryFrom`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionWire {
    idempotency_key: IdempotencyKey,
    agent_id: AgentId,
    expected_seq: Seq,
    created_at: Timestamp,
    // Must precede `unknown`: it takes `kind` and `payload` out of the
    // buffered entries, so `unknown` sees only the leftovers.
    #[serde(flatten)]
    kind: ActionKind,
    origin: Origin,
    // A set cannot be flattened; serde needs a map to collect leftover keys.
    #[allow(clippy::zero_sized_map_values)]
    #[serde(flatten)]
    unknown: BTreeMap<String, IgnoredAny>,
}

/// Rejection of an action body with fields the protocol does not define.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownFields(pub Vec<String>);

impl core::fmt::Display for UnknownFields {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "unknown field(s): {}", self.0.join(", "))
    }
}

impl core::error::Error for UnknownFields {}

impl TryFrom<ActionWire> for Action {
    type Error = UnknownFields;

    fn try_from(wire: ActionWire) -> Result<Self, Self::Error> {
        if !wire.unknown.is_empty() {
            return Err(UnknownFields(wire.unknown.into_keys().collect()));
        }
        Ok(Self {
            idempotency_key: wire.idempotency_key,
            agent_id: wire.agent_id,
            expected_seq: wire.expected_seq,
            created_at: wire.created_at,
            kind: wire.kind,
            origin: wire.origin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(kind_and_payload: &str) -> String {
        format!(
            r#"{{"idempotencyKey":"9ca09978-ef89-46fd-9a85-118b84bee7b4",
                "agentId":"w6:p1~583dc90c-15e5-4f5c-aeea-2d4701cb9d28",
                "expectedSeq":136,"createdAt":1762500000000,
                {kind_and_payload},"origin":"app"}}"#
        )
    }

    fn parse(kind_and_payload: &str) -> Result<Action, serde_json::Error> {
        serde_json::from_str(&body(kind_and_payload))
    }

    #[test]
    fn accepts_every_verb() {
        assert!(parse(r#""kind":"prompt","payload":{"text":"go"}"#).is_ok());
        assert!(
            parse(r#""kind":"choose","payload":{"pendingId":"p1","optionId":"allow_once"}"#)
                .is_ok()
        );
        let photo = parse(
            r#""kind":"prompt","payload":{"text":"see","attachments":["00000000-0000-4000-8000-0000000000a1"]}"#,
        )
        .unwrap();
        let ActionKind::Prompt(prompt) = &photo.kind else {
            panic!("a prompt")
        };
        assert_eq!(prompt.attachments.as_slice().len(), 1);
        assert!(
            parse(r#""kind":"choose","payload":{"pendingId":"p1","optionId":"deny","feedbackText":"no"}"#).is_ok()
        );
        assert!(parse(r#""kind":"keys","payload":{"keys":["2","enter"]}"#).is_ok());
        assert!(parse(r#""kind":"interrupt","payload":{}"#).is_ok());
        // Field order does not matter.
        assert!(parse(r#""payload":{"text":"go"},"kind":"prompt""#).is_ok());
    }

    #[test]
    fn sequence_form_payloads_still_validate() {
        let action = parse(r#""kind":"prompt","payload":["go"]"#).unwrap();
        // Serialization always uses the object form.
        assert!(
            serde_json::to_string(&action)
                .unwrap()
                .contains(r#""payload":{"text":"go"}"#)
        );
        assert!(parse(r#""kind":"prompt","payload":["\u001b[A"]"#).is_err());
        assert!(parse(r#""kind":"prompt","payload":["go","extra"]"#).is_err());
        assert!(parse(r#""kind":"choose","payload":["p1","deny","a\nb"]"#).is_err());
        assert!(parse(r#""kind":"keys","payload":[null,["enter"]]"#).is_ok());
        assert!(parse(r#""kind":"keys","payload":[null,["ctrl+z"]]"#).is_err());
    }

    #[test]
    fn choose_requires_pending_id_and_keys_may_carry_one() {
        assert!(parse(r#""kind":"choose","payload":{"optionId":"allow_once"}"#).is_err());
        assert!(
            parse(r#""kind":"choose","payload":{"pendingId":"..","optionId":"allow_once"}"#)
                .is_err()
        );
        let keys = parse(r#""kind":"keys","payload":{"pendingId":"p1","keys":["1"]}"#).unwrap();
        assert_eq!(keys.pending_id().map(PendingId::as_str), Some("p1"));
        let prompt = parse(r#""kind":"prompt","payload":{"text":"go"}"#).unwrap();
        assert_eq!(prompt.pending_id(), None);
    }

    #[test]
    fn pending_is_current_compares_pending_ids() {
        let current = PendingInput::unparsed_dialog(PendingId::new("p1").unwrap());
        let replaced = PendingInput::unparsed_dialog(PendingId::new("p2").unwrap());
        let choose =
            parse(r#""kind":"choose","payload":{"pendingId":"p1","optionId":"allow_once"}"#)
                .unwrap();
        assert!(choose.pending_is_current(Some(&current)));
        assert!(!choose.pending_is_current(Some(&replaced)));
        assert!(!choose.pending_is_current(None));
        let keys = parse(r#""kind":"keys","payload":{"keys":["1"]}"#).unwrap();
        assert!(keys.pending_is_current(None));
        assert!(keys.pending_is_current(Some(&replaced)));
        let interrupt = parse(r#""kind":"interrupt","payload":{}"#).unwrap();
        assert!(interrupt.pending_is_current(Some(&current)));
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        let err = parse(r#""kind":"interrupt","payload":{},"shell":"rm -rf /""#).unwrap_err();
        assert!(err.to_string().contains("unknown field(s): shell"), "{err}");
    }

    #[test]
    fn plain_prompts_serialize_without_attachments() {
        let plain = parse(r#""kind":"prompt","payload":{"text":"go"}"#).unwrap();
        let json = serde_json::to_string(&plain).unwrap();
        assert!(json.contains(r#""payload":{"text":"go"}"#), "{json}");
        assert!(
            parse(r#""kind":"prompt","payload":{"text":"go","attachments":["x"]}"#).is_err(),
            "attachment ids are UUIDs"
        );
    }

    #[test]
    fn rejects_unknown_payload_fields() {
        assert!(parse(r#""kind":"prompt","payload":{"text":"go","pane":"w1:p1"}"#).is_err());
        assert!(parse(r#""kind":"interrupt","payload":{"force":true}"#).is_err());
        assert!(
            parse(r#""kind":"choose","payload":{"pendingId":"p1","optionId":"a","always":true}"#)
                .is_err()
        );
    }

    #[test]
    fn rejects_unknown_verbs_and_origins() {
        assert!(parse(r#""kind":"exec","payload":{"cmd":"ls"}"#).is_err());
        let bad_origin = body(r#""kind":"interrupt","payload":{}"#).replace("\"app\"", "\"watch\"");
        assert!(serde_json::from_str::<Action>(&bad_origin).is_err());
    }

    #[test]
    fn rejects_payload_mismatched_with_kind() {
        assert!(parse(r#""kind":"prompt","payload":{"keys":["1"]}"#).is_err());
        assert!(parse(r#""kind":"keys","payload":{"text":"hi"}"#).is_err());
        assert!(parse(r#""kind":"prompt""#).is_err());
    }

    #[test]
    fn rejects_missing_envelope_fields() {
        for field in [
            "idempotencyKey",
            "agentId",
            "expectedSeq",
            "createdAt",
            "origin",
        ] {
            let mut value: serde_json::Value =
                serde_json::from_str(&body(r#""kind":"interrupt","payload":{}"#)).unwrap();
            value.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Action>(value).is_err(), "{field}");
        }
    }

    #[test]
    fn validates_nested_values() {
        // Oversized prompt, control characters, empty text.
        let huge = "x".repeat(crate::text::MAX_PROMPT_BYTES + 1);
        assert!(parse(&format!(r#""kind":"prompt","payload":{{"text":"{huge}"}}"#)).is_err());
        assert!(parse(r#""kind":"prompt","payload":{"text":"\u001b[A"}"#).is_err());
        assert!(parse(r#""kind":"prompt","payload":{"text":""}"#).is_err());
        // Oversized / empty feedback.
        let long = "x".repeat(crate::text::MAX_FEEDBACK_BYTES + 1);
        assert!(
            parse(&format!(
                r#""kind":"choose","payload":{{"pendingId":"p1","optionId":"deny","feedbackText":"{long}"}}"#
            ))
            .is_err()
        );
        assert!(
            parse(r#""kind":"choose","payload":{"pendingId":"p1","optionId":"deny","feedbackText":" "}"#).is_err()
        );
        // Bad option id, keys off the allowlist, empty key list.
        assert!(parse(r#""kind":"choose","payload":{"pendingId":"p1","optionId":"a b"}"#).is_err());
        assert!(parse(r#""kind":"keys","payload":{"keys":["ctrl+d"]}"#).is_err());
        assert!(parse(r#""kind":"keys","payload":{"keys":[]}"#).is_err());
        // Bad envelope ids.
        let bad_key = body(r#""kind":"interrupt","payload":{}"#)
            .replace("9ca09978-ef89-46fd-9a85-118b84bee7b4", "retry-1");
        assert!(serde_json::from_str::<Action>(&bad_key).is_err());
        let bad_agent = body(r#""kind":"interrupt","payload":{}"#).replace("w6:p1~", "w6:p1/");
        assert!(serde_json::from_str::<Action>(&bad_agent).is_err());
    }

    #[test]
    fn unknown_fields_display() {
        let err = UnknownFields(vec!["a".into(), "b".into()]);
        assert_eq!(err.to_string(), "unknown field(s): a, b");
    }
}
