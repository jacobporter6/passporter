//! The response of `POST /v1/actions`. Outbound: tolerant.
//!
//! # Outbox contract (design §6.5)
//!
//! An idempotency key identifies one user decision. How the outbox treats
//! each outcome:
//!
//! - **No response** (network error, tunnel drop, 5xx from the edge): retry
//!   with the *same* key. The bridge dedupes, so this never sends twice.
//! - **Any 2xx or 409 response is final for its key, whether or not the
//!   body parses.** An unparseable body is never "no response": record the
//!   outcome as unknown, surface it, and never retry the key (the bridge would
//!   replay the same body forever).
//! - **Sent** ([`ActionResult::Sent`]): done.
//! - **Failed** ([`ActionResult::Failed`]): **final for that key**. Delivery
//!   is uncertain, so the outbox must not retry it automatically; resending
//!   is a new user decision with a new key.
//! - **Stale** ([`ActionResult::Stale`], HTTP 409): nothing was sent and the
//!   result is final for that key. **Any HTTP 409 is final for its key,
//!   whether or not the body parses**: never treat it as "no response" and
//!   never retry the key. "Send anyway" is a new action with a **new
//!   idempotency key and the current `seq` and `pendingId`** (from `current`
//!   / `pending`, or a refetch), never a replay.
//! - **Refused** ([`ActionResult::Refused`]): nothing was sent. If
//!   [`RefusalReason::is_transient`] the bridge did not record the result, and
//!   the same key may be retried (after backoff, or after step-up). Step-up
//!   proof rides in a request header bound to the key (planned `WebAuthn` step-up), so
//!   retrying the same key and body with it is not an
//!   [`RefusalReason::IdempotencyConflict`]. Otherwise the refusal is final
//!   for that key: surface it, don't retry.
//!
//! Replaying a key that has a recorded result returns that result unchanged.

use core::fmt;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::agent::Agent;
use crate::loose::{
    Slot, Tagged, read_lenient, read_slot, read_tagged, take, take_lenient, take_or_default,
};
use crate::pending::PendingInput;
use crate::tolerant::Lenient;

/// What happened to an [`Action`](crate::Action), tagged by `"status"`.
/// See the [module docs](self) for how an outbox reacts to each.
///
/// `Deserialize` is hand-written to read the result in one pass, as
/// [`AgentEvent`](crate::AgentEvent)'s is: each field is decoded by its
/// name, and one the variant lacks is ignored wherever it comes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ActionResult {
    /// The bridge handed the action to herdr.
    Sent {
        /// True when this is the stored result of an earlier identical
        /// request, i.e. nothing was sent this time. `false` when omitted.
        replayed: bool,
    },
    /// HTTP 409: the agent moved on since the user decided (`expectedSeq`
    /// or `pendingId` differs, or the session changed). Nothing was sent; the
    /// app asks "send anyway / edit / discard".
    ///
    /// `current` and `pending` parse leniently: a nested value this app
    /// cannot parse becomes `None` (refetch it) instead of failing the result.
    Stale {
        /// The agent's current state, or `None` if the session is gone (or
        /// unparseable).
        #[serde(skip_serializing_if = "Option::is_none")]
        current: Option<Box<Agent>>,
        /// What the agent is asking now, if anything.
        #[serde(skip_serializing_if = "Option::is_none")]
        pending: Option<PendingInput>,
    },
    /// The bridge refused the action. Nothing was sent.
    Refused {
        /// Machine-readable reason.
        reason: RefusalReason,
        /// Human-readable text, e.g. "Please confirm on your phone." Empty
        /// if a bridge omits it.
        message: String,
    },
    /// herdr reported an error. Delivery is uncertain: a timeout does not
    /// prove the keys or prompt did not arrive. Final for this key.
    Failed {
        /// Human-readable error. Empty if a bridge omits it.
        message: String,
    },
    /// A status this app predates; treat as failed and reload.
    Unknown,
}

/// The variant a `status` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Sent,
    Stale,
    Refused,
    Failed,
    Unknown,
}

