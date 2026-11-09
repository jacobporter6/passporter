//! The thread panel: fetches the agent's [`ThreadPage`]s into the shared
//! [`ThreadModel`], follows live `thread_items` events, and renders items:
//! each finished turn's headline above it, messages as safe Markdown
//! ([`crate::ui::markdown`]), tool calls and diffs collapsed to one line
//! with plain-text bodies.
//!
//! Long threads stay skimmable on a phone: long assistant messages fold to
//! their first lines, runs of tool calls fold into one row, and the newest
//! turn starts unfolded. What the user unfolds or folds is kept per row for
//! the life of the page.
//!
//! By default the thread is summary-first ([`Layout`]): each turn shows
//! its prompt clamped to two lines, its headline (or a placeholder, and
//! its last assistant message while there is no headline; a provisional
//! headline, made from a turn still in progress, is marked "so far" and
//! keeps the last message too) and, under the
//! latest finished turn, the TL;DR card. Every turn's head has
//! "Summarise", which shows that turn's TL;DR card under it, whatever its
//! length (provisional, "so far", for the turn in progress). The rest of
//! the turn (prose,
//! tool calls, diffs) is rendered but hidden behind the turn's "Show full
//! turn"; the turn in progress shows only its latest assistant message,
//! folded. The setting "Show full transcripts by default" restores the
//! whole transcript.

use std::collections::{BTreeMap, HashMap};

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{
    Agent, AgentId, AgentStatus, Seq, ThreadContent, ThreadCursor, ThreadEpoch, ThreadItem,
    ToolStatus, TurnIndex,
};

use super::model::{Headline, Sync, ThreadModel};
use super::tldr_view::TldrCard;
use crate::live::{Conn, Live, use_live};
use crate::session;
use crate::ui::follow::{FollowBottom, Missed, scroll_to_bottom};
use crate::ui::icons::{self, Icon};
use crate::ui::markdown::{LinkConfirm, LinkPrompt, Markdown, visible_code};
use crate::ui::{AppState, use_app};

/// Items per page request.
const PAGE_LIMIT: u32 = 200;
/// Safety stop for one sync: pages fetched before giving up for now.
const MAX_PAGES: usize = 50;
/// Items rendered at first; "Show earlier" adds this many more.
const WINDOW: usize = 120;
/// Longest diff body rendered, in lines.
const MAX_DIFF_LINES: usize = 400;
/// Lines a folded message shows (`.msg-folded` in `app.css` clips to the
/// same).
const FOLD_LINES: usize = 6;
/// A message folds only when that hides at least this many more lines: a
/// "Show more" that reveals one line is noise.
const FOLD_SLACK: usize = 2;
/// The class that folds a message.
const FOLDED: &str = "msg-folded";
/// Characters of body text per line at phone width (360px).
const LINE_CHARS: usize = 40;
/// Lines a turn's prompt shows until the turn is opened (`.turn-prompt`
/// in `app.css` clips to the same).
const PROMPT_LINES: usize = 2;
/// How long after a turn finishes its missing headline reads as being
/// made ("Summarising…"); after that only the fallback message shows.
const SUMMARISING_MS: u64 = 90_000;

/// Thread state that lives with the agent screen, so it outlasts the panel
/// (switching to Screen and back): what arrived out of view, and the user's
/// fold choices.
#[derive(Debug, Clone, Copy)]
pub struct ThreadUi {
    /// New items arrived below where the user is reading (see
    /// [`JumpToLatest`]).
    pub missed: Missed,
    folds: StoredValue<FoldMemory>,
    /// Fires when the user opens or closes a turn (see
    /// [`FoldMemory::opened`]).
    turns: Trigger,
    /// The user's TL;DR choices per turn: shown (asked to summarise,
    /// `true`) or hidden (`false`) under its head; other turns show their
    /// default (only the latest finished one has a card). Turn numbers
    /// come from the transcript, so they outlive a new numbering of
    /// cursors.
    asked: RwSignal<Vec<(TurnIndex, bool)>>,
}

impl ThreadUi {
    /// Nothing missed, nothing chosen.
    #[must_use]
    pub fn new() -> Self {
        Self {
            missed: Missed::new(),
            folds: StoredValue::new(FoldMemory::default()),
            turns: Trigger::new(),
            asked: RwSignal::new(Vec::new()),
        }
    }
}

impl Default for ThreadUi {
    fn default() -> Self {
        Self::new()
    }
}

/// The user's fold choices by cursor, for one numbering of the thread.
#[derive(Debug, Default)]
struct FoldMemory {
    epoch: Option<ThreadEpoch>,
    open: HashMap<ThreadCursor, bool>,
    /// Turns the user opened with "Show full turn". Turn numbers come from
    /// the transcript, so they outlive a new numbering of cursors.
    opened: Vec<TurnIndex>,
}

impl FoldMemory {
    /// Forgets every fold choice when the thread's numbering changes: cursors
    /// are reused after `/clear` or a bridge restart.
    fn follow_epoch(&mut self, epoch: Option<&ThreadEpoch>) {
        if let Some(epoch) = epoch
            && self.epoch.as_ref() != Some(epoch)
        {
            self.epoch = Some(epoch.clone());
            self.open.clear();
        }
    }
}

/// Whether agent `id`'s held thread has `flag`, tracked.
fn thread_flag(live: Live, id: AgentId, flag: fn(&ThreadModel) -> bool) -> Memo<bool> {
    Memo::new(move |_| live.threads.with(|t| t.get(&id).is_some_and(flag)))
}

/// The thread panel for agent `id` (`agent`: its list entry), with the
/// TL;DR card under its latest finished turn (`finished_seq`: see
/// [`super::tldr::finished_seq`]).
#[component]
pub fn ThreadView(
    id: AgentId,
    ui: ThreadUi,
    agent: Memo<Option<Agent>>,
    finished_seq: Signal<Option<Seq>>,
) -> impl IntoView {
    let app = use_app();
    let live = use_live();
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let shown = RwSignal::new(WINDOW);
    let links: LinkPrompt = RwSignal::new(None);

    live.threads.update(|threads| {
        threads.entry(id.clone()).or_default();
    });

    let sync = syncer(app, live, id.clone(), loading, error);

    keep_synced(live, id.clone(), sync.clone());

    let total = {
        let id = id.clone();
        Memo::new(move |_| {
            live.threads
                .with(|t| t.get(&id).map_or(0, ThreadModel::len))
        })
    };
    let items = {
        let id = id.clone();
        Memo::new(move |_| {
            let limit = shown.get();
            live.threads.with(|t| {
                t.get(&id).map_or_else(Vec::new, |model| {
                    // Before any row reads a choice made for another numbering.
                    ui.folds.update_value(|f| f.follow_epoch(model.epoch()));
                    let all: Vec<&ThreadItem> = model.items().collect();
                    all[window_start(&all, limit)..]
                        .iter()
                        .map(|item| (*item).clone())
                        .collect::<Vec<_>>()
                })
            })
        })
    };
    let full = app.full_transcripts;
    let rows = headed_rows(live, id.clone(), items, layout(app, full, agent), ui.asked);
    let truncated = thread_flag(live, id.clone(), ThreadModel::truncated);
    let inferred = thread_flag(live, id.clone(), ThreadModel::inferred);
    let follow = FollowBottom::noting(ui.missed);
    follow.on_arrival(
        move || items.with(|i| i.last().map(|l| (l.cursor, l.rev))),
        arrived,
    );
    on_cleanup(move || ui.missed.clear());
    let open_turn = Memo::new(move |previous: Option<&Option<TurnIndex>>| {
        // Summary-first, everything starts folded: a turn's last message
        // shows its first lines.
        if !full.get() {
            return None;
        }
        let newest = items.with(|i| i.iter().map(|item| item.turn).max());
        hold_open_turn(previous.copied().flatten(), newest, follow.at_end())
    });
    let ctx = RowCtx {
        links,
        folds: ui.folds,
        open_turn,
        turns: ui.turns,
        asked: ui.asked,
        id: StoredValue::new(id),
        finished_seq,
    };
    let row = move |line: Line| thread_row(line, ctx);

    let earlier_hidden = move || items.with(Vec::len) < total.get();
    let history_dropped = move || truncated.get() && !earlier_hidden();
    let retry = sync.clone();
    view! {
        <div class="thread" class:thread-inferred=move || inferred.get() aria-busy=move || loading.get().to_string()>
            <Show when=earlier_hidden>
                <button type="button" class="button button-secondary button-small show-earlier" on:click=move |_| shown.update(|n| *n += WINDOW)>
                    "Show earlier"
                </button>
            </Show>
            <Show when=history_dropped>
                <p class="thread-note">"Older history is no longer kept by the bridge."</p>
            </Show>
            <Show when=move || items.with(Vec::is_empty) && !loading.get() && error.get().is_none()>
                <p class="thread-note">"No transcript for this agent — see Screen."</p>
            </Show>
            <Show when=move || items.with(Vec::is_empty) && loading.get()>
                <div class="skeleton" aria-hidden="true"><span></span><span></span><span></span></div>
            </Show>
            <ol class="thread-items" role="list">
                <For each=rows key=Line::key children=row />
            </ol>
            {move || error.get().map(|message| {
                let retry = retry.clone();
                view! {
                    <div class="inline-error" role="alert">
                        <span>{format!("Couldn't load the thread: {message}")}</span>
                        <button type="button" class="button button-secondary button-small" on:click=move |_| retry()>"Retry"</button>
                    </div>
                }
            })}
            <LinkConfirm prompt=links />
        </div>
    }
}

