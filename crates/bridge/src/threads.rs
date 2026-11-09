//! Agent threads from agent transcripts (`GET /v1/agents/:id/thread`, and
//! `thread_items` events on the stream).
//!
//! The transcript source depends on the agent kind ([`SourceKind`]): a
//! Claude agent's session transcript under `~/.claude/projects`, or a
//! Codex agent's rollout under `~/.codex/sessions`. When herdr reports the
//! agent's session id, that names the file (an id that names no file gets
//! no thread). When it reports none (a Claude agent running without hooks,
//! as the swarm's do), the transcript is **matched by folder**: the
//! session most recently written in the agent's cwd ([`FolderLookup`]),
//! or the earliest Codex session started there since herdr detected the
//! agent ([`Hub::arrived`], less [`ARRIVAL_SLACK`]; the latest for an
//! agent already running when the bridge subscribed). A match is kept
//! while it lives, so it never flips between two sessions in one folder,
//! and is replaced only once it has stopped growing and a session started
//! after its last write appears (`/clear`, `/new` or a restart in the same
//! pane). The lookup is shared by an agent's readers and kept while the
//! agent is registered. Thread pages say when their transcript was matched
//! by folder ([`ThreadPage::inferred`]).
//!
//! # Agent ids of agents matched by folder
//!
//! A matched session never becomes part of the [`AgentId`]: the id stays
//! herdr's (the pane alone), so it is stable for the app while the match
//! is made or replaced, and the stale guard on actions never depends on a
//! guess. The matched session lives in a side map instead
//! ([`Threads::session_of`]): what TL;DRs, headlines and status lines of
//! the agent are keyed on, as herdr's session id is for other agents
//! ([`Threads::summary_source`]). A replaced match changes that key, so
//! the new session's turns are never mixed with the old one's.
//!
//! One [`TranscriptReader`] per agent, created on the first thread request
//! and kept in a bounded map: at most [`ThreadSettings::max_readers`]
//! (least recently requested evicted first), and dropped once not requested
//! for [`ThreadSettings::idle_ttl`] or once the agent leaves the registry. A
//! poller refreshes the held readers every [`ThreadSettings::poll_every`]
//! and publishes what changed. Whoever refreshes a reader (poller or HTTP
//! request) publishes its changes, so none are lost between the two.
//!
//! TL;DRs read finished turns of agents nobody may be following. Those
//! use a separate, smaller set of *quiet* readers: never polled, never
//! published, and never evicting a reader a client relies on, but kept
//! (bounded, and dropped like the others) so each read takes only what
//! was appended and the transcript lookup stays throttled.
//!
//! Transcript I/O is blocking and runs on the blocking pool.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{
    AgentEvent, AgentId, SessionId, ThreadContent, ThreadCursor, ThreadEpoch, ThreadItem,
    ThreadPage, TurnIndex,
};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use transcripts::{
    CodexLocator, CodexLookup, CodexSession, DEFAULT_SCAN_INTERVAL, FolderLookup, Format, Locator,
    Lookup, Refresh, Scope, TranscriptReader,
};

use crate::registry::Hub;
use crate::shutdown::Shutdown;

/// Most items in one `thread_items` event; a client fetches the rest from
/// the event's resume cursor.
const MAX_EVENT_ITEMS: usize = 100;

/// How long before herdr reported a Codex agent its session may have
/// started (the `session_meta` timestamp is the process's start, a moment
/// before herdr recognizes the pane). Kept short: an older session of the
/// same cwd started within it would be taken for the agent's own.
const ARRIVAL_SLACK: Duration = Duration::from_secs(30);

/// Most quiet readers (for TL;DRs of agents nobody follows) held at once.
const MAX_QUIET_READERS: usize = 8;

/// Pacing and bounds for thread readers.
#[derive(Debug, Clone)]
pub struct ThreadSettings {
    /// Claude's projects directory; `None` serves Claude threads empty.
    pub projects_dir: Option<PathBuf>,
    /// Codex's sessions directory; `None` serves Codex threads empty.
    pub codex_sessions_dir: Option<PathBuf>,
    /// How often held readers are refreshed for pushes.
    pub poll_every: Duration,
    /// Least time between two searches for one agent's transcript (a
    /// directory listing; for one matched by folder, a check for a session
    /// that replaced it).
    pub scan_every: Duration,
    /// Most readers held at once.
    pub max_readers: usize,
    /// A reader not requested for this long is dropped.
    pub idle_ttl: Duration,
    /// Most items per page (a client's `limit` is capped to it).
    pub page_limit: usize,
}

