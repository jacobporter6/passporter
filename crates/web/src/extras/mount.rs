//! The extras module's side: runs in `extras_bg.wasm` (`src/bin/extras.rs`).
//!
//! Each [`Surface`] renders with this module's own Leptos runtime into an
//! element the page hands over, and is fed what only the page knows as a
//! string ([`super::page`] says which, per surface). Everything else the
//! page does for it, it asks ([`Ask`]) through the host function the page
//! hands over once.

use core::cell::RefCell;
use std::rc::Rc;

use js_sys::{Function, Promise};
use leptos::mount::{UnmountHandle, mount_to};
use leptos::prelude::*;
use leptos::tachys::view::any_view::AnyViewState;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlElement;

use super::{Ask, Surface};
use crate::api::{ApiClient, BearerToken, FetchTransport, TokenSource};

/// One surface as mounted: its view, and what takes its feed.
pub struct Mounted {
    _view: UnmountHandle<AnyViewState>,
    feed: Rc<dyn Fn(&str) -> bool>,
}

impl core::fmt::Debug for Mounted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Mounted").finish_non_exhaustive()
    }
}

impl Mounted {
    /// Mounts `view` in `root`; `feed` takes the page's later feeds and
    /// says whether it used them.
    pub fn new(
        root: HtmlElement,
        view: impl FnOnce() -> AnyView + 'static,
        feed: impl Fn(&str) -> bool + 'static,
    ) -> Self {
        Self {
            _view: mount_to(root, view),
            feed: Rc::new(feed),
        }
    }
}

thread_local! {
    static HOST: RefCell<Option<Function>> = const { RefCell::new(None) };
    static MOUNTED: RefCell<[Option<Mounted>; 5]> =
        const { RefCell::new([None, None, None, None, None]) };
}

/// Takes the page's host function. Called once, right after loading.
pub fn init(host: Function) {
    HOST.set(Some(host));
}

/// Asks the page `ask` with arguments `a` and `b` ([`Ask`] says which).
/// `undefined` before [`init`], or if the page failed to answer.
pub fn ask(ask: Ask, a: &JsValue, b: &JsValue) -> JsValue {
    HOST.with_borrow(|host| {
        host.as_ref()
            .and_then(|host| host.call3(&JsValue::NULL, &ask.name().into(), a, b).ok())
    })
    .unwrap_or(JsValue::UNDEFINED)
}

/// Asks the page `ask`, then waits for the promise it answers with.
///
/// # Errors
///
/// The page rejected it.
pub async fn ask_later(ask_: Ask, a: &JsValue, b: &JsValue) -> Result<JsValue, JsValue> {
    JsFuture::from(Promise::resolve(&ask(ask_, a, b))).await
}

/// Asks the page `ask`, which answers with a promise of `undefined` or
/// the failure's message, and waits for it.
///
/// # Errors
///
/// The failure's message.
pub async fn ask_outcome(ask_: Ask) -> Result<(), String> {
    match ask_later(ask_, &JsValue::UNDEFINED, &JsValue::UNDEFINED).await {
        Ok(answer) => answer.as_string().map_or(Ok(()), Err),
        Err(error) => Err(crate::platform::js_error_message(&error)),
    }
}

/// A note for the user, through the page.
pub fn toast(text: &str) {
    ask(Ask::Toast, &text.into(), &JsValue::UNDEFINED);
}

/// Closes `surface`, through the page.
pub fn close(surface: Surface) {
    ask(Ask::Close, &surface.name().into(), &JsValue::UNDEFINED);
}

/// The bridge client, authenticated with the page's session.
#[must_use]
pub fn api() -> ApiClient<FetchTransport, HostToken> {
    ApiClient::new(FetchTransport, HostToken)
}

/// The page's session token, through the host.
#[derive(Debug, Clone, Copy)]
pub struct HostToken;

impl TokenSource for HostToken {
    fn current(&self) -> Option<BearerToken> {
        let token = ask(Ask::Token, &JsValue::UNDEFINED, &JsValue::UNDEFINED);
        token.as_string().and_then(|t| BearerToken::new(t).ok())
    }

    fn unauthorized(&self, rejected: &BearerToken) {
        ask(
            Ask::Unauthorized,
            &rejected.expose().into(),
            &JsValue::UNDEFINED,
        );
    }
}

/// Draws `surface` in `root` with its first `feed`, replacing any earlier
/// mount of it.
pub fn mount(surface: Surface, root: HtmlElement, feed: &str) {
    unmount(surface);
    let mounted = match surface {
        Surface::NewAgent => crate::spawn::view::mount(root),
        Surface::Runbooks => crate::runbooks::view::mount(root, feed),
        Surface::Settings => crate::settings::view::mount(root, feed),
        Surface::Link => crate::ui::markdown::link::mount(root, feed),
        // Drawn by the terminal module (`src/bin/terminal.rs`), never here.
        Surface::Command => return,
    };
    MOUNTED.with_borrow_mut(|all| all[surface.index()] = Some(mounted));
}

/// Hands `surface` a new feed. False if it isn't mounted or didn't use it.
pub fn update(surface: Surface, feed: &str) -> bool {
    // Run outside the borrow: the view may close itself in response.
    let take = MOUNTED.with_borrow(|all| all[surface.index()].as_ref().map(|m| m.feed.clone()));
    take.is_some_and(|take| take(feed))
}

/// Removes `surface`'s view, if mounted.
pub fn unmount(surface: Surface) {
    // Dropped outside the borrow: cleanups may run arbitrary code.
    let mounted = MOUNTED.with_borrow_mut(|all| all[surface.index()].take());
    drop(mounted);
}