/// What lays the thread out, besides its items: summary-first unless the
/// device shows full transcripts (`full`), whether the agent works on its
/// newest turn, and whether its latest turn just finished (so a missing
/// headline is likely still being made). The flags are memos, so a clock
/// tick or a seq bump lays nothing out again until one flips.
fn layout(
    app: AppState,
    full: RwSignal<bool>,
    agent: Memo<Option<Agent>>,
) -> impl Fn() -> Layout + Send + core::marker::Sync + 'static {
    let status = move || agent.with(|a| a.as_ref().map(|a| (a.status, a.status_since)));
    let busy = Memo::new(move |_| {
        status().is_some_and(|(s, _)| matches!(s, AgentStatus::Working | AgentStatus::Blocked))
    });
    let recent = Memo::new(move |_| {
        status().is_some_and(|(s, since)| {
            s.is_ready() && since.millis_until(app.now.get()) < SUMMARISING_MS
        })
    });
    move || Layout {
        summary: !full.get(),
        // Filled in with the items: their newest turn.
        live: busy.get().then_some(TurnIndex(0)),
        summarising: recent.get(),
    }
}

/// The lines to show: `items` grouped and laid out by `layout`. Tracks the
/// model's headline revision, not the map itself.
fn headed_rows(
    live: Live,
    id: AgentId,
    items: Memo<Vec<ThreadItem>>,
    layout: impl Fn() -> Layout + Send + core::marker::Sync + 'static,
    asked: RwSignal<Vec<(TurnIndex, bool)>>,
) -> impl Fn() -> Vec<Line> {
    let headlines_rev = {
        let id = id.clone();
        Memo::new(move |_| {
            live.threads
                .with(|t| t.get(&id).map_or(0, ThreadModel::headlines_rev))
        })
    };
    // Laid out apart from the TL;DR choices: a Summarise tap only adds or
    // drops a card.
    let laid = Memo::new(move |_| {
        headlines_rev.track();
        let items = items.get();
        let mut layout = layout();
        layout.live = layout.live.and(items.last().map(|item| item.turn));
        let rows = group(items);
        let lines = live.threads.with_untracked(|t| {
            let none = BTreeMap::new();
            let headlines = t.get(&id).map_or(&none, ThreadModel::headlines);
            lay_out(rows, (headlines, headlines_rev.get_untracked()), layout)
        });
        (lines, layout.live)
    });
    move || laid.with(|(lines, live)| asked.with(|asked| with_summaries(lines, asked, *live)))
}

/// A sync starter for one thread: single flight, with a follow-up run when
/// asked again meanwhile.
fn syncer(
    app: AppState,
    live: Live,
    id: AgentId,
    loading: RwSignal<bool>,
    error: RwSignal<Option<String>>,
) -> impl Fn() + Clone + 'static {
    // A sync asked for while one runs (e.g. an epoch reset mid-fetch) runs
    // again right after it, so the thread never stays half-loaded.
    let again = StoredValue::new(false);
    move || {
        if !app.paired.get_untracked() {
            return;
        }
        if loading.get_untracked() {
            again.set_value(true);
            return;
        }
        loading.set(true);
        error.set(None);
        let id = id.clone();
        spawn_local(async move {
            loop {
                again.try_set_value(false);
                let result = sync_thread(live, app, id.clone()).await;
                if let Err(message) = result {
                    let _ = error.try_set(Some(message));
                    break;
                }
                let unloaded = live
                    .threads
                    .try_with_untracked(|t| t.get(&id).is_some_and(|m| !m.is_loaded()))
                    .unwrap_or(false);
                if again.try_get_value() != Some(true) && !unloaded {
                    break;
                }
            }
            let _ = loading.try_set(false);
        });
    }
}

/// Runs `sync` on open, whenever the model is reset (epoch change), and
/// when the stream comes back (events may have been missed meanwhile).
fn keep_synced(live: Live, id: AgentId, sync: impl Fn() + 'static) {
    let loaded = Memo::new(move |_| {
        live.threads
            .with(|t| t.get(&id).is_some_and(ThreadModel::is_loaded))
    });
    Effect::new(move |previous: Option<(bool, Conn)>| {
        let now = (loaded.get(), live.conn.get());
        let reconnected = now.1 == Conn::Live && previous.map(|p| p.1) != Some(Conn::Live);
        if !now.0 || reconnected {
            sync();
        }
        now
    });
}

/// Fetches pages from the model's resume cursor until the bridge has
/// nothing more, restarting from the top on an epoch change.
async fn sync_thread(live: Live, app: AppState, id: AgentId) -> Result<(), String> {
    let api = session::api(app);
    for _ in 0..MAX_PAGES {
        let since = live
            .threads
            .with_untracked(|t| t.get(&id).and_then(ThreadModel::resume));
        let page = api
            .thread(id.clone(), since, Some(PAGE_LIMIT))
            .await
            .map_err(|e| e.to_string())?;
        let mut outcome = Sync::Ignored;
        live.threads.try_update(|threads| {
            outcome = threads
                .entry(id.clone())
                .or_default()
                .apply_page(page, since);
        });
        match outcome {
            Sync::Merged { more: true } | Sync::Refetch => {}
            Sync::Merged { more: false } | Sync::Ignored => return Ok(()),
        }
    }
    Ok(())
}

/// What every row of one thread shares.
#[derive(Debug, Clone, Copy)]
struct RowCtx {
    links: LinkPrompt,
    folds: StoredValue<FoldMemory>,
    open_turn: Memo<Option<TurnIndex>>,
    turns: Trigger,
    asked: RwSignal<Vec<(TurnIndex, bool)>>,
    id: StoredValue<AgentId>,
    finished_seq: Signal<Option<Seq>>,
}

impl RowCtx {
    /// Whether the user opened `turn` (tracked).
    fn is_opened(self, turn: TurnIndex) -> bool {
        self.turns.track();
        self.folds.with_value(|f| f.opened.contains(&turn))
    }

    /// Opens or closes `turn`.
    fn toggle(self, turn: TurnIndex) {
        self.folds
            .update_value(|f| match f.opened.iter().position(|t| *t == turn) {
                Some(at) => _ = f.opened.swap_remove(at),
                None => f.opened.push(turn),
            });
        self.turns.notify();
    }

    /// Whether the user asked to summarise `turn` (tracked).
    fn is_asked(self, turn: TurnIndex) -> bool {
        self.asked.with(|asked| asked.contains(&(turn, true)))
    }

    /// Shows `turn`'s TL;DR card on request, or hides it (the latest
    /// finished turn's default card too).
    fn toggle_asked(self, turn: TurnIndex) {
        let show = !self.is_asked(turn);
        self.asked.update(|asked| {
            asked.retain(|(t, _)| *t != turn);
            asked.push((turn, show));
        });
    }
}