impl Status {
    fn of(tag: &str) -> Self {
        match tag {
            "sent" => Self::Sent,
            "stale" => Self::Stale,
            "refused" => Self::Refused,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

/// A field some variant has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Replayed,
    Current,
    Pending,
    Reason,
    Message,
}

/// Every variant's fields, until the result ends.
#[derive(Default)]
struct Fields {
    replayed: Option<Slot<bool>>,
    /// Lenient by design: an unparseable agent (`null` included) is `None`.
    current: Option<Slot<Lenient<Box<Agent>>>>,
    /// Lenient by design, like `current`.
    pending: Option<Slot<Lenient<PendingInput>>>,
    reason: Option<Slot<RefusalReason>>,
    message: Option<Slot<String>>,
}

impl Tagged for Fields {
    type Kind = Status;
    type Field = Field;
    const TAG: &'static str = "status";

    fn kind(tag: &str) -> Status {
        Status::of(tag)
    }

    fn field(key: &str) -> Option<Field> {
        Some(match key {
            "replayed" => Field::Replayed,
            "current" => Field::Current,
            "pending" => Field::Pending,
            "reason" => Field::Reason,
            "message" => Field::Message,
            _ => return None,
        })
    }

    fn has(status: Status, field: Field) -> bool {
        match status {
            Status::Sent => field == Field::Replayed,
            Status::Stale => matches!(field, Field::Current | Field::Pending),
            Status::Refused => matches!(field, Field::Reason | Field::Message),
            Status::Failed => field == Field::Message,
            Status::Unknown => false,
        }
    }

    fn read<'de, A: MapAccess<'de>>(
        &mut self,
        field: Field,
        map: &mut A,
        tag_known: bool,
    ) -> Result<(), A::Error> {
        match field {
            Field::Replayed => read_slot(map, &mut self.replayed, "replayed", tag_known),
            Field::Current => read_lenient(map, &mut self.current, "current", tag_known),
            Field::Pending => read_lenient(map, &mut self.pending, "pending", tag_known),
            Field::Reason => read_slot(map, &mut self.reason, "reason", tag_known),
            Field::Message => read_slot(map, &mut self.message, "message", tag_known),
        }
    }
}

impl Fields {
    fn into_result<E: serde::de::Error>(self, status: Status) -> Result<ActionResult, E> {
        Ok(match status {
            Status::Sent => ActionResult::Sent {
                replayed: take_or_default(self.replayed, "replayed")?,
            },
            Status::Stale => ActionResult::Stale {
                current: take_lenient(self.current, "current")?,
                pending: take_lenient(self.pending, "pending")?,
            },
            Status::Refused => ActionResult::Refused {
                reason: take(self.reason, "reason")?,
                message: take_or_default(self.message, "message")?,
            },
            Status::Failed => ActionResult::Failed {
                message: take_or_default(self.message, "message")?,
            },
            Status::Unknown => ActionResult::Unknown,
        })
    }
}

impl<'de> Deserialize<'de> for ActionResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ResultVisitor)
    }
}

struct ResultVisitor;

impl<'de> Visitor<'de> for ResultVisitor {
    type Value = ActionResult;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an action result")
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<ActionResult, A::Error> {
        let (status, fields) = read_tagged::<Fields, A>(map)?;
        fields.into_result(status)
    }
}

/// Why an action was [`ActionResult::Refused`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// Barred from the car (design §6.6); confirm on the phone.
    CarPolicy,
    /// Needs biometric step-up first (design §8). Transient: retry the same
    /// key after step-up.
    StepUpRequired,
    /// No such agent.
    UnknownAgent,
    /// The chosen option is not offered by the current pending input.
    /// Permanent.
    UnknownOption,
    /// A `choose` while the agent has no pending input. Permanent.
    NoPendingInput,
    /// `feedbackText` on an option that does not accept feedback. Permanent.
    FeedbackNotAccepted,
    /// Per-device rate limit hit. Transient: retry the same key later.
    RateLimited,
    /// The idempotency key was already used with a different body.
    IdempotencyConflict,
    /// The body failed validation (malformed JSON, unknown field, size limit,
    /// key off the allowlist, ...). Permanent: the same body will always fail.
    InvalidAction,
    /// A prompt names an attachment this device never uploaded, or one
    /// retention has deleted. Permanent.
    UnknownAttachment,
    /// A reason this app predates. Treated as permanent, so an old app
    /// never retries in a loop.
    #[serde(other)]
    Other,
}

impl RefusalReason {
    /// Whether the same idempotency key may be retried: the bridge does not
    /// record transient refusals against the key.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::StepUpRequired | Self::RateLimited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_status_and_reason_fall_back() {
        assert_eq!(
            serde_json::from_str::<ActionResult>(r#"{"status":"queued","eta":3}"#).unwrap(),
            ActionResult::Unknown
        );
        assert_eq!(
            serde_json::from_str::<RefusalReason>("\"moon_phase\"").unwrap(),
            RefusalReason::Other
        );
    }

