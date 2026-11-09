//! The outbox state machine (design §6.5), pure so it is tested on the
//! host. [`super::service`] drives it from the browser and mirrors it to
//! `IndexedDB`.
//!
//! Every user decision becomes an [`Entry`] under its [`OutboxKey`]. Its
//! [`Delivery`] moves like this:
//!
//! ```text
//!            begin                settle
//! Queued ───────────▶ Sending ───────────▶ Sent | Stale | Refused | Failed
//!   ▲                    │
//!   └────────────────────┘ no answer (or a transient refusal): backoff
//! ```
//!
//! The rules that keep it safe:
//!
//! - only a `Queued` entry is ever sent, and [`Outbox::begin`] moves it to
//!   `Sending` in the same step, so overlapping flushes (a timer, `online`
//!   and a visibility change at once) never send it twice;
//! - a retry resends the *same* action: same idempotency key, same
//!   `expectedSeq`; the bridge dedupes, so a lost answer is never a double
//!   send;
//! - an answer is final for its key: `Sent`, `Stale`, `Refused` and `Failed`
//!   are never queued again. "Send anyway" after a stale prompt is a *new*
//!   entry with a new key ([`Outbox::send_anyway`]); choices, keys and
//!   interrupts have no such path, because what they answered is gone;
//! - per agent, entries go out in the order they were made, one at a time;
//!   an interrupt is the exception: it never waits in line;
//! - a prompt queued behind another decision for the same agent carries the
//!   seq the agent had when it was typed, which that earlier decision moves
//!   once it is delivered. Such a prompt is *chained*: if it comes back
//!   stale with the agent's current seq, it is requeued once as a new
//!   action (a key derived from its own, that seq) instead of becoming a
//!   card. The 409 was
//!   caused by the user's own queue, not by the agent moving on. Choices,
//!   keys and interrupts are never chained.

use protocol::{
    Action, ActionKind, ActionResult, AgentId, IdempotencyKey, PendingId, RefusalReason, Seq,
    Timestamp,
};

use crate::api::{ActionOutcome, ApiError};
use crate::idgen::derived_key;
use crate::live::backoff_ms;
use crate::store::OutboxKey;

/// How long a delivered entry's record stays in the store, so other tabs
/// (even ones hidden for a while) learn that it was delivered.
pub const SENT_KEEP_MS: u64 = 10 * 60 * 1_000;

/// How long a send may take before it counts as unanswered.
pub const SEND_TIMEOUT_MS: u64 = 20_000;

/// How long past its due time another tab's queued entry must be before a
/// visible tab takes it on: its own tab would have sent it (and stored the
/// answer) by then, so that tab is probably gone.
const ORPHANED_AFTER_MS: u64 = SEND_TIMEOUT_MS + 10_000;

/// One durable user decision, stored under its [`OutboxKey`] as a
/// [`super::record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The action exactly as it is (re)sent.
    pub action: Action,
    /// The chosen option's label, for choices (status lines).
    pub label: Option<String>,
    /// Where it is.
    pub delivery: Delivery,
    /// An earlier decision for the same agent was delivered after this
    /// prompt was made, so its `expectedSeq` is behind because of the
    /// user's own queue (see the module docs).
    pub chained: bool,
    /// When the bridge's answer came, once settled.
    pub settled_at: Option<Timestamp>,
}

impl Entry {
    /// A new decision, due from when it was made.
    #[must_use]
    pub fn queued(action: Action, label: Option<String>) -> Self {
        let retry_at = action.created_at;
        Self {
            action,
            label,
            delivery: Delivery::Queued {
                attempts: 0,
                retry_at,
            },
            chained: false,
            settled_at: None,
        }
    }

    /// The record to store: an entry in flight is stored as queued, due
    /// from when it went (a reload resends it under the same key, and
    /// other tabs leave it to this one meanwhile, see [`Outbox::sync`]).
    #[must_use]
    pub fn for_store(&self, now: Timestamp) -> Self {
        let mut stored = self.clone();
        if let Delivery::Sending { attempts } = stored.delivery {
            stored.delivery = Delivery::Queued {
                attempts,
                retry_at: now,
            };
        }
        stored
    }

    /// The entry as a status row shows it: retry times are left out and
    /// attempt counts capped (a queued row only shows whether it was
    /// tried, a sending one nothing more), so a retry moving on changes
    /// nothing a row shows.
    #[must_use]
    pub fn shown(&self) -> Self {
        let mut shown = self.clone();
        match &mut shown.delivery {
            Delivery::Queued { attempts, retry_at } => {
                *attempts = (*attempts).min(1);
                *retry_at = Timestamp::from_unix_millis(0);
            }
            Delivery::Sending { attempts } => *attempts = 1,
            _ => {}
        }
        shown
    }

    /// Whether this is a delivery record past [`SENT_KEEP_MS`].
    fn expired(&self, now: Timestamp) -> bool {
        self.delivery == Delivery::Sent
            && self
                .settled_at
                .is_none_or(|at| at.unix_millis().saturating_add(SENT_KEEP_MS) <= now.unix_millis())
    }
}

/// Where an [`Entry`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Waiting to be sent, from `retry_at` on.
    Queued {
        /// Sends so far that got no answer.
        attempts: u32,
        /// Not before this.
        retry_at: Timestamp,
    },
    /// A request is in flight.
    Sending {
        /// Sends so far, this one included.
        attempts: u32,
    },
    /// Handed to the agent.
    Sent,
    /// HTTP 409: the agent moved on before it arrived; nothing was sent.
    Stale {
        /// The agent's seq in the 409, when the bridge sent its state.
        current: Option<Seq>,
    },
    /// The bridge refused it for good; nothing was sent.
    Refused {
        /// Why.
        reason: RefusalReason,
        /// The bridge's text (may be empty).
        message: String,
    },
    /// Final with delivery uncertain, or the device was rejected.
    Failed {
        /// What happened, for people.
        message: String,
    },
}

impl Delivery {
    /// Whether the entry still waits for an answer (queued or in flight).
    #[must_use]
    pub const fn is_unsettled(&self) -> bool {
        matches!(self, Self::Queued { .. } | Self::Sending { .. })
    }
}

/// Why [`Outbox::send_anyway`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendAnywayError {
    /// No such entry (already discarded, or another tab handled it).
    #[error("that message is no longer in the outbox")]
    Missing,
    /// The entry is not stale.
    #[error("only a message the agent moved past can be sent anyway")]
    NotStale,
    /// Choices, keys and interrupts answered something that is gone: the
    /// user looks again instead.
    #[error("only prompts can be sent anyway")]
    NotAPrompt,
}

/// Entries sorted by key (creation order). A sorted `Vec`: there are a
/// handful, and a `BTreeMap` costs far more WASM.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Entries(Vec<(OutboxKey, Entry)>);

impl Entries {
    fn find(&self, key: &OutboxKey) -> Result<usize, usize> {
        self.0.binary_search_by(|(k, _)| k.cmp(key))
    }

    fn contains_key(&self, key: &OutboxKey) -> bool {
        self.find(key).is_ok()
    }

    fn get(&self, key: &OutboxKey) -> Option<&Entry> {
        let at = self.find(key).ok()?;
        self.0.get(at).map(|(_, e)| e)
    }

    fn get_mut(&mut self, key: &OutboxKey) -> Option<&mut Entry> {
        let at = self.find(key).ok()?;
        self.0.get_mut(at).map(|(_, e)| e)
    }

    /// Inserts or replaces.
    fn insert(&mut self, key: OutboxKey, entry: Entry) {
        match self.find(&key) {
            Ok(at) => self.0[at].1 = entry,
            Err(at) => self.0.insert(at, (key, entry)),
        }
    }

    fn remove(&mut self, key: &OutboxKey) -> Option<Entry> {
        let at = self.find(key).ok()?;
        Some(self.0.remove(at).1)
    }

    fn iter(&self) -> impl Iterator<Item = (&OutboxKey, &Entry)> {
        self.0.iter().map(|(k, e)| (k, e))
    }

    fn values(&self) -> impl Iterator<Item = &Entry> {
        self.0.iter().map(|(_, e)| e)
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut Entry> {
        self.0.iter_mut().map(|(_, e)| e)
    }

    /// The entries made after `key`.
    fn after_mut(&mut self, key: &OutboxKey) -> impl Iterator<Item = &mut Entry> {
        let from = self.find(key).map_or_else(|at| at, |at| at + 1);
        self.0[from..].iter_mut().map(|(_, e)| e)
    }
}

/// Adds `key` to a small set kept as a list.
pub fn add_key(list: &mut Vec<OutboxKey>, key: &OutboxKey) {
    if !list.contains(key) {
        list.push(key.clone());
    }
}

/// Every entry this device holds, in creation order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outbox {
    entries: Entries,
    /// Decisions accepted but not yet keyed and stored: they count as
    /// queued for duplicate checks, and can be chained meanwhile.
    preparing: Vec<Preparing>,
    /// One key per request still out (a retry after a timeout can be out
    /// beside the original). While one is, a queued entry can't be
    /// cancelled: it may yet arrive.
    in_flight: Vec<OutboxKey>,
    /// Per agent, the dialog last answered by a delivered choice, wherever
    /// the user was when the answer came, and when. Forgotten once the
    /// agent shows another dialog, or after [`SENT_KEEP_MS`] (the service
    /// sets a timer): a restarted bridge may reuse a dialog id.
    answered: Vec<Answered>,
    /// Stale prompts whose "send anyway" replacement is being stored.
    superseding: Vec<OutboxKey>,
}

