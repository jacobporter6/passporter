//! herdr's view of an agent → the protocol's [`Agent`].
//!
//! Everything herdr reports about an agent (title, name, cwd) is shaped by
//! the agent or its repo and is therefore untrusted (design §8): text is
//! stripped of control and format characters and length-capped here, before
//! it reaches any client, notification or car display.

use protocol::{
    Agent, AgentId, AgentKind, AgentStatus, MAX_ASK_CHARS, PaneId, PendingInput, Seq, SessionId,
    Timestamp,
};

use super::repo::RepoLocation;
use crate::one_line::display_line;

/// The longest title, repo or worktree name sent to clients, in characters.
pub(crate) const MAX_LABEL_CHARS: usize = 120;

/// The herdr session kind whose value is an identifier we can carry.
const SESSION_KIND_ID: &str = "id";

/// Why a herdr agent cannot be shown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("herdr pane id {pane:?} is not a valid protocol pane id: {source}")]
pub(crate) struct Unrepresentable {
    pane: String,
    source: protocol::ValidationError,
}

/// The protocol id for a herdr agent: its pane, plus its session when herdr
/// reports one of kind `id` whose value fits the id charset (see
/// [`SessionId`]). Any other session yields a session-less id.
pub(crate) fn agent_id(agent: &herdr::Agent) -> Result<AgentId, Unrepresentable> {
    let pane = PaneId::new(agent.pane_id.as_str()).map_err(|source| Unrepresentable {
        pane: agent.pane_id.as_str().to_owned(),
        source,
    })?;
    let session = agent
        .agent_session
        .as_ref()
        .filter(|s| s.kind == SESSION_KIND_ID)
        .and_then(|s| SessionId::new(s.value.as_str()).ok());
    Ok(AgentId::new(pane, session))
}

/// Maps one herdr agent. `now` becomes `status_since`; the hub keeps the
/// previous value when the agent's state has not actually changed.
pub(crate) fn to_protocol(
    agent: &herdr::Agent,
    location: &RepoLocation,
    now: Timestamp,
) -> Result<Agent, Unrepresentable> {
    Ok(Agent {
        id: agent_id(agent)?,
        title: title(agent),
        repo: sanitize_label(&location.repo),
        worktree: location
            .worktree
            .as_deref()
            .map(sanitize_label)
            .filter(|w| !w.is_empty()),
        status: status(agent.agent_status),
        seq: Seq(agent.state_change_seq),
        status_since: now,
        // herdr's `done` is exactly "finished, not yet looked at".
        unread: agent.agent_status == herdr::AgentStatus::Done,
        kind: kind(agent.kind.as_deref()),
        headline: None,
        status_line: None,
        ask: None,
        quiet_done: false,
    })
}

/// The agent's name, else the title it reported, else the terminal title,
/// else its kind, else its pane id: the first one that is not blank.
fn title(agent: &herdr::Agent) -> String {
    let candidates = [
        agent.name.as_ref().map(herdr::AgentName::as_str),
        agent.title.as_deref(),
        agent.terminal_title_stripped.as_deref(),
        agent.kind.as_deref(),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(sanitize_label)
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| agent.pane_id.as_str().to_owned())
}

pub(crate) fn status(status: herdr::AgentStatus) -> AgentStatus {
    match status {
        herdr::AgentStatus::Idle => AgentStatus::Idle,
        herdr::AgentStatus::Working => AgentStatus::Working,
        herdr::AgentStatus::Blocked => AgentStatus::Blocked,
        herdr::AgentStatus::Done => AgentStatus::Done,
        herdr::AgentStatus::Unknown => AgentStatus::Unknown,
    }
}

/// The protocol kind of a herdr agent kind (`claude`, `codex`, …).
pub(crate) fn kind(kind: Option<&str>) -> AgentKind {
    match kind {
        Some("claude") => AgentKind::Claude,
        Some("codex") => AgentKind::Codex,
        Some("opencode") => AgentKind::Opencode,
        Some("pi") => AgentKind::Pi,
        _ => AgentKind::Unknown,
    }
}

/// A label: [`display_line`] capped at [`MAX_LABEL_CHARS`]. A label left
/// with nothing visible (e.g. only joiners) comes back empty, so it never
/// wins over the next title candidate.
pub(crate) fn sanitize_label(raw: &str) -> String {
    display_line(raw, MAX_LABEL_CHARS)
}

