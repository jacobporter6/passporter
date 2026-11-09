//! TL;DRs of long turns (`GET /v1/agents/:id/tldr`, `tldr_ready` events)
//! and headlines of every turn (`headline_ready` events; design §6.4).
//!
//! [`Tldrs`] serves the TL;DR of an agent's latest finished turn (or a
//! given one) from a cache keyed per (session, turn), via
//! [`summarize::TldrService`]. A miss for a long turn queues a *job* and
//! is answered "generating"; the job's result is cached and announced as
//! [`AgentEvent::TldrReady`].
//!
//! - **One job per turn.** Jobs are keyed by (session, turn) and share a
//!   single queue slot; a job re-reads its turn when it gets the slot, so
//!   it summarizes the turn as it is then.
//! - **Settling.** A turn that just finished may not be fully written to
//!   the transcript yet. Its number is (the prompt that started it is), so
//!   the watcher records the turn at once and its job waits
//!   [`TldrSettings::settle`], then until two reads that far apart agree,
//!   before summarizing it; requests for that turn meanwhile are answered
//!   "generating".
//! - **Failures back off.** A failed or timed-out generation is remembered
//!   per turn: until its backoff expires the turn is answered "none" and
//!   nothing is retried, so a failing model is not re-run on every poll.
//!   The backoff doubles per failure. A usage limit is account-wide: it
//!   pauses every generation for the longest backoff. When the backoff
//!   expires the bridge retries the session's latest turn by itself (a
//!   few times), so its TL;DR still arrives as `tldr_ready`.
//! - **Any turn on request.** `?turn=` asks for one turn's TL;DR
//!   ([`Tldrs::request`]): an earlier long turn, but also a short one or
//!   the turn in progress, which get one regardless of length. Only one
//!   such job is queued at a time (others are "busy"), so they neither
//!   pile up model runs nor hold up the eager ones by more than one
//!   generation, and each device may start only so many
//!   ([`TldrSettings::request_rate`]). A TL;DR of the turn in progress is
//!   provisional, keyed by the turn's size like any other: it is served
//!   for [`TldrSettings::provisional_for`] however the turn grows (so
//!   reads prompted by its own `tldr_ready` do not start another), then a
//!   request makes a fresh one.
//! - **Eager.** [`Tldrs::spawn_watcher`] follows the agent list and queues
//!   a job when a Claude agent goes from working (or blocked) to idle or
//!   done, so the TL;DR is there by the time the user looks.
//! - **Headlines.** Every job also makes the turn's one-sentence headline
//!   (a cheap call on a smaller digest), long turn or short: after the
//!   TL;DR of a long turn, in a queue slot of its own, so a suggestion
//!   waiting for the queue goes in between. Either call failing does not
//!   stop the other; the job's retry makes only what is still missing.
//!   Headlines are kept per (session, turn) and announced as
//!   [`AgentEvent::HeadlineReady`]; the agent list and the stream show the
//!   headline of the session's latest finished turn as the card headline
//!   ([`Tldrs::add_headline`]; none while that turn has none, rather than
//!   an older turn's), and thread pages carry the session's recent ones
//!   ([`Tldrs::page_headlines`]). A request for the latest turn queues a
//!   headline-only job for a short turn that has no headline yet.
//! - **Backfill.** An agent first seen already idle or done (every one
//!   at startup, and any that arrives finished) finished its turn where
//!   the watcher could not see it, so the watcher queues it for a
//!   *backfilled* headline: its latest finished turn is recorded and gets
//!   a headline-only job (no TL;DR; those stay on request), unless it has
//!   a headline already. Fresh work keeps priority: one agent is
//!   backfilled at a time, only while no other job is queued, so a turn
//!   that finishes live waits for at most one backfilled headline, and
//!   nothing is backfilled during a usage limit. Under steady live or
//!   on-demand work the backfill may wait indefinitely: fresh work first,
//!   by design. A backfilled turn seen finishing live meanwhile, or whose
//!   TL;DR is asked for, becomes that turn's eager job (rerun after the
//!   backfill job if that had already decided what to make).
//! - **Rolling headlines.** The turn in progress gets a provisional
//!   headline from the turn so far, made by the status-line job with its
//!   throttle and growth threshold (see [`status`]) and kept in the same
//!   store beside the turn's final headline: thread pages serve it and
//!   it is announced like a final one, marked provisional, but the card
//!   headline stays the final one, and it never counts as the turn's
//!   headline, so the finished turn's job replaces it with a final one.
//!
//! [`Suggestions`] (reply suggestions, design §6.2) shares the summarizer,
//! the queue and the usage-limit pause, and the watcher starts them too;
//! see [`suggest`]. So do [`StatusLines`] (live status lines of working
//! agents), which the watcher ticks; see [`status`].
//!
//! The input is the turn's thread items only. `git diff --stat` is left out
//! on purpose: running git in an agent's working directory would execute
//! whatever that repository's config asks for (hooks, `core.fsmonitor`),
//! and the repository is as untrusted as the agent's output (design §8).

mod headlines;
mod pace;
pub mod status;
mod suggest;
mod summarizer;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use protocol::{
    Agent, AgentEvent, AgentId, AgentStatus, SessionId, ThreadItem, Tldr, TurnHeadline, TurnIndex,
};
use summarize::{
    DigestLimits, MemoryCache, Summarizer, TldrService, TurnDigest, TurnKey, TurnStats,
};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use self::headlines::{HeadlineStore, MAX_PAGE_HEADLINES, MAX_RECENT_HEADLINES, SharedHeadlines};
use self::pace::{Pacing, UsageLimit, lock, settled};
use self::status::StatusDeps;
pub use self::status::{StatusLines, StatusRefresh, StatusSettings};
use self::suggest::SuggestDeps;
pub use self::suggest::{Suggestions, SuggestionsState};
pub use self::summarizer::{DynSummarizer, SharedSummarizer, SummaryFuture};
use crate::actions::Budget;
use crate::auth::{RateLimit, RateLimiter};
use crate::control::{AgentControl, Resolved, resolve};
use crate::registry::Hub;
use crate::shutdown::Shutdown;
use crate::threads::{ThreadSource, Threads, TurnQuery, TurnSlice, has_transcript};

/// Extra time the backstop timeout allows past the summarizer's own, so
/// the summarizer's timeout (which kills and reaps `claude -p`) fires
/// first.
pub const KILL_GRACE: Duration = Duration::from_secs(10);

/// Default backstop on one generation, once it has its queue slot.
pub const DEFAULT_TIMEOUT: Duration = summarize::DEFAULT_TIMEOUT.saturating_add(KILL_GRACE);

/// Default pause between an agent going idle and reading its turn.
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(2);

/// Default wait before retrying a turn whose generation failed.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_mins(1);

/// Default longest wait before a retry, and the pause after a usage limit.
pub const DEFAULT_MAX_RETRY_AFTER: Duration = Duration::from_mins(30);

/// Most failed turns remembered; the soonest-expiring are forgotten first.
const MAX_FAILURES: usize = 256;

/// Failures after which the bridge stops retrying a turn on its own (a
/// request after the backoff still retries it).
const MAX_ATTEMPTS: u32 = 4;

/// Most queued jobs for turns before the latest (asked for by number):
/// they are on demand only, and must not hold up the eager ones.
const MAX_OLDER_JOBS: usize = 1;

/// Most sessions whose latest finished turn is remembered.
const MAX_SESSIONS: usize = 1024;

/// Most agents waiting for a backfilled headline; agents seen beyond it
/// get theirs when they next finish a turn.
const MAX_BACKFILL: usize = 256;

/// Default time a provisional TL;DR of a turn in progress is served for
/// that turn, however it grows.
pub const DEFAULT_PROVISIONAL_FOR: Duration = Duration::from_secs(30);

/// Most turns whose latest provisional TL;DR is remembered.
const MAX_PROVISIONAL: usize = 64;

/// Default TL;DR requests that start a generation, per device.
pub const DEFAULT_REQUEST_RATE: RateLimit = RateLimit::per_minute(10);

/// How TL;DRs are made.
#[derive(Debug, Clone)]
pub struct TldrSettings {
    /// The model; `None` disables TL;DRs (the endpoint answers `null`) and
    /// reply suggestions.
    pub summarizer: Option<SharedSummarizer>,
    /// Backstop on one generation, not counting time spent queued. Set it
    /// above the summarizer's own timeout (see [`KILL_GRACE`]).
    pub timeout: Duration,
    /// Pause after a turn finishes before it is read and summarized.
    pub settle: Duration,
    /// Wait before retrying a failed turn; doubles with each failure.
    pub retry_after: Duration,
    /// Longest wait before a retry, and how long every generation pauses
    /// after a usage limit.
    pub max_retry_after: Duration,
    /// Most TL;DRs kept in memory.
    pub cache_capacity: usize,
    /// How often working agents' status lines are made.
    pub status: StatusSettings,
    /// Requests for a turn's TL;DR (`?turn=`) each device may make that
    /// start a generation (design §8: per-device rate limits).
    pub request_rate: RateLimit,
    /// How long a provisional TL;DR of a turn in progress is served for
    /// that turn however it grows; a request after that makes a fresh one.
    pub provisional_for: Duration,
}

impl Default for TldrSettings {
    /// Disabled, with default pacing.
    fn default() -> Self {
        Self {
            summarizer: None,
            timeout: DEFAULT_TIMEOUT,
            settle: DEFAULT_SETTLE,
            retry_after: DEFAULT_RETRY_AFTER,
            max_retry_after: DEFAULT_MAX_RETRY_AFTER,
            cache_capacity: summarize::DEFAULT_CACHE_CAPACITY,
            status: StatusSettings::default(),
            request_rate: DEFAULT_REQUEST_RATE,
            provisional_for: DEFAULT_PROVISIONAL_FOR,
        }
    }
}

/// The TL;DR of a turn, as far as the bridge knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TldrState {
    /// Generated and cached.
    Ready(Tldr),
    /// Being generated (or queued, or waiting for the turn to settle); a
    /// `tldr_ready` event follows if one is made (a turn found short only
    /// gets one on request).
    Generating,
    /// Not cached, and not generated now: a turn asked for by number,
    /// while another one is being generated on demand. Ask again later.
    Busy,
    /// Not cached, and not generated now: the device asked for too many
    /// TL;DRs lately. Ask again after this long.
    Limited(Duration),
    /// There is none: no finished turn, a short turn, no transcript (not a
    /// Claude agent), its generation failed recently, the account hit its
    /// usage limit, or TL;DRs are disabled.
    None,
}

