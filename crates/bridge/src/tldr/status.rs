//! Live status lines of working agents (`status_line` on the agent,
//! `status_line_ready` events, `POST /v1/agents/:id/status`; design §6.1,
//! §6.4).
//!
//! Long-running agents (the circus master) rarely finish a turn, so their
//! headline never arrives. While a Claude agent works, the bridge makes a
//! line of at most [`protocol::MAX_STATUS_LINE_CHARS`] characters (what it
//! is doing now, and what it waits on) from the tail of the turn in
//! progress, with the TL;DR summarizer and the same hardening
//! ([`Summarizer::status`]).
//!
//! - **Throttled.** The TL;DR watcher ticks every
//!   [`StatusSettings::check_every`]; an agent's line is remade at most every
//!   [`StatusSettings::every`], and only once its turn has grown
//!   meaningfully since the last line ([`MIN_GROWTH_ITEMS`] new thread items
//!   or [`MIN_GROWTH_WORDS`] new words of output; a new turn counts from
//!   zero). A turn that has not grown is only re-read (again after a third
//!   of the interval, or of the rolling headline's if shorter, not on
//!   every tick), never re-sent to the model. A
//!   periodic job gives way when the queue is busy: finished turns' TL;DRs
//!   and headlines, suggestions and taps come first.
//! - **Never the previous turn.** herdr reports an agent working before
//!   its new prompt reaches the transcript; until then the thread's current
//!   turn is the one the TL;DR watcher saw finish, and it is not described.
//! - **On demand.** A tap (`POST /v1/agents/:id/status`) remakes it now,
//!   skipping the interval and the growth threshold, at most once per
//!   [`StatusSettings::tap_every`] per agent. A turn that has not changed at
//!   all is answered at once ("unchanged"); a tap during a running job is
//!   answered by that job, with the tap's threshold.
//! - **One at a time, never while limited.** Generations take the queue
//!   slot shared with TL;DRs, headlines and suggestions, and none starts
//!   while the account's usage limit pause is on; a usage limit hit here
//!   pauses them all. Other failures back off per agent (doubling, like the
//!   TL;DR's).
//! - **Served while working.** The line is kept per agent and stamped on
//!   the agent in the list and on the stream only while it works
//!   ([`StatusLines::add_status_line`]); a new line is announced as
//!   [`AgentEvent::StatusLineReady`] and the stream re-sends the agent. It
//!   is dropped when the agent finishes its turn (its headline takes over)
//!   or leaves.
//! - **Rolling headline.** The same job also gives the turn in progress a
//!   provisional headline (made like the final one, see
//!   [`super::generate_headline`], from the turn so far): at most every
//!   [`StatusSettings::headline_every`] per agent, with the same growth
//!   threshold measured since the last one, in a queue slot of its own
//!   (after the line, and giving way when the queue is busy), never while
//!   limited, and never for a turn whose final headline already covers it.
//!   A tap does not skip its interval, but does not stop it either. It
//!   backs off on failure apart from the line, so neither holds the other
//!   up. It goes in the TL;DR headline store
//!   marked provisional and is announced as [`AgentEvent::HeadlineReady`]
//!   with `provisional` set; the turn's final headline replaces it when
//!   the turn finishes. Its throttle outlives the agent's stops: a
//!   long-running agent stops and goes on often.
//!
//! The input is the turn's thread items only, as for the TL;DR.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use protocol::{Agent, AgentEvent, AgentId, AgentStatus, StatusLine, ThreadItem, TurnIndex};
use summarize::{DigestLimits, Summarizer, TurnDigest, TurnStats};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::Instant;

use super::FinishedTurns;
use super::SharedSummarizer;
use super::headlines::SharedHeadlines;
use super::pace::{Pacing, lock};
use super::{Failure, HeadlineMade, generate_headline};
use crate::control::{AgentControl, Resolved, resolve};
use crate::registry::{Hub, now};
use crate::shutdown::Shutdown;
use crate::threads::{ThreadSource, Threads, TurnQuery, TurnSlice, has_transcript};

/// Default shortest time between two periodic lines for one agent.
pub const DEFAULT_EVERY: Duration = Duration::from_mins(3);

/// Default shortest time between two provisional headlines for one agent.
pub const DEFAULT_HEADLINE_EVERY: Duration = Duration::from_mins(5);

/// Default interval between checks of the working agents.
pub const DEFAULT_CHECK_EVERY: Duration = Duration::from_secs(30);

/// Default shortest time between two taps for one agent.
pub const DEFAULT_TAP_EVERY: Duration = Duration::from_secs(30);

/// New thread items since the last line that make a periodic one worth
/// making (tool calls are items: a busy agent grows by these).
pub const MIN_GROWTH_ITEMS: usize = 3;

/// New words of assistant output since the last line that make a periodic
/// one worth making.
pub const MIN_GROWTH_WORDS: usize = 100;

/// Most agents remembered.
const MAX_AGENTS: usize = 256;

/// A periodic check that found nothing to do (no growth yet) looks again
/// after this fraction of [`StatusSettings::every`] (or of
/// [`StatusSettings::headline_every`] if shorter), not on every tick:
/// each look is a herdr round-trip and a transcript read.
const RECHECK_FRACTION: u32 = 3;

