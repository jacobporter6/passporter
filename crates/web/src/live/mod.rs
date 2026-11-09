//! Live state from the bridge: the agent list and the threads the user has
//! opened, kept current by `WS /v1/stream`.
//!
//! # Stream protocol
//!
//! 1. Open `ws(s)://<origin>/v1/stream`; the first message the client sends
//!    is [`StreamAuth`] (browsers can't set headers on a WebSocket).
//! 2. The bridge answers with a `snapshot`, then `agent_updated`,
//!    `agent_removed`, `thread_items` and `pending_changed` events.
//!    `pending_changed` only bumps a per-agent generation: the open agent
//!    screen refetches `GET /v1/agents/:id/pending`, which is authoritative.
//!    `tldr_ready` does the same for `GET /v1/agents/:id/tldr`;
//!    `headline_ready` goes into the open thread's model;
//!    `runbook_updated` into the latest runs the Runbooks sheet follows.
//! 3. On close, reconnect with capped exponential backoff and jitter; the
//!    next snapshot resyncs everything. The phone also reconnects straight
//!    away when it comes back online or to the foreground (a socket that
//!    slept in the background is often dead without knowing it).
//!
//! The agent list is cached in `IndexedDB` so triage paints instantly, with
//! "as of HH:MM", while the stream (re)connects.

pub mod agents;
mod stream;

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{Agent, AgentEvent, AgentId, RunbookRun, Timestamp};

use crate::agent::model::{Sync, ThreadModel};
use crate::session;
use crate::store::{AgentsKey, Snapshot};
use crate::ui::{AppState, format};
use agents::ListChange;

pub use stream::{backoff_ms, jitter, stream_url};

/// The stream's connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Conn {
    /// No token, so no stream.
    Off,
    /// Opening the socket, or waiting for the first snapshot.
    Connecting,
    /// Snapshot received; events are flowing.
    Live,
    /// Lost; the next attempt is scheduled.
    Retrying,
}

/// Live, app-wide bridge state, provided as context.
#[derive(Debug, Clone, Copy)]
pub struct Live {
    /// Every agent the bridge reports (or the cached list, while not live).
    pub agents: RwSignal<Vec<Agent>>,
    /// The stream's state.
    pub conn: RwSignal<Conn>,
    /// When `agents` was last known to be current: the last stream message,
    /// or the cached snapshot's fetch time.
    pub fresh_at: RwSignal<Option<Timestamp>>,
    /// Threads of agents the user has opened, synced by stream events.
    pub threads: RwSignal<HashMap<AgentId, ThreadModel>>,
    /// Pending-input generations: one bump per `pending_changed` for the
    /// agent, plus `resyncs`.
    pending_gen: RwSignal<HashMap<AgentId, u64>>,
    /// TL;DR generations: one bump per `tldr_ready` for the agent, plus
    /// `resyncs`.
    tldr_gen: RwSignal<HashMap<AgentId, u64>>,
    /// The latest run of each runbook the stream has reported.
    runbook_runs: RwSignal<Vec<RunbookRun>>,
    /// Bumped by each snapshot: per-agent events may have been missed
    /// while disconnected, so every generation moves on.
    resyncs: RwSignal<u64>,
    save_pending: StoredValue<bool>,
    drafts: StoredValue<HashMap<AgentId, String>>,
    app: StoredValue<AppState>,
}

impl Live {
    /// Empty state for `app`.
    #[must_use]
    pub fn new(app: AppState) -> Self {
        Self {
            agents: RwSignal::new(Vec::new()),
            conn: RwSignal::new(Conn::Off),
            fresh_at: RwSignal::new(None),
            threads: RwSignal::new(HashMap::new()),
            pending_gen: RwSignal::new(HashMap::new()),
            tldr_gen: RwSignal::new(HashMap::new()),
            runbook_runs: RwSignal::new(Vec::new()),
            resyncs: RwSignal::new(0),
            save_pending: StoredValue::new(false),
            drafts: StoredValue::new(HashMap::new()),
            app: StoredValue::new(app),
        }
    }

    /// The agent with `id`, tracked.
    #[must_use]
    pub fn agent(&self, id: &AgentId) -> Option<Agent> {
        self.agents
            .with(|list| list.iter().find(|a| a.id == *id).cloned())
    }

    /// The pending-input generation of `id`, tracked: it changes whenever
    /// the agent's pending input may have changed.
    #[must_use]
    pub fn pending_generation(&self, id: &AgentId) -> u64 {
        let own = self.pending_gen.with(|g| g.get(id).copied().unwrap_or(0));
        own.wrapping_add(self.resyncs.get())
    }

    /// The TL;DR generation of `id`, tracked: it changes whenever a TL;DR
    /// is ready for one of its turns, or may have been while the stream
    /// was down.
    #[must_use]
    pub fn tldr_generation(&self, id: &AgentId) -> u64 {
        let own = self.tldr_gen.with(|g| g.get(id).copied().unwrap_or(0));
        own.wrapping_add(self.resyncs.get())
    }

    /// The latest run of each runbook the stream (or a run request)
    /// reported, tracked.
    #[must_use]
    pub fn runbook_runs(&self) -> Vec<RunbookRun> {
        self.runbook_runs.get()
    }

