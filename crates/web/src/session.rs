//! This device's bridge credentials, and the [`TokenSource`] every API call
//! uses ([`SessionToken`], backed by [`AppState::token`]).
//!
//! Two ways to hold a token:
//!
//! - **Device session** (secure contexts; design §8). The key made at
//!   pairing signs a server challenge for a short-lived token. In a browser
//!   the key is `WebCrypto`'s ([`device`], [`sign_in`]); in the Android app
//!   it is the native layer's Keystore key ([`native`]), which signs in for
//!   the page ([`Device`]). The token is renewed the same way a minute before it
//!   expires, and at once when the bridge rejects it (after a bridge
//!   restart, say). Only when the bridge says it does not know the device
//!   (410: revoked) is the key deleted and the app asks to pair again; a
//!   401 while signing in (a challenge lost to a restart) keeps the key.
//! - **Pasted token** (the MVP fallback over plain HTTP, where `WebCrypto`
//!   does not exist). Kept in `IndexedDB`; forgotten on the first 401.
//!
//! [`credential`] says which one is in use, for Settings.

use core::cell::{Cell, RefCell};

use leptos::prelude::*;
use leptos::task::spawn_local;
use protocol::{DeviceName, SessionGrant};

use crate::api::auth::{DeviceAuthError, grant_token, sign_in};
use crate::api::{ApiClient, BearerToken, FetchTransport, TokenSource};
use crate::pair::device::{self, DeviceKey};
use crate::pair::native;
use crate::pair::scanner::sleep;
use crate::platform;
use crate::push;
use crate::push::state::Unavailable;
use crate::store::{Store, TokenKey};
use crate::ui::AppState;

/// Renew this long before the token expires.
pub const RENEW_EARLY_SECS: u64 = 60;
/// Never renew more often than this.
const MIN_RENEW_SECS: u64 = 30;
/// Wait between attempts when the bridge cannot be reached.
const RETRY_MS: i32 = 5_000;
/// The longest wait between attempts while the Android app's native
/// identity keeps failing ([`Attempt::Later`]).
const MAX_BACKOFF_MS: i32 = 300_000;

/// Where this device's identity lives. Generic over the browser key only
/// so the choice ([`choose`]) is testable on the host.
#[derive(Debug, Clone)]
enum Identity<K> {
    /// A `WebCrypto` key in this browser's `IndexedDB`.
    Browser(K),
    /// The Android app's Keystore key, held by its native layer.
    Native,
}

/// This device's identity.
type Device = Identity<DeviceKey>;

impl Device {
    /// Signs in for an app-scope session.
    async fn sign_in(&self) -> Result<SessionGrant, DeviceAuthError> {
        match self {
            Self::Browser(key) => sign_in(&FetchTransport, key).await,
            Self::Native => native::session().await,
        }
    }
}

/// The identity to sign in with at startup: the native one once it is
/// paired, else a browser key (also one left from before the app had a
/// native identity), else none. `native_paired` is `None` without a native
/// layer.
fn choose<K>(native_paired: Option<bool>, browser: Option<K>) -> Option<Identity<K>> {
    match (native_paired, browser) {
        (Some(true), _) => Some(Identity::Native),
        (_, Some(key)) => Some(Identity::Browser(key)),
        _ => None,
    }
}

/// Why the push switch is off in the Android app ([`in_app_push`]), from
/// whether its native layer is here (`None` before [`load`] has asked) and
/// the identity in use.
fn push_in_app<K>(native: Option<bool>, device: Option<&Identity<K>>) -> Unavailable {
    match (native, device) {
        (Some(false), _) => Unavailable::NativeOff,
        (Some(true), Some(Identity::Browser(_))) => Unavailable::PairInApp,
        _ => Unavailable::NativeApp,
    }
}

thread_local! {
    /// The paired device's identity, once loaded or made.
    static DEVICE: RefCell<Option<Device>> = const { RefCell::new(None) };
    /// Whether the Android app's native identity is here, once [`load`] has
    /// asked: Forget then clears it too ([`forget_device`]).
    static NATIVE: Cell<Option<bool>> = const { Cell::new(None) };
    /// Renewal generation: bumping it cancels the scheduled renewal and any
    /// retry loop.
    static GENERATION: Cell<u32> = const { Cell::new(0) };
    /// A sign-in is in flight, so a burst of 401s starts only one.
    static RENEWING: Cell<bool> = const { Cell::new(false) };
    /// What the device signs in with ([`credential`]).
    static CREDENTIAL: ArcRwSignal<Credential> = ArcRwSignal::new(Credential::None);
}