/// The thread's current turn, finished or not: read as if the agent were
/// ready, which makes the turn in progress the "latest finished" one.
const CURRENT: TurnQuery = TurnQuery {
    ready: true,
    known_finished: None,
    wanted: None,
};

/// How often status lines are made.
#[derive(Debug, Clone)]
pub struct StatusSettings {
    /// Shortest time between two periodic lines for one agent.
    pub every: Duration,
    /// Interval between checks of the working agents.
    pub check_every: Duration,
    /// Shortest time between two taps for one agent.
    pub tap_every: Duration,
    /// Shortest time between two provisional headlines of the turn in
    /// progress for one agent.
    pub headline_every: Duration,
}

impl Default for StatusSettings {
    fn default() -> Self {
        Self {
            every: DEFAULT_EVERY,
            check_every: DEFAULT_CHECK_EVERY,
            tap_every: DEFAULT_TAP_EVERY,
            headline_every: DEFAULT_HEADLINE_EVERY,
        }
    }
}

/// What a tap did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusRefresh {
    /// A new line is being made; a `status_line_ready` event follows
    /// (unless it fails).
    Queued,
    /// The turn has not changed since the last line (or has nothing in
    /// progress yet): the line shown is current.
    Unchanged,
    /// Nothing to do: the agent is not a working Claude agent, or status
    /// lines are disabled.
    Idle,
    /// Tapped again too soon; try again after this long.
    TooSoon(Duration),
    /// The account is at its usage limit; nothing is made for this long.
    Paused(Duration),
}

/// Why a job runs, which decides how much the turn must have grown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// The periodic check: [`MIN_GROWTH_ITEMS`] or [`MIN_GROWTH_WORDS`].
    Every,
    /// A tap: any change at all.
    Tap,
}

/// How far a turn had got: what growth is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Size {
    turn: TurnIndex,
    items: usize,
    words: usize,
}

impl Size {
    fn of(turn: TurnIndex, items: &[ThreadItem]) -> Self {
        Self {
            turn,
            items: items.len(),
            words: TurnStats::measure(items, None).output_words,
        }
    }

    /// Whether the turn has grown enough since `before` (the size at the
    /// last line, if any) for `trigger`. Another turn counts from zero.
    fn grew(self, before: Option<Self>, trigger: Trigger) -> bool {
        let before = before.filter(|b| b.turn == self.turn);
        let (items, words) = before.map_or((0, 0), |b| (b.items, b.words));
        let new_items = self.items.saturating_sub(items);
        let new_words = self.words.saturating_sub(words);
        match trigger {
            Trigger::Every => new_items >= MIN_GROWTH_ITEMS || new_words >= MIN_GROWTH_WORDS,
            Trigger::Tap => before != Some(self) && self.items > 0,
        }
    }
}

/// What a job makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    /// The status line.
    Line,
    /// The turn's provisional headline.
    Headline,
}

impl Output {
    /// Its name in the generation log.
    const fn kind(self) -> &'static str {
        match self {
            Self::Line => "status",
            Self::Headline => "provisional_headline",
        }
    }

    /// Its name in messages.
    const fn what(self) -> &'static str {
        match self {
            Self::Line => "status line",
            Self::Headline => "provisional headline",
        }
    }
}

/// How making one output went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Made, or skipped for now (queue busy, usage limit, turn gone).
    Done,
    /// Nothing to summarize in the turn.
    Empty,
    /// The model failed; the agent backs off.
    Failed,
}

/// What a job will make, by how the turn has grown and what is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Wants {
    line: bool,
    headline: bool,
}

/// What is known about one agent's line.
#[derive(Debug, Default)]
struct Entry {
    /// The line, until the agent finishes its turn.
    line: Option<StatusLine>,
    /// When the last line was made.
    made_at: Option<Instant>,
    /// The turn's size when it was made.
    baseline: Option<Size>,
    /// When a periodic check last found nothing to do (no growth, or no
    /// turn in progress yet).
    checked_at: Option<Instant>,
    /// The last tap.
    tapped_at: Option<Instant>,
    /// The queued or running job, by its token.
    job: Option<u64>,
    /// Tokens handed out so far.
    jobs_started: u64,
    /// A tap arrived while a job was running: that job (or one right
    /// after it) answers it with the tap's threshold.
    tap_pending: bool,
    /// The line's failures.
    line_backoff: Backoff,
    /// Bumped when the line is dropped, so a job that started before
    /// cannot bring an old turn's line (or headline) back.
    epoch: u64,
    /// When the last provisional headline was made; kept when the line is
    /// dropped, so the throttle holds across the agent's stops.
    headline_made_at: Option<Instant>,
    /// The turn's size when it was made.
    headline_baseline: Option<Size>,
    /// The provisional headline's failures; kept when the line is dropped.
    headline_backoff: Backoff,
}

/// Failures in a row of one output, and until when the periodic check
/// leaves it alone.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Backoff {
    failures: u32,
    retry_at: Option<Instant>,
}

impl Backoff {
    /// Whether it no longer holds the output back at `now`.
    fn expired(self, now: Instant) -> bool {
        self.retry_at.is_none_or(|at| at <= now)
    }
}

