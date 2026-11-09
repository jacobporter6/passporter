//! Dictation (design §6.3, §7): the mic button (the dock's on an agent's
//! screen, the new-agent sheet's own there) and the strip above the
//! composer that shows the live meter, then the upload. Stopping sends what was said to the agent, as
//! a prompt through the outbox; with "Review dictation before sending"
//! on, a transcript card comes first, and its "Use" fills the composer.
//!
//! The recording, storing and uploading happen in the dictation WASM
//! ([`super::engine`], loaded by [`super::module`]); a [`Dictation`] here
//! forwards the user's taps to it and shows what it reports. Each agent's
//! is kept for the whole app session, in reference-counted signals rather
//! than the screen's reactive owner, so a clip uploading when the user
//! swipes away is still shown when they come back.

use core::cell::RefCell;

use js_sys::Array;
use leptos::html::Div;
use leptos::prelude::*;
use protocol::AgentId;
use wasm_bindgen::{JsCast, JsValue};

use super::browser;
use super::module;
use super::phase::Mic;
use crate::ui::icons::{self, Icon};
use crate::ui::{AppState, use_app};

thread_local! {
    /// Every agent's dictation, by agent id as the dictation WASM names it.
    static DICTATIONS: RefCell<Vec<Dictation>> = const { RefCell::new(Vec::new()) };
}

/// One agent's dictation, as the app sees it. A cheap, `Send` handle
/// (views need one): the signals are reference-counted.
#[derive(Debug, Clone)]
pub struct Dictation {
    agent: String,
    app: AppState,
    mic: ArcRwSignal<Mic>,
    /// Where transcripts go: the reply bar of the screen that shows this
    /// agent now ([`Self::on_take`]).
    on_take: ArcRwSignal<Option<Callback<(String, bool), bool>>>,
}

impl Dictation {
    /// `agent`'s dictation, made on first use.
    #[must_use]
    pub fn for_agent(app: AppState, agent: &AgentId) -> Self {
        let key = agent.to_string();
        // A handful of agents at most: a list is smaller than a map.
        DICTATIONS.with_borrow_mut(|all| {
            if let Some(known) = all.iter().find(|d| d.agent == key) {
                return known.clone();
            }
            let new = Self {
                agent: key,
                app,
                mic: ArcRwSignal::new(Mic::Ready),
                on_take: ArcRwSignal::new(None),
            };
            all.push(new.clone());
            new
        })
    }

    /// Runs `f` on the dictation of the agent the dictation WASM calls
    /// `key`, if the app has one.
    pub(crate) fn by_key<T>(key: &str, f: impl FnOnce(&Self) -> T) -> Option<T> {
        DICTATIONS
            .with_borrow(|all| all.iter().find(|d| d.agent == key).cloned())
            .map(|dictation| f(&dictation))
    }

    /// The mic button's state (from the dictation WASM).
    pub(crate) fn set_mic(&self, mic: Mic) {
        self.mic.set(mic);
    }

    /// `text` was said (from the dictation WASM): to send if `send`, else
    /// reviewed and used. False if no screen took it (the user left).
    pub(crate) fn take_text(&self, text: String, send: bool) -> bool {
        self.on_take
            .get_untracked()
            .and_then(|on_take| on_take.try_run((text, send)))
            .unwrap_or(false)
    }

    /// Where transcripts go from now on: `on_take` gets the text and
    /// whether to send it, and says whether it took it.
    pub fn on_take(&self, on_take: Callback<(String, bool), bool>) {
        self.on_take.set(Some(on_take));
    }

    /// Calls the dictation WASM's `name` for this agent, plus `extra`;
    /// loading the module first if `load`.
    fn send(&self, name: &'static str, extra: Option<JsValue>, load: bool) {
        let args = Array::of1(&JsValue::from_str(&self.agent));
        if let Some(extra) = extra {
            args.push(&extra);
        }
        module::send(self.app, name, args, load);
    }