/// What this device signs in with.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Credential {
    /// Nothing: never paired, or forgotten.
    #[default]
    None,
    /// A paired device key. `name` is the bridge's label for the device,
    /// once a sign-in has reported it.
    Device {
        /// The device's name on the bridge.
        name: Option<DeviceName>,
    },
    /// A pasted static token (plain HTTP over `WireGuard`).
    StaticToken,
}

/// What this device signs in with, kept current as it pairs, signs in,
/// pastes a token or forgets it.
#[must_use]
pub fn credential() -> ArcRwSignal<Credential> {
    CREDENTIAL.with(Clone::clone)
}

fn set_credential(credential: Credential) {
    CREDENTIAL.with(|signal| signal.set(credential));
}

/// The token source backed by [`AppState::token`].
#[derive(Debug, Clone, Copy)]
pub struct SessionToken(pub AppState);

impl TokenSource for SessionToken {
    fn current(&self) -> Option<BearerToken> {
        self.0.token.get_untracked()
    }

    fn unauthorized(&self, rejected: &BearerToken) {
        let app = self.0;
        if app.token.get_untracked().as_ref() != Some(rejected) {
            return;
        }
        app.token.set(None);
        if current_device().is_some() {
            // Expired or forgotten by a restarted bridge: sign in again.
            spawn_local(renew(app));
            return;
        }
        app.token_rejected.set(true);
        set_credential(Credential::None);
        spawn_local(async move {
            if let Ok(store) = app.store().await {
                let _ = store.delete(&TokenKey).await;
            }
            // Pings would only wake to a refused token.
            push::module::turn_off(None).await;
        });
    }
}

/// The bridge client, authenticated as this device.
#[must_use]
pub fn api(app: AppState) -> ApiClient<FetchTransport, SessionToken> {
    ApiClient::new(FetchTransport, SessionToken(app))
}

/// Why the push switch is off in the Android app: notifications come
/// through the app's own `UnifiedPush` subscription, which needs its native
/// identity, so this says whether they can.
#[must_use]
pub fn in_app_push() -> Unavailable {
    DEVICE.with_borrow(|device| push_in_app(NATIVE.get(), device.as_ref()))
}

/// Whether device keys can work here (`WebCrypto` needs a secure context).
#[must_use]
pub fn secure_context() -> bool {
    web_sys::window().is_some_and(|w| w.is_secure_context())
}

fn current_device() -> Option<Device> {
    DEVICE.with_borrow(Clone::clone)
}

/// Loads credentials at startup: the pasted token, and in a secure context
/// the device key, which then signs in. A stored token that no longer
/// validates is deleted, and push turned off with it unless a device key
/// takes over (its subscription belonged to that token, and Settings can't
/// turn off push without a token).
pub async fn load(app: AppState) {
    let (token, dropped) = match app.store().await {
        Ok(store) => stored_token(&store).await,
        Err(_) => (None, false),
    };
    if token.is_some() && app.token.get_untracked().is_none() {
        app.token.set(token);
        set_credential(Credential::StaticToken);
    }
    if secure_context() {
        let native = native::probe().await;
        NATIVE.set(Some(native.present()));
        // A native layer that failed to answer counts as paired: signing in
        // then says for sure ([`attempt`]), and nothing pairs a second time.
        let native_paired = native.present().then(|| native.maybe_paired());
        let browser = if native_paired == Some(true) {
            None
        } else {
            device::load().await.unwrap_or_else(|error| {
                leptos::logging::warn!("{error}");
                None
            })
        };
        if let Some(device) = choose(native_paired, browser) {
            DEVICE.set(Some(device));
            set_credential(Credential::Device { name: None });
            renew(app).await;
        }
    }
    if dropped && current_device().is_none() {
        spawn_local(push::module::turn_off(None));
    }
    app.token_loaded.set(true);
}