/// A dialog answered by a delivered choice: the agent, the dialog, when.
pub type Answered = (AgentId, PendingId, Timestamp);

/// A decision on its way into the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Preparing {
    action: Action,
    /// An earlier decision was delivered while this one was being stored.
    chained: bool,
}

impl Outbox {
    /// Merges what the store held after a reload. An entry that was in
    /// flight when the page went away may or may not have arrived; it is
    /// stored as queued ([`Entry::for_store`]), so it goes again with the
    /// same key (the bridge dedupes). Delivery records are not shown again
    /// (their tick is old news); once past [`SENT_KEEP_MS`] they are
    /// deleted. Entries already in memory win. Returns the keys whose
    /// stored record should be deleted.
    pub fn restore(
        &mut self,
        stored: impl IntoIterator<Item = (OutboxKey, Entry)>,
        now: Timestamp,
    ) -> Vec<OutboxKey> {
        let mut rewrite = Vec::new();
        for (key, mut entry) in stored {
            if self.entries.contains_key(&key) {
                continue;
            }
            match entry.delivery {
                Delivery::Sent => {
                    if entry.expired(now) {
                        rewrite.push(key);
                    }
                    continue;
                }
                // Never stored by this app (see `Entry::for_store`).
                Delivery::Sending { attempts } => {
                    entry.delivery = Delivery::Queued {
                        attempts,
                        retry_at: now,
                    };
                }
                Delivery::Queued { .. }
                | Delivery::Stale { .. }
                | Delivery::Refused { .. }
                | Delivery::Failed { .. } => {}
            }
            self.entries.insert(key, entry);
        }
        rewrite
    }

    /// Brings memory in line with a read of the whole store, which another
    /// tab may have changed (see [`Outbox::adopt`]).
    ///
    /// `known` are the keys this tab knew to be stored; `touched` the keys
    /// this tab changed while the read ran. Those keep this tab's version:
    /// the read may predate an entry just submitted (it would look deleted)
    /// or one just cancelled (it would come back). An entry in flight here
    /// keeps its state too. `unreadable` keys are stored but left alone.
    ///
    /// On `startup` every stored entry is taken on ([`Self::restore`]).
    /// Later, an unsettled entry this tab doesn't hold belongs to another
    /// tab, which sends it and stores the answer: it is taken on only once
    /// it is [`ORPHANED_AFTER_MS`] overdue (that tab is probably gone), so
    /// two live tabs don't both send everything. Nothing unsettled is
    /// written back, so another tab's answer is never overwritten.
    pub fn sync(
        &mut self,
        loaded: Vec<(OutboxKey, Entry)>,
        unreadable: &[OutboxKey],
        known: &[OutboxKey],
        touched: &[OutboxKey],
        startup: bool,
        now: Timestamp,
    ) -> Synced {
        let loaded: Vec<(OutboxKey, Entry)> = loaded
            .into_iter()
            .filter(|(k, _)| !touched.contains(k))
            .collect();
        let mut present = Vec::new();
        for key in loaded.iter().map(|(k, _)| k).chain(unreadable) {
            if !touched.contains(key) {
                add_key(&mut present, key);
            }
        }
        let sending = |o: &Self, k: &OutboxKey| {
            matches!(
                o.entries.get(k).map(|e| &e.delivery),
                Some(Delivery::Sending { .. })
            )
        };
        // Known, untouched, and gone: another tab delivered or discarded it.
        for key in known {
            if !present.contains(key) && !touched.contains(key) && !sending(self, key) {
                self.adopt(key, None);
            }
        }
        let mut delivered = Vec::new();
        let mut answered = Vec::new();
        for (key, entry) in &loaded {
            if entry.delivery.is_unsettled() || sending(self, key) {
                continue;
            }
            if entry.delivery != Delivery::Sent {
                self.adopt(key, Some(entry.clone()));
                continue;
            }
            // A delivery: news (a tick) only for an entry still waiting
            // here. A delivered choice seen for the first time answers its
            // dialog (once: a later sync must not bring back an answer the
            // agent has since moved past).
            let waiting = self
                .entries
                .get(key)
                .is_some_and(|e| e.delivery.is_unsettled());
            if waiting && self.adopt(key, Some(entry.clone())) {
                delivered.push(key.clone());
            } else if let ActionKind::Choose(choice) = &entry.action.kind
                && !known.contains(key)
                && !entry.expired(now)
            {
                answered.push((
                    entry.action.agent_id.clone(),
                    choice.pending_id.clone(),
                    entry.settled_at.unwrap_or(now),
                ));
            }
        }
        // Settled records are fine to take on; a queued one only once overdue.
        let live_elsewhere = |e: &Entry| {
            matches!(e.delivery, Delivery::Queued { retry_at, .. }
                if retry_at.unix_millis().saturating_add(ORPHANED_AFTER_MS) > now.unix_millis())
        };
        let loaded = loaded
            .into_iter()
            .filter(|(_, e)| startup || !live_elsewhere(e));
        let rewrite = self.restore(loaded, now);
        let mut stored = present;
        for key in known {
            if touched.contains(key) {
                add_key(&mut stored, key);
            }
        }
        Synced {
            rewrite,
            delivered,
            answered,
            stored,
        }
    }

    /// Reserves `action` while its key is allocated. `false` if it repeats
    /// a decision that is still waiting for an answer (a double tap, or a
    /// tap on the same option again): nothing new should be sent then.
    pub fn prepare(&mut self, action: &Action) -> bool {
        if self.duplicate(action) {
            return false;
        }
        self.preparing.push(Preparing {
            action: action.clone(),
            chained: false,
        });
        true
    }

    fn take_preparing(&mut self, action: &Action) -> Option<Preparing> {
        let at = self
            .preparing
            .iter()
            .position(|p| p.action.idempotency_key == action.idempotency_key)?;
        Some(self.preparing.remove(at))
    }

