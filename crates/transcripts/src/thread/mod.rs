//! Normalizing transcript records into protocol [`ThreadItem`]s.

mod codex;
mod records;
mod said;
mod store;

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};

use protocol::{
    ThreadContent, ThreadCursor, ThreadEpoch, ThreadItem, ThreadPage, Timestamp, ToolStatus,
    TurnIndex,
};
use serde_json::Value;

use crate::diagnostics::{Diagnostics, INVALID_JSON, NO_TYPE};
use crate::diff::Planned;
use crate::record::{Kind, Record};
use crate::render::Limits;

use store::Store;
pub use store::Window;

/// Bumped whenever normalization changes what items a transcript produces.
/// Part of the [`ThreadEpoch`] a bridge hands out, so clients drop caches
/// built by an older parser.
pub const PARSER_VERSION: u32 = 3;

/// Which agent wrote the transcript a [`Thread`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// A Claude Code session transcript (`~/.claude/projects/…`).
    #[default]
    Claude,
    /// A Codex CLI rollout (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`,
    /// codex-cli 0.166): `session_meta`, `response_item` and `event_msg`
    /// records. Only `response_item`s are rendered; see `thread/codex.rs`.
    Codex,
}

/// Which conversation a [`Thread`] shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// A session's own transcript: sidechain (subagent) records are skipped.
    #[default]
    Main,
    /// A subagent transcript (`<session>/subagents/**/agent-*.jsonl`), where
    /// every record is a sidechain record.
    Subagent,
}

/// An agent's thread, built incrementally from transcript lines.
///
/// # Cursors
///
/// Each item gets the next [`ThreadCursor`] when it is first seen, and keeps
/// it: a cursor identifies an item (within a [`generation`](Self::generation)).
/// Items are append-only with two exceptions: a [`ThreadContent::Tool`] item
/// is created `running` and later updated in place with its output and final
/// status, and the [`ThreadContent::Aside`] for a user's shell command gets
/// its output when the next record brings it. Every addition and update is reported once by
/// [`TranscriptReader::refresh`](crate::TranscriptReader::refresh), so a
/// bridge pushes exactly what changed. Clients upsert by cursor and fetch
/// from [`resume_cursor`](Self::resume_cursor), which never moves past a
/// tool call that is still running (see `protocol::thread`).
///
/// A running tool call is settled as [`ToolStatus::Unknown`] when its
/// result can no longer be expected: a new user turn starts, the same
/// `tool_use` id is reused, it falls more than [`Window::max_pending_age`]
/// items behind, more than [`Window::max_pending`] calls are running, or the
/// caller ends the session ([`settle_pending`](Self::settle_pending)).
///
/// # Memory
///
/// At most [`Window::max_items`] items and about [`Window::max_bytes`] of
/// text are kept; older items are dropped and
/// [`truncated_before`](Self::truncated_before) says from where history is
/// complete.
///
/// # Turns
///
/// Turn `n ≥ 1` starts at the user's `n`-th typed message and runs until the
/// next one; items before the first typed message belong to turn 0. Claude
/// Code records much else as user input (see `said`); none of it starts a
/// turn. Subagent reports, other sessions' messages, task notifications and
/// the user's `!` shell commands become [`ThreadContent::Aside`] items in the
/// current turn; tool results,
/// injected context, slash-command records, command and hook output and
/// interruption notices are not shown. Injected blocks are filtered block
/// by block, so typed text sent together with an injected
/// `<system-reminder>` still counts. A prompt queued while the agent was
/// busy (an `attachment` record) is shown in the current turn without
/// starting a new one, since the agent takes it mid-turn.
///
/// Running file-editing calls keep only their planned diff (capped at
/// [`Limits::patch`]), so at most [`Window::max_pending`] times that is held
/// outside the window's byte budget.
#[derive(Debug)]
pub struct Thread {
    format: Format,
    scope: Scope,
    limits: Limits,
    window: Window,
    store: Store,
    turn: Option<TurnIndex>,
    /// Running tool calls by `tool_use` id.
    pending: HashMap<String, PendingTool>,
    /// Codex: the session's working directory (from `session_meta` and
    /// `turn_context`), for relative diff paths.
    cwd: Option<String>,
    /// Codex: which Codex turn each thread turn belongs to.
    codex_turns: codex::Turns,
    /// The user's shell command just pushed, until the next item: its
    /// output arrives in the following record.
    shell: Option<ThreadCursor>,
    /// Bumped on every reset.
    generation: u64,
    /// Random per instance: tells this numbering apart from any other
    /// reader's, e.g. after the agent's session changed.
    identity: u64,
    diagnostics: Diagnostics,
}

