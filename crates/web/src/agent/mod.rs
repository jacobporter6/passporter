//! An agent's screen (`/agent/:id`): header, the live status line while it
//! works, thread (summary-first, with the TL;DR card under the latest
//! finished turn; or the whole transcript under the card, per the device's
//! setting) or terminal screen, and the one-thumb reply surface (design
//! §6.1, §6.2, §6.4).
//!
//! All agent text is untrusted (design §8) and rendered as text nodes only:
//! never as HTML. Thread messages go through the safe Markdown renderer
//! ([`crate::ui::markdown`]), which builds elements itself.

pub mod choices;
pub mod model;
pub mod pending;
pub mod reply;
pub mod reply_view;
pub mod screen;
pub mod status;
pub mod suggest;
pub mod thread;
pub mod tldr;
pub mod tldr_view;

use leptos::html;
use leptos::prelude::*;
use protocol::{Agent, AgentId, AgentScreen, AgentStatus, Seq};
use wasm_bindgen::JsCast;
use web_sys::Element;

use crate::commands::CommandStrip;
use crate::live::agents::{Segment, neighbours, next_blocked};
use crate::live::{Conn, use_live};
use crate::nav::{History, use_nav};
use crate::routes::AppRoute;
use crate::triage::StatusPill;
use crate::ui::format::time_in_state;
use crate::ui::icons::{self, Icon};
use crate::ui::swipe::{Direction, Move, Swipe, translate_x};
use crate::ui::{EmptyState, NotFound, use_app};
use reply_view::ReplyBar;
use screen::ScreenView;
use status::StatusLineBar;
use thread::{JumpToLatest, ThreadUi, ThreadView};
use tldr_view::TldrCard;

/// The agent screen, at `/agent/:id`.
#[component]
pub fn AgentPage() -> impl IntoView {
    let route = use_nav().route();
    let agent = Memo::new(move |_| match route.get() {
        Some(AppRoute::Agent(id)) => Some(id),
        _ => None,
    });
    move || match agent.get() {
        Some(id) => view! { <AgentView id=id /> }.into_any(),
        None => view! { <NotFound /> }.into_any(),
    }
}

/// Which panel the detail screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// The transcript.
    Thread,
    /// The terminal screen (what the keypad answers).
    Screen,
}