    /// Adds a decision, due now. The service writes it to the store first.
    /// Returns whether it was chained while being stored (the stored record
    /// then needs rewriting).
    pub fn enqueue(&mut self, key: OutboxKey, action: Action, label: Option<String>) -> bool {
        let chained = self.take_preparing(&action).is_some_and(|p| p.chained);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.chained |= chained;
        } else {
            let mut entry = Entry::queued(action, label);
            entry.chained = chained;
            self.entries.insert(key, entry);
        }
        chained
    }

    /// The dialog last answered by a delivered choice for `agent`.
    #[must_use]
    pub fn answered(&self, agent: &AgentId) -> Option<&PendingId> {
        self.answered
            .iter()
            .find(|(who, _, _)| who == agent)
            .map(|(_, id, _)| id)
    }

    /// Records `answer`, replacing the agent's previous one.
    pub fn remember(&mut self, answer: Answered) {
        self.answered.retain(|(who, _, _)| *who != answer.0);
        self.answered.push(answer);
    }

    /// Forgets `answer` if it is still the agent's (its time is up).
    /// Returns whether it was.
    pub fn forget_answer(&mut self, answer: &Answered) -> bool {
        let before = self.answered.len();
        self.answered.retain(|a| a != answer);
        self.answered.len() != before
    }

    /// The agent now shows `shown`: an answered dialog that isn't it is
    /// over, and is forgotten (so a reused id later isn't "answered").
    /// Returns whether anything was.
    pub fn dialog_shown(&mut self, agent: &AgentId, shown: Option<&PendingId>) -> bool {
        let before = self.answered.len();
        self.answered
            .retain(|(who, id, _)| who != agent || Some(id) == shown);
        self.answered.len() != before
    }

    /// Whether a "send anyway" replacement for the stale `key` is being
    /// stored.
    #[must_use]
    pub fn superseding(&self, key: &OutboxKey) -> bool {
        self.superseding.contains(key)
    }

    /// Marks (or clears) a "send anyway" replacement for `key` as being
    /// stored.
    pub fn set_superseding(&mut self, key: &OutboxKey, on: bool) {
        if on {
            add_key(&mut self.superseding, key);
        } else {
            self.superseding.retain(|k| k != key);
        }
    }

    /// Whether `action` repeats a decision for the same agent (same kind
    /// and payload) that is still on its way in: a double tap. A key,
    /// choice or interrupt repeats one that is preparing or unsettled. A
    /// prompt repeats only one still being stored: the composer clears
    /// once it is queued, so the same text after that is deliberate.
    #[must_use]
    pub fn duplicate(&self, action: &Action) -> bool {
        let same = |a: &Action| a.agent_id == action.agent_id && a.kind == action.kind;
        if self.preparing.iter().any(|p| same(&p.action)) {
            return true;
        }
        !matches!(action.kind, ActionKind::Prompt(_))
            && self
                .entries
                .values()
                .any(|e| e.delivery.is_unsettled() && same(&e.action))
    }

    /// The highest sequence number in memory (0 if empty).
    #[must_use]
    pub fn max_seq(&self) -> u64 {
        self.entries.iter().map(|(k, _)| k.seq).max().unwrap_or(0)
    }

    /// The entry under `key`.
    #[must_use]
    pub fn get(&self, key: &OutboxKey) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// One agent's entries, oldest first.
    pub fn for_agent<'a>(
        &'a self,
        agent: &'a AgentId,
    ) -> impl Iterator<Item = (&'a OutboxKey, &'a Entry)> + 'a {
        self.entries
            .iter()
            .filter(move |(_, e)| e.action.agent_id == *agent)
    }

    /// Whether a key or choice for `agent` still waits for an answer. The
    /// keypad keys and chips wait meanwhile: a second answer to a screen
    /// that is about to change would only come back stale. Interrupts never
    /// make anything wait, and never wait themselves.
    #[must_use]
    pub fn answers_pending(&self, agent: &AgentId) -> bool {
        let answers = |a: &Action| {
            a.agent_id == *agent && matches!(a.kind, ActionKind::Keys(_) | ActionKind::Choose(_))
        };
        self.preparing.iter().any(|p| answers(&p.action))
            || self
                .entries
                .values()
                .any(|e| e.delivery.is_unsettled() && answers(&e.action))
    }

    /// The queued entries that may go once due: per agent, the first
    /// unsettled entry if it is queued, plus every queued interrupt.
    fn heads(&self) -> impl Iterator<Item = (&OutboxKey, &Entry)> {
        // Agents with an unsettled entry so far (few: a Vec is plenty).
        let mut busy: Vec<&AgentId> = Vec::new();
        self.entries.iter().filter(move |(_, entry)| {
            let agent = &entry.action.agent_id;
            if is_interrupt(&entry.action) {
                // Never waits itself, but what was queued after it does:
                // a redirect must not overtake the interrupt.
                if entry.delivery.is_unsettled() && !busy.contains(&agent) {
                    busy.push(agent);
                }
                return matches!(entry.delivery, Delivery::Queued { .. });
            }
            if !entry.delivery.is_unsettled() {
                return false;
            }
            let head = !busy.contains(&agent);
            if head {
                busy.push(agent);
            }
            head && matches!(entry.delivery, Delivery::Queued { .. })
        })
    }

    /// The next entry to send: the oldest head that is due.
    #[cfg(test)]
    #[must_use]
    pub fn next_due(&self, now: Timestamp) -> Option<&OutboxKey> {
        self.heads().find(|(_, e)| is_due(e, now)).map(|(k, _)| k)
    }

    /// Marks `key` in flight and returns the action to send, unchanged.
    /// `None` unless it is a head and due. ([`Self::begin_due`] is what
    /// the service uses.)
    #[cfg(test)]
    pub fn begin(&mut self, key: &OutboxKey, now: Timestamp) -> Option<Action> {
        let due = self.heads().any(|(k, e)| k == key && is_due(e, now));
        if !due {
            return None;
        }
        self.start(key).map(|(action, _)| action)
    }

    /// Begins every entry that may go now (in one pass: starting a head
    /// never changes which others are heads). Returns what to send, with
    /// each send's attempt number. Each counts as out until
    /// [`Self::returned`].
    pub fn begin_due(&mut self, now: Timestamp) -> Vec<(OutboxKey, Action, u32)> {
        let due: Vec<OutboxKey> = self
            .heads()
            .filter(|(_, e)| is_due(e, now))
            .map(|(k, _)| k.clone())
            .collect();
        due.into_iter()
            .filter_map(|key| {
                let (action, attempt) = self.start(&key)?;
                Some((key, action, attempt))
            })
            .collect()
    }

    /// Queued → Sending; the action and this send's attempt number.
    fn start(&mut self, key: &OutboxKey) -> Option<(Action, u32)> {
        let entry = self.entries.get_mut(key)?;
        let Delivery::Queued { attempts, .. } = entry.delivery else {
            return None;
        };
        let attempt = attempts.saturating_add(1);
        entry.delivery = Delivery::Sending { attempts: attempt };
        self.in_flight.push(key.clone());
        Some((entry.action.clone(), attempt))
    }

    /// A request for `key` came back (answered or not), or was never sent.
    pub fn returned(&mut self, key: &OutboxKey) {
        remove_one(&mut self.in_flight, key);
    }

    /// Whether a request for `key` is still out.
    #[must_use]
    pub fn in_flight(&self, key: &OutboxKey) -> bool {
        self.in_flight.contains(key)
    }

    /// Records the answer to send number `attempt` of `key`.
    ///
    /// A definitive answer is the bridge's word on the *key*, so it settles
    /// the entry whenever it is still unsettled, even if it arrives after
    /// that attempt timed out. "No answer" only counts for the attempt in
    /// flight. Anything for an entry that is gone or settled is ignored.
    /// `jitter` (0..=255) spreads retries so tabs and devices that come
    /// back together don't retry in lockstep.
    pub fn settle(
        &mut self,
        key: &OutboxKey,
        attempt: u32,
        result: Result<ActionOutcome, ApiError>,
        now: Timestamp,
        jitter: u8,
    ) -> Option<Settled> {
        let entry = self.entries.get_mut(key)?;
        if !entry.delivery.is_unsettled() {
            return None;
        }
        let delivery = match classify(result) {
            Answer::NoAnswer => {
                if entry.delivery != (Delivery::Sending { attempts: attempt }) {
                    return None;
                }
                entry.delivery = Delivery::Queued {
                    attempts: attempt,
                    retry_at: Timestamp::from_unix_millis(
                        now.unix_millis()
                            .saturating_add(u64::from(retry_delay_ms(attempt, jitter))),
                    ),
                };
                return Some(Settled::Delivery(entry.delivery.clone()));
            }
            Answer::Final(delivery) => delivery,
        };
        let chained = entry.chained;
        if let Delivery::Stale { current: Some(seq) } = delivery
            && chained
            && let Some(rebased) = self.rebase(key, seq, now)
        {
            return Some(Settled::Rebased(rebased));
        }
        let entry = self.entries.get_mut(key)?;
        entry.delivery = delivery.clone();
        entry.settled_at = Some(now);
        if delivery == Delivery::Sent {
            let agent = entry.action.agent_id.clone();
            let prompt =
                |a: &Action| a.agent_id == agent && matches!(a.kind, ActionKind::Prompt(_));
            for next in self.entries.after_mut(key) {
                if next.delivery.is_unsettled() && prompt(&next.action) {
                    next.chained = true;
                }
            }
            // Prompts still being stored were made after it too.
            for next in &mut self.preparing {
                if prompt(&next.action) {
                    next.chained = true;
                }
            }
        }
        Some(Settled::Delivery(delivery))
    }

    /// Requeues the chained prompt `key` as a new action at `seq`, under a
    /// new key that keeps its place in line. Once: the new entry is not
    /// chained. The new idempotency key is derived from the old one, so
    /// tabs that rebase the same entry send the same key (the bridge
    /// dedupes).
    fn rebase(&mut self, key: &OutboxKey, seq: Seq, now: Timestamp) -> Option<OutboxKey> {
        let old = self.entries.remove(key)?;
        let new_key = derived_key(&old.action.idempotency_key);
        let to = OutboxKey {
            idempotency_key: new_key.clone(),
            ..key.clone()
        };
        self.entries.insert(
            to.clone(),
            Entry {
                action: Action {
                    idempotency_key: new_key,
                    expected_seq: seq,
                    ..old.action
                },
                label: old.label,
                delivery: Delivery::Queued {
                    attempts: 0,
                    retry_at: now,
                },
                chained: false,
                settled_at: None,
            },
        );
        Some(to)
    }

    /// Takes the store's word for `key` when another tab got there first:
    /// gone from the store means that tab delivered or discarded it, and a
    /// settled record there is that tab's answer. An unsettled record
    /// changes nothing. Returns whether the entry changed here.
    pub fn adopt(&mut self, key: &OutboxKey, stored: Option<Entry>) -> bool {
        match stored {
            None => self.entries.remove(key).is_some(),
            Some(stored) if !stored.delivery.is_unsettled() => match self.entries.get_mut(key) {
                Some(entry) if entry.delivery != stored.delivery => {
                    *entry = stored;
                    true
                }
                Some(_) => false,
                None => {
                    self.entries.insert(key.clone(), stored);
                    true
                }
            },
            Some(_) => false,
        }
    }

    /// Makes every queued entry due now (back online, app visible again,
    /// or the user asked). Returns whether anything changed.
    pub fn wake(&mut self, now: Timestamp) -> bool {
        let mut changed = false;
        for entry in self.entries.values_mut() {
            if let Delivery::Queued { retry_at, .. } = &mut entry.delivery
                && *retry_at > now
            {
                *retry_at = now;
                changed = true;
            }
        }
        changed
    }

    /// When the earliest head falls due, if any. Entries waiting behind an
    /// unsettled one are not counted: the answer to that one flushes them.
    #[must_use]
    pub fn next_wakeup(&self) -> Option<Timestamp> {
        self.heads()
            .filter_map(|(_, e)| match e.delivery {
                Delivery::Queued { retry_at, .. } => Some(retry_at),
                _ => None,
            })
            .min()
    }

    /// Removes `key` (the user discarded, dismissed or cancelled it).
    /// Settled entries always go. An unsettled one stays while a request
    /// for it is out: it may yet arrive, and its answer is on the way.
    pub fn discard(&mut self, key: &OutboxKey) -> bool {
        let unsettled = self
            .entries
            .get(key)
            .is_some_and(|e| e.delivery.is_unsettled());
        if unsettled && self.in_flight(key) {
            return false;
        }
        self.entries.remove(key).is_some()
    }

    /// Removes `key` if it is still `Sent` (the tick has been seen).
    /// Returns whether it did.
    pub fn forget_sent(&mut self, key: &OutboxKey) -> bool {
        let sent = matches!(
            self.entries.get(key).map(|e| &e.delivery),
            Some(Delivery::Sent)
        );
        if sent {
            self.entries.remove(key);
        }
        sent
    }

    /// "Send anyway" for a stale prompt: the same text as a *new* action,
    /// with a new idempotency key and the agent's current seq (from the 409,
    /// else `fallback`). The caller enqueues it like any new decision and
    /// discards the stale entry only once the new one is stored, so the
    /// text is never only in memory. Never for choices, keys or interrupts.
    pub fn send_anyway(
        &self,
        key: &OutboxKey,
        new_key: IdempotencyKey,
        fallback: Seq,
        now: Timestamp,
    ) -> Result<Action, SendAnywayError> {
        let entry = self.entries.get(key).ok_or(SendAnywayError::Missing)?;
        let Delivery::Stale { current } = entry.delivery else {
            return Err(SendAnywayError::NotStale);
        };
        if !matches!(entry.action.kind, ActionKind::Prompt(_)) {
            return Err(SendAnywayError::NotAPrompt);
        }
        let action = Action {
            idempotency_key: new_key,
            expected_seq: current.unwrap_or(fallback),
            created_at: now,
            ..entry.action.clone()
        };
        Ok(action)
    }
}

