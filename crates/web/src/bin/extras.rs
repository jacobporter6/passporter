//! The extras WASM: the app's rarely used surfaces (the "New agent" and
//! Runbooks sheets, the Settings groups, the link confirm), with its own
//! Leptos runtime.
//!
//! trunk builds this binary as an ES module (`data-type="worker"` with
//! `data-bindgen-target="web"` in `index.html`): `extras.js` and
//! `extras_bg.wasm`, which `public/module-boot.js` loads the first time
//! one of them opens (`web::extras::page`), keeping this code out of the
//! app's WASM. The logic lives in [`web::extras::mount`] and each
//! feature's `view`; these are only the exports, each surface named by
//! its [`Surface::name`], plus the composer's photo preview and upload
//! ([`web::attach::preview`]).

use js_sys::{Function, Promise};
use protocol::AttachmentId;
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::future_to_promise;
use web::extras::Surface;
use web::extras::mount;
use web::{attach, platform};
use web_sys::{File, HtmlElement};

/// [`mount::init`]: takes the page's host function. Called once, right after
/// loading. Panics in this module are reported to the console from here
/// on, like the app's.
#[wasm_bindgen]
pub fn extras_init(host: Function) {
    platform::install_panic_hook();
    mount::init(host);
}

/// [`mount::mount`]: draw `surface` in `root`, fed `feed`.
#[wasm_bindgen]
pub fn extras_mount(surface: &str, root: HtmlElement, feed: &str) {
    if let Some(surface) = Surface::named(surface) {
        mount::mount(surface, root, feed);
    }
}

/// [`mount::update`]: a new feed for `surface`. False if it isn't mounted
/// or didn't use it.
#[wasm_bindgen]
#[must_use]
pub fn extras_update(surface: &str, feed: &str) -> bool {
    Surface::named(surface).is_some_and(|surface| mount::update(surface, feed))
}

/// [`mount::unmount`]: remove `surface`.
#[wasm_bindgen]
pub fn extras_unmount(surface: &str) {
    if let Some(surface) = Surface::named(surface) {
        mount::unmount(surface);
    }
}

/// [`attach::preview::attach`]: draws photo `id`'s preview in `slot`, then
/// shrinks and uploads `file`; `removed` is called if the user removes it.
/// A promise of `undefined` once it is on the bridge, or of the failure's
/// text.
#[wasm_bindgen]
pub fn extras_attach(id: &str, file: File, slot: HtmlElement, removed: Function) -> Promise {
    let id = AttachmentId::new(id);
    future_to_promise(async move {
        let outcome = match id {
            Ok(id) => attach::preview::attach(id, file, slot, removed).await,
            Err(error) => Err(error.to_string()),
        };
        Ok(outcome.map_or_else(|why| JsValue::from_str(&why), |()| JsValue::UNDEFINED))
    })
}

/// [`attach::preview::clear`]: removes the photo's preview.
#[wasm_bindgen]
pub fn extras_attach_clear() {
    attach::preview::clear();
}

fn main() {}
