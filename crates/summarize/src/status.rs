//! Live status lines: what a working agent is doing now, and what it is
//! waiting on if anything, from the tail of its turn in progress (design
//! §6.1, §6.4). Long-running agents rarely finish a turn, so they would
//! otherwise never get a headline.
//!
//! Like the headline, the model's answer is untrusted: it is parsed
//! strictly (unknown fields rejected), whitespace-collapsed, trimmed at a
//! word boundary to [`MAX_STATUS_LINE_CHARS`] and checked with protocol's
//! text rules. Output longer than [`MAX_STATUS_LINE_SCHEMA_CHARS`] is
//! rejected.

use core::fmt;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::text::clean_line;

pub use protocol::MAX_STATUS_LINE_CHARS;

/// The length the output schema allows, with slack over the
/// [`MAX_STATUS_LINE_CHARS`] display cap: a hard cap at the display
/// length made the model retry (and fail) over a single character.
/// Validation trims anything between the two caps at a word boundary and
/// rejects longer output.
pub const MAX_STATUS_LINE_SCHEMA_CHARS: usize = 112;
const _: () = assert!(
    MAX_STATUS_LINE_SCHEMA_CHARS > MAX_STATUS_LINE_CHARS,
    "the schema needs slack over the display cap"
);

/// A validated status line: one line of at most [`MAX_STATUS_LINE_CHARS`]
/// characters, valid as protocol display text (no control, bidi or
/// invisible characters), never blank.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatusText(String);

impl StatusText {
    /// Normalizes `text` into a status line: whitespace runs collapsed, cut
    /// at a word boundary to [`MAX_STATUS_LINE_CHARS`] (ending in `…` when
    /// cut). `None` if it
    /// is blank or has characters protocol text rejects.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        clean_line(text, MAX_STATUS_LINE_CHARS).map(Self)
    }

    /// The line.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StatusText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<StatusText> for String {
    fn from(value: StatusText) -> Self {
        value.0
    }
}

/// The shape the model must produce; unknown fields fail validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStatus {
    status_line: String,
}

/// The JSON schema passed to the CLI (`--json-schema`).
pub(crate) fn status_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "status_line": {"type": "string", "maxLength": MAX_STATUS_LINE_SCHEMA_CHARS},
        },
        "required": ["status_line"],
    })
}

/// Validates model output into a [`StatusText`]. Fails if the value is not
/// exactly `{"status_line": "<text>"}`, the text is unusable or longer
/// than [`MAX_STATUS_LINE_SCHEMA_CHARS`]; a line over
/// [`MAX_STATUS_LINE_CHARS`] is trimmed at a word boundary.
pub fn validate_status(value: Value) -> Result<StatusText> {
    let raw: RawStatus = serde_json::from_value(value)
        .map_err(|e| Error::InvalidOutput(format!("status line: {e}")))?;
    let len = raw.status_line.chars().count();
    if len > MAX_STATUS_LINE_SCHEMA_CHARS {
        return Err(Error::InvalidOutput(format!(
            "status line: {len} characters, over the {MAX_STATUS_LINE_SCHEMA_CHARS} allowed"
        )));
    }
    StatusText::new(&raw.status_line)
        .ok_or_else(|| Error::InvalidOutput("status line: blank or has invalid characters".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_output_round_trips() {
        let line = validate_status(json!({"status_line": "Running the test suite; waiting on CI"}))
            .unwrap();
        assert_eq!(line.as_str(), "Running the test suite; waiting on CI");
        assert_eq!(String::from(line.clone()), line.to_string());
    }

    #[test]
    fn rejects_malformed_extra_and_unusable_output() {
        for bad in [
            json!("just text"),
            json!({}),
            json!({"status_line": 7}),
            json!({"headline": "wrong field"}),
            json!({"status_line": "ok", "command": "rm -rf /"}),
            json!({"status_line": "   "}),
            json!({"status_line": "a\u{202E}b"}),
            json!({"status_line": "a\u{1b}[2Jb"}),
        ] {
            assert!(
                matches!(validate_status(bad.clone()), Err(Error::InvalidOutput(_))),
                "{bad}"
            );
        }
    }

    /// `len` characters of space-separated words, the last word cut short.
    fn words(len: usize) -> String {
        "running the test suite "
            .chars()
            .cycle()
            .take(len)
            .collect()
    }

    #[test]
    fn lines_between_the_caps_are_trimmed_at_a_word_boundary() {
        for len in MAX_STATUS_LINE_CHARS + 1..=MAX_STATUS_LINE_SCHEMA_CHARS {
            let raw = words(len);
            let line = validate_status(json!({"status_line": raw})).unwrap();
            let text = line.as_str();
            assert!(
                text.chars().count() <= MAX_STATUS_LINE_CHARS,
                "{len}: {text}"
            );
            // A trailing space collapses away, which can bring it under the cap.
            let Some(kept) = text.strip_suffix('…') else {
                assert_eq!(text, raw.trim_end(), "{len}");
                continue;
            };
            assert!(raw.starts_with(kept), "{len}: {text}");
            assert_eq!(raw[kept.len()..].chars().next(), Some(' '), "{len}: {text}");
        }
        let line = validate_status(json!({"status_line": "word\n\n".repeat(10)})).unwrap();
        assert_eq!(line.as_str(), "word ".repeat(10).trim_end());
    }

    #[test]
    fn lines_over_the_schema_cap_are_invalid() {
        for len in [MAX_STATUS_LINE_SCHEMA_CHARS + 1, 5_000] {
            let err = validate_status(json!({"status_line": words(len)})).unwrap_err();
            assert!(matches!(err, Error::InvalidOutput(_)), "{len}: {err}");
        }
    }

    #[test]
    fn schema_caps_the_length_with_slack_and_forbids_extra_properties() {
        let schema = status_schema();
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["status_line"]["maxLength"],
            json!(MAX_STATUS_LINE_SCHEMA_CHARS)
        );
        assert_eq!(schema["required"], json!(["status_line"]));
    }
}