/// What [`Outbox::sync`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Synced {
    /// Keys whose stored record should be rewritten (or deleted).
    pub rewrite: Vec<OutboxKey>,
    /// Keys another tab delivered while they waited here, now `Sent` here
    /// too (show their tick).
    pub delivered: Vec<OutboxKey>,
    /// Dialogs answered by choices delivered elsewhere, oldest first, with
    /// when.
    pub answered: Vec<Answered>,
    /// The keys now known to be stored.
    pub stored: Vec<OutboxKey>,
}

/// What [`Outbox::settle`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// The entry is now here.
    Delivery(Delivery),
    /// A chained prompt came back stale and was requeued under this new
    /// key (the old key is gone).
    Rebased(OutboxKey),
}

/// Removes one occurrence of `key` from a list used as a counter (a few
/// keys at most; a map would cost more code than it saves).
pub fn remove_one(list: &mut Vec<OutboxKey>, key: &OutboxKey) {
    if let Some(at) = list.iter().position(|k| k == key) {
        list.swap_remove(at);
    }
}

/// The wait after send number `attempt` (1-based) got no answer: the
/// stream's reconnect backoff, jitter included.
fn retry_delay_ms(attempt: u32, jitter: u8) -> u32 {
    backoff_ms(attempt.saturating_sub(1), jitter)
}

fn is_due(entry: &Entry, now: Timestamp) -> bool {
    matches!(entry.delivery, Delivery::Queued { retry_at, .. } if retry_at <= now)
}

fn is_interrupt(action: &Action) -> bool {
    matches!(action.kind, ActionKind::Interrupt(_))
}

/// What a send's result means for its key.
enum Answer {
    /// Final for the key: never sent again.
    Final(Delivery),
    /// Nothing definitive came back: retry the same action after backoff.
    NoAnswer,
}

