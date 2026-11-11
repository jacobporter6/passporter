//! The pair WASM (`src/bin/pair.rs`), loaded when the Connect screen opens.
//!
//! The screen's body ([`super::screen`]: the camera, key generation, the
//! code and token forms) is compiled into its own module, with its own
//! Leptos runtime, rather than the app, whose WASM has no room to spare
//! for a screen used once per device ([`crate::lazy`]). `pair.js` and
//! `pair_bg.wasm` are in the shell cache, like the rest of the app.
//!
//! The two modules have separate memories and talk through plain values:
//! the page mounts the module's view into an element of its own and hands
//! it a host (made here) for toasts, for adopting the device key the
//! module made (or pairing through the Android app's native layer) or the
//! token it checked (the page stores them: the session
//! lives here, [`session`]), and for leaving the screen.

use core::cell::RefCell;

use js_sys::{Array, Object, Reflect};
use leptos::html::Div;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::future_to_promise;

use protocol::DeviceId;
use web_sys::CryptoKey;

use super::device::DeviceKey;
use crate::api::BearerToken;
use crate::lazy::{self, LazyModule, Showings};
use crate::nav::{History, Nav};
use crate::routes::AppRoute;
use crate::ui::AppState;
use crate::{platform, session};

thread_local! {
    static PAIR: LazyModule = const { LazyModule::new("pair") };
    /// The host handed to every mount, made on the first.
    static HOST: RefCell<Option<JsValue>> = const { RefCell::new(None) };
    static SHOWINGS: RefCell<Showings> = RefCell::new(Showings::default());
}

/// Shows the screen's body in `root` once the module has loaded, and
/// removes it when the calling component is cleaned up. `failed` turns on
/// if the module won't load.
pub fn show(app: AppState, nav: Nav, root: NodeRef<Div>, failed: RwSignal<bool>) {
    let showing = SHOWINGS.with_borrow_mut(Showings::start);
    let current = move || SHOWINGS.with_borrow(|s| s.is_current(showing));
    root.on_load(move |root| {
        spawn_local(async move {
            match LazyModule::load(&PAIR, |_| {}).await {
                Ok(module) if current() => {
                    let paired = JsValue::from_bool(app.paired.get_untracked());
                    let mounted = host(app, nav).and_then(|host| {
                        lazy::call(&module, "pair_mount", &Array::of3(&host, &root, &paired))
                    });
                    if let Err(error) = mounted {
                        platform::log_error("could not show pairing", &error);
                        let _ = failed.try_set(true);
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    platform::log_error("could not load pairing", &error);
                    if current() {
                        let _ = failed.try_set(true);
                    }
                }
            }
        });
    });
    // Keep the module's "This phone is connected" card current.
    Effect::new(move |_| {
        let paired = JsValue::from_bool(app.paired.get());
        if let Some(module) = PAIR.with(LazyModule::get).filter(|_| current()) {
            let _ = lazy::call(&module, "pair_paired", &Array::of1(&paired));
        }
    });
    on_cleanup(move || {
        if !SHOWINGS.with_borrow_mut(|s| s.end(showing)) {
            return;
        }
        if let Some(module) = PAIR.with(LazyModule::get) {
            let _ = lazy::call(&module, "pair_unmount", &Array::new());
        }
    });
}

/// The page's side of [`super::screen::Host`], made once: the app state
/// and navigation it captures live as long as the page.
fn host(app: AppState, nav: Nav) -> Result<JsValue, JsValue> {
    if let Some(host) = HOST.with_borrow(Clone::clone) {
        return Ok(host);
    }
    let host = Object::new();
    let set = |name: &str, f: JsValue| Reflect::set(&host, &JsValue::from_str(name), &f);
    set(
        "toast",
        Closure::<dyn Fn(String)>::new(move |text: String| app.toast(text)).into_js_value(),
    )?;
    set(
        "adoptDevice",
        Closure::<dyn Fn(String, CryptoKey) -> js_sys::Promise>::new(
            move |device_id: String, private: CryptoKey| {
                future_to_promise(async move {
                    let adopted = match DeviceId::new(device_id) {
                        Ok(id) => {
                            session::adopt_device(app, DeviceKey::from_parts(id, private)).await
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    Ok(outcome(adopted))
                })
            },
        )
        .into_js_value(),
    )?;
    set(
        "native",
        Closure::<dyn Fn() -> js_sys::Promise>::new(|| {
            future_to_promise(async { Ok(JsValue::from_bool(session::native_identity().await)) })
        })
        .into_js_value(),
    )?;
    set(
        "pairNative",
        Closure::<dyn Fn(String) -> js_sys::Promise>::new(move |code: String| {
            future_to_promise(async move {
                let paired = match protocol::PairingCode::new(code) {
                    Ok(code) => session::pair_native(app, code).await,
                    Err(e) => Err(e.to_string()),
                };
                Ok(outcome(paired))
            })
        })
        .into_js_value(),
    )?;
    set(
        "saveToken",
        Closure::<dyn Fn(String) -> js_sys::Promise>::new(move |token: String| {
            future_to_promise(async move {
                let saved = match BearerToken::new(token) {
                    Ok(token) => session::save(app, token).await,
                    Err(e) => Err(e.to_string()),
                };
                if saved.is_ok() {
                    spawn_local(session::request_persistent_storage(app));
                }
                Ok(outcome(saved))
            })
        })
        .into_js_value(),
    )?;
    set(
        "done",
        Closure::<dyn Fn()>::new(move || nav.go(&AppRoute::Triage, History::Push)).into_js_value(),
    )?;
    let host = JsValue::from(host);
    HOST.set(Some(host.clone()));
    Ok(host)
}

/// A host call's outcome as the module reads it: `undefined`, or the
/// failure's message.
fn outcome(result: Result<(), String>) -> JsValue {
    result.map_or_else(
        |message| JsValue::from_str(&message),
        |()| JsValue::UNDEFINED,
    )
}