#[component]
fn AgentView(id: AgentId) -> impl IntoView {
    let app = use_app();
    let live = use_live();
    let agent = {
        let id = id.clone();
        Memo::new(move |_| live.agent(&id))
    };
    let screen = RwSignal::new(None::<AgentScreen>);
    // The agent's latest known seq, from the list or a newer screen read.
    let seq = Signal::derive(move || {
        let listed = agent.with(|a| a.as_ref().map(|a| a.seq));
        let read = screen.with(|s| s.as_ref().map(|s| s.seq));
        listed.max(read).unwrap_or(Seq(0))
    });
    let status = Signal::derive(move || agent.with(|a| a.as_ref().map(|a| a.status)));
    let listed_seq = Signal::derive(move || agent.with(|a| a.as_ref().map(|a| a.seq)));
    let finished_seq = Signal::from(Memo::new(move |previous: Option<&Option<Seq>>| {
        tldr::finished_seq(previous.copied().flatten(), status.get(), listed_seq.get())
    }));
    let blocked_at_open = status.get_untracked() == Some(AgentStatus::Blocked);
    let panel = RwSignal::new(if blocked_at_open {
        Panel::Screen
    } else {
        Panel::Thread
    });

    let screen_error = screen::screen_feed(app, id.clone(), status, listed_seq, screen);
    let pending = pending::pending_feed(app, live, id.clone(), status, listed_seq);
    let key_seq = Signal::derive(move || {
        screen::keypad_seq(listed_seq.get(), screen.with(|s| s.as_ref().map(|s| s.seq)))
    });

    let gone = move || agent.with(Option::is_none) && live.conn.get() == Conn::Live;
    let around = {
        let id = id.clone();
        Memo::new(move |_| live.agents.with(|list| neighbours(list, &id)))
    };
    let segment = Signal::derive(move || status.get().map(Segment::for_status));
    // Moving sideways replaces the history entry, so Back returns to triage
    // instead of walking through every agent visited.
    let nav = use_nav();
    let visit = move |target: AgentId| {
        crate::platform::haptic_tick();
        nav.go(&AppRoute::Agent(target), History::Replace);
    };
    let go = move |direction: Direction| {
        let (previous, next) = around.get_untracked();
        if let Some(target) = direction.pick(previous, next) {
            visit(target);
        }
    };
    let panel_ref = NodeRef::<html::Div>::new();
    let swipe = PanelSwipe::new(panel_ref);
    let on_up = move |ev: web_sys::PointerEvent| {
        let landed = swipe
            .up(&ev)
            .and_then(|(direction, target)| lands_on(direction, target, around.get_untracked()));
        if let Some(target) = landed {
            visit(target);
        }
    };
    let marks = ScrollerMarks::new(panel_ref);
    // What arrived out of view, and fold choices: kept across panel switches.
    let thread_ui = ThreadUi::new();
    keep_scrollers_marked(marks, id.clone(), panel, screen);

    // The whole page listens, so a swipe can also start in the header or
    // the tabs; only the panel moves. A click that ends a drag is the
    // drag's, not a tap on what it lifted over.
    view! {
        <section
            class="page page-agent"
            on:pointerdown=move |ev| swipe.down(&ev, around.get_untracked())
            on:pointermove=move |ev| swipe.moved(&ev)
            on:pointerup=on_up
            on:pointercancel=move |ev| swipe.cancel(&ev)
            on:click:capture=move |ev| {
                if swipe.swallows(&ev) {
                    ev.prevent_default();
                    ev.stop_propagation();
                }
            }
        >
            <AgentHeader id=id.clone() agent=agent />
            <Show
                when=move || !gone()
                fallback=|| view! {
                    <EmptyState icon=icons::INBOX title="This agent is gone" text="Its pane closed or its session ended.">
                        <a class="button button-secondary" href=AppRoute::Triage.href()>"Back to agents"</a>
                    </EmptyState>
                }
            >
                <StatusLineBar id=id.clone() agent=agent />
                {
                    // Summary-first, the card sits under its turn instead.
                    let id = id.clone();
                    let full = app.full_transcripts;
                    move || full.get().then(|| view! { <TldrCard id=id.clone() finished_seq=finished_seq /> })
                }
                <div class="panel-row">
                    <PagerButton direction=Direction::Previous around=around segment=segment go=go />
                    <PanelTabs panel=panel />
                    <PagerButton direction=Direction::Next around=around segment=segment go=go />
                </div>
                <div
                    class="agent-panel"
                    role="tabpanel"
                    id="agent-panel"
                    node_ref=panel_ref
                    // Opening a tool call or diff (by tap, or by find in
                    // page) changes what fits, as does showing earlier items.
                    on:click=move |_| marks.schedule()
                    on:toggle:capture=move |_| marks.schedule()
                >
                    {
                        let id = id.clone();
                        move || match panel.get() {
                            Panel::Thread => view! { <ThreadView id=id.clone() ui=thread_ui agent=agent finished_seq=finished_seq /> }.into_any(),
                            Panel::Screen => view! { <ScreenView screen=screen error=screen_error /> }.into_any(),
                        }
                    }
                </div>
                // The reply bar and "jump to latest" stick to the bottom together.
                <div class="reply-dock">
                    <JumpToLatest missed=thread_ui.missed />
                    <CommandStrip id=id.clone() status=status listed_seq=listed_seq />
                    <ReplyBar id=id.clone() status=status seq=seq key_seq=key_seq listed_seq=listed_seq pending=pending panel=panel title=Signal::derive(move || agent.with(|a| a.as_ref().map(|a| a.title.clone()).unwrap_or_default())) />
                </div>
            </Show>
            {move || (!app.paired.get()).then(|| view! {
                <a class="button button-primary button-block" href=AppRoute::Pair.href()>"Connect to your bridge"</a>
            })}
        </section>
    }
}