    #[test]
    fn stale_without_current_state() {
        let result: ActionResult = serde_json::from_str(r#"{"status":"stale"}"#).unwrap();
        assert_eq!(
            result,
            ActionResult::Stale {
                current: None,
                pending: None
            }
        );
    }

    #[test]
    fn stale_with_unparseable_nested_values_still_parses() {
        // An agent id and duplicate option ids this app rejects.
        let json = r#"{"status":"stale",
            "current":{"id":"w1/p1","title":"t","repo":"r","status":"idle","seq":1,
                "statusSince":0,"kind":"pi"},
            "pending":{"id":"p1","kind":"permission","prompt":"?","source":"hook",
                "options":[{"id":"a","label":"A","risk":"low"},{"id":"a","label":"B","risk":"low"}]}}"#;
        let result: ActionResult = serde_json::from_str(json).unwrap();
        assert_eq!(
            result,
            ActionResult::Stale {
                current: None,
                pending: None
            }
        );
        // Valid nested values still come through.
        let ok = r#"{"status":"stale","pending":{"id":"p1","kind":"question","prompt":"?",
            "source":"screen","confidence":250}}"#;
        let ActionResult::Stale { pending, .. } = serde_json::from_str(ok).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(pending.map(|p| p.id.to_string()), Some("p1".into()));
    }

    #[test]
    fn missing_messages_default_to_empty() {
        assert_eq!(
            serde_json::from_str::<ActionResult>(r#"{"status":"failed"}"#).unwrap(),
            ActionResult::Failed {
                message: String::new()
            }
        );
        assert_eq!(
            serde_json::from_str::<ActionResult>(r#"{"status":"refused","reason":"car_policy"}"#)
                .unwrap(),
            ActionResult::Refused {
                reason: RefusalReason::CarPolicy,
                message: String::new()
            }
        );
    }

    #[test]
    fn fields_may_come_in_any_order_and_extras_are_ignored() {
        let result: ActionResult = serde_json::from_str(
            r#"{"message":"slow down","reason":"rate_limited","status":"refused","replayed":7}"#,
        )
        .unwrap();
        assert_eq!(
            result,
            ActionResult::Refused {
                reason: RefusalReason::RateLimited,
                message: "slow down".into()
            }
        );
        let sent: ActionResult =
            serde_json::from_str(r#"{"status":"sent","replayed":true,"reason":{"x":1}}"#).unwrap();
        assert_eq!(sent, ActionResult::Sent { replayed: true });
        // Before the tag too: `replayed` is not a failed result's field.
        let failed: ActionResult =
            serde_json::from_str(r#"{"replayed":"yes","message":"boom","status":"failed"}"#)
                .unwrap();
        assert_eq!(
            failed,
            ActionResult::Failed {
                message: "boom".into()
            }
        );
        let unknown: ActionResult =
            serde_json::from_str(r#"{"reason":[1],"status":"queued"}"#).unwrap();
        assert_eq!(unknown, ActionResult::Unknown);
    }

    #[test]
    fn known_results_are_checked() {
        let parse = |json: &str| serde_json::from_str::<ActionResult>(json);
        // Missing tag, missing reason, wrong type, duplicate field.
        assert!(parse(r#"{"replayed":true}"#).is_err());
        assert!(parse(r#"{"status":"refused"}"#).is_err());
        assert!(parse(r#"{"status":"sent","replayed":"yes"}"#).is_err());
        assert!(parse(r#"{"status":"failed","message":"a","message":"b"}"#).is_err());
        assert!(parse(r#"{"reason":"car_policy","message":7,"status":"refused"}"#).is_err());
    }

    #[test]
    fn transient_vs_permanent_refusals() {
        let transient = [RefusalReason::StepUpRequired, RefusalReason::RateLimited];
        let permanent = [
            RefusalReason::CarPolicy,
            RefusalReason::UnknownAgent,
            RefusalReason::UnknownOption,
            RefusalReason::NoPendingInput,
            RefusalReason::FeedbackNotAccepted,
            RefusalReason::IdempotencyConflict,
            RefusalReason::InvalidAction,
            RefusalReason::Other,
        ];
        assert!(transient.iter().all(|r| r.is_transient()));
        assert!(permanent.iter().all(|r| !r.is_transient()));
    }
}
