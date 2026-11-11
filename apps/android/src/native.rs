//! The shell's own Kotlin side, `dev.passporter.app.ShellPlugin`
//! (`gen/android/app/src/main/java/dev/passporter/app/`), registered as the
//! Tauri plugin `shell`.
//!
//! It tells the shell what to show ([`Request`]: a notification tap, the
//! *Bridge* shortcut, a failed load) and restarts the app after the bridge
//! is forgotten. The plugin has no permissions, so no page can call it:
//! Tauri checks every plugin command against the ACL.

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::plugin::mobile::PluginInvokeError;
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{Manager, Runtime};

use crate::destination::Request;

/// The Kotlin package and class of the plugin.
const PACKAGE: &str = "dev.passporter.app";
const CLASS: &str = "ShellPlugin";

/// The registered plugin.
pub(crate) struct Native<R: Runtime>(PluginHandle<R>);

/// The arguments of `watch`.
#[derive(Serialize)]
struct Watch {
    /// Where the Kotlin side sends later requests.
    channel: Channel,
}

/// The answer to `watch`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Watching {
    /// What the intent that started the app asked for, if anything.
    launch: Option<Request>,
}

impl<R: Runtime> Native<R> {
    /// Starts sending every later request to `on_request`, and answers what
    /// the intent that started the app asks for. `on_request` runs on the
    /// Android UI thread: it must not wait on it (no `run_mobile_plugin`).
    pub(crate) fn watch(
        &self,
        on_request: impl Fn(Request) + Send + Sync + 'static,
    ) -> Result<Option<Request>, PluginInvokeError> {
        let channel = Channel::new(move |body: InvokeResponseBody| {
            match body.deserialize::<Request>() {
                Ok(request) => on_request(request),
                Err(error) => crate::shell::log_to_logcat(&error),
            }
            Ok(())
        });
        let watching: Watching = self.0.run_mobile_plugin("watch", Watch { channel })?;
        Ok(watching.launch)
    }

    /// Restarts the app in a new process, after the Kotlin side forgets the
    /// native pairing. A capability Tauri granted cannot be taken back, so
    /// only a new process drops the old bridge's IPC grant. Returns once
    /// the restart is under way; the process ends soon after.
    pub(crate) fn restart(&self) -> Result<(), PluginInvokeError> {
        self.0
            .run_mobile_plugin::<IgnoredAny>("restart", ())
            .map(|_| ())
    }
}

/// Access to the [`Native`] plugin from an app handle.
pub(crate) trait NativeExt<R: Runtime> {
    /// The registered plugin.
    fn native(&self) -> &Native<R>;
}

impl<R: Runtime, T: Manager<R>> NativeExt<R> for T {
    fn native(&self) -> &Native<R> {
        self.state::<Native<R>>().inner()
    }
}

/// The plugin, for `AppHandle::plugin`.
pub(crate) fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("shell")
        .setup(|app, api| {
            let handle = api.register_android_plugin(PACKAGE, CLASS)?;
            app.manage(Native(handle));
            Ok(())
        })
        .build()
}
