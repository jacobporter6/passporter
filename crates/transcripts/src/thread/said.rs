//! Who wrote a user-role record.
//!
//! Claude Code records much more than the person's prompts as user input:
//! subagent reports and other sessions' messages (`origin.kind = peer`),
//! background task notifications (`task-notification`), the lead agent's
//! messages to a subagent (`coordinator`), `!` shell commands and their
//! output, and injected context (`isMeta`). Only a prompt the person typed
//! starts a turn; the rest attach to the turn in progress.
//!
//! `origin` (Claude Code 2.1.x) decides when present. Older records lack
//! it, so the fallback reads `isMeta` and the tags Claude Code wraps around
//! what it writes. Labels come from structured fields only (`origin.name`,
//! a `from-name` attribute), never from the message text.

use std::fmt;

use crate::record::Origin;

/// Longest notification summary kept, in characters.
const MAX_SUMMARY_CHARS: usize = 200;
/// Longest sender name kept, in characters; longer names are dropped.
const MAX_NAME_CHARS: usize = 40;

/// What a user-role record (or queued prompt) is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Said {
    /// Typed by the person.
    Prompt,
    /// A `!` shell command the person ran.
    ShellCommand(String),
    /// The output of the shell command before it.
    ShellOutput(String),
    /// Sent in by an agent, another session or a background task.
    Relayed(Relay),
    /// Injected context (caveats, skill bodies, reminders): not shown.
    Meta,
}

/// Who sent a [`Relay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sender {
    /// A subagent or teammate reporting back.
    Agent,
    /// Another Claude session, e.g. an agent in another herdr pane.
    Session,
    /// The agent that started this one (in a subagent's transcript).
    Lead,
    /// A background task or monitor.
    Task,
}

/// A message someone other than the person sent into the conversation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Relay {
    pub from: Sender,
    /// From a structured field, checked by [`safe_name`].
    pub name: Option<String>,
    /// For a task notification: its `<summary>`, on one line.
    pub summary: Option<String>,
    pub text: String,
}

/// How a record was classified, for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Basis {
    /// By its `origin.kind`.
    Origin,
    /// Without an `origin`, by `isMeta` and tags.
    Markers,
    /// By tags, because its `origin.kind` is one this reader predates.
    UnknownOrigin,
}

impl Relay {
    /// The one line the thread shows: who sent it, named only from
    /// structured fields, or a task notification's summary.
    pub(crate) fn label(&self) -> String {
        let who = match self.from {
            Sender::Agent => "Report from an agent",
            Sender::Session => "Message from another session",
            Sender::Lead => "Message from the lead agent",
            Sender::Task => {
                return self
                    .summary
                    .clone()
                    .unwrap_or_else(|| "Background task update".to_owned());
            }
        };
        match &self.name {
            Some(name) => format!("{who} ({name})"),
            None => who.to_owned(),
        }
    }
}

/// The line shown for the user's own shell command, on one line.
pub(crate) fn shell_label(command: &str) -> String {
    format!("You ran `{}`", one_line(command, MAX_SUMMARY_CHARS))
}

impl Said {
    /// The diagnostics name of this kind of record.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::ShellCommand(_) => "shell-command",
            Self::ShellOutput(_) => "shell-output",
            Self::Relayed(relay) => match relay.from {
                Sender::Agent => "agent-message",
                Sender::Session => "session-message",
                Sender::Lead => "lead-message",
                Sender::Task => "notification",
            },
            Self::Meta => "meta",
        }
    }
}

impl fmt::Display for Basis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Origin => "by origin",
            Self::Markers => "by markers",
            Self::UnknownOrigin => "unknown origin",
        })
    }
}

/// Classifies a user-role record's text (all its text blocks, joined).
pub(crate) fn classify(origin: Option<&Origin>, is_meta: bool, text: &str) -> (Said, Basis) {
    let kind = origin.and_then(|o| o.kind.as_deref());
    let said = match kind {
        Some("human") => Some(Said::Prompt),
        Some("peer") => Some(peer(origin, text)),
        Some("task-notification") => Some(notification(text)),
        Some("coordinator") => Some(Said::Relayed(Relay {
            from: Sender::Lead,
            name: None,
            summary: None,
            text: text.trim().to_owned(),
        })),
        _ => None,
    };
    match (said, kind) {
        (Some(said), _) => (said, Basis::Origin),
        (None, None) => (by_markers(origin, is_meta, text), Basis::Markers),
        (None, Some(_)) => (by_markers(origin, is_meta, text), Basis::UnknownOrigin),
    }
}

