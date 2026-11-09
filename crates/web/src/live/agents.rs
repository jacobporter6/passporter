//! Pure rules for the live agent list: applying stream events, the triage
//! segments and their order, and moving between agents.

use protocol::{Agent, AgentEvent, AgentId, AgentStatus};

/// A triage segment (design §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Segment {
    /// Waiting on the user. The only place the accent colour appears.
    Blocked,
    /// Running a turn.
    Working,
    /// Finished or idle: ready for the next prompt.
    Done,
}

impl Segment {
    /// Display order: blocked first.
    pub const ALL: [Self; 3] = [Self::Blocked, Self::Working, Self::Done];

    /// The segment an agent belongs in. herdr `idle` and `done` both count as
    /// Done (design §6.1); `unknown` is shown with Working, since it is
    /// neither waiting on the user nor known to be finished.
    #[must_use]
    pub const fn for_status(status: AgentStatus) -> Self {
        match status {
            AgentStatus::Blocked => Self::Blocked,
            AgentStatus::Idle | AgentStatus::Done => Self::Done,
            AgentStatus::Working | AgentStatus::Unknown => Self::Working,
        }
    }

    /// The tab label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Blocked => "Blocked",
            Self::Working => "Working",
            Self::Done => "Done",
        }
    }

    /// A stable lowercase name for ids and classes.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Working => "working",
            Self::Done => "done",
        }
    }
}

/// What applying a stream event did to the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListChange {
    /// The list changed.
    Changed,
    /// The event could not be applied (an agent this app could not parse);
    /// refetch `GET /v1/agents`.
    Refetch,
    /// Not an agent-list event.
    None,
}

/// Applies an agent-list event (`snapshot`, `agent_updated`,
/// `agent_removed`) to `agents`.
pub fn apply_event(agents: &mut Vec<Agent>, event: &AgentEvent) -> ListChange {
    match event {
        AgentEvent::Snapshot { agents: fresh } => {
            agents.clone_from(fresh);
            ListChange::Changed
        }
        AgentEvent::AgentUpdated { agent: Some(agent) } => {
            upsert(agents, agent.clone());
            ListChange::Changed
        }
        AgentEvent::AgentUpdated { agent: None } => ListChange::Refetch,
        AgentEvent::AgentRemoved { agent_id } => {
            agents.retain(|a| a.id != *agent_id);
            ListChange::Changed
        }
        _ => ListChange::None,
    }
}

/// Inserts `agent`, or replaces the entry with its id.
pub fn upsert(agents: &mut Vec<Agent>, agent: Agent) {
    match agents.iter_mut().find(|a| a.id == agent.id) {
        Some(held) => *held = agent,
        None => agents.push(agent),
    }
}

/// The agents in `segment`, in display order:
///
/// - Blocked: waiting longest first (inbox order);
/// - Working: running longest first;
/// - Done: unread first, then most recently finished.
///
/// Ties break by title, then id, so the order never jitters.
#[must_use]
pub fn in_segment(agents: &[Agent], segment: Segment) -> Vec<Agent> {
    let mut list: Vec<Agent> = agents
        .iter()
        .filter(|a| Segment::for_status(a.status) == segment)
        .cloned()
        .collect();
    // The id tiebreak makes this a total order, so the (smaller) unstable
    // sort gives the same result as a stable one.
    list.sort_unstable_by(|a, b| {
        let primary = match segment {
            Segment::Blocked | Segment::Working => a.status_since.cmp(&b.status_since),
            Segment::Done => b
                .unread
                .cmp(&a.unread)
                .then(b.status_since.cmp(&a.status_since)),
        };
        primary
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.id.cmp(&b.id))
    });
    list
}

/// How many agents each segment holds, in [`Segment::ALL`] order.
#[must_use]
pub fn counts(agents: &[Agent]) -> [usize; 3] {
    let mut counts = [0; 3];
    for agent in agents {
        let index = match Segment::for_status(agent.status) {
            Segment::Blocked => 0,
            Segment::Working => 1,
            Segment::Done => 2,
        };
        counts[index] += 1;
    }
    counts
}

/// The agents before and after `id` within its segment, for swiping.
#[must_use]
pub fn neighbours(agents: &[Agent], id: &AgentId) -> (Option<AgentId>, Option<AgentId>) {
    let Some(current) = agents.iter().find(|a| a.id == *id) else {
        return (None, None);
    };
    let list = in_segment(agents, Segment::for_status(current.status));
    let Some(index) = list.iter().position(|a| a.id == *id) else {
        return (None, None);
    };
    let previous = index
        .checked_sub(1)
        .and_then(|i| list.get(i))
        .map(|a| a.id.clone());
    let next = list.get(index + 1).map(|a| a.id.clone());
    (previous, next)
}

/// The blocked agent to handle after `current`: the next one in inbox
/// order, wrapping around, never `current` itself.
#[must_use]
pub fn next_blocked(agents: &[Agent], current: Option<&AgentId>) -> Option<AgentId> {
    let blocked = in_segment(agents, Segment::Blocked);
    let start = current
        .and_then(|id| blocked.iter().position(|a| a.id == *id))
        .map_or(0, |i| i + 1);
    blocked
        .iter()
        .cycle()
        .skip(start)
        .take(blocked.len())
        .find(|a| Some(&a.id) != current)
        .map(|a| a.id.clone())
}

#[cfg(test)]
mod tests {
    use protocol::{AgentKind, Seq, Timestamp};

    use super::*;

