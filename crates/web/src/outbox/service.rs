//! The outbox in the browser: drives [`Outbox`] and mirrors it to
//! `IndexedDB` (design §6.5).
//!
//! - A decision is written to the store *before* it joins the queue, so it
//!   is never sent before it is durable.
//! - Due entries flush when one is added, when a send settles, on a timer
//!   for the next backoff, and right away when the device comes back
//!   online, the app becomes visible again, or a token arrives. Nothing is
//!   sent (and no timer runs) while offline or unpaired, but decisions are
//!   still accepted and queued then.
//! - A send that hangs is given up after [`SEND_TIMEOUT_MS`] and retried
//!   with the same key; a late answer still counts. While a request is
//!   still out, its entry can't be cancelled (it may yet arrive). The app
//!   can't abort a request the bridge may already have, so this lasts
//!   until the browser gives up on it.
//! - On startup the stored entries are restored, so a reload resumes the
//!   queue.
//! - A sent entry stays in the store only while its tick shows (so other
//!   tabs can see it was delivered), then goes. Stale, refused and failed
//!   entries stay until the user deals with their card.
//!
//! # Several tabs
//!
//! Each tab holds the outbox in memory over the one shared store. Before a
//! send, a tab re-reads the entry: if another tab deleted it (delivered or
//! discarded) or settled it, this tab takes that answer instead of sending.
//! When the app becomes visible it re-reads the whole outbox the same way;
//! entries this tab changed while that read was running keep this tab's
//! version. Another tab's queued entries are left to it unless long
//! overdue (see [`Outbox::sync`]). Two tabs can still race to send the same
//! entry; both send the same key, and the bridge dedupes.

use core::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{Action, ActionKind, AgentId, PendingId, Seq, Timestamp};

use super::machine::{
    Delivery, Entry, Outbox, SEND_TIMEOUT_MS, SENT_KEEP_MS, SendAnywayError, Settled, add_key,
    remove_one,
};
use super::record;
use crate::api::{ActionOutcome, ApiError};
use crate::idgen::random_key;
use crate::live::jitter;
use crate::platform::log_error;
use crate::session;
use crate::store::{OutboxKey, Store};
use crate::ui::AppState;
use crate::ui::format::now;

/// How long a sent tick stays up.
const SENT_TTL: Duration = Duration::from_millis(2200);

/// The app-wide outbox, provided as context by [`crate::app::App`].
#[derive(Debug, Clone, Copy)]
pub struct OutboxService {
    app: AppState,
    state: RwSignal<Outbox>,
    /// Bookkeeping nothing renders (one stored value: each type of signal
    /// or stored value costs WASM of its own).
    book: StoredValue<Book>,
}

/// The service's non-reactive bookkeeping.
#[derive(Debug, Default)]
struct Book {
    /// One key per store write started and not finished: a sync that
    /// starts meanwhile must not trust its read of them.
    writing: Vec<OutboxKey>,
    /// Keys known to be in the store: only these are dropped when the store
    /// no longer has them (another tab handled them). An entry whose write
    /// failed is never mistaken for one.
    stored: Vec<OutboxKey>,
    /// While a sync reads the store: the keys this tab changed meanwhile,
    /// which the sync must leave alone. `None` when no sync runs.
    touched: Option<Vec<OutboxKey>>,
    timer: Option<TimeoutHandle>,
    /// The store has been read once (the startup restore is done).
    restored: bool,
    /// The last sequence number this tab gave out.
    last_seq: u64,
}

/// The outbox from context.
///
/// # Panics
///
/// Outside [`crate::app::App`], which provides it.
#[must_use]
pub fn use_outbox() -> OutboxService {
    expect_context::<OutboxService>()
}

impl OutboxService {
    /// Creates the outbox, restores what the store holds and starts
    /// flushing. Call once, inside the app's owner.
    #[must_use]
    pub fn start(app: AppState) -> Self {
        let service = Self {
            app,
            state: RwSignal::new(Outbox::default()),
            book: StoredValue::new(Book::default()),
        };
        spawn_local(service.sync());
        // Back online or paired again: everything queued is due now. Going
        // offline stops the timer (flush re-arms only when it can send).
        Effect::new(move |_| {
            if app.online.get() && app.paired.get() {
                service.wake();
            } else {
                service.disarm();
            }
        });
        // App-lifetime listener, never removed. `visibilitychange` bubbles
        // from the document to the window.
        let _ = window_event_listener(leptos::ev::visibilitychange, move |_| {
            if !document().hidden() {
                spawn_local(service.sync());
            }
        });
        service
    }

