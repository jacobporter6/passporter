//! The notice WASM (`src/bin/notice.rs`), loaded into the page on demand.
//!
//! The Push API glue in [`super::browser`] is compiled into the notice WASM
//! rather than the app: the service worker loads that module anyway, and
//! the app's own WASM has no room left in its size budget. The page loads
//! it the first time Settings asks about push (or credentials are
//! forgotten): `notice.js` as a classic script, which defines the global
//! `wasm_bindgen`, then `notice_bg.wasm`. Both are in the shell cache.
//!
//! Calls pass this device's current token, since the module has its own
//! memory. Nothing is loaded where [`super::browser::availability`] says
//! push can't work, such as plain-HTTP pages.

use core::cell::RefCell;

use js_sys::{Array, Function, Object, Promise, Reflect};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Notification, NotificationPermission};

use super::browser;
use super::state::{Probe, PushState};
use crate::api::{BearerToken, TokenSource};
use crate::platform::{js_error_message, log_error as browser_log};
use crate::session::SessionToken;

/// The module's glue and code, as the build names them.
const NOTICE_JS: &str = "/notice.js";
const NOTICE_WASM: &str = "/notice_bg.wasm";
/// Selects the page's `notice.js` script element.
const NOTICE_SCRIPT: &str = "script[src=\"/notice.js\"]";

#[wasm_bindgen]
extern "C" {
    /// `notice.js`'s global: the initializer, carrying the exports. Read
    /// it only once the script has run (see [`call`]).
    #[wasm_bindgen(thread_local_v2, js_name = wasm_bindgen)]
    static NOTICE: JsValue;
}

thread_local! {
    /// `notice.js` running, then `notice_bg.wasm` instantiating: each
    /// started at most once per page (the script declares a global, so it
    /// can't run twice), and awaited by every caller.
    static SCRIPT: RefCell<Option<Promise>> = const { RefCell::new(None) };
    static INIT: RefCell<Option<Promise>> = const { RefCell::new(None) };
}

/// The `name` of the `Error` the notice module rejects with when the
/// bridge refused the token it was given.
pub const UNAUTHORIZED: &str = "Unauthorized";

/// The switch's state once checked: whether push is on, or why it can't
/// be. A device without a token is off (losing the credentials turns push
/// off) and loads nothing. A module that fails to load (offline, an
/// evicted cache) is [`PushState::Unknown`], which tries again when
/// turned on; a refused token goes to `source`, which signs in again, and
/// the new token brings a new check.
pub async fn probe(source: SessionToken) -> PushState {
    if let Some(why) = browser::availability() {
        return PushState::Unavailable(why);
    }
    let Some(token) = source.current() else {
        return PushState::Off;
    };
    match call("push_probe", &Array::of1(&raw(Some(token.clone())))).await {
        Ok(name) => PushState::from_probe(Probe::parse(&name.as_string().unwrap_or_default())),
        Err(error) => {
            if is_unauthorized(&error) {
                source.unauthorized(&token);
            } else {
                browser_log("could not check push", &error);
            }
            PushState::Unknown
        }
    }
}

/// [`browser::enable`], once permission is granted. `Err` is a message
/// for the user.
pub async fn enable(source: SessionToken) -> Result<(), String> {
    with_session("push_enable", source).await
}

/// [`browser::disable`]: unsubscribes, then deletes the bridge's copy.
/// `Err` is a message for the user.
pub async fn disable(source: SessionToken) -> Result<(), String> {
    with_session("push_disable", source).await
}

/// Push off because this device's credentials are going away: forgotten
/// (pass the token, which still works), revoked or refused (the bridge
/// has dropped the device or can't be told; only the browser's copy goes).
/// Silent, and a no-op where no subscription can exist (permission not
/// granted). A re-paired device starts with push off.
pub async fn turn_off(token: Option<BearerToken>) {
    // `availability` first: it checks that `Notification` exists at all
    // (it doesn't in iOS Safari outside an installed web app).
    if browser::availability().is_some()
        || Notification::permission() != NotificationPermission::Granted
    {
        return;
    }
    if let Err(error) = call("push_disable", &Array::of1(&raw(token))).await {
        browser_log("could not turn push off", &error);
    }
}

