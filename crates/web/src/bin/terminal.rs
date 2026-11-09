//! The terminal WASM: the command confirm and the watched terminal of
//! terminal mode, with its own Leptos runtime.
//!
//! trunk builds this binary as an ES module (`data-type="worker"` with
//! `data-bindgen-target="web"` in `index.html`): `terminal.js` and
//! `terminal_bg.wasm`, which `public/module-boot.js` loads on the first tap
//! on a command chip, keeping this code out of the app's WASM and the
//! extras'. The page treats it as the module of
//! [`Surface::Command`](web::extras::Surface::Command)
//! (`web::extras::page`), so it exports the extras module's functions,
//! for that one surface. The logic lives in [`web::commands::view`].

use js_sys::Function;
use wasm_bindgen::prelude::wasm_bindgen;
use web::commands::view;
use web::extras::{Surface, mount};
use web::platform;
use web_sys::HtmlElement;

/// Takes the page's host function ([`mount::init`]). Called once, right
/// after loading. Panics in this module are reported to the console from
/// here on, like the app's.
#[wasm_bindgen]
pub fn extras_init(host: Function) {
    platform::install_panic_hook();
    mount::init(host);
}

/// [`view::mount`]: draw the confirm in `root`, fed `feed`.
#[wasm_bindgen]
pub fn extras_mount(surface: &str, root: HtmlElement, feed: &str) {
    if Surface::named(surface) == Some(Surface::Command) {
        view::mount(root, feed);
    }
}

/// [`view::update`]: a new feed. False if nothing is mounted.
#[wasm_bindgen]
#[must_use]
pub fn extras_update(surface: &str, feed: &str) -> bool {
    Surface::named(surface) == Some(Surface::Command) && view::update(feed)
}

/// [`view::unmount`]: remove the confirm.
#[wasm_bindgen]
pub fn extras_unmount(surface: &str) {
    if Surface::named(surface) == Some(Surface::Command) {
        view::unmount();
    }
}

fn main() {}
