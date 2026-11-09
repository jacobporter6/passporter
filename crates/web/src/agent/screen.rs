//! The agent's visible terminal tail (`AgentScreen`): fetched for as long
//! as the agent screen is open, shown as plain monospace text.
//!
//! The keypad answers what this screen *shows*, so the feed matters for
//! safety: it refreshes whenever the agent changes (and every couple of
//! seconds while it is active and the page is visible), and a refresh
//! asked for while one is in flight is queued, never dropped, so the
//! displayed screen always catches up with the agent's latest seq.

use core::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{AgentId, AgentScreen, AgentStatus, Seq};
use web_sys::VisibilityState;

use crate::session;
use crate::ui::AppState;
use crate::ui::follow::FollowBottom;

/// Poll interval while the agent is active.
const POLL: Duration = Duration::from_secs(2);

/// The seq the keypad may answer with: the displayed screen's, but only
/// while it is at least as new as the agent's listed seq. `None` means the
/// screen is missing or behind, so keys must wait ("updating…").
#[must_use]
pub fn keypad_seq(listed: Option<Seq>, displayed: Option<Seq>) -> Option<Seq> {
    displayed.filter(|shown| listed.is_none_or(|listed| listed <= *shown))
}

/// Single-flight fetching with one queued follow-up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flight {
    in_flight: bool,
    queued: bool,
}

impl Flight {
    /// Asks for a fetch; true if the caller should start one now.
    pub const fn request(&mut self) -> bool {
        if self.in_flight {
            self.queued = true;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    /// A fetch finished; true if a queued one should start now.
    pub const fn finish(&mut self) -> bool {
        if self.queued {
            self.queued = false;
            true
        } else {
            self.in_flight = false;
            false
        }
    }
}

/// Keeps `screen` current for this component's lifetime. Returns the last
/// fetch error, if any.
pub fn screen_feed(
    app: AppState,
    id: AgentId,
    status: Signal<Option<AgentStatus>>,
    listed_seq: Signal<Option<Seq>>,
    screen: RwSignal<Option<AgentScreen>>,
) -> RwSignal<Option<String>> {
    let error = RwSignal::new(None::<String>);
    let flight = StoredValue::new(Flight::default());

    let fetch = move || {
        if app.paired.get_untracked() && flight.try_update_value(Flight::request) == Some(true) {
            run(app, id.clone(), screen, error, flight);
        }
    };

    // On open, and whenever the agent changes (agent_updated).
    {
        let fetch = fetch.clone();
        Effect::new(move |_| {
            status.track();
            listed_seq.track();
            fetch();
        });
    }
    let poll = set_interval_with_handle(
        move || {
            let active = matches!(
                status.get_untracked(),
                Some(AgentStatus::Blocked | AgentStatus::Working)
            );
            if active && page_visible() {
                fetch();
            }
        },
        POLL,
    );
    on_cleanup(move || {
        if let Ok(handle) = poll {
            handle.clear();
        }
    });
    error
}

fn run(
    app: AppState,
    id: AgentId,
    screen: RwSignal<Option<AgentScreen>>,
    error: RwSignal<Option<String>>,
    flight: StoredValue<Flight>,
) {
    spawn_local(async move {
        let result = session::api(app).screen(id.clone()).await;
        match result {
            Ok(read) => {
                let _ = error.try_set(None);
                // Skip identical reads so nothing re-renders.
                if screen.try_with_untracked(|s| s.as_ref() != Some(&read)) == Some(true) {
                    let _ = screen.try_set(Some(read));
                }
            }
            Err(e) => {
                let _ = error.try_set(Some(e.to_string()));
            }
        }
        if flight.try_update_value(Flight::finish) == Some(true) {
            run(app, id, screen, error, flight);
        }
    });
}

/// The screen panel.
#[component]
pub fn ScreenView(
    screen: RwSignal<Option<AgentScreen>>,
    error: RwSignal<Option<String>>,
) -> impl IntoView {
    let follow = FollowBottom::new();
    follow.on_change(move || screen.with(|s| s.as_ref().map(|s| s.revision)));

    view! {
        <div class="screen-panel">
            {move || match screen.get() {
                Some(read) if !read.text.trim().is_empty() => view! {
                    <pre class="screen" aria-label="Agent terminal screen">{read.text}</pre>
                }.into_any(),
                Some(_) => view! { <p class="thread-note">"The screen is empty."</p> }.into_any(),
                None => view! { <div class="skeleton" aria-hidden="true"><span></span><span></span><span></span></div> }.into_any(),
            }}
            {move || error.get().map(|message| view! {
                <p class="inline-error" role="alert">{format!("Couldn't read the screen: {message}")}</p>
            })}
        </div>
    }
}

fn page_visible() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .is_none_or(|d| d.visibility_state() == VisibilityState::Visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypad_waits_for_a_screen_as_new_as_the_agent() {
        assert_eq!(keypad_seq(Some(Seq(5)), Some(Seq(5))), Some(Seq(5)));
        assert_eq!(keypad_seq(Some(Seq(5)), Some(Seq(6))), Some(Seq(6)));
        assert_eq!(keypad_seq(Some(Seq(6)), Some(Seq(5))), None);
        assert_eq!(keypad_seq(Some(Seq(6)), None), None);
        assert_eq!(keypad_seq(None, Some(Seq(2))), Some(Seq(2)));
    }

    #[test]
    fn a_request_during_a_fetch_queues_exactly_one_more() {
        let mut flight = Flight::default();
        assert!(flight.request());
        assert!(!flight.request());
        assert!(!flight.request());
        assert!(flight.finish(), "the queued fetch runs");
        assert!(!flight.finish(), "only one was queued");
        assert!(flight.request(), "idle again");
    }
}
