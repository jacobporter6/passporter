//! Which registry changes may deserve a ping: an agent that becomes blocked
//! (a new dialog, also at a new `seq` while still blocked) or finishes a
//! turn (`working` → `idle`/`done`). Whether a finished turn actually pings
//! is the dispatcher's call (swarm agents' do not, see
//! [`super::PushSettings::quiet_done`]). Everything else (titles,
//! unread flags, an agent going back to work, thread items) is silent.
//!
//! A change is only a reason while it lasts: by the time its debounced ping
//! is due, [`Change::lapsed`] says whether the agent has moved on (answered,
//! working again, its turn read), so the ping is not sent for nothing.

use std::collections::HashMap;

use protocol::{Agent, AgentEvent, AgentId, AgentStatus, Seq};

/// The last status seen per agent, fed from the hub's event stream.
///
/// Only a change from a known state counts. An agent seen for the first
/// time (in the first snapshot, in herdr's first sync after a bridge
/// restart, or in a later resync) only sets its baseline, so a restart
/// never pings for agents that were already blocked or done.
#[derive(Debug, Default)]
pub(crate) struct Transitions {
    known: HashMap<AgentId, (AgentStatus, Seq)>,
}

impl Transitions {
    /// Takes in one hub event; the changes in it that may warrant a ping.
    pub(crate) fn observe(&mut self, event: &AgentEvent) -> Vec<Change> {
        match event {
            AgentEvent::Snapshot { agents } => {
                // The first one or a resync after falling behind: diff it
                // like updates.
                let changes = agents.iter().filter_map(|agent| self.note(agent)).collect();
                self.known
                    .retain(|id, _| agents.iter().any(|agent| agent.id == *id));
                changes
            }
            AgentEvent::AgentUpdated { agent: Some(agent) } => {
                self.note(agent).into_iter().collect()
            }
            AgentEvent::AgentRemoved { agent_id } => {
                self.known.remove(agent_id);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Records `agent`'s state and says whether it just became blocked or
    /// finished. An agent not seen before only gets its baseline.
    fn note(&mut self, agent: &Agent) -> Option<Change> {
        let now = (agent.status, agent.seq);
        let before = self.known.insert(agent.id.clone(), now)?;
        let id = || agent.id.clone();
        match agent.status {
            AgentStatus::Blocked => (before != now).then(|| Change::Blocked(id())),
            status if status.is_ready() => {
                (before.0 == AgentStatus::Working).then(|| Change::Finished(id()))
            }
            _ => None,
        }
    }
}

/// A change that may be worth a ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    /// The agent is at a new dialog: always pings.
    Blocked(AgentId),
    /// The agent went `working` → `idle`/`done`: pings unless quiet.
    Finished(AgentId),
}

impl Change {
    /// Why the change no longer warrants a ping, given the agent in its pane
    /// now (`None`: gone); `None` while it still does. It does while the
    /// phone's notification would name the agent: still blocked, or
    /// finished with a turn the user has not seen (herdr's `done`; `idle`
    /// means seen) and not quiet.
    pub(crate) fn lapsed(&self, now: Option<&Agent>) -> Option<Lapse> {
        let Some(now) = now else {
            return Some(Lapse::Gone);
        };
        match (self, now.status) {
            (Self::Blocked(_), AgentStatus::Blocked) => None,
            (Self::Finished(_), AgentStatus::Done) if now.unread && !now.quiet_done => None,
            (Self::Finished(_), AgentStatus::Done) if now.quiet_done => Some(Lapse::Quiet),
            (_, status) if status.is_ready() => Some(Lapse::Seen),
            (Self::Finished(_), AgentStatus::Blocked) => Some(Lapse::Blocked),
            (_, AgentStatus::Working) => Some(Lapse::Working),
            _ => Some(Lapse::Unknown),
        }
    }
}

/// Why a change no longer warrants a ping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lapse {
    /// The agent left its pane.
    Gone,
    /// Answered or read: the agent is ready and its turn seen.
    Seen,
    /// It went back to work.
    Working,
    /// It finished, then blocked: that block is a change of its own.
    Blocked,
    /// Its finished turn is quiet (the orchestrator's).
    Quiet,
    /// herdr cannot tell what it is doing.
    Unknown,
}

impl Lapse {
    /// The reason, for logs.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Gone => "agent gone",
            Self::Seen => "answered or seen",
            Self::Working => "working again",
            Self::Blocked => "blocked since (pings on its own)",
            Self::Quiet => "quiet finished turn",
            Self::Unknown => "status unknown",
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use protocol::{AgentKind, Timestamp};