    /// One agent's entries, oldest first, as the reply bar shows them
    /// ([`Entry::shown`]: no retry times), so a memo over this changes only
    /// when a row would (reactive).
    #[must_use]
    pub fn entries(&self, agent: &AgentId) -> Vec<(OutboxKey, Entry)> {
        self.state.with(|o| {
            o.for_agent(agent)
                .map(|(k, e)| (k.clone(), e.shown()))
                .collect()
        })
    }

    /// Whether a key or choice for `agent` waits for an answer (reactive).
    #[must_use]
    pub fn answers_pending(&self, agent: &AgentId) -> bool {
        self.state.with(|o| o.answers_pending(agent))
    }

    /// The dialog last answered by a delivered choice for `agent`, if
    /// recent (reactive).
    #[must_use]
    pub fn answered(&self, agent: &AgentId) -> Option<PendingId> {
        self.state.with(|o| o.answered(agent).cloned())
    }

    /// Records that a choice delivered `at` answered `pending`, until
    /// [`SENT_KEEP_MS`] after that.
    fn remember_answer(&self, agent: AgentId, pending: PendingId, at: Timestamp) {
        let left = now().millis_until(Timestamp::from_unix_millis(
            at.unix_millis().saturating_add(SENT_KEEP_MS),
        ));
        if left == 0 {
            return;
        }
        let answer = (agent, pending, at);
        self.state.update(|o| o.remember(answer.clone()));
        let service = *self;
        set_timeout(
            move || {
                service.changed(|o| o.forget_answer(&answer));
            },
            Duration::from_millis(left),
        );
    }

    /// The agent now shows `shown`: an answered dialog that isn't it is
    /// over, and is forgotten (so a reused id later isn't "answered").
    pub fn dialog_shown(&self, agent: &AgentId, shown: Option<&PendingId>) {
        self.changed(|o| o.dialog_shown(agent, shown));
    }

    /// Applies `change`, and tells subscribers only if it reports one.
    fn changed(&self, change: impl FnOnce(&mut Outbox) -> bool) {
        if self.state.try_update_untracked(change) == Some(true) {
            self.state.notify();
        }
    }

    /// Whether a "send anyway" replacement for the stale `key` is being
    /// stored: the card's actions wait (reactive).
    #[must_use]
    pub fn superseding(&self, key: &OutboxKey) -> bool {
        self.state.with(|o| o.superseding(key))
    }

    /// Whether a request for `key` is still out, so cancelling it would be
    /// a lie: it may yet arrive (reactive).
    #[must_use]
    pub fn in_flight(&self, key: &OutboxKey) -> bool {
        self.state.with(|o| o.in_flight(key))
    }

    /// Accepts a new decision: stored first, then queued and sent. Works
    /// offline and unpaired too; it goes when it can. A repeat of a key or
    /// choice still waiting (a double tap) only makes the queue retry now.
    /// Returns whether `action` was accepted as new. `on_queued` runs once
    /// it is stored and queued (e.g. to clear the composer only then).
    pub fn submit(
        &self,
        action: Action,
        label: Option<String>,
        on_queued: Box<dyn FnOnce()>,
    ) -> bool {
        if self.state.try_update(|o| o.prepare(&action)) != Some(true) {
            self.wake();
            return false;
        }
        let key = OutboxKey::for_action(&action, self.next_seq());
        let service = *self;
        spawn_local(async move {
            match service.app.store().await {
                Ok(store) => {
                    let entry = Entry::queued(action.clone(), label.clone());
                    service.begin_write(&key);
                    service.write(&store, &key, Some(&entry)).await;
                }
                // Keep the decision in memory rather than drop it; it is
                // only lost if the page goes away before it is sent.
                Err(error) => log_error("outbox not durable", &error.to_string().into()),
            }
            // Durable (or as durable as this browser allows): now it may go.
            let chained = service
                .state
                .try_update(|o| o.enqueue(key.clone(), action, label));
            if chained == Some(true) {
                // Chained while it was being stored: write the mark down.
                service.persist(key);
            }
            on_queued();
            service.flush();
        });
        true
    }