#[component]
fn AgentHeader(id: AgentId, agent: Memo<Option<Agent>>) -> impl IntoView {
    let app = use_app();
    let live = use_live();
    let fallback_title = id.pane().to_string();
    let title = move || {
        agent.with(|a| {
            a.as_ref()
                .map_or_else(|| fallback_title.clone(), |a| a.title.clone())
        })
    };
    let place = move || {
        agent.with(|a| {
            a.as_ref().map(|a| match &a.worktree {
                Some(w) if *w != a.repo => format!("{} · {w}", a.repo),
                _ => a.repo.clone(),
            })
        })
    };
    let next = {
        let id = id.clone();
        Memo::new(move |_| live.agents.with(|list| next_blocked(list, Some(&id))))
    };
    view! {
        <header class="agent-header">
            <a class="back-link" href=AppRoute::Triage.href() aria-label="Back to agents">
                <Icon paths=icons::BACK />
            </a>
            <div class="agent-header-titles">
                <h1 class="agent-header-title">{title}</h1>
                <p class="agent-header-meta">
                    {move || agent.get().map(|a| view! {
                        <StatusPill status=a.status />
                        <span class="agent-time mono">{move || time_in_state(a.status_since, app.now.get())}</span>
                    })}
                    {move || place().map(|p| view! { <span class="agent-header-place">{p}</span> })}
                </p>
            </div>
            {move || next.get().map(|next| view! {
                <a class="next-blocked" href=AppRoute::Agent(next).href() aria-label="Next blocked agent">
                    <Icon paths=icons::NEXT />
                    <span class="next-blocked-label">"Next"</span>
                </a>
            })}
        </header>
    }
}

#[component]
fn PanelTabs(panel: RwSignal<Panel>) -> impl IntoView {
    let tab = move |which: Panel, label: &'static str| {
        view! {
            <button
                type="button"
                role="tab"
                class="panel-tab"
                aria-selected=move || (panel.get() == which).to_string()
                aria-controls="agent-panel"
                on:click=move |_| panel.set(which)
            >
                {label}
            </button>
        }
    };
    view! {
        <div class="panel-tabs" role="tablist" aria-label="View">
            {tab(Panel::Thread, "Thread")}
            {tab(Panel::Screen, "Screen")}
        </div>
    }
}

/// The accessible fallback for swiping: a button to the previous or next
/// agent in the same triage segment, disabled at the ends.
#[component]
fn PagerButton(
    direction: Direction,
    around: Memo<(Option<AgentId>, Option<AgentId>)>,
    segment: Signal<Option<Segment>>,
    go: impl Fn(Direction) + 'static,
) -> impl IntoView {
    let paths = direction.pick(icons::ARROW_LEFT, icons::ARROW_RIGHT);
    let disabled = move || around.with(|around| pager_disabled(direction, around));
    let label = move || pager_label(direction, segment.get());
    view! {
        <button
            type="button"
            class="icon-button pager-button"
            aria-label=label
            disabled=disabled
            on:click=move |_| go(direction)
        >
            <Icon paths=paths />
        </button>
    }
}

/// Whether the pager button for `direction` has nowhere to go.
fn pager_disabled(
    direction: Direction,
    (previous, next): &(Option<AgentId>, Option<AgentId>),
) -> bool {
    direction.pick(previous, next).is_none()
}

/// The pager button's accessible name. It steps through one segment in
/// list order and stops at the ends, so it is named for the segment
/// ("Next in Blocked"), unlike the header's wrapping "Next blocked agent".
fn pager_label(direction: Direction, segment: Option<Segment>) -> String {
    let word = direction.pick("Previous", "Next");
    match segment {
        Some(segment) => format!("{word} in {}", segment.label()),
        None => format!("{word} agent"),
    }
}

