//! Untrusted text made fit for one line on a small surface: a notification,
//! or a sentence the car reads aloud.

/// One display line of untrusted text: control characters and the
/// characters [`protocol::text::is_hidden_char`] flags (bidi controls,
/// zero-width and filler characters, tags) dropped, invisible modifiers
/// dropped where [`protocol::text::is_misplaced_modifier`] says no visible
/// text needs them (selector runs, repeated joiners), whitespace runs
/// (line breaks too) collapsed to one space, trimmed, and cut after `max`
/// characters with an ellipsis. Text left with nothing visible comes back
/// empty.
pub(crate) fn display_line(raw: &str, max: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max * 4));
    let mut chars = 0;
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() || protocol::text::is_hidden_char(c) {
            continue;
        }
        let prev = if pending_space {
            Some(' ')
        } else {
            out.chars().next_back()
        };
        if protocol::text::is_misplaced_modifier(prev, c) {
            continue;
        }
        let needed = usize::from(pending_space) + 1;
        if chars + needed > max {
            out.push('…');
            return out;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
        chars += needed;
    }
    if out.chars().all(protocol::text::is_blank_char) {
        out.clear();
    }
    out
}

/// [`display_line`] of at most `max` characters, the ellipsis included,
/// or `None` if nothing visible is left.
pub(crate) fn one_line(text: &str, max: usize) -> Option<String> {
    Some(display_line(text, max.saturating_sub(1))).filter(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_one_clean_bounded_line() {
        assert_eq!(
            one_line("  Run\n`ls`\u{202e}  now\u{1b}[2J ", 60).as_deref(),
            Some("Run `ls` now[2J")
        );
        assert_eq!(one_line("\u{200b} \n", 10), None);
        let long = one_line(&"word ".repeat(100), 20).expect("text");
        assert_eq!(long.chars().count(), 20);
        assert!(long.ends_with('…'));
        assert_eq!(one_line("short", 20).as_deref(), Some("short"));
        // Only blank-looking characters: nothing to show.
        assert_eq!(one_line("\u{3164} \u{fe0f}", 20), None);
    }
}