    use super::*;

    pub(crate) fn agent(pane: &str, status: AgentStatus, seq: u64) -> Agent {
        Agent {
            id: pane.parse().expect("id"),
            title: format!("title of {pane}"),
            repo: "repo".to_owned(),
            worktree: None,
            status,
            seq: Seq(seq),
            status_since: Timestamp::from_unix_millis(0),
            // As the registry maps herdr: `done` is a turn not yet seen.
            unread: status == AgentStatus::Done,
            kind: AgentKind::Claude,
            headline: None,
            status_line: None,
            ask: None,
            quiet_done: false,
        }
    }

    fn updated(pane: &str, status: AgentStatus, seq: u64) -> AgentEvent {
        AgentEvent::AgentUpdated {
            agent: Some(agent(pane, status, seq)),
        }
    }

    fn blocked(pane: &str) -> Vec<Change> {
        vec![Change::Blocked(pane.parse().expect("id"))]
    }

    fn finished(pane: &str) -> Vec<Change> {
        vec![Change::Finished(pane.parse().expect("id"))]
    }

    const NONE: Vec<Change> = Vec::new();

    fn primed(agents: Vec<Agent>) -> Transitions {
        let mut transitions = Transitions::default();
        assert_eq!(
            transitions.observe(&AgentEvent::Snapshot { agents }),
            NONE,
            "the first snapshot is the baseline"
        );
        transitions
    }

    #[test]
    fn becoming_blocked_counts_once_per_dialog() {
        let mut t = primed(vec![agent("w1:p1", AgentStatus::Working, 1)]);
        assert_eq!(
            t.observe(&updated("w1:p1", AgentStatus::Blocked, 2)),
            blocked("w1:p1")
        );
        assert_eq!(
            t.observe(&updated("w1:p1", AgentStatus::Blocked, 2)),
            NONE,
            "same dialog"
        );
        assert_eq!(
            t.observe(&updated("w1:p1", AgentStatus::Blocked, 3)),
            blocked("w1:p1"),
            "next dialog"
        );
        assert_eq!(
            t.observe(&updated("w9:p9", AgentStatus::Blocked, 1)),
            NONE,
            "new and blocked: only the baseline"
        );
        assert_eq!(
            t.observe(&updated("w9:p9", AgentStatus::Blocked, 2)),
            blocked("w9:p9"),
            "its next dialog"
        );
    }

    #[test]
    fn finishing_a_turn_counts_but_settling_does_not() {
        let mut t = primed(vec![
            agent("w1:p1", AgentStatus::Working, 1),
            agent("w1:p2", AgentStatus::Blocked, 1),
        ]);
        assert_eq!(
            t.observe(&updated("w1:p1", AgentStatus::Done, 2)),
            finished("w1:p1")
        );
        assert_eq!(
            t.observe(&updated("w1:p1", AgentStatus::Idle, 3)),
            NONE,
            "done → idle"
        );
        assert_eq!(
            t.observe(&updated("w1:p2", AgentStatus::Idle, 2)),
            NONE,
            "answered at the terminal"
        );
        assert_eq!(t.observe(&updated("w1:p2", AgentStatus::Working, 3)), NONE);
        assert_eq!(
            t.observe(&updated("w1:p2", AgentStatus::Idle, 4)),
            finished("w1:p2")
        );
        assert_eq!(
            t.observe(&updated("w2:p1", AgentStatus::Done, 1)),
            NONE,
            "new and done"
        );
        assert_eq!(t.observe(&updated("w1:p1", AgentStatus::Unknown, 5)), NONE);
    }