/// Where a released swipe lands: its `target` (fixed when the gesture
/// started) if that is still the neighbour in `direction` now. One that
/// left the segment or the list, or was overtaken, during the drag is not
/// landed on.
fn lands_on(
    direction: Direction,
    target: AgentId,
    (previous, next): (Option<AgentId>, Option<AgentId>),
) -> Option<AgentId> {
    (direction.pick(previous, next).as_ref() == Some(&target)).then_some(target)
}

/// Elements a swipe never starts in: the composer and keypad, text fields
/// and dialogs.
const SWIPE_EXEMPT: &str = "textarea, input, .reply, .link-confirm-backdrop";
/// Blocks that scroll sideways (code, diffs, the terminal, tables): a swipe
/// starts in one only while it (and any scroller around it) fits, so
/// sideways drags still scroll it.
const SIDEWAYS_SCROLLERS: &str = "pre, .md-table-wrap";
/// Class on a sideways scroller that fits: CSS hands its sideways pans to
/// the page (`touch-action`), which the browser reads before any pointer
/// event, so it must be set ahead of the gesture.
const FITS_CLASS: &str = "fits-x";

/// One tracked gesture and the neighbours it can land on, fixed when it
/// starts so the rubber band, the commit and the navigation agree.
#[derive(Debug, Clone)]
struct Gesture {
    pointer: i32,
    swipe: Swipe,
    previous: Option<AgentId>,
    next: Option<AgentId>,
}

/// The live swipe on the agent page: pointer events feed a [`Swipe`] and
/// the panel follows the finger with a transform written once per
/// animation frame, outside the reactive graph, so the drag stays at 60 fps.
#[derive(Debug, Clone, Copy)]
struct PanelSwipe {
    panel: NodeRef<html::Div>,
    gesture: StoredValue<Option<Gesture>>,
    /// An offset waiting for the next animation frame.
    pending: StoredValue<Option<f64>>,
    /// The last gesture reached a sideways drag: the click that follows
    /// it is swallowed, even if it snapped back.
    dragged: StoredValue<bool>,
}

impl PanelSwipe {
    fn new(panel: NodeRef<html::Div>) -> Self {
        Self {
            panel,
            gesture: StoredValue::new(None),
            pending: StoredValue::new(None),
            dragged: StoredValue::new(false),
        }
    }

    /// A pointer went down with these neighbours. Touch and pen only:
    /// mouse users have the buttons, and a mouse drag selects text.
    fn down(
        self,
        ev: &web_sys::PointerEvent,
        (previous, next): (Option<AgentId>, Option<AgentId>),
    ) {
        if ev.is_primary() {
            self.dragged.set_value(false);
        }
        if !ev.is_primary() || ev.pointer_type() == "mouse" || starts_exempt(ev) {
            return;
        }
        let swipe = Swipe::start(
            f64::from(ev.client_x()),
            f64::from(ev.client_y()),
            previous.is_some(),
            next.is_some(),
        );
        self.gesture.set_value(Some(Gesture {
            pointer: ev.pointer_id(),
            swipe,
            previous,
            next,
        }));
    }

    fn moved(self, ev: &web_sys::PointerEvent) {
        let pointer = ev.pointer_id();
        let step = self.gesture.try_update_value(|gesture| {
            let g = gesture.as_mut().filter(|g| g.pointer == pointer)?;
            let was_dragging = g.swipe.is_dragging();
            let step = g
                .swipe
                .moved(f64::from(ev.client_x()), f64::from(ev.client_y()));
            if step == Move::Scroll {
                *gesture = None;
            }
            Some((was_dragging, step))
        });
        let Some(Some((was_dragging, Move::Drag(offset)))) = step else {
            return;
        };
        if !was_dragging {
            self.dragged.set_value(true);
            if let Some(panel) = self.panel.get_untracked() {
                // Keep the pointer even when the finger leaves the panel.
                let _ = panel.set_pointer_capture(pointer);
                let _ = panel.class_list().add_1("swiping");
            }
        }
        self.follow(offset);
    }

