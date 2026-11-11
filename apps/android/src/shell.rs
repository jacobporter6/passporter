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
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

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
            grant_ipc(app, &origin)?;
            let url = origin.url().clone();
            let _ = bridge.set(origin);
            WebviewUrl::External(url)
        }
        Ok(None) => WebviewUrl::App(CONNECT_PAGE.into()),
        Err(error) => {
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

/// Lets pages on `origin`, in the main window only, use the native plugins
/// the bridge capability lists (none yet: later tasks add theirs).
///
/// Tauri tells frames apart only on `WebView`s with `WEB_MESSAGE_LISTENER`
/// (`WebView` 86 and later); on older ones every frame's message carries the
/// main frame's URL, and only the PWA's CSP keeps foreign frames out.
/// Messages sent while the Connect page is being replaced can even be
/// labelled local. Before the first permission is added here, gate the grant
/// on that feature, and never add a local capability (docs/development/android.md,
/// "How the shell works").
fn grant_ipc(app: &AppHandle, origin: &BridgeOrigin) -> tauri::Result<()> {
    app.add_capability(
        CapabilityBuilder::new(BRIDGE_CAPABILITY)
            .remote(origin.capability_pattern())
            .local(false)
            .window(MAIN),
    )
}

/// Pairs the app with the bridge at `address` (what the person typed) and
/// opens its PWA. Answers the bundled Connect page only.
///
/// The bridge is chosen once: to change it, clear the app's storage. The
/// address is saved first, so a failure there changes nothing.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri hands command arguments over by value"
)]
fn connect(
    app: AppHandle,
    window: WebviewWindow,
    shell: State<'_, Shell>,
    address: &str,
) -> Result<(), String> {
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
    let url = origin.url().clone();
    // Granted last: Tauri cannot take a capability back, so no grant is
    // made for an origin that did not become the bridge.
    let granted = grant_ipc(&app, &origin);
    shell
        .bridge
        .set(origin)
        .map_err(|_| "already connected".to_owned())?;
    granted.map_err(|error| format!("could not allow the bridge: {error}"))?;
    window
        .navigate(url)
        .map_err(|error| format!("could not open the bridge: {error}"))
}

/// Leaves a trace for "the app forgot its bridge": stderr goes to logcat.
#[expect(
    clippy::print_stderr,
    reason = "the shell has no logger; stderr is logcat"
)]
fn log_to_logcat(error: &dyn std::error::Error) {
    eprintln!("passporter: saved bridge unreadable, showing Connect: {error}");
}
