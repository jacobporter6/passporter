//! Android Auto replies: `POST /v1/agents/:id/car-reply` (design §6.6).
//!
//! The phone sends the car's transcribed utterance as it heard it. The
//! bridge decides what it means, under the car policy
//! ([`crate::policy::check_car_policy`]), so the native app stays a thin
//! pipe:
//!
//! ```json
//! {"idempotencyKey": "9ca09978-ef89-46fd-9a85-118b84bee7b4",
//!  "utterance": "No, use pnpm instead",
//!  "pendingId": "screen-136-0a1b2c3d4e5f"}
//! ```
//!
//! The answer is a [`CarReply`]: the [`ActionResult`] plus one short
//! sentence for the car to read aloud.
//!
//! # Vocabulary
//!
//! [`CarIntent::interpret`] is deliberately tiny and exact. After
//! normalising like runbook phrases ([`normalise_phrase`]: case,
//! punctuation, filler words):
//!
//! - `allow`, `yes`, `approve` or `allow once`, and nothing else, is
//!   [`CarIntent::Allow`];
//! - `deny` or `no` first is [`CarIntent::Deny`], with the rest of the
//!   utterance as feedback, unless the rest affirms ("no problem, go
//!   ahead"): that is refused as ambiguous;
//! - `allow`, `yes` or `approve` followed by more words, or nothing but
//!   filler, is refused ([`UtteranceRefusal`]): "yes, but…" must not allow;
//! - anything else is a [`CarIntent::Prompt`].
//!
//! What an intent may do is the bridge's call: Allow and Deny only answer
//! a hook-described dialog, and a prompt only goes when no dialog is open.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{IdempotencyKey, PendingId};
use crate::macros::validated_string;
use crate::result::ActionResult;
use crate::runbook::{FILLERS, normalise_phrase};
use crate::text::{FeedbackText, PromptText, check_text};

/// Longest [`Utterance`], in bytes: a spoken sentence or two.
pub const MAX_UTTERANCE_BYTES: usize = 1024;

/// Phrases that allow, after [`normalise_phrase`].
const ALLOW: [&str; 4] = ["allow", "yes", "approve", "allow once"];

/// Words that deny when they come first, after [`normalise_phrase`].
const DENY: [&str; 2] = ["deny", "no"];

/// Words that make a leading `no` doubtful ("no problem, go ahead", "no
/// worries, allow it"): a deny followed by one of them is ambiguous. A
/// real deny with one in its note ("no, don't allow that") is refused too,
/// which costs a plain "no"; denying by mistake costs more.
const AFFIRMING: [&str; 11] = [
    "yes", "yeah", "yep", "allow", "approve", "sure", "problem", "worries", "worry", "ahead",
    "fine",
];

validated_string!(
    /// What the car heard: not blank, at most [`MAX_UTTERANCE_BYTES`],
    /// **single-line**, and free of the characters
    /// [`FeedbackText`] refuses, so any part of it can become a prompt or
    /// feedback.
    Utterance,
    |v| check_text("utterance", MAX_UTTERANCE_BYTES, false, v)
);

/// `POST /v1/agents/:id/car-reply`. Inbound, so strict.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CarReplyRequest {
    /// Generated on the phone when the reply is heard; a retry reuses it.
    pub idempotency_key: IdempotencyKey,
    /// The transcribed utterance, raw.
    pub utterance: Utterance,
    /// The pending input the car read aloud (from the notice). Allow and
    /// Deny need it, and it must still be the current one; a prompt with
    /// one is stale once that dialog is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<PendingId>,
}

/// The answer to a [`CarReplyRequest`]. Outbound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarReply {
    /// One short sentence to read aloud ("Allowed: cargo test", "Sent",
    /// "That needs the phone.").
    pub speech: String,
    /// What happened, as for `POST /v1/actions`.
    pub result: ActionResult,
}

