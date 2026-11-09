//! The push wake, run in the service worker on every ping.

use core::cell::RefCell;

use protocol::DeviceId;

use super::serial::{Serial, one_at_a_time};
use super::{Notice, TRANSPORT};
use crate::api::auth::{DeviceSigner, grant_token, sign_in};
use crate::api::{ApiClient, ApiError, BearerToken, MemoryToken};
use crate::pair::device;
use crate::session;
use crate::store::Store;

/// Stop reusing a cached session when the app would renew it.
#[expect(clippy::cast_precision_loss, reason = "a small constant")]
const RENEW_EARLY_MS: f64 = (session::RENEW_EARLY_SECS * 1000) as f64;

thread_local! {
    /// The worker's own device session, reused across pings while it lasts.
    static SESSION: RefCell<Session> = const { RefCell::new(Session::None) };
    /// One token lookup at a time, so pings arriving together share one
    /// sign-in instead of each signing in. Its requests time out
    /// ([`super::client`]'s transport), so a hung sign-in holds it briefly.
    static SIGN_IN: Serial = const { Serial::new() };
}

/// A device session the worker signed in for, kept so that a burst of
/// pings doesn't sign in (and mint a session) for each one.
#[derive(Debug, Clone, PartialEq)]
enum Session {
    /// None yet, or dropped.
    None,
    /// A token for `device`, usable until `renew_at_ms` (device clock,
    /// `Date.now()`).
    Held {
        device: DeviceId,
        token: BearerToken,
        renew_at_ms: f64,
    },
}

impl Session {
    /// Holds `token` for `device`, which expires in `expires_in_secs` from
    /// `now_ms`.
    fn held(device: DeviceId, token: BearerToken, expires_in_secs: u64, now_ms: f64) -> Self {
        let lifetime_ms = f64::from(u32::try_from(expires_in_secs).unwrap_or(u32::MAX)) * 1000.0;
        Self::Held {
            device,
            token,
            renew_at_ms: now_ms + lifetime_ms - RENEW_EARLY_MS,
        }
    }

    /// The token, if it belongs to `device` (the key this device holds
    /// now: a forgotten or re-paired device's session is never reused) and
    /// is still good at `now_ms`.
    fn token(&self, device: &DeviceId, now_ms: f64) -> Option<BearerToken> {
        match self {
            Self::Held {
                device: held,
                token,
                renew_at_ms,
            } if held == device && now_ms < *renew_at_ms => Some(token.clone()),
            _ => None,
        }
    }
}

/// The push wake: gets a token the way the app does (signs in with the
/// device key, or reads the pasted token), fetches the agents and picks
/// the notification. Any failure (offline, unpaired, refused) gives
/// [`Notice::generic`]; it never fails, since every push must show a
/// notification. `sw.js` bounds how long it may take.
pub async fn wake() -> Notice {
    let Some(token) = wake_token().await else {
        return Notice::generic();
    };
    let api = ApiClient::new(TRANSPORT, MemoryToken::new(token));
    match api.agents().await {
        Ok(agents) => Notice::summarise(&agents),
        Err(error) => {
            if error == ApiError::Unauthorized {
                // Expired early (a bridge restart): sign in on the next ping.
                SESSION.set(Session::None);
            }
            Notice::generic()
        }
    }
}

/// A token for the wake: this worker's device session, signing in when it
/// has none or it is about to expire (the app keeps its own session in
/// memory only), otherwise the pasted token from the MVP fallback.
async fn wake_token() -> Option<BearerToken> {
    one_at_a_time(&SIGN_IN, find_token()).await
}

/// The device key is read first on every ping (a local `IndexedDB` read):
/// the cached session is only good while that key is still the one it
/// signed in with.
async fn find_token() -> Option<BearerToken> {
    if let Ok(Some(key)) = device::load().await {
        let device = key.device_id().clone();
        if let Some(token) =
            SESSION.with_borrow(|session| session.token(&device, js_sys::Date::now()))
        {
            return Some(token);
        }
        let grant = sign_in(&TRANSPORT, &key).await.ok()?;
        let token = grant_token(&grant).ok()?;
        SESSION.set(Session::held(
            device,
            token.clone(),
            grant.expires_in_secs,
            js_sys::Date::now(),
        ));
        return Some(token);
    }
    // Forgotten: its session goes too.
    SESSION.set(Session::None);
    let store = Store::open().await.ok()?;
    let token = session::pasted_token(&store).await;
    store.close();
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> BearerToken {
        BearerToken::new("t").unwrap()
    }

    fn device(id: &str) -> DeviceId {
        DeviceId::new(id).unwrap()
    }

    #[test]
    fn a_session_is_reused_until_shortly_before_it_expires() {
        let a = device("dev-000000000000000a");
        let session = Session::held(a.clone(), token(), 900, 1_000_000.0);
        assert_eq!(session.token(&a, 1_000_000.0), Some(token()));
        assert_eq!(session.token(&a, 1_000_000.0 + 839_999.0), Some(token()));
        assert_eq!(session.token(&a, 1_000_000.0 + 840_000.0), None);
        assert_eq!(Session::None.token(&a, 0.0), None);
    }

    #[test]
    fn a_very_short_session_is_not_reused() {
        let a = device("dev-000000000000000a");
        let session = Session::held(a.clone(), token(), 30, 5_000.0);
        assert_eq!(session.token(&a, 5_000.0), None);
    }

    #[test]
    fn a_re_paired_device_does_not_reuse_the_old_session() {
        let session = Session::held(device("dev-000000000000000a"), token(), 900, 0.0);
        assert_eq!(session.token(&device("dev-000000000000000b"), 1.0), None);
    }
}