impl Entry {
    /// Whether the periodic check may start a job at `now`: none running,
    /// the line or the headline due, and the last empty check at least
    /// [`RECHECK_FRACTION`] of the shorter interval ago.
    fn due(&self, now: Instant, settings: &StatusSettings) -> bool {
        let recheck = settings.every.min(settings.headline_every) / RECHECK_FRACTION;
        self.job.is_none()
            && (self.line_due(now, settings) || self.headline_due(now, settings))
            && self.checked_at.is_none_or(|at| now >= at + recheck)
    }

    /// Whether a periodic line is due at `now`: its interval has passed
    /// and it is not backing off.
    fn line_due(&self, now: Instant, settings: &StatusSettings) -> bool {
        self.line_backoff.expired(now) && self.made_at.is_none_or(|at| now >= at + settings.every)
    }

    /// Whether a provisional headline is due at `now`: its interval has
    /// passed and it is not backing off.
    fn headline_due(&self, now: Instant, settings: &StatusSettings) -> bool {
        self.headline_backoff.expired(now)
            && self
                .headline_made_at
                .is_none_or(|at| now >= at + settings.headline_every)
    }

    /// What a job with `trigger` makes of the turn, now `size`, at `now`:
    /// the line if it has grown enough since the last one for `trigger`
    /// (and, for the periodic check, it is due); the provisional headline
    /// if it is due and the turn has grown enough since the last one,
    /// whatever started the job (a tap neither skips nor drops it).
    fn wants(
        &self,
        size: Size,
        trigger: Trigger,
        now: Instant,
        settings: &StatusSettings,
    ) -> Wants {
        let grew = size.grew(self.baseline, trigger);
        Wants {
            line: grew && (trigger == Trigger::Tap || self.line_due(now, settings)),
            headline: self.headline_due(now, settings)
                && size.grew(self.headline_baseline, Trigger::Every),
        }
    }

    /// The backoff of `output`.
    const fn backoff_of(&self, output: Output) -> Backoff {
        match output {
            Output::Line => self.line_backoff,
            Output::Headline => self.headline_backoff,
        }
    }

    /// The backoff of `output`, to change.
    fn backoff(&mut self, output: Output) -> &mut Backoff {
        match output {
            Output::Line => &mut self.line_backoff,
            Output::Headline => &mut self.headline_backoff,
        }
    }

    /// Forgets the turn: its line, its baseline and its failures (not the
    /// provisional headline's throttle or backoff).
    fn drop_line(&mut self) {
        self.line = None;
        self.baseline = None;
        self.made_at = None;
        self.checked_at = None;
        self.line_backoff = Backoff::default();
        self.epoch = self.epoch.wrapping_add(1);
    }
}

/// Where status lines come from and go.
#[derive(Clone)]
pub(crate) struct StatusDeps {
    pub(crate) summarizer: Option<SharedSummarizer>,
    pub(crate) hub: Arc<Hub>,
    pub(crate) threads: Arc<Threads>,
    pub(crate) control: Arc<dyn AgentControl>,
    pub(crate) settings: StatusSettings,
    pub(crate) timeout: Duration,
    /// Failure backoff and the usage-limit pause, shared.
    pub(crate) pacing: Pacing,
    /// The one-at-a-time generation queue, shared.
    pub(crate) queue: Arc<Semaphore>,
    /// Turns the TL;DR watcher saw finish.
    pub(crate) finished: FinishedTurns,
    /// The TL;DR headline store, where provisional headlines go.
    pub(super) headlines: SharedHeadlines,
}

/// The status lines of working agents, and their generator.
pub struct StatusLines {
    deps: StatusDeps,
    book: Mutex<HashMap<AgentId, Entry>>,
    shutdown: Shutdown,
}

impl fmt::Debug for StatusLines {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatusLines")
            .field("enabled", &self.deps.summarizer.is_some())
            .field("settings", &self.deps.settings)
            .finish_non_exhaustive()
    }
}

/// A job's mark in its agent's entry, cleared however the job ends (if
/// it is still this job's).
struct JobSlot {
    lines: Arc<StatusLines>,
    id: AgentId,
    token: u64,
}

impl Drop for JobSlot {
    fn drop(&mut self) {
        if let Some(entry) = self.lines.book().get_mut(&self.id)
            && entry.job == Some(self.token)
        {
            entry.job = None;
        }
    }
}

/// How a job ended, for the periodic check's bookkeeping.
enum JobEnd {
    /// Nothing to summarize yet (no growth, or no turn in progress).
    Nothing,
    /// Made a line, failed, or stopped for another reason.
    Done,
}

impl StatusLines {
    pub(crate) fn new(deps: StatusDeps, shutdown: Shutdown) -> Self {
        Self {
            deps,
            book: Mutex::default(),
            shutdown,
        }
    }