    /// Records `run` as its runbook's latest, unless an update of a later
    /// or the same run is already known (events can overtake the answer
    /// that started the run).
    pub fn note_runbook_run(&self, run: RunbookRun) {
        self.runbook_runs.update(
            |runs| match runs.iter_mut().find(|r| r.runbook == run.runbook) {
                Some(known) if known.started_at > run.started_at => {}
                Some(known) if known.started_at == run.started_at && known.is_finished() => {}
                Some(known) => *known = run,
                None => runs.push(run),
            },
        );
    }

    /// Bumped by each snapshot, tracked: anything per-run or per-agent may
    /// have been missed while the stream was down.
    #[must_use]
    pub fn resync_generation(&self) -> u64 {
        self.resyncs.get()
    }

    /// The unsent composer text for `id` (kept across swipes).
    #[must_use]
    pub fn draft(&self, id: &AgentId) -> String {
        self.drafts
            .with_value(|d| d.get(id).cloned())
            .unwrap_or_default()
    }

    /// Remembers the composer text for `id`.
    pub fn set_draft(&self, id: &AgentId, text: String) {
        self.drafts.update_value(|d| {
            if text.is_empty() {
                d.remove(id);
            } else {
                d.insert(id.clone(), text);
            }
        });
    }

    /// Applies one stream event.
    fn apply(self, event: AgentEvent) {
        self.fresh_at.set(Some(format::now()));
        if let AgentEvent::ThreadItems {
            agent_id,
            epoch,
            items,
            resume_cursor,
        } = event
        {
            self.threads.update(|threads| {
                if let Some(thread) = threads.get_mut(&agent_id)
                    && thread.apply_event(&epoch, items, resume_cursor) == Sync::Refetch
                {
                    // The thread view refetches when it sees an unloaded model.
                    *thread = ThreadModel::default();
                }
            });
            return;
        }
        if let AgentEvent::HeadlineReady {
            agent_id,
            turn,
            headline,
            provisional,
        } = event
        {
            self.threads.update(|threads| {
                if let Some(thread) = threads.get_mut(&agent_id) {
                    thread.apply_headline(protocol::TurnHeadline {
                        turn,
                        text: headline,
                        provisional,
                    });
                }
            });
            return;
        }
        if let AgentEvent::PendingChanged { agent_id, .. } = &event {
            self.pending_gen.update(|g| {
                let n = g.entry(agent_id.clone()).or_default();
                *n = n.wrapping_add(1);
            });
            return;
        }
        if let AgentEvent::RunbookUpdated { run } = event {
            // An unparseable run is dropped: the sheet refetches the list
            // whenever it opens.
            if let Some(run) = run {
                self.note_runbook_run(run);
            }
            return;
        }
        if let AgentEvent::TldrReady { agent_id, .. } = &event {
            self.tldr_gen.update(|g| {
                let n = g.entry(agent_id.clone()).or_default();
                *n = n.wrapping_add(1);
            });
            return;
        }
        if matches!(event, AgentEvent::Snapshot { .. }) {
            self.conn.set(Conn::Live);
            // Runs reported before the gap may have ended unseen: forget
            // them; the Runbooks sheet refetches the list on resync.
            self.runbook_runs.update(Vec::clear);
            self.resyncs.update(|n| *n = n.wrapping_add(1));
        }
        let mut change = ListChange::None;
        self.agents
            .update(|list| change = agents::apply_event(list, &event));
        match change {
            ListChange::Changed => self.save_soon(),
            ListChange::Refetch => self.refetch(),
            ListChange::None => {}
        }
    }

    /// Replaces the list from `GET /v1/agents` (after an event this app
    /// could not parse).
    fn refetch(self) {
        spawn_local(async move {
            if let Ok(list) = session::api(self.app.get_value()).agents().await {
                self.agents.set(list);
                self.fresh_at.set(Some(format::now()));
                self.save_soon();
            }
        });
    }

    /// Writes the list to the snapshot cache, at most about once a second.
    fn save_soon(self) {
        if self.save_pending.get_value() {
            return;
        }
        self.save_pending.set_value(true);
        set_timeout(
            move || {
                self.save_pending.set_value(false);
                let value = self.agents.get_untracked();
                spawn_local(async move {
                    if let Ok(store) = self.app.get_value().store().await {
                        let snapshot = Snapshot {
                            fetched_at: format::now(),
                            value,
                        };
                        let _ = store.put(&AgentsKey, &snapshot).await;
                    }
                });
            },
            core::time::Duration::from_secs(1),
        );
    }

    /// Paints the cached list while the stream is not yet live.
    async fn load_cached(self) {
        let Ok(store) = self.app.get_value().store().await else {
            return;
        };
        let Ok(Some(snapshot)) = store.get(&AgentsKey).await else {
            return;
        };
        if self.conn.get_untracked() != Conn::Live && self.agents.with_untracked(Vec::is_empty) {
            self.agents.set(snapshot.value);
            self.fresh_at.set(Some(snapshot.fetched_at));
        }
    }
}

/// The [`Live`] state from context.
///
/// # Panics
///
/// Outside [`crate::app::App`], which always provides it.
#[must_use]
pub fn use_live() -> Live {
    expect_context()
}

/// Starts live sync for the app's lifetime: loads the cached list, then
/// keeps the stream connected whenever a token is present.
pub fn start(live: Live) {
    let app = live.app.get_value();
    spawn_local(live.load_cached());
    Effect::new(move |_| {
        if let Some(token) = app.token.get() {
            stream::connect(app, live, token);
        } else {
            stream::disconnect();
            live.conn.set(Conn::Off);
        }
    });
    stream::watch_foreground(app, live);
}