/// Where the TL;DR service gets agents and threads, and announces results.
#[derive(Clone)]
pub struct TldrDeps {
    /// The live agent list (watched, and `tldr_ready` goes out on it).
    pub hub: Arc<Hub>,
    /// Transcript readers.
    pub threads: Arc<Threads>,
    /// herdr, to re-read an agent when its job runs.
    pub control: Arc<dyn AgentControl>,
}

impl fmt::Debug for TldrDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TldrDeps").finish_non_exhaustive()
    }
}

/// What the backfill does next ([`Book::next_backfill`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum NextBackfill {
    /// Look up this agent's latest finished turn.
    Start(AgentId),
    /// Nothing until woken at this instant (a usage limit lifts).
    WakeAt(Instant),
    /// Nothing until a job ends or an agent is queued.
    Wait,
}

/// One turn of one session: what a job summarizes.
type TurnId = (SessionId, TurnIndex);

/// What a job is for, which decides whether it may be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    /// The session's latest finished turn: always queued.
    Latest,
    /// An earlier turn asked for by number: at most [`MAX_OLDER_JOBS`],
    /// on-demand jobs ([`Book::requested`]) included.
    Older,
    /// The latest finished turn of an agent first seen already finished
    /// (see [`Book::backfill`]): a headline only, never a TL;DR.
    Backfill,
}

/// A queued or running job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Job {
    kind: JobKind,
    /// The job makes a headline only (the turn is short, or its TL;DR is
    /// made), so TL;DR requests for it are not answered "generating".
    headline_only: bool,
    /// The job has read its turn and decided what to make
    /// ([`Book::decide`]); its kind no longer changes that.
    decided: bool,
    /// A backfill job turned into the turn's eager one after it had
    /// decided: the turn gets an eager job of its own once this one ends.
    rerun: bool,
}

impl Job {
    /// A job of `kind`, not yet decided.
    fn new(kind: JobKind, headline_only: bool) -> Self {
        Self {
            kind,
            headline_only,
            decided: false,
            rerun: false,
        }
    }

    /// Whether the job (or the rerun it asked for) may still produce a
    /// TL;DR.
    fn may_summarize(self) -> bool {
        !self.headline_only || self.rerun
    }
}

/// A turn whose generation failed.
#[derive(Debug, Clone, Copy)]
struct Failed {
    attempts: u32,
    retry_at: Instant,
}

/// The turn an agent was just seen finishing, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JustFinished {
    /// `None` until the watcher has read which turn it was.
    turn: Option<TurnIndex>,
    at: Instant,
}

/// Bookkeeping shared by requests, jobs and the watcher.
#[derive(Debug, Default)]
struct Book {
    /// Turns with a queued or running job.
    jobs: HashMap<TurnId, Job>,
    failures: HashMap<TurnId, Failed>,
    /// Turns with a retry scheduled for when their backoff ends.
    retrying: HashSet<TurnId>,
    /// Per session, the latest turn seen finished. Kept when an agent
    /// leaves (it may be the same session moving panes); bounded instead.
    /// Shared with status lines.
    finished: FinishedTurns,
    /// Per agent, the turn the watcher last saw it finish.
    just_finished: HashMap<AgentId, JustFinished>,
    /// Turns with a queued or running on-demand job: a short turn or the
    /// turn in progress, asked for by number (see [`Tldrs::request`]).
    requested: HashSet<TurnId>,
    /// Per turn in progress, its latest provisional TL;DR's key and when
    /// it was made.
    provisional: HashMap<TurnId, (TurnKey, Instant)>,
    /// Agents first seen already finished, oldest first, waiting for a
    /// headline of their latest finished turn: the bridge saw no turn
    /// finish for them (it just started, or they arrived finished). One
    /// at a time, and only while no other job is queued.
    backfill: VecDeque<AgentId>,
    /// A backfill step (an agent's latest finished turn being looked up)
    /// is running; its job, once started, counts in [`Self::jobs`].
    backfilling: bool,
    /// When the backfill wakes up after a usage limit, if it is waiting
    /// for one.
    backfill_wake: Option<Instant>,
    /// On-demand jobs that made nothing (failed, or found nothing to
    /// summarize), kept apart from [`Self::failures`] so a failed
    /// provisional TL;DR neither delays the turn's eager one nor spends
    /// its attempts.
    request_failures: HashMap<TurnId, Failed>,
}

impl Book {
    /// Until when `turn` may not be generated, if it may not at `now`: it
    /// failed and its backoff has not expired, or the account is at its
    /// usage limit (`limited`: until when, if it is at `now`).
    fn blocked_until(
        &self,
        turn: &TurnId,
        now: Instant,
        limited: Option<Instant>,
    ) -> Option<Instant> {
        let failed = self
            .failures
            .get(turn)
            .map(|failed| failed.retry_at)
            .filter(|at| *at > now);
        limited.max(failed)
    }

    /// Whether a blocked `turn` is worth retrying on its own when the
    /// block expires: a usage limit still on (`limited`) always is, a
    /// failing turn until it has failed [`MAX_ATTEMPTS`] times.
    fn may_retry(&self, turn: &TurnId, limited: Option<Instant>) -> bool {
        limited.is_some()
            || self
                .failures
                .get(turn)
                .is_none_or(|failed| failed.attempts < MAX_ATTEMPTS)
    }

    /// Whether `turn` may not be generated at `now` (see
    /// [`Self::blocked_until`]).
    fn is_blocked(&self, turn: &TurnId, now: Instant, limited: Option<Instant>) -> bool {
        self.blocked_until(turn, now, limited).is_some()
    }

    /// Records `turn` of `session` as finished, `items` long if known.
    fn note_finished(&mut self, session: &SessionId, turn: TurnIndex, items: Option<usize>) {
        self.finished.note(session, turn, items);
    }

    /// Forgets the agents in `gone`. Their sessions' finished turns stay:
    /// an agent that moved panes is a new agent with the same session.
    fn forget(&mut self, gone: impl IntoIterator<Item = AgentId>) {
        for id in gone {
            self.just_finished.remove(&id);
            self.backfill.retain(|queued| *queued != id);
        }
    }

    /// Queued or running jobs for turns asked for by number that may
    /// still make a TL;DR (one finishing its headline no longer holds up
    /// the next).
    fn older_jobs(&self) -> usize {
        self.requested.len()
            + self
                .jobs
                .values()
                .filter(|job| job.kind == JobKind::Older && !job.headline_only)
                .count()
    }

    /// Whether an on-demand job for `turn` may not run at `now`: its last
    /// one made nothing and its backoff has not expired, or the account
    /// is at its usage limit (`limited`).
    fn request_blocked(&self, turn: &TurnId, now: Instant, limited: Option<Instant>) -> bool {
        limited.is_some()
            || self
                .request_failures
                .get(turn)
                .is_some_and(|failed| failed.retry_at > now)
    }

    /// Remembers `key` as `turn`'s latest provisional TL;DR, made `at`.
    fn note_provisional(&mut self, turn: TurnId, key: TurnKey, at: Instant) {
        if !self.provisional.contains_key(&turn) && self.provisional.len() >= MAX_PROVISIONAL {
            let oldest = self
                .provisional
                .iter()
                .min_by_key(|(_, (_, made))| *made)
                .map(|(turn, _)| turn.clone());
            if let Some(oldest) = oldest {
                self.provisional.remove(&oldest);
            }
        }
        self.provisional.insert(turn, (key, at));
    }

    /// The key of `turn`'s provisional TL;DR if it was made less than
    /// `fresh_for` before `now`.
    fn recent_provisional(
        &self,
        turn: &TurnId,
        now: Instant,
        fresh_for: Duration,
    ) -> Option<&TurnKey> {
        self.provisional
            .get(turn)
            .filter(|(_, made)| now.duration_since(*made) < fresh_for)
            .map(|(key, _)| key)
    }

    /// Queues agent `id` for a backfilled headline, unless it is queued or
    /// the queue is full. Returns whether it was queued.
    fn queue_backfill(&mut self, id: AgentId) -> bool {
        if self.backfill.len() >= MAX_BACKFILL || self.backfill.contains(&id) {
            return false;
        }
        self.backfill.push_back(id);
        true
    }

    /// What the backfill does next, given the usage limit (`limited`:
    /// until when, if it is on). An agent may start only when nothing else
    /// is queued or running (an eager, retried or on-demand job, or
    /// another backfill), so a turn that finishes live waits for at most
    /// one backfilled headline; it is then taken off the queue and marked
    /// running. During a usage limit none starts, and the backfill asks to
    /// be woken when the limit lifts (once per limit).
    fn next_backfill(&mut self, limited: Option<Instant>) -> NextBackfill {
        if self.backfill.is_empty() {
            return NextBackfill::Wait;
        }
        if let Some(until) = limited {
            if self.backfill_wake == Some(until) {
                return NextBackfill::Wait;
            }
            self.backfill_wake = Some(until);
            return NextBackfill::WakeAt(until);
        }
        if self.backfilling || !self.jobs.is_empty() || !self.requested.is_empty() {
            return NextBackfill::Wait;
        }
        let Some(id) = self.backfill.pop_front() else {
            return NextBackfill::Wait;
        };
        self.backfilling = true;
        NextBackfill::Start(id)
    }

    /// Whether the job for `turn` is a backfill job.
    fn is_backfill(&self, turn: &TurnId) -> bool {
        self.jobs
            .get(turn)
            .is_some_and(|job| job.kind == JobKind::Backfill)
    }

    /// Turns the backfill job for `turn`, if there is one, into the turn's
    /// eager job, which makes its TL;DR too unless `headline_only`. One
    /// that has decided what to make keeps to it, and asks for a rerun.
    fn upgrade_backfill(&mut self, turn: &TurnId, headline_only: bool) {
        let Some(job) = self.jobs.get_mut(turn) else {
            return;
        };
        if job.kind != JobKind::Backfill {
            return;
        }
        job.kind = JobKind::Latest;
        if job.decided {
            job.rerun = true;
        } else {
            job.headline_only &= headline_only;
        }
    }

    /// Decides what the job for `turn` makes, given whether the turn is
    /// long and its TL;DR not cached (`wants_tldr`): a TL;DR too, unless
    /// it is a backfill job. Returns whether it makes the TL;DR.
    fn decide(&mut self, turn: &TurnId, wants_tldr: bool) -> bool {
        let Some(job) = self.jobs.get_mut(turn) else {
            return wants_tldr;
        };
        let needs_tldr = wants_tldr && job.kind != JobKind::Backfill;
        job.headline_only = !needs_tldr;
        job.decided = true;
        needs_tldr
    }

