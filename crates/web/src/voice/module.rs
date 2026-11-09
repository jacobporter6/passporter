//! The dictation WASM (`src/bin/voice.rs`), loaded into the page on demand.
//!
//! The recorder, the clip store and the resumable upload
//! ([`super::engine`]) are compiled into their own module rather than the
//! app, whose WASM has no room left in its size budget: the app keeps only
//! the mic button and the strip ([`super::view`]). The page loads it when
//! an agent's composer first mounts, so it is ready by the first tap
//! ([`crate::lazy`]): `voice.js` and `voice_bg.wasm` are in the shell
//! cache.
//!
//! The two modules have separate memories and talk through plain values:
//! the page calls the module's exports with the agent's id, and the
//! module reports back through the [`super::engine::Host`] made here.
//! Nothing is loaded where the browser can't record
//! ([`super::browser::available`]).

use js_sys::{Array, Object, Reflect};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

use super::browser;
use super::phase::Mic;
use super::view::Dictation;
use crate::api::{BearerToken, TokenSource};
use crate::lazy::{self, LazyModule};
use crate::platform;
use crate::session::SessionToken;
use crate::ui::AppState;

thread_local! {
    /// The module, handed the host once loaded.
    static VOICE: LazyModule = const { LazyModule::new("voice") };
}

/// Calls export `name` with `args`. Synchronous once the module is loaded,
/// so a call from a tap runs inside the tap (the microphone and its audio
/// need the user's gesture). Before that, `load` says whether to load it
/// and call when ready (starting a recording) or do nothing (stopping one
/// that can't exist).
pub fn send(app: AppState, name: &'static str, args: Array, load: bool) {
    let module = VOICE.with(LazyModule::get);
    match route(module.is_some(), load, browser::available()) {
        Route::Call => {
            if let Some(module) = module {
                call(app, &module, name, &args);
            }
        }
        Route::LoadThenCall => spawn_local(async move {
            match loaded(app).await {
                Ok(module) => call(app, &module, name, &args),
                Err(error) => {
                    platform::log_error("could not load dictation", &error);
                    app.toast(LOAD_FAILED);
                }
            }
        }),
        Route::Drop => {}
    }
}

/// Whether the module is loaded (a tap now runs inside the gesture).
#[must_use]
pub fn ready() -> bool {
    VOICE.with(|voice| voice.get().is_some())
}

/// What a call to the module does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Now, synchronously.
    Call,
    /// Once the module has loaded (or, failing that, says so).
    LoadThenCall,
    /// Nothing: there is no module and none is wanted.
    Drop,
}

const fn route(loaded: bool, load: bool, available: bool) -> Route {
    if loaded {
        Route::Call
    } else if load && available {
        Route::LoadThenCall
    } else {
        Route::Drop
    }
}

/// The note for a module that would not load.
const LOAD_FAILED: &str = "Dictation didn't load. Check the connection and try again.";
/// The note for a module call that threw.
const CALL_FAILED: &str = "Dictation hit a problem. Reload the app and try again.";

fn call(app: AppState, module: &Object, name: &str, args: &Array) {
    if let Err(error) = lazy::call(module, name, args) {
        platform::log_error("dictation call failed", &error);
        app.toast(CALL_FAILED);
    }
}

/// The module, loaded and handed the host if it wasn't. A host that can't
/// be handed over is reported like a failed call; the module stays loaded.
async fn loaded(app: AppState) -> Result<Object, JsValue> {
    LazyModule::load(&VOICE, |module| match host(app) {
        Ok(host) => call(app, module, "voice_init", &Array::of1(&host)),
        Err(error) => {
            platform::log_error("dictation host failed", &error);
            app.toast(CALL_FAILED);
        }
    })
    .await
}

/// The page's side of [`super::engine::Host`]: the mic button's state and
/// transcripts go to the agent's [`Dictation`]; the token comes from the
/// session, and whether to review dictation from the device's settings.
fn host(app: AppState) -> Result<JsValue, JsValue> {
    let host = Object::new();
    let set = |name: &str, f: JsValue| Reflect::set(&host, &JsValue::from_str(name), &f);
    set(
        "mic",
        Closure::<dyn Fn(String, u32)>::new(|agent: String, code| {
            Dictation::by_key(&agent, |d| d.set_mic(Mic::from_code(code)));
        })
        .into_js_value(),
    )?;
    set(
        "takeText",
        Closure::<dyn Fn(String, String, bool) -> bool>::new(|agent: String, text, send| {
            Dictation::by_key(&agent, |d| d.take_text(text, send)).unwrap_or(false)
        })
        .into_js_value(),
    )?;
    set(
        "reviewDictation",
        Closure::<dyn Fn() -> bool>::new(move || app.review_dictation.get_untracked())
            .into_js_value(),
    )?;
    set(
        "toast",
        Closure::<dyn Fn(String)>::new(move |text: String| app.toast(text)).into_js_value(),
    )?;
    set(
        "saved",
        Closure::<dyn Fn()>::new(move || {
            // Unsent words are worth keeping: ask the browser not to evict
            // them (best effort; once granted it sticks).
            if app.persistent_storage.get_untracked() == Some(true) {
                return;
            }
            spawn_local(async move {
                let granted = platform::request_persistent_storage().await;
                let _ = app.persistent_storage.try_set(Some(granted));
            });
        })
        .into_js_value(),
    )?;
    set(
        "token",
        Closure::<dyn Fn() -> Option<String>>::new(move || {
            SessionToken(app).current().map(|t| t.expose().to_owned())
        })
        .into_js_value(),
    )?;
    set(
        "unauthorized",
        Closure::<dyn Fn(String)>::new(move |token: String| {
            if let Ok(token) = BearerToken::new(token) {
                SessionToken(app).unauthorized(&token);
            }
        })
        .into_js_value(),
    )?;
    set(
        "tokenRejected",
        Closure::<dyn Fn() -> bool>::new(move || app.token_rejected.get_untracked())
            .into_js_value(),
    )?;
    Ok(host.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loaded_module_is_called_at_once() {
        assert_eq!(route(true, true, true), Route::Call);
        assert_eq!(route(true, false, true), Route::Call);
    }

    #[test]
    fn a_tap_before_the_module_is_ready_waits_for_it() {
        assert_eq!(route(false, true, true), Route::LoadThenCall);
    }

    #[test]
    fn nothing_loads_for_a_call_that_needs_no_module() {
        // Stopping a recording that can't exist.
        assert_eq!(route(false, false, true), Route::Drop);
    }

    #[test]
    fn nothing_loads_where_the_browser_cant_record() {
        assert_eq!(route(false, true, false), Route::Drop);
    }
}
