//! The live agent list and its change feed.
//!
//! The [`Hub`] holds the current [`Agent`] per herdr pane and broadcasts an
//! [`AgentEvent`] for every change. State and broadcast are updated under one
//! lock, and [`Hub::subscribe`] takes its snapshot under the same lock, so a
//! subscriber sees exactly the changes after its snapshot: none missed, none
//! twice.
//!
//! The hub also holds each blocked agent's parsed pending input (see
//! [`crate::pending`]) under the same lock: [`Hub::set_pending`] broadcasts
//! `pending_changed` when it changes, an agent leaving `blocked` or moving
//! to a new `seq` clears it (with a `pending_changed` to `null`), and a new subscriber gets one
//! `pending_changed` per known input right after its snapshot. The
//! snapshot, and [`Hub::cards`], carry each blocked agent's ask (its
//! input's prompt, [`Agent::ask`]); the stream keeps it current from the
//! `pending_changed` events that follow.
//!
//! The hub also remembers where herdr placed each agent's pane: its
//! workspace label and herdr agent name ([`Hub::placement`]). That is
//! bridge-internal (the push dispatcher uses it to quiet "done" pings for
//! swarm agents) and never sent to clients.
//!
//! It also notes when herdr detected each pane's agent
//! ([`Hub::note_arrival`], [`Hub::arrived`]): the time of the detection
//! event, which agents running before the bridge subscribed never get: a Codex agent without a session id
//! uses it to ignore rollouts started before it was.
//!
//! Each subscriber reads from a bounded broadcast queue. One that falls more
//! than the queue's capacity behind is told so ([`Feed::next`] then hands it a
//! fresh snapshot) instead of the hub buffering without limit.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::{
    Agent, AgentEvent, AgentId, AgentStatus, PendingId, PendingInput, PendingSource, ScreenTail,
    Timestamp,
};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use super::mapping::ask;

/// Whether `a` and `b` are the same dialog: equal but for the screen tail,
/// which moves with whatever else is on the screen (a spinner's timer, a
/// status line) while the dialog waits. Every reader re-reads the input on
/// `pending_changed`, and each read parses the screen again, so a tail
/// change counted as a new dialog would never let the chips settle.
fn same_dialog(a: &PendingInput, b: &PendingInput) -> bool {
    let without_tail = |input: &PendingInput| {
        let mut input = input.clone();
        if let PendingSource::Screen { screen_tail, .. } = &mut input.source {
            *screen_tail = ScreenTail::default();
        }
        input
    };
    a == b || without_tail(a) == without_tail(b)
}

/// Where herdr placed an agent, as far as the registry knows: facts that
/// tell the orchestrator's agents from the user's own. Bridge-internal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    /// The label of the agent's herdr workspace; `None` while unknown.
    pub workspace: Option<String>,
    /// The agent's herdr name, if it was given one (`agent start`).
    pub name: Option<String>,
}

/// The live agent list. Shared by the registry driver (the only writer) and
/// the HTTP handlers.
#[derive(Debug)]
pub struct Hub {
    state: Mutex<State>,
    events: broadcast::Sender<AgentEvent>,
}

#[derive(Debug, Default)]
struct State {
    agents: BTreeMap<herdr::PaneId, Agent>,
    /// Parsed pending input, only for agents in `agents` that are blocked.
    pending: BTreeMap<AgentId, PendingInput>,
    /// What herdr says about each pane beyond the agent (see [`Placement`]).
    placements: BTreeMap<herdr::PaneId, Placement>,
    /// When herdr's event detected the agent in each pane, if one did.
    arrived: BTreeMap<herdr::PaneId, Timestamp>,
}

impl State {
    /// Every agent, each blocked one with its [`Agent::ask`] from its
    /// pending input when one is known.
    fn cards(&self) -> Vec<Agent> {
        self.agents
            .values()
            .map(|agent| {
                let mut card = agent.clone();
                card.ask = (agent.status == AgentStatus::Blocked)
                    .then(|| self.pending.get(&agent.id).and_then(ask))
                    .flatten();
                card
            })
            .collect()
    }

