//! Passporter's dictation recorder on Android (design §6.3).
//!
//! In a webview, `MediaRecorder` lives and dies with the page: a long
//! dictation is lost if Android kills the app, and the PWA stops a take
//! when the screen turns off. In the APK the plugin's Kotlin code
//! (`android/src/main/java/dev/passporter/recorder/`) records instead, in a
//! foreground service, to app-private files written frame by frame, so a
//! take survives the screen turning off, the webview reloading and even the
//! process being killed (what was written is kept). The PWA starts and
//! stops takes and reads the finished audio over IPC
//! (`crates/web/src/voice/native.rs`), then stores, uploads and transcribes
//! it exactly as it does a browser recording.
//!
//! The Rust side only registers the Kotlin plugin: every command goes
//! straight to Kotlin, after the ACL has checked it.

#[cfg(target_os = "android")]
mod mobile;

#[cfg(target_os = "android")]
pub use mobile::init;