impl ThreadSettings {
    /// Defaults: refresh every second, search every
    /// [`DEFAULT_SCAN_INTERVAL`], 16 readers, 15 minutes idle, 500 items,
    /// no Codex sessions directory.
    #[must_use]
    pub fn new(projects_dir: Option<PathBuf>) -> Self {
        Self {
            projects_dir,
            codex_sessions_dir: None,
            poll_every: Duration::from_secs(1),
            scan_every: DEFAULT_SCAN_INTERVAL,
            max_readers: 16,
            idle_ttl: Duration::from_mins(15),
            page_limit: 500,
        }
    }
}

/// Which kind of transcript an agent writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A Claude Code session transcript.
    Claude,
    /// A Codex CLI rollout.
    Codex,
}

impl SourceKind {
    /// The source for an agent kind, if it has one.
    #[must_use]
    pub fn of(kind: protocol::AgentKind) -> Option<Self> {
        match kind {
            protocol::AgentKind::Claude => Some(Self::Claude),
            protocol::AgentKind::Codex => Some(Self::Codex),
            _ => None,
        }
    }

    /// The source for a herdr agent kind (`claude`, `codex`, …).
    #[must_use]
    pub fn of_herdr(kind: Option<&str>) -> Option<Self> {
        Self::of(crate::registry::agent_kind(kind))
    }
}

/// What the thread endpoint needs to know about an agent.
#[derive(Debug, Clone)]
pub struct ThreadSource {
    /// The agent (pane and session).
    pub id: AgentId,
    /// Where its transcript comes from.
    pub kind: SourceKind,
    /// The agent's working directory, for locating the transcript.
    pub cwd: PathBuf,
}

impl ThreadSource {
    /// Where `agent`'s thread comes from, if it has one: a Claude or Codex
    /// agent with an absolute working directory. With no session in `id`,
    /// its transcript is matched by folder (see the [module docs](self)).
    /// Other agents have no transcript.
    #[must_use]
    pub fn for_agent(id: AgentId, agent: &herdr::Agent) -> Option<Self> {
        let cwd = agent
            .cwd
            .as_ref()
            .or(agent.foreground_cwd.as_ref())
            .filter(|cwd| cwd.is_absolute())?;
        let kind = SourceKind::of_herdr(agent.kind.as_deref())?;
        Some(Self {
            id,
            kind,
            cwd: cwd.clone(),
        })
    }
}

/// Whether `agent` (as the registry lists it) may have a transcript: a
/// Claude or Codex agent, with or without a session id (see
/// [`ThreadSource::for_agent`]). The cheap test for which agents get
/// model-written summaries; which session they are keyed on is
/// [`Threads::summary_source`]'s answer.
#[must_use]
pub fn has_transcript(agent: &protocol::Agent) -> bool {
    SourceKind::of(agent.kind).is_some()
}

/// The held items of one finished turn: what a TL;DR summarizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSlice {
    /// Which turn.
    pub turn: TurnIndex,
    /// The latest finished turn (`turn` itself unless an earlier one was
    /// asked for).
    pub latest: TurnIndex,
    /// Its items, in cursor order.
    pub items: Vec<ThreadItem>,
}

/// Which finished turn of a thread to read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnQuery {
    /// The agent is idle or done, so the thread's current turn has
    /// finished.
    pub ready: bool,
    /// The latest turn seen finished earlier (while the agent was ready).
    /// Covers the moment after a new prompt when herdr already reports the
    /// agent working but the prompt is not in the transcript yet.
    pub known_finished: Option<TurnIndex>,
    /// A specific turn; `None` for the latest finished one.
    pub wanted: Option<TurnIndex>,
}

/// The finished turn `query` asks for, given the thread's `current` turn.
/// The current turn is finished only once the agent is ready; before that
/// the latest finished one is the one before it, or the one known to have
/// finished if that is later (the new prompt has not reached the
/// transcript). Turn 0 (what came before the first prompt) is never a turn
/// to summarize.
#[must_use]
pub fn finished_turn(current: Option<TurnIndex>, query: TurnQuery) -> Option<TurnIndex> {
    let current = current?;
    let latest = if query.ready {
        current
    } else {
        let previous = current.0.checked_sub(1).map(TurnIndex);
        let known = query.known_finished.filter(|known| *known <= current);
        previous.max(known)?
    };
    let turn = query.wanted.unwrap_or(latest);
    (turn.0 > 0 && turn <= latest).then_some(turn)
}

/// The [`TurnSlice`] `query` asks for in `thread`, if there is one.
fn slice_of(thread: &transcripts::Thread, query: TurnQuery) -> Option<TurnSlice> {
    let current = thread.current_turn();
    let turn = finished_turn(current, query)?;
    let latest = finished_turn(
        current,
        TurnQuery {
            wanted: None,
            ..query
        },
    )?;
    Some(TurnSlice {
        turn,
        latest,
        items: thread.turn(turn).to_vec(),
    })
}