/// The outbox contract of [`protocol::result`], applied to one result.
fn classify(result: Result<ActionOutcome, ApiError>) -> Answer {
    match result {
        Ok(ActionOutcome::Result(ActionResult::Sent { .. })) => Answer::Final(Delivery::Sent),
        Ok(ActionOutcome::Result(ActionResult::Stale { current, .. })) => {
            Answer::Final(Delivery::Stale {
                current: current.map(|agent| agent.seq),
            })
        }
        // The app has no step-up flow yet, so a step-up refusal is final
        // here even though the bridge would accept the key again.
        Ok(ActionOutcome::Result(ActionResult::Refused { reason, .. }))
            if reason.is_transient() && reason != RefusalReason::StepUpRequired =>
        {
            Answer::NoAnswer
        }
        Ok(ActionOutcome::Result(ActionResult::Refused { reason, message })) => {
            Answer::Final(Delivery::Refused { reason, message })
        }
        Ok(ActionOutcome::Result(ActionResult::Failed { message })) => {
            Answer::Final(Delivery::Failed {
                message: if message.trim().is_empty() {
                    "herdr reported an error; it may or may not have arrived. Check the screen."
                        .to_owned()
                } else {
                    format!("{message}. It may or may not have arrived; check the screen.")
                },
            })
        }
        Ok(ActionOutcome::Result(ActionResult::Unknown) | ActionOutcome::Unreadable { .. }) => {
            Answer::Final(Delivery::Failed {
                message:
                    "The bridge answered, but not in a way this app understands. Check the screen."
                        .to_owned(),
            })
        }
        // Usually an expired token: the session signs in again and the
        // outbox waits until it is paired, then retries the same key.
        Err(ApiError::Unauthorized) => Answer::NoAnswer,
        Err(error) if error.is_retryable() => Answer::NoAnswer,
        Err(error) => Answer::Final(Delivery::Failed {
            message: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use protocol::{AllowedKey, FeedbackText, OptionId, PendingId, PromptText};

    use super::*;
    use crate::agent::reply::{Draft, build_action};
    use crate::store::StoreKey;

    const K1: &str = "9ca09978-ef89-46fd-9a85-118b84bee7b4";
    const K2: &str = "0ca09978-ef89-46fd-9a85-118b84bee7b4";
    const K3: &str = "1ca09978-ef89-46fd-9a85-118b84bee7b4";
    const K4: &str = "2ca09978-ef89-46fd-9a85-118b84bee7b4";

    fn at(millis: u64) -> Timestamp {
        Timestamp::from_unix_millis(millis)
    }

    fn idem(k: &str) -> IdempotencyKey {
        IdempotencyKey::new(k).unwrap()
    }

    fn prompt(text: &str) -> Draft {
        Draft::Prompt(PromptText::new(text).unwrap())
    }

    fn choice(option: &str) -> Draft {
        Draft::Choose {
            pending: PendingId::new("perm-7").unwrap(),
            option: OptionId::new(option).unwrap(),
            label: format!("Label {option}"),
            feedback: None::<FeedbackText>,
        }
    }

    fn action_for(agent: &str, draft: &Draft, k: &str, created: u64) -> Action {
        build_action(
            agent.parse().unwrap(),
            Seq(3),
            draft,
            None,
            idem(k),
            at(created),
        )
    }

    /// Enqueues `draft` for agent `w1:p1` as entry number `seq`.
    fn add(outbox: &mut Outbox, draft: &Draft, k: &str, seq: u64) -> OutboxKey {
        add_for(outbox, "w1:p1", draft, k, seq)
    }

    fn add_for(outbox: &mut Outbox, agent: &str, draft: &Draft, k: &str, seq: u64) -> OutboxKey {
        let action = action_for(agent, draft, k, 10 * seq);
        assert!(outbox.prepare(&action));
        let key = OutboxKey::for_action(&action, seq);
        outbox.enqueue(key.clone(), action, None);
        key
    }

    #[allow(clippy::unnecessary_wraps)] // mirrors `ApiClient::act`
    fn answer(r: ActionResult) -> Result<ActionOutcome, ApiError> {
        Ok(ActionOutcome::Result(r))
    }

    fn sent() -> Result<ActionOutcome, ApiError> {
        answer(ActionResult::Sent { replayed: false })
    }

    fn stale(seq: Option<u64>) -> Result<ActionOutcome, ApiError> {
        let current = seq.map(|seq| {
            let json = format!(
                r#"{{"id":"w1:p1","title":"t","repo":"r","status":"idle","seq":{seq},
                    "statusSince":0,"kind":"claude"}}"#
            );
            Box::new(serde_json::from_str::<protocol::Agent>(&json).unwrap())
        });
        answer(ActionResult::Stale {
            current,
            pending: None,
        })
    }

    fn offline() -> Result<ActionOutcome, ApiError> {
        Err(ApiError::Transport("offline".into()))
    }

    /// The answer to the attempt in flight (or to the last one), as the
    /// delivery it leads to, once its request is back (as the service
    /// does it). A rebase reads as `None`.
    fn settle(
        outbox: &mut Outbox,
        key: &OutboxKey,
        result: Result<ActionOutcome, ApiError>,
        now: Timestamp,
    ) -> Option<Delivery> {
        let attempt = match outbox.get(key).map(|e| &e.delivery) {
            Some(Delivery::Sending { attempts } | Delivery::Queued { attempts, .. }) => *attempts,
            _ => 0,
        };
        outbox.returned(key);
        match outbox.settle(key, attempt, result, now, 0)? {
            Settled::Delivery(delivery) => Some(delivery),
            Settled::Rebased(_) => None,
        }
    }

    #[test]
    fn a_decision_in_flight_is_never_sent_twice() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        assert_eq!(outbox.next_due(at(10)), Some(&key));
        let first = outbox.begin(&key, at(10)).unwrap();
        // Overlapping flushes (timer + online + visible) find nothing.
        assert_eq!(outbox.begin(&key, at(10)), None);
        assert_eq!(outbox.begin_due(at(10)), []);
        assert_eq!(outbox.next_due(at(10)), None);
        outbox.wake(at(10));
        assert_eq!(outbox.begin(&key, at(10)), None);
        assert_eq!(
            outbox.get(&key).unwrap().delivery,
            Delivery::Sending { attempts: 1 }
        );
        assert_eq!(first.idempotency_key, idem(K1));
    }

    #[test]
    fn no_answer_retries_the_same_action_after_backoff() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        let first = outbox.begin(&key, at(10)).unwrap();
        assert_eq!(
            settle(&mut outbox, &key, offline(), at(100)),
            Some(Delivery::Queued {
                attempts: 1,
                retry_at: at(1_100)
            })
        );
        assert_eq!(outbox.next_due(at(1_099)), None);
        assert_eq!(outbox.begin(&key, at(1_099)), None);
        assert_eq!(outbox.next_wakeup(), Some(at(1_100)));
        let second = outbox.begin(&key, at(1_100)).unwrap();
        // Same key, same seq, same everything: the bridge dedupes.
        assert_eq!(second, first);
        assert_eq!(
            settle(
                &mut outbox,
                &key,
                Err(ApiError::Status { status: 503 }),
                at(2_000)
            ),
            Some(Delivery::Queued {
                attempts: 2,
                retry_at: at(4_000)
            })
        );
        // Back online: due now, still the same action.
        outbox.wake(at(2_500));
        assert_eq!(outbox.begin(&key, at(2_500)), Some(first.clone()));
        assert_eq!(
            settle(&mut outbox, &key, sent(), at(2_600)),
            Some(Delivery::Sent)
        );
        // Final: nothing brings it back.
        outbox.wake(at(9_000));
        assert_eq!(outbox.begin(&key, at(9_000)), None);
        assert_eq!(settle(&mut outbox, &key, offline(), at(9_000)), None);
        assert_eq!(outbox.get(&key).unwrap().delivery, Delivery::Sent);
        assert!(outbox.forget_sent(&key));
        assert!(!outbox.forget_sent(&key));
        assert_eq!(outbox.get(&key), None);
    }

    #[test]
    fn retries_back_off_with_jitter() {
        assert_eq!(retry_delay_ms(1, 0), 1_000);
        assert_eq!(retry_delay_ms(2, 0), 2_000);
        assert_eq!(retry_delay_ms(99, 0), 30_000);
        assert!(retry_delay_ms(3, 255) > retry_delay_ms(3, 0));
    }

    #[test]
    fn a_stale_prompt_is_sent_anyway_only_as_a_new_action() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        let first = outbox.begin(&key, at(10)).unwrap();
        assert_eq!(
            settle(&mut outbox, &key, stale(Some(42)), at(20)),
            Some(Delivery::Stale {
                current: Some(Seq(42))
            })
        );
        // A 409 is final for the key.
        outbox.wake(at(30));
        assert_eq!(outbox.next_due(at(30)), None);
        assert_eq!(outbox.begin(&key, at(30)), None);
        let again = outbox.send_anyway(&key, idem(K2), Seq(7), at(40)).unwrap();
        assert_ne!(again.idempotency_key, first.idempotency_key);
        assert_eq!(again.expected_seq, Seq(42));
        assert_eq!(again.created_at, at(40));
        assert_eq!(again.kind, first.kind);
        // The stale entry stays until its replacement is stored.
        assert!(outbox.get(&key).is_some());
        assert!(outbox.discard(&key));
        assert_eq!(
            outbox.send_anyway(&key, idem(K3), Seq(7), at(50)),
            Err(SendAnywayError::Missing)
        );
    }

    #[test]
    fn send_anyway_falls_back_to_the_newest_known_seq() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        outbox.begin(&key, at(10)).unwrap();
        settle(&mut outbox, &key, stale(None), at(20));
        let again = outbox.send_anyway(&key, idem(K2), Seq(7), at(40)).unwrap();
        assert_eq!(again.expected_seq, Seq(7));
    }

    #[test]
    fn choices_are_never_resent_after_a_409() {
        for draft in [
            choice("allow_once"),
            Draft::key(AllowedKey::Enter),
            Draft::Interrupt,
        ] {
            let mut outbox = Outbox::default();
            let key = add(&mut outbox, &draft, K1, 1);
            outbox.begin(&key, at(10)).unwrap();
            assert_eq!(
                settle(&mut outbox, &key, stale(Some(9)), at(20)),
                Some(Delivery::Stale {
                    current: Some(Seq(9))
                })
            );
            // Not by a retry, a wake-up, a late duplicate answer, or "send
            // anyway".
            outbox.wake(at(1_000_000));
            assert_eq!(outbox.begin_due(at(1_000_000)), []);
            assert_eq!(settle(&mut outbox, &key, offline(), at(30)), None);
            assert_eq!(
                outbox.send_anyway(&key, idem(K2), Seq(9), at(40)),
                Err(SendAnywayError::NotAPrompt)
            );
            assert!(matches!(
                outbox.get(&key).unwrap().delivery,
                Delivery::Stale { .. }
            ));
            // It survives a reload as a stale card, still never queued.
            let stored = round_trip(&outbox);
            let mut reloaded = Outbox::default();
            assert_eq!(reloaded.restore(stored, at(50)), []);
            assert_eq!(reloaded.begin_due(at(50)), []);
            // The same tap again is a new decision, not a resend.
            assert!(!reloaded.duplicate(&action_for("w1:p1", &draft, K3, 60)));
            // Dismissing the card is the only way out.
            assert!(reloaded.discard(&key));
            assert_eq!(reloaded.get(&key), None);
        }
    }

    #[test]
    fn send_anyway_needs_a_stale_entry() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        assert_eq!(
            outbox.send_anyway(&key, idem(K2), Seq(1), at(1)),
            Err(SendAnywayError::NotStale)
        );
    }

    #[test]
    fn each_agent_sends_in_order_one_at_a_time() {
        let mut outbox = Outbox::default();
        let a1 = add(&mut outbox, &prompt("one"), K1, 1);
        let a2 = add(&mut outbox, &prompt("two"), K2, 2);
        let b1 = add_for(&mut outbox, "w2:p1", &prompt("other"), K3, 3);
        let batch: Vec<OutboxKey> = outbox
            .begin_due(at(100))
            .into_iter()
            .map(|(k, _, _)| k)
            .collect();
        assert_eq!(batch, [a1.clone(), b1]);
        // The second waits for the first, even when asked directly.
        assert_eq!(outbox.begin(&a2, at(100)), None);
        settle(&mut outbox, &a1, offline(), at(100));
        // Still waiting: the first is queued again, ahead of it.
        assert_eq!(outbox.begin(&a2, at(5_000)), None);
        assert_eq!(outbox.next_due(at(5_000)), Some(&a1));
        outbox.begin(&a1, at(5_000)).unwrap();
        settle(&mut outbox, &a1, stale(None), at(5_001));
        // A settled head no longer holds the line.
        assert_eq!(outbox.next_due(at(5_001)), Some(&a2));
    }

    #[test]
    fn repeated_taps_are_not_new_decisions() {
        let mut outbox = Outbox::default();
        let draft = choice("allow_once");
        let first = action_for("w1:p1", &draft, K1, 1);
        assert!(outbox.prepare(&first));
        // A double tap before the key is even allocated.
        assert!(!outbox.prepare(&action_for("w1:p1", &draft, K2, 2)));
        assert!(outbox.answers_pending(&"w1:p1".parse().unwrap()));
        let key = OutboxKey::for_action(&first, 1);
        outbox.enqueue(key.clone(), first, Some("Allow".into()));
        assert!(outbox.duplicate(&action_for("w1:p1", &draft, K2, 2)));
        // Different option, different agent: new decisions.
        assert!(!outbox.duplicate(&action_for("w1:p1", &choice("deny"), K2, 2)));
        assert!(!outbox.duplicate(&action_for("w2:p1", &draft, K2, 2)));
        outbox.begin(&key, at(5)).unwrap();
        assert!(outbox.duplicate(&action_for("w1:p1", &draft, K2, 2)));
        settle(&mut outbox, &key, sent(), at(6));
        assert!(!outbox.duplicate(&action_for("w1:p1", &draft, K2, 2)));
        assert!(!outbox.answers_pending(&"w1:p1".parse().unwrap()));
    }

    #[test]
    fn prompts_do_not_hold_the_keypad() {
        let mut outbox = Outbox::default();
        let agent: AgentId = "w1:p1".parse().unwrap();
        add(&mut outbox, &prompt("go"), K1, 1);
        assert!(!outbox.answers_pending(&agent));
        let action = action_for("w1:p1", &Draft::key(AllowedKey::Esc), K2, 2);
        assert!(outbox.prepare(&action));
        assert!(outbox.answers_pending(&agent));
        outbox.enqueue(OutboxKey::for_action(&action, 2), action.clone(), None);
        let key = OutboxKey::for_action(&action, 2);
        assert!(outbox.answers_pending(&agent));
        assert!(outbox.discard(&key));
        assert!(!outbox.answers_pending(&agent));
    }

    #[test]
    fn an_entry_in_flight_cannot_be_discarded() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        outbox.begin(&key, at(10)).unwrap();
        assert!(!outbox.discard(&key));
        settle(&mut outbox, &key, offline(), at(10));
        assert!(outbox.discard(&key));
        // An answer for a discarded entry changes nothing.
        assert_eq!(settle(&mut outbox, &key, sent(), at(11)), None);
    }

    /// What `IndexedDB` would hand back: each durable entry through its
    /// stored JSON form.
    fn round_trip(outbox: &Outbox) -> Vec<(OutboxKey, Entry)> {
        outbox
            .entries
            .iter()
            .map(|(key, entry)| {
                let back = crate::outbox::record::decode(&crate::outbox::record::encode(entry));
                let key = OutboxKey::decode(&key.encode()).unwrap();
                (key, back.unwrap())
            })
            .collect()
    }

    #[test]
    fn reload_resumes_queued_items_without_double_sending() {
        let mut before = Outbox::default();
        let queued = add(&mut before, &prompt("queued"), K1, 1);
        let flying = add_for(&mut before, "w2:p1", &prompt("in flight"), K2, 2);
        let done = add_for(&mut before, "w3:p1", &prompt("done"), K3, 3);
        let stale_choice = add_for(&mut before, "w4:p1", &choice("deny"), K4, 4);
        // `queued` got no answer; `flying` was in flight when the page went.
        before.begin(&queued, at(100)).unwrap();
        settle(&mut before, &queued, offline(), at(100));
        let flying_action = before.begin(&flying, at(100)).unwrap();
        before.begin(&done, at(100)).unwrap();
        settle(&mut before, &done, sent(), at(100));
        before.begin(&stale_choice, at(100)).unwrap();
        settle(&mut before, &stale_choice, stale(None), at(100));

        let mut after = Outbox::default();
        let rewrite = after.restore(round_trip(&before), at(500));
        // The fresh delivery record stays (other tabs read it); it isn't
        // shown again here. Nothing unsettled is written back.
        assert_eq!(rewrite, []);
        assert_eq!(after.get(&done), None);
        assert_eq!(
            after.get(&flying).unwrap().delivery,
            Delivery::Queued {
                attempts: 1,
                retry_at: at(500)
            }
        );
        // Backoff survives the reload; the maybe-delivered one goes now,
        // with its original key.
        let batch = after.begin_due(at(500));
        assert_eq!(batch, [(flying.clone(), flying_action, 2)]);
        assert!(
            after
                .begin_due(at(1_100))
                .iter()
                .any(|(k, _, _)| *k == queued)
        );
        assert!(matches!(
            after.get(&stale_choice).unwrap().delivery,
            Delivery::Stale { .. }
        ));
        // Restoring again (another load) never resets what is in flight;
        // the delivery record is deleted once it is old.
        assert_eq!(after.restore(round_trip(&before), at(600)), []);
        let expired = at(100 + SENT_KEEP_MS);
        assert_eq!(after.restore(round_trip(&before), expired), [done]);
        assert_eq!(
            after.get(&flying).unwrap().delivery,
            Delivery::Sending { attempts: 2 }
        );
    }

    #[test]
    fn answers_follow_the_outbox_contract() {
        let refused = |reason| {
            answer(ActionResult::Refused {
                reason,
                message: String::new(),
            })
        };
        let retried = [
            offline(),
            // An expired token: retried once the session signs in again.
            Err(ApiError::Unauthorized),
            Err(ApiError::Status { status: 503 }),
            refused(RefusalReason::RateLimited),
        ];
        for result in retried {
            let mut outbox = Outbox::default();
            let key = add(&mut outbox, &prompt("go"), K1, 1);
            outbox.begin(&key, at(10)).unwrap();
            let delivery = settle(&mut outbox, &key, result.clone(), at(10));
            assert!(
                matches!(delivery, Some(Delivery::Queued { attempts: 1, .. })),
                "{result:?}: {delivery:?}"
            );
        }
        let last = [
            refused(RefusalReason::StepUpRequired),
            refused(RefusalReason::NoPendingInput),
            answer(ActionResult::Failed {
                message: String::new(),
            }),
            answer(ActionResult::Unknown),
            Ok(ActionOutcome::Unreadable { status: 200 }),
            Err(ApiError::Status { status: 400 }),
        ];
        for result in last {
            let mut outbox = Outbox::default();
            let key = add(&mut outbox, &prompt("go"), K1, 1);
            outbox.begin(&key, at(10)).unwrap();
            let delivery = settle(&mut outbox, &key, result.clone(), at(10));
            assert!(
                matches!(
                    delivery,
                    Some(Delivery::Refused { .. } | Delivery::Failed { .. })
                ),
                "{result:?}: {delivery:?}"
            );
            outbox.wake(at(1_000_000));
            assert!(outbox.begin_due(at(1_000_000)).is_empty(), "{result:?}");
        }
    }

    #[test]
    fn a_late_answer_still_counts_but_a_late_silence_does_not() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        outbox.begin(&key, at(10)).unwrap();
        // Attempt 1 timed out: queued again.
        let timed_out = outbox.settle(&key, 1, offline(), at(20), 0);
        assert!(matches!(
            timed_out,
            Some(Settled::Delivery(Delivery::Queued { attempts: 1, .. }))
        ));
        outbox.begin(&key, at(5_000)).unwrap();
        // Attempt 1's transport error arrives late: attempt 2 is untouched.
        assert_eq!(outbox.settle(&key, 1, offline(), at(5_001), 0), None);
        assert_eq!(
            outbox.get(&key).unwrap().delivery,
            Delivery::Sending { attempts: 2 }
        );
        // Attempt 1's real answer arrives late: it is the key's answer.
        assert_eq!(
            outbox.settle(&key, 1, sent(), at(5_002), 0),
            Some(Settled::Delivery(Delivery::Sent))
        );
        // And attempt 2's answer then changes nothing.
        assert_eq!(outbox.settle(&key, 2, stale(None), at(5_003), 0), None);
        assert_eq!(outbox.get(&key).unwrap().delivery, Delivery::Sent);
    }

    #[test]
    fn a_timed_out_entry_takes_a_late_answer_while_queued() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &choice("deny"), K1, 1);
        outbox.begin(&key, at(10)).unwrap();
        outbox.settle(&key, 1, offline(), at(20), 0);
        assert_eq!(
            outbox.settle(&key, 1, stale(None), at(30), 0),
            Some(Settled::Delivery(Delivery::Stale { current: None }))
        );
        assert_eq!(outbox.begin_due(at(1_000_000)), []);
    }

    #[test]
    fn a_prompt_queued_behind_a_delivered_one_is_rebased_once() {
        let mut outbox = Outbox::default();
        let first = add(&mut outbox, &prompt("one"), K1, 1);
        let second = add(&mut outbox, &prompt("two"), K2, 2);
        let typed_at = outbox.get(&second).unwrap().action.clone();
        outbox.begin(&first, at(30)).unwrap();
        settle(&mut outbox, &first, sent(), at(31));
        assert!(outbox.get(&second).unwrap().chained);
        outbox.begin(&second, at(32)).unwrap();
        // The first prompt moved the seq: 409 with the agent's state.
        let rebased = outbox.settle(&second, 1, stale(Some(50)), at(33), 0);
        let Some(Settled::Rebased(to)) = rebased else {
            panic!("not rebased: {rebased:?}")
        };
        assert_eq!(outbox.get(&second), None);
        assert_eq!((to.created_at, to.seq), (second.created_at, second.seq));
        let entry = outbox.get(&to).unwrap().clone();
        assert!(!entry.chained);
        assert_eq!(entry.action.idempotency_key, derived_key(&idem(K2)));
        assert_eq!(entry.action.expected_seq, Seq(50));
        assert_eq!(entry.action.kind, typed_at.kind);
        assert_eq!(entry.action.created_at, typed_at.created_at);
        // Due now, as a fresh decision.
        assert_eq!(outbox.next_due(at(33)), Some(&to));
        outbox.begin(&to, at(33)).unwrap();
        // Stale again: the agent really moved on. A card, not a loop.
        assert_eq!(
            outbox.settle(&to, 1, stale(Some(51)), at(34), 0),
            Some(Settled::Delivery(Delivery::Stale {
                current: Some(Seq(51))
            }))
        );
    }

    #[test]
    fn only_prompts_are_chained_and_only_with_the_agents_seq() {
        let mut outbox = Outbox::default();
        let first = add(&mut outbox, &prompt("one"), K1, 1);
        let picked = add(&mut outbox, &choice("deny"), K2, 2);
        let blind = add(&mut outbox, &prompt("two"), K3, 3);
        outbox.begin(&first, at(40)).unwrap();
        settle(&mut outbox, &first, sent(), at(41));
        assert!(!outbox.get(&picked).unwrap().chained);
        outbox.begin(&picked, at(42)).unwrap();
        // A choice behind a delivered prompt: still never resent.
        assert_eq!(
            outbox.settle(&picked, 1, stale(Some(50)), at(43), 0),
            Some(Settled::Delivery(Delivery::Stale {
                current: Some(Seq(50))
            }))
        );
        // A chained prompt whose 409 has no agent state becomes a card.
        outbox.begin(&blind, at(44)).unwrap();
        assert_eq!(
            outbox.settle(&blind, 1, stale(None), at(45), 0),
            Some(Settled::Delivery(Delivery::Stale { current: None }))
        );
    }

    #[test]
    fn an_interrupt_never_waits_in_line() {
        let mut outbox = Outbox::default();
        let agent: AgentId = "w1:p1".parse().unwrap();
        let key = add(&mut outbox, &Draft::key(AllowedKey::Enter), K1, 1);
        outbox.begin(&key, at(10)).unwrap();
        settle(&mut outbox, &key, offline(), at(10));
        // The key waits for its backoff; the interrupt goes anyway, and
        // never holds the keypad.
        let stop = add(&mut outbox, &Draft::Interrupt, K2, 2);
        assert_eq!(outbox.next_due(at(20)), Some(&stop));
        assert!(outbox.begin(&stop, at(20)).is_some());
        assert!(outbox.answers_pending(&agent));
        settle(&mut outbox, &key, sent(), at(21));
        assert!(!outbox.answers_pending(&agent));
    }

    #[test]
    fn nothing_waiting_in_line_needs_a_timer() {
        let mut outbox = Outbox::default();
        let first = add(&mut outbox, &prompt("one"), K1, 1);
        add(&mut outbox, &prompt("two"), K2, 2);
        outbox.begin(&first, at(30)).unwrap();
        // The second is overdue but waits for the first's answer: no
        // wake-up (which would spin).
        assert_eq!(outbox.next_wakeup(), None);
        settle(&mut outbox, &first, offline(), at(30));
        assert_eq!(outbox.next_wakeup(), Some(at(1_030)));
    }

    #[test]
    fn the_same_prompt_twice_is_two_messages() {
        let mut outbox = Outbox::default();
        add(&mut outbox, &prompt("yes"), K1, 1);
        assert!(!outbox.duplicate(&action_for("w1:p1", &prompt("yes"), K2, 20)));
        add(&mut outbox, &prompt("yes"), K2, 2);
        assert_eq!(outbox.for_agent(&"w1:p1".parse().unwrap()).count(), 2);
    }

    #[test]
    fn another_tabs_answer_wins() {
        let mut outbox = Outbox::default();
        let gone = add(&mut outbox, &prompt("one"), K1, 1);
        let settled = add_for(&mut outbox, "w2:p1", &prompt("two"), K2, 2);
        let still = add_for(&mut outbox, "w3:p1", &prompt("three"), K3, 3);
        // Another tab delivered `gone` and got a 409 for `settled`.
        assert!(outbox.adopt(&gone, None));
        let mut theirs = outbox.get(&settled).unwrap().clone();
        theirs.delivery = Delivery::Stale { current: None };
        assert!(outbox.adopt(&settled, Some(theirs.clone())));
        assert!(!outbox.adopt(&settled, Some(theirs)));
        // An unsettled record there changes nothing here.
        let queued = outbox.get(&still).unwrap().clone();
        assert!(!outbox.adopt(&still, Some(queued)));
        assert_eq!(outbox.get(&gone), None);
        assert!(outbox.get(&settled).is_some() && outbox.get(&still).is_some());
        assert_eq!(
            outbox
                .begin_due(at(100))
                .into_iter()
                .map(|(k, _, _)| k)
                .collect::<Vec<_>>(),
            [still]
        );
    }

    #[test]
    fn a_sync_never_drops_an_entry_submitted_while_it_read() {
        let mut outbox = Outbox::default();
        let old = add(&mut outbox, &prompt("old"), K1, 1);
        // The read started, then the user submitted `new` (written and
        // known to be stored); the read doesn't have it.
        let read = round_trip(&outbox);
        let new = add(&mut outbox, &prompt("new"), K2, 2);
        let known = vec![old.clone(), new.clone()];
        let touched = vec![new.clone()];
        let synced = outbox.sync(read, &[], &known, &touched, false, at(100));
        assert!(outbox.get(&new).is_some());
        assert_eq!(synced.stored, known);
    }

    #[test]
    fn a_sync_never_brings_back_an_entry_cancelled_while_it_read() {
        let mut outbox = Outbox::default();
        let cancelled = add(&mut outbox, &prompt("never mind"), K1, 1);
        let read = round_trip(&outbox);
        assert!(outbox.discard(&cancelled));
        let known = Vec::new(); // its delete already went through
        let touched = vec![cancelled.clone()];
        let synced = outbox.sync(read, &[], &known, &touched, false, at(100));
        assert_eq!(outbox.get(&cancelled), None);
        assert_eq!(outbox.begin_due(at(100)), []);
        assert_eq!(synced.stored, []);
    }

    #[test]
    fn a_sync_takes_other_tabs_deliveries_and_deletions() {
        let mut outbox = Outbox::default();
        let deleted = add(&mut outbox, &prompt("one"), K1, 1);
        let delivered = add_for(&mut outbox, "w2:p1", &choice("deny"), K2, 2);
        let flying = add_for(&mut outbox, "w3:p1", &prompt("three"), K3, 3);
        outbox.begin(&flying, at(30)).unwrap();
        let mut read = round_trip(&outbox);
        // The other tab deleted `deleted` and delivered `delivered`, and
        // its record of `flying` is gone too.
        read.retain(|(k, _)| *k == delivered);
        read[0].1.delivery = Delivery::Sent;
        let known = vec![deleted.clone(), delivered.clone(), flying.clone()];
        let synced = outbox.sync(read, &[], &known, &Vec::new(), false, at(100));
        assert_eq!(outbox.get(&deleted), None);
        assert_eq!(outbox.get(&delivered).unwrap().delivery, Delivery::Sent);
        assert_eq!(synced.delivered, std::slice::from_ref(&delivered));
        // What is in flight here stays: its answer is on the way.
        assert_eq!(
            outbox.get(&flying).unwrap().delivery,
            Delivery::Sending { attempts: 1 }
        );
        assert_eq!(synced.stored, vec![delivered]);
    }

    #[test]
    fn a_sync_leaves_unreadable_records_alone() {
        let mut outbox = Outbox::default();
        let odd = add(&mut outbox, &prompt("odd"), K1, 1);
        let known = vec![odd.clone()];
        let synced = outbox.sync(
            Vec::new(),
            std::slice::from_ref(&odd),
            &known,
            &Vec::new(),
            false,
            at(1),
        );
        assert!(outbox.get(&odd).is_some());
        assert_eq!(synced.stored, known);
    }

    #[test]
    fn the_chained_mark_survives_a_reload() {
        let mut before = Outbox::default();
        let first = add(&mut before, &prompt("one"), K1, 1);
        let second = add(&mut before, &prompt("two"), K2, 2);
        assert!(!round_trip(&before)[1].1.chained);
        before.begin(&first, at(30)).unwrap();
        settle(&mut before, &first, sent(), at(31));
        let mut after = Outbox::default();
        after.restore(round_trip(&before), at(40));
        assert!(after.get(&second).unwrap().chained);
        after.begin(&second, at(40)).unwrap();
        let rebased = after.settle(&second, 1, stale(Some(9)), at(41), 0);
        assert!(matches!(rebased, Some(Settled::Rebased(_))), "{rebased:?}");
    }

    #[test]
    fn a_prompt_stored_while_its_predecessor_is_delivered_is_chained() {
        let mut outbox = Outbox::default();
        let first = add(&mut outbox, &prompt("one"), K1, 1);
        // `second` is accepted but still being stored when `first` lands.
        let second = action_for("w1:p1", &prompt("two"), K2, 20);
        assert!(outbox.prepare(&second));
        outbox.begin(&first, at(30)).unwrap();
        settle(&mut outbox, &first, sent(), at(31));
        let key = OutboxKey::for_action(&second, 2);
        assert!(outbox.enqueue(key.clone(), second, None));
        assert!(outbox.get(&key).unwrap().chained);
        // Not chained when nothing was delivered meanwhile.
        let third = action_for("w1:p1", &prompt("three"), K3, 40);
        assert!(outbox.prepare(&third));
        assert!(!outbox.enqueue(OutboxKey::for_action(&third, 3), third, None));
    }

    #[test]
    fn begin_due_reports_each_attempt() {
        let mut outbox = Outbox::default();
        let a = add(&mut outbox, &prompt("one"), K1, 1);
        let b = add_for(&mut outbox, "w2:p1", &prompt("two"), K2, 2);
        outbox.begin(&a, at(10)).unwrap();
        settle(&mut outbox, &a, offline(), at(10));
        outbox.wake(at(20));
        let batch: Vec<(OutboxKey, u32)> = outbox
            .begin_due(at(20))
            .into_iter()
            .map(|(k, _, n)| (k, n))
            .collect();
        assert_eq!(batch, [(a, 2), (b, 1)]);
    }

    #[test]
    fn deliveries_seen_late_answer_dialogs_without_a_tick_once() {
        let mut outbox = Outbox::default();
        let mine = add(&mut outbox, &choice("deny"), K1, 1);
        // Another tab made and delivered `other`; this tab never held it.
        let mut elsewhere = Outbox::default();
        let other = add_for(&mut elsewhere, "w2:p1", &choice("allow"), K2, 2);
        elsewhere.begin(&other, at(30)).unwrap();
        settle(&mut elsewhere, &other, sent(), at(31));
        let mut read = round_trip(&outbox);
        read.extend(round_trip(&elsewhere));
        // That tab also delivered `mine` long after this tab last looked.
        read[0].1.delivery = Delivery::Sent;
        read[0].1.settled_at = Some(at(40));
        let known = vec![mine.clone()];
        let none = Vec::new();
        let synced = outbox.sync(read.clone(), &[], &known, &none, false, at(50));
        // `mine` was waiting here: a tick. `other` is news to this tab: no
        // tick, but its dialog counts as answered.
        assert_eq!(synced.delivered, std::slice::from_ref(&mine));
        assert_eq!(outbox.get(&other), None);
        assert_eq!(
            synced.answered,
            [(
                "w2:p1".parse::<AgentId>().unwrap(),
                PendingId::new("perm-7").unwrap(),
                at(31)
            )]
        );
        assert_eq!(synced.rewrite, []);
        // The next sync reads the same record: old news, so a dialog the
        // agent has moved past (and maybe reused the id of) stays open.
        let again = outbox.sync(read, &[], &synced.stored, &none, false, at(60));
        assert_eq!(again.answered, []);
        assert_eq!(again.delivered, []);
    }

    #[test]
    fn answered_dialogs_are_remembered_and_forgotten() {
        let mut outbox = Outbox::default();
        let agent: AgentId = "w1:p1".parse().unwrap();
        let (a, b) = (
            PendingId::new("perm-7").unwrap(),
            PendingId::new("perm-8").unwrap(),
        );
        let first = (agent.clone(), a.clone(), at(1));
        outbox.remember(first.clone());
        assert_eq!(outbox.answered(&agent), Some(&a));
        // Still shown: kept. Another dialog shown: over.
        assert!(!outbox.dialog_shown(&agent, Some(&a)));
        assert!(outbox.dialog_shown(&agent, Some(&b)));
        assert_eq!(outbox.answered(&agent), None);
        // A newer answer replaces the agent's last; an old timer then
        // forgets nothing.
        outbox.remember(first.clone());
        outbox.remember((agent.clone(), b.clone(), at(2)));
        assert!(!outbox.forget_answer(&first));
        assert_eq!(outbox.answered(&agent), Some(&b));
        assert!(outbox.forget_answer(&(agent.clone(), b, at(2))));
        assert_eq!(outbox.answered(&agent), None);
    }

    #[test]
    fn superseding_is_marked_per_key() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        assert!(!outbox.superseding(&key));
        outbox.set_superseding(&key, true);
        outbox.set_superseding(&key, true);
        assert!(outbox.superseding(&key));
        outbox.set_superseding(&key, false);
        assert!(!outbox.superseding(&key));
    }

    #[test]
    fn rows_ignore_retry_times() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        let fresh = outbox.get(&key).unwrap().shown();
        outbox.begin(&key, at(10)).unwrap();
        let sending = outbox.get(&key).unwrap().shown();
        settle(&mut outbox, &key, offline(), at(20));
        let retried = outbox.get(&key).unwrap().shown();
        outbox.begin(&key, at(5_000)).unwrap();
        assert_eq!(outbox.get(&key).unwrap().shown(), sending);
        settle(&mut outbox, &key, offline(), at(5_001));
        // A later retry, at another time, shows the same.
        assert_eq!(outbox.get(&key).unwrap().shown(), retried);
        assert_ne!(retried, fresh);
    }

    #[test]
    fn a_settled_entry_can_be_dismissed_while_a_request_is_out() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        outbox.begin(&key, at(10)).unwrap();
        // Attempt 1 timed out but its request is still out: no cancel.
        outbox.settle(&key, 1, offline(), at(20), 0);
        assert!(outbox.in_flight(&key));
        assert!(!outbox.discard(&key));
        // Its late answer is a 409: the card can be dealt with at once,
        // even though the request is still counted as out.
        outbox.settle(&key, 1, stale(Some(5)), at(30), 0);
        assert!(outbox.in_flight(&key));
        assert!(outbox.discard(&key));
        assert!(!outbox.discard(&key));
        outbox.returned(&key);
        assert!(!outbox.in_flight(&key));
    }

    #[test]
    fn tabs_rebase_the_same_prompt_to_the_same_key() {
        let mut tabs = [Outbox::default(), Outbox::default()];
        let mut rebased = Vec::new();
        for outbox in &mut tabs {
            let first = add(outbox, &prompt("one"), K1, 1);
            let second = add(outbox, &prompt("two"), K2, 2);
            outbox.begin(&first, at(30)).unwrap();
            settle(outbox, &first, sent(), at(31));
            outbox.begin(&second, at(32)).unwrap();
            let Some(Settled::Rebased(to)) = outbox.settle(&second, 1, stale(Some(50)), at(33), 0)
            else {
                panic!("not rebased");
            };
            rebased.push(outbox.get(&to).unwrap().action.idempotency_key.clone());
        }
        assert_eq!(rebased[0], rebased[1]);
        assert_ne!(rebased[0], idem(K2));
    }

    #[test]
    fn a_later_sync_leaves_another_live_tabs_queue_alone() {
        // Another tab queued `theirs` (due at 10) and is sending `flying`
        // (since 30).
        let mut other = Outbox::default();
        let theirs = add(&mut other, &prompt("theirs"), K1, 1);
        let flying = add_for(&mut other, "w2:p1", &prompt("flying"), K2, 2);
        other.begin(&flying, at(30)).unwrap();
        let read: Vec<(OutboxKey, Entry)> = other
            .entries
            .iter()
            .map(|(k, e)| (k.clone(), e.for_store(at(30))))
            .collect();
        let none = Vec::new();
        // While that tab is live, a visible tab neither takes them on nor
        // writes anything back.
        let mut outbox = Outbox::default();
        let synced = outbox.sync(read.clone(), &[], &none, &none, false, at(1_000));
        assert_eq!(outbox.get(&theirs), None);
        assert_eq!(outbox.get(&flying), None);
        assert_eq!(synced.rewrite, []);
        assert_eq!(synced.stored, vec![theirs.clone(), flying.clone()]);
        // Long overdue: that tab is gone; this one takes them on.
        let late = at(30 + ORPHANED_AFTER_MS);
        let synced = outbox.sync(read.clone(), &[], &none, &none, false, late);
        assert!(outbox.get(&theirs).is_some() && outbox.get(&flying).is_some());
        assert_eq!(synced.rewrite, []);
        // At startup everything stored is taken on at once.
        let mut fresh = Outbox::default();
        fresh.sync(read, &[], &none, &none, true, at(1_000));
        assert_eq!(fresh.begin_due(at(1_000)).len(), 2);
    }

    #[test]
    fn an_entry_in_flight_is_stored_as_queued() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        outbox.begin(&key, at(10)).unwrap();
        let stored = outbox.get(&key).unwrap().for_store(at(12));
        assert_eq!(
            stored.delivery,
            Delivery::Queued {
                attempts: 1,
                retry_at: at(12)
            }
        );
        let queued = add(&mut outbox, &prompt("next"), K2, 2);
        let entry = outbox.get(&queued).unwrap();
        assert_eq!(&entry.for_store(at(99)), entry);
    }

    #[test]
    fn a_prompt_still_being_stored_is_not_sent_twice() {
        let mut outbox = Outbox::default();
        let first = action_for("w1:p1", &prompt("go"), K1, 10);
        assert!(outbox.prepare(&first));
        // A second tap on Send while it is stored: the same message.
        assert!(!outbox.prepare(&action_for("w1:p1", &prompt("go"), K2, 11)));
        assert!(outbox.prepare(&action_for("w2:p1", &prompt("go"), K3, 11)));
        outbox.enqueue(OutboxKey::for_action(&first, 1), first, None);
        // Queued (the composer cleared): the same text again is new.
        assert!(outbox.prepare(&action_for("w1:p1", &prompt("go"), K4, 12)));
    }

    #[test]
    fn what_was_queued_after_an_interrupt_waits_for_it() {
        let mut outbox = Outbox::default();
        let stop = add(&mut outbox, &Draft::Interrupt, K1, 1);
        let redirect = add(&mut outbox, &prompt("do this instead"), K2, 2);
        let batch: Vec<OutboxKey> = outbox
            .begin_due(at(100))
            .into_iter()
            .map(|(k, _, _)| k)
            .collect();
        assert_eq!(batch, std::slice::from_ref(&stop));
        assert_eq!(outbox.begin(&redirect, at(100)), None);
        settle(&mut outbox, &stop, sent(), at(101));
        assert_eq!(outbox.next_due(at(101)), Some(&redirect));
    }

    #[test]
    fn wake_reports_whether_anything_changed() {
        let mut outbox = Outbox::default();
        let key = add(&mut outbox, &prompt("go"), K1, 1);
        assert!(!outbox.wake(at(100)));
        outbox.begin(&key, at(100)).unwrap();
        settle(&mut outbox, &key, offline(), at(100));
        assert!(outbox.wake(at(200)));
        assert!(!outbox.wake(at(200)));
    }
}