/// What an utterance asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarIntent {
    /// Choose the dialog's low-risk *Allow once* option.
    Allow,
    /// Choose the dialog's *Deny*, telling the agent `feedback` if any.
    Deny {
        /// The rest of the utterance after `no` / `deny`.
        feedback: Option<FeedbackText>,
    },
    /// Send the utterance as a prompt.
    Prompt(PromptText),
}

/// Why an utterance was not understood. `Display` is read aloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UtteranceRefusal {
    /// Nothing but filler words and punctuation.
    Empty,
    /// An allow word with more after it ("yes, but run the tests first"),
    /// or allow and deny together.
    Ambiguous,
}

impl fmt::Display for UtteranceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "I didn't catch that. Please say it again.",
            Self::Ambiguous => {
                "I'm not sure what you meant. Say just yes to allow, or no and what to do instead."
            }
        })
    }
}

impl core::error::Error for UtteranceRefusal {}

impl CarIntent {
    /// Reads `utterance` with the [module's](self) vocabulary.
    pub fn interpret(utterance: &Utterance) -> Result<Self, UtteranceRefusal> {
        let raw = utterance.as_str();
        let phrase = normalise_phrase(raw);
        let Some(first) = phrase.split(' ').next().filter(|word| !word.is_empty()) else {
            return Err(UtteranceRefusal::Empty);
        };
        if ALLOW.contains(&phrase.as_str()) {
            return Ok(Self::Allow);
        }
        if DENY.contains(&first)
            && let Some(rest) = after_first_word(raw)
        {
            let said = normalise_phrase(rest);
            if said.split(' ').any(|word| AFFIRMING.contains(&word)) {
                return Err(UtteranceRefusal::Ambiguous);
            }
            let no_note = said.is_empty()
                || DENY.contains(&said.as_str())
                || said
                    .split(' ')
                    .next()
                    .is_some_and(|word| word.starts_with("thank"));
            let feedback = if no_note {
                None
            } else {
                FeedbackText::new(rest).ok()
            };
            return Ok(Self::Deny { feedback });
        }
        if ALLOW.contains(&first) {
            return Err(UtteranceRefusal::Ambiguous);
        }
        PromptText::new(raw.trim())
            .map(Self::Prompt)
            .map_err(|_| UtteranceRefusal::Empty)
    }
}

