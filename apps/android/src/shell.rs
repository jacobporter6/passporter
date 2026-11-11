//! The Tauri app (Android only): one webview that shows the bundled
//! Connect page until a bridge is chosen, then the bridge's own PWA.
//!
//! Native IPC is fenced twice. The webview only ever navigates to the
//! bundled pages or the paired origin ([`crate::navigation`]), and the
//! bridge capability grants IPC to that exact origin only
//! ([`BridgeOrigin::capability_pattern`]). The app defines no ACL manifest,
//! so its own commands (`connect`) answer bundled pages only: Tauri refuses
//! app commands to any remote page that no capability names.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tauri::ipc::CapabilityBuilder;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_device::DeviceExt;

use crate::navigation;
use crate::origin::BridgeOrigin;
use crate::store::Store;

/// The app's single window and webview.
const MAIN: &str = "main";

/// The runtime capability that lets the bridge's PWA reach native plugins.
const BRIDGE_CAPABILITY: &str = "bridge";

/// The bundled page that asks for the bridge's address.
const CONNECT_PAGE: &str = "index.html";

/// Shared between the `connect` command and the navigation guard.
struct Shell {
    store: Store,
    /// Held for the whole of a `connect`, so two never interleave.
    connecting: Mutex<()>,
    /// Set once: when a saved origin is loaded at launch, or by `connect`.
    bridge: Arc<OnceLock<BridgeOrigin>>,
}

/// Starts the app. Called by the Android activity through Tauri's entry point.
///
/// # Panics
///
/// If Tauri cannot start, which leaves nothing to show; the message goes
/// to logcat.
#[tauri::mobile_entry_point]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_device::init())
        .invoke_handler(tauri::generate_handler![connect])
        .setup(|app| {
            setup(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the Passporter shell failed to start");
}

/// Loads the saved bridge, if any, and opens the main window on it (or on
/// the Connect page).
fn setup(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::in_dir(&app.path().app_data_dir()?);
    let bridge = Arc::new(OnceLock::new());
    // Anything unreadable (no file yet, a corrupt one, a storage error)
    // opens the Connect page; `connect` then replaces the file, or shows
    // why it cannot.
    let start = match store.load() {
        Ok(Some(origin)) => {
            // Without the grant the PWA still works, minus native plugins.
            if let Err(error) = grant_ipc(app, &origin) {
                log_to_logcat(&error);
            }
            let url = origin.url().clone();
            let _ = bridge.set(origin);
            WebviewUrl::External(url)
        }
        Ok(None) => WebviewUrl::App(CONNECT_PAGE.into()),
        Err(error) => {
            // A trace for "the app forgot its bridge".
            log_to_logcat(&error);
            WebviewUrl::App(CONNECT_PAGE.into())
        }
    };
    let guard = Arc::clone(&bridge);
    app.manage(Shell {
        store,
        connecting: Mutex::new(()),
        bridge,
    });
    WebviewWindowBuilder::new(app, MAIN, start)
        .use_https_scheme(true)
        .on_navigation(move |url| navigation::allows(url, guard.get()))
        .build()?;
    Ok(())
}

/// Why the bridge capability was not granted.
#[derive(Debug, thiserror::Error)]
enum GrantError {
    /// The system `WebView` cannot tell frames apart (see [`grant_ipc`]).
    #[error(
        "this WebView lacks WEB_MESSAGE_LISTENER, so native features stay off; update Android System WebView"
    )]
    WebViewTooOld,
    /// The device plugin could not answer whether it can.
    #[error("could not check the WebView: {0}")]
    Probe(#[from] tauri_plugin_device::Error),
    /// Tauri refused the capability.
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
}

/// Lets pages on `origin`, in the main window only, use the native plugins
/// the bridge capability lists: the device identity (`device:default`).
///
/// Tauri tells frames apart only on `WebView`s with `WEB_MESSAGE_LISTENER`
/// (`WebView` 86 and later); on older ones every frame's message carries the
/// main frame's URL, and only the PWA's CSP keeps foreign frames out.
/// Messages sent while the Connect page is being replaced can even be
/// labelled local. So nothing is granted without that feature (the PWA then
/// keeps its device key in `WebCrypto`, as in Chrome), and the app never adds
/// a local capability (docs/development/android.md, "How the shell works").
fn grant_ipc(app: &AppHandle, origin: &BridgeOrigin) -> Result<(), GrantError> {
    if !app.device().webview_isolates_frames()? {
        return Err(GrantError::WebViewTooOld);
    }
    app.add_capability(
        CapabilityBuilder::new(BRIDGE_CAPABILITY)
            .remote(origin.capability_pattern())
            .local(false)
            .window(MAIN)
            .permission("device:default"),
    )?;
    Ok(())
}

/// Pairs the app with the bridge at `address` (what the person typed) and
/// answers the URL to open. Answers the bundled Connect page only, which
/// then replaces itself with that URL (`location.replace`), so Back never
/// returns to it: history navigations skip the navigation guard.
///
/// The bridge is chosen once: to change it, clear the app's storage. The
/// address is saved first, so a failure there changes nothing.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri hands command arguments over by value"
)]
fn connect(app: AppHandle, shell: State<'_, Shell>, address: &str) -> Result<String, String> {
    let origin = BridgeOrigin::parse(address).map_err(|error| error.to_string())?;
    let _connecting = shell
        .connecting
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(current) = shell.bridge.get() {
        return Err(format!(
            "already connected to {current}; clear the app's storage to change it"
        ));
    }
    shell
        .store
        .save(&origin)
        .map_err(|error| format!("could not save the address: {error}"))?;
    let url = origin.url().to_string();
    // Granted last: Tauri cannot take a capability back, so no grant is
    // made for an origin that did not become the bridge.
    let granted = grant_ipc(&app, &origin);
    shell
        .bridge
        .set(origin)
        .map_err(|_| "already connected".to_owned())?;
    match granted {
        Ok(()) => {}
        // Nothing was granted, and the PWA works without native plugins, as
        // in Chrome. The bridge is chosen by now, so this is not an error:
        // the next launch tries the grant again.
        Err(error @ (GrantError::WebViewTooOld | GrantError::Probe(_))) => log_to_logcat(&error),
        Err(error @ GrantError::Tauri(_)) => {
            return Err(format!("could not allow the bridge: {error}"));
        }
    }
    Ok(url)
}

/// Logs a launch problem the app recovers from: stderr goes to logcat.
#[expect(
    clippy::print_stderr,
    reason = "the shell has no logger; stderr is logcat"
)]
fn log_to_logcat(error: &dyn std::error::Error) {
    eprintln!("passporter: {error}");
}
