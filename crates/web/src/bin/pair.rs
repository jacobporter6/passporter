//! The pairing WASM: the Connect screen's body (QR / code pairing, or the
//! pasted token), with its own Leptos runtime.
//!
//! trunk builds this binary as an ES module (`data-type="worker"` with
//! `data-bindgen-target="web"` in `index.html`): `pair.js` and
//! `pair_bg.wasm`, which `public/module-boot.js` loads when the screen opens
//! (`web::pair::module`), keeping this code out of the app's WASM. The
//! logic lives in [`web::pair::screen`]; these are only the exports.

use wasm_bindgen::prelude::wasm_bindgen;
use web::pair::screen::{self, Host};
use web::platform;
use web_sys::HtmlElement;

/// [`screen::mount`]: draw the screen in `root`. Panics in this module are
/// reported to the console from here on, like the app's.
#[wasm_bindgen]
pub fn pair_mount(host: Host, root: HtmlElement, paired: bool) {
    platform::install_panic_hook();
    screen::mount(host, root, paired);
}

/// [`screen::set_paired`].
#[wasm_bindgen]
pub fn pair_paired(paired: bool) {
    screen::set_paired(paired);
}

/// [`screen::unmount`].
#[wasm_bindgen]
pub fn pair_unmount() {
    screen::unmount();
}

fn main() {}