    /// The mic button went down at `at_ms`.
    pub fn press(&self, at_ms: f64) {
        if !module::ready() && browser::available() {
            // The press waits for the module: say so, or it looks ignored.
            self.app.toast("Starting dictation…");
        }
        self.send("voice_press", Some(at_ms.into()), true);
    }

    /// The mic button came up at `at_ms`: a hold ends the recording. If
    /// the press is still waiting for the module, this queues behind it.
    pub fn release(&self, at_ms: f64) {
        self.send("voice_release", Some(at_ms.into()), true);
    }

    /// The screen is going away: a recording stops (and is kept).
    pub fn leave(&self) {
        self.send("voice_leave", None, false);
    }

    /// Picks up where a previous screen or page load left off. Loads the
    /// dictation WASM, so it is ready by the first tap.
    pub fn resume(&self) {
        self.send("voice_resume", None, true);
    }
}

/// The device clock, in ms, for gestures.
fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// The mic button on the new-agent sheet (or, `fab`, the dock's big one):
/// tap to toggle, hold to talk. Not shown at all where the browser can't record
/// (outside a secure context).
#[component]
pub fn MicButton(dictation: Dictation, #[prop(optional)] fab: bool) -> impl IntoView {
    if !browser::available() {
        return ().into_any();
    }
    let app = use_app();
    let live = {
        let mic = dictation.mic.clone();
        move || mic.get() == Mic::Live
    };
    let enabled = {
        let mic = dictation.mic.clone();
        move || mic.get() != Mic::Busy && app.paired.get()
    };
    let on_down = {
        let d = dictation.clone();
        move |ev: leptos::ev::PointerEvent| {
            if ev.button() != 0 {
                return;
            }
            // Keeps focus (and the keyboard) where it was, and keeps the
            // pointer's `up` here even if the finger drifts off.
            ev.prevent_default();
            if let Some(target) = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            {
                let _ = target.set_pointer_capture(ev.pointer_id());
            }
            d.press(now_ms());
        }
    };
    let on_up = {
        let d = dictation.clone();
        move |_: leptos::ev::PointerEvent| d.release(now_ms())
    };
    let on_key = move |ev: leptos::ev::KeyboardEvent| {
        if matches!(ev.key().as_str(), "Enter" | " ") && !ev.repeat() {
            ev.prevent_default();
            let at = now_ms();
            dictation.press(at);
            dictation.release(at);
        }
    };
    let label = {
        let live = live.clone();
        move || match (live(), app.review_dictation.get()) {
            (false, _) => "Dictate",
            (true, false) => "Send dictation",
            (true, true) => "Stop dictation",
        }
    };
    view! {
        <button
            type="button"
            class=if fab { "mic-fab" } else { "mic-button" }
            class:mic-live=live.clone()
            aria-label=label
            aria-pressed=move || live().to_string()
            disabled=move || !enabled()
            on:pointerdown=on_down
            on:pointerup=on_up.clone()
            on:pointercancel=on_up
            on:contextmenu=|ev| ev.prevent_default()
            on:keydown=on_key
        >
            <Icon paths=icons::MIC />
        </button>
    }
    .into_any()
}

/// The strip above the composer: an element the dictation WASM draws
/// into (the meter, progress, the transcript card; see
/// [`super::panel`]). It lays out as if its contents were the reply bar's
/// own (`display: contents`).
#[component]
pub fn DictationStrip(dictation: Dictation) -> impl IntoView {
    let root = NodeRef::<Div>::new();
    let mounted = StoredValue::new_local(None::<web_sys::Element>);
    {
        let dictation = dictation.clone();
        root.on_load(move |element| {
            let element: web_sys::Element = element.unchecked_into();
            mounted.set_value(Some(element.clone()));
            dictation.send("voice_mount", Some(element.into()), true);
        });
    }
    on_cleanup(move || {
        if let Some(element) = mounted.try_get_value().flatten() {
            dictation.send("voice_unmount", Some(element.into()), false);
        }
    });
    view! { <div class="dictation-strip" node_ref=root></div> }
}
