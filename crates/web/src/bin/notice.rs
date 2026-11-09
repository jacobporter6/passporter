//! The notice WASM: the service worker's push wake, and the Push API glue
//! behind the Settings switch.
//!
//! trunk builds this binary as a worker (`data-type="worker"` in
//! `index.html`): `notice.js` (no-modules glue, defining the global
//! `wasm_bindgen`) and `notice_bg.wasm`. `public/sw.js` loads it with
//! `importScripts` for [`wake`]; the page loads it on demand for the rest
//! (`web::push::module`), which keeps that code out of the app's WASM.
//! The logic lives in [`web::push`]; these are only the exports.

use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::wasm_bindgen;
use web::push::browser::PushError;
use web::push::{self, browser, module};

/// Fetches the agents and resolves to the notification to show, as JSON
/// (see [`web::push::Notice`]). Never rejects: failures give the generic
/// notification.
#[wasm_bindgen]
pub async fn wake() -> String {
    let notice = push::wake().await;
    serde_json::to_string(&notice).unwrap_or_default()
}

/// [`browser::probe`], as [`web::push::state::Probe::as_str`]; rejects
/// with a [`rejection`] when the bridge refused the token.
#[wasm_bindgen]
pub async fn push_probe(token: Option<String>) -> Result<String, JsValue> {
    browser::probe(&push::client(token))
        .await
        .map(|probe| probe.as_str().to_owned())
        .map_err(|e| rejection(&e))
}

/// [`browser::enable`]; rejects with a [`rejection`].
#[wasm_bindgen]
pub async fn push_enable(token: Option<String>) -> Result<(), JsValue> {
    browser::enable(&push::client(token))
        .await
        .map_err(|e| rejection(&e))
}

/// [`browser::disable`]: resolves to whether the bridge refused the token
/// (the browser let go either way); rejects with a [`rejection`].
#[wasm_bindgen]
pub async fn push_disable(token: Option<String>) -> Result<bool, JsValue> {
    browser::disable(&push::client(token))
        .await
        .map_err(|e| rejection(&e))
}

/// The `Error` an export rejects with: the message for the user, named
/// [`module::UNAUTHORIZED`] when the bridge refused the token, so the page
/// can start signing in again.
fn rejection(error: &PushError) -> JsValue {
    let rejection = js_sys::Error::new(&error.to_string());
    if error.is_unauthorized() {
        rejection.set_name(module::UNAUTHORIZED);
    }
    rejection.into()
}

fn main() {}
