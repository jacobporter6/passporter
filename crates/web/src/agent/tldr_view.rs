//! The TL;DR card at the top of the agent screen (design §6.4): Outcome,
//! Changed, Decide and Risks for the latest finished turn, collapsible to
//! the outcome line, with "read aloud" through the browser's speech
//! synthesis.
//!
//! The card paints from the `IndexedDB` copy first (so it works offline),
//! then asks the bridge on open, whenever a turn may have finished (the
//! agent's seq while it is ready, see [`super::tldr::finished_seq`]), on
//! every `tldr_ready` for the agent and after a stream resync. While the
//! bridge is summarizing it also asks now and then, in case the stream is
//! down or the turn turned out short (no event then). The rules live in
//! [`super::tldr`].
//!
//! The same card shows a turn the user asked to summarise, inline under
//! its head: asked for by number (`?turn=`), any length, without the
//! offline copy, and read only until it has an answer.

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{AgentId, Seq, TurnIndex};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use super::screen::Flight;
use super::tldr::{CacheOp, Card, CardState, LatestTldrKey, Section, SectionKind};
use crate::live::use_live;
use crate::session;
use crate::ui::format::{self, local_clock};
use crate::ui::{AppState, use_app};

/// How long to wait before asking again while the bridge summarizes. A
/// fallback: `tldr_ready` normally triggers the read first.
const SUMMARIZING_RETRY: core::time::Duration = core::time::Duration::from_secs(15);

/// The TL;DR card for agent `id`'s latest finished turn, or `turn` when
/// the user asked to summarise it (`so_far`: the turn is in progress, so
/// the TL;DR is provisional); renders nothing when there is no TL;DR.
/// `finished_seq` is [`super::tldr::finished_seq`] of the agent.
#[component]
pub fn TldrCard(
    id: AgentId,
    finished_seq: Signal<Option<Seq>>,
    #[prop(optional_no_strip)] turn: Option<TurnIndex>,
    #[prop(optional)] so_far: bool,
) -> impl IntoView {
    let app = use_app();
    let state = RwSignal::new(CardState::Unknown);
    tldr_feed(app, id, finished_seq, turn, state);
    let label = if so_far { "TL;DR so far" } else { "TL;DR" };
    let body_id = turn.map_or_else(|| "tldr-body".to_owned(), |t| format!("tldr-body-{}", t.0));
    // Content and age are separate memos: confirming a cached summary only
    // drops "as of", without rebuilding the card (or stopping read-aloud).
    let card = Memo::new(move |_| state.with(|s| s.card(turn.is_some())));
    let as_of = Memo::new(move |_| state.with(CardState::as_of));
    // Kept across rebuilds: a new summary opens as the user left the last.
    let expanded = RwSignal::new(true);
    move || {
        match card.get() {
        None => ().into_any(),
        Some(card @ (Card::Summarizing | Card::Busy | Card::Unavailable)) => {
            let lede = match (card, turn) {
                (Card::Busy, _) => "Busy with another summary — try again shortly.",
                (Card::Unavailable, _) => "No TL;DR for this turn right now.",
                (_, Some(_)) => "Summarizing this turn…",
                (_, None) => "Summarizing the last turn…",
            };
            view! {
                <section class="tldr tldr-summarizing" aria-label="TL;DR" aria-live="polite">
                    <span class="tldr-label">{label}</span>
                    <span class="tldr-lede">{lede}</span>
                </section>
            }
            .into_any()
        }
        Some(Card::Summary { sections, speech }) => view! {
            <Summary sections=sections speech=speech as_of=as_of expanded=expanded label=label body_id=body_id.clone() />
        }
        .into_any(),
    }
    }
}