    fn agent(id: &str, status: AgentStatus, since: u64) -> Agent {
        Agent {
            id: id.parse().unwrap(),
            title: id.to_owned(),
            repo: "repo".into(),
            worktree: None,
            status,
            seq: Seq(1),
            status_since: Timestamp::from_unix_millis(since),
            unread: false,
            kind: AgentKind::Claude,
            headline: None,
            status_line: None,
            ask: None,
            quiet_done: false,
        }
    }

    fn ids(list: &[Agent]) -> Vec<String> {
        list.iter().map(|a| a.id.to_string()).collect()
    }

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    #[test]
    fn statuses_map_to_segments() {
        assert_eq!(Segment::for_status(AgentStatus::Blocked), Segment::Blocked);
        assert_eq!(Segment::for_status(AgentStatus::Working), Segment::Working);
        assert_eq!(Segment::for_status(AgentStatus::Idle), Segment::Done);
        assert_eq!(Segment::for_status(AgentStatus::Done), Segment::Done);
        assert_eq!(Segment::for_status(AgentStatus::Unknown), Segment::Working);
        assert_eq!(Segment::ALL[0], Segment::Blocked);
    }

    #[test]
    fn segments_sort_and_count() {
        let mut unread = agent("w1:p5", AgentStatus::Idle, 10);
        unread.unread = true;
        let agents = vec![
            agent("w1:p1", AgentStatus::Blocked, 300),
            agent("w1:p2", AgentStatus::Blocked, 100),
            agent("w1:p3", AgentStatus::Working, 50),
            agent("w1:p4", AgentStatus::Done, 900),
            unread,
            agent("w1:p6", AgentStatus::Idle, 500),
            agent("w1:p7", AgentStatus::Unknown, 40),
        ];
        assert_eq!(
            ids(&in_segment(&agents, Segment::Blocked)),
            ["w1:p2", "w1:p1"]
        );
        assert_eq!(
            ids(&in_segment(&agents, Segment::Working)),
            ["w1:p7", "w1:p3"]
        );
        assert_eq!(
            ids(&in_segment(&agents, Segment::Done)),
            ["w1:p5", "w1:p4", "w1:p6"]
        );
        assert_eq!(counts(&agents), [2, 2, 3]);
    }

    #[test]
    fn ties_break_by_title_then_id() {
        let agents = vec![
            agent("w1:p2", AgentStatus::Blocked, 1),
            agent("w1:p1", AgentStatus::Blocked, 1),
        ];
        assert_eq!(
            ids(&in_segment(&agents, Segment::Blocked)),
            ["w1:p1", "w1:p2"]
        );
    }

    #[test]
    fn events_update_the_list() {
        let mut agents = vec![agent("w1:p1", AgentStatus::Working, 1)];
        let moved = agent("w1:p1", AgentStatus::Blocked, 2);
        assert_eq!(
            apply_event(
                &mut agents,
                &AgentEvent::AgentUpdated {
                    agent: Some(moved.clone())
                }
            ),
            ListChange::Changed
        );
        assert_eq!(agents, [moved]);
        let added = agent("w1:p2", AgentStatus::Idle, 3);
        apply_event(
            &mut agents,
            &AgentEvent::AgentUpdated {
                agent: Some(added.clone()),
            },
        );
        assert_eq!(agents.len(), 2);
        apply_event(
            &mut agents,
            &AgentEvent::AgentRemoved {
                agent_id: id("w1:p1"),
            },
        );
        assert_eq!(agents, [added]);
        assert_eq!(
            apply_event(&mut agents, &AgentEvent::AgentUpdated { agent: None }),
            ListChange::Refetch
        );
        assert_eq!(
            apply_event(&mut agents, &AgentEvent::Snapshot { agents: vec![] }),
            ListChange::Changed
        );
        assert_eq!(agents.len(), 0);
        assert_eq!(
            apply_event(&mut agents, &AgentEvent::Unknown),
            ListChange::None
        );
    }

    #[test]
    fn neighbours_stay_in_segment() {
        let agents = vec![
            agent("w1:p1", AgentStatus::Blocked, 1),
            agent("w1:p2", AgentStatus::Working, 2),
            agent("w1:p3", AgentStatus::Blocked, 3),
            agent("w1:p4", AgentStatus::Blocked, 4),
        ];
        assert_eq!(
            neighbours(&agents, &id("w1:p3")),
            (Some(id("w1:p1")), Some(id("w1:p4")))
        );
        assert_eq!(neighbours(&agents, &id("w1:p1")), (None, Some(id("w1:p3"))));
        assert_eq!(neighbours(&agents, &id("w1:p2")), (None, None));
        assert_eq!(neighbours(&agents, &id("w9:p9")), (None, None));
    }

    #[test]
    fn next_blocked_wraps_and_skips_current() {
        let agents = vec![
            agent("w1:p1", AgentStatus::Blocked, 1),
            agent("w1:p2", AgentStatus::Blocked, 2),
            agent("w1:p3", AgentStatus::Idle, 3),
        ];
        assert_eq!(next_blocked(&agents, None), Some(id("w1:p1")));
        assert_eq!(next_blocked(&agents, Some(&id("w1:p1"))), Some(id("w1:p2")));
        assert_eq!(next_blocked(&agents, Some(&id("w1:p2"))), Some(id("w1:p1")));
        assert_eq!(next_blocked(&agents, Some(&id("w1:p3"))), Some(id("w1:p1")));
        let one = vec![agent("w1:p1", AgentStatus::Blocked, 1)];
        assert_eq!(next_blocked(&one, Some(&id("w1:p1"))), None);
    }
}
