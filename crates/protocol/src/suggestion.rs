//! Reply suggestions (design §6.2): short replies offered as chips; a tap
//! sends one as a prompt, and none is ever sent without a tap.
//!
//! One rule for the bridge, which makes them, and the app, which checks
//! them again (they are model output about untrusted agent output), so
//! the two cannot drift apart.

use core::fmt;

use crate::error::ValidationError;
use crate::text::FeedbackText;

/// Longest suggestion, in characters.
pub const MAX_SUGGESTION_CHARS: usize = 60;

/// Most suggestions offered at once.
pub const MAX_SUGGESTIONS: usize = 3;

/// Why a candidate suggestion is not offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionRejected {
    /// Blank, or contains characters protocol text does not allow.
    Invalid(ValidationError),
    /// Longer than [`MAX_SUGGESTION_CHARS`].
    TooLong,
    /// Starts with a command prefix (`!`, `/`, `#`).
    Elevated,
}

impl fmt::Display for SuggestionRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => write!(f, "invalid suggestion text: {error}"),
            Self::TooLong => write!(
                f,
                "suggestion is longer than {MAX_SUGGESTION_CHARS} characters"
            ),
            Self::Elevated => f.write_str("suggestion is elevated (starts with a command prefix)"),
        }
    }
}

impl core::error::Error for SuggestionRejected {}

/// A candidate as it may be offered: whitespace runs collapsed (so it is
/// single-line), then at most [`MAX_SUGGESTION_CHARS`] characters, valid
/// [`FeedbackText`] (so it can be sent as a prompt or as option feedback)
/// and not [elevated](FeedbackText::is_elevated). Nothing is truncated,
/// since a cut reply could change its meaning.
pub fn suggestion(candidate: &str) -> Result<FeedbackText, SuggestionRejected> {
    let collapsed = candidate.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > MAX_SUGGESTION_CHARS {
        return Err(SuggestionRejected::TooLong);
    }
    let text = FeedbackText::new(collapsed).map_err(SuggestionRejected::Invalid)?;
    if text.is_elevated() {
        return Err(SuggestionRejected::Elevated);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_short_plain_single_line_replies_pass() {
        assert_eq!(
            suggestion("Yes,\n  go ahead").map(|t| t.as_str().to_owned()),
            Ok("Yes, go ahead".to_owned())
        );
        assert!(suggestion(&"é".repeat(MAX_SUGGESTION_CHARS)).is_ok());
        assert_eq!(
            suggestion(&"x".repeat(MAX_SUGGESTION_CHARS + 1)),
            Err(SuggestionRejected::TooLong)
        );
        for elevated in ["/clear", "!rm -rf ~", "#remember this", "  /compact"] {
            assert_eq!(suggestion(elevated), Err(SuggestionRejected::Elevated));
        }
        for invalid in ["   ", "ok\u{1b}[2J", "fine\u{202E}", "zero\u{200B}width"] {
            assert!(
                matches!(suggestion(invalid), Err(SuggestionRejected::Invalid(_))),
                "{invalid:?}"
            );
        }
    }
}
