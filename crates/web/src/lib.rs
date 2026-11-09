//! Passporter's web app: a mobile-first, voice-first PWA in Leptos (CSR),
//! compiled to WASM with trunk (design §3, §7).
//!
//! The crate is a library plus a two-line binary, so everything except the
//! browser glue is unit-tested on the host with nextest. Browser APIs come
//! from `web-sys`; they compile on any target and are only *called* in the
//! browser.
//!
//! # Modules
//!
//! - [`app`]: root component, shell layout.
//! - [`routes`]: the typed route table every link is built from; [`nav`]:
//!   client-side navigation over it.
//! - [`api`]: typed bridge client over `crates/protocol`, with token handling
//!   behind [`api::TokenSource`].
//! - [`push`]: Web Push: the subscription toggle and the service worker's
//!   wake (notification text).
//! - [`store`]: typed key/value store over `IndexedDB` (snapshots, outbox).
//! - [`outbox`]: durable delivery of replies and choices, with retries and
//!   stale-result cards.
//! - [`session`]: the bridge token (MVP auth). [`live`]: the agent list and
//!   threads, kept current by the stream. [`idgen`]: idempotency keys.
//! - [`triage`], [`agent`], [`pair`], [`settings`]: the screens.
//! - [`ui`]: shared state and shell chrome. [`platform`]: browser glue.
//! - [`version`]: this build, the bridge's, and the update banner.
//! - [`voice`]: dictation: recorder, durable clips, resumable upload.
//! - [`lazy`]: the on-demand WASM modules (dictation, pairing, extras).
//! - [`extras`]: the rarely used surfaces, drawn by the extras WASM.
//! - [`runbooks`]: the Runbooks sheet and voice offers of named operations.
//! - [`spawn`]: the "New agent" sheet, starting agents from the phone.
//! - [`attach`]: photos sent with a prompt.
//! - [`commands`]: terminal mode: the commands in an agent's message, and
//!   the confirm that runs one.

// `#[component]` re-emits the function without outer attributes, so
// `#[must_use]` can't be applied to components; they are only ever used in
// `view!`, which consumes their output.
#![allow(clippy::must_use_candidate)]

pub mod agent;
pub mod api;
pub mod app;
pub mod attach;
pub mod commands;
pub mod extras;
pub mod idgen;
pub mod lazy;
pub mod live;
pub mod nav;
pub mod outbox;
pub mod pair;
pub mod platform;
pub mod push;
pub mod routes;
pub mod runbooks;
pub mod session;
pub mod settings;
pub mod spawn;
pub mod store;
pub mod triage;
pub mod ui;
pub mod version;
pub mod voice;

/// Boots the app: panic reporting, then the UI (which registers the
/// service worker).
pub fn start() {
    platform::install_panic_hook();
    leptos::mount::mount_to_body(app::App);
}
