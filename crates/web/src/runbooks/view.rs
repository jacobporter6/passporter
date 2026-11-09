//! The Runbooks sheet, drawn by the extras module
//! ([`crate::extras::mount`]): every runbook with its exact command and
//! latest run, the confirm, and the followed run's output. Its feed
//! ([`Feed`]) is what only the page knows: a spoken offer and the
//! stream's runs.

use core::cell::RefCell;

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{Runbook, RunbookName, RunbookRun};
use wasm_bindgen::JsValue;
use web_sys::HtmlElement;

use super::sheet::{self, Attempt, Feed, Next};
use crate::extras::mount::{self, Mounted};
use crate::extras::{Ask, Surface};
use crate::idgen::random_key;
use crate::ui::format::now;
use crate::ui::icons::{self, Icon};

thread_local! {
    /// The last run request, until the bridge decides it: kept for the
    /// app session, so a confirm after "no answer" repeats the same key.
    static ATTEMPT: RefCell<Attempt> = RefCell::new(Attempt::default());
    /// The bridge's runbooks once loaded, whether the last load failed,
    /// and the runbook whose run the sheet follows: kept while the page
    /// lives, so the sheet reopens with them while it refreshes.
    static LIST: ArcRwSignal<Option<Vec<Runbook>>> = ArcRwSignal::new(None);
    static FAILED: ArcRwSignal<bool> = ArcRwSignal::new(false);
    static FOLLOWED: ArcRwSignal<Option<RunbookName>> = ArcRwSignal::new(None);
}

/// The sheet's state.
#[derive(Debug, Clone, Copy)]
struct State {
    list: RwSignal<Option<Vec<Runbook>>>,
    failed: RwSignal<bool>,
    followed: RwSignal<Option<RunbookName>>,
    /// The runbook a tap offered for a confirm (a spoken offer comes in
    /// the feed).
    tapped: RwSignal<Option<RunbookName>>,
    feed: RwSignal<Feed>,
}

impl State {
    /// Fetches the list (fresh commands and latest runs), and hands it to
    /// the page for dictation to match against.
    fn refresh(self) {
        spawn_local(async move {
            match mount::api().runbooks().await {
                Ok(list) => {
                    if let Ok(json) = serde_json::to_string(&list) {
                        mount::ask(Ask::RunbooksListed, &json.into(), &JsValue::UNDEFINED);
                    }
                    let _ = self.list.try_set(Some(list.runbooks));
                    let _ = self.failed.try_set(false);
                }
                Err(_) => {
                    let _ = self.failed.try_set(true);
                }
            }
        });
    }

    /// The latest run of `name` known: the stream's or the list's,
    /// whichever is fresher ([`sheet::fresher`]).
    fn latest_run(self, name: &RunbookName) -> Option<RunbookRun> {
        let listed = self.list.with(|list| {
            list.as_ref()
                .and_then(|l| l.iter().find(|r| r.name == *name))
                .and_then(|r| r.run.clone())
        });
        let streamed = self
            .feed
            .with(|feed| feed.runs.iter().find(|r| r.runbook == *name).cloned());
        sheet::fresher(streamed, listed)
    }

    /// Drops the offer without running it; a spoken one's words go back
    /// to where they were dictated, through the page.
    fn decline(self) {
        self.tapped.set(None);
        if self.feed.with_untracked(|feed| feed.offer.is_some()) {
            mount::ask(Ask::Declined, &JsValue::UNDEFINED, &JsValue::UNDEFINED);
        }
    }
}

/// Draws the sheet in `root`, fed `feed` (a [`Feed`] as JSON).
pub fn mount(root: HtmlElement, feed: &str) -> Mounted {
    let feed = ArcRwSignal::new(parse(feed));
    let take = feed.clone();
    Mounted::new(
        root,
        move || {
            let state = State {
                list: LIST.with(|s| RwSignal::from(s.clone())),
                failed: FAILED.with(|s| RwSignal::from(s.clone())),
                followed: FOLLOWED.with(|s| RwSignal::from(s.clone())),
                tapped: RwSignal::new(None),
                feed: RwSignal::from(feed),
            };
            view! { <Sheet state=state /> }.into_any()
        },
        move |json| {
            take.set(parse(json));
            true
        },
    )
}

fn parse(json: &str) -> Feed {
    serde_json::from_str(json).unwrap_or_default()
}