    /// Whether a job for `turn` may still produce a TL;DR: it is queued or
    /// running and the turn is not known to be short.
    fn summarizing(&self, turn: &TurnId) -> bool {
        self.jobs.get(turn).is_some_and(|job| job.may_summarize())
    }

    /// Until when `turn` of agent `id` is settling (it just finished and
    /// its last lines may not be written yet), if it is at `now`.
    fn settling_until(
        &self,
        id: &AgentId,
        turn: TurnIndex,
        settle: Duration,
        now: Instant,
    ) -> Option<Instant> {
        let just = self
            .just_finished
            .get(id)
            .filter(|just| just.turn.is_none_or(|known| known == turn))?;
        let until = just.at + settle;
        (until > now).then_some(until)
    }
}

/// Per session, the latest turn seen finished and how many thread items it
/// had then: shared by TL;DRs (which record it) and status lines (which
/// must not take that turn for the one in progress while a new prompt has
/// not reached the transcript, but must describe it once it grows again:
/// task notifications and queued prompts continue a turn without starting
/// a new one, so a long-running agent works on in a turn already seen
/// finished).
#[derive(Debug, Clone, Default)]
pub(crate) struct FinishedTurns(Arc<Mutex<HashMap<SessionId, Finished>>>);

/// A turn seen finished, and its size then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Finished {
    /// The turn.
    pub(crate) turn: TurnIndex,
    /// Its thread items when it was seen finished; [`usize::MAX`] when
    /// that is unknown (it then never counts as grown).
    pub(crate) items: usize,
}

impl Finished {
    /// Whether `turn`, now `items` long, is work beyond this finished
    /// turn: a later turn, or this one grown since.
    pub(crate) fn is_behind(self, turn: TurnIndex, items: usize) -> bool {
        turn > self.turn || (turn == self.turn && items > self.items)
    }
}

impl FinishedTurns {
    /// The latest turn of `session` seen finished, if any.
    pub(crate) fn get(&self, session: &SessionId) -> Option<TurnIndex> {
        self.latest(session).map(|finished| finished.turn)
    }

    /// The latest turn of `session` seen finished and its size then.
    pub(crate) fn latest(&self, session: &SessionId) -> Option<Finished> {
        lock(&self.0).get(session).copied()
    }

    /// Records `turn` of `session` as finished with `items` thread items
    /// (`None`: not known). Only ever raised: a later turn replaces it,
    /// the same turn keeps the larger size.
    fn note(&self, session: &SessionId, turn: TurnIndex, items: Option<usize>) {
        let mut finished = lock(&self.0);
        if !finished.contains_key(session) && finished.len() >= MAX_SESSIONS {
            // Any will do: a forgotten hint only matters in the moment
            // between a new prompt and its transcript line.
            if let Some(evicted) = finished.keys().next().cloned() {
                finished.remove(&evicted);
            }
        }
        let items = items.unwrap_or(usize::MAX);
        let next = match finished.get(session) {
            Some(held) if held.turn > turn => *held,
            Some(held) if held.turn == turn => Finished {
                turn,
                items: held.items.max(items),
            },
            _ => Finished { turn, items },
        };
        finished.insert(session.clone(), next);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        lock(&self.0).len()
    }
}

/// A queued job's entry in [`Book::jobs`] (or, `requested`, in
/// [`Book::requested`]), removed when the job ends however it ends: done,
/// cancelled at shutdown, or panicked.
struct JobSlot {
    tldrs: Arc<Tldrs>,
    turn: TurnId,
    requested: bool,
    /// Set by [`Self::finish`], which has already taken the job out.
    finished: bool,
}

impl JobSlot {
    /// Ends an eager job: takes it out of the book (under one lock, so an
    /// upgrade cannot slip in between reading it and its leaving) and
    /// returns it. The slot's drop then removes nothing, so it can never
    /// take out a new job queued for the same turn meanwhile.
    fn finish(mut self) -> Option<Job> {
        self.finished = true;
        self.tldrs.book().jobs.remove(&self.turn)
    }
}

impl Drop for JobSlot {
    fn drop(&mut self) {
        if !self.finished {
            let mut book = self.tldrs.book();
            if self.requested {
                book.requested.remove(&self.turn);
            } else {
                book.jobs.remove(&self.turn);
            }
        }
        // The backfill may be waiting for the jobs to drain.
        self.tldrs.backfill_ready.notify_one();
    }
}

/// A running backfill step's mark ([`Book::backfilling`]), cleared when
/// the step ends however it ends.
struct BackfillSlot(Arc<Tldrs>);

impl Drop for BackfillSlot {
    fn drop(&mut self) {
        self.0.book().backfilling = false;
        self.0.backfill_ready.notify_one();
    }
}

/// How a job ended.
enum JobEnd {
    /// Done, or nothing (more) to do: no retry.
    Done,
    /// Blocked by a backoff or the usage limit; `latest` says whether the
    /// turn was the session's latest finished one, `headline_only` whether
    /// only its headline is still missing, `backfill` whether it was a
    /// backfill job.
    Blocked {
        latest: bool,
        headline_only: bool,
        backfill: bool,
    },
}

/// The TL;DR cache and its generator.
pub struct Tldrs {
    service: Option<TldrService<SharedSummarizer, MemoryCache>>,
    deps: TldrDeps,
    timeout: Duration,
    settle: Duration,
    /// The failure backoff and the account's usage-limit pause, shared
    /// with suggestions and status lines.
    pacing: Pacing,
    /// One generation at a time, shared with suggestions.
    queue: Arc<Semaphore>,
    book: Mutex<Book>,
    /// Final headlines, and status lines' provisional ones.
    headlines: SharedHeadlines,
    shutdown: Shutdown,
    suggestions: Arc<Suggestions>,
    status: Arc<StatusLines>,
    /// How often the watcher checks working agents' status lines.
    status_check_every: Duration,
    /// Per device, requests that start an on-demand generation.
    requests: RateLimiter<Budget>,
    /// See [`TldrSettings::provisional_for`].
    provisional_for: Duration,
    /// Wakes the watcher to start the next backfill ([`Book::backfill`]):
    /// one was queued, or a job, a backfill step or a usage limit ended.
    backfill_ready: Notify,
}

impl fmt::Debug for Tldrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tldrs")
            .field("enabled", &self.service.is_some())
            .field("timeout", &self.timeout)
            .field("jobs", &self.book().jobs.len())
            .finish_non_exhaustive()
    }
}

impl Tldrs {
    /// The service. Jobs stop at `shutdown`.
    #[must_use]
    pub fn new(settings: TldrSettings, deps: TldrDeps, shutdown: Shutdown) -> Self {
        let pacing = Pacing {
            limit: UsageLimit::default(),
            retry_after: settings.retry_after,
            max_retry_after: settings.max_retry_after.max(settings.retry_after),
        };
        let book = Book::default();
        let headlines = SharedHeadlines::default();
        let queue = Arc::new(Semaphore::new(1));
        let suggestions = Arc::new(Suggestions::new(
            SuggestDeps {
                summarizer: settings.summarizer.clone(),
                threads: Arc::clone(&deps.threads),
                control: Arc::clone(&deps.control),
                timeout: settings.timeout,
                settle: settings.settle,
                pacing: pacing.clone(),
                queue: Arc::clone(&queue),
            },
            shutdown.clone(),
        ));
        let status_check_every = settings.status.check_every;
        let requests = RateLimiter::new(settings.request_rate);
        let provisional_for = settings.provisional_for;
        let status = Arc::new(StatusLines::new(
            StatusDeps {
                summarizer: settings.summarizer.clone(),
                hub: Arc::clone(&deps.hub),
                threads: Arc::clone(&deps.threads),
                control: Arc::clone(&deps.control),
                settings: settings.status,
                timeout: settings.timeout,
                pacing: pacing.clone(),
                queue: Arc::clone(&queue),
                finished: book.finished.clone(),
                headlines: Arc::clone(&headlines),
            },
            shutdown.clone(),
        ));
        Self {
            service: settings.summarizer.map(|summarizer| {
                TldrService::new(summarizer, MemoryCache::new(settings.cache_capacity))
            }),
            deps,
            timeout: settings.timeout,
            settle: settings.settle,
            pacing,
            queue,
            book: Mutex::new(book),
            headlines,
            shutdown,
            suggestions,
            status,
            status_check_every,
            requests,
            provisional_for,
            backfill_ready: Notify::new(),
        }
    }

    /// Live status lines of working agents, made with the same summarizer.
    #[must_use]
    pub fn status_lines(&self) -> &Arc<StatusLines> {
        &self.status
    }

    /// Stamps `agent` with what the bridge knows beyond herdr: its card
    /// headline ([`Self::add_headline`]) and, while it works, its status
    /// line ([`StatusLines::add_status_line`]).
    pub fn decorate(&self, agent: &mut Agent) {
        self.add_headline(agent);
        self.status.add_status_line(agent);
    }

    /// Reply suggestions, made with the same summarizer.
    #[must_use]
    pub fn suggestions(&self) -> &Arc<Suggestions> {
        &self.suggestions
    }

