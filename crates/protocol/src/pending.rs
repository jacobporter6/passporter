//! What a blocked agent is asking (`GET /v1/agents/:id/pending`, design §6.2).
//! Outbound: tolerant, except that option ids must be unique.
//!
//! Option key sequences are bridge-internal and not on the wire: clients
//! identify an option by its [`OptionId`] only.

use core::fmt;

use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::ValidationError;
use crate::ids::{OptionId, PendingId};
use crate::loose::{Loose, required, set_once};

/// Maximum number of lines in a [`ScreenTail`].
pub const MAX_SCREEN_TAIL_LINES: usize = 30;

/// A normalized question, permission prompt or plan approval.
///
/// `Deserialize` is hand-written (see [`crate::loose`]) and accepts exactly
/// what the derived `Serialize` writes, plus unknown fields and sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInput {
    /// Bridge-assigned id; `choose` (and optionally `keys`) echo it, and the
    /// stale guard returns 409 when it no longer matches.
    pub id: PendingId,
    /// What kind of input is wanted.
    pub kind: PendingKind,
    /// A short line, e.g. ``Run `pnpm test`?``. Used for notifications and
    /// read aloud in the car, so it never contains code blocks.
    pub prompt: String,
    /// The command, file or plan body, for the full view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Discrete answers, rendered as chips. Ids are unique.
    #[serde(default)]
    pub options: PendingOptions,
    /// 2–3 model-generated short replies. Offered, never auto-sent.
    #[serde(default)]
    pub suggestions: Vec<String>,
    /// Where the bridge learned about the input: `"source"` on the wire, plus
    /// `confidence` and `screenTail` for screen-parsed input.
    #[serde(flatten)]
    pub source: PendingSource,
}

impl PendingInput {
    /// Whether the input is a dialog that consumes keystrokes, so a prompt
    /// typed now would land in it: it has options, or it is a permission,
    /// plan approval or unknown kind. A question or free-text request without
    /// options is not a dialog; a prompt answers it.
    #[must_use]
    pub fn is_dialog(&self) -> bool {
        !self.options.as_slice().is_empty()
            || matches!(
                self.kind,
                PendingKind::Permission | PendingKind::PlanApproval | PendingKind::Unknown
            )
    }

    /// The input the bridge uses for policy decisions when herdr reports the
    /// agent `blocked` but no structured input is known (no hook event, and
    /// the screen did not parse). It is a [dialog](Self::is_dialog) with no
    /// options: prompts need step-up and no option can be chosen.
    #[must_use]
    pub fn unparsed_dialog(id: PendingId) -> Self {
        Self {
            id,
            kind: PendingKind::Unknown,
            prompt: String::new(),
            detail: None,
            options: PendingOptions::default(),
            suggestions: Vec::new(),
            source: PendingSource::Unknown,
        }
    }
}

/// The kind of a [`PendingInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingKind {
    /// A tool-permission dialog (*Allow once* / *Always allow* / *Deny*).
    Permission,
    /// A question, e.g. "shall I proceed?" at the end of a turn.
    Question,
    /// A plan waiting for approval.
    PlanApproval,
    /// The agent wants free text.
    FreeText,
    /// A kind this app predates; show the prompt and the raw-keys pad.
    #[serde(other)]
    Unknown,
}

/// The options of a [`PendingInput`], with unique ids (checked on
/// construction and deserialization).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "Vec<PendingOption>", into = "Vec<PendingOption>")]
pub struct PendingOptions(Vec<PendingOption>);

impl PendingOptions {
    /// Validates that option ids are unique.
    pub fn new(options: Vec<PendingOption>) -> Result<Self, ValidationError> {
        for (i, option) in options.iter().enumerate() {
            if options[..i].iter().any(|earlier| earlier.id == option.id) {
                return Err(ValidationError::DuplicateOptionId(option.id.to_string()));
            }
        }
        Ok(Self(options))
    }

    /// Looks up an option by id.
    #[must_use]
    pub fn get(&self, id: &OptionId) -> Option<&PendingOption> {
        self.0.iter().find(|option| &option.id == id)
    }

    /// The options, in display order.
    #[must_use]
    pub fn as_slice(&self) -> &[PendingOption] {
        &self.0
    }
}

impl TryFrom<Vec<PendingOption>> for PendingOptions {
    type Error = ValidationError;

    fn try_from(options: Vec<PendingOption>) -> Result<Self, Self::Error> {
        Self::new(options)
    }
}

impl From<PendingOptions> for Vec<PendingOption> {
    fn from(options: PendingOptions) -> Self {
        options.0
    }
}

