//! Validated free text that a client sends into an agent's terminal.
//!
//! Text ends up in a shell-hosted TUI, so besides size limits (design §8) it
//! must not carry characters that act as keystrokes or disguise what is sent:
//!
//! - control characters (category Cc: `ESC`, `\r`, `\t`, `NUL`, C1, ...) are
//!   rejected; an embedded `ESC` or `\r` would escape the fixed-verb model;
//! - format characters (category Cf) are rejected, **except** the zero-width
//!   joiners U+200C/U+200D that emoji and many scripts need. This covers the
//!   bidi embeddings, overrides, isolates and marks (U+202A–202E,
//!   U+2066–2069, U+200E/F, U+061C) and invisible characters (U+200B, U+2060,
//!   U+FEFF, ...). The text reviewed on screen is the text sent;
//! - the whole U+E0000–E0FFF block is rejected: tags, the variation-selector
//!   supplement and unassigned code points, all usable to smuggle hidden
//!   payloads;
//! - other invisible or reserved characters are rejected for the same reason:
//!   the default-ignorable fillers U+034F, U+115F, U+1160, U+3164, U+FFA0,
//!   U+180B–180F, U+17B4/U+17B5, the unassigned U+2065, and U+FFF0–FFF8;
//! - Unicode line/paragraph separators (U+2028, U+2029) are rejected;
//! - the emoji variation selectors U+FE00–FE0F are allowed (emoji
//!   presentation) but count as blank;
//! - text that is blank once whitespace, the joiners and the variation
//!   selectors are ignored is rejected as empty.
//!
//! [`PromptText`] additionally allows `\n`; [`FeedbackText`] is single-line.
//! There is no Unicode table dependency: the Cf set is listed explicitly
//! (Unicode 16) in `is_format_char`.
//!
//! Elevation ([`PromptText::is_elevated`], [`FeedbackText::is_elevated`]) is
//! structural rather than blocklist-based: text is elevated when `!`, `/` or
//! `#` occurs before its first alphanumeric character, so no invisible or odd
//! leading character can hide a command prefix.

use crate::error::ValidationError;
use crate::macros::validated_string;

/// Maximum size of a [`PromptText`], in UTF-8 bytes (16 KiB).
pub const MAX_PROMPT_BYTES: usize = 16 * 1024;

/// Maximum size of a [`FeedbackText`], in UTF-8 bytes (4 KiB).
pub const MAX_FEEDBACK_BYTES: usize = 4 * 1024;

/// Characters that make text elevated when they occur before its first
/// alphanumeric character: `!` runs a shell command, `/` a slash command,
/// `#` writes memory.
pub const ELEVATED_PROMPT_PREFIXES: [char; 3] = ['!', '/', '#'];

/// The zero-width non-joiner and joiner: the only allowed format characters.
fn is_joiner(c: char) -> bool {
    matches!(c, '\u{200C}' | '\u{200D}')
}

/// The emoji variation selectors (allowed, but invisible on their own).
fn is_variation_selector(c: char) -> bool {
    matches!(c, '\u{FE00}'..='\u{FE0F}')
}

/// General category Cf (format), per Unicode 16. The tag characters
/// (U+E0001, U+E0020–E007F) are covered by the U+E0000–E0FFF block rule.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
    )
}

