//! The Tauri wiring: registers the Kotlin plugin.

use tauri::Runtime;
use tauri::plugin::{Builder, TauriPlugin};

/// The Kotlin package and class of the plugin.
const PACKAGE: &str = "dev.passporter.recorder";
const CLASS: &str = "RecorderPlugin";

/// The plugin, for `tauri::Builder::plugin`. Commands from the webview go
/// straight to the Kotlin plugin (Tauri forwards plugin commands that Rust
/// does not handle), after the ACL has checked them.
#[must_use]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("recorder")
        .setup(|_app, api| {
            api.register_android_plugin(PACKAGE, CLASS)?;
            Ok(())
        })
        .build()
}