    /// The pointer lifted: which way the gesture commits and the agent it
    /// was aimed at, if any. The panel springs back either way (a
    /// committed swipe replaces it).
    fn up(self, ev: &web_sys::PointerEvent) -> Option<(Direction, AgentId)> {
        let mut gesture = self.take(ev)?;
        self.reset();
        let direction = gesture
            .swipe
            .end(f64::from(ev.client_x()), f64::from(ev.client_y()))?;
        Some((direction, direction.pick(gesture.previous, gesture.next)?))
    }

    /// Whether `ev` is the click that ends a drag, which must not reach
    /// what the finger lifted over (a tool call, a link). Clicks from the
    /// keyboard (`detail` 0) always pass.
    fn swallows(self, ev: &web_sys::MouseEvent) -> bool {
        ev.detail() != 0 && self.dragged.try_update_value(std::mem::take) == Some(true)
    }

    /// The browser took the tracked pointer (a scroll, a pinch): spring
    /// back. Other pointers' cancels leave the drag alone.
    fn cancel(self, ev: &web_sys::PointerEvent) {
        if self.take(ev).is_some() {
            self.reset();
        }
    }

    /// Removes and returns the gesture, if `ev` is its pointer.
    fn take(self, ev: &web_sys::PointerEvent) -> Option<Gesture> {
        let pointer = ev.pointer_id();
        self.gesture
            .try_update_value(|g| {
                if g.as_ref().is_some_and(|g| g.pointer == pointer) {
                    g.take()
                } else {
                    None
                }
            })
            .flatten()
    }

    /// Ends the gesture and springs the panel back.
    fn reset(self) {
        self.gesture.set_value(None);
        self.pending.set_value(None);
        if let Some(panel) = self.panel.try_get_untracked().flatten() {
            let _ = panel.class_list().remove_1("swiping");
            let _ = web_sys::HtmlElement::style(&panel).remove_property("transform");
        }
    }

    /// Moves the panel to `offset` on the next frame, coalescing moves.
    fn follow(self, offset: f64) {
        let queued = self.pending.get_value().is_some();
        self.pending.set_value(Some(offset));
        if queued {
            return;
        }
        request_animation_frame(move || {
            let Some(offset) = self.pending.try_get_value().flatten() else {
                return;
            };
            self.pending.set_value(None);
            if let Some(panel) = self.panel.try_get_untracked().flatten() {
                let _ = web_sys::HtmlElement::style(&panel)
                    .set_property("transform", &translate_x(offset));
            }
        });
    }
}

/// Whether a gesture starting at `ev`'s target belongs to something else:
/// a text field or dialog, or a sideways scroller with somewhere to scroll.
fn starts_exempt(ev: &web_sys::PointerEvent) -> bool {
    let Some(target) = ev.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
        return false;
    };
    target.closest(SWIPE_EXEMPT).ok().flatten().is_some()
        || closest_scroller(&target).is_some_and(|el| scrolls_sideways(&el))
}

/// The nearest sideways scroller at or around `el`.
fn closest_scroller(el: &Element) -> Option<Element> {
    el.closest(SIDEWAYS_SCROLLERS).ok().flatten()
}

/// Whether `scroller`, or any sideways scroller around it, overflows.
fn scrolls_sideways(scroller: &Element) -> bool {
    let mut current = Some(scroller.clone());
    while let Some(el) = current {
        if el.scroll_width() > el.client_width() {
            return true;
        }
        current = el.parent_element().and_then(|p| closest_scroller(&p));
    }
    false
}

/// Keeps the panel's sideways scrollers tagged with [`FITS_CLASS`] when
/// they fit, refreshed at most once per animation frame however many
/// changes ask for it.
#[derive(Debug, Clone, Copy)]
struct ScrollerMarks {
    panel: NodeRef<html::Div>,
    /// A refresh is waiting for the next animation frame.
    queued: StoredValue<bool>,
}