#[derive(Debug)]
struct PendingTool {
    cursor: ThreadCursor,
    /// For file-editing tools: what is needed to render the diff, with the
    /// raw input already dropped (bounded by [`Limits::patch`]).
    planned: Option<Planned>,
    /// For Codex `apply_patch` calls: the diffs, already rendered (each
    /// bounded by [`Limits::patch`]), shown once the call succeeds.
    diffs: Vec<ThreadContent>,
    cwd: Option<String>,
}

impl Default for Thread {
    fn default() -> Self {
        Self::new(Scope::Main)
    }
}

impl Thread {
    /// An empty thread with default [`Limits`] and [`Window`].
    #[must_use]
    pub fn new(scope: Scope) -> Self {
        Self {
            format: Format::Claude,
            scope,
            limits: Limits::default(),
            window: Window::default(),
            store: Store::default(),
            turn: None,
            pending: HashMap::new(),
            cwd: None,
            codex_turns: codex::Turns::default(),
            shell: None,
            generation: 0,
            identity: RandomState::new().hash_one(std::time::SystemTime::now()),
            diagnostics: Diagnostics::default(),
        }
    }

    /// Overrides the size caps for rendered text.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Reads lines as `format` (Claude Code by default).
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.format = format;
        self
    }

    /// Overrides the memory bounds.
    #[must_use]
    pub fn with_window(mut self, window: Window) -> Self {
        self.window = window;
        self
    }

    /// Every item held, in cursor order.
    #[must_use]
    pub fn items(&self) -> &[ThreadItem] {
        self.store.items()
    }

    /// The item with this cursor, if still held.
    #[must_use]
    pub fn get(&self, cursor: ThreadCursor) -> Option<&ThreadItem> {
        self.store.get(cursor)
    }

    /// Items with a cursor greater than `since`. `ThreadCursor(0)` returns
    /// everything held.
    #[must_use]
    pub fn since(&self, since: ThreadCursor) -> &[ThreadItem] {
        self.store.since(since)
    }

    /// The response to `GET /v1/agents/:id/thread?since=`: at most `limit`
    /// items. When more remain, `hasMore` is set and `resumeCursor` points
    /// at the last item returned (or earlier, if a tool call before it is
    /// still running).
    #[must_use]
    pub fn page(&self, epoch: ThreadEpoch, since: ThreadCursor, limit: usize) -> ThreadPage {
        let all = self.since(since);
        let items = all.get(..limit).unwrap_or(all);
        let has_more = items.len() < all.len();
        let resume_cursor = match items.last() {
            Some(last) if has_more => self.resume_cursor().min(last.cursor),
            _ => self.resume_cursor(),
        };
        ThreadPage {
            epoch,
            items: items.to_vec(),
            resume_cursor,
            truncated_before: self.truncated_before(),
            has_more,
            // The bridge adds headlines; a transcript has none.
            headlines: Vec::new(),
            inferred: false,
        }
    }

    /// The epoch for this thread's current numbering. It differs between
    /// [`Thread`] instances ([`identity`](Self::identity)), across resets
    /// ([`generation`](Self::generation)) and parser versions; `boot` is the
    /// bridge's own per-process id, so a restart changes it too.
    #[must_use]
    pub fn epoch(&self, boot: &str) -> ThreadEpoch {
        ThreadEpoch(format!(
            "{boot}-{PARSER_VERSION}-{:016x}-{}",
            self.identity, self.generation
        ))
    }

    /// A random id for this thread instance, so a new reader (or a new
    /// transcript for the same agent) never reuses an old epoch.
    #[must_use]
    pub fn identity(&self) -> u64 {
        self.identity
    }

    /// The `since` a client should pass next: the last cursor, or just
    /// before the oldest item that may still change (a running tool call, a
    /// shell command awaiting its output), whichever is lower.
    #[must_use]
    pub fn resume_cursor(&self) -> ThreadCursor {
        self.pending
            .values()
            .map(|p| p.cursor)
            .chain(self.shell)
            .min()
            .map_or_else(|| self.last_cursor(), |c| ThreadCursor(c.0 - 1))
    }

    /// The cursor of the newest item (or of the last item before a reset;
    /// `ThreadCursor(0)` for a thread that never had one).
    #[must_use]
    pub fn last_cursor(&self) -> ThreadCursor {
        self.store.last_cursor()
    }

    /// The lowest cursor the held items can have. Cursors below it belong to
    /// items discarded by a reset or by the memory window.
    #[must_use]
    pub fn first_cursor(&self) -> ThreadCursor {
        self.store.first_cursor()
    }

    /// `Some(first_cursor)` when the memory window dropped older items since
    /// the last reset: history before it cannot be served.
    #[must_use]
    pub fn truncated_before(&self) -> Option<ThreadCursor> {
        self.store.truncated().then(|| self.first_cursor())
    }

    /// Counts resets (truncation or replacement of the transcript). Feed it
    /// into the epoch.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The turn in progress, if the user has said anything yet.
    #[must_use]
    pub fn current_turn(&self) -> Option<TurnIndex> {
        self.turn
    }

    /// The held items of one turn (the transcript slice a TL;DR summarizes).
    #[must_use]
    pub fn turn(&self, turn: TurnIndex) -> &[ThreadItem] {
        let items = self.store.items();
        let start = items.partition_point(|item| item.turn < turn);
        let end = items.partition_point(|item| item.turn <= turn);
        items.get(start..end).unwrap_or_default()
    }

    /// What was skipped so far.
    #[must_use]
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// Normalizes one transcript line. Never fails: anything unusable is
    /// counted in [`diagnostics`](Self::diagnostics) and skipped.
    pub fn ingest(&mut self, line: &[u8]) {
        self.diagnostics.lines += 1;
        if self.format == Format::Codex {
            return self.codex(line);
        }
        let Ok(record) = serde_json::from_slice::<Record>(line) else {
            return self.unparsed(line);
        };
        match record.kind.as_deref() {
            Some("user") => self.user(record),
            Some("assistant") => self.assistant(record),
            Some("attachment") => self.attachment(record),
            Some(kind) => self.diagnostics.skip_type(kind),
            None => self.diagnostics.malformed(NO_TYPE),
        }
    }

    /// Settles every running tool call as [`ToolStatus::Unknown`], e.g.
    /// when the agent's session ended. The updates are reported by the next
    /// [`take_changed`](Self::take_changed).
    pub fn settle_pending(&mut self) {
        let ids: Vec<String> = self.pending.keys().cloned().collect();
        for id in ids {
            self.settle(&id);
        }
    }

    /// Cursors of items added or updated since the last call, ascending.
    pub fn take_changed(&mut self) -> Vec<ThreadCursor> {
        self.store.take_changed()
    }

    /// Drops every item after the source was truncated or replaced. Cursors
    /// continue from where they were, so a stale client cursor never aliases
    /// a new item, and the generation changes.
    pub(crate) fn reset(&mut self) {
        self.store.reset();
        self.turn = None;
        self.pending.clear();
        self.cwd = None;
        self.codex_turns = codex::Turns::default();
        self.shell = None;
        self.generation += 1;
        self.diagnostics.resets += 1;
    }

    pub(crate) fn note_oversized(&mut self, lines: usize) {
        self.diagnostics.oversized_lines += lines as u64;
    }

    /// A line that is not a [`Record`]: classify it for diagnostics only.
    fn unparsed(&mut self, line: &[u8]) {
        let kind = serde_json::from_slice::<Kind>(line).map(|k| k.kind);
        match kind {
            Err(_) => self.diagnostics.malformed(INVALID_JSON),
            Ok(Some(Value::String(kind))) if kind == "user" || kind == "assistant" => {
                self.diagnostics.malformed(&kind);
            }
            Ok(Some(Value::String(kind))) => self.diagnostics.skip_type(&kind),
            Ok(_) => self.diagnostics.malformed(NO_TYPE),
        }
    }

    /// Appends an item, then applies the memory window and the limits on
    /// running tool calls.
    fn push(&mut self, at: Option<Timestamp>, content: ThreadContent) -> ThreadCursor {
        // A shell command's output comes in the very next record, if at all.
        self.shell = None;
        let cursor = self.store.push(self.turn.unwrap_or_default(), at, content);
        self.store.enforce(&self.window);
        let first = self.store.first_cursor();
        self.pending.retain(|_, p| p.cursor >= first);
        self.settle_stale();
        cursor
    }

    /// Settles running calls that are too old or too many.
    fn settle_stale(&mut self) {
        let newest = self.store.last_cursor().0;
        let max_age = self.window.max_pending_age;
        let stale: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.cursor.0.saturating_add(max_age) < newest)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.settle(&id);
        }
        while self.pending.len() > self.window.max_pending {
            let Some(oldest) = self
                .pending
                .iter()
                .min_by_key(|(_, p)| p.cursor)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.settle(&oldest);
        }
    }

    /// Stops tracking a running call and marks it [`ToolStatus::Unknown`].
    fn settle(&mut self, id: &str) {
        if let Some(pending) = self.pending.remove(id) {
            mark_unknown(&mut self.store, pending.cursor);
        }
    }
}

fn mark_unknown(store: &mut Store, cursor: ThreadCursor) {
    store.update(cursor, |content| {
        if let ThreadContent::Tool { status, .. } = content
            && *status == ToolStatus::Running
        {
            *status = ToolStatus::Unknown;
        }
    });
}