#[component]
fn Summary(
    sections: Vec<Section>,
    speech: String,
    as_of: Memo<Option<protocol::Timestamp>>,
    expanded: RwSignal<bool>,
    label: &'static str,
    body_id: String,
) -> impl IntoView {
    let (lede, rest): (Vec<Section>, Vec<Section>) = sections
        .into_iter()
        .partition(|s| s.kind == SectionKind::Outcome);
    let lede = lede
        .into_iter()
        .flat_map(|s| s.lines)
        .collect::<Vec<_>>()
        .join(" ");
    view! {
        <section class="tldr" aria-label="TL;DR">
            <button
                type="button"
                class="tldr-toggle"
                aria-expanded=move || expanded.get().to_string()
                aria-controls=body_id.clone()
                on:click=move |_| expanded.update(|e| *e = !*e)
            >
                <span class="tldr-label">{label}</span>
                <span class="tldr-lede">{lede}</span>
            </button>
            <div class="tldr-body" id=body_id hidden=move || !expanded.get()>
                {rest.into_iter().map(|section| view! { <SectionView section=section /> }).collect_view()}
                <div class="tldr-foot">
                    {move || as_of.get().map(|at| view! { <span class="tldr-as-of">{format!("as of {}", local_clock(at))}</span> })}
                    <ReadAloud text=speech />
                </div>
            </div>
        </section>
    }
}

#[component]
fn SectionView(section: Section) -> impl IntoView {
    let kind = section.kind;
    view! {
        <div class=kind.class()>
            <h2 class="tldr-heading">{kind.label()}</h2>
            {if kind == SectionKind::Decide {
                view! { <p class="tldr-text">{section.lines.join(" ")}</p> }.into_any()
            } else {
                view! {
                    <ul class="tldr-list" role="list">
                        {section.lines.into_iter().map(|line| view! { <li>{line}</li> }).collect_view()}
                    </ul>
                }
                .into_any()
            }}
        </div>
    }
}

/// "Read aloud" / "Stop", hidden where the browser has no speech synthesis.
#[component]
fn ReadAloud(text: String) -> impl IntoView {
    let speaking = RwSignal::new(false);
    on_cleanup(speech::stop);
    let toggle = move |_| {
        if speaking.get_untracked() {
            speech::stop();
            speaking.set(false);
        } else {
            let done = move || {
                let _ = speaking.try_set(false);
            };
            speaking.set(speech::speak(&text, done));
        }
    };
    speech::available().then(|| {
        view! {
            <button
                type="button"
                class="button button-secondary button-small tldr-speak"
                aria-pressed=move || speaking.get().to_string()
                on:click=toggle
            >
                {move || if speaking.get() { "Stop" } else { "Read aloud" }}
            </button>
        }
    })
}

/// The shared pieces of one card's feed.
#[derive(Debug, Clone, Copy)]
struct Feed {
    app: AppState,
    /// The turn asked for by number, if any (else the latest finished).
    turn: Option<TurnIndex>,
    state: RwSignal<CardState>,
    flight: StoredValue<Flight>,
    /// A "still summarizing" retry is scheduled: at most one at a time,
    /// however many reads come back summarizing.
    retry_scheduled: StoredValue<bool>,
}

/// Keeps `state` current for this component's lifetime: the cached copy
/// first, then the bridge's answers, written back to the cache. For a
/// `turn` asked for by number: the bridge's answers only, until it has
/// one.
fn tldr_feed(
    app: AppState,
    id: AgentId,
    finished_seq: Signal<Option<Seq>>,
    turn: Option<TurnIndex>,
    state: RwSignal<CardState>,
) {
    if turn.is_none() {
        let id = id.clone();
        spawn_local(async move {
            let Ok(store) = app.store().await else {
                return;
            };
            let cached = store.get(&LatestTldrKey(id)).await.ok().flatten();
            let _ = state.try_update(|s| *s = core::mem::take(s).with_cached(cached));
        });
    }
    let feed = Feed {
        app,
        turn,
        state,
        flight: StoredValue::new(Flight::default()),
        retry_scheduled: StoredValue::new(false),
    };
    let live = use_live();
    Effect::new(move |_| {
        if turn.is_none() {
            finished_seq.track();
        }
        app.online.track();
        // Tracked: a `tldr_ready` for this agent (or a resync) triggers a
        // read.
        let _ = live.tldr_generation(&id);
        if turn.is_none() || state.with_untracked(CardState::is_pending) {
            request(&feed, id.clone());
        }
    });
}