/// One line of the list, with where its folds find their state.
fn thread_row(line: Line, ctx: RowCtx) -> AnyView {
    let Line { row, role } = line;
    let links = ctx.links;
    let fold = |item: &ThreadItem| Fold {
        cursor: item.cursor,
        turn: item.turn,
        folds: ctx.folds,
        open_turn: ctx.open_turn,
    };
    let turn = row.first().map_or(TurnIndex(0), |item| item.turn);
    // Behind the turn's toggle, or clamped until it opens.
    let hidden = move || role == Role::Detail && !ctx.is_opened(turn);
    let clamped = move || role == Role::Prompt && !ctx.is_opened(turn);
    match row {
        Row::Head {
            turn,
            text,
            so_far,
            note,
            more,
            ..
        } => {
            let placeholder = text.is_none();
            // An empty label (no headline, nothing to say) is not shown.
            let label = text.unwrap_or_else(|| note.label().unwrap_or_default().to_owned());
            // The separator is for screen readers and copied text only.
            let marker = so_far.then(|| {
                view! { <span class="turn-so-far"><span class="visually-hidden">" — "</span>"so far"</span> }
            });
            view! {
                <li class="turn-headline">
                    <h3 class:turn-note=placeholder>{label}{marker}</h3>
                    <button
                        type="button"
                        class="turn-toggle"
                        hidden=!more
                        aria-expanded=move || ctx.is_opened(turn).to_string()
                        on:click=move |_| ctx.toggle(turn)
                    >
                        // "Show " or "Hide " comes from `app.css`, by state.
                        "full turn"
                    </button>
                    <button
                        type="button"
                        class="turn-summarise"
                        aria-expanded=move || ctx.is_asked(turn).to_string()
                        on:click=move |_| ctx.toggle_asked(turn)
                    >
                        {move || if ctx.is_asked(turn) { "Hide TL;DR" } else { "Summarise" }}
                    </button>
                </li>
            }
            .into_any()
        }
        Row::Tldr { turn, so_far, .. } => view! {
            <li class="turn-tldr"><TldrCard id=ctx.id.get_value() finished_seq=ctx.finished_seq turn=turn so_far=so_far /></li>
        }
        .into_any(),
        Row::One(item) => {
            let fold = fold(&item);
            view! { <li class:turn-start=role == Role::Prompt hidden=hidden class:turn-prompt=clamped><Item item=item links=links fold=fold /></li> }
                .into_any()
        }
        Row::Tools(calls) => {
            let fold = fold(&calls[0]);
            view! { <li hidden=hidden><ToolGroup calls=calls links=links fold=fold /></li> }.into_any()
        }
    }
}

/// "Jump to latest": floats above the reply bar while items arrived below
/// where the user is reading.
#[component]
pub fn JumpToLatest(missed: Missed) -> impl IntoView {
    view! {
        <button
            type="button"
            class="jump-latest"
            hidden=move || !missed.get()
            on:click=move |_| {
                scroll_to_bottom();
                missed.clear();
            }
        >
            <Icon paths=icons::ARROW_DOWN />
            "Latest"
        </button>
    }
}

/// A row of the thread as shown: a turn's head, the TL;DR card, one
/// item, or a run of tool calls folded into one line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    /// A turn's head, above the turn's first shown row after its prompt
    /// (`before`: the turn's first shown cursor): its headline (untrusted:
    /// rendered as text; `so_far`: provisional, made from the turn in
    /// progress) or a placeholder, and, summary-first, the turn's "Show
    /// full turn" when it hides anything (`more`).
    Head {
        turn: TurnIndex,
        before: ThreadCursor,
        text: Option<String>,
        so_far: bool,
        note: Note,
        more: bool,
        /// The revision of the headlines it was made from.
        rev: u64,
    },
    /// The TL;DR card, under a turn's head: the latest finished turn's
    /// (`turn` `None`), or the one the user asked for (`so_far`: of the
    /// turn in progress, so provisional).
    Tldr {
        before: ThreadCursor,
        turn: Option<TurnIndex>,
        so_far: bool,
    },
    /// A message, diff, or lone tool call.
    One(ThreadItem),
    /// Two or more tool calls in a row.
    Tools(Vec<ThreadItem>),
}

impl Row {
    /// The first item of a row of items.
    fn first(&self) -> Option<&ThreadItem> {
        match self {
            Self::Head { .. } | Self::Tldr { .. } => None,
            Self::One(item) => Some(item),
            Self::Tools(calls) => calls.first(),
        }
    }
}

/// What a turn's head says when it has no headline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Note {
    /// Nothing: the turn's last message shows under it instead.
    None,
    /// The turn is in progress.
    Live,
    /// The turn just finished: its headline is likely being made.
    Summarising,
}

impl Note {
    const fn label(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Live => Some("Working…"),
            Self::Summarising => Some("Summarising…"),
        }
    }
}

/// How a row of items shows in the summary-first view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Always (and every row of the full transcript).
    Shown,
    /// The turn's prompt: clamped until the turn is opened.
    Prompt,
    /// Behind the turn's "Show full turn".
    Detail,
}

/// A row and how it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    row: Row,
    role: Role,
}

impl Line {
    const fn shown(row: Row) -> Self {
        Self {
            row,
            role: Role::Shown,
        }
    }

    /// Identifies a line and its content for the keyed list: first and
    /// last cursor, the newest revision (a head: the headlines' revision)
    /// and a tag for its kind and role (a head: and what it says besides
    /// its headline), so a line is rebuilt only when it
    /// changes, and a head or card never collides with the row after it.
    fn key(&self) -> (ThreadCursor, ThreadCursor, u64, u8) {
        let role = self.role as u8;
        match &self.row {
            Row::Head {
                before,
                rev,
                note,
                more,
                ..
            } => (
                *before,
                *before,
                *rev,
                3 + 4 * (*note as u8) + 16 * u8::from(*more),
            ),
            // Apart from every head's tag (odd, 3 and up by 4) and role.
            Row::Tldr {
                before,
                turn,
                so_far,
            } => (
                *before,
                *before,
                0,
                4 + u8::from(turn.is_some()) + u8::from(*so_far),
            ),
            Row::One(item) => (item.cursor, item.cursor, item.rev, role),
            Row::Tools(calls) => {
                let first = calls.first().map_or(ThreadCursor(0), |c| c.cursor);
                let last = calls.last().map_or(first, |c| c.cursor);
                let rev = calls.iter().map(|c| c.rev).max().unwrap_or(0);
                (first, last, rev, role)
            }
        }
    }
}

/// What lays a thread out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Layout {
    /// Summary-first; otherwise the whole transcript with headlines.
    summary: bool,
    /// The turn in progress, if the agent is working on one.
    live: Option<TurnIndex>,
    /// The latest finished turn just finished: a missing headline reads
    /// "Summarising…".
    summarising: bool,
}

/// `rows` as lines: summary-first ([`summarize_turn`] per turn), or each
/// turn's headline inserted above the first of its rows shown.
fn lay_out(rows: Vec<Row>, headlines: Headlines<'_>, layout: Layout) -> Vec<Line> {
    if !layout.summary {
        return headed(rows, headlines);
    }
    let latest_finished = rows
        .iter()
        .filter_map(|row| row.first().map(|item| item.turn))
        .filter(|turn| Some(*turn) != layout.live)
        .max();
    let mut out = Vec::with_capacity(rows.len() + 8);
    let mut rows = rows.into_iter().peekable();
    while let Some(first) = rows.next() {
        let Some(turn) = first.first().map(|item| item.turn) else {
            continue;
        };
        let mut group = vec![first];
        while let Some(next) = rows.next_if(|r| r.first().is_some_and(|i| i.turn == turn)) {
            group.push(next);
        }
        let head = TurnHead {
            turn,
            headline: headlines.0.get(&turn),
            rev: headlines.1,
            live: layout.live == Some(turn),
            latest: latest_finished == Some(turn),
            summarising: layout.summarising,
        };
        summarize_turn(group, &head, &mut out);
    }
    out
}

/// A thread's headlines by turn, and their revision.
type Headlines<'a> = (&'a BTreeMap<TurnIndex, Headline>, u64);

/// The full transcript's lines: each turn's headline above the first of
/// its rows shown.
fn headed(rows: Vec<Row>, (headlines, rev): Headlines<'_>) -> Vec<Line> {
    let mut out = Vec::with_capacity(rows.len());
    let mut turn = None;
    for row in rows {
        if let Some(first) = row.first()
            && turn != Some(first.turn)
        {
            turn = Some(first.turn);
            if let Some(headline) = headlines.get(&first.turn) {
                out.push(Line::shown(Row::Head {
                    turn: first.turn,
                    before: first.cursor,
                    text: Some(headline.text.clone()),
                    so_far: headline.provisional,
                    note: Note::None,
                    more: false,
                    rev,
                }));
            }
        }
        out.push(Line::shown(row));
    }
    out
}

/// What [`summarize_turn`] knows about a turn.
#[derive(Debug, Clone, Copy)]
struct TurnHead<'a> {
    turn: TurnIndex,
    headline: Option<&'a Headline>,
    /// The headlines' revision.
    rev: u64,
    /// In progress.
    live: bool,
    /// The latest finished turn: the TL;DR card goes under it.
    latest: bool,
    /// See [`Layout::summarising`].
    summarising: bool,
}