/// Invisible, ignorable or reserved characters outside Cf.
fn is_invisible_or_reserved(c: char) -> bool {
    matches!(
        c,
        '\u{034F}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{17B4}'
            | '\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{2065}'
            | '\u{3164}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// Whether `c` is hidden or can disguise text: a format character other
/// than the zero-width joiners, an invisible, ignorable or reserved
/// character, or a Unicode line/paragraph separator. Control characters are
/// not included; callers decide about those (e.g. `\n`).
///
/// This is the rule [`PromptText`] and [`FeedbackText`] enforce, exported so
/// other untrusted text (agent titles shown on cards and in the car) can be
/// cleaned with exactly the same list.
#[must_use]
pub fn is_hidden_char(c: char) -> bool {
    (is_format_char(c) && !is_joiner(c))
        || is_invisible_or_reserved(c)
        || matches!(c, '\u{2028}' | '\u{2029}')
}

/// Whether [`PromptText`] (`allow_newline`) or [`FeedbackText`] would
/// reject `c`: a control character (other than an allowed `\n`) or a
/// [hidden](is_hidden_char) one. Exported so other untrusted text bound
/// for a prompt (dictation transcripts) is cleaned by the same rule.
#[must_use]
pub fn is_disallowed(c: char, allow_newline: bool) -> bool {
    if c == '\n' {
        return !allow_newline;
    }
    c.is_control() || is_hidden_char(c)
}

/// Whether `c` carries nothing visible on its own: whitespace, a zero-width
/// joiner or an emoji variation selector. Text made only of these is blank.
#[must_use]
pub fn is_blank_char(c: char) -> bool {
    c.is_whitespace() || is_joiner(c) || is_variation_selector(c)
}

/// Whether `!`, `/` or `#` occurs before the first alphanumeric character.
///
/// Known limit: a leading combining mark that Unicode counts as alphabetic
/// (e.g. U+0345) ends the scan, so `"\u{0345}/clear"` is not elevated. The
/// agent's input also sees a non-`/` first character there, so it is not run
/// as a command; catching it here would need Unicode category tables, which
/// this crate avoids to stay serde-only.
fn has_command_prefix(text: &str) -> bool {
    text.chars()
        .find(|&c| c.is_alphanumeric() || ELEVATED_PROMPT_PREFIXES.contains(&c))
        .is_some_and(|c| ELEVATED_PROMPT_PREFIXES.contains(&c))
}

pub(crate) fn check_text(
    field: &'static str,
    max: usize,
    allow_newline: bool,
    value: &str,
) -> Result<(), ValidationError> {
    if value.chars().all(is_blank_char) {
        return Err(ValidationError::Empty { field });
    }
    if value.len() > max {
        return Err(ValidationError::TooLong {
            field,
            max,
            actual: value.len(),
        });
    }
    if value.chars().any(|c| is_disallowed(c, allow_newline)) {
        return Err(ValidationError::ControlCharacter { field });
    }
    if has_invisible_run(value) {
        return Err(ValidationError::ControlCharacter { field });
    }
    Ok(())
}

/// Whether `c` is an invisible modifier in a place no visible text needs it,
/// given the character before it (`None` at the start): a variation selector
/// anywhere but directly after a visible character, or a joiner right after
/// another joiner. Such runs could smuggle hidden data into text that renders
/// identically. Emoji sequences such as `❤\u{FE0F}` and `❤\u{FE0F}\u{200D}🔥`
/// never trip it.
///
/// [`PromptText`] and [`FeedbackText`] reject text containing one; other
/// untrusted text (agent titles) can drop them with the same rule.
#[must_use]
pub fn is_misplaced_modifier(prev: Option<char>, c: char) -> bool {
    let after_visible = prev.is_some_and(|p| !is_blank_char(p));
    (is_variation_selector(c) && !after_visible) || (is_joiner(c) && prev.is_some_and(is_joiner))
}

fn has_invisible_run(text: &str) -> bool {
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if is_misplaced_modifier(prev, c) {
            return true;
        }
        prev = Some(c);
    }
    false
}

validated_string!(
    /// Text sent to an agent as a new prompt (`prompt` action): not blank, at
    /// most [`MAX_PROMPT_BYTES`], and free of the characters listed in the
    /// [module docs](self) except `\n`.
    ///
    /// **Delivery contract:** the bridge must deliver it through herdr's
    /// `agent prompt`, which wraps the text in bracketed paste, so an embedded
    /// `\n` is pasted as a line break, never sent as Enter. (`ESC` is
    /// rejected, so the text cannot end the paste early.)
    ///
    /// A prompt can be [elevated](Self::is_elevated); such prompts need
    /// step-up and are refused from the car.
    PromptText,
    |v| check_text("text", MAX_PROMPT_BYTES, true, v)
);

impl PromptText {
    /// Whether `!` (shell), `/` (slash command) or `#` (memory) occurs
    /// before the first alphanumeric character, whatever precedes it. Such
    /// prompts act on more than the conversation, so they need step-up
    /// (design §8).
    #[must_use]
    pub fn is_elevated(&self) -> bool {
        has_command_prefix(self.as_str())
    }
}

validated_string!(
    /// The "tell Claude what to do differently" text attached to a `choose`
    /// action, e.g. the reason for a *Deny* (design §6.2): not blank, at most
    /// [`MAX_FEEDBACK_BYTES`], and **single-line**, free of every character
    /// listed in the [module docs](self). It is typed into a dialog's text
    /// box, where `\n` would submit early and `\t` would move focus.
    ///
    /// It is only accepted on an option that takes feedback
    /// ([`PendingOption::accepts_feedback`](crate::PendingOption::accepts_feedback)),
    /// and elevated feedback needs step-up like an elevated prompt.
    FeedbackText,
    |v| check_text("feedbackText", MAX_FEEDBACK_BYTES, false, v)
);

impl FeedbackText {
    /// Same rule as [`PromptText::is_elevated`]: the text reaches the agent
    /// as a message, so a leading command prefix needs step-up.
    #[must_use]
    pub fn is_elevated(&self) -> bool {
        has_command_prefix(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_char_predicate_matches_the_text_rules() {
        for c in [
            '\u{202E}',
            '\u{200B}',
            '\u{3164}',
            '\u{115F}',
            '\u{034F}',
            '\u{180B}',
            '\u{0600}',
            '\u{2028}',
            '\u{E0041}',
        ] {
            assert!(is_hidden_char(c), "{:04X}", u32::from(c));
            assert!(
                PromptText::new(format!("a{c}b")).is_err(),
                "{:04X}",
                u32::from(c)
            );
        }
        for c in ['a', ' ', '\u{200D}', '\u{FE0F}', '👍'] {
            assert!(!is_hidden_char(c), "{:04X}", u32::from(c));
        }
        assert!(is_blank_char('\u{200D}') && is_blank_char(' ') && !is_blank_char('a'));
    }

    #[test]
    fn invisible_modifier_runs_are_rejected() {
        let payload = "Yes\u{FE00}\u{FE01}\u{FE0F}\u{FE05}\u{FE00}\u{FE0A}";
        assert!(
            PromptText::new(payload).is_err(),
            "variation-selector payload"
        );
        assert!(
            PromptText::new("ok \u{FE0F}").is_err(),
            "selector after whitespace"
        );
        assert!(
            PromptText::new("a\u{200D}\u{200D}b").is_err(),
            "consecutive joiners"
        );
        assert!(
            FeedbackText::new("no\u{FE0F}\u{FE0F}").is_err(),
            "feedback too"
        );
        for emoji in [
            "\u{2764}\u{FE0F}",
            "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\u{0645}\u{200C}\u{06CC}",
        ] {
            assert!(
                PromptText::new(format!("ok {emoji}")).is_ok(),
                "{emoji:?} should stay valid"
            );
        }
    }

    #[test]
    fn prompt_accepts_multiline_text_up_to_the_limit() {
        assert_eq!(
            PromptText::new("run the tests\nthen push")
                .unwrap()
                .as_str(),
            "run the tests\nthen push"
        );
        assert!(PromptText::new("x".repeat(MAX_PROMPT_BYTES)).is_ok());
        assert!(FeedbackText::new("x".repeat(MAX_FEEDBACK_BYTES)).is_ok());
    }

    #[test]
    fn accepts_unicode_including_joiners() {
        for ok in [
            "emoji ok: 🚗 ünïcödé",
            "family: 👨\u{200D}👩\u{200D}👧",
            "zwnj: می\u{200C}خواهم",
            "joiner then text: \u{200D}ok",
            "emoji presentation: \u{2764}\u{FE0F}",
        ] {
            assert!(PromptText::new(ok).is_ok(), "{ok:?}");
            assert!(FeedbackText::new(ok).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn rejects_blank() {
        for blank in [
            "",
            "   ",
            "\n ",
            "\u{200D}",
            " \u{200C}\u{200D} \n",
            "\u{FE0F}",
            " \u{FE00}\u{200D}",
        ] {
            assert_eq!(
                PromptText::new(blank),
                Err(ValidationError::Empty { field: "text" })
            );
        }
        assert_eq!(
            FeedbackText::new("  "),
            Err(ValidationError::Empty {
                field: "feedbackText"
            })
        );
    }

    #[test]
    fn rejects_oversized_counting_bytes() {
        assert_eq!(
            PromptText::new("x".repeat(MAX_PROMPT_BYTES + 1)),
            Err(ValidationError::TooLong {
                field: "text",
                max: MAX_PROMPT_BYTES,
                actual: MAX_PROMPT_BYTES + 1
            })
        );
        // 4-byte chars: 1025 of them is 4100 bytes > 4 KiB.
        assert_eq!(
            FeedbackText::new("🚗".repeat(1025)),
            Err(ValidationError::TooLong {
                field: "feedbackText",
                max: MAX_FEEDBACK_BYTES,
                actual: 4100
            })
        );
    }

    #[test]
    fn rejects_control_bidi_and_separator_characters() {
        for bad in [
            "a\u{1b}[A",
            "a\rb",
            "a\tb",
            "a\0b",
            "a\u{7f}",
            "a\u{9b}b",
            "a\u{3}",
            "a\u{202A}b",
            "a\u{202E}b",
            "a\u{2066}b",
            "a\u{2069}b",
            "a\u{2028}b",
            "a\u{2029}b",
            // Bidi marks.
            "a\u{200E}b",
            "a\u{200F}b",
            "a\u{061C}b",
            // Invisible format characters.
            "a\u{200B}b",
            "a\u{2060}b",
            "a\u{FEFF}b",
            "a\u{00AD}b",
            // Tag characters (hidden text).
            "a\u{E0001}b",
            "a\u{E0041}\u{E0042}b",
            "a\u{E007F}b",
            // Default-ignorable fillers.
            "a\u{034F}b",
            "a\u{115F}b",
            "a\u{1160}b",
            "a\u{3164}b",
            "a\u{FFA0}b",
            "a\u{180B}b",
            "a\u{180F}b",
            // Invisible prefixes that would hide an elevated command.
            "\u{FEFF}/clear",
            "\u{200B}!rm -rf x",
            // Only invisible content, but not blank by the joiner rule.
            "\u{200B}",
            // Variation-selector supplement, unassigned tag-block, Khmer
            // inherent vowels, unassigned U+2065, reserved specials.
            "\u{E0100}!ls",
            "\u{E0002}/clear",
            "a\u{E0FFF}b",
            "\u{17B4}/clear",
            "a\u{17B5}b",
            "\u{2065}/clear",
            "a\u{FFF0}b",
            "a\u{FFF8}b",
        ] {
            assert_eq!(
                PromptText::new(bad),
                Err(ValidationError::ControlCharacter { field: "text" }),
                "{bad:?}"
            );
            assert_eq!(
                FeedbackText::new(bad),
                Err(ValidationError::ControlCharacter {
                    field: "feedbackText"
                }),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn feedback_is_single_line() {
        assert_eq!(
            FeedbackText::new("no\n1"),
            Err(ValidationError::ControlCharacter {
                field: "feedbackText"
            })
        );
    }

    #[test]
    fn elevated_prompts() {
        for elevated in [
            "!rm -rf target",
            "/clear",
            "#remember this",
            "  \n !ls",
            "\u{200D}!ls",
            "\u{200C} \u{200D}/clear",
            "\u{200D}#note",
            "\u{200D}/clear",
            "- /clear",
            "?? !ls",
            "  (#note)",
        ] {
            assert!(
                PromptText::new(elevated).unwrap().is_elevated(),
                "{elevated:?}"
            );
        }
        for plain in [
            "run the tests",
            "what does !x do?",
            "a/b",
            "C# please",
            "2 / 3",
            "\u{200D}yes #1",
            "émoji! fine",
        ] {
            assert!(!PromptText::new(plain).unwrap().is_elevated(), "{plain:?}");
        }
    }

    #[test]
    fn elevated_feedback() {
        for elevated in ["/clear", "!rm -rf x", "\u{200D}#note"] {
            assert!(
                FeedbackText::new(elevated).unwrap().is_elevated(),
                "{elevated:?}"
            );
        }
        assert!(
            !FeedbackText::new("use nextest, not /bin/test")
                .unwrap()
                .is_elevated()
        );
    }

    #[test]
    fn deserialization_validates() {
        assert!(serde_json::from_str::<PromptText>("\"\\u001b[2J\"").is_err());
        assert!(serde_json::from_str::<PromptText>("\"\"").is_err());
        assert!(serde_json::from_str::<FeedbackText>("\"a\\nb\"").is_err());
        let ok: PromptText = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(ok.to_string(), "hello");
        assert_eq!(String::from(ok), "hello");
    }
}
