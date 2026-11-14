//! The Tauri app (Android only): one webview that shows the bundled
//! Connect page until a bridge is chosen, then the bridge's own PWA.
//!
//! Native IPC is fenced twice. The webview only ever navigates to the
//! bundled pages or the paired origin ([`crate::navigation`]), and the
//! bridge capability grants IPC to that exact origin only
//! ([`BridgeOrigin::capability_pattern`]). The app defines no ACL manifest,
//! so Tauri lets its own commands (`connect`, `bridge`, `forget_bridge`)
//! answer only messages it labels local, from the bundled pages. On a
//! `WebView` without `WEB_MESSAGE_LISTENER` that label is the URL the main
//! frame last started loading, so a message a bridge page sends while the
//! shell replaces it with the Bridge page can pass as local: there
//! [`forget_bridge`] refuses ([`grant_ipc`] has the details), and `connect`
//! refuses anyway once a bridge is chosen.
//!
//! The Kotlin side ([`crate::native`]) asks for the other screens: an agent
//! on a notification tap, the bundled Bridge page from the launcher
//! shortcut or when a bridge page fails to load, and the bridge again when
//! wry rebuilds the webview on a page the guard now refuses
//! ([`crate::destination`]).

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tauri::ipc::CapabilityBuilder;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_device::DeviceExt;

use crate::connect::Connected;
use crate::destination::{self, CONNECT_PAGE, FirstPage, Request};
use crate::native::{self, NativeExt};
use crate::navigation::Guard;
use crate::origin::BridgeOrigin;
use crate::store::Store;

/// The app's single window and webview.
const MAIN: &str = "main";

/// The runtime capability that lets the bridge's PWA reach native plugins.
const BRIDGE_CAPABILITY: &str = "bridge";

/// Why [`forget_bridge`] refuses on a `WebView` without
/// `WEB_MESSAGE_LISTENER`, and what to do instead.
const OLD_WEBVIEW_FORGET: &str = "The app can't change the bridge safely on this WebView (an \
     old Android System WebView?). Update it, or clear Passporter's storage in Android's app \
     settings to start over.";

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
        .plugin(tauri_plugin_recorder::init())
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
    match store.load() {
        Ok(Some(origin)) => {
            // Without the grant the PWA still works, minus native plugins.
            if let Err(error) = grant_ipc(app, &origin) {
                log_to_logcat(&error);
            }
            let _ = bridge.set(origin);
        }
        Ok(None) => {}
        // A trace for "the app forgot its bridge".
        Err(error) => log_to_logcat(&error),
    }
    let guard = Arc::new(Guard::new(Arc::clone(&bridge)));
    // The first page depends on what `watch` answers, so it is set just
    // after; a request that comes sooner has no window to show it in yet.
    let first = Arc::new(OnceLock::new());
    let launch = app.native().and_then(|native| {
        let handle = app.clone();
        let guard = Arc::clone(&guard);
        let first = Arc::clone(&first);
        native
            .watch(move |request| {
                if let Some(first) = first.get() {
                    show(&handle, &request, &guard, first);
                }
            })
            .unwrap_or_else(|error| {
                log_to_logcat(&error);
                None
            })
    });
    let (first_page, later) = FirstPage::choose(bridge.get(), launch);
    let start = match &first_page {
        FirstPage::Connect => WebviewUrl::App(CONNECT_PAGE.into()),
        FirstPage::Bridge(url) => WebviewUrl::External(url.clone()),
    };
    let _ = first.set(first_page.clone());
    let first = first_page;
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
        // Once the first page has loaded: wry posts that first load, so a
        // navigation sent any earlier would be replaced by it.
        .on_page_load({
            let later = Mutex::new(later);
            move |window, payload| {
                if payload.event() == PageLoadEvent::Finished
                    && let Some(request) =
                        later.lock().unwrap_or_else(PoisonError::into_inner).take()
                {
                    show(window.app_handle(), &request, &guard, &first);
                }
            }
        })
        .build()?;
    Ok(())
}

