//! The page's side of the extras module: loading it, mounting a
//! [`Surface`] into an [`Extra`] element, feeding it, and the host it
//! reaches the page through.
//!
//! What each surface's feed is:
//!
//! - [`Surface::NewAgent`]: nothing at mount; later, dictated text for the
//!   prompt.
//! - [`Surface::Runbooks`]: [`crate::runbooks::sheet::Feed`] as JSON.
//! - [`Surface::Settings`]: [`crate::settings::model::Snapshot`] as JSON.
//! - [`Surface::Link`]: the URL to confirm.
//! - [`Surface::Command`]: [`crate::commands::Feed`] as JSON. Drawn by the
//!   terminal module ([`Surface::module`]), loaded the same way.

use core::cell::RefCell;
use std::thread::LocalKey;

use js_sys::{Array, Object, Promise};
use leptos::html::Div;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, future_to_promise};
use web_sys::HtmlElement;

use super::{Ask, Surface};
use crate::api::{BearerToken, TokenSource};
use crate::lazy::{self, LazyModule, Showings};
use crate::live::{Live, use_live};
use crate::nav::{Nav, use_nav};
use crate::platform;
use crate::runbooks::{Runbooks, use_runbooks};
use crate::session::SessionToken;
use crate::ui::{AppState, use_app};
use crate::{push, settings, spawn};

thread_local! {
    static EXTRAS: LazyModule = const { LazyModule::new("extras") };
    static TERMINAL: LazyModule = const { LazyModule::new("terminal") };
    static SHOWINGS: RefCell<[Showings; 5]> = RefCell::new(Default::default());
    /// What closes each surface on screen, from its [`Extra`].
    static CLOSERS: RefCell<[Option<Callback<()>>; 5]> = const { RefCell::new([None; 5]) };
}

/// The module that draws `surface` ([`Surface::module`]).
fn module_of(surface: Surface) -> &'static LocalKey<LazyModule> {
    match surface {
        Surface::Command => &TERMINAL,
        Surface::NewAgent | Surface::Runbooks | Surface::Settings | Surface::Link => &EXTRAS,
    }
}

/// Where a surface's load is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Load {
    Loading,
    Ready,
    Failed,
}

