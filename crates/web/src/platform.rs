//! Thin wrappers over browser APIs the shell needs: the global scope (a
//! window, or the service worker for push wakes), service worker
//! registration and updates, persistent storage, connectivity, haptics,
//! panic reporting.
//!
//! All best effort: a missing API degrades the feature, never the app. Many
//! of these APIs are absent in some contexts (`navigator.vibrate` on iOS,
//! `navigator.serviceWorker` and `navigator.storage` outside a secure
//! context), and their `web-sys` bindings throw instead of returning `Err`
//! when the member is missing, so every call is feature-tested first.

use core::cell::Cell;

use js_sys::{Function, Reflect};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    IdbFactory, Navigator, Request, ServiceWorkerContainer, ServiceWorkerRegistration,
    ServiceWorkerState,
};

use crate::pair::scanner::sleep;

/// Path of the service worker script. It must sit at the root so its scope
/// covers the whole app.
pub const SERVICE_WORKER_URL: &str = "/sw.js";

/// Message that tells a waiting service worker to take over (see `sw.js`).
const SKIP_WAITING: &str = "skip-waiting";

thread_local! {
    /// Set when the user asked to apply an update, so the following
    /// `controllerchange` reloads the page (and only then).
    static RELOAD_ON_TAKEOVER: Cell<bool> = const { Cell::new(false) };
}

#[wasm_bindgen]
extern "C" {
    /// The global scope the WASM runs in, through what windows and workers
    /// share (`WindowOrWorkerGlobalScope`): the app's window, or the
    /// service worker, which runs the push wake (see `src/bin/notice.rs`).
    #[derive(Debug, Clone)]
    pub type Global;

    /// `fetch(request)`.
    #[wasm_bindgen(method, js_name = fetch)]
    pub fn fetch(this: &Global, request: &Request) -> js_sys::Promise;

    /// `crypto`.
    #[wasm_bindgen(method, getter, catch)]
    pub fn crypto(this: &Global) -> Result<web_sys::Crypto, JsValue>;

    /// `indexedDB`, if this context has it.
    #[wasm_bindgen(method, getter, catch, js_name = indexedDB)]
    pub fn indexed_db(this: &Global) -> Result<Option<IdbFactory>, JsValue>;
}

impl Global {
    /// The current global scope.
    #[must_use]
    pub fn current() -> Self {
        js_sys::global().unchecked_into()
    }
}

fn navigator() -> Option<Navigator> {
    web_sys::window().map(|w| w.navigator())
}

/// Stops every track of `stream`: the camera or microphone light goes off.
pub fn stop_tracks(stream: &web_sys::MediaStream) {
    for track in stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
            track.stop();
        }
    }
}

/// Whether `target` has a member called `name`. Used before calling
/// `web-sys` bindings that throw when the member is missing.
pub(crate) fn has(target: &JsValue, name: &str) -> bool {
    Reflect::has(target, &JsValue::from_str(name)).unwrap_or(false)
}

/// The service worker container, if this context has one (secure context,
/// not disabled, e.g. by a private window).
pub(crate) fn service_workers() -> Option<ServiceWorkerContainer> {
    let window = web_sys::window()?;
    let navigator = window.navigator();
    (window.is_secure_context() && has(&navigator, "serviceWorker"))
        .then(|| navigator.service_worker())
}

/// Registers the app-shell service worker (see `public/sw.js`) and calls
/// `on_update` when a new version has installed and is waiting to take
/// over this page. Call [`reload_into_latest`] to switch to it.
pub async fn register_service_worker(on_update: impl Fn() + 'static) {
    let Some(container) = service_workers() else {
        return;
    };
    let registration: ServiceWorkerRegistration =
        match JsFuture::from(container.register(SERVICE_WORKER_URL)).await {
            Ok(registration) => registration.unchecked_into(),
            Err(error) => {
                log_error("service worker registration failed", &error);
                return;
            }
        };

    // No controller means this is the first install: nothing to update.
    let on_update = std::rc::Rc::new(on_update);
    if registration.waiting().is_some() && container.controller().is_some() {
        on_update();
    }

    let watched = registration.clone();
    let watched_container = container.clone();
    let on_found = Closure::<dyn FnMut()>::new(move || {
        let Some(worker) = watched.installing() else {
            return;
        };
        let installing = worker.clone();
        let container = watched_container.clone();
        let on_update = on_update.clone();
        let on_state = Closure::<dyn FnMut()>::new(move || {
            if installing.state() == ServiceWorkerState::Installed
                && container.controller().is_some()
            {
                on_update();
            }
        });
        worker.set_onstatechange(Some(on_state.as_ref().unchecked_ref()));
        // Lives as long as that worker; at most one per update.
        on_state.forget();
    });
    registration.set_onupdatefound(Some(on_found.as_ref().unchecked_ref()));
    // App-lifetime handlers: registered once per page load.
    on_found.forget();

    let on_takeover = Closure::<dyn FnMut()>::new(|| {
        if RELOAD_ON_TAKEOVER.get()
            && let Some(window) = web_sys::window()
        {
            let _ = window.location().reload();
        }
    });
    container.set_oncontrollerchange(Some(on_takeover.as_ref().unchecked_ref()));
    on_takeover.forget();
}