/// Shows what the Kotlin side asked for, if a bridge is paired. `first` is
/// the webview's first page, which wry loads again when it rebuilds it.
fn show(app: &AppHandle, request: &Request, guard: &Guard, first: &FirstPage) {
    let Some(url) = destination::destination(request, guard.bridge(), first) else {
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
/// the bridge capability lists: the device identity (`device:default`) and
/// the dictation recorder (`recorder:default`).
///
/// Tauri tells frames apart only on `WebView`s with `WEB_MESSAGE_LISTENER`
/// (`WebView` 86 and later); on older ones every frame's message carries the
/// main frame's URL, and only the PWA's CSP keeps foreign frames out.
/// Messages sent while the main frame changes pages can even be labelled
/// local: the Connect page's while it is replaced by the bridge, or a bridge
/// page's while the shell replaces it with the Bridge page. So nothing is
/// granted without that feature (the PWA then keeps its device key in
/// `WebCrypto`, as in Chrome), the app never adds a local capability, and
/// [`forget_bridge`] refuses (docs/development/android.md, "How the shell
/// works").
fn grant_ipc(app: &AppHandle, origin: &BridgeOrigin) -> Result<(), GrantError> {
    if !app.device().webview_isolates_frames()? {
        return Err(GrantError::WebViewTooOld);
    }
    app.add_capability(
        CapabilityBuilder::new(BRIDGE_CAPABILITY)
            .remote(origin.capability_pattern())
            .local(false)
            .window(MAIN)
            .permission("device:default")
            .permission("recorder:default"),
    )?;
    Ok(())
}

/// Pairs the app with the bridge at `address` (what the person typed) and
/// answers the URL to open. Answers the bundled Connect page only, which
/// then replaces itself with that URL (`location.replace`), so Back never
/// returns to it: history navigations skip the navigation guard.
///
/// The bridge is chosen once per process ([`crate::connect::connect`]): to
/// change it, the Bridge page calls [`forget_bridge`], which restarts the
/// app.
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri hands command arguments over by value"
)]
fn connect(app: AppHandle, shell: State<'_, Shell>, address: &str) -> Result<String, String> {
    let _connecting = shell
        .connecting
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let Connected { url, grant } =
        crate::connect::connect(&shell.store, &shell.bridge, address, |origin| {
            grant_ipc(&app, origin)
        })?;
    // Nothing was granted, and the PWA works without native plugins, as in
    // Chrome. The bridge is chosen by now, so this is not an error: the
    // next launch tries the grant again.
    if let Err(error) = grant {
        log_to_logcat(&error);
    }
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
/// Answers the bundled Bridge page only, and only on a `WebView` that tells
/// frames apart: on an older one a bridge page's message can be labelled
/// local (see [`grant_ipc`]), and no script on the bridge may unpair the
/// app. There, clearing the app's data starts over instead.
///
/// The restart is the point: Tauri cannot take back the capability granted
/// to the old origin, and the webview may hold its pages, so only a new
/// process starts clean. The saved address goes first: if anything after it
/// fails, the next launch opens the Connect page anyway.
///
/// It is `async` so the call into Kotlin (`restart`) is awaited off the
/// main thread.
#[tauri::command]
async fn forget_bridge(app: AppHandle, shell: State<'_, Shell>) -> Result<(), String> {
    // Fails closed: a WebView the plugin cannot check counts as old.
    let isolated = app
        .device()
        .webview_isolates_frames()
        .unwrap_or_else(|error| {
            log_to_logcat(&error);
            false
        });
    if !isolated {
        return Err(OLD_WEBVIEW_FORGET.to_owned());
    }
    shell
        .store
        .forget()
        .map_err(|error| format!("could not forget the address: {error}"))?;
    let native = app.native().ok_or_else(|| {
        "forgot the address, but could not restart; close and reopen the app".to_owned()
    })?;
    native.restart().await.map_err(|error| {
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