    /// Locks the bookkeeping, recovering from poisoning: no critical
    /// section here can panic midway.
    fn book(&self) -> MutexGuard<'_, Book> {
        lock(&self.book)
    }

    fn headlines(&self) -> MutexGuard<'_, HeadlineStore> {
        lock(&self.headlines)
    }

    /// Whether `turn` has a headline (or needs none), however long it has
    /// grown since.
    fn has_headline(&self, (session, turn): &TurnId) -> bool {
        self.headlines().contains(session, *turn)
    }

    /// Whether `turn`, now `items` long, has a headline made from all of it.
    fn headline_covers(&self, (session, turn): &TurnId, items: usize) -> bool {
        self.headlines().covers(session, *turn, items)
    }

    /// Sets `agent`'s card headline to the headline of its session's
    /// latest finished turn, when that turn has one. An older turn's
    /// headline is never shown: it may be a question the agent no longer
    /// asks.
    pub fn add_headline(&self, agent: &mut Agent) {
        let Some(session) = self.deps.threads.session_of(&agent.id) else {
            return;
        };
        let Some(turn) = self.book().finished.get(&session) else {
            return;
        };
        if let Some(text) = self.headlines().get(&session, turn) {
            agent.headline = Some(text.to_owned());
        }
    }

    /// Headlines of agent `id`'s session for a thread page, in turn order:
    /// the recent set on a page fetched from the start, the last few on a
    /// later page (which catch up on any `headline_ready` missed while the
    /// app was disconnected).
    #[must_use]
    pub fn page_headlines(&self, id: &AgentId, from_start: bool) -> Vec<TurnHeadline> {
        let max = if from_start {
            MAX_PAGE_HEADLINES
        } else {
            MAX_RECENT_HEADLINES
        };
        self.deps
            .threads
            .session_of(id)
            .map_or_else(Vec::new, |session| self.headlines().recent(&session, max))
    }

    /// The TL;DR of a finished turn of `agent` (herdr's current view of
    /// `id`): `wanted`, or the latest. Queues a job for it if it is long,
    /// not cached and not blocked by a failure or the usage limit.
    pub async fn get(
        self: &Arc<Self>,
        id: &AgentId,
        agent: &herdr::Agent,
        wanted: Option<TurnIndex>,
    ) -> TldrState {
        let Some(service) = &self.service else {
            return TldrState::None;
        };
        let Some((session, source)) = self.deps.threads.summary_source(id, agent).await else {
            return TldrState::None;
        };
        {
            // Cheap path for polls while the latest turn is being made: no
            // transcript read.
            let book = self.book();
            let latest = wanted.or_else(|| book.finished.get(&session));
            if latest.is_some_and(|turn| book.summarizing(&(session.clone(), turn))) {
                return TldrState::Generating;
            }
        }
        let ready = agent.agent_status.is_ready();
        let Some(TurnSlice {
            turn,
            latest,
            items,
        }) = self.read_turn(source, &session, ready, wanted).await
        else {
            return TldrState::None;
        };
        if let Some(tldr) = service.cached(&TurnKey::new(session.clone(), turn, &items, true)) {
            return TldrState::Ready(tldr);
        }
        let kind = if turn == latest {
            JobKind::Latest
        } else {
            JobKind::Older
        };
        let turn_id = (session, turn);
        let long = TurnStats::measure(&items, None).is_long();
        {
            let now = Instant::now();
            let limited = self.pacing.limit.active(now);
            let mut book = self.book();
            if book.is_blocked(&turn_id, now, limited) {
                return TldrState::None;
            }
            if long && kind == JobKind::Latest {
                // A backfill job makes no TL;DR; asked for one, it does.
                book.upgrade_backfill(&turn_id, false);
            }
            if let Some(job) = book.jobs.get(&turn_id) {
                return if job.may_summarize() {
                    TldrState::Generating
                } else {
                    TldrState::None
                };
            }
            if book.settling_until(id, turn, self.settle, now).is_some() {
                // Just finished: its job reads it once it is written.
                drop(book);
                self.start(id.clone(), turn_id, kind, false);
                return TldrState::Generating;
            }
        }
        if !long {
            // No TL;DR, but every turn gets a headline: make a missing one
            // for the latest turn (an earlier one only has what was made),
            // or a new one once it has grown and finished again.
            let stale = ready && !self.headline_covers(&turn_id, items.len());
            if kind == JobKind::Latest && (stale || !self.has_headline(&turn_id)) {
                self.start(id.clone(), turn_id, kind, true);
            } else {
                tracing::debug!(agent = %id, turn = turn.0, "TL;DR skipped: short turn");
            }
            return TldrState::None;
        }
        if self.start(id.clone(), turn_id, kind, false) {
            TldrState::Generating
        } else {
            TldrState::Busy
        }
    }

    /// Reads the finished turn asked for, remembering the latest finished
    /// turn while the agent is ready.
    async fn read_turn(
        &self,
        source: ThreadSource,
        session: &SessionId,
        ready: bool,
        wanted: Option<TurnIndex>,
    ) -> Option<TurnSlice> {
        let known_finished = self.book().finished.get(session);
        let query = TurnQuery {
            ready,
            known_finished,
            wanted,
        };
        let slice = self.deps.threads.finished_turn(source, query).await?;
        if ready {
            let items = (slice.turn == slice.latest).then_some(slice.items.len());
            self.book().note_finished(session, slice.latest, items);
        }
        Some(slice)
    }

    /// Re-reads agent `id` from herdr and the given finished turn of its
    /// thread. `None` if the agent moved or is gone, or the turn is not
    /// (or no longer) a finished turn of its thread.
    async fn reread_turn(&self, id: &AgentId, (session, turn): &TurnId) -> Option<TurnSlice> {
        let agent = self.current_agent(id).await?;
        let source = self.summary_source(id, &agent, session).await?;
        let ready = agent.agent_status.is_ready();
        self.read_turn(source, session, ready, Some(*turn)).await
    }

    /// Agent `id`'s thread, while its summaries are still keyed on
    /// `session`: `None` once its transcript matched by folder is another.
    async fn summary_source(
        &self,
        id: &AgentId,
        agent: &herdr::Agent,
        session: &SessionId,
    ) -> Option<ThreadSource> {
        let (now, source) = self.deps.threads.summary_source(id, agent).await?;
        (now == *session).then_some(source)
    }

    /// herdr's current view of agent `id`; `None` if it moved, is gone or
    /// herdr failed.
    async fn current_agent(&self, id: &AgentId) -> Option<herdr::Agent> {
        match resolve(self.deps.control.as_ref(), id).await {
            Ok(Resolved::Current(agent)) => Some(agent),
            Ok(Resolved::Moved(_) | Resolved::Gone) => None,
            Err(error) => {
                tracing::debug!(%error, agent = %id, "herdr agent lookup failed");
                None
            }
        }
    }

    /// The TL;DR of `turn` of `agent` (herdr's current view of `id`),
    /// asked for by `requester` (`?turn=`): any turn after turn 0, long or
    /// short, finished or in progress (see the [module docs](self)). A
    /// long finished turn takes the usual path ([`Self::get`]); any other
    /// is generated regardless of length by an on-demand job, sharing the
    /// one slot for turns asked for by number, so the answer is
    /// [`TldrState::Busy`] while that is taken. A TL;DR of the turn in
    /// progress is announced as provisional.
    pub(crate) async fn request(
        self: &Arc<Self>,
        id: &AgentId,
        agent: &herdr::Agent,
        turn: TurnIndex,
        requester: &Budget,
    ) -> TldrState {
        let Some(service) = &self.service else {
            return TldrState::None;
        };
        let Some((session, source)) = self.deps.threads.summary_source(id, agent).await else {
            return TldrState::None;
        };
        let turn_id = (session, turn);
        if self.book().requested.contains(&turn_id) {
            // Cheap path for polls while it is made: no transcript read. (A
            // turn an eager job is on may yet turn out short, so that is
            // only known after the read below; [`Self::get`] then answers
            // without reading it again.)
            return TldrState::Generating;
        }
        let Some(slice) = self.read_any_turn(source, turn).await else {
            return TldrState::None;
        };
        let in_progress = self.in_progress(agent, &turn_id.0, &slice);
        let settling = self
            .book()
            .settling_until(id, turn, self.settle, Instant::now())
            .is_some();
        // A turn that just finished may not be fully written: it may look
        // short now. The usual path waits for it to settle; a short one is
        // asked for on demand after that.
        if !in_progress && (settling || TurnStats::measure(&slice.items, None).is_long()) {
            return self.get(id, agent, Some(turn)).await;
        }
        if slice.items.is_empty() {
            return TldrState::None;
        }
        let key = TurnKey::new(turn_id.0.clone(), turn, &slice.items, !in_progress);
        let now = Instant::now();
        let limited = self.pacing.limit.active(now);
        let mut book = self.book();
        // The turn in progress keeps its recent provisional TL;DR however
        // it grows.
        let recent = in_progress
            .then(|| book.recent_provisional(&turn_id, now, self.provisional_for))
            .flatten();
        if let Some(tldr) = service
            .cached(&key)
            .or_else(|| recent.and_then(|recent| service.cached(recent)))
        {
            return TldrState::Ready(tldr);
        }
        if book.request_blocked(&turn_id, now, limited) {
            return TldrState::None;
        }
        if book.requested.contains(&turn_id) {
            return TldrState::Generating;
        }
        if book.older_jobs() >= MAX_OLDER_JOBS {
            return TldrState::Busy;
        }
        if let Err(wait) = self.requests.check(requester) {
            return TldrState::Limited(wait);
        }
        book.requested.insert(turn_id.clone());
        drop(book);
        self.start_requested(id.clone(), turn_id);
        TldrState::Generating
    }

    /// Reads `turn` of `source`'s thread, finished or in progress.
    async fn read_any_turn(&self, source: ThreadSource, turn: TurnIndex) -> Option<TurnSlice> {
        let query = TurnQuery {
            // As if ready: the turn in progress counts as finished.
            ready: true,
            known_finished: None,
            wanted: Some(turn),
        };
        self.deps.threads.finished_turn(source, query).await
    }

    /// Whether `slice` (read by [`Self::read_any_turn`]) is the turn
    /// `agent` is still working on: its thread's current turn while it is
    /// not ready, unless that is the turn last seen finished and has not
    /// grown since (the new prompt is not in the transcript yet).
    fn in_progress(&self, agent: &herdr::Agent, session: &SessionId, slice: &TurnSlice) -> bool {
        !agent.agent_status.is_ready()
            && slice.turn == slice.latest
            && self
                .book()
                .finished
                .latest(session)
                .is_none_or(|done| done.is_behind(slice.turn, slice.items.len()))
    }

    /// Runs the on-demand job for `turn` of agent `id`, already entered in
    /// [`Book::requested`].
    fn start_requested(self: &Arc<Self>, id: AgentId, turn: TurnId) {
        let slot = JobSlot {
            tldrs: Arc::clone(self),
            turn,
            requested: true,
            finished: false,
        };
        let mut shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let this = Arc::clone(&slot.tldrs);
            tokio::select! {
                end = this.run_requested(&id, &slot.turn) => {
                    if let Err(failure) = end {
                        this.requested_failed(&id, &slot.turn, &failure);
                    }
                }
                () = shutdown.wait() => {}
            }
            drop(slot);
        });
    }

    /// Records and logs an on-demand job for `turn` that made nothing, so
    /// requests for it are answered "none" until its backoff ends rather
    /// than queueing the same job again.
    fn requested_failed(&self, id: &AgentId, turn_id: &TurnId, failure: &Failure) {
        let wait = self.record_request_failure(turn_id, failure.usage_limit);
        tracing::warn!(
            error = %failure.message,
            agent = %id,
            turn = turn_id.1.0,
            retry_in = ?wait,
            "requested TL;DR generation failed"
        );
    }

    /// Settles (a turn that just finished), waits for the queue, re-reads
    /// the agent and the turn, then makes, caches and announces its TL;DR,
    /// however short; provisional if the turn is still in progress. `Err`
    /// when it made nothing for a reason other than a block already in
    /// place (a backoff or the usage limit, which requests see anyway).
    async fn run_requested(&self, id: &AgentId, turn_id: &TurnId) -> Result<(), Failure> {
        let Some(service) = &self.service else {
            return Ok(());
        };
        let (session, turn) = turn_id;
        let settling = self
            .book()
            .settling_until(id, *turn, self.settle, Instant::now());
        if let Some(until) = settling {
            tokio::time::sleep_until(until).await;
            self.wait_until_written(id, turn_id).await;
        }
        let Ok(_permit) = self.queue.acquire().await else {
            return Ok(());
        };
        let unreadable = || Failure::nothing("the agent or its turn could not be read");
        let agent = self.current_agent(id).await.ok_or_else(unreadable)?;
        let source = self
            .summary_source(id, &agent, session)
            .await
            .ok_or_else(unreadable)?;
        let slice = self
            .read_any_turn(source, *turn)
            .await
            .ok_or_else(unreadable)?;
        let now = Instant::now();
        if self
            .book()
            .request_blocked(turn_id, now, self.pacing.limit.active(now))
        {
            tracing::debug!(agent = %id, turn = turn.0, "requested TL;DR skipped: backing off or at the usage limit");
            return Ok(());
        }
        let digest = TurnDigest::build(&slice.items, None);
        if digest.is_empty() {
            return Err(Failure::nothing("nothing to summarize"));
        }
        let in_progress = self.in_progress(&agent, session, &slice);
        let key = TurnKey::new(session.clone(), *turn, &slice.items, !in_progress);
        let started = Instant::now();
        let generated =
            tokio::time::timeout(self.timeout, service.get_or_generate(&key, &digest)).await;
        match generated {
            Ok(Ok(tldr)) => {
                log_generated("tldr", id, started, tldr_chars(&tldr));
                if in_progress {
                    self.book()
                        .note_provisional(turn_id.clone(), key, Instant::now());
                }
                self.deps.hub.publish(AgentEvent::TldrReady {
                    agent_id: id.clone(),
                    turn: *turn,
                    tldr,
                    provisional: in_progress,
                });
                Ok(())
            }
            Ok(Err(error)) => Err(Failure::from(&error)),
            Err(_) => Err(Failure::timeout(self.timeout)),
        }
    }

    /// Queues a job for `turn` of agent `id`, unless one is queued.
    /// `headline_only` says the job will make no TL;DR. False if it is
    /// refused: an earlier turn beyond [`MAX_OLDER_JOBS`].
    fn start(
        self: &Arc<Self>,
        id: AgentId,
        turn: TurnId,
        kind: JobKind,
        headline_only: bool,
    ) -> bool {
        {
            let mut book = self.book();
            if book.jobs.contains_key(&turn) {
                // A turn seen finishing live while its backfill job runs
                // is that turn's eager job now.
                if kind == JobKind::Latest {
                    book.upgrade_backfill(&turn, headline_only);
                }
                return true;
            }
            if kind == JobKind::Older && book.older_jobs() >= MAX_OLDER_JOBS {
                return false;
            }
            book.jobs
                .insert(turn.clone(), Job::new(kind, headline_only));
        }
        let slot = JobSlot {
            tldrs: Arc::clone(self),
            turn,
            requested: false,
            finished: false,
        };
        let mut shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let this = Arc::clone(&slot.tldrs);
            let turn = slot.turn.clone();
            let end = tokio::select! {
                end = this.run_job(&id, &turn) => end,
                () = shutdown.wait() => return,
            };
            let rerun = slot.finish().is_some_and(|job| job.rerun);
            if rerun {
                this.start(id, turn, JobKind::Latest, false);
            } else if let JobEnd::Blocked {
                latest: true,
                headline_only,
                backfill,
            } = end
            {
                this.retry_later(id, turn, headline_only, backfill);
            }
        });
        true
    }

    /// Settles, waits for the queue, re-reads the agent and the turn, then
    /// makes, keeps and announces what it lacks: its TL;DR if it is long,
    /// and its headline. One failing does not stop the other.
    async fn run_job(&self, id: &AgentId, turn_id: &TurnId) -> JobEnd {
        let Some(service) = &self.service else {
            return JobEnd::Done;
        };
        let (_, turn) = turn_id;
        let settling = self
            .book()
            .settling_until(id, *turn, self.settle, Instant::now());
        if let Some(until) = settling {
            tokio::time::sleep_until(until).await;
            self.wait_until_written(id, turn_id).await;
        }
        let Ok(permit) = self.queue.acquire().await else {
            return JobEnd::Done;
        };
        let Some(TurnSlice { latest, items, .. }) = self.reread_turn(id, turn_id).await else {
            return JobEnd::Done;
        };
        let latest = latest == *turn;
        if !latest && self.book().is_backfill(turn_id) {
            return JobEnd::Done; // Superseded: the card shows a later turn.
        }
        let needs_tldr = self.decide(service, turn_id, &items);
        // Read again: an upgrade may have come in meanwhile.
        let backfill = self.book().is_backfill(turn_id);
        // A turn that grew since its headline (it went on after a task
        // notification or a queued prompt) gets a new one.
        let needs_headline = !self.headline_covers(turn_id, items.len());
        if !needs_tldr && !needs_headline {
            tracing::debug!(agent = %id, turn = turn.0, "summary job skipped: TL;DR and headline are current");
            return JobEnd::Done;
        }
        if self.is_blocked(turn_id) {
            tracing::debug!(agent = %id, turn = turn.0, "summary job skipped: backing off or at the usage limit");
            return JobEnd::Blocked {
                latest,
                headline_only: !needs_tldr,
                backfill,
            };
        }
        let mut failures = Vec::new();
        let mut tldr_missing = false;
        let mut permit = Some(permit);
        if needs_tldr {
            match self.make_tldr(service, id, turn_id, &items).await {
                Ok(()) => {
                    // Made and announced: TL;DR reads are no longer
                    // "generating" while the headline is made.
                    self.set_headline_only(turn_id, true);
                    // Its attempts do not count against the headline's.
                    self.book().failures.remove(turn_id);
                }
                Err(failure) => {
                    tldr_missing = true;
                    failures.push(("TL;DR", failure));
                }
            }
            // The headline takes a slot of its own: a suggestion waiting
            // for the queue goes between the two calls.
            permit = None;
        }
        if needs_headline {
            let _permit = match permit {
                Some(permit) => permit,
                None => match self.queue.acquire().await {
                    Ok(permit) => permit,
                    Err(_) => return JobEnd::Done,
                },
            };
            let limited = failures.iter().any(|(_, f): &(_, Failure)| f.usage_limit)
                || self.pacing.limit.active(Instant::now()).is_some();
            if limited {
                // A usage limit (this job's or a suggestion's) holds the
                // headline back too; it is retried when the limit lifts.
                tracing::debug!(agent = %id, turn = turn.0, "headline skipped: usage limit");
                if failures.is_empty() {
                    return JobEnd::Blocked {
                        latest,
                        headline_only: !tldr_missing,
                        backfill,
                    };
                }
            } else if let Err(failure) = self.make_headline(service, id, turn_id, &items).await {
                failures.push(("headline", failure));
            }
        }
        if failures.is_empty() {
            self.book().failures.remove(turn_id);
            return JobEnd::Done;
        }
        let usage_limit = failures.iter().any(|(_, failure)| failure.usage_limit);
        let wait = self.record_failure(turn_id, usage_limit);
        for (what, failure) in failures {
            tracing::warn!(
                error = %failure.message,
                agent = %id,
                turn = turn.0,
                retry_in = ?wait,
                "{what} generation failed"
            );
        }
        JobEnd::Blocked {
            latest,
            headline_only: !tldr_missing,
            backfill,
        }
    }

    /// Decides what the job for `turn`, now `items` long, makes (see
    /// [`Book::decide`]); returns whether it makes the TL;DR: the turn is
    /// long, its TL;DR is not cached, and it is not a backfill job.
    fn decide(
        &self,
        service: &TldrService<SharedSummarizer, MemoryCache>,
        turn_id: &TurnId,
        items: &[ThreadItem],
    ) -> bool {
        let (session, turn) = turn_id;
        let wants_tldr = TurnStats::measure(items, None).is_long()
            && service
                .cached(&TurnKey::new(session.clone(), *turn, items, true))
                .is_none();
        self.book().decide(turn_id, wants_tldr)
    }

    /// Marks the job for `turn` as headline-only (or not).
    fn set_headline_only(&self, turn: &TurnId, headline_only: bool) {
        if let Some(job) = self.book().jobs.get_mut(turn) {
            job.headline_only = headline_only;
        }
    }

    /// Whether `turn` may not be generated now: a backoff or the usage
    /// limit.
    fn is_blocked(&self, turn: &TurnId) -> bool {
        let now = Instant::now();
        let limited = self.pacing.limit.active(now);
        self.book().is_blocked(turn, now, limited)
    }

    /// Makes, caches and announces the TL;DR of the long turn `turn_id`.
    async fn make_tldr(
        &self,
        service: &TldrService<SharedSummarizer, MemoryCache>,
        id: &AgentId,
        (session, turn): &TurnId,
        items: &[ThreadItem],
    ) -> Result<(), Failure> {
        let started = Instant::now();
        let generated = tokio::time::timeout(
            self.timeout,
            service.on_turn_complete(session.clone(), *turn, items, None),
        )
        .await;
        match generated {
            Ok(Ok(Some(tldr))) => {
                log_generated("tldr", id, started, tldr_chars(&tldr));
                self.deps.hub.publish(AgentEvent::TldrReady {
                    agent_id: id.clone(),
                    turn: *turn,
                    tldr,
                    provisional: false,
                });
                Ok(())
            }
            // A short turn after all: nothing to summarize.
            Ok(Ok(None)) => Ok(()),
            Ok(Err(error)) => Err(Failure::from(&error)),
            Err(_) => Err(Failure::timeout(self.timeout)),
        }
    }

    /// Makes, keeps and announces the headline of the finished turn
    /// `turn_id` from its `items`. A turn with nothing in it is recorded as
    /// needing none, so it is not asked for again.
    async fn make_headline(
        &self,
        service: &TldrService<SharedSummarizer, MemoryCache>,
        id: &AgentId,
        (session, turn): &TurnId,
        items: &[ThreadItem],
    ) -> Result<(), Failure> {
        let made = generate_headline(service.summarizer(), id, items, self.timeout, "headline");
        let headline = match made.await {
            HeadlineMade::Made(headline) => headline,
            HeadlineMade::Empty => {
                self.headlines().put(session, *turn, None, items.len());
                tracing::debug!(agent = %id, turn = turn.0, "headline skipped: nothing to headline");
                return Ok(());
            }
            HeadlineMade::Failed(failure) => return Err(failure),
        };
        self.headlines()
            .put(session, *turn, Some(headline.clone()), items.len());
        // The job re-read it as a finished turn: the card headline follows
        // the session's latest finished turn.
        self.book().note_finished(session, *turn, Some(items.len()));
        self.deps.hub.publish(AgentEvent::HeadlineReady {
            agent_id: id.clone(),
            turn: *turn,
            headline,
            provisional: false,
        });
        Ok(())
    }

    /// Waits until a just-finished turn stops changing: re-reads it every
    /// [`TldrSettings::settle`] until two reads agree, at most
    /// [`pace::MAX_SETTLE_ROUNDS`] times. A turn summarized while lines are
    /// still arriving would be summarized partly, then again.
    async fn wait_until_written(&self, id: &AgentId, turn: &TurnId) {
        settled(self.settle, || self.reread_turn(id, turn)).await;
    }

    /// Queues `turn` again once what blocks it (a backoff or the usage
    /// limit) expires, unless a retry is already scheduled. So a failure
    /// does not leave the latest turn without a TL;DR until the next one;
    /// only the latest turn is retried (not one a later turn has
    /// superseded meanwhile), and only so often (see [`MAX_ATTEMPTS`]). A
    /// backfill job's retry goes back on the backfill queue, behind fresh
    /// work.
    fn retry_later(
        self: &Arc<Self>,
        id: AgentId,
        turn: TurnId,
        headline_only: bool,
        backfill: bool,
    ) {
        let at = {
            let now = Instant::now();
            let limited = self.pacing.limit.active(now);
            let mut book = self.book();
            let Some(at) = book.blocked_until(&turn, now, limited) else {
                return;
            };
            if !book.may_retry(&turn, limited) || !book.retrying.insert(turn.clone()) {
                return;
            }
            at
        };
        let this = Arc::clone(self);
        let mut shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep_until(at) => {}
                () = shutdown.wait() => return,
            }
            let superseded = {
                let mut book = this.book();
                book.retrying.remove(&turn);
                book.finished
                    .get(&turn.0)
                    .is_some_and(|latest| latest > turn.1)
            };
            if superseded {
                return;
            }
            if backfill {
                this.queue_backfill(id);
            } else {
                this.start(id, turn, JobKind::Latest, headline_only);
            }
        });
    }

    /// Remembers a failed generation; returns how long it now waits. A
    /// usage limit is account-wide, so it pauses every generation.
    fn record_failure(&self, turn: &TurnId, usage_limit: bool) -> Duration {
        if usage_limit {
            return self.pacing.after_failure(1, true);
        }
        self.backoff(&mut self.book().failures, turn)
    }

    /// Remembers an on-demand job for `turn` that made nothing (see
    /// [`Book::request_failures`]); returns how long it now waits. A usage
    /// limit pauses every generation, as for any job.
    fn record_request_failure(&self, turn: &TurnId, usage_limit: bool) -> Duration {
        if usage_limit {
            return self.pacing.after_failure(1, true);
        }
        self.backoff(&mut self.book().request_failures, turn)
    }

    /// Counts one more failure of `turn` in `failures` and backs it off;
    /// returns the wait.
    fn backoff(&self, failures: &mut HashMap<TurnId, Failed>, turn: &TurnId) -> Duration {
        let now = Instant::now();
        let attempts = failures
            .get(turn)
            .map_or(0, |failed| failed.attempts)
            .saturating_add(1);
        let wait = self.pacing.after_failure(attempts, false);
        // Expired entries stay (their attempt counts keep the backoff
        // doubling) until the map is full.
        while !failures.contains_key(turn) && failures.len() >= MAX_FAILURES {
            let soonest = failures
                .iter()
                .min_by_key(|(_, failed)| failed.retry_at)
                .map(|(key, _)| key.clone());
            match soonest {
                Some(key) => failures.remove(&key),
                None => break,
            };
        }
        failures.insert(
            turn.clone(),
            Failed {
                attempts,
                retry_at: now + wait,
            },
        );
        wait
    }

    /// Follows the agent list until `shutdown`, queueing a job for each
    /// Claude agent's turn as it finishes and a backfilled headline for
    /// each agent first seen already finished (see the [module
    /// docs](self)), and checking working agents' status lines every
    /// [`StatusSettings::check_every`] (see [`status`]). `None` when TL;DRs
    /// are disabled.
    #[must_use]
    pub fn spawn_watcher(self: &Arc<Self>, mut shutdown: Shutdown) -> Option<JoinHandle<()>> {
        self.service.as_ref()?;
        let this = Arc::clone(self);
        let mut feed = self.deps.hub.subscribe();
        let check_every = self.status_check_every.max(Duration::from_millis(1));
        let mut status_check = tokio::time::interval_at(Instant::now() + check_every, check_every);
        status_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Some(tokio::spawn(async move {
            let mut statuses = HashMap::new();
            loop {
                let event = tokio::select! {
                    event = feed.next() => event,
                    _ = status_check.tick() => {
                        this.status.tick(&this.deps.hub.agents());
                        continue;
                    }
                    () = this.backfill_ready.notified() => {
                        this.next_backfill();
                        continue;
                    }
                    () = shutdown.wait() => break,
                };
                this.status.observe(&event);
                this.book().forget(departed(&statuses, &event));
                for id in arrived_finished(&statuses, &event) {
                    this.queue_backfill(id);
                }
                for id in finished_turns(&mut statuses, &event) {
                    // Recorded now, so requests racing the turn lookup
                    // below already wait for the turn to settle.
                    let at = Instant::now();
                    let just = JustFinished { turn: None, at };
                    this.book().just_finished.insert(id.clone(), just);
                    this.on_turn_finished(id, at);
                }
            }
        }))
    }

    /// Agent `id` finished a turn at `at`: learns which one (its number is
    /// already in the transcript, even if its last lines are not), records
    /// it, and queues its job, which waits out the settle pause.
    fn on_turn_finished(self: &Arc<Self>, id: AgentId, at: Instant) {
        let this = Arc::clone(self);
        let mut shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = this.queue_finished(id, at) => {}
                () = shutdown.wait() => {}
            }
        });
    }

    async fn queue_finished(self: &Arc<Self>, id: AgentId, at: Instant) {
        let Some(agent) = self.current_agent(&id).await else {
            return;
        };
        // Reply suggestions for a question the turn ended on, made now so
        // they are ready when the user looks (design §6.2).
        self.suggestions.for_turn(&id, &agent);
        let Some((session, source)) = self.deps.threads.summary_source(&id, &agent).await else {
            return;
        };
        // The watcher saw the turn finish, whatever herdr says by now: a
        // quick reply may already have the agent working again, before
        // its prompt reaches the transcript.
        let Some(TurnSlice { turn, .. }) = self.read_turn(source, &session, true, None).await
        else {
            return;
        };
        {
            let mut book = self.book();
            if let Some(just) = book.just_finished.get_mut(&id).filter(|just| just.at == at) {
                just.turn = Some(turn);
            }
        }
        self.start(id, (session, turn), JobKind::Latest, false);
    }

    /// Queues agent `id`, first seen already finished, for a headline of
    /// its latest finished turn (see [`Book::backfill`]).
    fn queue_backfill(&self, id: AgentId) {
        if self.book().queue_backfill(id) {
            self.backfill_ready.notify_one();
        }
    }

    /// Starts the next backfill step if it may start now (see
    /// [`Book::next_backfill`]). During a usage limit nothing starts; the
    /// backfill wakes up again when it lifts.
    fn next_backfill(self: &Arc<Self>) {
        let limited = self.pacing.limit.active(Instant::now());
        let next = self.book().next_backfill(limited);
        let id = match next {
            NextBackfill::Start(id) => id,
            NextBackfill::WakeAt(at) => return self.wake_backfill_at(at),
            NextBackfill::Wait => return,
        };
        let slot = BackfillSlot(Arc::clone(self));
        let mut shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = slot.0.backfill(id) => {}
                () = shutdown.wait() => {}
            }
            drop(slot);
        });
    }

    /// Wakes the backfill at `at` (when a usage limit lifts).
    fn wake_backfill_at(self: &Arc<Self>, at: Instant) {
        let this = Arc::clone(self);
        let mut shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep_until(at) => this.backfill_ready.notify_one(),
                () = shutdown.wait() => {}
            }
        });
    }

    /// Looks up the latest finished turn of agent `id` (first seen already
    /// finished), records it, so the card shows its headline, and queues a
    /// headline-only job for it unless it has a headline (or needs none).
    /// An agent working again by now is left to its live finish.
    async fn backfill(self: &Arc<Self>, id: AgentId) {
        let Some(agent) = self.current_agent(&id).await else {
            return;
        };
        if !agent.agent_status.is_ready() {
            return;
        }
        let Some((session, source)) = self.deps.threads.summary_source(&id, &agent).await else {
            return;
        };
        let Some(TurnSlice { turn, items, .. }) =
            self.read_turn(source, &session, true, None).await
        else {
            return;
        };
        let turn_id = (session, turn);
        if self.headline_covers(&turn_id, items.len()) {
            tracing::debug!(agent = %id, turn = turn.0, "headline backfill skipped: already made");
            return;
        }
        if !self.book().may_retry(&turn_id, None) {
            // Out of attempts: an agent re-listed (a herdr blip) does not
            // buy the turn another model run.
            tracing::debug!(agent = %id, turn = turn.0, "headline backfill skipped: out of attempts");
            return;
        }
        self.start(id, turn_id, JobKind::Backfill, true);
    }
}

