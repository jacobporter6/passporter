//! The on-demand WASM modules (dictation, pairing, extras, terminal) the page loads only
//! when it needs them, keeping their code out of the app's WASM.
//!
//! Each is an ES module built by trunk from `src/bin/<name>.rs`:
//! `<name>.js` and `<name>_bg.wasm`. `public/module-boot.js` (a classic
//! script, since the CSP forbids inline ones) imports it and leaves the
//! promise in `globalThis.passporterModules[<name>]`. A [`LazyModule`]
//! loads it once per page, shares a load in progress with every caller,
//! and retries a failed one on the next call.

use core::cell::RefCell;
use std::thread::LocalKey;

use js_sys::{Array, Function, Object, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, future_to_promise};

/// The loader script every module shares.
const BOOT_JS: &str = "/module-boot.js";
/// The global the loader leaves each module's promise in, by name.
const GLOBAL: &str = "passporterModules";

/// One on-demand module. Keep it in a `thread_local!` and load it with
/// [`LazyModule::load`].
#[derive(Debug)]
pub struct LazyModule {
    name: &'static str,
    loads: RefCell<Loads<Object, Promise>>,
}

impl LazyModule {
    /// The module built from `src/bin/<name>.rs`.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            loads: RefCell::new(Loads::new()),
        }
    }

    /// The module, if it has loaded.
    #[must_use]
    pub fn get(&self) -> Option<Object> {
        self.loads.borrow().module.clone()
    }

    /// The module, loaded if it isn't. Every caller during a load awaits
    /// the same one. `init` runs once, by the caller that sees the load
    /// finish, before any caller gets the module; it reports its own
    /// failures (the module is kept either way).
    ///
    /// # Errors
    ///
    /// The module didn't load (offline, an evicted cache). The next call
    /// tries again, with a fresh URL.
    pub async fn load(
        this: &'static LocalKey<Self>,
        init: impl FnOnce(&Object),
    ) -> Result<Object, JsValue> {
        let (attempt, loading) = match this.with(|lazy| lazy.loads.borrow_mut().start()) {
            Start::Ready(module) => return Ok(module),
            Start::Join(attempt, loading) => (attempt, loading),
            Start::Begin(attempt) => this.with(|lazy| {
                let loading = future_to_promise(import(lazy.name, attempt));
                lazy.loads.borrow_mut().begun(attempt, loading.clone());
                (attempt, loading)
            }),
        };
        match JsFuture::from(loading).await {
            Ok(module) => {
                // An ES module namespace has no prototype, so it fails the
                // `instanceof Object` that `dyn_into` checks.
                let module: Object = module.unchecked_into();
                let (module, fresh) = this.with(|lazy| lazy.loads.borrow_mut().loaded(module));
                if fresh {
                    init(&module);
                }
                Ok(module)
            }
            Err(error) => {
                this.with(|lazy| lazy.loads.borrow_mut().failed(attempt));
                Err(error)
            }
        }
    }
}

/// What a caller of [`Loads::start`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Start<M, P> {
    /// Use the loaded module.
    Ready(M),
    /// Await attempt `.0`, already in flight.
    Join(u32, P),
    /// Start attempt `.0` and report it with [`Loads::begun`].
    Begin(u32),
}

/// A module's loads: the browser-free state behind [`LazyModule`], over
/// the module `M` and the shareable in-flight load `P`.
#[derive(Debug)]
struct Loads<M, P> {
    module: Option<M>,
    /// The attempt in flight.
    loading: Option<(u32, P)>,
    /// Attempts started, numbering the next one's URL: the browser keeps a
    /// failed import failed for its URL until the page reloads.
    attempts: u32,
}

impl<M: Clone, P: Clone> Loads<M, P> {
    const fn new() -> Self {
        Self {
            module: None,
            loading: None,
            attempts: 0,
        }
    }

    fn start(&mut self) -> Start<M, P> {
        if let Some(module) = &self.module {
            return Start::Ready(module.clone());
        }
        if let Some((attempt, loading)) = &self.loading {
            return Start::Join(*attempt, loading.clone());
        }
        self.attempts += 1;
        Start::Begin(self.attempts)
    }

    fn begun(&mut self, attempt: u32, loading: P) {
        self.loading = Some((attempt, loading));
    }

    /// The module arrived: the one to use, and whether it is new
    /// (initialise it). A module that arrived first stays.
    fn loaded(&mut self, module: M) -> (M, bool) {
        self.loading = None;
        if let Some(first) = &self.module {
            return (first.clone(), false);
        }
        self.module = Some(module.clone());
        (module, true)
    }

    /// `attempt` failed: the next call starts a new one, unless a newer
    /// attempt is already in flight.
    fn failed(&mut self, attempt: u32) {
        if self
            .loading
            .as_ref()
            .is_some_and(|(current, _)| *current == attempt)
        {
            self.loading = None;
        }
    }
}

