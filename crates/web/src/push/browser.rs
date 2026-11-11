//! The browser side of the toggle: what the browser allows, turning push on
//! and off against the Push API and the bridge, and dropping it when the
//! credentials go.
//!
//! Runs in the page. [`availability`] and [`PermissionPrompt`] run in the
//! app itself; the rest runs in the notice WASM, which the page loads on
//! demand ([`super::module`]), so it stays out of the app's size budget.
//! The subscription lives in the service worker's registration, so it
//! outlives the page and wakes the worker on a ping.

use js_sys::{JSON, Promise};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DomException, Notification, NotificationPermission, PushManager, PushSubscription,
    PushSubscriptionOptionsInit, ServiceWorkerRegistration,
};

use super::serial::{Serial, across_tabs, both};
use super::state::{Outcome, Probe, Unavailable};
use crate::api::{ApiClient, ApiError, TokenSource, Transport};
use crate::pair::native;
use crate::platform::{self, has, js_error_message, log_error};

/// Length of a VAPID public key (65 bytes) in unpadded base64url.
const VAPID_KEY_CHARS: usize = 87;
/// The Web Lock that orders every change to the subscription, in every tab.
const LOCK: &str = "passporter-push";

thread_local! {
    /// The bridge's VAPID key, fetched by [`probe`] for [`enable`] and kept
    /// for every later attempt in this page.
    static KEY: core::cell::RefCell<Option<String>> = const { core::cell::RefCell::new(None) };
    /// [`LOCK`] where the Web Locks API is missing: this page only.
    static IN_PAGE: Serial = const { Serial::new() };
}

/// Runs `work` holding [`LOCK`]. Every whole "on", "off" and probe check
/// takes it, so none interleaves with another in any tab: a probe can't
/// let go of a subscription another tab is still registering, and a
/// background "off" (Forget) can't undo a quick re-pair's "on". Their
/// bridge requests time out ([`super::client`]), so a hung tunnel holds
/// it only briefly.
async fn exclusively<T>(work: impl Future<Output = T>) -> T {
    across_tabs(LOCK, &IN_PAGE, work).await
}

/// Why turning push on or off failed. The message is shown as a toast.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PushError {
    /// No service worker is registered yet (the first seconds of the
    /// first visit, or registration failed).
    #[error("The app is still installing. Try again in a moment.")]
    NoWorker,
    /// A browser API threw.
    #[error("{0}")]
    Browser(String),
    /// `GET /v1/push/key` returned something that isn't a P-256 key.
    #[error("The bridge sent an unusable push key.")]
    BadKey,
    /// A bridge call failed.
    #[error("{}", bridge_message(.0))]
    Bridge(ApiError),
    /// The browser holds a subscription made with another VAPID key, so
    /// `subscribe` refused (`InvalidStateError`).
    #[error("This browser is subscribed with another push key.")]
    OtherKey,
    /// The bridge's VAPID key changed while turning push on; the new one is
    /// kept, so trying again works.
    #[error("The bridge's push key just changed. Try again.")]
    KeyChanged,
}

impl PushError {
    /// Whether the bridge refused the token.
    #[must_use]
    pub const fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Bridge(ApiError::Unauthorized))
    }
}

impl From<ApiError> for PushError {
    fn from(error: ApiError) -> Self {
        Self::Bridge(error)
    }
}

/// The toast for a failed push call to the bridge.
fn bridge_message(error: &ApiError) -> String {
    match error {
        ApiError::Status { status: 404 } => "Push is turned off on the bridge.".to_owned(),
        ApiError::Status { status: 400 } => {
            "The bridge doesn't accept this browser's push service.".to_owned()
        }
        ApiError::Status { status: 409 } => {
            "This phone has too many push subscriptions on the bridge.".to_owned()
        }
        other => other.to_string(),
    }
}

fn browser(error: &JsValue) -> PushError {
    PushError::Browser(js_error_message(error))
}