impl ScrollerMarks {
    fn new(panel: NodeRef<html::Div>) -> Self {
        Self {
            panel,
            queued: StoredValue::new(false),
        }
    }

    /// Refreshes the tags on the next frame, after layout settles. The
    /// frame may come after the page is gone: then it does nothing.
    fn schedule(self) {
        if self.queued.try_update_value(|q| std::mem::replace(q, true)) != Some(false) {
            return;
        }
        request_animation_frame(move || {
            if self.queued.try_set_value(false).is_none() {
                self.mark();
            }
        });
    }

    /// Tags every sideways scroller in the panel that fits, and untags
    /// those that overflow.
    fn mark(self) {
        let Some(panel) = self.panel.try_get_untracked().flatten() else {
            return;
        };
        let Ok(scrollers) = panel.query_selector_all(SIDEWAYS_SCROLLERS) else {
            return;
        };
        for el in
            (0..scrollers.length()).filter_map(|i| scrollers.item(i)?.dyn_into::<Element>().ok())
        {
            let _ = el
                .class_list()
                .toggle_with_force(FITS_CLASS, !scrolls_sideways(&el));
        }
    }
}

/// Keeps [`ScrollerMarks`] current: after the panel's content changes
/// (tab switch, this agent's thread, a new screen) and on resize. Taps and
/// opened tool calls are handled by the panel's own listeners.
fn keep_scrollers_marked(
    marks: ScrollerMarks,
    id: AgentId,
    panel: RwSignal<Panel>,
    screen: RwSignal<Option<AgentScreen>>,
) {
    let live = use_live();
    // Only this agent's thread, and only when it gains or updates items
    // (every merged event moves the resume cursor).
    let thread = Memo::new(move |_| {
        live.threads
            .with(|t| t.get(&id).map(|m| (m.len(), m.resume())))
    });
    Effect::new(move |_| {
        panel.track();
        screen.track();
        thread.track();
        marks.schedule();
    });
    let resize = window_event_listener(leptos::ev::resize, move |_| marks.schedule());
    on_cleanup(move || resize.remove());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AgentId {
        s.parse().unwrap()
    }

    #[test]
    fn a_swipe_lands_only_on_the_current_neighbour() {
        let around = (Some(id("w1:p1")), Some(id("w1:p3")));
        assert_eq!(
            lands_on(Direction::Next, id("w1:p3"), around.clone()),
            Some(id("w1:p3"))
        );
        assert_eq!(
            lands_on(Direction::Previous, id("w1:p1"), around.clone()),
            Some(id("w1:p1"))
        );
        // The target left the segment, or another agent moved in between.
        assert_eq!(lands_on(Direction::Next, id("w1:p4"), around.clone()), None);
        // Wrong side.
        assert_eq!(lands_on(Direction::Previous, id("w1:p3"), around), None);
        // The current agent left the list: nothing is anyone's neighbour.
        assert_eq!(lands_on(Direction::Next, id("w1:p3"), (None, None)), None);
    }

    #[test]
    fn pager_buttons_disable_at_the_ends() {
        let first = (None, Some(id("w1:p2")));
        assert!(pager_disabled(Direction::Previous, &first));
        assert!(!pager_disabled(Direction::Next, &first));
        let last = (Some(id("w1:p1")), None);
        assert!(!pager_disabled(Direction::Previous, &last));
        assert!(pager_disabled(Direction::Next, &last));
        assert!(pager_disabled(Direction::Next, &(None, None)));
    }

    #[test]
    fn pager_labels_name_the_segment_not_the_next_blocked_link() {
        assert_eq!(
            pager_label(Direction::Next, Some(Segment::Blocked)),
            "Next in Blocked"
        );
        assert_eq!(
            pager_label(Direction::Previous, Some(Segment::Working)),
            "Previous in Working"
        );
        assert_eq!(pager_label(Direction::Next, None), "Next agent");
        assert_ne!(
            pager_label(Direction::Next, Some(Segment::Blocked)),
            "Next blocked agent"
        );
    }
}
