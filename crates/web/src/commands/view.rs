//! The command confirm, drawn by the terminal WASM (`src/bin/terminal.rs`,
//! loaded on the first chip tap like an extras surface:
//! [`crate::extras::page::Extra`] with [`crate::extras::Surface::Command`]): the exact command,
//! where the agent asked for it, and the two ways to run it; after a
//! new-terminal run, "Watch" shows that terminal's last lines with a few
//! keys. Its feed is a [`Feed`] as JSON.
//!
//! It reaches the page the way the extras module does, through the same
//! host ([`crate::extras::mount`]: [`mount::init`], [`mount::api`],
//! [`mount::toast`], [`mount::close`]).

use core::cell::RefCell;
use core::time::Duration;

use leptos::mount::{UnmountHandle, mount_to};
use leptos::prelude::*;
use leptos::tachys::view::any_view::AnyViewState;
use leptos::task::spawn_local;
use protocol::{
    AllowedKey, CommandCandidate, KeySequence, Origin, RunTarget, Terminal, TerminalKeysRequest,
};
use web_sys::HtmlElement;

use super::{Attempt, Feed, Next, here_unavailable, next, order, suggests};
use crate::extras::Surface;
use crate::extras::mount;
use crate::idgen::random_key;
use crate::ui::format::now;

thread_local! {
    /// The last run request, until the bridge decides it: kept for the
    /// app session, so a confirm after "no answer" repeats the same key.
    static ATTEMPT: RefCell<Attempt> = RefCell::new(Attempt::default());
    /// The confirm on screen, and its feed.
    static MOUNTED: RefCell<Option<Mounted>> = const { RefCell::new(None) };
}

/// The confirm as mounted.
struct Mounted {
    _view: UnmountHandle<AnyViewState>,
    feed: ArcRwSignal<Option<Feed>>,
}

/// How often a watched terminal is read.
const WATCH_POLL: Duration = Duration::from_millis(1500);

/// The keys the watch view offers, with their labels.
const KEYS: [(AllowedKey, &str); 5] = [
    (AllowedKey::CtrlC, "Ctrl+C"),
    (AllowedKey::Enter, "Enter"),
    (AllowedKey::Esc, "Esc"),
    (AllowedKey::Up, "↑"),
    (AllowedKey::Down, "↓"),
];

/// Where the confirm is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Asking, or sending.
    Confirm,
    /// Running in a new terminal: offer to watch it.
    Started(Terminal),
    /// Watching that terminal.
    Watching(Terminal),
}

/// Draws the confirm in `root`, fed `feed` (a [`Feed`] as JSON),
/// replacing any earlier one.
pub fn mount(root: HtmlElement, feed: &str) {
    unmount();
    let feed = ArcRwSignal::new(parse(feed));
    let shown = feed.clone();
    let view = mount_to(root, move || {
        view! { <Confirm feed=RwSignal::from(shown).read_only() /> }.into_any()
    });
    MOUNTED.set(Some(Mounted { _view: view, feed }));
}

/// Hands the confirm a new feed (JSON). False if it isn't mounted.
pub fn update(feed: &str) -> bool {
    // Set outside the borrow: the view may close itself in response.
    let signal = MOUNTED.with_borrow(|m| m.as_ref().map(|m| m.feed.clone()));
    signal.map(|signal| signal.set(parse(feed))).is_some()
}

fn parse(json: &str) -> Option<Feed> {
    serde_json::from_str(json).ok()
}

/// Removes the confirm, if mounted.
pub fn unmount() {
    // Dropped outside the borrow: cleanups may run arbitrary code.
    let mounted = MOUNTED.take();
    drop(mounted);
}

/// Asks the page to close the confirm.
fn close() {
    mount::close(Surface::Command);
}