/// `surface`, drawn by the extras module: a spinner while the module
/// loads, then the view, fed `feed` (now and whenever it changes); a
/// message with "Try again" if the module won't load. `on_close` runs
/// when the user closes the surface.
#[component]
pub fn Extra(
    surface: Surface,
    #[prop(optional, into)] feed: MaybeProp<String>,
    #[prop(optional)] on_close: Option<Callback<()>>,
) -> impl IntoView {
    let host = Ctx {
        app: use_app(),
        live: use_live(),
        nav: use_nav(),
        runbooks: use_runbooks(),
    };
    let i = surface.index();
    let root = NodeRef::<Div>::new();
    let load = RwSignal::new(Load::Loading);
    let showing = SHOWINGS.with_borrow_mut(|s| s[i].start());
    let current = move || SHOWINGS.with_borrow(|s| s[i].is_current(showing));
    CLOSERS.with_borrow_mut(|closers| closers[i] = on_close);

    let start = move |root: HtmlElement| {
        load.set(Load::Loading);
        spawn_local(async move {
            let loaded = LazyModule::load(module_of(surface), |module| init(module, &host)).await;
            let mounted = loaded.and_then(|module| {
                if !current() {
                    return Ok(());
                }
                let feed = JsValue::from_str(&feed.get_untracked().unwrap_or_default());
                let args = Array::of3(&surface.name().into(), &root, &feed);
                lazy::call(&module, "extras_mount", &args).map(drop)
            });
            match mounted {
                Ok(()) => {
                    let _ = load.try_set(Load::Ready);
                }
                Err(error) => {
                    platform::log_error("could not load extras", &error);
                    if current() {
                        let _ = load.try_set(Load::Failed);
                    }
                }
            }
        });
    };
    root.on_load(move |root| start(root.into()));
    Effect::new(move |_| {
        let feed = feed.get().unwrap_or_default();
        if load.get_untracked() == Load::Ready && current() {
            send(surface, &feed);
        }
    });
    on_cleanup(move || {
        if !SHOWINGS.with_borrow_mut(|s| s[i].end(showing)) {
            return;
        }
        CLOSERS.with_borrow_mut(|closers| closers[i] = None);
        if let Some(module) = module_of(surface).with(LazyModule::get) {
            let _ = lazy::call(
                &module,
                "extras_unmount",
                &Array::of1(&surface.name().into()),
            );
        }
    });

    let retry = move |_| {
        if let Some(root) = root.get_untracked() {
            start(root.into());
        }
    };
    let close = move |_| {
        if let Some(on_close) = on_close {
            on_close.run(());
        }
    };
    view! {
        <div class="extra" node_ref=root></div>
        {move || match load.get() {
            Load::Ready => None,
            Load::Loading => Some(view! {
                <p class="extra-status" class:extra-status-float=surface.floats() role="status">
                    <span class="spinner" aria-hidden="true"></span>
                    "Loading…"
                </p>
            }.into_any()),
            Load::Failed => Some(view! {
                <div class="extra-status extra-failed" class:extra-status-float=surface.floats() role="alert">
                    <p>{surface.title()} " didn't load. Check the connection and try again."</p>
                    <div class="confirm-actions">
                        {on_close.map(|_| view! {
                            <button type="button" class="button button-secondary button-small" on:click=close>"Close"</button>
                        })}
                        <button type="button" class="button button-primary button-small" on:click=retry>"Try again"</button>
                    </div>
                </div>
            }.into_any()),
        }}
    }
}

/// Hands `surface` a new feed, if its view is mounted. False if it isn't,
/// or didn't use it.
pub fn send(surface: Surface, feed: &str) -> bool {
    let Some(module) = module_of(surface).with(LazyModule::get) else {
        return false;
    };
    let args = Array::of2(&surface.name().into(), &feed.into());
    lazy::call(&module, "extras_update", &args).is_ok_and(|taken| taken.is_truthy())
}

/// Closes `surface`, as if the user had.
pub fn close(surface: Surface) {
    let on_close = CLOSERS.with_borrow(|closers| closers[surface.index()]);
    if let Some(on_close) = on_close {
        on_close.try_run(());
    }
}

/// The extras module's exports beyond its surfaces (a photo's upload),
/// called from the page. Made in a component: it holds the app's handles
/// for the module's host, in case this call is what loads it.
#[derive(Debug, Clone, Copy)]
pub struct Exports(Ctx);

impl Exports {
    /// The exports, for the app around the calling component.
    #[must_use]
    pub fn current() -> Self {
        Self(Ctx {
            app: use_app(),
            live: use_live(),
            nav: use_nav(),
            runbooks: use_runbooks(),
        })
    }

    /// Calls export `name` without arguments if the module is loaded
    /// (else there is nothing for it to act on), ignoring what it returns.
    pub fn call_loaded(self, name: &str) {
        if let Some(module) = EXTRAS.with(LazyModule::get) {
            let _ = lazy::call(&module, name, &Array::new());
        }
    }

    /// Calls export `name` with `args` (loading the module first if it
    /// isn't) and waits for the promise it returns.
    ///
    /// # Errors
    ///
    /// The module didn't load, or the call threw or rejected.
    pub async fn call(self, name: &str, args: &Array) -> Result<JsValue, JsValue> {
        let module = LazyModule::load(&EXTRAS, |module| init(module, &self.0)).await?;
        JsFuture::from(Promise::resolve(&lazy::call(&module, name, args)?)).await
    }
}