/// Starts a read unless one is running (then queues one).
fn request(feed: &Feed, id: AgentId) {
    if feed.app.paired.get_untracked()
        && feed.flight.try_update_value(Flight::request) == Some(true)
    {
        run(feed, id);
    }
}

fn run(feed: &Feed, id: AgentId) {
    let feed = *feed;
    spawn_local(async move {
        let read = session::api(feed.app).tldr(id.clone(), feed.turn).await;
        let Some((next, op)) = feed
            .state
            .try_with_untracked(|s| s.with_read(read, format::now()))
        else {
            return;
        };
        let summarizing = next.is_summarizing();
        if feed.state.try_with_untracked(|s| *s != next) == Some(true) {
            let _ = feed.state.try_set(next);
        }
        if feed.turn.is_none() {
            write_cache(feed.app, &id, op).await;
        }
        if summarizing
            && feed
                .retry_scheduled
                .try_update_value(|s| !core::mem::replace(s, true))
                == Some(true)
        {
            let id = id.clone();
            set_timeout(
                move || {
                    let _ = feed.retry_scheduled.try_set_value(false);
                    if feed.state.try_with_untracked(CardState::is_summarizing) == Some(true) {
                        request(&feed, id);
                    }
                },
                SUMMARIZING_RETRY,
            );
        }
        if feed.flight.try_update_value(Flight::finish) == Some(true) {
            // A card for a turn asked for by number is not read again once
            // answered (a read of the turn in progress could make another
            // provisional TL;DR): drop the queued read.
            let answered = feed.turn.is_some()
                && feed.state.try_with_untracked(CardState::is_pending) == Some(false);
            if answered {
                let _ = feed.flight.try_update_value(Flight::finish);
            } else {
                run(&feed, id);
            }
        }
    });
}

/// Applies `op` to the cached copy; a store failure only costs the offline
/// copy.
async fn write_cache(app: AppState, id: &AgentId, op: CacheOp) {
    if op == CacheOp::Keep {
        return;
    }
    let Ok(store) = app.store().await else {
        return;
    };
    let key = LatestTldrKey(id.clone());
    let _ = match op {
        CacheOp::Put(snapshot) => store.put(&key, &snapshot).await,
        CacheOp::Delete => store.delete(&key).await,
        CacheOp::Keep => Ok(()),
    };
}

/// The browser's `speechSynthesis`, used for one utterance at a time.
mod speech {
    use super::{Closure, JsCast};

    fn synthesis() -> Option<web_sys::SpeechSynthesis> {
        let window = web_sys::window()?;
        // Absent in some webviews: check before calling the getter.
        js_sys::Reflect::has(&window, &"speechSynthesis".into())
            .ok()
            .filter(|has| *has)?;
        window.speech_synthesis().ok()
    }

    /// Whether this browser can read aloud.
    pub(super) fn available() -> bool {
        synthesis().is_some()
    }

    /// Reads `text` aloud, replacing anything being read; `done` runs once
    /// when it ends or fails. False if speech is unavailable.
    pub(super) fn speak(text: &str, done: impl FnOnce() + 'static) -> bool {
        let Some(synthesis) = synthesis() else {
            return false;
        };
        let Ok(utterance) = web_sys::SpeechSynthesisUtterance::new_with_text(text) else {
            return false;
        };
        // One callback for both events, freed by its single call: it
        // unhooks itself first, so the other event (if a browser fires
        // both) finds no handler.
        let unhook = utterance.clone();
        let finished = Closure::once_into_js(move || {
            unhook.set_onend(None);
            unhook.set_onerror(None);
            done();
        });
        utterance.set_onend(Some(finished.unchecked_ref()));
        utterance.set_onerror(Some(finished.unchecked_ref()));
        synthesis.cancel();
        synthesis.speak(&utterance);
        true
    }

    /// Stops reading aloud.
    pub(super) fn stop() {
        if let Some(synthesis) = synthesis() {
            synthesis.cancel();
        }
    }
}