/// What a blocked agent with `pending` input is asking, for its card
/// ([`Agent::ask`]): the input's prompt as one [`display_line`] cut after
/// [`MAX_ASK_CHARS`] characters, or `None` when nothing visible is left
/// (an unparsed dialog has no prompt).
pub(crate) fn ask(pending: &PendingInput) -> Option<String> {
    Some(display_line(&pending.prompt, MAX_ASK_CHARS)).filter(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use protocol::PendingId;

    use super::*;

    #[test]
    fn the_ask_is_the_prompt_as_one_clean_capped_line() {
        let asking = |prompt: &str| {
            let mut input = PendingInput::unparsed_dialog(PendingId::new("screen-1").expect("id"));
            input.prompt = prompt.to_owned();
            ask(&input)
        };
        assert_eq!(
            asking("Run `cargo nextest run -p bridge`?").as_deref(),
            Some("Run `cargo nextest run -p bridge`?")
        );
        assert_eq!(
            asking("  Which\n\nbackend?\u{202E} \u{1b}[31m ").as_deref(),
            Some("Which backend? [31m")
        );
        assert_eq!(asking(""), None, "an unparsed dialog");
        assert_eq!(asking(" \u{200B}\n "), None, "nothing visible");
        let long = asking(&"x".repeat(MAX_ASK_CHARS * 2)).expect("an ask");
        assert_eq!(long.chars().count(), MAX_ASK_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    fn herdr_agent() -> herdr::Agent {
        serde_json::from_value(serde_json::json!({
            "terminal_id": "term_1", "pane_id": "w7:p1", "tab_id": "w7:t1",
            "workspace_id": "w7", "agent": "claude", "agent_status": "blocked",
            "state_change_seq": 151, "revision": 4, "focused": false,
            "title": "Fix the login flow", "cwd": "/home/me/git/gadget",
        }))
        .expect("valid herdr agent")
    }

    fn location() -> RepoLocation {
        RepoLocation {
            repo: "gadget".to_owned(),
            worktree: Some("login".to_owned()),
        }
    }

    fn session(kind: &str, value: &str) -> herdr::AgentSession {
        herdr::AgentSession {
            source: "herdr:claude".to_owned(),
            agent: "claude".to_owned(),
            kind: kind.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn maps_every_field() {
        let now = Timestamp::from_unix_millis(42);
        let agent = to_protocol(&herdr_agent(), &location(), now).expect("maps");
        assert_eq!(agent.id.to_string(), "w7:p1");
        assert_eq!(agent.title, "Fix the login flow");
        assert_eq!(agent.repo, "gadget");
        assert_eq!(agent.worktree.as_deref(), Some("login"));
        assert_eq!(agent.status, AgentStatus::Blocked);
        assert_eq!(agent.seq, Seq(151));
        assert_eq!(agent.status_since, now);
        assert!(!agent.unread);
        assert_eq!(agent.kind, AgentKind::Claude);
        assert_eq!(agent.headline, None);
    }

    #[test]
    fn session_ids_are_carried_only_for_kind_id() {
        let uuid = "9ca09978-ef89-46fd-9a85-118b84bee7b4";
        let mut agent = herdr_agent();
        agent.agent_session = Some(session("id", uuid));
        assert_eq!(
            agent_id(&agent).expect("id").to_string(),
            format!("w7:p1~{uuid}")
        );
        agent.agent_session = Some(session("path", "/tmp/session"));
        assert_eq!(agent_id(&agent).expect("id").to_string(), "w7:p1");
        agent.agent_session = Some(session("id", "has space"));
        assert_eq!(agent_id(&agent).expect("id").to_string(), "w7:p1");
    }

    #[test]
    fn unrepresentable_panes_are_errors() {
        let mut agent = herdr_agent();
        agent.pane_id = herdr::PaneId::new("w7/p1");
        assert!(agent_id(&agent).is_err());
    }

    #[test]
    fn title_prefers_the_name_then_falls_back() {
        let mut agent = herdr_agent();
        agent.name = Some(herdr::AgentName::new("circus").expect("name"));
        assert_eq!(title(&agent), "circus");
        agent.name = None;
        agent.title = Some("\u{3164}\u{200D}".to_owned());
        agent.terminal_title_stripped = Some("Passporter".to_owned());
        assert_eq!(title(&agent), "Passporter");
        agent.terminal_title_stripped = None;
        assert_eq!(title(&agent), "claude");
        agent.kind = None;
        assert_eq!(title(&agent), "w7:p1");
    }

    #[test]
    fn done_is_unread_and_kinds_map() {
        let mut agent = herdr_agent();
        agent.agent_status = herdr::AgentStatus::Done;
        agent.kind = Some("aider".to_owned());
        let mapped = to_protocol(&agent, &location(), Timestamp::default()).expect("maps");
        assert!(mapped.unread);
        assert_eq!(mapped.status, AgentStatus::Done);
        assert_eq!(mapped.kind, AgentKind::Unknown);
        assert_eq!(kind(Some("codex")), AgentKind::Codex);
        assert_eq!(kind(Some("opencode")), AgentKind::Opencode);
        assert_eq!(kind(Some("pi")), AgentKind::Pi);
    }

    #[test]
    fn empty_worktree_is_dropped() {
        let loc = RepoLocation {
            repo: "r".to_owned(),
            worktree: Some("\u{202E}".to_owned()),
        };
        let mapped = to_protocol(&herdr_agent(), &loc, Timestamp::default()).expect("maps");
        assert_eq!(mapped.worktree, None);
    }

    #[test]
    fn labels_are_sanitized() {
        assert_eq!(sanitize_label("  a\tb\n\nc  "), "a b c");
        assert_eq!(sanitize_label("\u{1b}[31mred\u{1b}[0m"), "[31mred[0m");
        assert_eq!(sanitize_label("evil\u{202E}txt.exe"), "eviltxt.exe");
        assert_eq!(sanitize_label("zero\u{200B}width"), "zerowidth");
        assert_eq!(sanitize_label("emoji 👍🏽 ok"), "emoji 👍🏽 ok");
        let family = "👩\u{200D}👧";
        assert_eq!(sanitize_label(family), family, "joiners are kept");
        for filler in [
            "\u{3164}",
            "\u{115F}\u{1160}",
            "\u{034F}",
            "\u{180B}",
            "\u{0600}",
        ] {
            assert_eq!(sanitize_label(filler), "", "{filler:?}");
        }
        assert_eq!(
            sanitize_label("\u{200D}\u{FE0F} "),
            "",
            "nothing visible left"
        );
    }

    #[test]
    fn invisible_modifier_runs_are_cleaned_like_protocol_text() {
        // protocol's own smuggling payload: one selector may follow a visible
        // character, the rest of the run goes.
        assert_eq!(
            sanitize_label("Yes\u{FE00}\u{FE01}\u{FE0F}\u{FE05}"),
            "Yes\u{FE00}"
        );
        assert_eq!(sanitize_label("a\u{200D}\u{200D}\u{200D}b"), "a\u{200D}b");
        assert_eq!(
            sanitize_label("ok \u{FE0F}x"),
            "ok x",
            "a selector after a space"
        );
        assert_eq!(sanitize_label("❤\u{FE0F}\u{200D}🔥"), "❤\u{FE0F}\u{200D}🔥");
        // Whatever comes out is text protocol itself would accept.
        for raw in [
            "Yes\u{FE00}\u{FE01}\u{FE0F}\u{FE05}",
            "a\u{200D}\u{200D}b",
            "\u{FE0F}lead",
            "x \u{FE0F}\u{FE0F} y",
            "evil\u{202E}txt \u{3164}name",
        ] {
            let clean = sanitize_label(raw);
            assert!(
                protocol::PromptText::new(clean.as_str()).is_ok(),
                "{raw:?} -> {clean:?}"
            );
        }
    }

    #[test]
    fn labels_are_capped_on_a_char_boundary() {
        let long = "é".repeat(MAX_LABEL_CHARS + 10);
        let capped = sanitize_label(&long);
        assert_eq!(capped.chars().count(), MAX_LABEL_CHARS + 1);
        assert!(capped.ends_with('…'));
        let exact = "x".repeat(MAX_LABEL_CHARS);
        assert_eq!(sanitize_label(&exact), exact);
    }
}