/// The stored pasted token, and whether one that no longer validates was
/// found (and deleted).
async fn stored_token(store: &Store) -> (Option<BearerToken>, bool) {
    let Some(raw) = store.get(&TokenKey).await.ok().flatten() else {
        return (None, false);
    };
    if let Ok(token) = BearerToken::new(raw) {
        return (Some(token), false);
    }
    let _ = store.delete(&TokenKey).await;
    (None, true)
}

/// The stored pasted token, if there is one that still validates. Shared
/// with the service worker's push wake.
pub async fn pasted_token(store: &Store) -> Option<BearerToken> {
    let raw = store.get(&TokenKey).await.ok().flatten()?;
    BearerToken::new(raw).ok()
}

/// What one sign-in attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    Done,
    /// Try again soon: the bridge could not be reached.
    Retry,
    /// Try again, waiting longer each time: the native identity failed in
    /// a way that may pass.
    Later,
    Stop,
}

impl Attempt {
    /// How long to wait before the next attempt, given the wait before
    /// this one (0 for the first); `None` to stop.
    fn wait_after(self, last_ms: i32) -> Option<i32> {
        match self {
            Self::Retry => Some(RETRY_MS),
            Self::Later => Some(last_ms.saturating_mul(2).clamp(RETRY_MS, MAX_BACKOFF_MS)),
            Self::Done | Self::Stop => None,
        }
    }
}

/// Signs in with the device key, retrying while the bridge is unreachable
/// (and, ever more slowly, while the native identity fails), and
/// schedules the next renewal. Starts a new renewal generation, which
/// cancels any older scheduled one. The first attempt is awaited; retries
/// continue in the background.
async fn renew(app: AppState) {
    if RENEWING.replace(true) {
        return;
    }
    let generation = GENERATION.get().wrapping_add(1);
    GENERATION.set(generation);
    let first = attempt(app, generation).await;
    RENEWING.set(false);
    if let Some(wait_ms) = first.wait_after(0) {
        spawn_local(retry(app, generation, wait_ms));
    }
}

async fn retry(app: AppState, generation: u32, mut wait_ms: i32) {
    loop {
        sleep(wait_ms).await;
        if GENERATION.get() != generation || RENEWING.replace(true) {
            return;
        }
        let outcome = attempt(app, generation).await;
        RENEWING.set(false);
        match outcome.wait_after(wait_ms) {
            Some(next) => wait_ms = next,
            None => return,
        }
    }
}

async fn attempt(app: AppState, generation: u32) -> Attempt {
    let Some(device) = current_device() else {
        return Attempt::Stop;
    };
    let result = device.sign_in().await.and_then(|grant| {
        Ok((
            grant_token(&grant)?,
            grant.expires_in_secs,
            grant.device_name,
        ))
    });
    if GENERATION.get() != generation {
        // Forgotten or re-paired meanwhile: this result is stale.
        return Attempt::Stop;
    }
    match result {
        Ok((token, expires_in_secs, name)) => {
            app.token_rejected.set(false);
            app.token.set(Some(token));
            set_credential(Credential::Device { name });
            schedule(app, generation, expires_in_secs);
            Attempt::Done
        }
        Err(error) if error.is_retryable() && !native_unavailable(&error) => Attempt::Retry,
        Err(DeviceAuthError::Refused) => {
            // Refused even with a fresh challenge, yet the bridge knows the
            // device: keep the key and keep trying (rate limits pace it).
            leptos::logging::warn!("signing in was refused; retrying");
            Attempt::Retry
        }
        Err(DeviceAuthError::Forgotten) => {
            // The bridge no longer knows this device: it was revoked, and
            // its push subscriptions with it.
            let _ = forget_device().await;
            spawn_local(push::module::turn_off(None));
            app.token.set(None);
            app.token_rejected.set(true);
            set_credential(Credential::None);
            Attempt::Stop
        }
        Err(error) => {
            leptos::logging::warn!("signing in failed: {error}");
            let lost = matches!(device, Identity::Native) && !native::probe().await.maybe_paired();
            if lost {
                if GENERATION.get() == generation {
                    // The native layer lost its pairing (its key is gone,
                    // or it cannot read the bridge's address): the app is
                    // unpaired, but nothing more is deleted.
                    DEVICE.set(None);
                    spawn_local(push::module::turn_off(app.token.get_untracked()));
                    app.token.set(None);
                    app.token_rejected.set(true);
                    set_credential(Credential::None);
                }
                return Attempt::Stop;
            }
            // Still paired natively: a busy Keystore or a plugin failure
            // may pass, so try again, ever more slowly.
            if native_unavailable(&error) {
                Attempt::Later
            } else {
                Attempt::Stop
            }
        }
    }
}