/// One turn's rows (`group`, all of `head.turn`), summary-first: the
/// prompt (clamped), the head, the TL;DR card for the latest finished
/// turn, then the rest behind the toggle. Without a final headline, or
/// while the turn is in progress, its last assistant message stays shown,
/// so a turn never reads empty. Rows of kinds this does not know are
/// details.
fn summarize_turn(group: Vec<Row>, head: &TurnHead<'_>, out: &mut Vec<Line>) {
    let before = group
        .first()
        .and_then(Row::first)
        .map_or(ThreadCursor(0), |item| item.cursor);
    let said = |row: &Row| matches!(row, Row::One(item) if matches!(item.content, ThreadContent::Assistant { .. }));
    let prompt = match group.first() {
        Some(Row::One(ThreadItem {
            content: ThreadContent::User { text },
            ..
        })) => Some(estimated_lines(text) > PROMPT_LINES),
        _ => None,
    };
    let keep_last = head.headline.is_none_or(|h| h.provisional) || head.live;
    let last_said = group.iter().rposition(said).filter(|_| keep_last);
    let role = |at: usize| match (at, prompt) {
        (0, Some(_)) => Role::Prompt,
        _ if Some(at) == last_said => Role::Shown,
        _ => Role::Detail,
    };
    let note = if head.live {
        Note::Live
    } else if head.headline.is_none() && head.latest && head.summarising {
        Note::Summarising
    } else {
        Note::None
    };
    // Rows other than the prompt and the kept message hide, and so may a
    // long prompt's end.
    let kept = usize::from(prompt.is_some()) + usize::from(last_said.is_some());
    let more = group.len() > kept || prompt == Some(true);
    let mut lines = group.into_iter().enumerate().map(|(at, row)| Line {
        row,
        role: role(at),
    });
    if prompt.is_some() {
        out.extend(lines.next());
    }
    out.push(Line::shown(Row::Head {
        turn: head.turn,
        before,
        text: head.headline.map(|h| h.text.clone()),
        so_far: head.headline.is_some_and(|h| h.provisional),
        note,
        more,
        rev: head.rev,
    }));
    if head.latest {
        out.push(Line::shown(Row::Tldr {
            before,
            turn: None,
            so_far: false,
        }));
    }
    out.extend(lines);
}

/// `lines` with the user's TL;DR choices (`asked`) applied: a card under
/// the head of each turn asked to summarise, none under one they hid,
/// either way in place of the latest finished turn's default card. The
/// card of the turn in progress (`live`) is provisional.
fn with_summaries(
    lines: &[Line],
    asked: &[(TurnIndex, bool)],
    live: Option<TurnIndex>,
) -> Vec<Line> {
    let mut out = Vec::with_capacity(lines.len() + asked.len());
    // The choice for the turn whose head came last.
    let mut chosen = None;
    for line in lines {
        match &line.row {
            Row::Head { turn, before, .. } => {
                chosen = asked.iter().find(|(t, _)| t == turn).map(|(_, show)| *show);
                out.push(line.clone());
                if chosen == Some(true) {
                    out.push(Line::shown(Row::Tldr {
                        before: *before,
                        turn: Some(*turn),
                        so_far: live == Some(*turn),
                    }));
                }
            }
            Row::Tldr { turn: None, .. } if chosen.is_some() => {}
            _ => out.push(line.clone()),
        }
    }
    out
}

/// Where the shown window starts in `items` to show the last `limit`:
/// moved back so it never cuts a run of tool calls (a group is known by
/// its first call).
fn window_start(items: &[&ThreadItem], limit: usize) -> usize {
    let is_tool = |at: usize| matches!(items[at].content, ThreadContent::Tool { .. });
    let mut start = items.len().saturating_sub(limit);
    while start > 0 && is_tool(start) && is_tool(start - 1) {
        start -= 1;
    }
    start
}

/// Whether the thread's end moved on: a newer last item, or a newer
/// revision of it. Showing earlier items, or emptying and refilling on a
/// reload, is not news.
#[expect(
    clippy::ref_option,
    reason = "the shape FollowBottom::on_arrival compares"
)]
fn arrived(previous: &Option<(ThreadCursor, u64)>, current: &Option<(ThreadCursor, u64)>) -> bool {
    matches!((previous, current), (Some(p), Some(c)) if c > p)
}

/// The turn from which rows start open: the newest one, but held while
/// the user reads above, so a new turn does not fold what they are reading
/// (unless the thread restarted below it).
fn hold_open_turn(
    previous: Option<TurnIndex>,
    newest: Option<TurnIndex>,
    at_end: bool,
) -> Option<TurnIndex> {
    match previous {
        Some(held) if !at_end && newest.is_some_and(|n| n >= held) => Some(held),
        _ => newest,
    }
}

/// `items` as rows: consecutive tool calls grouped.
fn group(items: Vec<ThreadItem>) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut run: Vec<ThreadItem> = Vec::new();
    let flush = |run: &mut Vec<ThreadItem>, rows: &mut Vec<Row>| match run.len() {
        0 => {}
        1 => rows.extend(run.drain(..).map(Row::One)),
        _ => rows.push(Row::Tools(core::mem::take(run))),
    };
    for item in items {
        if matches!(item.content, ThreadContent::Tool { .. }) {
            run.push(item);
        } else {
            flush(&mut run, &mut rows);
            rows.push(Row::One(item));
        }
    }
    flush(&mut run, &mut rows);
    rows
}

