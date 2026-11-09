//! The "New agent" sheet, drawn by the extras module
//! ([`crate::extras::mount`]): the pickers, the prompt and Start. The
//! page opens the new agent and owns the prompt's dictation, mounted into
//! two slots here (`super::mount_dictation`); its feed is dictated text
//! for the prompt.

use core::cell::RefCell;

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{ProjectName, SpawnKind, SpawnOptions};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::HtmlElement;

use super::sheet::{self, Attempt, Form, Next};
use crate::extras::mount::{self, Mounted};
use crate::extras::{Ask, Surface};
use crate::idgen::random_key;
use crate::ui::format::now;
use crate::ui::icons::{self, Icon};
use crate::voice::phase::append_transcript;

thread_local! {
    /// The last request sent, until the bridge decides it. Kept for the
    /// whole app session, not the sheet's: closing and reopening the sheet
    /// after "no answer" must still retry with the same key.
    static ATTEMPT: RefCell<Attempt> = RefCell::new(Attempt::default());
}

/// Draws the sheet in `root`. Its feed is dictated text, appended to the
/// prompt.
pub fn mount(root: HtmlElement) -> Mounted {
    let form = ArcRwSignal::new(Form::default());
    let dictated = form.clone();
    Mounted::new(
        root,
        move || view! { <Sheet form=RwSignal::from(form) /> }.into_any(),
        move |said| {
            dictated
                .try_update(|f| f.prompt = append_transcript(&f.prompt, said))
                .is_some()
        },
    )
}

/// What the sheet's Start is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the user.
    Idle,
    /// The request is out.
    Starting,
    /// The agent is running; the sheet opens it.
    Opening,
}

#[component]
fn Sheet(form: RwSignal<Form>) -> impl IntoView {
    // The bridge's allowlist once it loads, and what to say until (or
    // instead of) a form: loading, failed, or spawning is off.
    let options = RwSignal::new(None::<SpawnOptions>);
    let notice = RwSignal::new(Some("Loading projects…".to_owned()));
    let error = RwSignal::new(None::<String>);
    // What Start is doing; anything but `Idle` keeps the form still.
    let phase = RwSignal::new(Phase::Idle);

    spawn_local(async move {
        // Mounted by now: focus the dialog, so keys and screen readers
        // start in it.
        focus_by_id("new-agent-sheet");
        match mount::api().spawn_options().await {
            Ok(loaded) if loaded.projects.is_empty() || loaded.kinds.is_empty() => {
                let _ = notice.try_set(Some(
                    "Starting agents is off: the bridge config has no [spawn] projects.".to_owned(),
                ));
            }
            Ok(loaded) => {
                // An undecided request comes back as it was sent.
                let shown = ATTEMPT
                    .with_borrow(Attempt::pending_form)
                    .unwrap_or_else(|| Form::for_options(&loaded));
                let _ = form.try_set(shown);
                let _ = notice.try_set(None);
                let _ = options.try_set(Some(loaded));
            }
            Err(_) => {
                let _ = notice.try_set(Some(
                    "Couldn't load the projects; close and try again.".to_owned(),
                ));
            }
        }
    });

    let close = move || {
        if phase.get_untracked() == Phase::Idle {
            mount::close(Surface::NewAgent);
        }
    };

    let submit = move || {
        if phase.get_untracked() != Phase::Idle {
            return;
        }
        let built =
            ATTEMPT.with_borrow_mut(|a| a.request(&form.get_untracked(), random_key, now()));
        let request = match built {
            Ok(request) => request,
            Err(why) => {
                error.set(Some(why.to_string()));
                return;
            }
        };
        phase.set(Phase::Starting);
        error.set(None);
        spawn_local(async move {
            let next = sheet::next(mount::api().spawn(&request).await);
            if next.is_final() {
                ATTEMPT.with_borrow_mut(Attempt::settle);
            }
            match next {
                Next::Open { agent, note } => {
                    // A new agent is running: the page opens it, then
                    // closes the sheet.
                    let _ = phase.try_set(Phase::Opening);
                    let note = note.map_or(JsValue::UNDEFINED, Into::into);
                    let _ =
                        mount::ask_later(Ask::OpenAgent, &agent.to_string().into(), &note).await;
                }
                Next::Retry(message) | Next::Show(message) => {
                    let _ = phase.try_set(Phase::Idle);
                    let _ = error.try_set(Some(message));
                }
            }
        });
    };

    let on_key = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Escape" {
            ev.prevent_default();
            close();
        }
    };
    let label = move || {
        if phase.get() == Phase::Idle {
            "Start agent"
        } else {
            "Starting…"
        }
    };

    view! {
        <div class="sheet-backdrop" on:click=move |_| close()></div>
        <section
            id="new-agent-sheet"
            class="sheet new-agent-sheet"
            role="dialog"
            aria-modal="true"
            aria-labelledby="new-agent-title"
            tabindex="-1"
            on:keydown=on_key
        >
            <header class="sheet-header">
                <h2 id="new-agent-title" class="sheet-title">"New agent"</h2>
                <button type="button" class="icon-button" aria-label="Close" on:click=move |_| close()>
                    <Icon paths=icons::CLOSE />
                </button>
            </header>
            <p class="field-help sheet-notice" role="status">{move || notice.get()}</p>
            {move || options.get().map(|loaded| view! {
                <SpawnForm options=loaded form=form phase=phase />
            })}
            <p id="new-agent-error" class="field-error" role="alert">{move || error.get()}</p>
            <button
                type="button"
                class="button button-primary button-block new-agent-start"
                disabled=move || phase.get() != Phase::Idle || options.with(Option::is_none) || form.with(|f| f.project.is_none() || f.kind.is_none())
                on:click=move |_| submit()
            >
                {label}
            </button>
        </section>
    }
}