    fn book(&self) -> MutexGuard<'_, HashMap<AgentId, Entry>> {
        lock(&self.book)
    }

    /// Whether status lines are made at all.
    pub(crate) fn enabled(&self) -> bool {
        self.deps.summarizer.is_some()
    }

    /// Sets `agent`'s status line while it works; never otherwise.
    pub fn add_status_line(&self, agent: &mut Agent) {
        if agent.status != AgentStatus::Working {
            return;
        }
        if let Some(line) = self.book().get(&agent.id).and_then(|e| e.line.clone()) {
            agent.status_line = Some(line);
        }
    }

    /// The periodic check: starts a job for each working Claude agent in
    /// `agents` that is due (see the [module docs](self)).
    pub(crate) fn tick(self: &Arc<Self>, agents: &[Agent]) {
        if !self.enabled() {
            return;
        }
        let now = Instant::now();
        if self.deps.pacing.limit.active(now).is_some() {
            tracing::debug!("status lines skipped: usage limit");
            return;
        }
        let due: Vec<AgentId> = {
            let mut book = self.book();
            agents
                .iter()
                .filter(|a| is_working_with_transcript(a))
                .filter(|a| {
                    let due =
                        entry(&mut book, &a.id).is_some_and(|e| e.due(now, &self.deps.settings));
                    if !due {
                        tracing::debug!(agent = %a.id, "status line skipped: throttled");
                    }
                    due
                })
                .map(|a| a.id.clone())
                .collect()
        };
        for id in due {
            self.start(id, Trigger::Every, None);
        }
    }

    /// Follows the agent list: drops the line of an agent that finished
    /// its turn, and forgets agents that left (once their job is over: a
    /// running job keeps its entry, with the line dropped, so it cannot
    /// store a line for a newcomer with the same id).
    pub(crate) fn observe(&self, event: &AgentEvent) {
        let mut book = self.book();
        match event {
            AgentEvent::Snapshot { agents } => {
                for agent in agents.iter().filter(|a| a.status.is_ready()) {
                    if let Some(entry) = book.get_mut(&agent.id) {
                        entry.drop_line();
                    }
                }
                book.retain(|id, entry| entry.job.is_some() || agents.iter().any(|a| a.id == *id));
            }
            AgentEvent::AgentUpdated { agent: Some(agent) } if agent.status.is_ready() => {
                if let Some(entry) = book.get_mut(&agent.id) {
                    entry.drop_line();
                }
            }
            AgentEvent::AgentRemoved { agent_id } => {
                if let Some(entry) = book.get_mut(agent_id) {
                    if entry.job.is_some() {
                        entry.drop_line();
                    } else {
                        book.remove(agent_id);
                    }
                }
            }
            _ => {}
        }
    }

    /// A tap on agent `id` (herdr's current view of it is `agent`): makes
    /// its line now, unless tapped too recently, limited, or the turn has
    /// not changed since the last line.
    pub async fn refresh(self: &Arc<Self>, id: &AgentId, agent: &herdr::Agent) -> StatusRefresh {
        if !self.enabled() || agent.agent_status != herdr::AgentStatus::Working {
            return StatusRefresh::Idle;
        }
        let Some((_, source)) = self.deps.threads.summary_source(id, agent).await else {
            return StatusRefresh::Idle;
        };
        let now = Instant::now();
        let baseline = {
            let mut book = self.book();
            let Some(entry) = entry(&mut book, id) else {
                return StatusRefresh::TooSoon(self.deps.settings.tap_every);
            };
            if let Some(next) = entry
                .tapped_at
                .map(|at| at + self.deps.settings.tap_every)
                .filter(|next| *next > now)
            {
                return StatusRefresh::TooSoon(next - now);
            }
            entry.tapped_at = Some(now);
            if let Some(until) = self.deps.pacing.limit.active(now) {
                return StatusRefresh::Paused(until - now);
            }
            if entry.job.is_some() {
                // The running job answers it, with the tap's threshold.
                entry.tap_pending = true;
                return StatusRefresh::Queued;
            }
            entry.baseline
        };
        // Read now, so an unchanged turn is answered at once.
        let grew = self
            .read_current(&source)
            .await
            .is_some_and(|slice| Size::of(slice.turn, &slice.items).grew(baseline, Trigger::Tap));
        if !grew {
            return StatusRefresh::Unchanged;
        }
        self.start(id.clone(), Trigger::Tap, Some(source));
        StatusRefresh::Queued
    }

    /// Starts a job for agent `id` (whose thread is `source`, if already
    /// looked up), unless one is running; a tap then waits for that one.
    fn start(self: &Arc<Self>, id: AgentId, trigger: Trigger, source: Option<ThreadSource>) {
        let (token, epoch) = {
            let mut book = self.book();
            let Some(entry) = entry(&mut book, &id) else {
                return;
            };
            if entry.job.is_some() {
                entry.tap_pending |= trigger == Trigger::Tap;
                return;
            }
            entry.jobs_started = entry.jobs_started.wrapping_add(1);
            entry.job = Some(entry.jobs_started);
            (entry.jobs_started, entry.epoch)
        };
        let slot = JobSlot {
            lines: Arc::clone(self),
            id,
            token,
        };
        let mut shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let this = Arc::clone(&slot.lines);
            let ended = tokio::select! {
                end = this.run_job(&slot.id, trigger, source, epoch) => Some(end),
                () = shutdown.wait() => None,
            };
            let Some(end) = ended else {
                return;
            };
            if this.finish(&slot.id, token, &end) {
                // A tap came in after this job had decided: answer it now.
                this.start(slot.id.clone(), Trigger::Tap, None);
            }
            drop(slot);
        });
    }

    /// Clears job `token` of agent `id`, noting an empty periodic check;
    /// returns whether a tap is still waiting.
    fn finish(&self, id: &AgentId, token: u64, end: &JobEnd) -> bool {
        let mut book = self.book();
        let Some(entry) = book.get_mut(id).filter(|e| e.job == Some(token)) else {
            return false;
        };
        entry.job = None;
        if matches!(end, JobEnd::Nothing) {
            entry.checked_at = Some(Instant::now());
        }
        std::mem::take(&mut entry.tap_pending)
    }

    /// Whether a tap is waiting for the running job (and takes it).
    fn take_tap(&self, id: &AgentId) -> bool {
        self.book()
            .get_mut(id)
            .is_some_and(|entry| std::mem::take(&mut entry.tap_pending))
    }

    /// Reads the turn in progress; if it has grown enough for `trigger`
    /// (a tap waiting makes it a tap), makes its line, then its
    /// provisional headline if that is due too, each in a queue slot of
    /// its own (see [`Self::slot`]).
    async fn run_job(
        &self,
        id: &AgentId,
        trigger: Trigger,
        source: Option<ThreadSource>,
        epoch: u64,
    ) -> JobEnd {
        let Some(summarizer) = &self.deps.summarizer else {
            return JobEnd::Done;
        };
        let source = match source {
            Some(source) => source,
            None => match self.working_source(id).await {
                Some(source) => source,
                None => return JobEnd::Nothing,
            },
        };
        let Some(mut slice) = self.read_current(&source).await else {
            return JobEnd::Nothing;
        };
        let trigger = if self.take_tap(id) {
            Trigger::Tap
        } else {
            trigger
        };
        let size = Size::of(slice.turn, &slice.items);
        let mut wants = self
            .book()
            .get(id)
            .map(|e| e.wants(size, trigger, Instant::now(), &self.deps.settings))
            .unwrap_or_default();
        if wants.headline && self.headline_covers(id, size) {
            tracing::debug!(agent = %id, turn = size.turn.0, "provisional headline skipped: the final headline is current");
            wants.headline = false;
        }
        if !wants.line && !wants.headline {
            tracing::debug!(agent = %id, turn = size.turn.0, "status line skipped: not grown enough");
            return JobEnd::Nothing;
        }
        let mut steps = Vec::with_capacity(2);
        if wants.line {
            let step = self.make_line(summarizer, id, trigger, &source, &mut slice, epoch);
            steps.push(step.await);
        }
        if wants.headline {
            let step = self.make_headline(summarizer, id, &source, &mut slice, epoch);
            steps.push(step.await);
        }
        if steps.iter().all(|step| *step == Step::Empty) {
            JobEnd::Nothing
        } else {
            JobEnd::Done
        }
    }

    /// Whether the turn, now `size`, already has its final headline (it
    /// was seen finished at this size), so a provisional one is not needed.
    fn headline_covers(&self, id: &AgentId, size: Size) -> bool {
        self.deps.threads.session_of(id).is_some_and(|session| {
            lock(&self.deps.headlines).covers(&session, size.turn, size.items)
        })
    }

    /// Takes the queue slot for `output`: a periodic job gives way if it is
    /// busy (finished turns and taps come first); a tap waits for it, then
    /// reads the turn in progress again into `slice`, as the wait may have
    /// been long (a periodic job's read is as fresh as it needs to be).
    /// `None` when the queue is busy, the account is at its usage limit, or
    /// there is no turn in progress any more.
    async fn slot(
        &self,
        id: &AgentId,
        trigger: Trigger,
        source: &ThreadSource,
        slice: &mut TurnSlice,
        output: Output,
    ) -> Option<SemaphorePermit<'_>> {
        let permit = match trigger {
            Trigger::Every => self.deps.queue.try_acquire().ok(),
            Trigger::Tap => self.deps.queue.acquire().await.ok(),
        };
        let Some(permit) = permit else {
            // Busy: the next check tries again.
            tracing::debug!(agent = %id, "{} skipped: queue busy", output.what());
            return None;
        };
        if self.deps.pacing.limit.active(Instant::now()).is_some() {
            tracing::debug!(agent = %id, "{} skipped: usage limit", output.what());
            return None;
        }
        if trigger == Trigger::Tap {
            *slice = self.read_current(source).await?;
        }
        Some(permit)
    }

    /// Makes, keeps and announces the line of the turn in progress.
    async fn make_line(
        &self,
        summarizer: &SharedSummarizer,
        id: &AgentId,
        trigger: Trigger,
        source: &ThreadSource,
        slice: &mut TurnSlice,
        epoch: u64,
    ) -> Step {
        let Some(_permit) = self.slot(id, trigger, source, slice, Output::Line).await else {
            return Step::Done;
        };
        let digest = TurnDigest::build_with(&slice.items, None, &DigestLimits::status());
        if digest.is_empty() {
            return Step::Empty;
        }
        let started = Instant::now();
        let made =
            tokio::time::timeout(self.deps.timeout, Summarizer::status(summarizer, &digest)).await;
        let failure = match made {
            Ok(Ok(text)) => {
                let text = String::from(text);
                super::log_generated(Output::Line.kind(), id, started, text.chars().count());
                self.keep(id, epoch, Size::of(slice.turn, &slice.items), text);
                return Step::Done;
            }
            Ok(Err(error)) => Failure::from(&error),
            Err(_) => Failure::timeout(self.deps.timeout),
        };
        self.failed(id, Output::Line, &failure);
        Step::Failed
    }

    /// Makes, keeps and announces the provisional headline of the turn in
    /// progress, from the turn so far (`slice`, as the line read it).
    async fn make_headline(
        &self,
        summarizer: &SharedSummarizer,
        id: &AgentId,
        source: &ThreadSource,
        slice: &mut TurnSlice,
        epoch: u64,
    ) -> Step {
        let slot = self.slot(id, Trigger::Every, source, slice, Output::Headline);
        let Some(_permit) = slot.await else {
            return Step::Done;
        };
        let kind = Output::Headline.kind();
        let made = generate_headline(summarizer, id, &slice.items, self.deps.timeout, kind);
        match made.await {
            HeadlineMade::Made(text) => {
                self.keep_headline(id, epoch, Size::of(slice.turn, &slice.items), text);
                Step::Done
            }
            HeadlineMade::Empty => Step::Empty,
            HeadlineMade::Failed(failure) => {
                self.failed(id, Output::Headline, &failure);
                Step::Failed
            }
        }
    }

    /// Agent `id`'s thread, if herdr still shows it working.
    async fn working_source(&self, id: &AgentId) -> Option<ThreadSource> {
        let agent = match resolve(self.deps.control.as_ref(), id).await {
            Ok(Resolved::Current(agent)) => agent,
            Ok(Resolved::Moved(_) | Resolved::Gone) => return None,
            Err(error) => {
                tracing::debug!(%error, agent = %id, "herdr agent lookup failed");
                return None;
            }
        };
        if agent.agent_status != herdr::AgentStatus::Working {
            return None;
        }
        self.deps
            .threads
            .summary_source(id, &agent)
            .await
            .map(|(_, source)| source)
    }

    /// The turn in progress in `source`'s thread. `None` without one, and
    /// while the thread's current turn is one already seen finished and
    /// has not grown since: herdr reports the agent working before its new
    /// prompt reaches the transcript, and the old turn must not be
    /// described as the new one. A finished turn that grows again is in
    /// progress: task notifications and queued prompts continue a turn
    /// without starting one, which is how a long-running agent (the circus
    /// master) works most of the time.
    async fn read_current(&self, source: &ThreadSource) -> Option<TurnSlice> {
        let session = self.deps.threads.session_of(&source.id)?;
        let slice = self
            .deps
            .threads
            .finished_turn(source.clone(), CURRENT)
            .await?;
        let finished = self.deps.finished.latest(&session);
        let current = finished.is_none_or(|done| done.is_behind(slice.turn, slice.items.len()));
        if !current {
            tracing::debug!(agent = %source.id, turn = slice.turn.0, "status line skipped: the turn is the finished one, not grown");
        }
        current.then_some(slice)
    }

    /// Keeps and announces a new line, unless the agent finished its turn
    /// since the job started (`epoch` moved on).
    fn keep(&self, id: &AgentId, epoch: u64, size: Size, text: String) {
        let line = StatusLine { text, at: now() };
        {
            let mut book = self.book();
            let Some(entry) = book.get_mut(id).filter(|e| e.epoch == epoch) else {
                return;
            };
            entry.line = Some(line.clone());
            entry.made_at = Some(Instant::now());
            entry.baseline = Some(size);
            entry.line_backoff = Backoff::default();
        }
        self.deps.hub.publish(AgentEvent::StatusLineReady {
            agent_id: id.clone(),
            status_line: line,
        });
    }

    /// Keeps and announces a provisional headline of the turn in progress,
    /// now `size`, unless the agent finished its turn since the job
    /// started (`epoch` moved on: its final headline is coming) or the
    /// turn's final headline already covers it.
    fn keep_headline(&self, id: &AgentId, epoch: u64, size: Size, text: String) {
        let Some(session) = self.deps.threads.session_of(id) else {
            return;
        };
        {
            let mut book = self.book();
            let Some(entry) = book.get_mut(id).filter(|e| e.epoch == epoch) else {
                tracing::debug!(agent = %id, turn = size.turn.0, "provisional headline dropped: the turn finished");
                return;
            };
            entry.headline_made_at = Some(Instant::now());
            entry.headline_baseline = Some(size);
            entry.headline_backoff = Backoff::default();
            // Stored while the entry is held, so the turn finishing (which
            // drops the line under this lock) cannot slip in between.
            let stored = lock(&self.deps.headlines).put_provisional(
                &session,
                size.turn,
                text.clone(),
                size.items,
            );
            if !stored {
                tracing::debug!(agent = %id, turn = size.turn.0, "provisional headline dropped: the final headline is current");
                return;
            }
        }
        self.deps.hub.publish(AgentEvent::HeadlineReady {
            agent_id: id.clone(),
            turn: size.turn,
            headline: text,
            provisional: true,
        });
    }

    /// Records a failed generation of `output`: a usage limit pauses every
    /// generation, anything else backs this agent's `output` off.
    fn failed(&self, id: &AgentId, output: Output, failure: &Failure) {
        let wait = {
            let mut book = self.book();
            let entry = book.get_mut(id);
            let attempts = entry
                .as_ref()
                .map_or(1, |e| e.backoff_of(output).failures.saturating_add(1));
            let wait = self
                .deps
                .pacing
                .after_failure(attempts, failure.usage_limit);
            if let Some(entry) = entry.filter(|_| !failure.usage_limit) {
                *entry.backoff(output) = Backoff {
                    failures: attempts,
                    retry_at: Some(Instant::now() + wait),
                };
            }
            wait
        };
        tracing::warn!(error = %failure.message, agent = %id, retry_in = ?wait, "{} generation failed", output.what());
    }
}

