//! Passporter's Android app: a Tauri 2 shell around the PWA (design §3,
//! decision D8).
//!
//! The shell does not bundle the PWA. It asks for the bridge's HTTPS
//! address (once; *Change address* on its Bridge page starts over), then
//! loads the PWA from there, same origin as in Chrome, and lets that origin
//! (and no other page) reach native plugins over Tauri's IPC. The pieces
//! that decide what is allowed are plain Rust, tested on any host; the
//! Tauri wiring builds for Android only.

pub mod destination;
pub mod navigation;
pub mod origin;
pub mod store;

#[cfg(target_os = "android")]
mod native;
#[cfg(target_os = "android")]
mod shell;

#[cfg(target_os = "android")]
pub use shell::run;