    #[test]
    fn other_events_are_silent_and_a_removed_agent_starts_over() {
        let mut t = primed(vec![agent("w1:p1", AgentStatus::Working, 1)]);
        assert_eq!(t.observe(&AgentEvent::AgentUpdated { agent: None }), NONE);
        assert_eq!(
            t.observe(&AgentEvent::AgentRemoved {
                agent_id: "w1:p1".parse().expect("id")
            }),
            NONE
        );
        // Removed, so a later agent in that pane is new: a baseline.
        assert_eq!(t.observe(&updated("w1:p1", AgentStatus::Blocked, 1)), NONE);
        assert_eq!(
            t.observe(&updated("w1:p1", AgentStatus::Blocked, 2)),
            blocked("w1:p1")
        );
    }

    #[test]
    fn a_restart_does_not_count_agents_already_blocked_or_done() {
        // The dispatcher subscribes before the registry's first sync: an
        // empty snapshot, then herdr's agents arrive as updates.
        let mut t = primed(Vec::new());
        assert_eq!(t.observe(&updated("w1:p1", AgentStatus::Blocked, 4)), NONE);
        assert_eq!(t.observe(&updated("w1:p2", AgentStatus::Done, 2)), NONE);
        assert_eq!(t.observe(&updated("w1:p1", AgentStatus::Blocked, 4)), NONE);
        // From here on they are known, and real transitions count.
        assert_eq!(
            t.observe(&updated("w1:p1", AgentStatus::Blocked, 5)),
            blocked("w1:p1")
        );
        assert_eq!(t.observe(&updated("w1:p2", AgentStatus::Working, 3)), NONE);
        assert_eq!(
            t.observe(&updated("w1:p2", AgentStatus::Done, 4)),
            finished("w1:p2")
        );
    }

    #[test]
    fn a_change_lasts_while_the_notification_would_name_the_agent() {
        let blocked = Change::Blocked("w1:p1".parse().expect("id"));
        let finished = Change::Finished("w1:p1".parse().expect("id"));
        let now = |status| agent("w1:p1", status, 9);
        let done = now(AgentStatus::Done);

        assert_eq!(blocked.lapsed(Some(&now(AgentStatus::Blocked))), None);
        assert_eq!(finished.lapsed(Some(&done)), None);

        assert_eq!(
            blocked.lapsed(Some(&now(AgentStatus::Working))),
            Some(Lapse::Working),
            "answered and back at work"
        );
        assert_eq!(
            blocked.lapsed(Some(&now(AgentStatus::Idle))),
            Some(Lapse::Seen)
        );
        assert_eq!(
            finished.lapsed(Some(&now(AgentStatus::Working))),
            Some(Lapse::Working),
            "prompted again"
        );
        assert_eq!(
            finished.lapsed(Some(&now(AgentStatus::Idle))),
            Some(Lapse::Seen),
            "read at the desk"
        );
        let mut read = done.clone();
        read.unread = false;
        assert_eq!(finished.lapsed(Some(&read)), Some(Lapse::Seen));
        let mut quiet = done;
        quiet.quiet_done = true;
        assert_eq!(finished.lapsed(Some(&quiet)), Some(Lapse::Quiet));
        assert_eq!(
            finished.lapsed(Some(&now(AgentStatus::Blocked))),
            Some(Lapse::Blocked)
        );
        assert_eq!(
            blocked.lapsed(Some(&now(AgentStatus::Unknown))),
            Some(Lapse::Unknown)
        );
        assert_eq!(blocked.lapsed(None), Some(Lapse::Gone));
    }

    #[test]
    fn a_resync_snapshot_is_diffed_against_what_was_known() {
        let mut t = primed(vec![
            agent("w1:p1", AgentStatus::Working, 1),
            agent("w1:p2", AgentStatus::Working, 1),
        ]);
        assert_eq!(
            t.observe(&AgentEvent::Snapshot {
                agents: vec![agent("w1:p1", AgentStatus::Working, 1)],
            }),
            NONE
        );
        assert_eq!(
            t.observe(&AgentEvent::Snapshot {
                agents: vec![
                    agent("w1:p1", AgentStatus::Blocked, 2),
                    agent("w1:p3", AgentStatus::Done, 1),
                ],
            }),
            blocked("w1:p1"),
            "every agent noted, the new one only as a baseline"
        );
    }
}