#[component]
fn Confirm(feed: ReadSignal<Option<Feed>>) -> impl IntoView {
    let phase = RwSignal::new(Phase::Confirm);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let chosen = move || -> Option<(CommandCandidate, bool)> {
        feed.with(|f| {
            let f = f.as_ref()?;
            Some((f.command.as_ref()?.candidate(), f.ready))
        })
    };
    let run = move |target: RunTarget| {
        let Some((agent, (candidate, _))) = feed
            .with_untracked(|f| f.as_ref().map(|f| f.agent.clone()))
            .zip(chosen())
        else {
            return;
        };
        if busy.get_untracked() {
            return;
        }
        let request =
            ATTEMPT.with_borrow_mut(|a| a.request(&agent, &candidate, target, random_key, now()));
        let Some(request) = request else {
            error.set(Some("This browser can't make a request id.".to_owned()));
            return;
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let next = next(mount::api().run_command(&agent, &request).await);
            if next.is_final() {
                ATTEMPT.with_borrow_mut(Attempt::settle);
            }
            let _ = busy.try_set(false);
            match next {
                Next::Sent => {
                    mount::toast("Sent to the agent.");
                    close();
                }
                Next::Started(terminal) => {
                    let _ = phase.try_set(Phase::Started(terminal));
                }
                Next::Retry(message) | Next::Show(message) => {
                    let _ = error.try_set(Some(message));
                }
            }
        });
    };
    let ask = move || {
        let Some((candidate, ready)) = chosen() else {
            return view! {
                <h2 id="command-confirm-title" class="command-confirm-title">"That command is gone"</h2>
                <p class="confirm-text">"The agent's latest message no longer has it. Nothing ran."</p>
            }
            .into_any();
        };
        let unavailable = here_unavailable(&candidate, ready);
        let buttons = order(candidate.intent).map(|target| {
            let off = target == RunTarget::Here && unavailable.is_some();
            let text = match target {
                RunTarget::Here => "Run here (!)",
                RunTarget::NewTerminal => "Run in new terminal",
            };
            view! {
                <button
                    type="button"
                    class="button"
                    class:button-primary=!off
                    class:button-secondary=off
                    disabled=move || off || busy.get()
                    on:click=move |_| run(target)
                >
                    {text}
                </button>
            }
        });
        view! {
            <h2 id="command-confirm-title" class="command-confirm-title">"Run this command?"</h2>
            <pre class="command-text"><code>{candidate.text}</code></pre>
            <p class="field-help">{suggests(candidate.intent)} " " {unavailable}</p>
            <p class="field-help">"It runs as you on the dev machine. Check it: agent text is not trusted."</p>
            <div class="command-actions">{buttons.into_iter().collect_view()}</div>
        }
        .into_any()
    };
    let dismiss = move || {
        if !busy.get_untracked() {
            close();
        }
    };
    view! {
        <div class="link-confirm-backdrop" on:click=move |_| dismiss()>
            <div
                class="confirm link-confirm command-confirm"
                role="alertdialog"
                aria-modal="true"
                aria-labelledby="command-confirm-title"
                on:click=|ev| ev.stop_propagation()
                on:keydown=move |ev: leptos::ev::KeyboardEvent| if ev.key() == "Escape" { dismiss() }
            >
                {move || match phase.get() {
                    Phase::Confirm => ask(),
                    Phase::Started(terminal) => {
                        let label = terminal.label.clone();
                        view! {
                            <h2 id="command-confirm-title" class="command-confirm-title">"Running in a new terminal"</h2>
                            <p class="confirm-text">"It's in the herdr tab “" <span class="mono">{label}</span> "”."</p>
                            <button type="button" class="button button-primary" on:click=move |_| phase.set(Phase::Watching(terminal.clone()))>"Watch"</button>
                        }.into_any()
                    }
                    Phase::Watching(terminal) => view! { <Watch terminal=terminal problem=error /> }.into_any(),
                }}
                <p class="field-error" role="alert">{move || error.get()}</p>
                <div class="confirm-actions">
                    <button type="button" class="button button-secondary" disabled=move || busy.get() on:click=move |_| close()>
                        {move || if phase.with(|p| *p == Phase::Confirm) { "Cancel" } else { "Close" }}
                    </button>
                </div>
            </div>
        </div>
    }
}

/// The terminal's last lines (untrusted output, plain text), read every
/// [`WATCH_POLL`] while shown, and a few keys. Problems go to `problem`.
#[component]
fn Watch(terminal: Terminal, problem: RwSignal<Option<String>>) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let label = terminal.label;
    let pane = terminal.pane;
    let read = {
        let pane = pane.clone();
        move || {
            let pane = pane.clone();
            spawn_local(async move {
                match mount::api().terminal_screen(pane).await {
                    Ok(read) => {
                        let _ = problem.try_set(None);
                        if text.try_with_untracked(|t| *t != read.text) == Some(true) {
                            let _ = text.try_set(read.text);
                        }
                    }
                    Err(e) => {
                        let _ = problem.try_set(Some(format!("Couldn't read the terminal: {e}")));
                    }
                }
            });
        }
    };
    read();
    let poll = set_interval_with_handle(read, WATCH_POLL);
    on_cleanup(move || {
        if let Ok(handle) = poll {
            handle.clear();
        }
    });
    let send = move |key: AllowedKey| {
        let (Some(idempotency_key), Ok(keys)) = (random_key(), KeySequence::new(vec![key])) else {
            return;
        };
        let request = TerminalKeysRequest {
            idempotency_key,
            origin: Origin::App,
            created_at: now(),
            keys,
        };
        let pane = pane.clone();
        spawn_local(async move {
            if let Next::Retry(message) | Next::Show(message) =
                next(mount::api().terminal_keys(&pane, &request).await)
            {
                let _ = problem.try_set(Some(message));
            }
        });
    };
    view! {
        <h2 id="command-confirm-title" class="command-confirm-title mono">{label}</h2>
        <pre class="screen terminal-screen" aria-label="Terminal output">{move || text.get()}</pre>
        <div class="terminal-keys" role="group" aria-label="Keys">
            {KEYS.into_iter().map(|(key, name)| {
                let send = send.clone();
                view! {
                    <button type="button" class="button button-secondary button-small" on:click=move |_| send(key)>{name}</button>
                }
            }).collect_view()}
        </div>
    }
}