/// Whether `error` is the native identity failing in a way that may pass,
/// retried with backoff ([`Attempt::Later`]) while the native layer still
/// says it is paired.
fn native_unavailable(error: &DeviceAuthError) -> bool {
    matches!(error, DeviceAuthError::NativeUnavailable(_))
}

/// Renews shortly before a token that lasts `expires_in_secs` expires.
fn schedule(app: AppState, generation: u32, expires_in_secs: u64) {
    let wait_secs = expires_in_secs
        .saturating_sub(RENEW_EARLY_SECS)
        .max(MIN_RENEW_SECS);
    let wait_ms = i32::try_from(wait_secs * 1000).unwrap_or(i32::MAX);
    spawn_local(async move {
        sleep(wait_ms).await;
        if GENERATION.get() == generation {
            renew(app).await;
        }
    });
}

/// Stores the device key pairing just made and signs in with it (the
/// pair WASM makes and registers it: [`crate::pair::screen`]). A pasted
/// token is dropped, since the device session replaces it. Nothing changes
/// if the key can't be stored.
pub async fn adopt_device(app: AppState, key: DeviceKey) -> Result<(), String> {
    retire_previous(app);
    device::save(&key).await.map_err(|e| e.to_string())?;
    install(app, Identity::Browser(key)).await
}

/// Signs in with the Android app's native key, which the pair WASM has just
/// paired ([`crate::pair::screen`]). A browser key left from before is
/// deleted: the native key replaces it.
pub async fn adopt_native(app: AppState) -> Result<(), String> {
    retire_previous(app);
    if let Err(error) = device::forget().await {
        leptos::logging::warn!("{error}");
    }
    install(app, Identity::Native).await
}

/// The new identity starts with push off: the old one's subscription (a
/// pasted token, or an earlier pairing) goes, while its token still works.
fn retire_previous(app: AppState) {
    if let Some(previous) = app.token.get_untracked() {
        spawn_local(push::module::turn_off(Some(previous)));
    }
}

/// Makes `device` this device's identity and signs in with it.
async fn install(app: AppState, device: Device) -> Result<(), String> {
    DEVICE.set(Some(device));
    set_credential(Credential::Device { name: None });
    if let Ok(store) = app.store().await {
        let _ = store.delete(&TokenKey).await;
    }
    app.token.set(None);
    renew(app).await;
    if app.token.get_untracked().is_none() {
        return Err(
            "Paired, but signing in failed. Check the connection; the app keeps trying.".to_owned(),
        );
    }
    request_persistent_storage(app).await;
    Ok(())
}

/// Asks for persistent storage, now that something worth keeping lives on
/// the device, and records the answer.
pub async fn request_persistent_storage(app: AppState) {
    let persistent = platform::request_persistent_storage().await;
    let _ = app.persistent_storage.try_set(Some(persistent));
}

/// Stores a pasted `token` and makes it current.
pub async fn save(app: AppState, token: BearerToken) -> Result<(), String> {
    let store = app.store().await.map_err(|e| e.to_string())?;
    store
        .put(&TokenKey, &token.expose().to_owned())
        .await
        .map_err(|e| e.to_string())?;
    app.token_rejected.set(false);
    app.token.set(Some(token));
    if current_device().is_none() {
        set_credential(Credential::StaticToken);
    }
    Ok(())
}

/// Deletes the device identity (native and browser) and cancels renewals.
async fn forget_device() -> Result<(), String> {
    GENERATION.set(GENERATION.get().wrapping_add(1));
    let device = DEVICE.take();
    // Both are tried, so one failing never leaves the other behind. The
    // native one also when the identity in use is a browser key: `choose`
    // picks that only while the native side is unpaired, so this is a
    // no-op there, and it keeps Forget complete.
    let native = if NATIVE.get() == Some(true) || matches!(device, Some(Identity::Native)) {
        native::forget().await.map_err(|e| e.to_string())
    } else {
        Ok(())
    };
    let browser = if device.is_some() || secure_context() {
        device::forget().await.map_err(|e| e.to_string())
    } else {
        Ok(())
    };
    native.and(browser)
}