/// Calls export `name` with the app's current token. A refused token goes
/// to the app's token source, which signs in again (or, for a pasted
/// token, asks for a new one), as any other API call would: a rejection
/// named [`UNAUTHORIZED`], or `true` from `push_disable`, whose browser
/// step succeeded anyway.
async fn with_session(name: &str, source: SessionToken) -> Result<(), String> {
    let token = source.current();
    let result = call(name, &Array::of1(&raw(token.clone()))).await;
    let refused = match &result {
        Ok(value) => value.as_bool() == Some(true),
        Err(error) => is_unauthorized(error),
    };
    if refused && let Some(token) = &token {
        source.unauthorized(token);
    }
    match result {
        Ok(_) => Ok(()),
        Err(_) if refused => {
            Err("The bridge asked this phone to sign in again. Try again in a moment.".to_owned())
        }
        Err(error) => Err(js_error_message(&error)),
    }
}

fn is_unauthorized(error: &JsValue) -> bool {
    error
        .dyn_ref::<js_sys::Error>()
        .is_some_and(|e| e.name() == UNAUTHORIZED)
}

fn raw(token: Option<BearerToken>) -> JsValue {
    token.map_or(JsValue::UNDEFINED, |t| JsValue::from_str(t.expose()))
}

/// Loads the module if needed, then calls its export `name` with `args`
/// and waits for the result. Where push can't work, fails without loading
/// anything.
async fn call(name: &str, args: &Array) -> Result<JsValue, JsValue> {
    if let Some(why) = browser::availability() {
        return Err(JsValue::from_str(why.reason()));
    }
    if let Err(error) = JsFuture::from(started(&SCRIPT, run_script)?).await {
        // The script never ran (offline, evicted cache), so it declared
        // nothing: the next call may try again.
        SCRIPT.set(None);
        return Err(error);
    }
    if let Err(error) = JsFuture::from(started(&INIT, instantiate)?).await {
        // The glue forgets a failed init (it keeps only a successful
        // instance), so the next call may try again.
        INIT.set(None);
        return Err(error);
    }
    let export: Function = NOTICE
        .with(|notice| Reflect::get(notice, &JsValue::from_str(name)))?
        .dyn_into()?;
    JsFuture::from(Promise::resolve(&export.apply(&JsValue::NULL, args)?)).await
}

/// The promise in `slot`, started by `start` if it hasn't been.
fn started(
    slot: &'static std::thread::LocalKey<RefCell<Option<Promise>>>,
    start: fn() -> Result<Promise, JsValue>,
) -> Result<Promise, JsValue> {
    slot.with_borrow_mut(|slot| match slot {
        Some(promise) => Ok(promise.clone()),
        None => Ok(slot.insert(start()?).clone()),
    })
}

/// Appends `<script src="/notice.js">`; settles once it has run.
fn run_script() -> Result<Promise, JsValue> {
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let body = document
        .body()
        .ok_or_else(|| JsValue::from_str("no body"))?;
    // A script element left by a failed attempt goes before a retry.
    if let Some(failed) = document.query_selector(NOTICE_SCRIPT)? {
        failed.remove();
    }
    let script: web_sys::HtmlElement = document.create_element("script")?.unchecked_into();
    script.set_attribute("src", NOTICE_JS)?;
    let loaded = Promise::new(&mut |resolve: Function, reject: Function| {
        script.set_onload(Some(&resolve));
        script.set_onerror(Some(&reject));
    });
    body.append_child(&script)?;
    Ok(loaded)
}

/// Instantiates `notice_bg.wasm`, once `notice.js` has run.
fn instantiate() -> Result<Promise, JsValue> {
    let options = Object::new();
    Reflect::set(
        &options,
        &JsValue::from_str("module_or_path"),
        &JsValue::from_str(NOTICE_WASM),
    )?;
    let init: Function = NOTICE.with(Clone::clone).dyn_into()?;
    Ok(Promise::resolve(&init.call1(&JsValue::NULL, &options)?))
}
