//! The Tauri app (Android only): one webview that shows the bundled
//! Connect page until a bridge is chosen, then the bridge's own PWA.
//!
//! Native IPC is fenced twice. The webview only ever navigates to the
//! bundled pages or the paired origin ([`crate::navigation`]), and the
//! bridge capability grants IPC to that exact origin only
//! ([`BridgeOrigin::capability_pattern`]). The app defines no ACL manifest,
//! so its own commands (`connect`, `bridge`, `forget_bridge`) answer bundled
//! pages only: Tauri refuses app commands to any remote page that no
//! capability names.
//!
//! The Kotlin side ([`crate::native`]) asks for the other screens: an agent
//! on a notification tap, the bundled Bridge page from the launcher
//! shortcut or when a bridge page fails to load ([`crate::destination`]).

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tauri::ipc::CapabilityBuilder;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};

use crate::destination::{self, Request};
use crate::native::{self, NativeExt};
use crate::navigation::{self, Guard};
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
        .invoke_handler(tauri::generate_handler![connect, bridge, forget_bridge])
        .setup(|app| {
            setup(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the Passporter shell failed to start");
}

/// Loads the saved bridge, if any, and opens the main window on it (or on
/// the Connect page, or where the intent that started the app asks).
fn setup(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    // Without the Kotlin side, taps and the shortcut do nothing; the app
    // still works.
    if let Err(error) = app.plugin(native::init()) {
        log_to_logcat(&error);
    }
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
    let guard = Arc::new(Guard::new(Arc::clone(&bridge)));
    let requests = Arc::clone(&guard);
    let handle = app.clone();
    let launch = app.native().and_then(|native| {
        native
            .watch(move |request| show(&handle, &request, &requests))
            .unwrap_or_else(|error| {
                log_to_logcat(&error);
                None
            })
    });
    // An agent on the bridge is the first page. The Bridge page is shown
    // once the window is up instead: the first page is also what wry
    // reloads if it ever rebuilds the webview, and the guard refuses a
    // bundled page there once paired.
    let (start, later) = match launch
        .as_ref()
        .and_then(|request| destination::destination(request, bridge.get()))
    {
        Some(url) if navigation::allows(&url, bridge.get()) => (WebviewUrl::External(url), None),
        _ => (start, launch),
    };
    app.manage(Shell {
        store,
        connecting: Mutex::new(()),
        bridge,
    });
    WebviewWindowBuilder::new(app, MAIN, start)
        .use_https_scheme(true)
        .on_navigation({
            let guard = Arc::clone(&guard);
            move |url| guard.allows(url)
        })
        .build()?;
    if let Some(request) = later {
        show(app, &request, &guard);
    }
    Ok(())
}

/// Shows what the Kotlin side asked for, if a bridge is paired.
fn show(app: &AppHandle, request: &Request, guard: &Guard) {
    let Some(url) = destination::destination(request, guard.bridge()) else {
        return;
    };
    // No window yet: the request came during setup, before there was
    // anything to show it in.
    let Some(window) = app.get_webview_window(MAIN) else {
        return;
    };
    // wry checks this load against the guard too.
    guard.expect(&url);
    if let Err(error) = window.navigate(url) {
        guard.unexpect();
        log_to_logcat(&error);
    }
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
/// answers the URL to open. Answers the bundled Connect page only, which
/// then replaces itself with that URL (`location.replace`), so Back never
/// returns to it: history navigations skip the navigation guard.
///
/// The bridge is chosen once per process: to change it, the Bridge page
/// calls [`forget_bridge`], which restarts the app. The address is saved
/// first, so a failure there changes nothing.
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
            "already connected to {current}; change it from the Bridge page"
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
    granted.map_err(|error| format!("could not allow the bridge: {error}"))?;
    Ok(url)
}

/// The paired bridge's origin, for the bundled Bridge page; `None` before
/// connecting.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri hands command arguments over by value"
)]
fn bridge(shell: State<'_, Shell>) -> Option<String> {
    shell.bridge.get().map(ToString::to_string)
}

/// Forgets the bridge, then restarts the app, which opens the Connect page.
/// Answers the bundled Bridge page only.
///
/// The restart is the point: Tauri cannot take back the capability granted
/// to the old origin, and the webview may hold its pages, so only a new
/// process starts clean. The saved address goes first: if anything after it
/// fails, the next launch opens the Connect page anyway.
///
/// It must stay `async`: Tauri runs async commands off the main thread, and
/// the blocking call into Kotlin (`restart`) must not run on it.
#[tauri::command]
async fn forget_bridge(app: AppHandle, shell: State<'_, Shell>) -> Result<(), String> {
    shell
        .store
        .forget()
        .map_err(|error| format!("could not forget the address: {error}"))?;
    let native = app.native().ok_or_else(|| {
        "forgot the address, but could not restart; close and reopen the app".to_owned()
    })?;
    native.restart().map_err(|error| {
        format!("forgot the address, but could not restart: {error}; close and reopen the app")
    })
}

/// Logs a problem the app recovers from: stderr goes to logcat.
#[expect(
    clippy::print_stderr,
    reason = "the shell has no logger; stderr is logcat"
)]
pub(crate) fn log_to_logcat(error: &dyn std::error::Error) {
    eprintln!("passporter: {error}");
}
