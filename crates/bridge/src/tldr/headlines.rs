//! Generated turn headlines, kept per (session, turn).
//!
//! Unlike a TL;DR, a final headline is only ever made for a finished,
//! settled turn (by its job), so the (session, turn) pair alone identifies
//! it, and it can be looked up without reading the transcript: the agent
//! list and the stream decorate every agent with the headline of its
//! session's latest finished turn. A turn with nothing to headline is
//! recorded as such, so it is not asked for again.
//!
//! Each headline remembers how many thread items its turn had when it was
//! made: a turn can grow after it was seen finished (task notifications
//! and queued prompts continue it), and a grown turn needs a new headline
//! ([`HeadlineStore::covers`]).
//!
//! The turn in progress may also hold a *provisional* headline, made from
//! the turn so far by the status-line job (see [`super::status`]). It is
//! kept beside the turn's final headline, never in its place: thread
//! pages serve it (marked as such) while it is the newer of the two, but
//! it never counts as covering its turn, so the turn still gets its final
//! headline when it finishes, and the card headline is only ever a final
//! one ([`HeadlineStore::get`]). A final headline made from at least as
//! much of the turn clears it, and refuses a late one
//! ([`HeadlineStore::put_provisional`]).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use protocol::{SessionId, TurnHeadline, TurnIndex};

/// Most headlines kept in all; the oldest stored are forgotten first.
pub(super) const MAX_HEADLINES: usize = 4096;

/// Most headlines a thread page fetched from the start carries: the
/// session's most recent.
pub(super) const MAX_PAGE_HEADLINES: usize = 200;

/// Most headlines a later (incremental) thread page carries.
pub(super) const MAX_RECENT_HEADLINES: usize = 8;

/// The headline store, shared by TL;DR jobs (final headlines) and status
/// lines (provisional ones).
pub(super) type SharedHeadlines = Arc<Mutex<HeadlineStore>>;

/// A headline and its turn's thread items when it was made.
#[derive(Debug)]
struct Made<T> {
    text: T,
    items: usize,
}

/// One turn's headlines.
#[derive(Debug, Default)]
struct Kept {
    /// The final headline, once made; its text is `None` when the turn
    /// has nothing to headline (and needs none).
    settled: Option<Made<Option<String>>>,
    /// A provisional headline made from the turn after (or without) the
    /// final one.
    provisional: Option<Made<String>>,
}

impl Kept {
    /// What a thread page shows: the provisional headline while there is
    /// one (it is newer), else the final one.
    fn shown(&self) -> Option<(&str, bool)> {
        match (&self.provisional, &self.settled) {
            (Some(made), _) => Some((made.text.as_str(), true)),
            (None, Some(made)) => made.text.as_deref().map(|text| (text, false)),
            (None, None) => None,
        }
    }
}

/// A bounded map of headlines by session and turn.
#[derive(Debug)]
pub(super) struct HeadlineStore {
    capacity: usize,
    by_session: HashMap<SessionId, BTreeMap<TurnIndex, Kept>>,
    /// Insertion order, for eviction.
    order: VecDeque<(SessionId, TurnIndex)>,
}

impl Default for HeadlineStore {
    fn default() -> Self {
        Self::new(MAX_HEADLINES)
    }
}

impl HeadlineStore {
    /// A store holding at most `capacity` headlines (at least one).
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            by_session: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Whether `turn` of `session`, now `items` long, is done: it has a
    /// final headline, or was found to need none, made when it was at
    /// least that long. A provisional headline never covers its turn.
    pub(super) fn covers(&self, session: &SessionId, turn: TurnIndex, items: usize) -> bool {
        self.kept(session, turn)
            .and_then(|kept| kept.settled.as_ref())
            .is_some_and(|made| made.items >= items)
    }

    fn kept(&self, session: &SessionId, turn: TurnIndex) -> Option<&Kept> {
        self.by_session.get(session)?.get(&turn)
    }

    /// Whether `turn` of `session` has a headline (or needs none), however
    /// long the turn has grown since.
    pub(super) fn contains(&self, session: &SessionId, turn: TurnIndex) -> bool {
        self.covers(session, turn, 0)
    }