    /// "Send anyway" for a stale prompt: a new action with a new key, at the
    /// agent's current seq (or `fallback`, the newest known).
    pub fn send_anyway(&self, key: &OutboxKey, fallback: Seq) -> Result<(), SendAnywayError> {
        let Some(new_key) = random_key() else {
            self.app
                .toast("This browser can't generate secure random ids.");
            return Ok(());
        };
        if self.state.with_untracked(|o| o.superseding(key)) {
            return Ok(()); // A double tap: the replacement is on its way.
        }
        let action = self
            .state
            .try_with_untracked(|o| o.send_anyway(key, new_key, fallback, now()))
            .ok_or(SendAnywayError::Missing)??;
        self.state.update(|o| o.set_superseding(key, true));
        // The stale entry goes only once its replacement is stored, so the
        // text is never only in memory.
        let service = *self;
        let stale = key.clone();
        self.submit(
            action,
            None,
            Box::new(move || {
                service.state.update(|o| o.set_superseding(&stale, false));
                service.discard(&stale);
            }),
        );
        Ok(())
    }

    /// Drops `key` (discarded, dismissed, or cancelled while queued; see
    /// [`Outbox::discard`]). Returns whether it went.
    pub fn discard(&self, key: &OutboxKey) -> bool {
        let gone = self.state.try_update(|o| o.discard(key)) == Some(true);
        if gone {
            self.persist(key.clone());
        }
        gone
    }

    /// Makes everything queued due now and flushes.
    pub fn wake(&self) {
        // Subscribers hear of it only if something changed.
        self.changed(|o| o.wake(now()));
        self.flush();
    }

    /// Whether sending is possible at all right now.
    fn can_send(&self) -> bool {
        self.app.online.get_untracked() && self.app.paired.get_untracked()
    }

    /// Sends whatever may go now, and arms the timer for the next retry.
    fn flush(&self) {
        if !self.can_send() {
            self.disarm();
            return;
        }
        let batch = self
            .state
            .try_update_untracked(|o| o.begin_due(now()))
            .unwrap_or_default();
        if !batch.is_empty() {
            self.state.notify();
        }
        for (key, action, attempt) in batch {
            spawn_local(self.send(key, action, attempt));
        }
        self.arm_timer();
    }

    /// Sends one attempt that [`Outbox::begin_due`] started (and counts as
    /// out until it is back).
    async fn send(self, key: OutboxKey, action: Action, attempt: u32) {
        // A hung request (or a stalled store read) must not hold the
        // agent's queue: after the timeout the attempt counts as unanswered
        // and the entry is retried.
        let timed_out = key.clone();
        let timeout = set_timeout_with_handle(
            move || {
                self.answer(
                    &timed_out,
                    attempt,
                    Err(ApiError::Transport("timed out".into())),
                );
            },
            Duration::from_millis(SEND_TIMEOUT_MS),
        )
        .ok();
        let result = if self.taken_by_another_tab(&key).await {
            None
        } else {
            Some(session::api(self.app).act(&action).await)
        };
        self.state.try_update(|o| o.returned(&key));
        if let Some(timeout) = timeout {
            timeout.clear();
        }
        match result {
            Some(result) => self.answer(&key, attempt, result),
            None => self.flush(),
        }
    }

    /// Applies an answer, mirrors it to the store and flushes.
    fn answer(&self, key: &OutboxKey, attempt: u32, result: Result<ActionOutcome, ApiError>) {
        let settled = self
            .state
            .try_update(|o| o.settle(key, attempt, result, now(), jitter()))
            .flatten();
        match settled {
            Some(Settled::Delivery(Delivery::Sent)) => {
                self.persist(key.clone());
                self.persist_chained(key);
                self.on_sent(key);
            }
            Some(Settled::Delivery(_)) => self.persist(key.clone()),
            Some(Settled::Rebased(to)) => {
                self.persist(key.clone());
                self.persist(to);
            }
            None => {}
        }
        self.flush();
    }