/// Whether `agent` gets status lines: one that may have a transcript (see
/// [`has_transcript`]) that is working.
fn is_working_with_transcript(agent: &Agent) -> bool {
    agent.status == AgentStatus::Working && has_transcript(agent)
}

/// The entry for `id`, made if needed. `None` only when the book is full
/// of agents with jobs.
fn entry<'a>(book: &'a mut HashMap<AgentId, Entry>, id: &AgentId) -> Option<&'a mut Entry> {
    if !book.contains_key(id) && book.len() >= MAX_AGENTS {
        let idle = book
            .iter()
            .filter(|(_, entry)| entry.job.is_none())
            .min_by_key(|(_, entry)| entry.made_at)
            .map(|(id, _)| id.clone())?;
        book.remove(&idle);
    }
    Some(book.entry(id.clone()).or_default())
}

#[cfg(test)]
mod tests {
    use protocol::AgentKind;
    use protocol::{Seq, ThreadContent, ThreadCursor, Timestamp};

    use super::*;

    fn size(turn: u32, items: usize, words: usize) -> Size {
        Size {
            turn: TurnIndex(turn),
            items,
            words,
        }
    }

    #[test]
    fn periodic_lines_wait_for_meaningful_growth() {
        let before = Some(size(2, 10, 400));
        assert!(!size(2, 10, 400).grew(before, Trigger::Every));
        assert!(!size(2, 12, 499).grew(before, Trigger::Every));
        assert!(size(2, 13, 400).grew(before, Trigger::Every));
        assert!(size(2, 11, 500).grew(before, Trigger::Every));
        // A new turn, or none made yet, counts from zero.
        assert!(size(3, 3, 0).grew(before, Trigger::Every));
        assert!(!size(3, 2, 10).grew(before, Trigger::Every));
        assert!(!size(1, 1, 20).grew(None, Trigger::Every));
        assert!(size(1, 1, 120).grew(None, Trigger::Every));
    }