/// Which showing of a view an on-demand module draws is current. A
/// showing starts when the page mounts the view and ends when it is
/// cleaned up; a load that finishes after its showing ended mounts nothing,
/// and an old showing's cleanup never removes a newer showing's view.
#[derive(Debug, Default)]
pub struct Showings {
    current: u32,
}

impl Showings {
    /// A new showing, which supersedes any earlier one.
    pub const fn start(&mut self) -> u32 {
        self.current = self.current.wrapping_add(1);
        self.current
    }

    /// Whether `showing` is still the one on screen.
    #[must_use]
    pub const fn is_current(&self, showing: u32) -> bool {
        self.current == showing
    }

    /// Ends `showing`. True when its view is the one mounted and must go;
    /// a newer showing's mount has already replaced it otherwise.
    pub const fn end(&mut self, showing: u32) -> bool {
        if !self.is_current(showing) {
            return false;
        }
        self.current = self.current.wrapping_add(1);
        true
    }
}

/// Calls `module`'s export `name` with `args`.
///
/// # Errors
///
/// There is no such export, or it threw.
pub fn call(module: &Object, name: &str, args: &Array) -> Result<JsValue, JsValue> {
    Reflect::get(module, &JsValue::from_str(name))
        .and_then(JsCast::dyn_into::<Function>)
        .and_then(|export| export.apply(&JsValue::NULL, args))
}

/// Runs the loader for attempt `attempt` at module `name`, then resolves
/// to the module it imported.
async fn import(name: &'static str, attempt: u32) -> Result<JsValue, JsValue> {
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let body = document
        .body()
        .ok_or_else(|| JsValue::from_str("no body"))?;
    // A script element left by a failed attempt goes before a retry.
    if let Some(failed) = document.query_selector(&format!("script[data-module=\"{name}\"]"))? {
        failed.remove();
    }
    let script: web_sys::HtmlElement = document.create_element("script")?.unchecked_into();
    script.set_attribute("data-module", name)?;
    script.set_attribute("data-attempt", &attempt.to_string())?;
    script.set_attribute("src", BOOT_JS)?;
    let ran = Promise::new(&mut |resolve: Function, reject: Function| {
        script.set_onload(Some(&resolve));
        script.set_onerror(Some(&reject));
    });
    body.append_child(&script)?;
    JsFuture::from(ran).await?;
    let modules = Reflect::get(&js_sys::global(), &JsValue::from_str(GLOBAL))?;
    let module: Promise = Reflect::get(&modules, &JsValue::from_str(name))?.dyn_into()?;
    JsFuture::from(module).await
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestLoads = Loads<&'static str, &'static str>;

    #[test]
    fn callers_during_a_load_share_it() {
        let mut loads = TestLoads::new();
        assert_eq!(loads.start(), Start::Begin(1));
        loads.begun(1, "p1");
        assert_eq!(loads.start(), Start::Join(1, "p1"));
        assert_eq!(loads.loaded("m"), ("m", true));
        assert_eq!(loads.start(), Start::Ready("m"));
    }

    #[test]
    fn only_the_first_arrival_is_initialised() {
        let mut loads = TestLoads::new();
        assert_eq!(loads.start(), Start::Begin(1));
        loads.begun(1, "p1");
        assert_eq!(loads.loaded("m"), ("m", true));
        // A second waiter on the same load gets the same module, uninitialised.
        assert_eq!(loads.loaded("m"), ("m", false));
    }

    #[test]
    fn a_failed_load_is_retried_as_a_new_attempt() {
        let mut loads = TestLoads::new();
        assert_eq!(loads.start(), Start::Begin(1));
        loads.begun(1, "p1");
        loads.failed(1);
        assert_eq!(loads.start(), Start::Begin(2));
    }

    #[test]
    fn a_load_that_finishes_after_leaving_mounts_nothing() {
        let mut showings = Showings::default();
        let first = showings.start();
        assert!(showings.end(first), "its view goes");
        assert!(!showings.is_current(first));
    }

    #[test]
    fn a_newer_showing_owns_the_view() {
        let mut showings = Showings::default();
        let old = showings.start();
        let new = showings.start();
        assert!(!showings.is_current(old));
        // The old showing's cleanup must not unmount the new view.
        assert!(!showings.end(old));
        assert!(showings.is_current(new));
        assert!(showings.end(new));
    }

    #[test]
    fn ending_twice_unmounts_once() {
        let mut showings = Showings::default();
        let showing = showings.start();
        assert!(showings.end(showing));
        assert!(!showings.end(showing));
    }

    #[test]
    fn a_late_failure_leaves_a_newer_attempt_alone() {
        let mut loads = TestLoads::new();
        assert_eq!(loads.start(), Start::Begin(1));
        loads.begun(1, "p1");
        loads.failed(1);
        assert_eq!(loads.start(), Start::Begin(2));
        loads.begun(2, "p2");
        // Another waiter on attempt 1 reports the failure late.
        loads.failed(1);
        assert_eq!(loads.start(), Start::Join(2, "p2"));
    }
}
