//! The stored form of an outbox [`Entry`]: a small hand-written, versioned
//! record instead of serde-derived JSON. Decoding an `Action` through serde
//! (flattened, adjacently tagged) and an enum of struct variants linked
//! tens of kilobytes into the WASM; this is one non-generic encoder and
//! decoder.
//!
//! A record is a sequence of fields, each `<byte length>:<text>`, so any
//! text (prompts, labels, bridge messages) round-trips without escaping.
//! The first field is the version. Optional fields are empty for `None`
//! and `=`-prefixed for `Some`. Anything that doesn't decode is reported
//! as unreadable and left in the store (it holds something the user said).

use core::fmt::Write as _;

use protocol::{
    Action, ActionKind, AgentId, AllowedKey, AttachmentId, Attachments, ChoosePayload,
    FeedbackText, IdempotencyKey, InterruptPayload, KeySequence, KeysPayload, OptionId, Origin,
    PendingId, PromptPayload, PromptText, RefusalReason, Seq, Timestamp,
};

use super::machine::{Delivery, Entry};

/// The record format version.
const VERSION: &str = "1";

/// Every refusal reason, by stored index. A test keeps it complete.
const REASONS: [RefusalReason; 11] = [
    RefusalReason::CarPolicy,
    RefusalReason::StepUpRequired,
    RefusalReason::UnknownAgent,
    RefusalReason::UnknownOption,
    RefusalReason::NoPendingInput,
    RefusalReason::FeedbackNotAccepted,
    RefusalReason::RateLimited,
    RefusalReason::IdempotencyConflict,
    RefusalReason::InvalidAction,
    RefusalReason::Other,
    RefusalReason::UnknownAttachment,
];

/// Index `i` (under 26) as one letter, `a` for 0.
fn letter(i: usize) -> char {
    char::from(b'a'.saturating_add(u8::try_from(i).unwrap_or(25)))
}

/// The index a [`letter`] stands for.
fn index(letter: u8) -> Option<usize> {
    letter.checked_sub(b'a').map(usize::from)
}

/// `n` in decimal (through `fmt`, which the app links anyway).
fn decimal(n: u64) -> String {
    let mut out = String::new();
    let _ = write!(out, "{n}");
    out
}

/// A decimal number made by [`decimal`] (a small parser instead of
/// `str::parse`, which links one copy per integer type).
fn parse_decimal(text: &str) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    text.bytes().try_fold(0_u64, |n, b| {
        let digit = b.checked_sub(b'0').filter(|d| *d < 10)?;
        n.checked_mul(10)?.checked_add(u64::from(digit))
    })
}

/// Builds a record field by field.
struct Writer(String);

impl Writer {
    fn field(&mut self, text: &str) {
        let _ = write!(self.0, "{}:", text.len());
        self.0.push_str(text);
    }

    fn number(&mut self, n: u64) {
        self.field(&decimal(n));
    }

    fn optional(&mut self, text: Option<&str>) {
        match text {
            Some(text) => {
                let _ = write!(self.0, "{}:=", text.len() + 1);
                self.0.push_str(text);
            }
            None => self.field(""),
        }
    }
}

/// A record that is not well formed.
struct Malformed;

/// Reads a record field by field; `None` once it is malformed or done.
struct Reader<'a>(&'a str);

impl<'a> Reader<'a> {
    fn field(&mut self) -> Option<&'a str> {
        let (len, rest) = self.0.split_once(':')?;
        let len = usize::try_from(parse_decimal(len)?).ok()?;
        let text = rest.get(..len)?;
        self.0 = rest.get(len..)?;
        Some(text)
    }

    fn number(&mut self) -> Option<u64> {
        parse_decimal(self.field()?)
    }

    /// `Ok(None)` for an empty field, `Ok(Some(text))` for `=text`.
    fn optional(&mut self) -> Result<Option<&'a str>, Malformed> {
        let text = self.field().ok_or(Malformed)?;
        if text.is_empty() {
            return Ok(None);
        }
        text.strip_prefix('=').map(Some).ok_or(Malformed)
    }
}

