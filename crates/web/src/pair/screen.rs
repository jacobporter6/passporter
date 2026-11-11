//! The Connect screen's body: QR / code pairing, or the pasted token over
//! plain HTTP ([`super::token`]).
//!
//! This runs in `pair_bg.wasm` (`src/bin/pair.rs`), which the page loads
//! only when the screen opens ([`super::module`]), keeping the camera, key
//! generation and these views out of the app's WASM. The module renders
//! with its own Leptos runtime into an element the page mounts, below the
//! page's header. The page keeps the session: the module hands it what it
//! made (the device key, or the checked token) through the [`Host`] it
//! hands over, and the page stores and adopts it.

use core::cell::RefCell;

use js_sys::Promise;
use leptos::ev::SubmitEvent;
use leptos::html::Video;
use leptos::mount::{UnmountHandle, mount_to};
use leptos::prelude::*;
use leptos::tachys::view::any_view::AnyViewState;
use leptos::task::spawn_local;
use protocol::PairRequest;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::JsFuture;
use web_sys::{CryptoKey, HtmlElement, MediaStream};

use super::code::{self, PairingCode};
use super::device;
use super::scanner::{self, QrDetector, ScanError};
use super::token::TokenForm;
use crate::api::FetchTransport;
use crate::api::auth::{DeviceSigner, pair_device};
use crate::platform;
use crate::session;
use crate::ui::icons::{self, Icon};

/// How often the scanner looks at a camera frame.
const SCAN_INTERVAL_MS: i32 = 200;

#[wasm_bindgen]
extern "C" {
    /// The page, as `crate::pair::module` builds it.
    #[derive(Debug, Clone)]
    pub type Host;

    /// A note for the user.
    #[wasm_bindgen(method)]
    fn toast(this: &Host, text: &str);

    /// Stores the device key just registered as `device_id`, and signs in
    /// with it. Resolves to `undefined`, or to the message to show when it
    /// failed.
    #[wasm_bindgen(method, js_name = adoptDevice)]
    fn adopt_device(this: &Host, device_id: &str, private: &CryptoKey) -> Promise;

    /// Resolves to whether the page pairs through the Android app's native
    /// layer ([`super::native`]) rather than with a `WebCrypto` key.
    #[wasm_bindgen(method)]
    fn native(this: &Host) -> Promise;

    /// Pairs through the native layer with `code` and signs in. Resolves
    /// like [`Host::adopt_device`].
    #[wasm_bindgen(method, js_name = pairNative)]
    fn pair_native(this: &Host, code: &str) -> Promise;

    /// Stores `token` as this device's credential. Resolves like
    /// [`Host::adopt_device`].
    #[wasm_bindgen(method, js_name = saveToken)]
    fn save_token(this: &Host, token: &str) -> Promise;

    /// Leaves the screen for the agents.
    #[wasm_bindgen(method)]
    fn done(this: &Host);
}

/// The screen as mounted: the page's view of the device, and the view.
struct Mounted {
    paired: ArcRwSignal<bool>,
    _view: UnmountHandle<AnyViewState>,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    static MOUNTED: RefCell<Option<Mounted>> = const { RefCell::new(None) };
}

/// Draws the screen in `root`, replacing any earlier mount. `paired` is
/// whether the device holds a credential already.
pub fn mount(host: Host, root: HtmlElement, paired: bool) {
    unmount();
    HOST.set(Some(host));
    let signal = ArcRwSignal::new(paired);
    let paired_now = signal.clone();
    let view = mount_to(root, move || {
        let paired = RwSignal::from(paired_now).read_only();
        if session::secure_context() {
            view! { <CodePairing paired=paired /> }.into_any()
        } else {
            view! { <TokenForm paired=paired /> }.into_any()
        }
    });
    MOUNTED.set(Some(Mounted {
        paired: signal,
        _view: view,
    }));
}

/// The page's view of whether the device holds a credential changed.
pub fn set_paired(paired: bool) {
    MOUNTED.with_borrow(|mounted| {
        if let Some(mounted) = mounted {
            mounted.paired.set(paired);
        }
    });
}

/// Removes the screen (the page left it), turning the camera off. Work
/// still in flight no longer toasts or leaves the screen for the user.
pub fn unmount() {
    HOST.set(None);
    // Dropped outside the borrow: cleanups may run arbitrary code.
    let mounted = MOUNTED.take();
    drop(mounted);
}

/// A note for the user, through the page.
pub(super) fn toast(text: &str) {
    if let Some(host) = host() {
        host.toast(text);
    }
}

/// Back to the agents, through the page.
pub(super) fn done() {
    if let Some(host) = host() {
        host.done();
    }
}

/// Asks `host` (from [`host`], taken when the work started) to store
/// `token`.
pub(super) async fn save_token(host: Option<Host>, token: &str) -> Result<(), String> {
    settle(host.map(|host| host.save_token(token))).await
}