    #[test]
    fn a_tap_needs_only_some_change() {
        let before = Some(size(2, 10, 400));
        assert!(!size(2, 10, 400).grew(before, Trigger::Tap));
        assert!(size(2, 11, 400).grew(before, Trigger::Tap));
        assert!(size(3, 1, 0).grew(before, Trigger::Tap));
        assert!(size(1, 1, 0).grew(None, Trigger::Tap));
        assert!(!size(1, 0, 0).grew(None, Trigger::Tap));
    }

    fn settings() -> StatusSettings {
        StatusSettings::default()
    }

    #[test]
    fn an_agent_is_due_after_the_interval_unless_busy_or_backing_off() {
        let settings = settings();
        let every = settings.every;
        let now = Instant::now();
        let mut entry = Entry::default();
        assert!(entry.due(now, &settings));
        entry.made_at = Some(now);
        // Its headline is due too, until made.
        assert!(entry.due(now, &settings));
        entry.headline_made_at = Some(now + every);
        assert!(!entry.due(now + Duration::from_mins(2), &settings));
        assert!(entry.due(now + every, &settings));
        entry.job = Some(1);
        assert!(!entry.due(now + every, &settings));
        entry.job = None;
        entry.line_backoff.retry_at = Some(now + Duration::from_mins(5));
        assert!(!entry.due(now + every, &settings));
        assert!(entry.due(now + Duration::from_mins(5), &settings));
        // A check that found nothing waits a third of the interval.
        entry.checked_at = Some(now + Duration::from_mins(5));
        assert!(!entry.due(now + Duration::from_mins(5), &settings));
        assert!(entry.due(now + Duration::from_mins(6), &settings));
    }

