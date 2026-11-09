//! What the push notification says: which agents are blocked or done, by
//! name only. Pure, so the wording is tested on the host; the service worker
//! shows whatever [`Notice`] it is handed.

use protocol::Agent;
use protocol::text::is_hidden_char;
use serde::Serialize;

use crate::live::agents::{Segment, in_segment};
use crate::routes::AppRoute;

/// The tag every Passporter notification shares, so a new ping replaces
/// the previous notification instead of stacking another.
pub const TAG: &str = "passporter-agents";

/// Names listed before the rest are counted ("a, b, c and 2 more").
const MAX_NAMES: usize = 3;
/// Longest agent name shown, in characters, before it is cut with "…".
const MAX_NAME_CHARS: usize = 32;

/// One notification, as the service worker shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    /// The notification title.
    pub title: String,
    /// The notification body.
    pub body: String,
    /// The coalescing tag ([`TAG`]).
    pub tag: &'static str,
    /// The in-app path a tap opens.
    pub path: String,
    /// Nothing needs the user: the push's reason went away before it
    /// arrived. `public/sw.js` then leaves a showing alert as it is, or
    /// shows this one silently and closes it.
    pub quiet: bool,
}

impl Notice {
    /// What to show when the agents couldn't be fetched (offline, not
    /// paired, the bridge refused): nothing specific, just a nudge.
    /// `public/sw.js` has the same text for when the WASM itself can't load.
    #[must_use]
    pub fn generic() -> Self {
        Self::new(
            "Passporter".to_owned(),
            "Your agents have news. Open Passporter to see.".to_owned(),
            &AppRoute::Triage,
        )
    }

    /// The notification for the current agent list: blocked agents
    /// (waiting longest first) and agents done with a turn the user hasn't
    /// seen (most recent first). The orchestrator's agents
    /// ([`Agent::quiet_done`]) count only when blocked: another agent is
    /// waiting on their finished turns, not the user. A tap opens the agent
    /// when there is just one, and the triage list otherwise. With nothing
    /// to report (the ping's reason went away before it arrived) the notice
    /// is [`Self::quiet`].
    #[must_use]
    pub fn summarise(agents: &[Agent]) -> Self {
        let blocked = in_segment(agents, Segment::Blocked);
        let done: Vec<Agent> = in_segment(agents, Segment::Done)
            .into_iter()
            .filter(|a| a.unread && !a.quiet_done)
            .collect();

        let path = match (blocked.as_slice(), done.as_slice()) {
            ([only], []) | ([], [only]) => AppRoute::Agent(only.id.clone()),
            _ => AppRoute::Triage,
        };
        let (title, body) = match (blocked.as_slice(), done.as_slice()) {
            ([], []) => {
                return Self {
                    quiet: true,
                    ..Self::new("Passporter is up to date".to_owned(), String::new(), &path)
                };
            }
            ([only], []) => (
                format!("{} is blocked", name(only)),
                "Waiting for you.".to_owned(),
            ),
            ([], [only]) => (
                format!("{} is done", name(only)),
                "Finished its turn.".to_owned(),
            ),
            (blocked, []) => (format!("{} agents blocked", blocked.len()), names(blocked)),
            ([], done) => (format!("{} agents done", done.len()), names(done)),
            (blocked, done) => (
                format!("{} blocked, {} done", blocked.len(), done.len()),
                format!("Blocked: {}. Done: {}.", names(blocked), names(done)),
            ),
        };
        Self::new(title, body, &path)
    }

    fn new(title: String, body: String, route: &AppRoute) -> Self {
        Self {
            title,
            body,
            tag: TAG,
            path: route.href(),
            quiet: false,
        }
    }
}