/// Whether `text` looks like a VAPID public key: 65 bytes of base64url
/// (padding allowed) starting with `0x04`, an uncompressed point, which
/// always encodes to a leading `B`. The browser checks the point itself;
/// this only refuses an obviously wrong answer with a clear message.
fn is_vapid_key(text: &str) -> bool {
    let text = text.trim_end_matches('=');
    text.len() == VAPID_KEY_CHARS
        && text.starts_with('B')
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Why push can't work in this page, if it can't: checked in the app
/// before the notice WASM is loaded. In the Android app it never does: the
/// app has its own (`UnifiedPush`) subscription, so the page must not add a
/// second one.
#[must_use]
pub fn availability() -> Option<Unavailable> {
    if native::in_app() {
        return Some(Unavailable::NativeApp);
    }
    let Some(window) = web_sys::window() else {
        return Some(Unavailable::Unsupported);
    };
    if !window.is_secure_context() {
        return Some(Unavailable::Insecure);
    }
    if !has(&window, "PushManager")
        || !has(&window, "Notification")
        || platform::service_workers().is_none()
    {
        return Some(Unavailable::Unsupported);
    }
    (Notification::permission() == NotificationPermission::Denied).then_some(Unavailable::Denied)
}

/// Whether push is on, where [`availability`] allows it. A subscription the
/// browser holds counts only if the bridge has it too; one the bridge has
/// dropped is let go. When the bridge can't be reached (offline, a hung
/// tunnel), the browser is trusted; any other error is
/// [`Probe::Unknown`], or [`Probe::BridgeOff`] for a 404. `Err` only when
/// the bridge refused the token, which the caller passes on.
///
/// The bridge's VAPID key is fetched, unless already known, and kept for
/// [`enable`], so a tap goes from the permission prompt to `subscribe`
/// without waiting on the network (Safari wants both close to the user's
/// gesture). [`enable`] checks it again anyway.
pub async fn probe<T: Transport, A: TokenSource>(
    api: &ApiClient<T, A>,
) -> Result<Probe, PushError> {
    let known = KEY.with_borrow(Option::is_some);
    // Fetched alongside the subscription check.
    let key = async {
        if known {
            None
        } else {
            api.push_key().await.ok()
        }
    };
    let (key, probe) = both(key, exclusively(subscription_state(api))).await;
    if let Some(key) = key {
        KEY.set(Some(key.vapid_public_key));
    }
    probe
}

async fn subscription_state<T: Transport, A: TokenSource>(
    api: &ApiClient<T, A>,
) -> Result<Probe, PushError> {
    let subscription = match current_subscription().await {
        Ok(Some(subscription)) => subscription,
        Ok(None) | Err(PushError::NoWorker) => return Ok(Probe::NotSubscribed),
        Err(error) => {
            log_error("push probe failed", &JsValue::from_str(&error.to_string()));
            return Ok(Probe::Unknown);
        }
    };
    let endpoint = subscription.endpoint();
    let listed = api
        .push_endpoints()
        .await
        .map(|endpoints| endpoints.contains(&endpoint));
    let probe = probe_for(&listed)?;
    if probe.is_none() {
        // The bridge dropped it, most likely because the push service
        // said it is gone: registering it again would show "on" while
        // nothing arrives. Let it go; turning push on makes a new one.
        let _ = unsubscribe(&subscription).await;
    }
    Ok(probe.unwrap_or(Probe::NotSubscribed))
}

/// What the bridge's answer to "which subscriptions do you hold" means
/// for one the browser holds (`Ok(true)`: listed). `Ok(None)`: the bridge
/// doesn't have it, so it should be let go.
fn probe_for(listed: &Result<bool, ApiError>) -> Result<Option<Probe>, PushError> {
    Ok(Some(match listed {
        Ok(false) => return Ok(None),
        // Unreachable (offline, timed out): trust the browser.
        Ok(true) | Err(ApiError::Transport(_)) => Probe::Subscribed,
        Err(ApiError::Unauthorized) => return Err(PushError::Bridge(ApiError::Unauthorized)),
        Err(ApiError::Status { status: 404 }) => Probe::BridgeOff,
        Err(_) => Probe::Unknown,
    }))
}

/// The notification permission prompt, the first step of turning push on.
/// Start it in the tap's event handler itself: the prompt needs the user's
/// gesture, and iOS Safari refuses it once the handler has returned.
#[derive(Debug)]
pub struct PermissionPrompt(Option<Promise>);

impl PermissionPrompt {
    /// Opens the prompt, unless permission is already decided.
    #[must_use]
    pub fn start() -> Self {
        Self(
            (Notification::permission() == NotificationPermission::Default)
                .then(|| Notification::request_permission().ok())
                .flatten(),
        )
    }

    /// Waits for the answer: `None` when granted, otherwise how turning
    /// push on ends.
    pub async fn refused(self) -> Result<Option<Outcome>, PushError> {
        let permission = match self.0 {
            Some(prompt) => {
                let answer = JsFuture::from(prompt).await.map_err(|e| browser(&e))?;
                NotificationPermission::from_js_value(&answer)
                    .unwrap_or(NotificationPermission::Default)
            }
            None => Notification::permission(),
        };
        Ok(match permission {
            NotificationPermission::Granted => None,
            NotificationPermission::Denied => Some(Outcome::Denied),
            _ => Some(Outcome::Dismissed),
        })
    }
}

/// Turns push on once permission is granted ([`PermissionPrompt`]):
/// subscribes with the bridge's VAPID key (the one [`probe`] fetched, if
/// any) and registers the subscription. An `Err` leaves the browser's
/// subscription as it was, except one bound to a key the bridge no longer
/// uses, which is replaced. If the bridge's key changed since it was
/// fetched, the new subscription is dropped and [`PushError::KeyChanged`]
/// returned; the new key is kept for the next try.
pub async fn enable<T: Transport, A: TokenSource>(api: &ApiClient<T, A>) -> Result<(), PushError> {
    exclusively(enable_now(api)).await
}

async fn enable_now<T: Transport, A: TokenSource>(api: &ApiClient<T, A>) -> Result<(), PushError> {
    let key = match KEY.with_borrow(Clone::clone) {
        Some(key) => key,
        None => api.push_key().await?.vapid_public_key,
    };
    if !is_vapid_key(&key) {
        return Err(PushError::BadKey);
    }

    let (subscribed, replaced) = subscribe_with(&key).await;
    if let Some(old) = replaced
        && let Err(error) = api.unregister_push(&old).await
    {
        log_error(
            "could not remove the old push subscription",
            &JsValue::from_str(&error.to_string()),
        );
    }
    let (subscription, created) = subscribed?;
    // The key may have changed since Settings fetched it: a subscription
    // made with the old one would never be delivered to. Checked alongside
    // registering.
    let (registered, current) = both(register(api, &subscription), api.push_key()).await;
    let rotated = match current {
        Ok(current) if current.vapid_public_key != key => {
            KEY.set(Some(current.vapid_public_key));
            true
        }
        _ => false,
    };
    if registered.is_err() || rotated {
        // Don't leave a subscription the bridge can't use.
        if created || rotated {
            let _ = unsubscribe(&subscription).await;
        }
        if rotated && registered.is_ok() {
            let _ = api.unregister_push(&subscription.endpoint()).await;
        }
        registered?;
        return Err(PushError::KeyChanged);
    }
    Ok(())
}

/// Subscribes with `key`: the subscription and whether it is new, plus the
/// endpoint of a subscription dropped because it was bound to another key
/// (which can never be delivered to, so it goes even if subscribing again
/// then fails; the caller removes it from the bridge). With the same key,
/// `subscribe` hands back the existing subscription; any error other than
/// "another key" (an abort, a lost connection to the push service) leaves
/// the existing one alone.
async fn subscribe_with(
    key: &str,
) -> (Result<(PushSubscription, bool), PushError>, Option<String>) {
    let lookup = async {
        let manager = push_manager().await?;
        let existing = subscription_of(&manager).await?;
        Ok::<_, PushError>((manager, existing))
    };
    let (manager, existing) = match lookup.await {
        Ok(found) => found,
        Err(error) => return (Err(error), None),
    };
    match (subscribe(&manager, key).await, existing) {
        (Ok(subscription), existing) => {
            let created = existing.is_none_or(|old| old.endpoint() != subscription.endpoint());
            (Ok((subscription, created)), None)
        }
        (Err(PushError::OtherKey), Some(old)) => {
            if let Err(error) = unsubscribe(&old).await {
                return (Err(error), None);
            }
            let renewed = subscribe(&manager, key).await.map(|s| (s, true));
            (renewed, Some(old.endpoint()))
        }
        (Err(error), _) => (Err(error), None),
    }
}

/// Unsubscribes, then tells the bridge. Once the browser has let go no
/// ping can arrive, so a failed delete (offline, or no working token when
/// the device was revoked or its token refused) is only logged: the push
/// service answers the bridge's next send with "gone", and the bridge
/// drops it. `Ok(true)` when the bridge refused the token, which the
/// caller passes on.
pub async fn disable<T: Transport, A: TokenSource>(
    api: &ApiClient<T, A>,
) -> Result<bool, PushError> {
    exclusively(disable_now(api)).await
}

async fn disable_now<T: Transport, A: TokenSource>(
    api: &ApiClient<T, A>,
) -> Result<bool, PushError> {
    let Some(endpoint) = unsubscribe_current().await? else {
        return Ok(false);
    };
    match api.unregister_push(&endpoint).await {
        Ok(()) => Ok(false),
        Err(ApiError::Unauthorized) => Ok(true),
        Err(error) => {
            log_error(
                "could not remove the push subscription from the bridge",
                &JsValue::from_str(&error.to_string()),
            );
            Ok(false)
        }
    }
}

/// Unsubscribes the browser's subscription, if any: its endpoint.
async fn unsubscribe_current() -> Result<Option<String>, PushError> {
    let Some(subscription) = current_subscription().await? else {
        return Ok(None);
    };
    unsubscribe(&subscription).await?;
    Ok(Some(subscription.endpoint()))
}

/// Registers `subscription` with the bridge.
async fn register<T: Transport, A: TokenSource>(
    api: &ApiClient<T, A>,
    subscription: &PushSubscription,
) -> Result<(), PushError> {
    // `JSON.stringify` uses `toJSON()`: `{endpoint, expirationTime, keys}`.
    let json = JSON::stringify(subscription)
        .ok()
        .and_then(|s| s.as_string())
        .ok_or_else(|| PushError::Browser("could not read the subscription".into()))?;
    Ok(api.register_push(json).await?)
}

async fn subscribe(manager: &PushManager, key: &str) -> Result<PushSubscription, PushError> {
    let options = PushSubscriptionOptionsInit::new();
    options.set_user_visible_only(true);
    // The Push API takes the base64url text as is.
    options.set_application_server_key_opt_str(Some(key));
    let promise = manager
        .subscribe_with_options(&options)
        .map_err(|e| browser(&e))?;
    match JsFuture::from(promise).await {
        Ok(subscription) => Ok(subscription.unchecked_into()),
        // Permission was revoked between the check and now.
        Err(error)
            if error
                .dyn_ref::<DomException>()
                .is_some_and(|e| e.name() == "NotAllowedError") =>
        {
            Err(PushError::Browser(
                "Notifications are blocked for this site.".into(),
            ))
        }
        Err(error)
            if error
                .dyn_ref::<DomException>()
                .is_some_and(|e| e.name() == "InvalidStateError") =>
        {
            Err(PushError::OtherKey)
        }
        Err(error) => Err(browser(&error)),
    }
}

async fn push_manager() -> Result<PushManager, PushError> {
    let container = platform::service_workers().ok_or(PushError::NoWorker)?;
    let registration = JsFuture::from(container.get_registration())
        .await
        .map_err(|e| browser(&e))?;
    let registration: ServiceWorkerRegistration =
        registration.dyn_into().map_err(|_| PushError::NoWorker)?;
    // Still installing (first visit): `subscribe` would throw
    // `InvalidStateError`, which must keep meaning "another key".
    if registration.active().is_none() {
        return Err(PushError::NoWorker);
    }
    registration.push_manager().map_err(|e| browser(&e))
}

async fn current_subscription() -> Result<Option<PushSubscription>, PushError> {
    subscription_of(&push_manager().await?).await
}

async fn subscription_of(manager: &PushManager) -> Result<Option<PushSubscription>, PushError> {
    let found = JsFuture::from(manager.get_subscription().map_err(|e| browser(&e))?)
        .await
        .map_err(|e| browser(&e))?;
    Ok(found.dyn_into::<PushSubscription>().ok())
}

async fn unsubscribe(subscription: &PushSubscription) -> Result<(), PushError> {
    JsFuture::from(subscription.unsubscribe().map_err(|e| browser(&e))?)
        .await
        .map(drop)
        .map_err(|e| browser(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_failures_read_as_sentences() {
        assert_eq!(
            PushError::from(ApiError::Status { status: 404 }).to_string(),
            "Push is turned off on the bridge."
        );
        assert!(
            PushError::from(ApiError::Status { status: 409 })
                .to_string()
                .contains("too many")
        );
        assert_eq!(
            PushError::from(ApiError::Unauthorized).to_string(),
            ApiError::Unauthorized.to_string()
        );
    }

    #[test]
    fn only_an_unreachable_bridge_falls_back_to_the_browser() {
        assert_eq!(probe_for(&Ok(true)), Ok(Some(Probe::Subscribed)));
        assert_eq!(probe_for(&Ok(false)), Ok(None));
        assert_eq!(
            probe_for(&Err(ApiError::Transport("timed out".into()))),
            Ok(Some(Probe::Subscribed))
        );
        assert_eq!(
            probe_for(&Err(ApiError::Status { status: 404 })),
            Ok(Some(Probe::BridgeOff))
        );
        assert_eq!(
            probe_for(&Err(ApiError::Status { status: 503 })),
            Ok(Some(Probe::Unknown))
        );
        assert_eq!(
            probe_for(&Err(ApiError::Decode("garbage".into()))),
            Ok(Some(Probe::Unknown))
        );
        assert!(
            probe_for(&Err(ApiError::Unauthorized))
                .unwrap_err()
                .is_unauthorized()
        );
    }

    #[test]
    fn vapid_keys_are_65_bytes_of_base64url() {
        let key = format!("B{}", "A".repeat(VAPID_KEY_CHARS - 1));
        assert!(is_vapid_key(&key));
        assert!(is_vapid_key(&format!("{key}=")));
        let with_url_chars = format!("B-_{}", "a".repeat(VAPID_KEY_CHARS - 3));
        assert!(is_vapid_key(&with_url_chars));

        assert!(!is_vapid_key(""));
        assert!(!is_vapid_key(&key[..VAPID_KEY_CHARS - 1]));
        assert!(!is_vapid_key(&format!("{key}A")));
        assert!(!is_vapid_key(&format!("A{}", &key[1..])), "not 0x04");
        assert!(!is_vapid_key(&format!("B+/{}", &key[3..])), "plain base64");
    }
}