    /// After `key` was delivered, the prompts queued behind it are chained:
    /// write that down, so a reload keeps it.
    fn persist_chained(&self, key: &OutboxKey) {
        let chained: Vec<OutboxKey> = self.state.with_untracked(|o| {
            let Some(agent) = o.get(key).map(|e| e.action.agent_id.clone()) else {
                return Vec::new();
            };
            o.for_agent(&agent)
                .filter(|(_, e)| e.chained && e.delivery.is_unsettled())
                .map(|(k, _)| k.clone())
                .collect()
        });
        for key in chained {
            self.persist(key);
        }
    }

    /// A delivered entry (here or, seen in the store, by another tab):
    /// record an answered dialog, and let the tick linger before it goes.
    /// The stored record stays for [`super::machine::SENT_KEEP_MS`] so other
    /// tabs learn of the delivery; a later sync deletes it.
    fn on_sent(&self, key: &OutboxKey) {
        let choice = self.state.with_untracked(|o| {
            o.get(key).and_then(|e| match &e.action.kind {
                ActionKind::Choose(c) => Some((e.action.agent_id.clone(), c.pending_id.clone())),
                _ => None,
            })
        });
        if let Some((agent, pending)) = choice {
            self.remember_answer(agent, pending, now());
        }
        let service = *self;
        let shown = key.clone();
        set_timeout(
            move || {
                service.changed(|o| o.forget_sent(&shown));
            },
            SENT_TTL,
        );
    }

    /// Whether another tab already delivered, discarded or settled `key`
    /// (then this tab takes its word and does not send).
    async fn taken_by_another_tab(&self, key: &OutboxKey) -> bool {
        if !self.book.with_value(|b| b.stored.contains(key)) {
            return false;
        }
        let Ok(store) = self.app.store().await else {
            return false;
        };
        // The whole outbox is a handful of records; reading it all reuses
        // the sync's read instead of another code path.
        let Some((entries, unreadable)) = load(&store).await else {
            return false;
        };
        if unreadable.contains(key) {
            return false; // not this tab's to judge
        }
        let stored = entries.into_iter().find(|(k, _)| k == key).map(|(_, e)| e);
        if stored.as_ref().is_some_and(|e| e.delivery.is_unsettled()) {
            return false;
        }
        if stored.is_none() {
            self.book.update_value(|b| b.stored.retain(|k| k != key));
        }
        self.adopt(key, stored);
        true
    }

    /// Takes the store's word for `key` (see [`Outbox::adopt`]); a delivery
    /// seen this way is treated like one made here.
    fn adopt(&self, key: &OutboxKey, stored: Option<Entry>) {
        let sent = stored
            .as_ref()
            .is_some_and(|e| e.delivery == Delivery::Sent);
        if self.state.try_update(|o| o.adopt(key, stored)) == Some(true) && sent {
            self.on_sent(key);
        }
    }

    fn disarm(&self) {
        if let Some(handle) = self.book.try_update_value(|b| b.timer.take()).flatten() {
            handle.clear();
        }
    }

    /// Re-arms the retry timer for the earliest head, if sending is
    /// possible. Entries waiting behind one in flight need no timer: its
    /// answer flushes them.
    fn arm_timer(&self) {
        self.disarm();
        if !self.can_send() {
            return;
        }
        let Some(due) = self.state.try_with_untracked(Outbox::next_wakeup).flatten() else {
            return;
        };
        let service = *self;
        let wait = Duration::from_millis(now().millis_until(due));
        if let Ok(handle) = set_timeout_with_handle(move || service.flush(), wait) {
            self.book.update_value(|b| b.timer = Some(handle));
        }
    }

    /// Notes a local change to `key`, so a running sync leaves it alone.
    fn touch(&self, key: &OutboxKey) {
        self.book.update_value(|b| {
            if let Some(t) = &mut b.touched {
                add_key(t, key);
            }
        });
    }

    /// Counts a store write for `key` as started (and touched).
    fn begin_write(&self, key: &OutboxKey) {
        self.touch(key);
        self.book.update_value(|b| b.writing.push(key.clone()));
    }

