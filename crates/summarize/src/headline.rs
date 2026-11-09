//! Turn headlines: one sentence per finished turn, long or short (design
//! §6.4), shown at the top of the turn in the thread and as the agent's
//! card subtitle in triage.
//!
//! Like the TL;DR, the model's answer is untrusted: it is parsed strictly
//! (unknown fields rejected), whitespace-collapsed, trimmed at a word
//! boundary to [`MAX_HEADLINE_CHARS`] and checked with protocol's text
//! rules. Output longer than [`MAX_HEADLINE_SCHEMA_CHARS`] is rejected.

use core::fmt;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::text::clean_line;

pub use protocol::MAX_HEADLINE_CHARS;

/// The length the output schema allows, with slack over the
/// [`MAX_HEADLINE_CHARS`] display cap: a hard cap at the display length
/// made the model retry (and fail) over a single character. Validation
/// trims anything between the two caps at a word boundary and rejects
/// longer output.
pub const MAX_HEADLINE_SCHEMA_CHARS: usize = 140;
const _: () = assert!(
    MAX_HEADLINE_SCHEMA_CHARS > MAX_HEADLINE_CHARS,
    "the schema needs slack over the display cap"
);

/// A validated headline: one line of at most [`MAX_HEADLINE_CHARS`]
/// characters, valid as protocol display text (no control, bidi or
/// invisible characters), never blank.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Headline(String);

impl Headline {
    /// Normalizes `text` into a headline: whitespace runs collapsed, cut at
    /// a word boundary to [`MAX_HEADLINE_CHARS`] (ending in `…` when cut).
    /// `None` if it is blank or has characters protocol text rejects.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        clean_line(text, MAX_HEADLINE_CHARS).map(Self)
    }

    /// The headline text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Headline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Headline> for String {
    fn from(value: Headline) -> Self {
        value.0
    }
}

/// The shape the model must produce; unknown fields fail validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHeadline {
    headline: String,
}

/// The JSON schema passed to the CLI (`--json-schema`).
pub(crate) fn headline_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "headline": {"type": "string", "maxLength": MAX_HEADLINE_SCHEMA_CHARS},
        },
        "required": ["headline"],
    })
}

/// Validates model output into a [`Headline`]. Fails if the value is not
/// exactly `{"headline": "<text>"}`, the text is unusable or longer than
/// [`MAX_HEADLINE_SCHEMA_CHARS`]; a headline over [`MAX_HEADLINE_CHARS`] is
/// trimmed at a word boundary.
pub fn validate_headline(value: Value) -> Result<Headline> {
    let raw: RawHeadline = serde_json::from_value(value)
        .map_err(|e| Error::InvalidOutput(format!("headline: {e}")))?;
    let len = raw.headline.chars().count();
    if len > MAX_HEADLINE_SCHEMA_CHARS {
        return Err(Error::InvalidOutput(format!(
            "headline: {len} characters, over the {MAX_HEADLINE_SCHEMA_CHARS} allowed"
        )));
    }
    Headline::new(&raw.headline)
        .ok_or_else(|| Error::InvalidOutput("headline: blank or has invalid characters".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_output_round_trips() {
        let headline =
            validate_headline(json!({"headline": "Fixed the flaky test; asks whether to push."}))
                .unwrap();
        assert_eq!(
            headline.as_str(),
            "Fixed the flaky test; asks whether to push."
        );
        assert_eq!(String::from(headline.clone()), headline.to_string());
    }

    #[test]
    fn rejects_malformed_extra_and_unusable_output() {
        for bad in [
            json!("just text"),
            json!({}),
            json!({"headline": 7}),
            json!({"headline": "ok", "command": "rm -rf /"}),
            json!({"headline": "   "}),
            json!({"headline": "a\u{202E}b"}),
            json!({"headline": "a\u{1b}[2Jb"}),
        ] {
            assert!(
                matches!(validate_headline(bad.clone()), Err(Error::InvalidOutput(_))),
                "{bad}"
            );
        }
    }

    /// `len` characters of space-separated words, the last word cut short.
    fn words(len: usize) -> String {
        "fixed the flaky test ".chars().cycle().take(len).collect()
    }

    #[test]
    fn headlines_between_the_caps_are_trimmed_at_a_word_boundary() {
        for len in MAX_HEADLINE_CHARS + 1..=MAX_HEADLINE_SCHEMA_CHARS {
            let raw = words(len);
            let headline = validate_headline(json!({"headline": raw})).unwrap();
            let text = headline.as_str();
            assert!(text.chars().count() <= MAX_HEADLINE_CHARS, "{len}: {text}");
            // A trailing space collapses away, which can bring it under the cap.
            let Some(kept) = text.strip_suffix('…') else {
                assert_eq!(text, raw.trim_end(), "{len}");
                continue;
            };
            assert!(raw.starts_with(kept), "{len}: {text}");
            assert_eq!(raw[kept.len()..].chars().next(), Some(' '), "{len}: {text}");
        }
    }

    #[test]
    fn headlines_at_the_cap_are_kept_and_whitespace_collapsed() {
        let raw = words(MAX_HEADLINE_CHARS - 1) + "x";
        assert_eq!(
            validate_headline(json!({"headline": raw.clone()}))
                .unwrap()
                .as_str(),
            raw
        );
        let headline = validate_headline(json!({"headline": "word\n\n".repeat(20)})).unwrap();
        assert_eq!(headline.as_str(), "word ".repeat(20).trim_end());
    }

    #[test]
    fn headlines_over_the_schema_cap_are_invalid() {
        for len in [MAX_HEADLINE_SCHEMA_CHARS + 1, 5_000] {
            let err = validate_headline(json!({"headline": words(len)})).unwrap_err();
            assert!(matches!(err, Error::InvalidOutput(_)), "{len}: {err}");
        }
    }

    #[test]
    fn schema_caps_the_length_with_slack_and_forbids_extra_properties() {
        let schema = headline_schema();
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["headline"]["maxLength"],
            json!(MAX_HEADLINE_SCHEMA_CHARS)
        );
        assert_eq!(schema["required"], json!(["headline"]));
    }
}