/// Logs one successful generation: what, for whom, how long it took and
/// how long the output is. Never the content: it is about untrusted agent
/// output (design §8).
pub(crate) fn log_generated(kind: &'static str, agent: &AgentId, started: Instant, chars: usize) {
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(kind, %agent, latency_ms, chars, "summary generated");
}

/// What [`generate_headline`] made.
pub(crate) enum HeadlineMade {
    /// The headline.
    Made(String),
    /// Nothing: the turn has nothing to headline.
    Empty,
    /// The model failed or timed out.
    Failed(Failure),
}

/// Makes a headline of agent `id`'s turn from its `items`, finished
/// (`kind` "headline") or so far (status lines' provisional one): the
/// headline digest, the `timeout`, and the generation log entry. Shared
/// so the two headlines are made alike.
pub(crate) async fn generate_headline(
    summarizer: &SharedSummarizer,
    id: &AgentId,
    items: &[ThreadItem],
    timeout: Duration,
    kind: &'static str,
) -> HeadlineMade {
    let digest = TurnDigest::build_with(items, None, &DigestLimits::headline());
    if digest.is_empty() {
        return HeadlineMade::Empty;
    }
    let started = Instant::now();
    match tokio::time::timeout(timeout, Summarizer::headline(summarizer, &digest)).await {
        Ok(Ok(headline)) => {
            let headline = String::from(headline);
            log_generated(kind, id, started, headline.chars().count());
            HeadlineMade::Made(headline)
        }
        Ok(Err(error)) => HeadlineMade::Failed(Failure::from(&error)),
        Err(_) => HeadlineMade::Failed(Failure::timeout(timeout)),
    }
}