/// Encodes `entry` as a record.
#[must_use]
pub fn encode(entry: &Entry) -> String {
    let mut w = Writer(String::with_capacity(160));
    let action = &entry.action;
    w.field(VERSION);
    w.field(action.idempotency_key.as_str());
    w.field(&action.agent_id.to_string());
    w.number(action.expected_seq.0);
    w.number(action.created_at.unix_millis());
    w.field(match action.origin {
        Origin::App => "a",
        Origin::Notification => "n",
        Origin::Car => "c",
    });
    match &action.kind {
        // A prompt with photos is `P`, with the ids comma-separated, so
        // records made before photos existed still read as `p`.
        ActionKind::Prompt(p) if p.attachments.is_empty() => {
            w.field("p");
            w.field(p.text.as_str());
        }
        ActionKind::Prompt(p) => {
            w.field("P");
            w.field(p.text.as_str());
            let ids: Vec<&str> = p
                .attachments
                .as_slice()
                .iter()
                .map(AttachmentId::as_str)
                .collect();
            w.field(&ids.join(","));
        }
        ActionKind::Choose(c) => {
            w.field("c");
            w.field(c.pending_id.as_str());
            w.field(c.option_id.as_str());
            w.optional(c.feedback_text.as_ref().map(FeedbackText::as_str));
        }
        ActionKind::Keys(k) => {
            w.field("k");
            w.optional(k.pending_id.as_ref().map(PendingId::as_str));
            // One letter per key: its index in `AllowedKey::ALL`.
            let keys: String = k
                .keys
                .as_slice()
                .iter()
                .filter_map(|key| AllowedKey::ALL.iter().position(|k| k == key))
                .map(letter)
                .collect();
            w.field(&keys);
        }
        ActionKind::Interrupt(_) => w.field("i"),
    }
    w.optional(entry.label.as_deref());
    w.field(if entry.chained { "1" } else { "" });
    w.optional(
        entry
            .settled_at
            .map(|at| decimal(at.unix_millis()))
            .as_deref(),
    );
    match &entry.delivery {
        Delivery::Queued { attempts, retry_at } => {
            w.field("q");
            w.number(u64::from(*attempts));
            w.number(retry_at.unix_millis());
        }
        Delivery::Sending { attempts } => {
            w.field("s");
            w.number(u64::from(*attempts));
        }
        Delivery::Sent => w.field("t"),
        Delivery::Stale { current } => {
            w.field("x");
            w.optional(current.map(|seq| decimal(seq.0)).as_deref());
        }
        Delivery::Refused { reason, message } => {
            w.field("r");
            let at = REASONS.iter().position(|r| r == reason).unwrap_or(9);
            w.field(letter(at).encode_utf8(&mut [0; 4]));
            w.field(message);
        }
        Delivery::Failed { message } => {
            w.field("f");
            w.field(message);
        }
    }
    w.0
}

/// The rest of a `P` record's kind: a prompt's text and its photos' ids.
fn photo_prompt(r: &mut Reader<'_>) -> Option<ActionKind> {
    let text = PromptText::new(r.field()?).ok()?;
    let ids = r
        .field()?
        .split(',')
        .map(|id| AttachmentId::new(id).ok())
        .collect::<Option<Vec<_>>>()?;
    Some(ActionKind::Prompt(PromptPayload {
        text,
        attachments: Attachments::new(ids).ok()?,
    }))
}