/// What a tool group's row says it holds: `Bash ×3, Edit ×2`, most used
/// first (ties in order of first use).
fn tally<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    // (name, count, first use), kept in order as it fills: an entry moves
    // left past neighbours it now outranks (no sort, which costs WASM size).
    let mut counts: Vec<(&str, usize, usize)> = Vec::new();
    for name in names {
        let mut at = counts
            .iter()
            .position(|&(n, ..)| n == name)
            .unwrap_or_else(|| {
                counts.push((name, 0, counts.len()));
                counts.len() - 1
            });
        counts[at].1 += 1;
        let outranks = |a: (&str, usize, usize), b: (&str, usize, usize)| {
            a.1 > b.1 || (a.1 == b.1 && a.2 < b.2)
        };
        while at > 0 && outranks(counts[at], counts[at - 1]) {
            counts.swap(at - 1, at);
            at -= 1;
        }
    }
    counts
        .into_iter()
        .map(|(name, count, _)| {
            if count == 1 {
                name.to_owned()
            } else {
                format!("{name} ×{count}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The status a tool group shows: running while any call runs, else failed
/// if any failed, else succeeded if all did.
fn group_status(statuses: impl IntoIterator<Item = ToolStatus>) -> ToolStatus {
    let statuses: Vec<ToolStatus> = statuses.into_iter().collect();
    if statuses.contains(&ToolStatus::Running) {
        ToolStatus::Running
    } else if statuses.contains(&ToolStatus::Error) {
        ToolStatus::Error
    } else if statuses.iter().all(|s| *s == ToolStatus::Ok) {
        ToolStatus::Ok
    } else {
        ToolStatus::Unknown
    }
}

/// About how many lines `text` takes at phone width: prose wraps at
/// [`LINE_CHARS`], fenced code scrolls sideways (one line per line).
fn estimated_lines(text: &str) -> usize {
    let mut fenced = false;
    text.trim()
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                fenced = !fenced;
                1
            } else if fenced {
                1
            } else {
                line.chars().count().div_ceil(LINE_CHARS).max(1)
            }
        })
        .sum()
}

/// Whether an assistant message is long enough to fold.
fn is_long(text: &str) -> bool {
    estimated_lines(text) > FOLD_LINES + FOLD_SLACK
}

/// Where a foldable row finds whether it is open.
#[derive(Debug, Clone, Copy)]
struct Fold {
    cursor: ThreadCursor,
    turn: TurnIndex,
    folds: StoredValue<FoldMemory>,
    open_turn: Memo<Option<TurnIndex>>,
}

/// A foldable row's open state: the user's choice once made (kept for the
/// agent screen's life), else open from the open turn on.
#[derive(Debug, Clone, Copy)]
struct FoldState {
    fold: Fold,
    choice: RwSignal<Option<bool>>,
}

impl FoldState {
    fn new(fold: Fold) -> Self {
        let choice = fold.folds.with_value(|f| f.open.get(&fold.cursor).copied());
        Self {
            fold,
            choice: RwSignal::new(choice),
        }
    }

    /// Whether the row is open (tracked).
    fn is_open(self) -> bool {
        self.choice.get().unwrap_or_else(|| {
            self.fold
                .open_turn
                .get()
                .is_some_and(|from| self.fold.turn >= from)
        })
    }

    /// Opens or folds the row as the user asked; restating the current
    /// state (a `toggle` event for a state set here) is not a choice.
    fn set(self, open: bool) {
        if open != untrack(|| self.is_open()) {
            self.choice.set(Some(open));
            self.fold
                .folds
                .update_value(|f| _ = f.open.insert(self.fold.cursor, open));
        }
    }
}

/// One thread entry.
#[component]
fn Item(item: ThreadItem, links: LinkPrompt, fold: Fold) -> impl IntoView {
    match shown(item.content) {
        ThreadContent::User { text } => view! {
            <div class="msg msg-user"><div class="msg-text"><Markdown source=text links=links /></div></div>
        }
        .into_any(),
        ThreadContent::Assistant { text } if is_long(&text) => {
            view! { <LongMessage text=text links=links fold=fold /> }.into_any()
        }
        ThreadContent::Assistant { text } => view! {
            <div class="msg msg-assistant"><div class="msg-text"><Markdown source=text links=links /></div></div>
        }
        .into_any(),
        ThreadContent::Tool {
            name,
            input,
            output,
            status,
        } => view! { <ToolCall name=name input=input output=output status=status /> }.into_any(),
        ThreadContent::Diff {
            path,
            added,
            removed,
            patch,
        } => view! { <Diff path=path added=added removed=removed patch=patch /> }.into_any(),
        ThreadContent::Aside { label, text } => aside(label, text),
        ThreadContent::Unknown => ().into_any(),
    }
}

/// What the user did not type, though Claude Code recorded it as theirs (a
/// relayed message, their own shell command): one muted line, never a
/// message bubble, that opens to `body` (plain text) when there is one.
fn aside(head: String, body: String) -> AnyView {
    let (head, body) = aside_text(head, body);
    let body = body.map(|b| view! { <pre class="aside-text">{b}</pre> });
    view! {
        <details class="aside">
            <summary class="aside-summary"><span class="aside-head">"↳ "{head}</span></summary>
            {body}
        </details>
    }
    .into_any()
}

/// An aside's line and body as shown: plain text with hidden characters
/// replaced (design §8), and no body when it would be blank.
fn aside_text(head: String, body: String) -> (String, Option<String>) {
    let body = (!body.trim().is_empty()).then(|| visible_code(body));
    (visible_code(head), body)
}

/// A long assistant message, folded to its first lines with a fade and
/// "Show more". It is measured once, folded, after mount: one that fits
/// anyway (a wide screen) shows whole, with no button.
#[component]
fn LongMessage(text: String, links: LinkPrompt, fold: Fold) -> impl IntoView {
    let state = FoldState::new(fold);
    let fits = RwSignal::new(false);
    let body = NodeRef::<html::Div>::new();
    // Tracks nothing: runs once, after mount.
    Effect::new(move |_| {
        request_animation_frame(move || {
            let Some(el) = body.get_untracked() else {
                return;
            };
            // Hidden behind its turn's toggle: nothing to measure, so it
            // keeps its "Show more".
            if el.client_height() == 0 {
                return;
            }
            let classes = el.class_list();
            let folded = classes.contains(FOLDED);
            let _ = classes.add_1(FOLDED);
            let whole = el.scroll_height() <= el.client_height() + 1;
            if !folded {
                let _ = classes.remove_1(FOLDED);
            }
            if whole {
                let _ = fits.try_set(true);
            }
        });
    });
    let open = move || state.is_open();
    let toggle = move |_| state.set(!untrack(open));
    view! {
        <div class="msg msg-assistant">
            <div class="msg-text" class:msg-folded=move || !open() && !fits.get() node_ref=body>
                <Markdown source=text links=links />
            </div>
            <button type="button" class="msg-more" hidden=move || fits.get() aria-expanded=move || open().to_string() on:click=toggle>
                {move || if open() { "Show less" } else { "Show more" }}
            </button>
        </div>
    }
}

/// Two or more consecutive tool calls as one row (`5 tool calls`, `Bash
/// ×3, Edit ×2`) that opens to their one-line summaries.
#[component]
fn ToolGroup(calls: Vec<ThreadItem>, links: LinkPrompt, fold: Fold) -> impl IntoView {
    let mut names = Vec::with_capacity(calls.len());
    let mut statuses = Vec::with_capacity(calls.len());
    for call in &calls {
        if let ThreadContent::Tool { name, status, .. } = &call.content {
            names.push(visible_code(name.clone()));
            statuses.push(*status);
        }
    }
    let (mark, class, label) = status_mark(group_status(statuses));
    let count = format!("{} tool calls", calls.len());
    let tools = tally(names.iter().map(String::as_str));
    let state = FoldState::new(fold);
    let inner = calls
        .into_iter()
        .map(|call| view! { <li><Item item=call links=links fold=fold /></li> })
        .collect_view();
    view! {
        <details
            class=format!("tool tool-group {class}")
            prop:open=move || state.is_open()
            on:toggle=move |ev| state.set(event_target::<web_sys::Element>(&ev).has_attribute("open"))
        >
            <summary class="tool-summary">
                <span class="tool-name">{count}</span>
                <span class="tool-line">{tools}</span>
                <span class="tool-mark" role="img" aria-label=label>{mark}</span>
            </summary>
            <ol class="tool-group-items" role="list">{inner}</ol>
        </details>
    }
}

/// A tool status as its mark, CSS class and spoken label.
const fn status_mark(status: ToolStatus) -> (&'static str, &'static str, &'static str) {
    match status {
        ToolStatus::Ok => ("✓", "tool-ok", "succeeded"),
        ToolStatus::Error => ("✗", "tool-error", "failed"),
        ToolStatus::Running => ("", "tool-running", "running"),
        ToolStatus::Unknown => ("?", "tool-unknown", "status unknown"),
    }
}

#[component]
fn ToolCall(
    name: String,
    input: String,
    output: Option<String>,
    status: ToolStatus,
) -> impl IntoView {
    let (mark, class, label) = status_mark(status);
    let summary = tool_summary(&name, &input);
    view! {
        <details class=format!("tool {class}")>
            <summary class="tool-summary">
                <span class="tool-name">{name}</span>
                <span class="tool-line">{summary}</span>
                <span class="tool-mark" role="img" aria-label=label>{mark}</span>
            </summary>
            <pre class="code">{input}</pre>
            {output.filter(|o| !o.is_empty()).map(|o| view! { <pre class="code code-output">{o}</pre> })}
        </details>
    }
}

#[component]
fn Diff(path: String, added: u32, removed: u32, patch: Option<String>) -> impl IntoView {
    let lines: Vec<(&'static str, String)> = patch
        .as_deref()
        .unwrap_or_default()
        .lines()
        .take(MAX_DIFF_LINES)
        .map(|line| (diff_line_class(line), line.to_owned()))
        .collect();
    let has_patch = !lines.is_empty();
    view! {
        <details class="diff">
            <summary class="tool-summary">
                <span class="tool-line">{short_path(&path)}</span>
                <span class="diff-stat mono">
                    <span class="diff-added">{format!("+{added}")}</span>
                    " "
                    <span class="diff-removed">{format!("−{removed}")}</span>
                </span>
            </summary>
            // The summary keeps only the end of the path; the full one is here.
            <p class="diff-path">{path}</p>
            {if has_patch {
                view! {
                    <pre class="code diff-body">
                        {lines.into_iter().map(|(class, line)| view! { <span class=class>{line}"\n"</span> }).collect_view()}
                    </pre>
                }.into_any()
            } else {
                view! { <p class="thread-note">"Diff too large to show."</p> }.into_any()
            }}
        </details>
    }
}

/// `content` as the thread shows it: tool calls and diffs are plain text,
/// not Markdown, so their hidden characters are replaced here (an injected
/// U+202E could otherwise reorder a command or a file name; design §8).
/// Messages are cleaned by the Markdown parser; relayed messages and shell
/// commands by [`aside`].
fn shown(content: ThreadContent) -> ThreadContent {
    match content {
        ThreadContent::Tool {
            name,
            input,
            output,
            status,
        } => ThreadContent::Tool {
            name: visible_code(name),
            input: visible_code(input),
            output: output.map(visible_code),
            status,
        },
        ThreadContent::Diff {
            path,
            added,
            removed,
            patch,
        } => ThreadContent::Diff {
            path: visible_code(path),
            added,
            removed,
            patch: patch.map(visible_code),
        },
        other => other,
    }
}

/// The one line a collapsed tool call shows: what tool `name` ran or
/// touched, not the whole input (that is in the expanded body). The bridge
/// already reduces `input` to the command, path or pattern.
///
/// - File tools (`Edit`, `Read`, …) show the file: its name and parent
///   folder, so the line fits a phone;
/// - everything else, shell commands included, shows its input's first
///   line, with `…` when it goes on.
#[must_use]
pub fn tool_summary(name: &str, input: &str) -> String {
    let file_tool = matches!(
        name,
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit"
    );
    let input = input.trim();
    if file_tool && looks_like_path(input) {
        short_path(input)
    } else {
        first_line(input)
    }
}

/// Whether `text` (trimmed) can be a lone file path, not the bridge's JSON
/// fallback for an input it could not reduce: one line that does not open
/// a JSON object (`{"…` or `{}`). Template paths (`{{slug}}/x`) pass.
fn looks_like_path(text: &str) -> bool {
    let json_object = text
        .strip_prefix('{')
        .is_some_and(|rest| rest.trim_start().starts_with(['"', '}']));
    !text.is_empty() && !text.contains('\n') && !json_object
}

/// The first non-blank line of `text`, trimmed, with `…` if more follows.
fn first_line(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or_default();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_owned()
    }
}

/// The last two components of `path` (`…/src/main.rs`), or the whole path
/// when it is that short already.
fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [.., parent, file] if parts.len() > 2 => format!("…/{parent}/{file}"),
        _ => path.to_owned(),
    }
}

