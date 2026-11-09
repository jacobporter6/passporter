//! The dictation WASM: the recorder, the clip store and the resumable
//! upload behind the composer's mic button.
//!
//! trunk builds this binary as an ES module (`data-type="worker"` with
//! `data-bindgen-target="web"` in `index.html`): `voice.js` and
//! `voice_bg.wasm`, which `public/module-boot.js` loads on demand for the
//! page (`web::voice::module`), keeping this code out of the app's WASM.
//! The logic lives in [`web::voice::engine`]; these are only the exports,
//! each for one agent, named by its id.

use protocol::AgentId;
use wasm_bindgen::prelude::wasm_bindgen;
use web::voice::engine::{self, Dictation, Host};
use web_sys::Element;

/// Takes the page's host. Called once, right after loading.
#[wasm_bindgen]
pub fn voice_init(host: Host) {
    engine::init(host);
}

/// [`Dictation::press`].
#[wasm_bindgen]
pub fn voice_press(agent: &str, at_ms: f64) {
    with(agent, |d| d.press(at_ms));
}

/// [`Dictation::release`].
#[wasm_bindgen]
pub fn voice_release(agent: &str, at_ms: f64) {
    with(agent, |d| d.release(at_ms));
}

/// [`Dictation::leave`].
#[wasm_bindgen]
pub fn voice_leave(agent: &str) {
    with(agent, |d| d.leave());
}

/// [`Dictation::resume`].
#[wasm_bindgen]
pub fn voice_resume(agent: &str) {
    with(agent, Dictation::resume);
}

/// [`Dictation::mount`]: draw the strip in `root`.
#[wasm_bindgen]
pub fn voice_mount(agent: &str, root: Element) {
    with(agent, |d| d.mount(root));
}

/// [`Dictation::unmount`]: stop drawing in `root`.
#[wasm_bindgen]
pub fn voice_unmount(agent: &str, root: &Element) {
    with(agent, |d| d.unmount(root));
}

/// Runs `f` on `agent`'s dictation; an id that doesn't parse is ignored.
fn with(agent: &str, f: impl FnOnce(&std::rc::Rc<Dictation>)) {
    if let Ok(agent) = agent.parse::<AgentId>() {
        f(&engine::dictation(&agent));
    }
}

fn main() {}
