//! Small string helpers shared by the digest and output validation.

use protocol::FeedbackText;

/// Cuts `s` to at most `max` bytes on a char boundary, marking the cut.
pub(crate) fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} … [{} more bytes]", &s[..end], s.len() - end)
}

/// Cuts `s` to at most `max` chars, ending with `…` when cut. The cut
/// falls at the last word boundary that keeps at least half the room, so a
/// word is only split when it alone would leave the line mostly empty;
/// punctuation left dangling at the cut (`,` `;` `:` dashes) is dropped.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let room = max.saturating_sub(1);
    let kept: String = s.chars().take(room).collect();
    // A word ends at the cut when the next char is whitespace.
    let next_is_space = s.chars().nth(room).is_some_and(char::is_whitespace);
    let cut = if next_is_space {
        kept.as_str()
    } else {
        match kept.rfind(char::is_whitespace) {
            Some(at) if kept[..at].chars().count() >= room / 2 => &kept[..at],
            _ => kept.as_str(),
        }
    };
    let cut = cut.trim_end_matches(|c: char| c.is_whitespace() || ",;:-–—".contains(c));
    format!("{cut}…")
}

/// Joins all whitespace runs (including newlines) into single spaces.
pub(crate) fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalizes one line of model output for display: whitespace collapsed,
/// cut to `max_chars`, then checked with protocol's single-line text rules
/// (no control, bidi or invisible characters). `None` if it is blank or
/// fails the rules.
pub(crate) fn clean_line(s: &str, max_chars: usize) -> Option<String> {
    let line = truncate_chars(&collapse_whitespace(s), max_chars);
    FeedbackText::new(line).ok().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_bytes("short", 10), "short");
        assert_eq!(truncate_bytes("ééé", 3), "é … [4 more bytes]");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("ab", 4), "ab");
    }

    #[test]
    fn truncation_prefers_word_boundaries() {
        assert_eq!(truncate_chars("fixed the flaky test", 12), "fixed the…");
        assert_eq!(truncate_chars("fixed the flaky test", 11), "fixed the…");
        assert_eq!(truncate_chars("fixed the, flaky test", 12), "fixed the…");
        // A single long word is split rather than leaving the line mostly empty.
        assert_eq!(truncate_chars("a supercalifragilistic", 12), "a supercali…");
    }

    #[test]
    fn clean_line_normalizes_and_rejects() {
        assert_eq!(clean_line("  a\n\tb  ", 10).as_deref(), Some("a b"));
        assert_eq!(clean_line("   ", 10), None);
        assert_eq!(clean_line("a\u{202E}b", 10), None);
        assert_eq!(clean_line("a\u{1b}[2Jb", 10), None);
        assert_eq!(clean_line("abcdefghij", 5).as_deref(), Some("abcd…"));
    }
}
