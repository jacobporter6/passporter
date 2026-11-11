//! The Tauri wiring: registers the Kotlin plugin, and exposes the one call
//! the shell makes into it.

use serde::Deserialize;
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{Manager, Runtime};

/// The Kotlin package and class of the plugin.
const PACKAGE: &str = "dev.passporter.device";
const CLASS: &str = "DevicePlugin";

/// Why the plugin could not answer.
#[derive(Debug, thiserror::Error)]
#[error("device plugin: {0}")]
pub struct Error(#[from] tauri::plugin::mobile::PluginInvokeError);

/// The device plugin, as the app holds it.
pub struct Device<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> std::fmt::Debug for Device<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Device")
    }
}

/// The answer to `webviewFeatures`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WebviewFeatures {
    web_message_listener: bool,
}

impl<R: Runtime> Device<R> {
    /// Whether the system `WebView` supports `WEB_MESSAGE_LISTENER`
    /// (androidx.webkit), through which Tauri learns each frame's own
    /// origin. Without it every IPC message carries the main frame's URL,
    /// so a remote capability cannot be trusted to mean that origin.
    pub fn webview_isolates_frames(&self) -> Result<bool, Error> {
        let features: WebviewFeatures = self.0.run_mobile_plugin("webviewFeatures", ())?;
        Ok(features.web_message_listener)
    }
}

/// Access to the [`Device`] plugin from an app handle.
pub trait DeviceExt<R: Runtime> {
    /// The registered device plugin.
    fn device(&self) -> &Device<R>;
}

impl<R: Runtime, T: Manager<R>> DeviceExt<R> for T {
    fn device(&self) -> &Device<R> {
        self.state::<Device<R>>().inner()
    }
}

/// The plugin, for `tauri::Builder::plugin`. Commands from the webview go
/// straight to the Kotlin plugin (Tauri forwards plugin commands that Rust
/// does not handle), after the ACL has checked them.
#[must_use]
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("device")
        .setup(|app, api| {
            let handle = api.register_android_plugin(PACKAGE, CLASS)?;
            app.manage(Device(handle));
            Ok(())
        })
        .build()
}