/// The CSS class for one unified-diff line.
#[must_use]
pub fn diff_line_class(line: &str) -> &'static str {
    if line.starts_with("+++") || line.starts_with("---") {
        "diff-file"
    } else if line.starts_with('+') {
        "diff-add"
    } else if line.starts_with('-') {
        "diff-del"
    } else if line.starts_with("@@") {
        "diff-hunk"
    } else {
        "diff-ctx"
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use protocol::{ThreadContent, ThreadCursor, ThreadEpoch, ThreadItem, ToolStatus, TurnIndex};

    use super::{
        FOLD_LINES, FOLD_SLACK, FoldMemory, Headline, LINE_CHARS, Layout, Line, Note, Role, Row,
        arrived, aside_text, diff_line_class, estimated_lines, group, group_status, headed,
        hold_open_turn, is_long, lay_out, shown, tally, tool_summary as detail, window_start,
        with_summaries,
    };

    #[test]
    fn asides_are_plain_text_without_hidden_characters() {
        let (head, body) = aside_text(
            "Message from another session (ops\u{202e})".into(),
            "see [this](https://x.test)\u{200b}\nok".into(),
        );
        assert!(!head.contains('\u{202e}'), "{head:?}");
        assert_eq!(
            body.as_deref(),
            Some("see [this](https://x.test)\u{fffd}\nok"),
            "kept as text, never a link"
        );
        assert_eq!(
            aside_text("x".into(), " \n".into()).1,
            None,
            "nothing to open"
        );
    }

    fn item(cursor: u64, turn: u32, content: ThreadContent) -> ThreadItem {
        ThreadItem {
            cursor: ThreadCursor(cursor),
            turn: TurnIndex(turn),
            rev: cursor,
            at: None,
            content,
        }
    }

    fn tool(cursor: u64, name: &str) -> ThreadItem {
        item(
            cursor,
            0,
            ThreadContent::Tool {
                name: name.into(),
                input: String::new(),
                output: None,
                status: ToolStatus::Ok,
            },
        )
    }

    /// A final headline.
    fn done(text: &str) -> Headline {
        Headline {
            text: text.into(),
            provisional: false,
        }
    }

    fn said(cursor: u64, text: &str) -> ThreadItem {
        item(cursor, 0, ThreadContent::Assistant { text: text.into() })
    }

    /// The rows' shapes: `1` for a single item, `n` for a group of `n`,
    /// `0` for a head and `9` for the TL;DR card.
    fn shape<'a>(rows: impl IntoIterator<Item = &'a Row>) -> Vec<usize> {
        rows.into_iter()
            .map(|row| match row {
                Row::Head { .. } => 0,
                Row::Tldr { .. } => 9,
                Row::One(_) => 1,
                Row::Tools(calls) => calls.len(),
            })
            .collect()
    }

    fn rows_of(lines: &[Line]) -> impl Iterator<Item = &Row> {
        lines.iter().map(|line| &line.row)
    }

    fn in_turn(mut item: ThreadItem, turn: u32) -> ThreadItem {
        item.turn = TurnIndex(turn);
        item
    }

    fn asked(cursor: u64, turn: u32, text: &str) -> ThreadItem {
        item(cursor, turn, ThreadContent::User { text: text.into() })
    }

    /// Two finished turns and one in progress: prompt, tools, prose, a
    /// diff (an item kind summaries treat as detail) and the last word.
    fn three_turns() -> Vec<Row> {
        let diff = ThreadContent::Diff {
            path: "a.rs".into(),
            added: 1,
            removed: 0,
            patch: None,
        };
        group(vec![
            asked(1, 1, "Fix the parser"),
            in_turn(tool(2, "Read"), 1),
            in_turn(tool(3, "Edit"), 1),
            in_turn(said(4, "Looking at it."), 1),
            item(5, 1, diff),
            in_turn(said(6, "Fixed; shall I push?"), 1),
            asked(7, 2, "Yes"),
            in_turn(said(8, "Pushed."), 2),
            asked(9, 3, "Now the docs"),
            in_turn(said(10, "Reading the docs."), 3),
            in_turn(tool(11, "Read"), 3),
        ])
    }

    fn roles(lines: &[Line]) -> Vec<Role> {
        lines.iter().map(|line| line.role).collect()
    }

    #[test]
    fn short_messages_stay_whole() {
        let limit = FOLD_LINES + FOLD_SLACK;
        assert!(!is_long(""));
        assert!(!is_long("Done."));
        assert!(!is_long(&vec!["line"; limit].join("\n")));
        assert!(is_long(&vec!["line"; limit + 1].join("\n")));
        // Blank edges are not lines.
        assert!(!is_long(&format!(
            "\n\n{}\n\n\n",
            vec!["l"; limit].join("\n")
        )));
    }

    #[test]
    fn long_lines_count_as_wrapped() {
        let full = "x".repeat(LINE_CHARS);
        assert_eq!(estimated_lines(&full), 1);
        assert_eq!(estimated_lines(&format!("{full}x")), 2);
        let paragraph = "word ".repeat(LINE_CHARS * 2); // 10 lines' worth
        assert!(is_long(&paragraph));
    }

    #[test]
    fn fenced_code_lines_do_not_wrap() {
        let wide = "y".repeat(LINE_CHARS * 5);
        let code = format!("```\n{wide}\n{wide}\n```");
        assert_eq!(estimated_lines(&code), 4);
        assert!(!is_long(&code));
        assert_eq!(estimated_lines(&format!("{code}\n{wide}")), 9);
    }

    #[test]
    fn consecutive_tool_calls_group() {
        let rows = group(vec![
            said(1, "hi"),
            tool(2, "Bash"),
            tool(3, "Edit"),
            tool(4, "Bash"),
            said(5, "ok"),
            tool(6, "Read"),
            said(7, "bye"),
            tool(8, "Bash"),
            tool(9, "Bash"),
        ]);
        assert_eq!(shape(&rows), [1, 3, 1, 1, 1, 2]);
        assert!(matches!(&rows[3], Row::One(t) if t.cursor == ThreadCursor(6)));
        assert!(matches!(&rows[1], Row::Tools(t) if t[0].cursor == ThreadCursor(2)));
    }

    #[test]
    fn a_headline_heads_the_first_shown_row_of_its_turn() {
        let rows = group(vec![
            in_turn(tool(1, "Bash"), 1),
            in_turn(tool(2, "Edit"), 1),
            in_turn(said(3, "done"), 1),
            in_turn(said(4, "next"), 2),
            in_turn(said(5, "running"), 3),
        ]);
        let headlines =
            BTreeMap::from([(TurnIndex(1), done("One.")), (TurnIndex(2), done("Two."))]);
        let rows = headed(rows, (&headlines, 0));
        // Turn 3 is still running: no headline yet.
        assert_eq!(shape(rows_of(&rows)), [0, 2, 1, 0, 1, 1]);
        assert!(
            matches!(&rows[0].row, Row::Head { turn, before, text: Some(text), more: false, .. }
            if *turn == TurnIndex(1) && *before == ThreadCursor(1) && text == "One.")
        );
        assert!(matches!(&rows[3].row, Row::Head { before, .. } if *before == ThreadCursor(4)));
        // The full transcript hides nothing.
        assert!(rows.iter().all(|line| line.role == Role::Shown));
        // Its key never collides with the row it heads.
        assert_ne!(rows[0].key(), rows[1].key());
        assert_eq!(headed(Vec::new(), (&headlines, 0)), []);
    }

    #[test]
    fn summarise_puts_a_card_under_the_asked_turns_head() {
        let headlines = BTreeMap::from([(TurnIndex(1), done("Fixed the parser; asks to push."))]);
        let layout = Layout {
            summary: true,
            live: Some(TurnIndex(3)),
            summarising: false,
        };
        let cards = |lines: &[Line]| -> Vec<(Option<TurnIndex>, bool)> {
            rows_of(lines)
                .filter_map(|row| match row {
                    Row::Tldr { turn, so_far, .. } => Some((*turn, *so_far)),
                    _ => None,
                })
                .collect()
        };
        let plain = lay_out(three_turns(), (&headlines, 1), layout);
        assert_eq!(with_summaries(&plain, &[], layout.live), plain);
        assert_eq!(cards(&plain), [(None, false)], "the latest finished turn's");
        // A finished turn, the latest finished one (its card replaced, not
        // doubled) and the one in progress (provisional).
        let asked = [
            (TurnIndex(1), true),
            (TurnIndex(2), true),
            (TurnIndex(3), true),
        ];
        let lines = with_summaries(&plain, &asked, layout.live);
        assert_eq!(
            cards(&lines),
            [
                (Some(TurnIndex(1)), false),
                (Some(TurnIndex(2)), false),
                (Some(TurnIndex(3)), true)
            ]
        );
        // Each card follows its turn's head, and keys apart from it.
        for (at, line) in lines.iter().enumerate() {
            if let Row::Tldr {
                turn: Some(turn), ..
            } = line.row
            {
                assert!(matches!(lines[at - 1].row, Row::Head { turn: t, .. } if t == turn));
                assert_ne!(line.key(), lines[at - 1].key());
            }
        }
        // Hidden: no card, the latest finished turn's default one included;
        // a turn never asked about keeps its default.
        let hidden = [(TurnIndex(1), false), (TurnIndex(2), false)];
        assert_eq!(cards(&with_summaries(&plain, &hidden, layout.live)), []);
        let other = [(TurnIndex(1), false)];
        assert_eq!(
            cards(&with_summaries(&plain, &other, layout.live)),
            [(None, false)]
        );
        // The full transcript: under the heads it has.
        let full = with_summaries(&headed(three_turns(), (&headlines, 1)), &asked, None);
        assert_eq!(cards(&full), [(Some(TurnIndex(1)), false)]);
    }

    #[test]
    fn summaries_show_prompts_and_headlines_and_hide_the_rest() {
        use Role::{Detail, Prompt, Shown};
        let headlines = BTreeMap::from([
            (TurnIndex(1), done("Fixed the parser; asks to push.")),
            (TurnIndex(2), done("Pushed.")),
        ]);
        let layout = Layout {
            summary: true,
            live: Some(TurnIndex(3)),
            summarising: false,
        };
        let lines = lay_out(three_turns(), (&headlines, 1), layout);
        // Per turn: prompt, head, then the rest; the TL;DR card under the
        // latest finished turn (2), not the one in progress.
        assert_eq!(
            shape(rows_of(&lines)),
            [1, 0, 2, 1, 1, 1, 1, 0, 9, 1, 1, 0, 1, 1]
        );
        assert_eq!(
            roles(&lines),
            [
                Prompt, Shown, Detail, Detail, Detail, Detail, // turn 1
                Prompt, Shown, Shown, Detail, // turn 2: head, card, prose
                Prompt, Shown, Shown, Detail, // turn 3: its latest message shows
            ]
        );
        assert!(
            matches!(&lines[1].row, Row::Head { text: Some(t), note: Note::None, more: true, .. }
            if t.starts_with("Fixed"))
        );
        assert!(matches!(
            &lines[11].row,
            Row::Head {
                text: None,
                note: Note::Live,
                more: true,
                ..
            }
        ));
        // Every key is distinct, so the keyed list never mixes rows up.
        let mut keys: Vec<_> = lines.iter().map(Line::key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), lines.len());
    }

    #[test]
    fn a_turn_without_a_headline_keeps_its_last_message() {
        use Role::{Detail, Prompt, Shown};
        let layout = Layout {
            summary: true,
            live: None,
            summarising: true,
        };
        let lines = lay_out(three_turns(), (&BTreeMap::new(), 0), layout);
        // Turn 1's last message stands in for its headline; the newest
        // turn just finished, so its head says it is being made.
        assert_eq!(
            roles(&lines)[..6],
            [Prompt, Shown, Detail, Detail, Detail, Shown]
        );
        assert!(matches!(
            &lines[1].row,
            Row::Head {
                note: Note::None,
                ..
            }
        ));
        let newest = lines
            .iter()
            .rposition(|l| matches!(l.row, Row::Head { .. }))
            .expect("a head");
        assert!(matches!(
            &lines[newest].row,
            Row::Head {
                note: Note::Summarising,
                ..
            }
        ));
        assert!(matches!(lines[newest + 1].row, Row::Tldr { .. }));
        assert_eq!(lines.last().map(|l| l.role), Some(Detail), "its tool call");
    }

    #[test]
    fn a_provisional_headline_replaces_the_placeholder_marked_so_far() {
        use Role::{Detail, Prompt, Shown};
        let so_far = Headline {
            text: "Merging the swarm's PRs.".into(),
            provisional: true,
        };
        let layout = Layout {
            summary: true,
            live: Some(TurnIndex(3)),
            summarising: false,
        };
        let headlines = BTreeMap::from([(TurnIndex(3), so_far.clone())]);
        let lines = lay_out(three_turns(), (&headlines, 1), layout);
        let head = lines
            .iter()
            .rposition(|l| matches!(l.row, Row::Head { .. }))
            .expect("a head");
        assert!(matches!(
            &lines[head].row,
            Row::Head { text: Some(t), so_far: true, .. } if t.starts_with("Merging")
        ));
        assert_eq!(
            roles(&lines)[head - 1..],
            [Prompt, Shown, Shown, Detail],
            "its latest message still shows"
        );
        // The agent stopped, the final headline not in yet: still marked,
        // and the last message still stands in.
        let stopped = Layout {
            live: None,
            ..layout
        };
        let lines = lay_out(three_turns(), (&headlines, 1), stopped);
        let head = lines
            .iter()
            .rposition(|l| matches!(l.row, Row::Head { .. }))
            .expect("a head");
        assert!(matches!(
            &lines[head].row,
            Row::Head {
                so_far: true,
                note: Note::None,
                ..
            }
        ));
        assert!(
            lines[head + 1..]
                .iter()
                .any(|l| l.role == Shown && matches!(l.row, Row::One(_)))
        );
        // The full transcript marks it too.
        let full = headed(three_turns(), (&headlines, 1));
        assert!(
            full.iter()
                .any(|l| matches!(l.row, Row::Head { so_far: true, .. }))
        );
    }

    #[test]
    fn asides_are_details_and_never_the_prompt() {
        use Role::{Detail, Prompt, Shown};
        let layout = Layout {
            summary: true,
            ..Layout::default()
        };
        let aside = |cursor, turn| {
            item(
                cursor,
                turn,
                ThreadContent::Aside {
                    label: "Report from an agent".into(),
                    text: "No changelog entry.".into(),
                },
            )
        };
        // A turn cut by the window, starting at an aside: no prompt.
        let rows = group(vec![
            aside(1, 1),
            in_turn(said(2, "Noted."), 1),
            asked(3, 2, "Ship it"),
            aside(4, 2),
            in_turn(said(5, "Shipped."), 2),
        ]);
        let headlines = BTreeMap::from([(TurnIndex(2), done("Shipped."))]);
        let lines = lay_out(rows, (&headlines, 1), layout);
        assert_eq!(
            roles(&lines),
            [
                Shown, Detail, Shown, // turn 1: head, aside, the last message for a headline
                Prompt, Shown, Shown, Detail,
                Detail, // turn 2: prompt, head, card, aside, reply
            ]
        );
        assert!(
            matches!(&lines[3].row, Row::One(i) if matches!(i.content, ThreadContent::User { .. }))
        );
    }

    #[test]
    fn a_short_turn_has_nothing_to_open_and_a_headline_change_rekeys_its_head() {
        let layout = Layout {
            summary: true,
            ..Layout::default()
        };
        let rows = || {
            group(vec![
                asked(1, 1, "Status?"),
                in_turn(said(2, "All green."), 1),
            ])
        };
        let lines = lay_out(rows(), (&BTreeMap::new(), 0), layout);
        assert!(
            matches!(lines[1].row, Row::Head { more: false, .. }),
            "{lines:?}"
        );
        let headline = |text: &str| BTreeMap::from([(TurnIndex(1), done(text))]);
        let first = lay_out(rows(), (&headline("All green."), 1), layout);
        let second = lay_out(rows(), (&headline("CI is green."), 2), layout);
        assert!(
            matches!(first[1].row, Row::Head { more: true, .. }),
            "the reply hides"
        );
        assert_ne!(first[1].key(), second[1].key());
        // A long prompt can be opened even when nothing else hides.
        let long = group(vec![asked(1, 1, &"word ".repeat(40))]);
        let lines = lay_out(long, (&BTreeMap::new(), 0), layout);
        assert!(matches!(lines[1].row, Row::Head { more: true, .. }));
    }

    #[test]
    fn diffs_and_messages_break_a_run() {
        let diff = item(
            3,
            0,
            ThreadContent::Diff {
                path: "a.rs".into(),
                added: 1,
                removed: 0,
                patch: None,
            },
        );
        let rows = group(vec![
            tool(1, "Edit"),
            tool(2, "Edit"),
            diff,
            tool(4, "Bash"),
        ]);
        assert_eq!(shape(&rows), [2, 1, 1]);
        assert_eq!(group(Vec::new()), []);
    }

    #[test]
    fn tally_counts_most_used_first() {
        assert_eq!(
            tally(["Edit", "Bash", "Bash", "Edit", "Bash"]),
            "Bash ×3, Edit ×2"
        );
        assert_eq!(tally(["Read", "Grep", "Read"]), "Read ×2, Grep");
        assert_eq!(tally(["Grep", "Read"]), "Grep, Read");
    }

    #[test]
    fn group_status_surfaces_running_then_failures() {
        use ToolStatus::{Error, Ok, Running, Unknown};
        assert_eq!(group_status([Ok, Ok]), Ok);
        assert_eq!(group_status([Ok, Error, Ok]), Error);
        assert_eq!(group_status([Error, Running]), Running);
        assert_eq!(group_status([Ok, Unknown]), Unknown);
    }

    #[test]
    fn tally_ties_keep_first_use_order() {
        assert_eq!(tally(["Read", "Grep", "Grep", "Read"]), "Read ×2, Grep ×2");
        assert_eq!(
            tally(["Grep", "Read", "Read", "Bash", "Grep", "Bash", "Bash"]),
            "Bash ×3, Grep ×2, Read ×2"
        );
    }

    #[test]
    fn the_window_never_cuts_a_tool_run() {
        let items = [
            said(1, "a"),
            tool(2, "Bash"),
            tool(3, "Bash"),
            tool(4, "Edit"),
            said(5, "b"),
        ];
        let refs: Vec<&ThreadItem> = items.iter().collect();
        // The last 3 would start mid-run at cursor 3: back to its start.
        assert_eq!(window_start(&refs, 3), 1);
        assert_eq!(window_start(&refs, 2), 1);
        assert_eq!(window_start(&refs, 1), 4);
        assert_eq!(window_start(&refs, 4), 1);
        assert_eq!(window_start(&refs, 99), 0);
        assert_eq!(window_start(&[], 3), 0);
    }

    #[test]
    fn group_keys_change_with_their_content() {
        let key = |items: Vec<ThreadItem>| Line::shown(group(items).remove(0)).key();
        let two = key(vec![tool(2, "Bash"), tool(3, "Edit")]);
        let three = key(vec![tool(2, "Bash"), tool(3, "Edit"), tool(4, "Read")]);
        assert_eq!(two.0, three.0, "known by its first call");
        assert_ne!(two, three);
        let mut newer = tool(3, "Edit");
        newer.rev = 9;
        assert_ne!(key(vec![tool(2, "Bash"), newer]), two);
    }

    #[test]
    fn only_a_moving_end_is_news() {
        let at = |cursor, rev| Some((ThreadCursor(cursor), rev));
        assert!(arrived(&at(4, 4), &at(5, 5)), "a new item");
        assert!(arrived(&at(4, 4), &at(4, 7)), "the last item grew");
        assert!(!arrived(&at(4, 4), &at(4, 4)), "show earlier: same end");
        assert!(!arrived(&at(4, 4), &None), "a reload empties");
        assert!(!arrived(&None, &at(9, 9)), "and refills");
        assert!(!arrived(&at(9, 9), &at(3, 3)), "a restart below");
    }

    #[test]
    fn the_open_turn_holds_while_reading_above() {
        let t = |n| Some(TurnIndex(n));
        assert_eq!(hold_open_turn(None, t(2), false), t(2), "first load");
        assert_eq!(hold_open_turn(t(2), t(3), true), t(3), "at the end: moves");
        assert_eq!(hold_open_turn(t(2), t(3), false), t(2), "reading: held");
        assert_eq!(hold_open_turn(t(5), t(1), false), t(1), "restarted below");
        assert_eq!(hold_open_turn(t(5), None, false), None);
    }

    #[test]
    fn fold_choices_are_forgotten_with_the_numbering() {
        let (a, b) = (ThreadEpoch("a".into()), ThreadEpoch("b".into()));
        let mut memory = FoldMemory::default();
        memory.follow_epoch(Some(&a));
        memory.open.insert(ThreadCursor(3), true);
        memory.follow_epoch(None);
        memory.follow_epoch(Some(&a));
        assert_eq!(
            memory.open.get(&ThreadCursor(3)),
            Some(&true),
            "same numbering"
        );
        memory.follow_epoch(Some(&b));
        assert!(memory.open.is_empty(), "cursors were reused");
    }

    #[test]
    fn tool_calls_and_diffs_never_show_hidden_characters() {
        let hidden = |text: &str| text.chars().any(protocol::text::is_hidden_char);
        let tool = shown(ThreadContent::Tool {
            name: "Ba\u{200B}sh".into(),
            input: "rm -rf /tmp/x \u{202E}fdp.exe\tok".into(),
            output: Some("done\u{2066}\n".into()),
            status: ToolStatus::Ok,
        });
        let ThreadContent::Tool {
            name,
            input,
            output,
            ..
        } = tool
        else {
            panic!("still a tool call");
        };
        assert!(!hidden(&name) && !hidden(&input), "{name:?} {input:?}");
        assert_eq!(input, "rm -rf /tmp/x \u{FFFD}fdp.exe\tok");
        assert_eq!(output.as_deref(), Some("done\u{FFFD}\n"));

        let diff = shown(ThreadContent::Diff {
            path: "src/\u{202E}sr.bil".into(),
            added: 1,
            removed: 0,
            patch: Some("+let a = 1; // \u{2067}x\n".into()),
        });
        let ThreadContent::Diff { path, patch, .. } = diff else {
            panic!("still a diff");
        };
        assert!(!hidden(&path), "{path:?}");
        assert!(!hidden(patch.as_deref().unwrap_or_default()), "{patch:?}");
    }

    #[test]
    fn bash_shows_the_command() {
        assert_eq!(detail("Bash", "cargo nextest run"), "cargo nextest run");
        assert_eq!(
            detail("Bash", "\n  cd crates/web\n  just verify\n"),
            "cd crates/web …"
        );
    }

    #[test]
    fn edit_shows_the_file() {
        assert_eq!(
            detail(
                "Edit",
                "/home/dev/passporter/crates/web/src/agent/thread.rs"
            ),
            "…/agent/thread.rs"
        );
        assert_eq!(detail("Read", "src/lib.rs"), "src/lib.rs");
        assert_eq!(detail("Write", "Cargo.toml"), "Cargo.toml");
        assert_eq!(
            detail("MultiEdit", "C:\\repo\\src\\main.rs"),
            "…/src/main.rs"
        );
        assert_eq!(detail("Edit", "  a/b/c.rs\n"), "…/b/c.rs");
    }

    #[test]
    fn file_tools_without_a_path_fall_back_to_the_input() {
        let json = r#"{"old_string":"a/b","new_string":"c/d/e"}"#;
        assert_eq!(detail("Edit", json), json);
        assert_eq!(detail("Read", ""), "");
        assert_eq!(detail("Edit", "{ \"path\": 1 }"), "{ \"path\": 1 }");
        assert_eq!(detail("Edit", "{}"), "{}");
    }

    #[test]
    fn template_paths_are_still_paths() {
        assert_eq!(
            detail("Write", "tmpl/{{cookiecutter.slug}}/src/main.rs"),
            "…/src/main.rs"
        );
        assert_eq!(
            detail("Edit", "{{cookiecutter.slug}}/x"),
            "{{cookiecutter.slug}}/x"
        );
    }

    #[test]
    fn other_tools_show_their_first_line() {
        assert_eq!(detail("Grep", "fn main in crates"), "fn main in crates");
        assert_eq!(
            detail("Task", "Explore: find the router\nmore"),
            "Explore: find the router …"
        );
        assert_eq!(detail("Mystery", ""), "");
    }

    #[test]
    fn diff_lines_are_classified() {
        assert_eq!(diff_line_class("+++ b/x"), "diff-file");
        assert_eq!(diff_line_class("--- a/x"), "diff-file");
        assert_eq!(diff_line_class("+added"), "diff-add");
        assert_eq!(diff_line_class("-gone"), "diff-del");
        assert_eq!(diff_line_class("@@ -1 +1 @@"), "diff-hunk");
        assert_eq!(diff_line_class(" same"), "diff-ctx");
    }
}