    #[test]
    fn a_provisional_headline_has_its_own_interval_and_growth() {
        let settings = settings();
        let now = Instant::now();
        let mut entry = Entry {
            made_at: Some(now),
            baseline: Some(size(2, 10, 400)),
            headline_made_at: Some(now),
            headline_baseline: Some(size(2, 4, 100)),
            ..Entry::default()
        };
        let grown = size(2, 13, 400);
        let wants = |entry: &Entry, at, trigger| entry.wants(grown, trigger, at, &settings);
        // Within both intervals: nothing, and the check is not due.
        assert_eq!(wants(&entry, now, Trigger::Every), Wants::default());
        assert!(!entry.due(now + settings.every / 2, &settings));
        // The line's interval passed (3 min), not the headline's (5).
        let at = now + settings.every;
        assert_eq!(
            wants(&entry, at, Trigger::Every),
            Wants {
                line: true,
                headline: false
            }
        );
        let at = now + settings.headline_every;
        assert!(entry.due(at, &settings));
        assert_eq!(
            wants(&entry, at, Trigger::Every),
            Wants {
                line: true,
                headline: true
            }
        );
        // The headline's growth counts from its own baseline.
        entry.headline_baseline = Some(size(2, 12, 400));
        assert!(!wants(&entry, at, Trigger::Every).headline);
        // A tap makes the line at once, and the headline only when due:
        // it neither skips the headline's interval nor drops it.
        entry.headline_baseline = None;
        assert_eq!(
            wants(&entry, now, Trigger::Tap),
            Wants {
                line: true,
                headline: false
            }
        );
        assert!(wants(&entry, at, Trigger::Tap).headline);
        // The turn finishing drops the line but keeps the headline's
        // throttle: a long-running agent stops often.
        entry.drop_line();
        assert_eq!(entry.headline_made_at, Some(now));
        assert!(!wants(&entry, now, Trigger::Every).headline);
    }