    /// The snapshot (as [cards](Self::cards)) plus a `pending_changed` per
    /// known input.
    fn snapshot(&self) -> (Vec<Agent>, VecDeque<AgentEvent>) {
        let agents = self.cards();
        let pending = self
            .pending
            .iter()
            .map(|(agent_id, pending)| AgentEvent::PendingChanged {
                agent_id: agent_id.clone(),
                pending: Some(pending.clone()),
            })
            .collect();
        (agents, pending)
    }
}

impl Hub {
    /// An empty hub whose subscribers may each fall `queue` events behind
    /// before being resynced with a snapshot.
    #[must_use]
    pub fn new(queue: usize) -> Self {
        let (events, _) = broadcast::channel(queue.max(1));
        Self {
            state: Mutex::new(State::default()),
            events,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every agent, ordered by pane.
    #[must_use]
    pub fn agents(&self) -> Vec<Agent> {
        self.lock().agents.values().cloned().collect()
    }

    /// Every agent as its triage card: [`Self::agents`], each blocked one
    /// with what it is asking ([`Agent::ask`]) from its last parsed pending
    /// input. Reads only what the hub holds, never a screen.
    #[must_use]
    pub fn cards(&self) -> Vec<Agent> {
        self.lock().cards()
    }

    /// The agent currently in `pane`, if any.
    #[must_use]
    pub fn agent_in(&self, pane: &protocol::PaneId) -> Option<Agent> {
        self.lock()
            .agents
            .values()
            .find(|agent| agent.id.pane() == pane)
            .cloned()
    }

    /// The last parsed pending input of `agent`, if it is blocked.
    #[must_use]
    pub fn pending(&self, agent: &AgentId) -> Option<PendingInput> {
        self.lock().pending.get(agent).cloned()
    }

    /// The id of `agent`'s last parsed pending input and what it asks (as
    /// its card shows it), without copying the input.
    #[must_use]
    pub(crate) fn dialog(&self, agent: &AgentId) -> Option<(PendingId, Option<String>)> {
        self.lock()
            .pending
            .get(agent)
            .map(|pending| (pending.id.clone(), ask(pending)))
    }

    /// When herdr detected `agent` in its pane, if the bridge saw the
    /// detection event (agents found by a list have no known start).
    #[must_use]
    pub fn arrived(&self, agent: &AgentId) -> Option<Timestamp> {
        let state = self.lock();
        let pane = herdr::PaneId::new(agent.pane().as_str());
        let known = state.agents.get(&pane)?;
        (known.id == *agent)
            .then(|| state.arrived.get(&pane).copied())
            .flatten()
    }

    /// Where herdr placed the agent in `pane`; `None` if no agent is there.
    /// Keyed by pane rather than [`AgentId`], so it still answers for an
    /// agent whose session id appeared since.
    #[must_use]
    pub fn placement(&self, pane: &protocol::PaneId) -> Option<Placement> {
        let state = self.lock();
        let pane = herdr::PaneId::new(pane.as_str());
        state
            .agents
            .contains_key(&pane)
            .then(|| state.placements.get(&pane).cloned().unwrap_or_default())
    }

    /// Records where herdr placed `pane`. Set it before upserting the pane's
    /// agent, so whoever reacts to the agent's update already sees it.
    /// Broadcasts nothing.
    pub(crate) fn set_placement(&self, pane: &herdr::PaneId, placement: Placement) {
        self.lock().placements.insert(pane.clone(), placement);
    }

    /// Records `agent`'s parsed pending input, broadcasting
    /// `pending_changed` if the dialog changed ([`same_dialog`]). `Some` is
    /// ignored unless the hub knows the agent as blocked (the registry
    /// catches up, and the next parse lands); `None` clears.
    pub(crate) fn set_pending(&self, agent: &AgentId, pending: Option<PendingInput>) {
        let mut state = self.lock();
        match pending {
            Some(pending) => {
                let blocked = state
                    .agents
                    .values()
                    .any(|known| known.id == *agent && known.status == AgentStatus::Blocked);
                if !blocked {
                    return;
                }
                let changed = state
                    .pending
                    .insert(agent.clone(), pending.clone())
                    .is_none_or(|was| !same_dialog(&was, &pending));
                if !changed {
                    return;
                }
                self.send(AgentEvent::PendingChanged {
                    agent_id: agent.clone(),
                    pending: Some(pending),
                });
            }
            None => {
                if state.pending.remove(agent).is_some() {
                    self.send(AgentEvent::PendingChanged {
                        agent_id: agent.clone(),
                        pending: None,
                    });
                }
            }
        }
    }

    /// Broadcasts an event that is not part of the agent list (e.g. new
    /// thread items) to every subscriber.
    pub(crate) fn publish(&self, event: AgentEvent) {
        self.send(event);
    }

    /// A snapshot plus every change after it.
    #[must_use]
    pub fn subscribe(self: &Arc<Self>) -> Feed {
        let state = self.lock();
        let rx = self.events.subscribe();
        let (snapshot, queued) = state.snapshot();
        drop(state);
        Feed {
            hub: Arc::clone(self),
            rx,
            snapshot: Some(snapshot),
            queued,
        }
    }

    /// Notes that herdr detected an agent in `pane` at `at` (a detection
    /// event, possibly before the agent could be read). Kept until the
    /// pane's agent is forgotten or another detection replaces it.
    pub(crate) fn note_arrival(&self, pane: &herdr::PaneId, at: Timestamp) {
        self.lock().arrived.insert(pane.clone(), at);
    }

    /// Records `next` for `pane`, broadcasting the change if there is one.
    /// `next.status_since` is replaced by the previous value unless the agent
    /// actually moved (another status or `seq`, or another session).
    pub(crate) fn upsert(&self, pane: &herdr::PaneId, next: Agent) {
        let mut state = self.lock();
        self.upsert_locked(&mut state, pane, next);
    }

    /// Forgets the agent in `pane`, broadcasting its removal if it was known.
    pub(crate) fn remove(&self, pane: &herdr::PaneId) {
        let mut state = self.lock();
        self.remove_locked(&mut state, pane);
    }

    /// Replaces the whole list (a full resync), broadcasting only the
    /// differences. Applied under one lock, so no reader or subscriber ever
    /// sees a half-applied resync.
    pub(crate) fn replace_all(&self, next: BTreeMap<herdr::PaneId, Agent>) {
        let mut state = self.lock();
        let gone: Vec<herdr::PaneId> = state
            .agents
            .keys()
            .filter(|pane| !next.contains_key(*pane))
            .cloned()
            .collect();
        for pane in &gone {
            self.remove_locked(&mut state, pane);
        }
        for (pane, agent) in next {
            self.upsert_locked(&mut state, &pane, agent);
        }
    }

    fn upsert_locked(&self, state: &mut State, pane: &herdr::PaneId, mut next: Agent) {
        let prev_seq = state.agents.get(pane).map(|prev| prev.seq);
        if let Some(prev) = state.agents.get(pane) {
            if prev.id != next.id {
                state.pending.remove(&prev.id);
                self.send(AgentEvent::AgentRemoved {
                    agent_id: prev.id.clone(),
                });
            } else if prev.status == next.status && prev.seq == next.seq {
                next.status_since = prev.status_since;
            }
            if *prev == next {
                return;
            }
        }
        state.agents.insert(pane.clone(), next.clone());
        // A pending input belongs to one blocked state: leaving `blocked`,
        // or a new `seq` while blocked (another dialog, maybe), drops it
        // until the next parse.
        let moved_on = next.status != AgentStatus::Blocked || prev_seq != Some(next.seq);
        let id = next.id.clone();
        self.send(AgentEvent::AgentUpdated { agent: Some(next) });
        if moved_on && state.pending.remove(&id).is_some() {
            self.send(AgentEvent::PendingChanged {
                agent_id: id,
                pending: None,
            });
        }
    }

    fn remove_locked(&self, state: &mut State, pane: &herdr::PaneId) {
        state.placements.remove(pane);
        state.arrived.remove(pane);
        if let Some(gone) = state.agents.remove(pane) {
            state.pending.remove(&gone.id);
            self.send(AgentEvent::AgentRemoved { agent_id: gone.id });
        }
    }

    fn send(&self, event: AgentEvent) {
        // No subscribers is fine: nobody is watching.
        let _ = self.events.send(event);
    }
}

/// One subscriber's view of the [`Hub`]: a snapshot, the known pending
/// inputs, then changes.
#[derive(Debug)]
pub struct Feed {
    hub: Arc<Hub>,
    rx: broadcast::Receiver<AgentEvent>,
    snapshot: Option<Vec<Agent>>,
    /// `pending_changed` events that follow the snapshot.
    queued: VecDeque<AgentEvent>,
}

impl Feed {
    /// The next event to send: a [`AgentEvent::Snapshot`] first, then one
    /// [`AgentEvent::PendingChanged`] per known pending input, then
    /// changes. A subscriber that fell too far behind gets a fresh snapshot
    /// (and pending inputs) in place of the changes it missed.
    pub async fn next(&mut self) -> AgentEvent {
        if let Some(agents) = self.snapshot.take() {
            return AgentEvent::Snapshot { agents };
        }
        if let Some(event) = self.queued.pop_front() {
            return event;
        }
        match self.rx.recv().await {
            Ok(event) => event,
            // `Closed` cannot happen while this feed holds the hub (and so the
            // sender); resubscribing covers it without a panic path.
            Err(RecvError::Lagged(_) | RecvError::Closed) => {
                tracing::debug!("stream subscriber fell behind; resending snapshot");
                let fresh = self.hub.subscribe();
                self.rx = fresh.rx;
                self.queued = fresh.queued;
                AgentEvent::Snapshot {
                    agents: fresh.snapshot.unwrap_or_default(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use protocol::{AgentKind, AgentStatus, PendingId, Seq, Timestamp};

    use super::*;

    fn agent(pane: &str, status: AgentStatus, seq: u64, since: u64) -> Agent {
        Agent {
            id: pane.parse().expect("id"),
            title: pane.to_owned(),
            repo: "r".to_owned(),
            worktree: None,
            status,
            seq: Seq(seq),
            status_since: Timestamp::from_unix_millis(since),
            unread: false,
            kind: AgentKind::Claude,
            headline: None,
            status_line: None,
            ask: None,
            quiet_done: false,
        }
    }

    fn pane(id: &str) -> herdr::PaneId {
        herdr::PaneId::new(id)
    }

    async fn next(feed: &mut Feed) -> AgentEvent {
        tokio::time::timeout(Duration::from_secs(1), feed.next())
            .await
            .expect("event in time")
    }

    fn assert_quiet(feed: &mut Feed) {
        assert!(feed.rx.try_recv().is_err(), "expected no further events");
    }

    #[tokio::test]
    async fn snapshot_then_changes() {
        let hub = Arc::new(Hub::new(16));
        hub.upsert(&pane("w1:p1"), agent("w1:p1", AgentStatus::Idle, 1, 10));
        let mut feed = hub.subscribe();
        let AgentEvent::Snapshot { agents } = next(&mut feed).await else {
            panic!("snapshot first");
        };
        assert_eq!(agents.len(), 1);

        hub.upsert(&pane("w1:p1"), agent("w1:p1", AgentStatus::Working, 2, 20));
        let AgentEvent::AgentUpdated { agent: Some(a) } = next(&mut feed).await else {
            panic!("update");
        };
        assert_eq!((a.status, a.seq), (AgentStatus::Working, Seq(2)));

        hub.remove(&pane("w1:p1"));
        assert_eq!(
            next(&mut feed).await,
            AgentEvent::AgentRemoved {
                agent_id: "w1:p1".parse().expect("id")
            }
        );
        assert_eq!(hub.agents(), Vec::<Agent>::new());
    }

    #[tokio::test]
    async fn unchanged_upserts_are_silent_and_keep_status_since() {
        let hub = Arc::new(Hub::new(16));
        hub.upsert(&pane("w1:p1"), agent("w1:p1", AgentStatus::Idle, 1, 10));
        let mut feed = hub.subscribe();
        let _ = next(&mut feed).await;
        hub.upsert(&pane("w1:p1"), agent("w1:p1", AgentStatus::Idle, 1, 99));
        assert_quiet(&mut feed);
        assert_eq!(
            hub.agents()[0].status_since,
            Timestamp::from_unix_millis(10)
        );
    }

    #[tokio::test]
    async fn a_new_session_in_the_same_pane_replaces_the_agent() {
        let hub = Arc::new(Hub::new(16));
        hub.upsert(&pane("w1:p1"), agent("w1:p1", AgentStatus::Idle, 1, 10));
        let mut feed = hub.subscribe();
        let _ = next(&mut feed).await;
        hub.upsert(&pane("w1:p1"), agent("w1:p1~s2", AgentStatus::Idle, 1, 20));
        assert!(
            matches!(next(&mut feed).await, AgentEvent::AgentRemoved { agent_id } if agent_id.to_string() == "w1:p1")
        );
        let AgentEvent::AgentUpdated { agent: Some(a) } = next(&mut feed).await else {
            panic!("update");
        };
        assert_eq!(a.id.to_string(), "w1:p1~s2");
        assert_eq!(
            a.status_since,
            Timestamp::from_unix_millis(20),
            "a new agent"
        );
    }

    #[tokio::test]
    async fn placements_follow_the_pane_and_are_silent() {
        let hub = Arc::new(Hub::new(16));
        let swarm = |name: Option<&str>| Placement {
            workspace: Some("agent-swarm".to_owned()),
            name: name.map(str::to_owned),
        };
        let id: AgentId = "w1:p1".parse().expect("id");
        hub.set_placement(&pane("w1:p1"), swarm(Some("impl-t1")));
        assert_eq!(hub.placement(id.pane()), None, "no agent there yet");
        hub.upsert(&pane("w1:p1"), agent("w1:p1", AgentStatus::Idle, 1, 10));
        let mut feed = hub.subscribe();
        let _ = next(&mut feed).await;
        assert_eq!(hub.placement(id.pane()), Some(swarm(Some("impl-t1"))));

        hub.set_placement(&pane("w1:p1"), swarm(None));
        assert_quiet(&mut feed);
        assert_eq!(hub.placement(id.pane()), Some(swarm(None)));

        hub.remove(&pane("w1:p1"));
        hub.upsert(&pane("w1:p1"), agent("w1:p1", AgentStatus::Idle, 1, 10));
        assert_eq!(
            hub.placement(id.pane()),
            Some(Placement::default()),
            "forgotten with the agent"
        );
    }

    #[test]
    fn arrivals_are_detection_times_kept_until_the_agent_goes() {
        let hub = Hub::new(16);
        let listed = agent("w1:p1", AgentStatus::Idle, 1, 10);
        hub.replace_all(BTreeMap::from([(pane("w1:p1"), listed.clone())]));
        assert_eq!(hub.arrived(&listed.id), None, "found by a list");
        let detected = Timestamp::from_unix_millis(1_000);
        hub.note_arrival(&pane("w1:p2"), detected);
        // Read later (a retry, or once its kind is known): still the
        // detection time.
        let later = agent("w1:p2", AgentStatus::Idle, 1, 10);
        hub.upsert(&pane("w1:p2"), later.clone());
        assert_eq!(hub.arrived(&later.id), Some(detected));
        hub.upsert(&pane("w1:p2"), agent("w1:p2", AgentStatus::Working, 2, 20));
        hub.replace_all(BTreeMap::from([(pane("w1:p2"), later.clone())]));
        assert_eq!(hub.arrived(&later.id), Some(detected));
        hub.remove(&pane("w1:p2"));
        assert_eq!(hub.arrived(&later.id), None);
    }

    #[tokio::test]
    async fn replace_all_sends_only_differences() {
        let hub = Arc::new(Hub::new(16));
        hub.upsert(&pane("w1:p1"), agent("w1:p1", AgentStatus::Idle, 1, 10));
        hub.upsert(&pane("w1:p2"), agent("w1:p2", AgentStatus::Idle, 1, 10));
        let mut feed = hub.subscribe();
        let _ = next(&mut feed).await;

        let mut next_list = BTreeMap::new();
        next_list.insert(pane("w1:p1"), agent("w1:p1", AgentStatus::Idle, 1, 50));
        next_list.insert(pane("w1:p3"), agent("w1:p3", AgentStatus::Blocked, 4, 50));
        hub.replace_all(next_list);

        assert!(
            matches!(next(&mut feed).await, AgentEvent::AgentRemoved { agent_id } if agent_id.to_string() == "w1:p2")
        );
        assert!(
            matches!(next(&mut feed).await, AgentEvent::AgentUpdated { agent: Some(a) } if a.id.to_string() == "w1:p3")
        );
        assert_quiet(&mut feed);
        let ids: Vec<String> = hub.agents().iter().map(|a| a.id.to_string()).collect();
        assert_eq!(ids, ["w1:p1", "w1:p3"]);
    }

    #[tokio::test]
    async fn a_lagging_subscriber_gets_a_fresh_snapshot() {
        let hub = Arc::new(Hub::new(2));
        let p = pane("w1:p1");
        hub.upsert(&p, agent("w1:p1", AgentStatus::Idle, 1, 0));
        let mut feed = hub.subscribe();
        let _ = next(&mut feed).await;
        for seq in 2..10 {
            let status = if seq % 2 == 0 {
                AgentStatus::Working
            } else {
                AgentStatus::Idle
            };
            hub.upsert(&p, agent("w1:p1", status, seq, seq));
        }
        let AgentEvent::Snapshot { agents } = next(&mut feed).await else {
            panic!("expected a resync snapshot");
        };
        assert_eq!(agents[0].seq, Seq(9), "the snapshot is current");
        assert_quiet(&mut feed);

        hub.upsert(&p, agent("w1:p1", AgentStatus::Blocked, 10, 10));
        assert!(matches!(
            next(&mut feed).await,
            AgentEvent::AgentUpdated { .. }
        ));
    }

    fn input(id: &str) -> PendingInput {
        PendingInput::unparsed_dialog(PendingId::new(id).expect("id"))
    }

    fn with_tail(mut input: PendingInput, line: &str) -> PendingInput {
        input.source = PendingSource::Screen {
            confidence: protocol::Confidence::new(90).expect("confidence"),
            screen_tail: ScreenTail::new(vec![line.to_owned()]),
        };
        input
    }

    #[test]
    fn a_dialog_is_the_same_whatever_else_the_screen_shows() {
        let a = with_tail(input("screen-1"), "Running… (3s)");
        let b = with_tail(input("screen-1"), "Running… (4s)");
        assert!(same_dialog(&a, &b));
        assert!(!same_dialog(
            &a,
            &with_tail(input("screen-2"), "Running… (3s)")
        ));
        assert!(!same_dialog(&a, &input("screen-1")), "another source");
    }

    #[tokio::test]
    async fn pending_input_is_kept_for_blocked_agents_only() {
        let hub = Arc::new(Hub::new(16));
        let p = pane("w1:p1");
        let id: AgentId = "w1:p1".parse().expect("id");
        hub.upsert(&p, agent("w1:p1", AgentStatus::Idle, 1, 0));
        let mut feed = hub.subscribe();
        let _ = next(&mut feed).await;

        hub.set_pending(&id, Some(input("screen-1")));
        assert_quiet(&mut feed);
        assert_eq!(hub.pending(&id), None, "not blocked");

        hub.upsert(&p, agent("w1:p1", AgentStatus::Blocked, 2, 1));
        let _ = next(&mut feed).await;
        hub.set_pending(&id, Some(input("screen-2")));
        assert_eq!(
            next(&mut feed).await,
            AgentEvent::PendingChanged {
                agent_id: id.clone(),
                pending: Some(input("screen-2"))
            }
        );
        hub.set_pending(&id, Some(input("screen-2")));
        assert_quiet(&mut feed);
        // Read off the screen, then only the rest of the screen moved: the
        // same dialog, kept fresh but not announced.
        hub.set_pending(&id, Some(with_tail(input("screen-2"), "Running… (3s)")));
        let _ = next(&mut feed).await;
        let ticked = with_tail(input("screen-2"), "Running… (4s)");
        hub.set_pending(&id, Some(ticked.clone()));
        assert_quiet(&mut feed);
        assert_eq!(hub.pending(&id), Some(ticked));

        // A late subscriber gets it right after the snapshot.
        let mut late = hub.subscribe();
        assert!(matches!(next(&mut late).await, AgentEvent::Snapshot { .. }));
        assert!(matches!(
            next(&mut late).await,
            AgentEvent::PendingChanged {
                pending: Some(_),
                ..
            }
        ));

        // Leaving `blocked` clears it.
        hub.upsert(&p, agent("w1:p1", AgentStatus::Working, 3, 2));
        assert!(matches!(
            next(&mut feed).await,
            AgentEvent::AgentUpdated { .. }
        ));
        assert_eq!(
            next(&mut feed).await,
            AgentEvent::PendingChanged {
                agent_id: id.clone(),
                pending: None
            }
        );
        assert_eq!(hub.pending(&id), None);

        // Removing the agent drops it silently (agent_removed says enough).
        hub.upsert(&p, agent("w1:p1", AgentStatus::Blocked, 4, 3));
        hub.set_pending(&id, Some(input("screen-4")));
        hub.remove(&p);
        assert_eq!(hub.pending(&id), None);
    }

    #[tokio::test]
    async fn cards_and_snapshots_carry_what_blocked_agents_ask() {
        let hub = Arc::new(Hub::new(16));
        let (p1, p2) = (pane("w1:p1"), pane("w1:p2"));
        let id: AgentId = "w1:p1".parse().expect("id");
        hub.upsert(&p1, agent("w1:p1", AgentStatus::Blocked, 1, 0));
        hub.upsert(&p2, agent("w1:p2", AgentStatus::Working, 1, 0));
        let mut asking = input("screen-1");
        asking.prompt = "Run `cargo test`?\nnow".to_owned();
        hub.set_pending(&id, Some(asking));

        let asks = |agents: &[Agent]| -> Vec<Option<String>> {
            agents.iter().map(|a| a.ask.clone()).collect()
        };
        let want = [Some("Run `cargo test`? now".to_owned()), None];
        assert_eq!(asks(&hub.cards()), want);
        assert_eq!(asks(&hub.agents()), [None, None], "the list as herdr says");
        let mut feed = hub.subscribe();
        let AgentEvent::Snapshot { agents } = next(&mut feed).await else {
            panic!("snapshot first");
        };
        assert_eq!(asks(&agents), want);
        assert!(matches!(
            next(&mut feed).await,
            AgentEvent::PendingChanged {
                pending: Some(_),
                ..
            }
        ));

        // A new blocked state: the old dialog's ask goes with it.
        hub.upsert(&p1, agent("w1:p1", AgentStatus::Blocked, 2, 1));
        assert!(matches!(
            next(&mut feed).await,
            AgentEvent::AgentUpdated { .. }
        ));
        assert_eq!(
            next(&mut feed).await,
            AgentEvent::PendingChanged {
                agent_id: id.clone(),
                pending: None
            }
        );
        assert_eq!(asks(&hub.cards()), [None, None], "until the next parse");
        hub.set_pending(&id, Some(input("screen-2")));
        hub.upsert(&p1, agent("w1:p1", AgentStatus::Working, 3, 2));
        assert_eq!(asks(&hub.cards()), [None, None], "answered");
    }
}
