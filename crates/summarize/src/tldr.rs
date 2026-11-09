//! Validation of the model's TL;DR into [`protocol::Tldr`].
//!
//! The model is constrained by a JSON schema, but its output is still
//! treated as untrusted: it is parsed strictly (unknown fields rejected),
//! every string is whitespace-collapsed, size-capped and checked with
//! protocol's text rules, and lists are cut to their maximum length.

use protocol::Tldr;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::text::clean_line;

/// Size caps for a validated [`Tldr`]. Tight on purpose: the TL;DR is read
/// at a glance on a phone (the corpus eval put the old caps' output at a
/// median of ~1060 characters; these aim under 450).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TldrLimits {
    /// Characters in `outcome` (at most two short sentences) and `decide`.
    pub sentence_chars: usize,
    /// Characters in one `changed` or `risks` bullet.
    pub bullet_chars: usize,
    /// Bullets in `changed`.
    pub changed_items: usize,
    /// Bullets in `risks` (none is the usual answer).
    pub risk_items: usize,
}

impl Default for TldrLimits {
    fn default() -> Self {
        Self {
            sentence_chars: 160,
            bullet_chars: 90,
            changed_items: 3,
            risk_items: 2,
        }
    }
}

/// The shape the model must produce. Mirrors [`Tldr`] but rejects unknown
/// fields, so a model talked into "adding" fields fails validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTldr {
    outcome: String,
    changed: Vec<String>,
    #[serde(default)]
    decide: Option<String>,
    risks: Vec<String>,
}

/// The JSON schema passed to the CLI (`--json-schema`).
pub(crate) fn tldr_schema(limits: &TldrLimits) -> Value {
    let bullets = |max: usize| {
        json!({
            "type": "array",
            "maxItems": max,
            "items": {"type": "string", "maxLength": limits.bullet_chars},
        })
    };
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "outcome": {"type": "string", "maxLength": limits.sentence_chars},
            "changed": bullets(limits.changed_items),
            "decide": {"type": ["string", "null"], "maxLength": limits.sentence_chars},
            "risks": bullets(limits.risk_items),
        },
        "required": ["outcome", "changed", "decide", "risks"],
    })
}

/// Validates model output into a display-safe [`Tldr`].
///
/// Fails if the value is not exactly the expected object or the outcome is
/// unusable. Over-long strings are cut; bullets that are blank or contain
/// control, bidi or invisible characters are dropped; a `decide` of "none"
/// (or similar) becomes absent.
pub fn validate_tldr(value: Value, limits: &TldrLimits) -> Result<Tldr> {
    let raw: RawTldr =
        serde_json::from_value(value).map_err(|e| Error::InvalidOutput(format!("tldr: {e}")))?;
    let outcome = clean_line(&raw.outcome, limits.sentence_chars).ok_or_else(|| {
        Error::InvalidOutput("tldr: outcome is blank or has invalid characters".into())
    })?;
    let bullets = |items: Vec<String>, max: usize| -> Vec<String> {
        items
            .iter()
            .filter_map(|item| clean_line(item, limits.bullet_chars))
            .take(max)
            .collect()
    };
    let decide = raw
        .decide
        .and_then(|d| clean_line(&d, limits.sentence_chars))
        .filter(|d| !is_nothing(d));
    Ok(Tldr {
        outcome,
        changed: bullets(raw.changed, limits.changed_items),
        decide,
        risks: bullets(raw.risks, limits.risk_items),
    })
}

/// Whether a `decide` value means "nothing to decide".
fn is_nothing(text: &str) -> bool {
    let text = text.trim_end_matches('.').to_ascii_lowercase();
    matches!(
        text.as_str(),
        "none" | "nothing" | "n/a" | "na" | "-" | "no"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(value: Value) -> Result<Tldr> {
        validate_tldr(value, &TldrLimits::default())
    }

    #[test]
    fn valid_output_round_trips() {
        let tldr = validate(json!({
            "outcome": "Tests pass after fixing the race.",
            "changed": ["src/lib.rs +11 -3"],
            "decide": null,
            "risks": ["No test for Windows paths"],
        }))
        .unwrap();
        assert_eq!(
            tldr,
            Tldr {
                outcome: "Tests pass after fixing the race.".into(),
                changed: vec!["src/lib.rs +11 -3".into()],
                decide: None,
                risks: vec!["No test for Windows paths".into()],
            }
        );
    }

    #[test]
    fn rejects_malformed_and_extra_fields() {
        for bad in [
            json!("just text"),
            json!({"outcome": "ok", "changed": [], "risks": [], "command": "rm -rf /"}),
            json!({"outcome": "ok", "changed": "not a list", "risks": []}),
            json!({"changed": [], "risks": []}),
            json!({"outcome": 7, "changed": [], "risks": []}),
            json!({"outcome": "   ", "changed": [], "risks": []}),
            json!({"outcome": "\u{202E}", "changed": [], "risks": []}),
        ] {
            assert!(
                matches!(validate(bad.clone()), Err(Error::InvalidOutput(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn oversized_output_is_capped_and_cleaned() {
        let limits = TldrLimits::default();
        let tldr = validate(json!({
            "outcome": "x".repeat(10_000),
            "changed": vec!["a\nb"; 50],
            "decide": "None.",
            "risks": ["ok", "", "bad\u{1b}[2J", "y".repeat(5_000)],
        }))
        .unwrap();
        assert_eq!(tldr.outcome.chars().count(), limits.sentence_chars);
        assert_eq!(tldr.changed, vec!["a b"; limits.changed_items]);
        assert_eq!(tldr.decide, None);
        assert_eq!(tldr.risks.len(), 2);
        assert_eq!(tldr.risks[1].chars().count(), limits.bullet_chars);
    }

    #[test]
    fn schema_forbids_extra_properties() {
        let schema = tldr_schema(&TldrLimits::default());
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["properties"]["changed"]["maxItems"], json!(3));
        assert_eq!(schema["properties"]["risks"]["maxItems"], json!(2));
        assert_eq!(schema["properties"]["outcome"]["maxLength"], json!(160));
        assert_eq!(schema["properties"]["decide"]["maxLength"], json!(160));
        let bullet = &schema["properties"]["changed"]["items"]["maxLength"];
        assert_eq!(*bullet, json!(90));
    }

    #[test]
    fn caps_bound_the_whole_tldr() {
        let limits = TldrLimits::default();
        assert_eq!(limits.changed_items, 3);
        assert_eq!(limits.risk_items, 2);
        // Even a TL;DR at every cap stays a phone-screen read.
        let most = 2 * limits.sentence_chars
            + (limits.changed_items + limits.risk_items) * limits.bullet_chars;
        assert!(most <= 800, "{most}");
    }

    #[test]
    fn no_risks_and_no_decision_are_valid_answers() {
        let tldr = validate(json!({
            "outcome": "Renamed the flag; tests pass.",
            "changed": ["cli.rs +4 -4"],
            "decide": null,
            "risks": [],
        }))
        .unwrap();
        assert_eq!(tldr.risks, Vec::<String>::new());
        assert_eq!(tldr.decide, None);
    }
}