/// The page while the screen is mounted.
pub(super) fn host() -> Option<Host> {
    HOST.with_borrow(Clone::clone)
}

/// Awaits a host call's promise: `undefined` is success, a string the
/// failure's message.
async fn settle(promise: Option<Promise>) -> Result<(), String> {
    let promise = promise.ok_or_else(|| "The app isn't ready. Reload and try again.".to_owned())?;
    match JsFuture::from(promise).await {
        Ok(outcome) => outcome.as_string().map_or(Ok(()), Err),
        Err(error) => Err(platform::js_error_message(&error)),
    }
}

/// Pairs this device with `code` (from `passporter pair`): makes a key and
/// registers it, then hands it to the page, which stores it and signs in.
/// The page is the one this pairing started on: a key the bridge has
/// registered is adopted even if the user left the screen meanwhile. In the
/// Android app the page pairs through the native layer instead.
async fn pair(code: protocol::PairingCode) -> Result<(), String> {
    let host = host();
    if let Some(host) = &host {
        let native = JsFuture::from(host.native()).await;
        if native.is_ok_and(|native| native.as_bool() == Some(true)) {
            return settle(Some(host.pair_native(code.as_str()))).await;
        }
    }
    let new = device::generate().await.map_err(|e| e.to_string())?;
    let paired = pair_device(
        &FetchTransport,
        &PairRequest {
            code,
            public_key: new.public.clone(),
            name: None,
        },
    )
    .await
    .map_err(super::pairing_failure)?;
    let key = new.paired(paired.device_id);
    settle(host.map(|host| host.adopt_device(key.device_id().as_str(), key.private_key()))).await
}