    /// Stores the final headline of `turn` of `session`, made when it was
    /// `items` long (`None`: it needs none), replacing an earlier one and
    /// clearing a provisional one made from no more of the turn.
    pub(super) fn put(
        &mut self,
        session: &SessionId,
        turn: TurnIndex,
        text: Option<String>,
        items: usize,
    ) {
        let kept = self.entry(session, turn);
        if kept.provisional.as_ref().is_some_and(|p| p.items <= items) {
            kept.provisional = None;
        }
        kept.settled = Some(Made { text, items });
        self.evict();
    }

    /// Stores a provisional headline of `turn` of `session`, made from the
    /// turn so far (`items` long), beside its final one, unless that final
    /// one was made from at least as much of the turn (a job that raced
    /// the turn's end). Returns whether it was stored.
    pub(super) fn put_provisional(
        &mut self,
        session: &SessionId,
        turn: TurnIndex,
        text: String,
        items: usize,
    ) -> bool {
        if self.covers(session, turn, items) {
            return false;
        }
        self.entry(session, turn).provisional = Some(Made { text, items });
        self.evict();
        true
    }

    /// The entry for `turn` of `session`, made (and queued for eviction)
    /// if new.
    fn entry(&mut self, session: &SessionId, turn: TurnIndex) -> &mut Kept {
        let turns = self.by_session.entry(session.clone()).or_default();
        if !turns.contains_key(&turn) {
            self.order.push_back((session.clone(), turn));
        }
        turns.entry(turn).or_default()
    }

    /// Forgets the oldest stored turns beyond the capacity.
    fn evict(&mut self) {
        while self.order.len() > self.capacity {
            let Some((session, turn)) = self.order.pop_front() else {
                break;
            };
            if let Some(turns) = self.by_session.get_mut(&session) {
                turns.remove(&turn);
                if turns.is_empty() {
                    self.by_session.remove(&session);
                }
            }
        }
    }

    /// The final headline of `turn` of `session`, if it has one: what the
    /// card shows, never a provisional one.
    pub(super) fn get(&self, session: &SessionId, turn: TurnIndex) -> Option<&str> {
        self.kept(session, turn)?.settled.as_ref()?.text.as_deref()
    }