/// One discrete answer to a [`PendingInput`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingOption {
    /// Stable id, echoed back in a `choose` action.
    pub id: OptionId,
    /// Chip label, e.g. *Allow once*.
    pub label: String,
    /// Whether choosing it needs step-up and is barred from the car.
    pub risk: Risk,
    /// Whether the option takes a `feedbackText` (e.g. *Deny*, whose "tell
    /// Claude what to do differently" box receives it). A `choose` with
    /// feedback on any other option is refused. Defaults to `false`.
    #[serde(default)]
    pub accepts_feedback: bool,
}

/// The risk of choosing an option (design §8 step-up, §6.6 car policy).
///
/// *Always allow* is always [`Risk::Elevated`]. An unrecognized value
/// deserializes as `Elevated`, so an unknown risk fails safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Safe to choose with a tap, from a notification or from the car.
    Low,
    /// Needs biometric step-up; never from the car.
    #[serde(other)]
    Elevated,
}

/// Where a [`PendingInput`] came from, tagged by `"source"`. Confidence and
/// the screen tail exist only for screen-parsed input. It is only ever on
/// the wire flattened into its input, so it has no `Deserialize` of its own:
/// [`PendingInput`]'s decodes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "source",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PendingSource {
    /// A Claude Code hook event: structured and reliable.
    Hook,
    /// Parsed from the terminal screen: best effort.
    Screen {
        /// How sure the parser is of the options.
        confidence: Confidence,
        /// The last screen lines, for the keypad fallback (design §6.2).
        #[serde(default)]
        screen_tail: ScreenTail,
    },
    /// A source this app predates.
    #[serde(other)]
    Unknown,
}

/// A parser's confidence, as an integer percentage `0..=100`.
///
/// [`Confidence::new`] rejects values above 100. Deserialization is tolerant
/// (outbound type): any non-negative integer is accepted and values above
/// 100 are clamped to 100, so a newer bridge cannot break an older app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(from = "u64", into = "u8")]
pub struct Confidence(u8);

impl From<u64> for Confidence {
    /// Clamps to `0..=100`.
    fn from(percent: u64) -> Self {
        Self(u8::try_from(percent.min(100)).unwrap_or(100))
    }
}

impl Confidence {
    /// Validates `percent <= 100`.
    pub fn new(percent: u8) -> Result<Self, ValidationError> {
        if percent > 100 {
            return Err(ValidationError::OutOfRange {
                field: "confidence",
                max: 100,
                actual: percent.into(),
            });
        }
        Ok(Self(percent))
    }

    /// The percentage, `0..=100`.
    #[must_use]
    pub const fn percent(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Confidence {
    type Error = ValidationError;

    fn try_from(percent: u8) -> Result<Self, Self::Error> {
        Self::new(percent)
    }
}

impl From<Confidence> for u8 {
    fn from(confidence: Confidence) -> Self {
        confidence.0
    }
}

/// The last at most [`MAX_SCREEN_TAIL_LINES`] lines of the agent's screen.
///
/// Longer input is truncated to its last lines (never rejected), both on
/// construction and on deserialization, so a newer bridge cannot break an
/// older app. The lines are untrusted terminal output: render as plain text.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "Vec<String>", into = "Vec<String>")]
pub struct ScreenTail(Vec<String>);

impl ScreenTail {
    /// Keeps the last [`MAX_SCREEN_TAIL_LINES`] of `lines`.
    #[must_use]
    pub fn new(mut lines: Vec<String>) -> Self {
        let excess = lines.len().saturating_sub(MAX_SCREEN_TAIL_LINES);
        lines.drain(..excess);
        Self(lines)
    }

    /// The lines, top to bottom.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.0
    }
}

impl From<Vec<String>> for ScreenTail {
    fn from(lines: Vec<String>) -> Self {
        Self::new(lines)
    }
}

impl From<ScreenTail> for Vec<String> {
    fn from(tail: ScreenTail) -> Self {
        tail.0
    }
}

impl<'de> Deserialize<'de> for PendingInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(PendingInputVisitor)
    }
}

struct PendingInputVisitor;