    #[test]
    fn a_finished_turn_takes_its_backoff_with_it() {
        let now = Instant::now();
        let mut entry = Entry {
            line: Some(StatusLine {
                text: "old".into(),
                at: Timestamp::default(),
            }),
            made_at: Some(now),
            line_backoff: Backoff {
                failures: 5,
                retry_at: Some(now + Duration::from_mins(30)),
            },
            checked_at: Some(now),
            ..Entry::default()
        };
        entry.drop_line();
        assert_eq!(entry.line, None);
        assert_eq!(entry.line_backoff, Backoff::default());
        assert!(entry.due(now, &settings()));
        assert_eq!(entry.epoch, 1);
    }

    #[test]
    fn the_line_and_the_headline_back_off_apart() {
        let settings = settings();
        let now = Instant::now();
        let failing = Backoff {
            failures: 2,
            retry_at: Some(now + Duration::from_mins(10)),
        };
        let grown = size(1, 20, 900);
        // A failing headline does not hold the line back ...
        let entry = Entry {
            headline_backoff: failing,
            ..Entry::default()
        };
        assert!(entry.due(now, &settings));
        let wants = entry.wants(grown, Trigger::Every, now, &settings);
        assert_eq!(
            wants,
            Wants {
                line: true,
                headline: false
            }
        );
        // ... nor a failing line the headline.
        let entry = Entry {
            line_backoff: failing,
            ..Entry::default()
        };
        assert!(entry.due(now, &settings));
        let wants = entry.wants(grown, Trigger::Every, now, &settings);
        assert_eq!(
            wants,
            Wants {
                line: false,
                headline: true
            }
        );
        // Both failing: nothing is due until one expires.
        let mut entry = Entry {
            line_backoff: failing,
            headline_backoff: failing,
            ..Entry::default()
        };
        assert!(!entry.due(now, &settings));
        assert!(entry.due(now + Duration::from_mins(10), &settings));
        // The turn finishing clears the line's backoff, not the headline's.
        entry.drop_line();
        assert_eq!(entry.headline_backoff, failing);
    }

    #[test]
    fn the_book_is_bounded_but_keeps_agents_with_jobs() {
        let id = |n: usize| -> AgentId { format!("w1:p{n}").parse().expect("id") };
        let mut book = HashMap::new();
        for n in 0..MAX_AGENTS {
            entry(&mut book, &id(n)).expect("room").job = (n > 0).then_some(1);
        }
        assert!(entry(&mut book, &id(MAX_AGENTS)).is_some());
        assert_eq!(book.len(), MAX_AGENTS);
        assert!(!book.contains_key(&id(0)), "the idle one goes");
        // Every remaining one has a job, except the newcomer.
        entry(&mut book, &id(MAX_AGENTS)).expect("kept").job = Some(1);
        assert!(entry(&mut book, &id(MAX_AGENTS + 1)).is_none());
    }

    fn agent(status: AgentStatus, kind: AgentKind) -> Agent {
        Agent {
            id: "w1:p1~5f0c6a1e".parse().expect("agent id"),
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

    #[test]
    fn only_working_agents_with_a_transcript_get_lines() {
        let lines = |status, kind| is_working_with_transcript(&agent(status, kind));
        assert!(lines(AgentStatus::Working, AgentKind::Claude));
        assert!(!lines(AgentStatus::Blocked, AgentKind::Claude));
        assert!(lines(AgentStatus::Working, AgentKind::Codex));
        assert!(!lines(AgentStatus::Working, AgentKind::Pi));
        // Without a session in the id the transcript is matched by folder.
        for kind in [AgentKind::Claude, AgentKind::Codex] {
            let mut bare = agent(AgentStatus::Working, kind);
            bare.id = "w1:p1".parse().expect("id");
            assert!(is_working_with_transcript(&bare));
        }
    }

    #[test]
    fn size_counts_items_and_output_words() {
        let item = |content| ThreadItem {
            cursor: ThreadCursor(1),
            turn: TurnIndex(4),
            rev: 1,
            at: None,
            content,
        };
        let items = [
            item(ThreadContent::User {
                text: "go on".into(),
            }),
            item(ThreadContent::Assistant {
                text: "three more words".into(),
            }),
        ];
        assert_eq!(Size::of(TurnIndex(4), &items), size(4, 2, 3));
    }
}