    /// The most recent `max` headlines of `session`, in turn order.
    pub(super) fn recent(&self, session: &SessionId, max: usize) -> Vec<TurnHeadline> {
        let Some(turns) = self.by_session.get(session) else {
            return Vec::new();
        };
        let mut recent: Vec<TurnHeadline> = turns
            .iter()
            .rev()
            .filter_map(|(turn, kept)| {
                kept.shown().map(|(text, provisional)| TurnHeadline {
                    turn: *turn,
                    text: text.to_owned(),
                    provisional,
                })
            })
            .take(max)
            .collect();
        recent.reverse();
        recent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(n: u32) -> SessionId {
        SessionId::new(format!("00000000-0000-4000-8000-{n:012}")).expect("session")
    }

    #[test]
    fn lookups_and_recent_follow_turn_order() {
        let mut store = HeadlineStore::default();
        let s = session(1);
        assert_eq!(store.get(&s, TurnIndex(3)), None);
        store.put(&s, TurnIndex(3), Some("third".into()), 1);
        store.put(&s, TurnIndex(1), Some("first".into()), 1);
        store.put(&session(2), TurnIndex(9), Some("other".into()), 1);
        assert_eq!(store.get(&s, TurnIndex(3)), Some("third"));
        // A turn that needs no headline is done, and shows none.
        store.put(&s, TurnIndex(4), None, 1);
        assert!(store.contains(&s, TurnIndex(4)));
        assert_eq!(store.get(&s, TurnIndex(4)), None);
        assert!(store.contains(&s, TurnIndex(1)));
        assert!(!store.contains(&s, TurnIndex(2)));
        let turns: Vec<u32> = store.recent(&s, 10).iter().map(|h| h.turn.0).collect();
        assert_eq!(turns, [1, 3]);
        let last = store.recent(&s, 1);
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].text, "third");
        assert_eq!(store.recent(&session(3), 5), Vec::new());
    }

    #[test]
    fn a_turn_that_grew_needs_a_new_headline() {
        let mut store = HeadlineStore::default();
        let s = session(1);
        assert!(!store.covers(&s, TurnIndex(2), 0));
        store.put(&s, TurnIndex(2), Some("Started the swarm.".into()), 12);
        assert!(store.covers(&s, TurnIndex(2), 12));
        assert!(store.covers(&s, TurnIndex(2), 3), "a shorter read");
        assert!(!store.covers(&s, TurnIndex(2), 13), "the turn went on");
        assert!(store.contains(&s, TurnIndex(2)));
        store.put(&s, TurnIndex(2), Some("Merged four PRs.".into()), 40);
        assert!(store.covers(&s, TurnIndex(2), 40));
        assert_eq!(store.get(&s, TurnIndex(2)), Some("Merged four PRs."));
    }

    #[test]
    fn a_provisional_headline_waits_for_the_final_one() {
        let mut store = HeadlineStore::default();
        let s = session(1);
        let shown = |store: &HeadlineStore| {
            let recent = store.recent(&s, 5);
            (recent[0].text.clone(), recent[0].provisional)
        };
        assert!(store.put_provisional(&s, TurnIndex(2), "So far: merging.".into(), 8));
        assert_eq!(shown(&store), ("So far: merging.".into(), true));
        assert_eq!(store.get(&s, TurnIndex(2)), None, "never the card headline");
        assert!(
            !store.covers(&s, TurnIndex(2), 0),
            "still needs its final one"
        );
        assert!(!store.contains(&s, TurnIndex(2)));
        // A later provisional one replaces it.
        assert!(store.put_provisional(&s, TurnIndex(2), "So far: merged two.".into(), 12));
        assert_eq!(shown(&store), ("So far: merged two.".into(), true));
        // The final one replaces it, and is not replaced by one made from
        // less of the turn (a job that raced the turn's end).
        store.put(&s, TurnIndex(2), Some("Merged four PRs.".into()), 14);
        assert!(store.covers(&s, TurnIndex(2), 14));
        assert_eq!(shown(&store), ("Merged four PRs.".into(), false));
        assert!(!store.put_provisional(&s, TurnIndex(2), "Stale.".into(), 13));
        assert!(!store.put_provisional(&s, TurnIndex(2), "Stale.".into(), 14));
        assert_eq!(shown(&store), ("Merged four PRs.".into(), false));
        // Once the turn goes on, a provisional one is shown again, but the
        // final one stays: for the card, and should the turn's next final
        // headline never come.
        assert!(store.put_provisional(&s, TurnIndex(2), "Merging more.".into(), 20));
        assert_eq!(shown(&store), ("Merging more.".into(), true));
        assert_eq!(store.get(&s, TurnIndex(2)), Some("Merged four PRs."));
        assert!(!store.covers(&s, TurnIndex(2), 20));
        // A final one made from less of the turn keeps the newer
        // provisional one.
        store.put(&s, TurnIndex(2), Some("Merged five.".into()), 18);
        assert_eq!(shown(&store), ("Merging more.".into(), true));
        assert_eq!(store.get(&s, TurnIndex(2)), Some("Merged five."));
        assert_eq!(store.by_session[&s].len(), 1);
        assert_eq!(store.order.len(), 1);
    }

    #[test]
    fn the_oldest_stored_are_evicted_first() {
        let mut store = HeadlineStore::new(2);
        let s = session(1);
        store.put(&s, TurnIndex(1), Some("a".into()), 1);
        store.put(&session(2), TurnIndex(1), Some("b".into()), 1);
        // Replacing does not count twice.
        store.put(&s, TurnIndex(1), Some("a2".into()), 1);
        store.put(&s, TurnIndex(2), Some("c".into()), 1);
        assert!(!store.contains(&s, TurnIndex(1)));
        assert_eq!(store.get(&s, TurnIndex(2)), Some("c"));
        assert_eq!(store.get(&session(2), TurnIndex(1)), Some("b"));
        store.put(&s, TurnIndex(3), Some("d".into()), 1);
        assert_eq!(
            store.get(&session(2), TurnIndex(1)),
            None,
            "an emptied session goes"
        );
        assert_eq!(store.by_session.len(), 1);
    }
}