impl<'de> Visitor<'de> for PendingInputVisitor {
    type Value = PendingInput;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a pending input")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PendingInput, A::Error> {
        let (mut id, mut kind, mut prompt, mut detail) = (None, None, None, None);
        let (mut options, mut suggestions) = (None, None);
        let (mut source, mut confidence, mut screen_tail) = (None, None, None::<Loose>);
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "id" => set_once(&mut id, "id", map.next_value()?)?,
                "kind" => set_once(&mut kind, "kind", map.next_value()?)?,
                "prompt" => set_once(&mut prompt, "prompt", map.next_value()?)?,
                "detail" => set_once(&mut detail, "detail", map.next_value::<Option<String>>()?)?,
                "options" => set_once(&mut options, "options", map.next_value()?)?,
                "suggestions" => set_once(&mut suggestions, "suggestions", map.next_value()?)?,
                "source" => set_once(&mut source, "source", map.next_value::<String>()?)?,
                "confidence" => set_once(&mut confidence, "confidence", map.next_value()?)?,
                "screenTail" => set_once(&mut screen_tail, "screenTail", map.next_value()?)?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let source = match required::<_, A::Error>(source, "source")?.as_str() {
            "hook" => PendingSource::Hook,
            "screen" => PendingSource::Screen {
                confidence: Confidence::from(
                    required::<Loose, _>(confidence, "confidence")?.uint()?,
                ),
                screen_tail: screen_tail.map_or(Ok(Vec::new()), Loose::strings)?.into(),
            },
            _ => PendingSource::Unknown,
        };
        Ok(PendingInput {
            id: required(id, "id")?,
            kind: required(kind, "kind")?,
            prompt: required(prompt, "prompt")?,
            detail: detail.flatten(),
            options: options.unwrap_or_default(),
            suggestions: suggestions.unwrap_or_default(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str) -> PendingOption {
        PendingOption {
            id: OptionId::new(id).unwrap(),
            label: id.into(),
            risk: Risk::Low,
            accepts_feedback: false,
        }
    }

    #[test]
    fn unknown_risk_fails_safe() {
        assert_eq!(
            serde_json::from_str::<Risk>("\"medium\"").unwrap(),
            Risk::Elevated
        );
        assert_eq!(serde_json::from_str::<Risk>("\"low\"").unwrap(), Risk::Low);
    }

    #[test]
    fn unknown_kind_and_source_fall_back() {
        assert_eq!(
            serde_json::from_str::<PendingKind>("\"poll\"").unwrap(),
            PendingKind::Unknown
        );
        let pending: PendingInput =
            serde_json::from_str(r#"{"id":"p","kind":"question","prompt":"?","source":"psychic"}"#)
                .unwrap();
        assert_eq!(pending.source, PendingSource::Unknown);
    }

    #[test]
    fn option_keys_are_not_on_the_wire_and_extra_fields_are_ignored() {
        let json = r#"{"id":"x","label":"X","keys":["shift+tab"],"risk":"low"}"#;
        assert_eq!(
            serde_json::from_str::<PendingOption>(json).unwrap(),
            option_with("x", "X")
        );
    }

    fn option_with(id: &str, label: &str) -> PendingOption {
        PendingOption {
            label: label.into(),
            ..option(id)
        }
    }

    #[test]
    fn option_ids_must_be_unique() {
        assert_eq!(
            PendingOptions::new(vec![option("a"), option("b"), option("a")]),
            Err(ValidationError::DuplicateOptionId("a".into()))
        );
        let json = r#"{"id":"p","kind":"permission","prompt":"Run?","source":"hook",
            "options":[{"id":"a","label":"A","risk":"low"},{"id":"a","label":"B","risk":"low"}]}"#;
        assert!(serde_json::from_str::<PendingInput>(json).is_err());
    }

    #[test]
    fn option_lookup() {
        let options = PendingOptions::new(vec![option("allow_once"), option("deny")]).unwrap();
        assert_eq!(options.as_slice().len(), 2);
        assert!(options.get(&OptionId::new("allow_once").unwrap()).is_some());
        assert!(options.get(&OptionId::new("nope").unwrap()).is_none());
    }

    fn input(kind: PendingKind, options: Vec<PendingOption>) -> PendingInput {
        PendingInput {
            id: PendingId::new("p1").unwrap(),
            kind,
            prompt: "?".into(),
            detail: None,
            options: PendingOptions::new(options).unwrap(),
            suggestions: vec![],
            source: PendingSource::Hook,
        }
    }

    #[test]
    fn dialog_classification() {
        assert!(input(PendingKind::Permission, vec![]).is_dialog());
        assert!(input(PendingKind::PlanApproval, vec![]).is_dialog());
        assert!(input(PendingKind::Unknown, vec![]).is_dialog());
        assert!(input(PendingKind::Question, vec![option("yes")]).is_dialog());
        assert!(!input(PendingKind::Question, vec![]).is_dialog());
        assert!(!input(PendingKind::FreeText, vec![]).is_dialog());
        let unparsed = PendingInput::unparsed_dialog(PendingId::new("blocked-7").unwrap());
        assert!(unparsed.is_dialog());
        assert_eq!(unparsed.options, PendingOptions::default());
        assert_eq!(unparsed.source, PendingSource::Unknown);
    }

    #[test]
    fn pending_id_is_required() {
        assert!(
            serde_json::from_str::<PendingInput>(
                r#"{"kind":"question","prompt":"?","source":"hook"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn confidence_is_a_percentage() {
        assert_eq!(Confidence::new(100).map(Confidence::percent), Ok(100));
        assert_eq!(
            Confidence::new(101),
            Err(ValidationError::OutOfRange {
                field: "confidence",
                max: 100,
                actual: 101
            })
        );
        assert_eq!(
            serde_json::from_str::<Confidence>("101").unwrap(),
            Confidence::new(100).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Confidence>("100000").unwrap(),
            Confidence::new(100).unwrap()
        );
        assert!(serde_json::from_str::<Confidence>("-1").is_err());
        assert_eq!(
            serde_json::from_str::<Confidence>("42").unwrap(),
            Confidence::new(42).unwrap()
        );
    }

    #[test]
    fn screen_tail_keeps_the_last_lines() {
        let lines: Vec<String> = (0..40).map(|i| i.to_string()).collect();
        let tail = ScreenTail::new(lines.clone());
        assert_eq!(tail.lines().len(), MAX_SCREEN_TAIL_LINES);
        assert_eq!(tail.lines()[0], "10");
        let parsed: ScreenTail =
            serde_json::from_str(&serde_json::to_string(&lines).unwrap()).unwrap();
        assert_eq!(parsed, tail);
        assert_eq!(ScreenTail::new(vec!["a".into()]).lines(), ["a"]);
    }

    #[test]
    fn screen_source_without_tail() {
        let pending: PendingInput = serde_json::from_str(
            r#"{"id":"p","kind":"question","prompt":"?","source":"screen","confidence":40}"#,
        )
        .unwrap();
        assert_eq!(
            pending.source,
            PendingSource::Screen {
                confidence: Confidence::new(40).unwrap(),
                screen_tail: ScreenTail::default()
            }
        );
    }

    #[test]
    fn unknown_sources_take_fields_of_any_shape() {
        let pending: PendingInput = serde_json::from_str(
            r#"{"confidence":"high","screenTail":[1,{}],"id":"p","kind":"question","prompt":"?",
            "source":"telepathy"}"#,
        )
        .unwrap();
        assert_eq!(pending.source, PendingSource::Unknown);
    }

    #[test]
    fn screen_source_is_checked() {
        let parse = |rest: &str| {
            serde_json::from_str::<PendingInput>(&format!(
                r#"{{"id":"p","kind":"question","prompt":"?","source":"screen"{rest}}}"#
            ))
        };
        let tail: Vec<String> = (0..35).map(|i| format!("\"{i}\"")).collect();
        let PendingSource::Screen {
            confidence,
            screen_tail,
        } = parse(&format!(
            r#","confidence":250,"screenTail":[{}]"#,
            tail.join(",")
        ))
        .unwrap()
        .source
        else {
            panic!("not a screen source");
        };
        assert_eq!(confidence.percent(), 100);
        assert_eq!(screen_tail.lines().len(), MAX_SCREEN_TAIL_LINES);
        assert_eq!(screen_tail.lines()[0], "5");
        assert!(parse("").is_err(), "confidence is required");
        assert!(parse(r#","confidence":-1"#).is_err());
        assert!(parse(r#","confidence":5,"screenTail":["a",2]"#).is_err());
        assert!(parse(r#","confidence":5,"screenTail":null"#).is_err());
    }

    #[test]
    fn required_and_duplicate_fields() {
        let parse = |json: &str| serde_json::from_str::<PendingInput>(json);
        assert!(parse(r#"{"id":"p","kind":"question","prompt":"?"}"#).is_err());
        assert!(parse(r#"{"id":"p","kind":"question","source":"hook"}"#).is_err());
        assert!(
            parse(r#"{"id":"p","kind":"question","prompt":"?","source":"hook","source":"hook"}"#)
                .is_err()
        );
        let pending = parse(
            r#"{"id":"p","kind":"free_text","prompt":"?","detail":null,"source":"hook","x":[{}]}"#,
        )
        .unwrap();
        assert_eq!(pending.detail, None);
        assert_eq!(pending.kind, PendingKind::FreeText);
    }
}