/// What the host answers with: app-lifetime handles.
#[derive(Debug, Clone, Copy)]
struct Ctx {
    app: AppState,
    live: Live,
    nav: Nav,
    runbooks: Runbooks,
}

/// Hands the module its host function, once, when it first loads.
fn init(module: &Object, ctx: &Ctx) {
    let ctx = *ctx;
    let host = Closure::<dyn Fn(String, JsValue, JsValue) -> JsValue>::new(
        move |ask: String, a: JsValue, b: JsValue| {
            Ask::named(&ask).map_or(JsValue::UNDEFINED, |ask| answer(&ctx, ask, a, b))
        },
    )
    .into_js_value();
    if let Err(error) = lazy::call(module, "extras_init", &Array::of1(&host)) {
        platform::log_error("could not start extras", &error);
    }
}

/// The page's answer to `ask` with arguments `a` and `b` ([`Ask`] says
/// what each takes and returns).
fn answer(ctx: &Ctx, ask: Ask, a: JsValue, b: JsValue) -> JsValue {
    let Ctx {
        app,
        live,
        nav,
        runbooks,
    } = *ctx;
    let text = a.as_string().unwrap_or_default();
    match ask {
        Ask::Token => SessionToken(app)
            .current()
            .map_or(JsValue::UNDEFINED, |t| t.expose().into()),
        Ask::Unauthorized => {
            if let Ok(token) = BearerToken::new(text) {
                SessionToken(app).unauthorized(&token);
            }
            JsValue::UNDEFINED
        }
        Ask::Toast => {
            app.toast(text);
            JsValue::UNDEFINED
        }
        Ask::Close => {
            if let Some(surface) = Surface::named(&text) {
                close(surface);
            }
            JsValue::UNDEFINED
        }
        Ask::MountDictation => {
            if let (Ok(strip), Ok(mic)) = (a.dyn_into(), b.dyn_into()) {
                spawn::mount_dictation(app, strip, mic);
            }
            JsValue::UNDEFINED
        }
        Ask::RunbooksListed => {
            runbooks.listed(&text);
            JsValue::UNDEFINED
        }
        Ask::RunbookStarted => {
            runbooks.started(live, &text);
            JsValue::UNDEFINED
        }
        Ask::Declined => {
            runbooks.decline();
            JsValue::UNDEFINED
        }
        Ask::SetPref => {
            settings::prefs::set(app, &text, b.is_truthy());
            JsValue::UNDEFINED
        }
        Ask::OpenAgent
        | Ask::Forget
        | Ask::ClearSnapshots
        | Ask::PushProbe
        | Ask::PushEnable
        | Ask::PushDisable => future_to_promise(answer_later(app, live, nav, ask, text)).into(),
    }
}

/// The page's answer to an `ask` that waits.
async fn answer_later(
    app: AppState,
    live: Live,
    nav: Nav,
    ask: Ask,
    text: String,
) -> Result<JsValue, JsValue> {
    let outcome = |done: Result<(), String>| {
        done.map_or_else(
            |message| JsValue::from_str(&message),
            |()| JsValue::UNDEFINED,
        )
    };
    Ok(match ask {
        Ask::OpenAgent => {
            if let Ok(opened) = serde_json::from_str(&text) {
                spawn::open_agent(app, live, nav, opened).await;
            }
            JsValue::UNDEFINED
        }
        Ask::Forget => outcome(settings::forget(app, live).await),
        Ask::ClearSnapshots => {
            settings::clear_snapshots(app).await;
            JsValue::UNDEFINED
        }
        Ask::PushProbe => {
            let state = push::module::probe(SessionToken(app)).await;
            serde_json::to_string(&state).map_or(JsValue::UNDEFINED, JsValue::from)
        }
        Ask::PushEnable => outcome(push::module::enable(SessionToken(app)).await),
        Ask::PushDisable => outcome(push::module::disable(SessionToken(app)).await),
        _ => JsValue::UNDEFINED,
    })
}
