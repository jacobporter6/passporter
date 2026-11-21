//! Passporter's device identity on Android (design §8).
//!
//! Inside the APK the device key is a non-exportable P-256 key in the
//! Android Keystore, owned by the plugin's Kotlin code
//! (`android/src/main/java/dev/passporter/device/`), which also pairs,
//! signs in and caches session tokens. Native features (notification
//! replies, push registration, Android Auto) call its `DeviceAuth` directly,
//! so they work with the app closed; the PWA reaches the same identity
//! through this plugin's commands (`status`, `pair`, `session`, `forget`),
//! and only ever gets an app-scope token.
//!
//! The Rust side only registers the Kotlin plugin and asks it one thing for
//! the shell: whether the `WebView` can tell frames apart
//! ([`Device::webview_isolates_frames`]), which gates every IPC grant.

#[cfg(target_os = "android")]
mod mobile;

#[cfg(target_os = "android")]
pub use mobile::{Device, DeviceExt, Error, init};