#[component]
fn Sheet(state: State) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    // Fetch now, and again after every stream resync: updates may have
    // been missed while disconnected (the stream forgets its runs then).
    let resyncs = Memo::new(move |_| state.feed.with(|feed| feed.resyncs));
    Effect::new(move |_| {
        resyncs.track();
        state.refresh();
    });

    let close = move || {
        if !busy.get_untracked() {
            mount::close(Surface::Runbooks);
        }
    };
    let run = move |name: RunbookName| {
        if busy.get_untracked() {
            return;
        }
        let Some(request) = ATTEMPT.with_borrow_mut(|a| a.request(&name, random_key, now())) else {
            error.set(Some("This browser can't make a request id.".to_owned()));
            return;
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let next = sheet::next(mount::api().run_runbook(&name, &request).await);
            if next.is_final() {
                ATTEMPT.with_borrow_mut(Attempt::settle);
            }
            let _ = busy.try_set(false);
            match next {
                Next::Follow(run) => {
                    if let Ok(json) = serde_json::to_string(&run) {
                        mount::ask(Ask::RunbookStarted, &json.into(), &JsValue::UNDEFINED);
                    }
                    let _ = state.tapped.try_set(None);
                    let _ = state.followed.try_set(Some(name));
                }
                Next::Retry(message) | Next::Show(message) => {
                    let _ = error.try_set(Some(message));
                }
            }
        });
    };
    let pick = move |runbook: &Runbook| {
        error.set(None);
        if runbook.confirm {
            state.tapped.set(Some(runbook.name.clone()));
        } else {
            run(runbook.name.clone());
        }
    };
    let on_key = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Escape" {
            ev.prevent_default();
            close();
        }
    };
    let notice = move || {
        if state.failed.get() && state.list.with(Option::is_none) {
            Some("Couldn't load the runbooks; close and try again.")
        } else {
            match state.list.with(|l| l.as_ref().map(Vec::is_empty)) {
                None => Some("Loading runbooks…"),
                Some(true) => Some("No runbooks: add [[runbooks]] to the bridge config."),
                Some(false) => None,
            }
        }
    };
    // The runbook offered for a confirm (spoken, else tapped), once the
    // list knows it.
    let offered = move || {
        let (name, said) = state
            .feed
            .with(|feed| feed.offer.clone())
            .map(|offer| (offer.name, offer.said))
            .or_else(|| state.tapped.get().map(|name| (name, None)))?;
        let runbook = state
            .list
            .with(|l| l.as_ref()?.iter().find(|r| r.name == name).cloned())?;
        Some((runbook, said))
    };

    view! {
        <div class="sheet-backdrop" on:click=move |_| close()></div>
        <section
            id="runbooks-sheet"
            class="sheet runbooks-sheet"
            role="dialog"
            aria-modal="true"
            aria-labelledby="runbooks-title"
            tabindex="-1"
            on:keydown=on_key
        >
            <header class="sheet-header">
                <h2 id="runbooks-title" class="sheet-title">"Runbooks"</h2>
                <button type="button" class="icon-button" aria-label="Close" on:click=move |_| close()>
                    <Icon paths=icons::CLOSE />
                </button>
            </header>
            <p class="field-help sheet-notice" role="status">{notice}</p>
            {move || match offered() {
                Some((runbook, said)) => view! {
                    <Confirm runbook=runbook said=said busy=busy on_run=Callback::new(run) on_cancel=Callback::new(move |()| state.decline()) />
                }.into_any(),
                None => view! {
                    <ul class="runbook-list" role="list">
                        {move || state.list.get().unwrap_or_default().into_iter().map(|runbook| {
                            let name = runbook.name.clone();
                            let shown = name.clone();
                            let command = protocol::command_line(&runbook.argv);
                            let status = move || state.latest_run(&shown).map(|run| sheet::status(&run));
                            view! {
                                <li>
                                    <button
                                        type="button"
                                        class="runbook-row"
                                        disabled=move || busy.get()
                                        on:click=move |_| pick(&runbook)
                                    >
                                        <span class="runbook-name">{name.to_string()}</span>
                                        <code class="runbook-command">{command}</code>
                                        <span class="runbook-status">{status}</span>
                                    </button>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                }.into_any(),
            }}
            <p class="field-error" role="alert">{move || error.get()}</p>
            {move || state.followed.get().map(|name| view! { <RunOutput state=state name=name /> })}
        </section>
    }
}

/// The confirm: the runbook's name, folder and exact command, then Run.
#[component]
fn Confirm(
    runbook: Runbook,
    said: Option<String>,
    busy: RwSignal<bool>,
    on_run: Callback<RunbookName>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let name = runbook.name.clone();
    let heard = said.map(|said| view! { <p class="field-help">"You said “" {said} "”."</p> });
    view! {
        <div class="sheet-group runbook-confirm" role="group" aria-labelledby="runbook-confirm-title">
            {heard}
            <h3 id="runbook-confirm-title" class="runbook-confirm-title">"Run " {runbook.name.to_string()} "?"</h3>
            <p class="field-label">"Runs exactly"</p>
            <pre class="runbook-argv"><code>{protocol::command_line(&runbook.argv)}</code></pre>
            <p class="field-help">"in " <code>{runbook.cwd}</code>", in its own herdr tab."</p>
            <div class="runbook-actions">
                <button type="button" class="button" disabled=move || busy.get() on:click=move |_| on_cancel.run(())>
                    "Cancel"
                </button>
                <button
                    type="button"
                    class="button button-primary runbook-run"
                    disabled=move || busy.get()
                    on:click=move |_| on_run.run(name.clone())
                >
                    {move || if busy.get() { "Starting…" } else { "Run" }}
                </button>
            </div>
        </div>
    }
}

/// The followed run: its state and last lines (untrusted output, shown as
/// plain text).
#[component]
fn RunOutput(state: State, name: RunbookName) -> impl IntoView {
    let run = Memo::new(move |_| state.latest_run(&name));
    move || {
        run.get().map(|run| {
            let finished = run.is_finished();
            let ok = run.succeeded();
            let lines = run.last_lines.join("\n");
            view! {
                <div class="sheet-group runbook-output" role="status" aria-live="polite">
                    <p
                        class="runbook-run-status"
                        class:runbook-ok=ok
                        class:runbook-failed=move || finished && !ok
                    >
                        {run.runbook.to_string()} " · " {sheet::status(&run)}
                    </p>
                    {run.message.map(|message| view! { <p class="field-help">{message}</p> })}
                    <pre class="runbook-lines">{lines}</pre>
                </div>
            }
        })
    }
}