/// The pickers and the prompt.
#[component]
fn SpawnForm(options: SpawnOptions, form: RwSignal<Form>, phase: RwSignal<Phase>) -> impl IntoView {
    let busy = move || phase.get() != Phase::Idle;
    // The page's dictation goes in these two slots.
    let strip = NodeRef::<leptos::html::Div>::new();
    let mic = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        if let (Some(strip), Some(mic)) = (strip.get(), mic.get()) {
            mount::ask(Ask::MountDictation, &strip, &mic);
        }
    });
    let projects = options.projects.into_iter().map(Choice::Project).collect();
    // One kind needs no picker: it is picked already.
    let kinds = (options.kinds.len() > 1).then(|| {
        choices(
            "Agent",
            options.kinds.into_iter().map(Choice::Kind).collect(),
            form,
            phase,
        )
    });
    view! {
        {choices("Project", projects, form, phase)}
        {kinds}
        <div class="sheet-group">
            <label class="field-label" for="new-agent-prompt">"First prompt (optional)"</label>
            <div class="slot" node_ref=strip></div>
            <div class="composer new-agent-composer">
                <textarea
                    id="new-agent-prompt"
                    class="composer-input"
                    placeholder="What should it do?"
                    aria-describedby="new-agent-error"
                    disabled=busy
                    prop:value=move || form.with(|f| f.prompt.clone())
                    on:input=move |ev| form.update(|f| f.prompt = event_target_value(&ev))
                ></textarea>
                <div class="slot" node_ref=mic></div>
            </div>
        </div>
    }
}

/// One pick of the form: a project or a kind.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Choice {
    Project(ProjectName),
    Kind(SpawnKind),
}

impl Choice {
    fn label(&self) -> String {
        match self {
            Self::Project(name) => name.as_str().to_owned(),
            Self::Kind(kind) => kind.label().to_owned(),
        }
    }

    fn is_picked(&self, form: &Form) -> bool {
        match self {
            Self::Project(name) => form.project.as_ref() == Some(name),
            Self::Kind(kind) => form.kind == Some(*kind),
        }
    }

    fn pick(&self, form: &mut Form) {
        match self {
            Self::Project(name) => form.project = Some(name.clone()),
            Self::Kind(kind) => form.kind = Some(*kind),
        }
    }
}

/// A radio group of chips under `legend`. One function for every picker,
/// so they share one copy of the view code (the WASM size budget).
fn choices(
    legend: &'static str,
    choices: Vec<Choice>,
    form: RwSignal<Form>,
    phase: RwSignal<Phase>,
) -> impl IntoView {
    view! {
        <fieldset class="sheet-group" disabled=move || phase.get() != Phase::Idle>
            <legend class="field-label">{legend}</legend>
            <div class="option-chips" role="radiogroup" aria-label=legend>
                {choices.into_iter().map(|choice| {
                    let label = choice.label();
                    let shown = choice.clone();
                    view! {
                        <button
                            type="button"
                            role="radio"
                            class="option-chip"
                            aria-checked=move || form.with(|f| shown.is_picked(f)).to_string()
                            on:click=move |_| form.update(|f| choice.pick(f))
                        >
                            {label}
                        </button>
                    }
                }).collect_view()}
            </div>
        </fieldset>
    }
}

/// Focuses the element with `id`, if there is one.
fn focus_by_id(id: &str) {
    if let Some(element) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    {
        // Only ever our own `<section>`, an HTML element.
        let _ = element.unchecked_into::<HtmlElement>().focus();
    }
}