/// How long [`reload_into_latest`] waits for a new version to finish
/// installing (precaching the shell) before reloading anyway.
const INSTALL_WAIT_MS: i32 = 10_000;
/// How often it checks on the install meanwhile.
const INSTALL_POLL_MS: i32 = 200;
/// How long it waits for the new worker to take over, which reloads the
/// page, before reloading itself.
const TAKEOVER_WAIT_MS: i32 = 3_000;

/// This page's service worker registration, if any.
async fn registration() -> Option<ServiceWorkerRegistration> {
    let container = service_workers()?;
    JsFuture::from(container.get_registration())
        .await
        .ok()?
        .dyn_into()
        .ok()
}

/// Asks the browser to look for a new service worker now, rather than on
/// the next navigation (which an installed PWA resumed from the background
/// never makes). A new one installs, and the update prompt follows.
/// Best effort: offline, it just fails.
pub async fn check_for_update() {
    if let Some(promise) = registration().await.and_then(|r| r.update().ok()) {
        let _ = JsFuture::from(promise).await;
    }
}

/// Reloads into the newest build the bridge serves, past the service
/// worker: looks for a new worker if none is waiting yet, lets it finish
/// installing, tells it to take over (`controllerchange` then reloads the
/// page), and reloads anyway if any step fails or stalls. Without a
/// service worker, a plain reload fetches the new build.
pub async fn reload_into_latest() {
    if let Some(registration) = registration().await {
        if registration.waiting().is_none() {
            if let Ok(promise) = registration.update() {
                let _ = JsFuture::from(promise).await;
            }
            let mut waited = 0;
            while registration.waiting().is_none()
                && registration.installing().is_some()
                && waited < INSTALL_WAIT_MS
            {
                sleep(INSTALL_POLL_MS).await;
                waited += INSTALL_POLL_MS;
            }
        }
        if let Some(waiting) = registration.waiting() {
            RELOAD_ON_TAKEOVER.set(true);
            match waiting.post_message(&JsValue::from_str(SKIP_WAITING)) {
                Ok(()) => sleep(TAKEOVER_WAIT_MS).await,
                Err(error) => log_error("could not activate the update", &error),
            }
        }
    }
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

/// The `sessionStorage` value under `key`: kept across reloads of this tab,
/// gone when the app is closed. `None` where storage is unavailable.
#[must_use]
pub fn session_value(key: &str) -> Option<String> {
    let storage = web_sys::window()?.session_storage().ok()??;
    storage.get_item(key).ok()?
}

/// Sets the `sessionStorage` value under `key`; best effort.
pub fn set_session_value(key: &str, value: &str) {
    if let Some(Ok(Some(storage))) = web_sys::window().map(|w| w.session_storage()) {
        let _ = storage.set_item(key, value);
    }
}

/// Asks the browser not to evict our storage under pressure, because the
/// outbox holds unsent user decisions (design §6.5). Call it when the
/// device has something worth keeping (after pairing, on the first outbox
/// write), not on launch: browsers may prompt or decide based on engagement.
/// Resolves to whether storage is now persistent.
pub async fn request_persistent_storage() -> bool {
    storage_query(web_sys::StorageManager::persist).await
}

/// Whether storage is already persistent, without asking.
pub async fn storage_is_persistent() -> bool {
    storage_query(web_sys::StorageManager::persisted).await
}

async fn storage_query(
    call: impl FnOnce(&web_sys::StorageManager) -> Result<js_sys::Promise, JsValue>,
) -> bool {
    let Some(navigator) = navigator().filter(|n| has(n, "storage")) else {
        return false;
    };
    let Ok(promise) = call(&navigator.storage()) else {
        return false;
    };
    JsFuture::from(promise)
        .await
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Whether the browser believes it is online. `true` when unknown, so the
/// app never shows a false "offline" banner.
#[must_use]
pub fn is_online() -> bool {
    navigator().is_none_or(|n| n.on_line())
}

/// A short haptic tick (design §7: send, record, stale warnings). A no-op
/// where vibration is missing (iOS) or refused; never throws, so it can't
/// affect the caller's control flow.
pub fn haptic_tick() {
    let Some(navigator) = navigator() else {
        return;
    };
    let vibrate = Reflect::get(&navigator, &JsValue::from_str("vibrate"))
        .ok()
        .and_then(|f| f.dyn_into::<Function>().ok());
    if let Some(vibrate) = vibrate {
        let _ = vibrate.call1(&navigator, &JsValue::from(12));
    }
}

/// Routes Rust panics to the console, with the panic message, instead of
/// an opaque `unreachable` trap.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&JsValue::from_str(&info.to_string()));
    }));
}

/// Logs `error` with `context` to the console.
pub fn log_error(context: &str, error: &JsValue) {
    web_sys::console::error_2(&JsValue::from_str(context), error);
}

/// A short, human-readable message for a thrown JS value: a `DOMException`
/// or `Error` message, a thrown string, or a generic fallback.
#[must_use]
pub fn js_error_message(error: &JsValue) -> String {
    error
        .dyn_ref::<web_sys::DomException>()
        .map(web_sys::DomException::message)
        .or_else(|| {
            error
                .dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
        })
        .or_else(|| error.as_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "unknown browser error".to_owned())
}