/// An agent's latest message ([`Threads::latest_message`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestMessage {
    /// The turn it belongs to.
    pub turn: TurnIndex,
    /// Its text (Markdown): consecutive assistant items joined by a blank
    /// line.
    pub text: String,
}

/// The last run of assistant items in `thread`'s current turn; items of
/// unknown kinds between them are skipped, anything else ends the run.
fn latest_message(thread: &transcripts::Thread) -> Option<LatestMessage> {
    let turn = thread.current_turn()?;
    let mut parts: Vec<&str> = Vec::new();
    for item in thread.turn(turn).iter().rev() {
        match &item.content {
            ThreadContent::Assistant { text } => parts.push(text),
            ThreadContent::Unknown => {}
            _ if parts.is_empty() => {}
            _ => break,
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some(LatestMessage {
        turn,
        text: parts.join("\n\n"),
    })
}

/// The per-agent readers.
#[derive(Debug)]
pub struct Threads {
    settings: ThreadSettings,
    locator: Option<Arc<Locator>>,
    codex: Option<Arc<CodexLocator>>,
    /// Random per process, so a restart changes every epoch.
    boot: String,
    hub: Arc<Hub>,
    /// Readers clients follow: polled, and their changes published.
    slots: Mutex<HashMap<AgentId, Slot>>,
    /// Quiet readers, for TL;DRs only (see the [module docs](self)).
    quiet: Mutex<HashMap<AgentId, Slot>>,
    /// Each agent's transcript lookup, shared by its readers so they agree
    /// on a transcript matched by folder, and kept while the agent is
    /// registered so the match stays stable when readers come and go.
    lookups: Mutex<HashMap<AgentId, Arc<Mutex<SourceLookup>>>>,
    /// The session of each agent without one in its id whose transcript
    /// was matched by folder: what its summaries are keyed on.
    matched: Mutex<HashMap<AgentId, SessionId>>,
}

#[derive(Debug)]
struct Slot {
    entry: Arc<Mutex<Entry>>,
    last_used: Instant,
}

#[derive(Debug)]
struct Entry {
    id: AgentId,
    lookup: Arc<Mutex<SourceLookup>>,
    reader: Option<TranscriptReader>,
    /// The reader's transcript was matched by folder.
    inferred: bool,
}

/// Where an agent's transcript was found.
#[derive(Debug)]
struct Located {
    path: PathBuf,
    /// Matched by folder: herdr named no session.
    inferred: bool,
    /// When matched by folder, its session (from the file name), if that
    /// is a protocol session id.
    session: Option<SessionId>,
}

/// Finds one agent's transcript, by kind and by whether herdr names its
/// session.
#[derive(Debug)]
enum SourceLookup {
    /// A Claude session named by herdr.
    Claude(Lookup, Arc<Locator>),
    /// A Claude session matched by folder.
    ClaudeFolder(FolderLookup, Arc<Locator>),
    /// A Codex rollout, named by herdr or matched by folder.
    Codex {
        lookup: CodexLookup,
        locator: Arc<CodexLocator>,
        by_folder: bool,
    },
}

impl SourceLookup {
    /// The lookup for `source` with its kind's locator; `None` without one
    /// (no directory configured) or when herdr's session id is not a valid
    /// file-name id. An agent is matched by folder only when herdr reports
    /// **no** session: one with an unusable id gets no thread rather than
    /// a guessed one. `arrived` is when herdr detected the agent, if known.
    fn new(
        source: &ThreadSource,
        claude: Option<&Arc<Locator>>,
        codex: Option<&Arc<CodexLocator>>,
        arrived: Option<protocol::Timestamp>,
        scan_every: Duration,
    ) -> Option<Self> {
        let session = match source.id.session() {
            Some(session) => Some(transcripts::SessionId::new(session.as_str()).ok()?),
            None => None,
        };
        let since = arrived.map(|at| {
            let slack = u64::try_from(ARRIVAL_SLACK.as_millis()).unwrap_or(u64::MAX);
            at.unix_millis().saturating_sub(slack)
        });
        match (source.kind, session) {
            (SourceKind::Claude, Some(session)) => Some(Self::Claude(
                Lookup::new(&source.cwd, session).with_interval(scan_every),
                Arc::clone(claude?),
            )),
            (SourceKind::Claude, None) => {
                let not_before = since.map(|ms| UNIX_EPOCH + Duration::from_millis(ms));
                Some(Self::ClaudeFolder(
                    FolderLookup::new(&source.cwd, not_before).with_interval(scan_every),
                    Arc::clone(claude?),
                ))
            }
            (SourceKind::Codex, session) => {
                let by_folder = session.is_none();
                let wanted = session.map_or_else(
                    || CodexSession::LatestIn {
                        cwd: source.cwd.clone(),
                        not_before: since.map(protocol::Timestamp::from_unix_millis),
                    },
                    CodexSession::Id,
                );
                Some(Self::Codex {
                    lookup: CodexLookup::new(wanted).with_interval(scan_every),
                    locator: Arc::clone(codex?),
                    by_folder,
                })
            }
        }
    }

    fn format(&self) -> Format {
        match self {
            Self::Claude(..) | Self::ClaudeFolder(..) => Format::Claude,
            Self::Codex { .. } => Format::Codex,
        }
    }

    /// The transcript, as found so far (searches are throttled; once found
    /// it is kept, see [`FolderLookup`] and [`CodexLookup`] for when a
    /// match is replaced). Blocking.
    fn poll(&mut self) -> transcripts::Result<Option<Located>> {
        let now = std::time::Instant::now();
        let session = |id: &transcripts::SessionId| SessionId::new(id.as_str()).ok();
        Ok(match self {
            Self::Claude(lookup, locator) => lookup.poll(locator, now)?.map(|path| Located {
                path,
                inferred: false,
                session: None,
            }),
            Self::ClaudeFolder(lookup, locator) => lookup
                .poll(locator, now, SystemTime::now())?
                .map(|found| Located {
                    session: session(&found.session),
                    path: found.path,
                    inferred: true,
                }),
            Self::Codex {
                lookup,
                locator,
                by_folder,
            } => lookup.poll(locator, now)?.map(|path| Located {
                session: by_folder
                    .then(|| CodexLocator::session_of(&path))
                    .flatten()
                    .as_ref()
                    .and_then(session),
                inferred: *by_folder,
                path,
            }),
        })
    }
}

impl Threads {
    /// Readers publishing to `hub`.
    #[must_use]
    pub fn new(settings: ThreadSettings, hub: Arc<Hub>) -> Self {
        let locator = settings
            .projects_dir
            .clone()
            .map(|dir| Arc::new(Locator::new(dir)));
        let codex = settings
            .codex_sessions_dir
            .clone()
            .map(|dir| Arc::new(CodexLocator::new(dir)));
        let boot = format!(
            "{:016x}",
            RandomState::new().hash_one((std::process::id(), SystemTime::now()))
        );
        Self {
            settings,
            locator,
            codex,
            boot,
            hub,
            slots: Mutex::new(HashMap::new()),
            quiet: Mutex::new(HashMap::new()),
            lookups: Mutex::new(HashMap::new()),
            matched: Mutex::new(HashMap::new()),
        }
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<AgentId, Slot>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn quiet(&self) -> MutexGuard<'_, HashMap<AgentId, Slot>> {
        self.quiet.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lookups(&self) -> MutexGuard<'_, HashMap<AgentId, Arc<Mutex<SourceLookup>>>> {
        self.lookups.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn matched(&self) -> MutexGuard<'_, HashMap<AgentId, SessionId>> {
        self.matched.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The session agent `id`'s summaries are keyed on: herdr's, else the
    /// session of the transcript matched by folder, once one is (no I/O).
    #[must_use]
    pub fn session_of(&self, id: &AgentId) -> Option<SessionId> {
        id.session()
            .cloned()
            .or_else(|| self.matched().get(id).cloned())
    }

    /// Where `agent`'s summaries come from: its thread, and the session
    /// they are keyed on ([`Self::session_of`]). An agent without a session
    /// in its id whose transcript is not matched yet is looked up now.
    /// `None` without a thread or a session.
    pub async fn summary_source(
        self: &Arc<Self>,
        id: &AgentId,
        agent: &herdr::Agent,
    ) -> Option<(SessionId, ThreadSource)> {
        let source = ThreadSource::for_agent(id.clone(), agent)?;
        if let Some(session) = self.session_of(id) {
            return Some((session, source));
        }
        let this = Arc::clone(self);
        let looking = source.clone();
        let looked = tokio::task::spawn_blocking(move || {
            let lookup = this.lookup(&looking)?;
            let mut lookup = lookup.lock().unwrap_or_else(PoisonError::into_inner);
            this.locate(&looking.id, &mut lookup)
        })
        .await;
        if let Err(error) = looked {
            tracing::error!(%error, "transcript lookup task failed");
        }
        Some((self.session_of(id)?, source))
    }

    /// The shared lookup for `source`'s agent, created if needed. `None`
    /// when its transcript cannot be located (see [`SourceLookup::new`]).
    fn lookup(&self, source: &ThreadSource) -> Option<Arc<Mutex<SourceLookup>>> {
        let mut lookups = self.lookups();
        if let Some(lookup) = lookups.get(&source.id) {
            return Some(Arc::clone(lookup));
        }
        let lookup = Arc::new(Mutex::new(SourceLookup::new(
            source,
            self.locator.as_ref(),
            self.codex.as_ref(),
            self.hub.arrived(&source.id),
            self.settings.scan_every,
        )?));
        lookups.insert(source.id.clone(), Arc::clone(&lookup));
        Some(lookup)
    }

    /// Polls agent `id`'s `lookup` and notes a session matched by folder.
    /// Blocking.
    fn locate(&self, id: &AgentId, lookup: &mut SourceLookup) -> Option<Located> {
        let located = match lookup.poll() {
            Ok(located) => located,
            Err(error) => {
                tracing::debug!(%error, agent = %id, "cannot locate transcript");
                None
            }
        };
        let session = located.as_ref().and_then(|l| l.session.clone());
        let mut matched = self.matched();
        match session {
            Some(session) => matched.insert(id.clone(), session),
            None => matched.remove(id),
        };
        located
    }

    /// Finds `entry`'s transcript (throttled; a new one replaces the
    /// reader) and reads what was appended. `None` while there is no
    /// transcript; else whether the reader is new, and the read. Blocking.
    fn advance(&self, entry: &mut Entry) -> Option<(bool, transcripts::Result<Refresh>)> {
        let (located, format) = {
            let mut lookup = entry.lookup.lock().unwrap_or_else(PoisonError::into_inner);
            (self.locate(&entry.id, &mut lookup), lookup.format())
        };
        let Some(located) = located else {
            entry.reader = None;
            return None;
        };
        let fresh = entry
            .reader
            .as_ref()
            .is_none_or(|reader| reader.path() != located.path);
        if fresh {
            entry.reader =
                Some(TranscriptReader::new(located.path, Scope::Main).with_format(format));
            entry.inferred = located.inferred;
        }
        let reader = entry.reader.as_mut()?;
        Some((fresh, reader.refresh()))
    }

    /// The epoch of a thread with no transcript (agents of other kinds, or
    /// one not written yet). Once a transcript appears its epoch differs, so a
    /// client drops the empty cache.
    #[must_use]
    pub fn empty_page(&self) -> ThreadPage {
        ThreadPage {
            epoch: ThreadEpoch(format!("{}-empty", self.boot)),
            items: Vec::new(),
            resume_cursor: ThreadCursor(0),
            truncated_before: None,
            has_more: false,
            headlines: Vec::new(),
            inferred: false,
        }
    }

    /// A page of `source`'s thread: items after `since`, at most `limit`
    /// (capped to [`ThreadSettings::page_limit`]).
    pub async fn page(
        self: &Arc<Self>,
        source: ThreadSource,
        since: ThreadCursor,
        limit: Option<usize>,
    ) -> ThreadPage {
        let Some(entry) = self.entry(&source) else {
            return self.empty_page();
        };
        let limit = limit
            .unwrap_or(self.settings.page_limit)
            .clamp(1, self.settings.page_limit);
        let this = Arc::clone(self);
        let page = tokio::task::spawn_blocking(move || {
            let mut entry = entry.lock().unwrap_or_else(PoisonError::into_inner);
            this.refresh(&mut entry, false);
            entry.reader.as_ref().map(|reader| {
                let thread = reader.thread();
                ThreadPage {
                    inferred: entry.inferred,
                    ..thread.page(thread.epoch(&this.boot), since, limit)
                }
            })
        })
        .await;
        match page {
            Ok(Some(page)) => page,
            Ok(None) => self.empty_page(),
            Err(error) => {
                tracing::error!(%error, "thread read task failed");
                self.empty_page()
            }
        }
    }

    /// The items of a finished turn of `source`'s thread, read up to date:
    /// see [`finished_turn`] for which. `None` without a transcript or such
    /// a turn.
    pub async fn finished_turn(
        self: &Arc<Self>,
        source: ThreadSource,
        query: TurnQuery,
    ) -> Option<TurnSlice> {
        self.read(source, move |thread| slice_of(thread, query))
            .await
    }

    /// The agent's latest message: the last run of assistant items in the
    /// thread's current turn (one reply can be written as several), read
    /// up to date. `None` without a transcript, or when the current turn
    /// has no assistant text yet (the user has spoken since the last
    /// reply).
    pub async fn latest_message(self: &Arc<Self>, source: ThreadSource) -> Option<LatestMessage> {
        self.read(source, latest_message).await
    }

    /// `read` applied to `source`'s thread, brought up to date. Uses the
    /// agent's reader if one is held and open (a client is following the
    /// thread), else a quiet reader (see the [module docs](self)).
    async fn read<T: Send + 'static>(
        self: &Arc<Self>,
        source: ThreadSource,
        read: impl Fn(&transcripts::Thread) -> Option<T> + Send + 'static,
    ) -> Option<T> {
        let held = self
            .slots()
            .get(&source.id)
            .map(|slot| Arc::clone(&slot.entry));
        let this = Arc::clone(self);
        let found = tokio::task::spawn_blocking(move || {
            if let Some(entry) = held {
                let mut entry = entry.lock().unwrap_or_else(PoisonError::into_inner);
                this.refresh(&mut entry, false);
                // A held slot may not have found its transcript yet (the
                // lookup is throttled): then try the quiet reader.
                if let Some(reader) = &entry.reader {
                    return read(reader.thread());
                }
            }
            let entry = this.quiet_entry(&source)?;
            let mut entry = entry.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(Err(error)) = this.advance(&mut entry).map(|(_, read)| read) {
                tracing::debug!(%error, agent = %source.id, "transcript read failed");
            }
            entry
                .reader
                .as_ref()
                .and_then(|reader| read(reader.thread()))
        })
        .await;
        found.unwrap_or_else(|error| {
            tracing::error!(%error, "thread read task failed");
            None
        })
    }

    /// The entry for `source`, created (and the least recently used one
    /// evicted) if needed. `None` when transcripts cannot be located.
    fn entry(&self, source: &ThreadSource) -> Option<Arc<Mutex<Entry>>> {
        let (entry, created) =
            self.entry_in(&mut self.slots(), source, self.settings.max_readers)?;
        if created {
            // The held reader supersedes a quiet one.
            self.quiet().remove(&source.id);
        }
        Some(entry)
    }

    /// The quiet entry for `source`, created if needed.
    fn quiet_entry(&self, source: &ThreadSource) -> Option<Arc<Mutex<Entry>>> {
        self.entry_in(&mut self.quiet(), source, MAX_QUIET_READERS)
            .map(|(entry, _)| entry)
    }

    /// The entry for `source` in `slots`, and whether it was just created
    /// (evicting the least recently used one beyond `max`). `None` when
    /// transcripts cannot be located.
    fn entry_in(
        &self,
        slots: &mut HashMap<AgentId, Slot>,
        source: &ThreadSource,
        max: usize,
    ) -> Option<(Arc<Mutex<Entry>>, bool)> {
        let now = Instant::now();
        if let Some(slot) = slots.get_mut(&source.id) {
            slot.last_used = now;
            return Some((Arc::clone(&slot.entry), false));
        }
        let lookup = self.lookup(source)?;
        while slots.len() >= max.max(1) {
            let oldest = slots
                .iter()
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => slots.remove(&id),
                None => break,
            };
        }
        let entry = Arc::new(Mutex::new(Entry {
            id: source.id.clone(),
            lookup,
            reader: None,
            inferred: false,
        }));
        slots.insert(
            source.id.clone(),
            Slot {
                entry: Arc::clone(&entry),
                last_used: now,
            },
        );
        Some((entry, true))
    }

    /// Finds the transcript if needed, reads what was appended, and
    /// publishes the changes. A newly found transcript is announced (its
    /// epoch, no items) only if `announce_new`: an HTTP caller gets the page
    /// itself. Blocking.
    fn refresh(&self, entry: &mut Entry, announce_new: bool) {
        let Some((fresh, read)) = self.advance(entry) else {
            return;
        };
        let Some(reader) = entry.reader.as_ref() else {
            return;
        };
        match read {
            // A new reader's epoch is new to every client: they refetch
            // from 0 on seeing it, so no items need pushing.
            Ok(_) if fresh => {
                if announce_new {
                    let thread = reader.thread();
                    self.hub.publish(AgentEvent::ThreadItems {
                        agent_id: entry.id.clone(),
                        epoch: thread.epoch(&self.boot),
                        items: Vec::new(),
                        resume_cursor: ThreadCursor(0),
                    });
                }
            }
            Ok(refresh) => {
                if let Some(event) = self.changes(&entry.id, reader, &refresh) {
                    self.hub.publish(event);
                }
            }
            Err(error) => {
                tracing::debug!(%error, agent = %entry.id, "transcript refresh failed");
            }
        }
    }

    fn changes(
        &self,
        id: &AgentId,
        reader: &TranscriptReader,
        refresh: &Refresh,
    ) -> Option<AgentEvent> {
        if refresh.changed.is_empty() && !refresh.reset {
            return None;
        }
        let thread = reader.thread();
        let mut items: Vec<ThreadItem> = refresh
            .changed
            .iter()
            .filter_map(|cursor| thread.get(*cursor).cloned())
            .collect();
        let mut resume_cursor = thread.resume_cursor();
        if items.len() > MAX_EVENT_ITEMS {
            items.truncate(MAX_EVENT_ITEMS);
            if let Some(last) = items.last() {
                resume_cursor = resume_cursor.min(last.cursor);
            }
        }
        Some(AgentEvent::ThreadItems {
            agent_id: id.clone(),
            epoch: thread.epoch(&self.boot),
            items,
            resume_cursor,
        })
    }

    /// Drops idle readers (quiet ones too), and the readers, lookups and
    /// matches of agents no longer registered; returns the rest of the
    /// polled readers.
    fn live_entries(&self) -> Vec<Arc<Mutex<Entry>>> {
        let now = Instant::now();
        let ttl = self.settings.idle_ttl;
        let registered = |id: &AgentId| {
            self.hub
                .agent_in(id.pane())
                .is_some_and(|agent| agent.id == *id)
        };
        let keep = |id: &AgentId, slot: &mut Slot| {
            now.saturating_duration_since(slot.last_used) < ttl && registered(id)
        };
        self.quiet().retain(keep);
        self.lookups().retain(|id, _| registered(id));
        self.matched().retain(|id, _| registered(id));
        let mut slots = self.slots();
        slots.retain(keep);
        slots.values().map(|slot| Arc::clone(&slot.entry)).collect()
    }

    /// Refreshes every held reader each [`ThreadSettings::poll_every`] until
    /// `shutdown`.
    #[must_use]
    pub fn spawn_poller(self: &Arc<Self>, mut shutdown: Shutdown) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(this.settings.poll_every);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    () = shutdown.wait() => break,
                }
                let entries = this.live_entries();
                if entries.is_empty() {
                    continue;
                }
                let worker = Arc::clone(&this);
                let polled = tokio::task::spawn_blocking(move || {
                    for entry in entries {
                        let mut entry = entry.lock().unwrap_or_else(PoisonError::into_inner);
                        worker.refresh(&mut entry, true);
                    }
                })
                .await;
                if let Err(error) = polled {
                    tracing::error!(%error, "thread poll task failed");
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(ready: bool, wanted: Option<u32>) -> TurnQuery {
        TurnQuery {
            ready,
            known_finished: None,
            wanted: wanted.map(TurnIndex),
        }
    }

    #[test]
    fn the_current_turn_is_finished_only_when_ready() {
        let t = |n| Some(TurnIndex(n));
        assert_eq!(finished_turn(None, query(true, None)), None);
        assert_eq!(finished_turn(t(3), query(true, None)), t(3));
        assert_eq!(finished_turn(t(3), query(false, None)), t(2));
        assert_eq!(finished_turn(t(1), query(false, None)), None);
        assert_eq!(finished_turn(t(0), query(true, None)), None);
    }

    #[test]
    fn a_turn_known_finished_wins_while_the_new_prompt_is_not_written() {
        let t = |n| Some(TurnIndex(n));
        let known = |n| TurnQuery {
            ready: false,
            known_finished: t(n),
            wanted: None,
        };
        // Working, but the transcript still ends with finished turn 3.
        assert_eq!(finished_turn(t(3), known(3)), t(3));
        // Turn 4 has started: 3 is the latest either way.
        assert_eq!(finished_turn(t(4), known(3)), t(3));
        // A stale hint never hides a later finished turn.
        assert_eq!(finished_turn(t(6), known(2)), t(5));
        // A hint from another numbering (beyond the transcript) is ignored.
        assert_eq!(finished_turn(t(2), known(9)), t(1));
    }

    #[test]
    fn a_wanted_turn_must_be_finished() {
        let t = |n| Some(TurnIndex(n));
        assert_eq!(finished_turn(t(3), query(false, Some(1))), t(1));
        assert_eq!(finished_turn(t(3), query(false, Some(3))), None);
        assert_eq!(finished_turn(t(3), query(true, Some(3))), t(3));
        assert_eq!(finished_turn(t(3), query(true, Some(0))), None);
        assert_eq!(finished_turn(t(3), query(true, Some(4))), None);
    }

    const SESSION: &str = "7a1d2c3b-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

    fn source(pane: &str, cwd: &std::path::Path) -> ThreadSource {
        ThreadSource {
            id: format!("{pane}~{SESSION}").parse().expect("agent id"),
            kind: SourceKind::Claude,
            cwd: cwd.to_path_buf(),
        }
    }

    fn append(path: &std::path::Path, records: &[serde_json::Value]) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open");
        for record in records {
            writeln!(file, "{record}").expect("write");
        }
    }

    #[tokio::test]
    async fn finished_turns_of_unfollowed_agents_use_a_kept_quiet_reader() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        let projects = tmp.path().join("projects");
        let project = projects.join(transcripts::project_slug(&cwd));
        std::fs::create_dir_all(&project).expect("project dir");
        let transcript = project.join(format!("{SESSION}.jsonl"));
        let user = |text: &str| serde_json::json!({"type": "user", "message": {"content": text}});
        let assistant =
            |text: &str| serde_json::json!({"type": "assistant", "message": {"content": text}});
        append(&transcript, &[user("go"), assistant("done")]);

        let threads = Arc::new(Threads::new(
            ThreadSettings::new(Some(projects)),
            Arc::new(Hub::new(16)),
        ));
        let ready = TurnQuery {
            ready: true,
            ..TurnQuery::default()
        };
        let first = threads
            .finished_turn(source("w1:p1", &cwd), ready)
            .await
            .expect("turn 1");
        assert_eq!((first.turn, first.latest), (TurnIndex(1), TurnIndex(1)));
        assert_eq!(first.items.len(), 2);
        // Kept, quietly: no polled reader for an agent nobody follows.
        assert!(threads.slots().is_empty());
        assert_eq!(threads.quiet().len(), 1);

        // The next read takes what was appended, from the same reader.
        append(&transcript, &[assistant("and the docs"), user("next")]);
        let earlier = TurnQuery {
            wanted: Some(TurnIndex(1)),
            ..ready
        };
        let again = threads
            .finished_turn(source("w1:p1", &cwd), earlier)
            .await
            .expect("turn 1");
        assert_eq!((again.turn, again.latest), (TurnIndex(1), TurnIndex(2)));
        assert_eq!(again.items.len(), 3);
        assert_eq!(threads.quiet().len(), 1);

        // Following the thread replaces the quiet reader.
        let _page = threads
            .page(source("w1:p1", &cwd), ThreadCursor(0), None)
            .await;
        assert!(threads.quiet().is_empty());
        assert_eq!(threads.slots().len(), 1);
    }

    #[tokio::test]
    async fn the_latest_message_is_the_last_reply_of_the_current_turn() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().join("repo");
        let projects = tmp.path().join("projects");
        let project = projects.join(transcripts::project_slug(&cwd));
        std::fs::create_dir_all(&project).expect("project dir");
        let transcript = project.join(format!("{SESSION}.jsonl"));
        let user = |text: &str| serde_json::json!({"type": "user", "message": {"content": text}});
        let assistant =
            |text: &str| serde_json::json!({"type": "assistant", "message": {"content": text}});
        append(&transcript, &[user("go"), assistant("one"), user("again")]);
        let threads = Arc::new(Threads::new(
            ThreadSettings::new(Some(projects)),
            Arc::new(Hub::new(16)),
        ));
        // The user has spoken since the last reply: nothing to offer.
        assert_eq!(threads.latest_message(source("w1:p1", &cwd)).await, None);
        append(
            &transcript,
            &[assistant("first part"), assistant("second part")],
        );
        let latest = threads
            .latest_message(source("w1:p1", &cwd))
            .await
            .expect("a reply");
        assert_eq!(latest.turn, TurnIndex(2));
        assert_eq!(latest.text, "first part\n\nsecond part");
    }

    #[test]
    fn a_codex_session_id_that_names_no_file_gets_no_guessed_thread() {
        let codex = Arc::new(CodexLocator::new("/nonexistent"));
        let source = |id: &str| ThreadSource {
            id: id.parse().expect("agent id"),
            kind: SourceKind::Codex,
            cwd: PathBuf::from("/repo"),
        };
        // herdr reports a session protocol accepts but no rollout can be
        // named for: no lookup at all, never one by cwd.
        let odd = source("w1:p1~codex:1.2");
        assert!(odd.id.session().is_some());
        assert!(SourceLookup::new(&odd, None, Some(&codex), None, DEFAULT_SCAN_INTERVAL).is_none());
        // No session: looked up by cwd.
        let bare = source("w1:p1");
        assert!(matches!(
            SourceLookup::new(&bare, None, Some(&codex), None, DEFAULT_SCAN_INTERVAL),
            Some(SourceLookup::Codex { .. })
        ));
        assert!(SourceLookup::new(&bare, None, None, None, DEFAULT_SCAN_INTERVAL).is_none());
    }

    #[test]
    fn quiet_readers_are_bounded() {
        let threads = Threads::new(
            ThreadSettings::new(Some(PathBuf::from("/nonexistent"))),
            Arc::new(Hub::new(16)),
        );
        for n in 0..=MAX_QUIET_READERS {
            let pane = format!("w1:p{n}");
            assert!(
                threads
                    .quiet_entry(&source(&pane, std::path::Path::new("/repo")))
                    .is_some()
            );
        }
        assert_eq!(threads.quiet().len(), MAX_QUIET_READERS);
    }
}