/// The length of a TL;DR's text, in characters.
fn tldr_chars(tldr: &Tldr) -> usize {
    [&tldr.outcome]
        .into_iter()
        .chain(&tldr.changed)
        .chain(&tldr.decide)
        .chain(&tldr.risks)
        .map(|text| text.chars().count())
        .sum()
}

/// Why a generation failed, for the log and the backoff.
pub(crate) struct Failure {
    /// What went wrong.
    pub(crate) message: String,
    /// The account hit its usage limit.
    pub(crate) usage_limit: bool,
}

impl Failure {
    /// Nothing was generated, for `why`.
    fn nothing(why: &str) -> Self {
        Self {
            message: why.to_owned(),
            usage_limit: false,
        }
    }

    /// The generation took longer than `after`.
    pub(crate) fn timeout(after: Duration) -> Self {
        Self {
            message: format!("timed out after {after:?}"),
            usage_limit: false,
        }
    }
}

impl From<&summarize::Error> for Failure {
    fn from(error: &summarize::Error) -> Self {
        Self {
            message: error.to_string(),
            usage_limit: error.is_usage_limit(),
        }
    }
}

/// The agents `event` shows gone, given the statuses tracked so far.
fn departed(statuses: &HashMap<AgentId, AgentStatus>, event: &AgentEvent) -> Vec<AgentId> {
    match event {
        AgentEvent::AgentRemoved { agent_id } => vec![agent_id.clone()],
        AgentEvent::Snapshot { agents } => statuses
            .keys()
            .filter(|id| !agents.iter().any(|a| a.id == **id))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// The agents that may have a transcript (see [`has_transcript`])
/// that `event` shows for the first time, given the statuses tracked so
/// far, already idle or done: the bridge saw no turn of theirs finish (it
/// just started, or they arrived finished), so their latest finished turn
/// gets a backfilled headline.
fn arrived_finished(statuses: &HashMap<AgentId, AgentStatus>, event: &AgentEvent) -> Vec<AgentId> {
    let arrived = |agent: &&Agent| {
        has_transcript(agent) && agent.status.is_ready() && !statuses.contains_key(&agent.id)
    };
    match event {
        AgentEvent::Snapshot { agents } => agents
            .iter()
            .filter(arrived)
            .map(|agent| agent.id.clone())
            .collect(),
        AgentEvent::AgentUpdated { agent: Some(agent) } if arrived(&agent) => {
            vec![agent.id.clone()]
        }
        _ => Vec::new(),
    }
}

/// Tracks the status of each agent that may have a transcript (Claude or
/// Codex, see [`has_transcript`]) through `event` and returns the
/// agents that just finished a turn: went from working or blocked to idle
/// or done. A snapshot is compared with the statuses it replaces, so
/// turns that finished while the watcher lagged are still caught; the
/// first snapshot only records statuses, so a restart does not summarize
/// every idle agent (they get a backfilled headline instead, see
/// [`arrived_finished`]).
fn finished_turns(
    statuses: &mut HashMap<AgentId, AgentStatus>,
    event: &AgentEvent,
) -> Vec<AgentId> {
    let finished = |before: Option<AgentStatus>, now: AgentStatus| {
        matches!(before, Some(AgentStatus::Working | AgentStatus::Blocked)) && now.is_ready()
    };
    match event {
        AgentEvent::Snapshot { agents } => {
            let tracked = || agents.iter().filter(|a| has_transcript(a));
            let done = tracked()
                .filter(|a| finished(statuses.get(&a.id).copied(), a.status))
                .map(|a| a.id.clone())
                .collect();
            *statuses = tracked().map(|a| (a.id.clone(), a.status)).collect();
            done
        }
        AgentEvent::AgentUpdated { agent: Some(agent) } if has_transcript(agent) => {
            let before = statuses.insert(agent.id.clone(), agent.status);
            if finished(before, agent.status) {
                vec![agent.id.clone()]
            } else {
                Vec::new()
            }
        }
        AgentEvent::AgentRemoved { agent_id } => {
            statuses.remove(agent_id);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use protocol::AgentKind;
    use protocol::{Agent, Seq, Timestamp};

    use super::*;

    const NONE: [AgentId; 0] = [];

    fn agent(pane: &str, kind: AgentKind, status: AgentStatus) -> Agent {
        Agent {
            id: format!("{pane}~5f0c6a1e").parse().expect("agent id"),
            title: "worker".into(),
            repo: "repo".into(),
            worktree: None,
            status,
            seq: Seq(1),
            status_since: Timestamp::default(),
            unread: false,
            kind,
            headline: None,
            status_line: None,
            ask: None,
            quiet_done: false,
        }
    }

    fn updated(agent: Agent) -> AgentEvent {
        AgentEvent::AgentUpdated { agent: Some(agent) }
    }

    fn snapshot(agents: &[&Agent]) -> AgentEvent {
        AgentEvent::Snapshot {
            agents: agents.iter().map(|a| (*a).clone()).collect(),
        }
    }

    #[test]
    fn a_turn_finishes_when_a_busy_claude_agent_becomes_ready() {
        let mut statuses = HashMap::new();
        let working = agent("w1:p1", AgentKind::Claude, AgentStatus::Working);
        assert_eq!(finished_turns(&mut statuses, &snapshot(&[&working])), NONE);
        let done = agent("w1:p1", AgentKind::Claude, AgentStatus::Done);
        let id = done.id.clone();
        assert_eq!(
            finished_turns(&mut statuses, &updated(done.clone())),
            std::slice::from_ref(&id)
        );
        // Done again (e.g. a title change) is not another finished turn.
        assert_eq!(finished_turns(&mut statuses, &updated(done.clone())), NONE);
        let blocked = agent("w1:p1", AgentKind::Claude, AgentStatus::Blocked);
        assert_eq!(finished_turns(&mut statuses, &updated(blocked)), NONE);
        let idle = agent("w1:p1", AgentKind::Claude, AgentStatus::Idle);
        assert_eq!(finished_turns(&mut statuses, &updated(idle)), [id]);
    }

    #[test]
    fn a_resync_snapshot_still_catches_finished_turns() {
        let mut statuses = HashMap::new();
        let working = agent("w1:p1", AgentKind::Claude, AgentStatus::Working);
        let idle = agent("w1:p2", AgentKind::Claude, AgentStatus::Idle);
        assert_eq!(
            finished_turns(&mut statuses, &snapshot(&[&working, &idle])),
            NONE
        );
        // The watcher lagged; the resync shows p1 done.
        let done = agent("w1:p1", AgentKind::Claude, AgentStatus::Done);
        let id = done.id.clone();
        assert_eq!(
            finished_turns(&mut statuses, &snapshot(&[&done, &idle])),
            [id]
        );
        assert_eq!(statuses.len(), 2);
    }

    #[test]
    fn unknown_and_other_agents_are_not_summarized() {
        let mut statuses = HashMap::new();
        // First sighting: nothing is known about the turn before.
        let idle = agent("w1:p1", AgentKind::Claude, AgentStatus::Idle);
        assert_eq!(finished_turns(&mut statuses, &updated(idle.clone())), NONE);
        let pi = agent("w1:p2", AgentKind::Pi, AgentStatus::Working);
        finished_turns(&mut statuses, &updated(pi));
        let pi_done = agent("w1:p2", AgentKind::Pi, AgentStatus::Done);
        assert_eq!(finished_turns(&mut statuses, &updated(pi_done)), NONE);
        // A removed agent starts over.
        let working = agent("w1:p1", AgentKind::Claude, AgentStatus::Working);
        finished_turns(&mut statuses, &updated(working));
        let removed = AgentEvent::AgentRemoved {
            agent_id: idle.id.clone(),
        };
        finished_turns(&mut statuses, &removed);
        assert_eq!(finished_turns(&mut statuses, &updated(idle)), NONE);
    }

    #[test]
    fn an_agent_first_seen_finished_is_backfilled() {
        let mut statuses = HashMap::new();
        let done = agent("w1:p1", AgentKind::Claude, AgentStatus::Done);
        let idle = agent("w1:p2", AgentKind::Codex, AgentStatus::Idle);
        let working = agent("w1:p3", AgentKind::Claude, AgentStatus::Working);
        let blocked = agent("w1:p4", AgentKind::Claude, AgentStatus::Blocked);
        let pi = agent("w1:p5", AgentKind::Pi, AgentStatus::Done);
        let mut no_session = agent("w1:p6", AgentKind::Codex, AgentStatus::Done);
        no_session.id = "w1:p6".parse().expect("agent id");
        // At startup: every finished agent that may have a transcript, one
        // matched by folder too.
        let first = snapshot(&[&done, &idle, &working, &blocked, &pi, &no_session]);
        assert_eq!(
            arrived_finished(&statuses, &first),
            [done.id.clone(), idle.id.clone(), no_session.id.clone()]
        );
        finished_turns(&mut statuses, &first);
        // Seen before: not again, whatever it does.
        assert_eq!(arrived_finished(&statuses, &updated(done.clone())), NONE);
        assert_eq!(arrived_finished(&statuses, &first), NONE);
        // One arriving finished later is; one arriving working is not.
        let late = agent("w2:p1", AgentKind::Claude, AgentStatus::Idle);
        let late_id = late.id.clone();
        assert_eq!(arrived_finished(&statuses, &updated(late)), [late_id]);
        let busy = agent("w2:p2", AgentKind::Claude, AgentStatus::Working);
        assert_eq!(arrived_finished(&statuses, &updated(busy)), NONE);
        // The agent first seen working finishes live: an eager job, not a
        // backfill.
        let finished = agent("w1:p3", AgentKind::Claude, AgentStatus::Done);
        assert_eq!(
            arrived_finished(&statuses, &updated(finished.clone())),
            NONE
        );
        assert_eq!(
            finished_turns(&mut statuses, &updated(finished.clone())),
            [finished.id]
        );
    }

    fn job(kind: JobKind) -> Job {
        Job::new(kind, kind == JobKind::Backfill)
    }

    #[test]
    fn a_backfill_job_upgraded_before_it_decides_makes_the_tldr_itself() {
        let mut book = Book::default();
        let turn = (session(1), TurnIndex(2));
        book.jobs.insert(turn.clone(), job(JobKind::Backfill));
        assert!(!book.summarizing(&turn), "headline only");
        assert!(
            !book.decide(&turn.clone(), true),
            "a backfill makes no TL;DR"
        );
        book.jobs.insert(turn.clone(), job(JobKind::Backfill));
        // A TL;DR is asked for (or the turn is seen finishing live).
        book.upgrade_backfill(&turn, false);
        let upgraded = book.jobs[&turn];
        assert_eq!(upgraded.kind, JobKind::Latest);
        assert!(!upgraded.headline_only && !upgraded.rerun);
        assert!(book.summarizing(&turn));
        assert!(book.decide(&turn, true), "it makes the TL;DR");
        // Upgrading only ever applies to a backfill job.
        book.upgrade_backfill(&turn, false);
        assert!(!book.jobs[&turn].rerun);
    }

    #[test]
    fn a_backfill_job_upgraded_after_it_decides_asks_for_a_rerun() {
        let mut book = Book::default();
        let turn = (session(1), TurnIndex(2));
        book.jobs.insert(turn.clone(), job(JobKind::Backfill));
        assert!(!book.decide(&turn, true));
        book.upgrade_backfill(&turn, false);
        let upgraded = book.jobs[&turn];
        assert_eq!(upgraded.kind, JobKind::Latest);
        assert!(upgraded.headline_only, "it keeps to its decision");
        assert!(upgraded.rerun, "the TL;DR comes from a rerun");
        // Requests wait for that rerun.
        assert!(book.summarizing(&turn));
        assert!(!book.is_backfill(&turn), "its retry is an eager one");
    }

    #[test]
    fn a_departed_agent_leaves_the_backfill_queue() {
        let mut book = Book::default();
        let a = agent("w1:p1", AgentKind::Claude, AgentStatus::Done).id;
        let b = agent("w1:p2", AgentKind::Claude, AgentStatus::Done).id;
        book.queue_backfill(a.clone());
        book.queue_backfill(b.clone());
        book.forget([a]);
        assert_eq!(book.backfill, [b]);
    }

    #[test]
    fn backfills_run_one_at_a_time_behind_every_other_job() {
        let mut book = Book::default();
        let a = agent("w1:p1", AgentKind::Claude, AgentStatus::Done).id;
        let b = agent("w1:p2", AgentKind::Claude, AgentStatus::Done).id;
        assert!(book.queue_backfill(a.clone()));
        assert!(!book.queue_backfill(a.clone()), "queued once");
        assert!(book.queue_backfill(b.clone()));
        // A turn that finished live is queued: it goes first.
        let live = (session(1), TurnIndex(3));
        book.jobs.insert(live.clone(), job(JobKind::Latest));
        assert_eq!(book.next_backfill(None), NextBackfill::Wait);
        book.jobs.remove(&live);
        // So does an on-demand TL;DR.
        book.requested.insert(live.clone());
        assert_eq!(book.next_backfill(None), NextBackfill::Wait);
        book.requested.remove(&live);
        // Then one backfill at a time: its lookup, then its job.
        assert_eq!(book.next_backfill(None), NextBackfill::Start(a));
        assert_eq!(
            book.next_backfill(None),
            NextBackfill::Wait,
            "a's lookup runs"
        );
        book.backfilling = false;
        let backfilled = (session(2), TurnIndex(1));
        book.jobs.insert(backfilled.clone(), job(JobKind::Backfill));
        assert!(book.is_backfill(&backfilled));
        assert!(!book.summarizing(&backfilled), "headline only");
        assert_eq!(book.next_backfill(None), NextBackfill::Wait, "a's job runs");
        book.jobs.remove(&backfilled);
        assert_eq!(book.next_backfill(None), NextBackfill::Start(b));
        book.backfilling = false;
        assert_eq!(book.next_backfill(None), NextBackfill::Wait, "drained");
    }

    #[test]
    fn no_backfill_starts_during_a_usage_limit() {
        let mut book = Book::default();
        let now = Instant::now();
        let until = now + Duration::from_mins(30);
        assert_eq!(book.next_backfill(Some(until)), NextBackfill::Wait, "empty");
        let a = agent("w1:p1", AgentKind::Claude, AgentStatus::Done).id;
        book.queue_backfill(a.clone());
        // Woken when the limit lifts, asked once per limit.
        assert_eq!(book.next_backfill(Some(until)), NextBackfill::WakeAt(until));
        assert_eq!(book.next_backfill(Some(until)), NextBackfill::Wait);
        let later = until + Duration::from_mins(5);
        assert_eq!(book.next_backfill(Some(later)), NextBackfill::WakeAt(later));
        assert_eq!(book.backfill.len(), 1, "still queued");
        // Lifted: it starts.
        assert_eq!(book.next_backfill(None), NextBackfill::Start(a));
    }

    #[test]
    fn the_backfill_queue_is_bounded() {
        let mut book = Book::default();
        for n in 0..MAX_BACKFILL {
            let id = format!("w{n}:p1").parse().expect("agent id");
            assert!(book.queue_backfill(id));
        }
        let over = "w9999:p1".parse().expect("agent id");
        assert!(!book.queue_backfill(over));
        assert_eq!(book.backfill.len(), MAX_BACKFILL);
    }

    fn session(n: u32) -> SessionId {
        SessionId::new(format!("00000000-0000-4000-8000-{n:012}")).expect("session")
    }

    #[test]
    fn an_agent_leaving_keeps_its_sessions_finished_turn() {
        let mut book = Book::default();
        let old = agent("w1:p1", AgentKind::Claude, AgentStatus::Done).id;
        let session = old.session().cloned().expect("session");
        book.note_finished(&session, TurnIndex(4), Some(3));
        let at = Instant::now();
        book.just_finished
            .insert(old.clone(), JustFinished { turn: None, at });
        // The agent moved panes: the old id goes, the session lives on.
        book.forget([old.clone()]);
        assert!(!book.just_finished.contains_key(&old));
        assert_eq!(book.finished.get(&session), Some(TurnIndex(4)));
        // Only ever raised.
        book.note_finished(&session, TurnIndex(2), Some(9));
        assert_eq!(book.finished.get(&session), Some(TurnIndex(4)));
    }

    #[test]
    fn a_finished_turn_is_behind_once_it_grows_or_another_starts() {
        let finished = FinishedTurns::default();
        let s = session(1);
        finished.note(&s, TurnIndex(3), Some(10));
        let held = finished.latest(&s).expect("noted");
        assert!(
            !held.is_behind(TurnIndex(3), 10),
            "not grown: the gap after a prompt"
        );
        assert!(
            held.is_behind(TurnIndex(3), 11),
            "went on after a notification"
        );
        assert!(held.is_behind(TurnIndex(4), 1), "a new turn");
        assert!(!held.is_behind(TurnIndex(2), 50));
        // The same turn keeps the larger size; an unknown size never grows.
        finished.note(&s, TurnIndex(3), Some(4));
        assert_eq!(finished.latest(&s).map(|f| f.items), Some(10));
        finished.note(&s, TurnIndex(5), None);
        let unknown = finished.latest(&s).expect("noted");
        assert!(!unknown.is_behind(TurnIndex(5), 1_000));
        finished.note(&s, TurnIndex(4), Some(1));
        assert_eq!(finished.get(&s), Some(TurnIndex(5)), "only ever raised");
    }

    #[test]
    fn finished_turns_are_kept_for_a_bounded_number_of_sessions() {
        let mut book = Book::default();
        for n in 0..=u32::try_from(MAX_SESSIONS).expect("small") {
            book.note_finished(&session(n), TurnIndex(1), None);
        }
        assert_eq!(book.finished.len(), MAX_SESSIONS);
    }

    #[test]
    fn provisional_tldrs_are_served_while_fresh_and_kept_for_a_bounded_number_of_turns() {
        let mut book = Book::default();
        let now = Instant::now();
        let fresh_for = Duration::from_secs(30);
        let key =
            |n: u32, items: &[ThreadItem]| TurnKey::new(session(n), TurnIndex(1), items, false);
        let turn = (session(0), TurnIndex(1));
        assert_eq!(book.recent_provisional(&turn, now, fresh_for), None);
        book.note_provisional(turn.clone(), key(0, &[]), now);
        assert_eq!(
            book.recent_provisional(&turn, now, fresh_for),
            Some(&key(0, &[]))
        );
        assert_eq!(
            book.recent_provisional(&turn, now + fresh_for, fresh_for),
            None,
            "stale"
        );
        for n in 1..=u32::try_from(MAX_PROVISIONAL).expect("small") {
            let at = now + Duration::from_millis(u64::from(n));
            book.note_provisional((session(n), TurnIndex(1)), key(n, &[]), at);
        }
        assert_eq!(book.provisional.len(), MAX_PROVISIONAL);
        assert!(!book.provisional.contains_key(&turn), "the oldest goes");
    }

    #[test]
    fn a_failing_turn_is_retried_by_itself_only_a_few_times() {
        let mut book = Book::default();
        let turn = (session(1), TurnIndex(3));
        let now = Instant::now();
        assert!(book.may_retry(&turn, None));
        assert_eq!(book.blocked_until(&turn, now, None), None);
        let retry_at = now + Duration::from_secs(60);
        book.failures.insert(
            turn.clone(),
            Failed {
                attempts: MAX_ATTEMPTS - 1,
                retry_at,
            },
        );
        assert!(book.may_retry(&turn, None));
        assert_eq!(book.blocked_until(&turn, now, None), Some(retry_at));
        assert_eq!(book.blocked_until(&turn, retry_at, None), None, "expired");
        book.failures.insert(
            turn.clone(),
            Failed {
                attempts: MAX_ATTEMPTS,
                retry_at,
            },
        );
        assert!(!book.may_retry(&turn, None));
        // A usage limit is always worth waiting out, and blocks every turn
        // until the later of the two.
        let pause = UsageLimit::default();
        let limit = now + Duration::from_secs(600);
        pause.pause_until(limit);
        let limited = pause.active(now);
        assert!(book.may_retry(&turn, limited));
        assert_eq!(book.blocked_until(&turn, now, limited), Some(limit));
        let other = (session(2), TurnIndex(1));
        assert_eq!(book.blocked_until(&other, now, limited), Some(limit));
        // Once the limit has passed, the attempt cap holds again (the
        // pause is shared with suggestions, so it must not stick).
        let later = limit + Duration::from_secs(1);
        assert!(!book.may_retry(&turn, pause.active(later)));
    }
}