/// The "This phone is connected" card, while it is.
#[component]
pub(super) fn ConnectedCard(paired: ReadSignal<bool>, hint: &'static str) -> impl IntoView {
    view! {
        <Show when=move || paired.get()>
            <div class="card card-success" role="status">
                <span class="card-icon"><Icon paths=icons::CHECK /></span>
                <span class="card-body">
                    <span class="card-title">"This phone is connected"</span>
                    <span class="card-text">{hint}</span>
                </span>
            </div>
        </Show>
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Camera {
    Off,
    Starting,
    Scanning,
    Failed(String),
}

/// Where pairing with an accepted code stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pairing {
    Working(PairingCode),
    Failed(PairingCode, String),
}

/// QR / code pairing (secure contexts only).
#[component]
fn CodePairing(paired: ReadSignal<bool>) -> impl IntoView {
    let accepted = RwSignal::new(None::<Pairing>);
    let camera = RwSignal::new(Camera::Off);
    let stream = StoredValue::new_local(None::<MediaStream>);
    // Camera session counter: bumped whenever a session starts or must end
    // (code accepted, screen left), so a camera that opens late (after the
    // permission prompt) knows it is stale and turns itself off.
    let session = StoredValue::new(0_u32);
    let video = NodeRef::<Video>::new();

    let stop = move || {
        session.try_update_value(|s| *s = s.wrapping_add(1));
        if let Some(s) = stream.try_update_value(Option::take).flatten() {
            platform::stop_tracks(&s);
        }
    };
    let accept = move |code: PairingCode| {
        stop();
        camera.set(Camera::Off);
        platform::haptic_tick();
        accepted.set(Some(Pairing::Working(code.clone())));
        spawn_local(async move {
            match pair(code.to_protocol()).await {
                Ok(()) => {
                    platform::haptic_tick();
                    toast("Paired");
                    done();
                }
                Err(message) => {
                    let _ = accepted.try_set(Some(Pairing::Failed(code, message)));
                }
            }
        });
    };
    on_cleanup(stop);

    let start = move |()| {
        stop();
        let id = session.get_value();
        camera.set(Camera::Starting);
        spawn_local(async move {
            let current = move || session.try_get_value() == Some(id);
            // A stale session's outcome is dropped: the user moved on.
            match scan(video, stream, camera, current).await {
                Ok(Some(code)) if current() => accept(code),
                Err(error) if current() => {
                    stop();
                    let _ = camera.try_set(Camera::Failed(error.to_string()));
                }
                Ok(_) | Err(_) => {}
            }
        });
    };

    view! {
        <div class="code-pairing">
            <ConnectedCard paired=paired hint="Pair again with a new code to replace it." />
            {move || match accepted.get() {
                Some(pairing) => view! { <Progress pairing=pairing on_reset=move |()| accepted.set(None) /> }.into_any(),
                None => view! {
                    <Viewfinder video=video camera=camera on_start=start />
                    <div class="divider" role="separator"><span>"or enter the code"</span></div>
                    <CodeForm on_code=accept />
                }.into_any(),
            }}
        </div>
    }
}

/// Opens the camera and polls frames until a valid code appears or the
/// session ends (`current` turns false: accepted, restarted or screen left).
async fn scan(
    video: NodeRef<Video>,
    stream: StoredValue<Option<MediaStream>, LocalStorage>,
    camera: RwSignal<Camera>,
    current: impl Fn() -> bool,
) -> Result<Option<PairingCode>, ScanError> {
    let detector = QrDetector::new()?;
    let Some(element) = video.get_untracked() else {
        return Ok(None);
    };
    let opened = scanner::start_camera(&element).await?;
    if !current() || stream.try_set_value(Some(opened.clone())).is_some() {
        // The session ended while the permission prompt was up.
        platform::stop_tracks(&opened);
        return Ok(None);
    }
    let _ = camera.try_set(Camera::Scanning);
    while current() {
        if let Some(code) = detector
            .detect(&element)
            .await
            .and_then(|raw| PairingCode::parse(&raw).ok())
        {
            return Ok(Some(code));
        }
        scanner::sleep(SCAN_INTERVAL_MS).await;
    }
    Ok(None)
}

#[component]
fn Viewfinder(
    video: NodeRef<Video>,
    camera: RwSignal<Camera>,
    #[prop(into)] on_start: Callback<()>,
) -> impl IntoView {
    if !scanner::supported() {
        return view! {
            <p class="notice">
                <Icon paths=icons::QR />
                <span>"QR scanning needs Chrome on Android. Type the code below instead."</span>
            </p>
        }
        .into_any();
    }
    let live = move || matches!(camera.get(), Camera::Starting | Camera::Scanning);
    view! {
        <div class="viewfinder" class:viewfinder-live=live>
            <video node_ref=video class="viewfinder-video" autoplay=true muted=true playsinline=true prop:muted=true aria-label="Camera preview"></video>
            <div class="viewfinder-frame" aria-hidden="true"></div>
            <Show when=move || !live()>
                <div class="viewfinder-idle">
                    <Icon paths=icons::QR />
                    {move || match camera.get() {
                        Camera::Failed(message) => view! { <p class="field-error" role="alert">{message}</p> }.into_any(),
                        _ => view! { <p>"Point the camera at the QR code"</p> }.into_any(),
                    }}
                    <button type="button" class="button button-primary" on:click=move |_| on_start.run(())>
                        "Start camera"
                    </button>
                </div>
            </Show>
            <Show when=move || camera.get() == Camera::Scanning>
                <p class="viewfinder-hint" role="status">"Looking for a code…"</p>
            </Show>
        </div>
    }
    .into_any()
}

#[component]
fn CodeForm(#[prop(into)] on_code: Callback<PairingCode>) -> impl IntoView {
    let input = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        match PairingCode::parse(&input.get_untracked()) {
            Ok(code) => on_code.run(code),
            Err(e) => error.set(Some(e.to_string())),
        }
    };
    view! {
        <form class="code-form" on:submit=on_submit novalidate=true>
            <label class="field-label" for="pair-code">"Pairing code"</label>
            <div class="code-row">
                <input
                    id="pair-code"
                    class="field code-field"
                    type="text"
                    inputmode="text"
                    autocomplete="one-time-code"
                    autocapitalize="characters"
                    spellcheck="false"
                    maxlength=code::MAX_INPUT_LEN.to_string()
                    placeholder="XXXX-XXXX"
                    aria-invalid=move || error.get().is_some().to_string()
                    aria-describedby="pair-code-error"
                    prop:value=move || input.get()
                    on:input=move |ev| {
                        input.set(event_target_value(&ev));
                        error.set(None);
                    }
                />
                <button type="submit" class="button button-primary">"Pair"</button>
            </div>
            <p id="pair-code-error" class="field-error" role="alert">{move || error.get()}</p>
        </form>
    }
}

#[component]
fn Progress(pairing: Pairing, #[prop(into)] on_reset: Callback<()>) -> impl IntoView {
    match pairing {
        Pairing::Working(code) => view! {
            <div class="card" role="status" aria-live="polite">
                <span class="card-icon"><Icon paths=icons::QR /></span>
                <span class="card-body">
                    <span class="card-title">"Pairing with " <span class="mono">{code.to_string()}</span> "…"</span>
                    <span class="card-text">"Making this phone's key and registering it with the bridge."</span>
                </span>
            </div>
        }
        .into_any(),
        Pairing::Failed(code, message) => view! {
            <div class="card" role="alert">
                <span class="card-body">
                    <span class="card-title">"Couldn't pair with " <span class="mono">{code.to_string()}</span></span>
                    <span class="card-text field-error">{message}</span>
                </span>
            </div>
            <button type="button" class="button button-secondary button-block" on:click=move |_| on_reset.run(())>
                "Try another code"
            </button>
        }
        .into_any(),
    }
}
