//! The agent's pending input (`GET /v1/agents/:id/pending`), kept current
//! for as long as the agent screen is open, for the option chips.
//!
//! Each read records the agent's listed seq and pending generation from
//! when it *started*, so [`super::choices::gate`] can tell whether what is
//! shown is still current. A read is (re)started whenever the agent changes
//! while it is blocked or asking something, on every `pending_changed`, and
//! on demand ([`PendingFeed::refresh`], after a choice got its answer). A
//! request during a read is queued, never dropped.
//!
//! A read gives up after [`TIMEOUT`] (a phone back from the background may
//! send it down a dead connection), and a failed read is retried every few
//! seconds. Meanwhile the last input read stays shown, so its chips don't
//! vanish when the phone drops the network, but they and the keypad wait
//! for a good read (see [`super::choices::gate`]).

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{AgentId, AgentStatus, Seq};

use super::choices::PendingRead;
use super::screen::Flight;
use crate::api::{ApiClient, TimedFetch};
use crate::live::Live;
use crate::session::SessionToken;
use crate::ui::AppState;

/// How long after a failed read it is retried.
const RETRY: core::time::Duration = core::time::Duration::from_secs(3);

/// How long a read may take, in milliseconds, before it counts as failed.
const TIMEOUT: u32 = 8_000;

/// The pending input of one agent, as last read.
#[derive(Debug, Clone, Copy)]
pub struct PendingFeed {
    /// The last read.
    pub read: RwSignal<PendingRead>,
    /// The agent's current pending generation: changes on every
    /// `pending_changed` (and snapshot) and every [`Self::refresh`].
    pub generation: Memo<u64>,
    local: RwSignal<u64>,
}

impl PendingFeed {
    /// Marks what is shown as possibly outdated and reads it again.
    pub fn refresh(&self) {
        let _ = self.local.try_update(|n| *n = n.wrapping_add(1));
    }
}

/// Keeps a [`PendingFeed`] for `id` current for this component's lifetime.
pub fn pending_feed(
    app: AppState,
    live: Live,
    id: AgentId,
    status: Signal<Option<AgentStatus>>,
    listed_seq: Signal<Option<Seq>>,
) -> PendingFeed {
    let read = RwSignal::new(PendingRead::default());
    let local = RwSignal::new(0_u64);
    let generation = {
        let id = id.clone();
        Memo::new(move |_| live.pending_generation(&id).wrapping_add(local.get()))
    };
    let feed = PendingFeed {
        read,
        generation,
        local,
    };
    let flight = StoredValue::new(Flight::default());
    Effect::new(move |_| {
        let generation = generation.get();
        listed_seq.track();
        let blocked = status.get() == Some(AgentStatus::Blocked);
        // A seq change only matters while something is (or may be) asked.
        let wanted =
            blocked || read.with_untracked(|r| r.pending.is_some() || r.generation != generation);
        if wanted
            && app.paired.get_untracked()
            && flight.try_update_value(Flight::request) == Some(true)
        {
            run(app, id.clone(), feed, listed_seq, flight);
        }
    });
    feed
}

fn run(
    app: AppState,
    id: AgentId,
    feed: PendingFeed,
    listed_seq: Signal<Option<Seq>>,
    flight: StoredValue<Flight>,
) {
    // What the read will be current for, captured before it starts.
    let (Some(seq), Some(generation)) = (
        listed_seq.try_get_untracked(),
        feed.generation.try_get_untracked(),
    ) else {
        return;
    };
    spawn_local(async move {
        // No answer keeps what was shown, waiting (whether it is still
        // asked is unknown); it is retried shortly.
        let api = ApiClient::new(TimedFetch(TIMEOUT), SessionToken(app));
        let result = api.pending(id.clone()).await;
        let failed = result.is_err();
        let pending = match result {
            Ok(pending) => pending,
            Err(_) => feed
                .read
                .try_with_untracked(|r| r.pending.clone())
                .flatten(),
        };
        let next = PendingRead {
            pending,
            seq,
            generation,
            failed,
        };
        if failed {
            set_timeout(
                move || {
                    if feed.read.try_with_untracked(|r| r.failed) == Some(true) {
                        feed.refresh();
                    }
                },
                RETRY,
            );
        }
        if feed.read.try_with_untracked(|r| *r != next) == Some(true) {
            let _ = feed.read.try_set(next);
        }
        if flight.try_update_value(Flight::finish) == Some(true) {
            run(app, id, feed, listed_seq, flight);
        }
    });
}