    /// Mirrors `key`'s entry to the store: written while held, deleted once
    /// gone.
    fn persist(&self, key: OutboxKey) {
        self.begin_write(&key);
        let service = *self;
        let entry = self
            .state
            .try_with_untracked(|o| o.get(&key).map(|e| e.for_store(now())))
            .flatten();
        spawn_local(async move {
            match service.app.store().await {
                Ok(store) => service.write(&store, &key, entry.as_ref()).await,
                Err(error) => {
                    service.end_write(&key);
                    log_error("outbox write failed", &error.to_string().into());
                }
            }
        });
    }

    fn end_write(&self, key: &OutboxKey) {
        self.book.update_value(|b| remove_one(&mut b.writing, key));
    }

    /// Writes (or with `None`, deletes) one record, tracking what is
    /// stored. Callers count it with [`Self::begin_write`] first.
    async fn write(&self, store: &Store, key: &OutboxKey, entry: Option<&Entry>) {
        let result = match entry {
            Some(entry) => store.put(key, &record::encode(entry)).await,
            None => store.delete(key).await,
        };
        self.end_write(key);
        match result {
            Ok(()) => self.book.update_value(|b| {
                if entry.is_some() {
                    add_key(&mut b.stored, key);
                } else {
                    b.stored.retain(|k| k != key);
                }
            }),
            Err(error) => log_error("outbox write failed", &error.to_string().into()),
        }
    }

    /// Reads the store into memory (at startup, and when the app becomes
    /// visible again, to see what other tabs did), then flushes. Keys this
    /// tab changed while the read ran keep this tab's version.
    async fn sync(self) {
        let started = self.book.try_update_value(|b| {
            if b.touched.is_some() {
                return false; // One sync at a time; the running one will do.
            }
            // Writes already under way may land after the read: treat
            // their keys as touched too.
            let mut writing = Vec::new();
            for key in &b.writing {
                add_key(&mut writing, key);
            }
            b.touched = Some(writing);
            true
        });
        if started != Some(true) {
            return;
        }
        let loaded = match self.app.store().await {
            Ok(store) => load(&store).await,
            Err(_) => None,
        };
        let touched = self
            .book
            .try_update_value(|b| b.touched.take())
            .flatten()
            .unwrap_or_default();
        let Some((loaded, unreadable)) = loaded else {
            self.wake();
            return;
        };
        let (known, startup) = self
            .book
            .try_update_value(|b| (b.stored.clone(), !core::mem::replace(&mut b.restored, true)))
            .unwrap_or_default();
        let synced = self
            .state
            .try_update(|o| o.sync(loaded, &unreadable, &known, &touched, startup, now()))
            .unwrap_or_default();
        self.book.update_value(|b| b.stored = synced.stored);
        for key in &synced.delivered {
            self.on_sent(key);
        }
        for (agent, pending, at) in synced.answered {
            self.remember_answer(agent, pending, at);
        }
        for key in synced.rewrite {
            self.persist(key);
        }
        self.wake();
    }

    /// The next entry's sequence number: after every one this tab made or
    /// holds. It only orders entries made in the same millisecond (keys
    /// start with the time); the idempotency key keeps tabs apart.
    fn next_seq(&self) -> u64 {
        let held = self.state.with_untracked(Outbox::max_seq);
        self.book
            .try_update_value(|b| {
                b.last_seq = b.last_seq.max(held).saturating_add(1);
                b.last_seq
            })
            .unwrap_or_else(|| held.saturating_add(1))
    }
}

/// Every readable stored entry plus the keys of unreadable ones (left in
/// place: they hold something the user said), or `None` if the outbox
/// can't be read.
async fn load(store: &Store) -> Option<(Vec<(OutboxKey, Entry)>, Vec<OutboxKey>)> {
    let all = match store.outbox_entries().await {
        Ok(all) => all,
        Err(error) => {
            log_error("outbox unreadable", &error.to_string().into());
            return None;
        }
    };
    let mut entries = Vec::with_capacity(all.len());
    let mut unreadable = Vec::new();
    for (key, stored) in all {
        if let Some(entry) = stored.ok().as_deref().and_then(record::decode) {
            entries.push((key, entry));
        } else {
            log_error("outbox entry unreadable", &key.to_string().into());
            unreadable.push(key);
        }
    }
    Some((entries, unreadable))
}