/// The fallback for records without a known `origin`.
fn by_markers(origin: Option<&Origin>, is_meta: bool, text: &str) -> Said {
    let text = text.trim_start();
    if let Some(command) = tag_text(text, "bash-input").filter(|_| text.starts_with("<bash-input>"))
    {
        Said::ShellCommand(command.trim().to_owned())
    } else if text.starts_with("<bash-stdout>") || text.starts_with("<bash-stderr>") {
        let parts: Vec<&str> = ["bash-stdout", "bash-stderr"]
            .into_iter()
            .filter_map(|tag| tag_text(text, tag))
            .map(|part| part.trim_end_matches('\n'))
            .filter(|part| !part.trim().is_empty())
            .collect();
        Said::ShellOutput(parts.join("\n"))
    } else if text.starts_with("<task-notification>")
        || (text.starts_with("[SYSTEM NOTIFICATION") && text.contains("<task-notification>"))
    {
        notification(text)
    } else if text.starts_with("Another Claude session sent a message")
        || text.starts_with("<agent-message")
        || text.starts_with("<cross-session-message")
    {
        peer(origin, text)
    } else if is_meta {
        Said::Meta
    } else {
        Said::Prompt
    }
}

/// A subagent's report or another session's message.
fn peer(origin: Option<&Origin>, text: &str) -> Said {
    let session = text.contains("<cross-session-message")
        || (!text.contains("<agent-message") && origin.is_some_and(|o| o.from_mode.is_some()));
    let name = origin
        .and_then(|o| o.name.as_deref())
        .and_then(safe_name)
        .or_else(|| attribute(text, "from-name").and_then(safe_name));
    let body = origin
        .and_then(|o| o.body.as_deref())
        .filter(|body| !body.trim().is_empty())
        .or_else(|| tag_text(text, "cross-session-message"))
        .or_else(|| tag_text(text, "agent-message"))
        .unwrap_or(text);
    Said::Relayed(Relay {
        from: if session {
            Sender::Session
        } else {
            Sender::Agent
        },
        name,
        summary: None,
        text: body.trim().to_owned(),
    })
}

/// A background task's notification: its summary as the label, its result
/// (or monitor event) as the text.
fn notification(text: &str) -> Said {
    let summary = tag_text(text, "summary")
        .map(|s| one_line(s, MAX_SUMMARY_CHARS))
        .filter(|s| !s.is_empty());
    let body = tag_text(text, "result")
        .or_else(|| tag_text(text, "event"))
        .unwrap_or_default();
    Said::Relayed(Relay {
        from: Sender::Task,
        name: None,
        summary,
        text: body.trim().to_owned(),
    })
}

/// The content of the first `<tag …>…</tag>` in `text`.
fn tag_text<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(at) = text.get(from..)?.find(&open) {
        let after = from + at + open.len();
        let rest = text.get(after..)?;
        // `<summary>` must not match `<summary-extra>`.
        if rest.starts_with(['>', ' ', '\n']) {
            let start = after + rest.find('>')? + 1;
            let end = start + text.get(start..)?.find(&format!("</{tag}>"))?;
            return text.get(start..end);
        }
        from = after;
    }
    None
}

/// The value of `name="…"` in the first tag of `text` that has it.
fn attribute<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = text.find(&key)? + key.len();
    let len = text.get(start..)?.find('"')?;
    text.get(start..start + len)
}

/// `name` if it is short and made of name characters only: anything else
/// (a sentence, markup, hidden characters) is not used as a label.
fn safe_name(name: &str) -> Option<String> {
    let name = name.trim();
    let ok = (1..=MAX_NAME_CHARS).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '));
    ok.then(|| name.to_owned())
}