/// An agent's name for a notification: hidden characters (bidi controls,
/// zero-width and other format characters; [`is_hidden_char`]) dropped,
/// whitespace and control characters collapsed to single spaces, cut to
/// [`MAX_NAME_CHARS`]. The title is untrusted (design §8), and the OS
/// shows it outside the app.
fn name(agent: &Agent) -> String {
    let visible: String = agent
        .title
        .chars()
        .filter(|&c| c.is_whitespace() || !is_hidden_char(c))
        .collect();
    let words: Vec<&str> = visible
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let tidy = words.join(" ");
    if tidy.is_empty() {
        return agent.id.to_string();
    }
    if tidy.chars().count() <= MAX_NAME_CHARS {
        return tidy;
    }
    let cut: String = tidy.chars().take(MAX_NAME_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// "a", "a and b", "a, b and c", "a, b, c and 2 more".
fn names(agents: &[Agent]) -> String {
    let shown: Vec<String> = agents.iter().take(MAX_NAMES).map(name).collect();
    let rest = agents.len() - shown.len();
    if rest > 0 {
        return format!("{} and {rest} more", shown.join(", "));
    }
    match shown.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, init)) => format!("{} and {last}", init.join(", ")),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use protocol::{AgentKind, AgentStatus, Seq, Timestamp};

    use super::*;

    /// Like the bridge, a `done` agent starts unread.
    fn agent(id: &str, title: &str, status: AgentStatus, since: u64) -> Agent {
        Agent {
            id: id.parse().unwrap(),
            title: title.to_owned(),
            repo: "repo".into(),
            worktree: None,
            status,
            seq: Seq(1),
            status_since: Timestamp::from_unix_millis(since),
            unread: status == AgentStatus::Done,
            kind: AgentKind::Claude,
            headline: Some("secret question text".into()),
            status_line: None,
            ask: None,
            quiet_done: false,
        }
    }

    #[test]
    fn one_blocked_agent_opens_that_agent() {
        let agents = [
            agent("w1:p1", "api", AgentStatus::Blocked, 5),
            agent("w1:p2", "web", AgentStatus::Working, 1),
        ];
        let notice = Notice::summarise(&agents);
        assert_eq!(notice.title, "api is blocked");
        assert_eq!(notice.body, "Waiting for you.");
        assert_eq!(notice.path, "/agent/w1:p1");
        assert_eq!(notice.tag, TAG);
    }

    #[test]
    fn one_done_agent_opens_that_agent() {
        let notice = Notice::summarise(&[agent("w1:p3", "docs", AgentStatus::Done, 5)]);
        assert_eq!(notice.title, "docs is done");
        assert_eq!(notice.body, "Finished its turn.");
        assert_eq!(notice.path, "/agent/w1:p3");
    }

    #[test]
    fn several_blocked_list_longest_waiting_first() {
        let agents = [
            agent("w1:p1", "new", AgentStatus::Blocked, 30),
            agent("w1:p2", "old", AgentStatus::Blocked, 10),
        ];
        let notice = Notice::summarise(&agents);
        assert_eq!(notice.title, "2 agents blocked");
        assert_eq!(notice.body, "old and new");
        assert_eq!(notice.path, "/");
    }

    #[test]
    fn blocked_and_done_together() {
        let agents = [
            agent("w1:p1", "api", AgentStatus::Blocked, 1),
            agent("w1:p2", "web", AgentStatus::Done, 2),
            agent("w1:p3", "cli", AgentStatus::Done, 3),
            agent("w1:p4", "lib", AgentStatus::Working, 1),
        ];
        let notice = Notice::summarise(&agents);
        assert_eq!(notice.title, "1 blocked, 2 done");
        assert_eq!(notice.body, "Blocked: api. Done: cli and web.");
        assert_eq!(notice.path, "/");
    }

    #[test]
    fn long_lists_are_counted() {
        let agents: Vec<Agent> = (1..=5)
            .map(|i| {
                agent(
                    &format!("w1:p{i}"),
                    &format!("a{i}"),
                    AgentStatus::Done,
                    100 - i,
                )
            })
            .collect();
        let notice = Notice::summarise(&agents);
        assert_eq!(notice.title, "5 agents done");
        assert_eq!(notice.body, "a1, a2, a3 and 2 more");
    }

    #[test]
    fn only_unread_finished_agents_count_as_done() {
        let mut seen = agent("w1:p1", "seen", AgentStatus::Done, 1);
        seen.unread = false;
        let idle = agent("w1:p2", "idle", AgentStatus::Idle, 2);
        let mut fresh = agent("w1:p3", "fresh", AgentStatus::Idle, 3);
        fresh.unread = true;
        let notice = Notice::summarise(&[seen.clone(), idle.clone(), fresh]);
        assert_eq!(notice.title, "fresh is done");

        // Seen agents don't crowd out the one that needs you, and the tap
        // still goes straight to it.
        let blocked = agent("w1:p4", "api", AgentStatus::Blocked, 4);
        let notice = Notice::summarise(&[seen, idle, blocked]);
        assert_eq!(notice.title, "api is blocked");
        assert_eq!(notice.path, "/agent/w1:p4");
    }

    #[test]
    fn the_orchestrators_agents_count_when_blocked_but_not_when_done() {
        let mut implementer = agent("w8:p1", "impl-t26f", AgentStatus::Done, 5);
        implementer.quiet_done = true;
        let mine = agent("w1:p1", "docs", AgentStatus::Done, 1);
        let notice = Notice::summarise(&[implementer.clone(), mine]);
        assert_eq!(notice.title, "docs is done");
        assert_eq!(notice.path, "/agent/w1:p1");

        let mut reviewer = agent("w8:p2", "rev-t26f-r1", AgentStatus::Blocked, 3);
        reviewer.quiet_done = true;
        let notice = Notice::summarise(&[implementer.clone(), reviewer]);
        assert_eq!(notice.title, "rev-t26f-r1 is blocked");
        assert_eq!(notice.body, "Waiting for you.");

        // Only quiet agents finished: nothing to alert for.
        let notice = Notice::summarise(&[implementer]);
        assert!(notice.quiet, "{notice:?}");
    }

    #[test]
    fn nothing_to_report_is_a_quiet_notice() {
        let mut seen = agent("w1:p2", "seen", AgentStatus::Done, 2);
        seen.unread = false;
        let agents = [
            agent("w1:p1", "busy", AgentStatus::Working, 1),
            seen,
            agent("w1:p3", "idle", AgentStatus::Idle, 3),
        ];
        for agents in [&agents[..], &[]] {
            let notice = Notice::summarise(agents);
            assert!(notice.quiet, "{notice:?}");
            assert_eq!(notice.title, "Passporter is up to date");
            assert_eq!(notice.body, "");
            assert_eq!(notice.path, "/");
            assert_eq!(notice.tag, TAG);
        }
        let json = serde_json::to_value(Notice::summarise(&[])).unwrap();
        assert_eq!(json["quiet"], true);
    }

    #[test]
    fn news_and_the_generic_nudge_are_not_quiet() {
        assert!(!Notice::generic().quiet);
        let blocked = agent("w1:p1", "api", AgentStatus::Blocked, 1);
        assert!(!Notice::summarise(&[blocked]).quiet);
        let done = agent("w1:p2", "docs", AgentStatus::Done, 1);
        assert!(!Notice::summarise(&[done]).quiet);
    }

    #[test]
    fn names_only_never_other_agent_content() {
        let agents = [
            agent("w1:p1", "api", AgentStatus::Blocked, 1),
            agent("w1:p2", "web", AgentStatus::Done, 2),
        ];
        let json = serde_json::to_string(&Notice::summarise(&agents)).unwrap();
        assert!(!json.contains("secret"), "{json}");
        assert!(!json.contains("repo"), "{json}");
    }

    #[test]
    fn names_are_tidied_and_cut() {
        let messy = agent("w1:p1", "  fix\nthe\t\u{7}bug  ", AgentStatus::Blocked, 1);
        assert_eq!(name(&messy), "fix the bug");
        let long = agent("w1:p1", &"x".repeat(80), AgentStatus::Blocked, 1);
        let cut = name(&long);
        assert_eq!(cut.chars().count(), MAX_NAME_CHARS);
        assert!(cut.ends_with('…'));
        let blank = agent("w1:p9", " \n ", AgentStatus::Blocked, 1);
        assert_eq!(name(&blank), "w1:p9");
    }

    #[test]
    fn names_drop_bidi_and_invisible_characters() {
        let disguised = agent(
            "w1:p1",
            "\u{202E}gpj.exe\u{202C} \u{2066}ok\u{2069}\u{200B}\u{FEFF}\u{E0041}",
            AgentStatus::Blocked,
            1,
        );
        assert_eq!(name(&disguised), "gpj.exe ok");
        // Joiners stay: emoji sequences need them.
        let family = agent("w1:p1", "a\u{200D}b", AgentStatus::Blocked, 1);
        assert_eq!(name(&family), "a\u{200D}b");
        let only_hidden = agent("w1:p9", "\u{200B}\u{2067}", AgentStatus::Blocked, 1);
        assert_eq!(name(&only_hidden), "w1:p9");
    }

    #[test]
    fn serializes_for_the_service_worker() {
        let json = serde_json::to_value(Notice::generic()).unwrap();
        assert_eq!(json["tag"], TAG);
        assert_eq!(json["path"], "/");
        assert!(json["title"].is_string() && json["body"].is_string());
        assert_eq!(json["quiet"], false);
    }
}