/// Forgets every credential (device key and pasted token), here and on disk.
/// Push is then turned off in the background with the token as it was,
/// which the bridge still accepts, so a slow bridge never holds up Forget.
/// [`credential`] follows what is still stored: if a delete fails, it keeps
/// naming the credential that is left.
pub async fn forget(app: AppState) -> Result<(), String> {
    let token = app.token.get_untracked();
    app.token.set(None);
    spawn_local(push::module::turn_off(token));
    forget_device().await?;
    // The key is gone; whatever remains is at most the pasted token.
    let store = match app.store().await {
        Ok(store) => store,
        Err(error) => {
            set_credential(Credential::None);
            return Err(error.to_string());
        }
    };
    let deleted = store.delete(&TokenKey).await.map_err(|e| e.to_string());
    let left = match deleted {
        Ok(()) => None,
        Err(_) => pasted_token(&store).await,
    };
    set_credential(if left.is_some() {
        Credential::StaticToken
    } else {
        Credential::None
    });
    deleted?;
    // The cached agent list belongs to that bridge.
    store.clear_snapshots().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_identity_wins_once_paired() {
        assert!(matches!(
            choose(Some(true), Some("browser key")),
            Some(Identity::Native)
        ));
        assert!(matches!(
            choose::<()>(Some(true), None),
            Some(Identity::Native)
        ));
    }

    #[test]
    fn a_browser_key_is_used_without_a_native_pairing() {
        // Chrome, or an app whose WebView was too old for the native layer.
        assert!(matches!(
            choose(None, Some("key")),
            Some(Identity::Browser("key"))
        ));
        // A key paired in the app before it had a native identity.
        assert!(matches!(
            choose(Some(false), Some("key")),
            Some(Identity::Browser("key"))
        ));
    }

    #[test]
    fn an_unreachable_bridge_is_retried_at_a_steady_pace() {
        assert_eq!(Attempt::Retry.wait_after(0), Some(RETRY_MS));
        assert_eq!(Attempt::Retry.wait_after(MAX_BACKOFF_MS), Some(RETRY_MS));
    }

    #[test]
    fn a_failing_native_identity_is_retried_ever_more_slowly() {
        let mut waits = Vec::new();
        let mut last = 0;
        for _ in 0..8 {
            last = Attempt::Later.wait_after(last).expect("keeps trying");
            waits.push(last);
        }
        assert_eq!(
            waits,
            [
                5_000, 10_000, 20_000, 40_000, 80_000, 160_000, 300_000, 300_000
            ]
        );
        // Back to the steady pace once the bridge is what fails.
        assert_eq!(Attempt::Retry.wait_after(last), Some(RETRY_MS));
    }

    #[test]
    fn done_and_stop_end_the_retries() {
        assert_eq!(Attempt::Done.wait_after(RETRY_MS), None);
        assert_eq!(Attempt::Stop.wait_after(RETRY_MS), None);
    }

    #[test]
    fn in_the_app_push_says_whether_unified_push_can_work() {
        let browser = Identity::Browser("key");
        assert_eq!(
            push_in_app(Some(true), Some(&Identity::<&str>::Native)),
            Unavailable::NativeApp
        );
        // Not known yet, or nothing paired: the app's own pairing will be native.
        assert_eq!(push_in_app::<()>(None, None), Unavailable::NativeApp);
        assert_eq!(push_in_app::<()>(Some(true), None), Unavailable::NativeApp);
        // A WebView too old for the native layer: no UnifiedPush at all.
        assert_eq!(
            push_in_app(Some(false), Some(&browser)),
            Unavailable::NativeOff
        );
        assert_eq!(push_in_app::<()>(Some(false), None), Unavailable::NativeOff);
        // A browser key from before the native layer could be used.
        assert_eq!(
            push_in_app(Some(true), Some(&browser)),
            Unavailable::PairInApp
        );
    }

    #[test]
    fn nothing_paired_is_nothing() {
        assert!(choose::<()>(None, None).is_none());
        assert!(choose::<()>(Some(false), None).is_none());
    }
}