/// Decodes a record made by [`encode`]; `None` if it isn't one.
#[must_use]
pub fn decode(record: &str) -> Option<Entry> {
    let mut r = Reader(record);
    if r.field()? != VERSION {
        return None;
    }
    let idempotency_key = IdempotencyKey::new(r.field()?).ok()?;
    let agent_id: AgentId = r.field()?.parse().ok()?;
    let expected_seq = Seq(r.number()?);
    let created_at = Timestamp::from_unix_millis(r.number()?);
    let origin = match r.field()? {
        "a" => Origin::App,
        "n" => Origin::Notification,
        "c" => Origin::Car,
        _ => return None,
    };
    let kind = match r.field()? {
        "p" => ActionKind::Prompt(PromptPayload::text(PromptText::new(r.field()?).ok()?)),
        "P" => photo_prompt(&mut r)?,
        "c" => ActionKind::Choose(ChoosePayload {
            pending_id: PendingId::new(r.field()?).ok()?,
            option_id: OptionId::new(r.field()?).ok()?,
            feedback_text: match r.optional().ok()? {
                Some(text) => Some(FeedbackText::new(text).ok()?),
                None => None,
            },
        }),
        "k" => {
            let pending_id = match r.optional().ok()? {
                Some(id) => Some(PendingId::new(id).ok()?),
                None => None,
            };
            let keys = r
                .field()?
                .bytes()
                .map(|b| AllowedKey::ALL.get(index(b)?).copied())
                .collect::<Option<Vec<_>>>()?;
            ActionKind::Keys(KeysPayload {
                pending_id,
                keys: KeySequence::new(keys).ok()?,
            })
        }
        "i" => ActionKind::Interrupt(InterruptPayload {}),
        _ => return None,
    };
    let label = r.optional().ok()?.map(str::to_owned);
    let chained = !r.field()?.is_empty();
    let settled_at = match r.optional().ok()? {
        Some(at) => Some(Timestamp::from_unix_millis(parse_decimal(at)?)),
        None => None,
    };
    let attempts = |r: &mut Reader| u32::try_from(r.number()?).ok();
    let delivery = match r.field()? {
        "q" => Delivery::Queued {
            attempts: attempts(&mut r)?,
            retry_at: Timestamp::from_unix_millis(r.number()?),
        },
        "s" => Delivery::Sending {
            attempts: attempts(&mut r)?,
        },
        "t" => Delivery::Sent,
        "x" => Delivery::Stale {
            current: match r.optional().ok()? {
                Some(seq) => Some(Seq(parse_decimal(seq)?)),
                None => None,
            },
        },
        "r" => Delivery::Refused {
            reason: match r.field()?.as_bytes() {
                [b] => *REASONS.get(index(*b)?)?,
                _ => return None,
            },
            message: r.field()?.to_owned(),
        },
        "f" => Delivery::Failed {
            message: r.field()?.to_owned(),
        },
        _ => return None,
    };
    if !r.0.is_empty() {
        return None;
    }
    Some(Entry {
        action: Action {
            idempotency_key,
            agent_id,
            expected_seq,
            created_at,
            kind,
            origin,
        },
        label,
        delivery,
        chained,
        settled_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";

    fn action(kind: ActionKind, origin: Origin) -> Action {
        Action {
            idempotency_key: IdempotencyKey::new(KEY).unwrap(),
            agent_id: "w1:p1".parse().unwrap(),
            expected_seq: Seq(42),
            created_at: Timestamp::from_unix_millis(1_700_000_000_123),
            kind,
            origin,
        }
    }

    fn kinds() -> Vec<ActionKind> {
        vec![
            ActionKind::Prompt(PromptPayload::text(
                PromptText::new("line one\nline 2: \"quoted\" 12:34 é 🚀").unwrap(),
            )),
            ActionKind::Prompt(PromptPayload {
                text: PromptText::new("see: these").unwrap(),
                attachments: Attachments::new(vec![
                    AttachmentId::new("00000000-0000-4000-8000-0000000000a1").unwrap(),
                    AttachmentId::new("00000000-0000-4000-8000-0000000000b2").unwrap(),
                ])
                .unwrap(),
            }),
            ActionKind::Choose(ChoosePayload {
                pending_id: PendingId::new("perm-7").unwrap(),
                option_id: OptionId::new("deny").unwrap(),
                feedback_text: Some(FeedbackText::new("not now: later").unwrap()),
            }),
            ActionKind::Choose(ChoosePayload {
                pending_id: PendingId::new("perm-7").unwrap(),
                option_id: OptionId::new("allow").unwrap(),
                feedback_text: None,
            }),
            ActionKind::Keys(KeysPayload {
                pending_id: Some(PendingId::new("screen-5-aaaaaaaaaaaa").unwrap()),
                keys: KeySequence::new(AllowedKey::ALL[..16].to_vec()).unwrap(),
            }),
            ActionKind::Keys(KeysPayload {
                pending_id: None,
                keys: KeySequence::new(AllowedKey::ALL[16..].to_vec()).unwrap(),
            }),
            ActionKind::Interrupt(InterruptPayload {}),
        ]
    }

    fn deliveries() -> Vec<Delivery> {
        let mut all = vec![
            Delivery::Queued {
                attempts: 3,
                retry_at: Timestamp::from_unix_millis(99),
            },
            Delivery::Sending { attempts: 1 },
            Delivery::Sent,
            Delivery::Stale { current: None },
            Delivery::Stale {
                current: Some(Seq(7)),
            },
            Delivery::Failed {
                message: "5:x:y — odd".into(),
            },
        ];
        all.extend(REASONS.iter().map(|&reason| Delivery::Refused {
            reason,
            message: String::new(),
        }));
        all
    }

    #[test]
    fn every_entry_round_trips() {
        for kind in kinds() {
            for delivery in deliveries() {
                for origin in [Origin::App, Origin::Notification, Origin::Car] {
                    let entry = Entry {
                        action: action(kind.clone(), origin),
                        label: Some("Allow: once".into()),
                        delivery: delivery.clone(),
                        chained: true,
                        settled_at: Some(Timestamp::from_unix_millis(5)),
                    };
                    assert_eq!(decode(&encode(&entry)).as_ref(), Some(&entry));
                    let plain = Entry {
                        label: None,
                        chained: false,
                        settled_at: None,
                        ..entry
                    };
                    assert_eq!(decode(&encode(&plain)).as_ref(), Some(&plain));
                }
            }
        }
    }

    #[test]
    fn decimals_round_trip_and_reject_junk() {
        for n in [0, 7, 10, 1_700_000_000_123, u64::MAX] {
            assert_eq!(parse_decimal(&decimal(n)), Some(n));
        }
        for bad in ["", "-1", "1a", " 1", "18446744073709551616"] {
            assert_eq!(parse_decimal(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn junk_is_unreadable() {
        let entry = Entry::queued(action(kinds().remove(0), Origin::App), None);
        let good = encode(&entry);
        assert!(decode(&good).is_some());
        for bad in [
            "",
            "{\"action\":{}}",
            "1:2",
            &good[..good.len() - 1],
            &format!("{good}0:"),
            &good.replacen("1:1", "1:9", 1),
        ] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn every_refusal_reason_has_an_index() {
        // A new variant breaks this match: add it to `REASONS` too.
        for reason in REASONS {
            match reason {
                RefusalReason::CarPolicy
                | RefusalReason::StepUpRequired
                | RefusalReason::UnknownAgent
                | RefusalReason::UnknownOption
                | RefusalReason::NoPendingInput
                | RefusalReason::FeedbackNotAccepted
                | RefusalReason::RateLimited
                | RefusalReason::IdempotencyConflict
                | RefusalReason::InvalidAction
                | RefusalReason::UnknownAttachment
                | RefusalReason::Other => {}
            }
        }
        for (i, reason) in REASONS.iter().enumerate() {
            assert_eq!(REASONS.iter().position(|r| r == reason), Some(i));
        }
    }
}