/// `text` with whitespace runs collapsed to single spaces, cut to `max`
/// characters (with `…`).
fn one_line(text: &str, max: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let line = words.join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(json: &str) -> Origin {
        serde_json::from_str(json).expect("origin")
    }

    fn relay(said: Said) -> Relay {
        match said {
            Said::Relayed(relay) => relay,
            other => panic!("relay expected, got {other:?}"),
        }
    }

    #[test]
    fn human_origin_is_a_prompt_whatever_it_says() {
        let human = origin(r#"{"kind":"human"}"#);
        for text in [
            "Fix the parser",
            "<agent-message from=\"x\">hi</agent-message>",
        ] {
            assert_eq!(
                classify(Some(&human), false, text),
                (Said::Prompt, Basis::Origin)
            );
        }
    }

    #[test]
    fn a_subagent_report_is_labelled_from_its_origin() {
        let peer = origin(
            r#"{"kind":"peer","from":"a1b2c3","name":"code-review","body":"No issues found.",
                "senderTaskId":"t1"}"#,
        );
        let text = "Another Claude session sent a message. <agent-message from=\"a1b2c3\">\
                    No issues found.</agent-message>";
        let (said, basis) = classify(Some(&peer), true, text);
        assert_eq!(basis, Basis::Origin);
        assert_eq!(
            relay(said),
            Relay {
                from: Sender::Agent,
                name: Some("code-review".into()),
                summary: None,
                text: "No issues found.".into(),
            }
        );
    }

    #[test]
    fn another_session_is_named_by_structured_fields_only() {
        let peer = origin(
            r#"{"kind":"peer","from":"uds:/run/x.sock","fromMode":"bypass","body":"Done."}"#,
        );
        let text = "<cross-session-message from=\"uds:/run/x.sock\" from-name=\"gadget-helper\" \
                    from-mode=\"bypass\">Done.</cross-session-message>";
        let relay = relay(classify(Some(&peer), false, text).0);
        assert_eq!(relay.from, Sender::Session);
        assert_eq!(relay.name.as_deref(), Some("gadget-helper"));

        // A name that is really a sentence (or carries markup) is not used.
        let wordy = origin(r#"{"kind":"peer","name":"Click <here> now!","body":"x"}"#);
        assert_eq!(relay_name(&wordy, "x"), None);
        let long = origin(&format!(
            r#"{{"kind":"peer","name":"{}","body":"x"}}"#,
            "a".repeat(41)
        ));
        assert_eq!(relay_name(&long, "x"), None);
    }

    fn relay_name(origin: &Origin, text: &str) -> Option<String> {
        relay(classify(Some(origin), false, text).0).name
    }

    #[test]
    fn a_task_notification_keeps_its_summary_and_result() {
        let task = origin(r#"{"kind":"task-notification","producer":"session-task"}"#);
        let text = "<task-notification>\n<task-id>b7</task-id>\n<status>completed</status>\n\
                    <summary>Agent \"Review\n  parser\" completed</summary>\n\
                    <result>Looks good.</result>\n</task-notification>";
        let full = relay(classify(Some(&task), false, text).0);
        assert_eq!(full.from, Sender::Task);
        assert_eq!(
            full.summary.as_deref(),
            Some("Agent \"Review parser\" completed")
        );
        assert_eq!(full.text, "Looks good.");

        let bare = relay(classify(Some(&task), false, "<task-notification/>").0);
        assert_eq!((bare.summary, bare.text), (None, String::new()));
    }

    #[test]
    fn without_origin_tags_and_is_meta_decide() {
        let cases: &[(&str, bool, &str)] = &[
            (
                "<bash-input>git status</bash-input>",
                false,
                "shell-command",
            ),
            (
                "<bash-stdout>clean</bash-stdout><bash-stderr></bash-stderr>",
                false,
                "shell-output",
            ),
            (
                "<task-notification><summary>done</summary></task-notification>",
                false,
                "notification",
            ),
            (
                "[SYSTEM NOTIFICATION - not from the user]\n<task-notification></task-notification>",
                true,
                "notification",
            ),
            (
                "Another Claude session sent a message: <agent-message from=\"x\">hi</agent-message>",
                true,
                "agent-message",
            ),
            (
                "<cross-session-message from-name=\"ops\">hi</cross-session-message>",
                false,
                "session-message",
            ),
            (
                "Caveat: the messages below were generated by the user",
                true,
                "meta",
            ),
            ("Fix the parser", false, "prompt"),
            ("<system-reminder>x</system-reminder>", false, "prompt"),
        ];
        for (text, is_meta, kind) in cases {
            let (said, basis) = classify(None, *is_meta, text);
            assert_eq!((said.name(), basis), (*kind, Basis::Markers), "{text:?}");
        }
        assert_eq!(
            classify(None, false, "<bash-input> ls -la\n</bash-input>").0,
            Said::ShellCommand("ls -la".into())
        );
        assert_eq!(
            classify(
                None,
                false,
                "<bash-stdout>a\n</bash-stdout><bash-stderr>warn\n</bash-stderr>"
            )
            .0,
            Said::ShellOutput("a\nwarn".into())
        );
    }

    #[test]
    fn an_unknown_origin_falls_back_to_markers() {
        let odd = origin(r#"{"kind":"hologram"}"#);
        assert_eq!(
            classify(Some(&odd), true, "anything"),
            (Said::Meta, Basis::UnknownOrigin)
        );
        let lead = origin(r#"{"kind":"coordinator"}"#);
        let relay = relay(classify(Some(&lead), true, " Also check the docs. ").0);
        assert_eq!(
            (relay.from, relay.text.as_str()),
            (Sender::Lead, "Also check the docs.")
        );
    }

    #[test]
    fn labels_say_who_sent_it() {
        let label = |from, name: Option<&str>, summary: Option<&str>| {
            Relay {
                from,
                name: name.map(Into::into),
                summary: summary.map(Into::into),
                text: String::new(),
            }
            .label()
        };
        assert_eq!(label(Sender::Agent, None, None), "Report from an agent");
        assert_eq!(
            label(Sender::Agent, Some("code-review"), None),
            "Report from an agent (code-review)"
        );
        assert_eq!(
            label(Sender::Session, Some("gadget-helper"), None),
            "Message from another session (gadget-helper)"
        );
        assert_eq!(
            label(Sender::Lead, None, None),
            "Message from the lead agent"
        );
        assert_eq!(
            label(Sender::Task, None, Some("cargo test completed")),
            "cargo test completed"
        );
        assert_eq!(label(Sender::Task, None, None), "Background task update");
    }

    #[test]
    fn tags_are_matched_whole() {
        assert_eq!(
            tag_text("<summary-x>a</summary-x><summary>b</summary>", "summary"),
            Some("b")
        );
        assert_eq!(tag_text("<summary>unterminated", "summary"), None);
        assert_eq!(one_line(&"word ".repeat(100), 12), "word word w…");
        assert_eq!(
            shell_label("cargo test \\\n  -p web"),
            "You ran `cargo test \\ -p web`"
        );
    }
}