/// `raw` after its first word that is not a filler, without the
/// whitespace and sentence punctuation that followed it. Everything else
/// is kept as said, so `"no, /clear"` keeps its `/` (and is refused as
/// elevated feedback) rather than turning into `"clear"`. `None` when that
/// word is the start of a hyphenated one (`"No-op that change"`,
/// `"no-one"`): not the word itself.
fn after_first_word(raw: &str) -> Option<&str> {
    let is_word_char = |c: char| c.is_alphanumeric() || matches!(c, '\'' | '’');
    let mut rest = raw;
    loop {
        let start = rest.find(is_word_char)?;
        let tail = &rest[start..];
        let end = tail.find(|c: char| !is_word_char(c)).unwrap_or(tail.len());
        let word = normalise_phrase(&tail[..end]);
        rest = &tail[end..];
        if !word.is_empty() && !FILLERS.contains(&word.as_str()) {
            let mut after = rest.chars();
            if after.next() == Some('-') && after.next().is_some_and(char::is_alphanumeric) {
                return None;
            }
            return Some(
                rest.trim_start_matches(|c: char| {
                    c.is_whitespace()
                        || matches!(c, ',' | '.' | ';' | ':' | '!' | '?' | '-' | '–' | '—')
                })
                .trim_end(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(text: &str) -> Result<CarIntent, UtteranceRefusal> {
        CarIntent::interpret(&Utterance::new(text).unwrap())
    }

    fn deny(feedback: Option<&str>) -> CarIntent {
        CarIntent::Deny {
            feedback: feedback.map(|f| FeedbackText::new(f).unwrap()),
        }
    }

    fn prompt(text: &str) -> CarIntent {
        CarIntent::Prompt(PromptText::new(text).unwrap())
    }

    #[test]
    fn allow_is_a_tiny_exact_vocabulary() {
        for said in [
            "allow",
            "Yes.",
            "yes please",
            "Um, approve!",
            "Allow once",
            "  YES  ",
        ] {
            assert_eq!(intent(said), Ok(CarIntent::Allow), "{said:?}");
        }
    }

    #[test]
    fn deny_keeps_the_rest_as_feedback() {
        assert_eq!(intent("no"), Ok(deny(None)));
        assert_eq!(intent("Deny."), Ok(deny(None)));
        assert_eq!(intent("uh, no no"), Ok(deny(None)));
        assert_eq!(
            intent("No, use pnpm instead."),
            Ok(deny(Some("use pnpm instead.")))
        );
        assert_eq!(
            intent("um deny — run the linter first"),
            Ok(deny(Some("run the linter first")))
        );
        // A command prefix survives, so policy can refuse it.
        assert_eq!(intent("no, /clear"), Ok(deny(Some("/clear"))));
        assert_eq!(intent("No thanks."), Ok(deny(None)), "no note of thanks");
        assert_eq!(intent("no, thank you"), Ok(deny(None)));
        assert_eq!(intent("no - wait"), Ok(deny(Some("wait"))));
    }

    #[test]
    fn ambiguous_and_empty_utterances_are_refused() {
        for said in [
            "yes but run the tests first",
            "yes no",
            "approve it",
            "no, yes",
        ] {
            assert_eq!(intent(said), Err(UtteranceRefusal::Ambiguous), "{said:?}");
        }
        for said in ["um", "okay.", "uh, so"] {
            assert_eq!(intent(said), Err(UtteranceRefusal::Empty), "{said:?}");
        }
        assert!(UtteranceRefusal::Ambiguous.to_string().contains("yes"));
    }

    #[test]
    fn anything_else_is_a_prompt() {
        assert_eq!(
            intent("carry on with the tests"),
            Ok(prompt("carry on with the tests"))
        );
        assert_eq!(intent(" Nope, stop "), Ok(prompt("Nope, stop")));
        assert_eq!(intent("/clear"), Ok(prompt("/clear")));
        // A hyphenated word starting with "no" is not a no.
        assert_eq!(intent("No-op that change"), Ok(prompt("No-op that change")));
        assert_eq!(intent("no-one needs that"), Ok(prompt("no-one needs that")));
    }

    #[test]
    fn utterances_are_single_line_and_bounded() {
        assert!(Utterance::new("a\nb").is_err());
        assert!(Utterance::new("a\u{202e}b").is_err());
        assert!(Utterance::new("  ").is_err());
        assert!(Utterance::new("x".repeat(MAX_UTTERANCE_BYTES + 1)).is_err());
    }

    #[test]
    fn requests_are_strict() {
        let ok = r#"{"idempotencyKey":"9ca09978-ef89-46fd-9a85-118b84bee7b4","utterance":"yes","pendingId":"screen-1-ab"}"#;
        let request: CarReplyRequest = serde_json::from_str(ok).unwrap();
        assert_eq!(request.pending_id.unwrap().as_str(), "screen-1-ab");
        let extra = ok.replace('}', r#","origin":"app"}"#);
        assert!(serde_json::from_str::<CarReplyRequest>(&extra).is_err());
        let no_key = r#"{"utterance":"yes"}"#;
        assert!(serde_json::from_str::<CarReplyRequest>(no_key).is_err());
    }

    #[test]
    fn replies_round_trip() {
        let reply = CarReply {
            speech: "Sent.".into(),
            result: ActionResult::Sent { replayed: false },
        };
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"speech": "Sent.", "result": {"status": "sent", "replayed": false}})
        );
        assert_eq!(serde_json::from_value::<CarReply>(json).unwrap(), reply);
    }
}
