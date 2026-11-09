//! Device authentication (design §8): pairing, device keys, signed
//! challenges, short-lived sessions and revocation.
//!
//! - `passporter pair` asks the running bridge, over the [`admin`] socket,
//!   for a one-time pairing code (single use, [`AuthSettings::pairing_ttl`]).
//! - The phone registers a P-256 public key with the code
//!   (`POST /v1/auth/pair`), then trades a signature over a single-use
//!   challenge for a session token (`POST /v1/auth/challenge`, then
//!   `POST /v1/auth/session`; [`AuthSettings::session_ttl`]). Refreshing is
//!   signing a new challenge. The signature covers the device id and the
//!   challenge. Step-up sessions are refused until real user verification
//!   exists: a client's `stepUp` claim proves nothing.
//! - Each session has a [`Scope`] chosen when it is made: `app` (the
//!   default) or `car` for Android Auto. The [`Principal`] carries it; the
//!   server keeps car sessions to the car-safe routes and judges their
//!   actions as `origin: "car"` whatever the body claims.
//! - A session's open streams close when it expires; the client renews
//!   and reconnects.
//! - `passporter devices revoke` removes the device and its sessions, and
//!   closes its open streams through their [`Revocation`]. A device the
//!   bridge does not know gets `410 Gone`
//!   ([`protocol::auth::DEVICE_UNKNOWN_STATUS`]), the only refusal after
//!   which the phone forgets its key.
//! - The three endpoints are rate limited per client IP, and the pending
//!   codes burn after [`AuthSettings::max_failed_pairs`] wrong guesses.
//!
//! The static MVP token ([`StaticToken`]) is still accepted when the config
//! opts in, for clients without a secure context (`WebCrypto` needs one).
//!
//! Paired devices persist in the state dir ([`devices`]); everything else
//! is in memory. Pairing and revoking change memory first and then save in
//! turn, off the auth lock, so a slow disk never stalls authentication.
//! Saves run in their own task: a request that is dropped half-way (client
//! gone, timeout) still saves, and still undoes a pairing whose save failed.
//!
//! [`Revocation`]: crate::server::Revocation

pub mod admin;
pub mod devices;
mod rate;
mod routes;
mod secret;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use protocol::auth::{DEVICE_ID_HEX, DEVICE_ID_PREFIX};
use protocol::{
    Challenge, ChallengeRequest, ChallengeResponse, DeviceId, DeviceName, PairRequest,
    PairResponse, PairingCode, Scope, SessionGrant, SessionRequest, signed_message,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinError;
use tokio::time::Instant;

pub use self::devices::{DeviceRecord, StoreError};
use self::devices::{DeviceStore, verifying_key};
pub use self::rate::RateLimit;
pub(crate) use self::rate::RateLimiter;
pub use self::secret::RandomUnavailable;
use self::secret::{TokenDigest, digest, same};
use crate::server::{
    AuthFuture, AuthRejection, AuthRequest, Authenticator, Principal, Revocation, StaticToken,
    bearer, detached,
};

/// The name a device gets when neither it nor `passporter pair` gave one.
const DEFAULT_DEVICE_NAME: &str = "phone";

/// Revocations a slow listener may fall behind by before it misses some.
const REVOCATION_BACKLOG: usize = 64;

/// Lifetimes and limits.
#[derive(Debug, Clone)]
pub struct AuthSettings {
    /// How long a pairing code is valid.
    pub pairing_ttl: Duration,
    /// How long a challenge may wait for its signature.
    pub challenge_ttl: Duration,
    /// How long a session token is valid.
    pub session_ttl: Duration,
    /// `POST /v1/auth/pair`, per client.
    pub pair_rate: RateLimit,
    /// `POST /v1/auth/challenge` and `POST /v1/auth/session`, per client.
    pub session_rate: RateLimit,
    /// Wrong pairing codes tolerated before every pending code is burned.
    pub max_failed_pairs: u32,
    /// Paired devices at most.
    pub max_devices: usize,
    /// Pairing codes outstanding at most; a new one replaces the oldest.
    pub max_pending_codes: usize,
    /// Outstanding challenges at most, across devices.
    pub max_challenges: usize,
    /// Live sessions at most, across devices; the oldest is dropped.
    pub max_sessions: usize,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            pairing_ttl: Duration::from_mins(5),
            challenge_ttl: Duration::from_secs(60),
            session_ttl: Duration::from_mins(15),
            pair_rate: RateLimit::per_minute(5),
            session_rate: RateLimit::per_minute(30),
            max_failed_pairs: 10,
            max_devices: 16,
            max_pending_codes: 4,
            max_challenges: 256,
            max_sessions: 256,
        }
    }
}

/// Why device auth could not start or an admin request failed.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The device file.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The OS random source.
    #[error(transparent)]
    Random(#[from] RandomUnavailable),
    /// `devices revoke` named a device that is not paired.
    #[error("no paired device {0}")]
    UnknownDevice(DeviceId),
    /// The device is revoked, but the device file still lists it: it
    /// would be back after a restart, unless a later save succeeds first
    /// (revoking it again retries).
    #[error(
        "device {device} is revoked, but the device file could not be updated, so it would \
         return after a restart; revoke it again to retry: {source}"
    )]
    RevocationNotSaved {
        /// The revoked device.
        device: DeviceId,
        /// Why the save failed.
        source: StoreError,
    },
}

/// A pairing code just issued.
#[derive(Debug, Clone)]
pub struct IssuedCode {
    /// The code.
    pub code: PairingCode,
    /// How long it is valid.
    pub valid_for: Duration,
}

/// A paired device, for `passporter devices list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    /// The stored record.
    #[serde(flatten)]
    pub record: DeviceRecord,
    /// Live sessions.
    pub sessions: usize,
}

/// Why a `/v1/auth` request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refusal {
    /// Wrong, expired or used code, challenge or signature.
    #[error("unauthenticated")]
    Unauthenticated,
    /// The device is not paired (never was, or was revoked).
    #[error("unknown device")]
    UnknownDevice,
    /// A request the bridge will not serve (a step-up session).
    #[error("bad request")]
    BadRequest,
    /// Too many requests from this client.
    #[error("rate limited")]
    RateLimited(Duration),
    /// The device limit is reached.
    #[error("too many paired devices")]
    TooManyDevices,
    /// Storage or randomness failed.
    #[error("internal error")]
    Internal,
}

/// A pending pairing code.
#[derive(Debug)]
struct PendingCode {
    code: PairingCode,
    name: Option<DeviceName>,
    expires: Instant,
}

/// A paired device in memory.
#[derive(Debug)]
struct Paired {
    record: DeviceRecord,
    key: VerifyingKey,
    revoked: watch::Sender<bool>,
}

/// An issued challenge.
#[derive(Debug)]
struct Issued {
    device: DeviceId,
    expires: Instant,
}

/// A live session.
#[derive(Debug)]
struct Session {
    device: DeviceId,
    expires: Instant,
    /// What it may do, as asked for when it was made.
    scope: Scope,
}

#[derive(Debug, Default)]
struct State {
    devices: BTreeMap<DeviceId, Paired>,
    codes: Vec<PendingCode>,
    failed_pairs: u32,
    challenges: HashMap<Challenge, Issued>,
    sessions: HashMap<TokenDigest, Session>,
    /// Revoked devices the device file may still list, until a save has
    /// written them out; revoking one again retries the save.
    unsaved: BTreeSet<DeviceId>,
}

impl State {
    /// Drops expired codes, challenges and sessions.
    fn prune(&mut self, now: Instant) {
        self.codes.retain(|c| c.expires > now);
        if self.codes.is_empty() {
            self.failed_pairs = 0;
        }
        self.challenges.retain(|_, c| c.expires > now);
        self.sessions.retain(|_, s| s.expires > now);
    }

    fn records(&self) -> Vec<DeviceRecord> {
        self.devices.values().map(|d| d.record.clone()).collect()
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a save needs, detached from the request that asked for it.
#[derive(Debug, Clone)]
struct Saver {
    store: Arc<DeviceStore>,
    state: Arc<Mutex<State>>,
}

impl Saver {
    /// Saves the paired devices as they are when this save's turn comes.
    async fn save(&self) -> Result<(), StoreError> {
        let mut written = BTreeSet::new();
        let saved = self
            .store
            .save_in_turn(|| {
                let state = lock(&self.state);
                written.clone_from(&state.unsaved);
                state.records()
            })
            .await;
        match &saved {
            Ok(()) => lock(&self.state).unsaved.retain(|id| !written.contains(id)),
            Err(error) => tracing::error!(%error, "could not save the paired devices"),
        }
        saved
    }

    /// Runs `work` on a clone of this saver, [`detached`], so it finishes
    /// even if the caller stops waiting; `failed` answers if its task dies.
    async fn detached<T, F>(
        &self,
        work: impl FnOnce(Self) -> F,
        failed: impl FnOnce(JoinError) -> T,
    ) -> T
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        detached(work(self.clone())).await.unwrap_or_else(failed)
    }

    /// [`Self::save`], detached.
    async fn save_detached(&self) -> Result<(), StoreError> {
        self.detached(
            |saver| async move { saver.save().await },
            |error| Err(self.store.interrupted(&error)),
        )
        .await
    }

    /// Saves a device just paired in memory: `Ok` once it is on disk. If
    /// the save fails, the device is forgotten and saved away again. A
    /// device revoked meanwhile is [`Refusal::UnknownDevice`].
    async fn keep_paired(&self, device: &DeviceId) -> Result<(), Refusal> {
        if self.save().await.is_ok() {
            let present = lock(&self.state).devices.contains_key(device);
            return if present {
                Ok(())
            } else {
                Err(Refusal::UnknownDevice)
            };
        }
        self.unpair(device).await;
        Err(Refusal::Internal)
    }

    /// Forgets a device whose pairing did not complete, and saves without
    /// it (a save that ran meanwhile may have written it). Should that fail
    /// too, the stray entry is harmless (its key never got its id) and
    /// `devices revoke` removes it.
    async fn unpair(&self, device: &DeviceId) {
        lock(&self.state).devices.remove(device);
        if self.save().await.is_err() {
            tracing::warn!(device = %device, "an unpaired device may remain in the device file");
        }
    }
}

/// Undoes a pairing whose request is dropped before it answers (client
/// gone, timeout): the phone never learns the id, so the device must not
/// keep a slot.
#[derive(Debug)]
struct PendingPairing {
    saver: Saver,
    device: Option<DeviceId>,
}

impl PendingPairing {
    /// The pairing answered: keep the device.
    fn answered(mut self) {
        self.device = None;
    }
}

impl Drop for PendingPairing {
    fn drop(&mut self) {
        let Some(device) = self.device.take() else {
            return;
        };
        tracing::info!(device = %device, "pairing request dropped; unpairing");
        let saver = self.saver.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn(async move { saver.unpair(&device).await })),
            // Shutting down: memory goes anyway, and the stray entry is
            // harmless (see `Saver::unpair`).
            Err(_) => drop(lock(&saver.state).devices.remove(&device)),
        }
    }
}

/// The bridge's [`Authenticator`]: device sessions, plus the static token
/// when the config opts in. Serves the `/v1/auth` routes.
#[derive(Debug)]
pub struct DeviceAuth {
    settings: AuthSettings,
    /// The device file and the state it saves.
    saver: Saver,
    static_token: Option<StaticToken>,
    pair_limit: RateLimiter,
    session_limit: RateLimiter,
    /// Announces each revoked device id ([`Authenticator::revocations`]).
    revoked: broadcast::Sender<String>,
}

impl DeviceAuth {
    /// Device auth with its device file in `state_dir`. `static_token` is
    /// the opted-in MVP fallback, if any.
    pub fn open(
        state_dir: &Path,
        settings: AuthSettings,
        static_token: Option<StaticToken>,
    ) -> Result<Self, AuthError> {
        let store = DeviceStore::open(state_dir)?;
        let devices = store
            .load()?
            .into_iter()
            .filter_map(|record| {
                // `load` has checked every key.
                let key = record.verifying_key()?;
                Some((
                    record.id.clone(),
                    Paired {
                        record,
                        key,
                        revoked: watch::Sender::new(false),
                    },
                ))
            })
            .collect();
        Ok(Self {
            pair_limit: RateLimiter::new(settings.pair_rate),
            session_limit: RateLimiter::new(settings.session_rate),
            settings,
            saver: Saver {
                store: Arc::new(store),
                state: Arc::new(Mutex::new(State {
                    devices,
                    ..State::default()
                })),
            },
            static_token,
            revoked: broadcast::Sender::new(REVOCATION_BACKLOG),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.saver.state)
    }

    /// Issues a pairing code (`passporter pair`). `name` labels the device,
    /// whatever name the device reports for itself.
    pub fn start_pairing(&self, name: Option<DeviceName>) -> Result<IssuedCode, AuthError> {
        let code = secret::pairing_code()?;
        let now = Instant::now();
        let mut state = self.state();
        state.prune(now);
        while state.codes.len() >= self.settings.max_pending_codes.max(1) {
            state.codes.remove(0);
        }
        state.codes.push(PendingCode {
            code: code.clone(),
            name,
            expires: now + self.settings.pairing_ttl,
        });
        Ok(IssuedCode {
            code,
            valid_for: self.settings.pairing_ttl,
        })
    }

    /// Every paired device (`passporter devices list`).
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let mut state = self.state();
        state.prune(Instant::now());
        state
            .devices
            .values()
            .map(|d| DeviceInfo {
                record: d.record.clone(),
                sessions: state
                    .sessions
                    .values()
                    .filter(|s| s.device == d.record.id)
                    .count(),
            })
            .collect()
    }

    /// Revokes `id` (`passporter devices revoke`): forgets the device, ends
    /// its sessions and challenges, closes its open streams and announces
    /// it on [`Authenticator::revocations`] (push stops pinging it), all at
    /// once, then saves the device file. Returns how many sessions ended. A
    /// failed save does not undo the revocation: it is
    /// [`AuthError::RevocationNotSaved`], and revoking the device again
    /// retries the save.
    pub async fn revoke(&self, id: &DeviceId) -> Result<usize, AuthError> {
        let ended = {
            let mut state = self.state();
            match state.devices.remove(id) {
                Some(device) => {
                    device.revoked.send_replace(true);
                    // Nobody listening (push off) is fine.
                    let _ = self.revoked.send(id.to_string());
                    state.unsaved.insert(id.clone());
                    let before = state.sessions.len();
                    state.sessions.retain(|_, s| &s.device != id);
                    state.challenges.retain(|_, c| &c.device != id);
                    before - state.sessions.len()
                }
                // Revoked already, but not saved yet: save again.
                None if state.unsaved.contains(id) => 0,
                None => return Err(AuthError::UnknownDevice(id.clone())),
            }
        };
        tracing::info!(device = %id, sessions = ended, "device revoked");
        self.saver
            .save_detached()
            .await
            .map_err(|source| AuthError::RevocationNotSaved {
                device: id.clone(),
                source,
            })?;
        Ok(ended)
    }

    /// `POST /v1/auth/pair`. The device is saved before its id is
    /// returned; if that fails, it is forgotten again. Both run
    /// [`detached`], so they finish even if the request is dropped, and a
    /// dropped request unpairs the device (the phone never got its id). A
    /// device revoked while it was being saved is refused as unknown, so
    /// the phone drops its key.
    pub(crate) async fn pair(
        &self,
        client: std::net::IpAddr,
        request: PairRequest,
    ) -> Result<PairResponse, Refusal> {
        let id = self.pair_in_memory(client, request)?;
        let pending = PendingPairing {
            saver: self.saver.clone(),
            device: Some(id.clone()),
        };
        let device = id.clone();
        let outcome = self
            .saver
            .detached(
                |saver| async move { saver.keep_paired(&device).await },
                |error| {
                    tracing::error!(%error, device = %id, "pairing task failed");
                    self.state().devices.remove(&id);
                    Err(Refusal::Internal)
                },
            )
            .await;
        // Answered either way: a failed pairing has cleaned up already.
        pending.answered();
        outcome?;
        tracing::info!(device = %id, "device paired");
        Ok(PairResponse { device_id: id })
    }

    /// Checks a pairing request and adds the device, in memory only.
    fn pair_in_memory(
        &self,
        client: std::net::IpAddr,
        request: PairRequest,
    ) -> Result<DeviceId, Refusal> {
        self.pair_limit
            .check(&client)
            .map_err(Refusal::RateLimited)?;
        let key = verifying_key(&request.public_key).ok_or(Refusal::Unauthenticated)?;
        let now = Instant::now();
        let mut state = self.state();
        state.prune(now);
        let Some(index) = state
            .codes
            .iter()
            .position(|c| same(c.code.as_str().as_bytes(), request.code.as_str().as_bytes()))
        else {
            state.failed_pairs += 1;
            if state.failed_pairs >= self.settings.max_failed_pairs && !state.codes.is_empty() {
                tracing::warn!("too many wrong pairing codes; burning every pending code");
                state.codes.clear();
                state.failed_pairs = 0;
            }
            return Err(Refusal::Unauthenticated);
        };
        // Revoked devices not saved yet are still in the device file, and
        // would be back after a restart: they keep their slots until saved.
        if state.devices.len() + state.unsaved.len() >= self.settings.max_devices {
            return Err(Refusal::TooManyDevices);
        }
        // Single use: gone whatever happens next.
        let pending = state.codes.remove(index);
        let id = loop {
            let id = new_device_id().map_err(|_| Refusal::Internal)?;
            if !state.devices.contains_key(&id) {
                break id;
            }
        };
        // The operator's label wins over the device's own claim.
        let name = pending
            .name
            .or(request.name)
            .or_else(|| DeviceName::new(DEFAULT_DEVICE_NAME).ok())
            .ok_or(Refusal::Internal)?;
        let record = DeviceRecord {
            id: id.clone(),
            name,
            public_key: request.public_key,
            paired_at: crate::registry::now(),
        };
        state.devices.insert(
            id.clone(),
            Paired {
                record,
                key,
                revoked: watch::Sender::new(false),
            },
        );
        Ok(id)
    }

    /// `POST /v1/auth/challenge`.
    pub(crate) fn challenge(
        &self,
        client: std::net::IpAddr,
        request: &ChallengeRequest,
    ) -> Result<ChallengeResponse, Refusal> {
        self.session_limit
            .check(&client)
            .map_err(Refusal::RateLimited)?;
        let challenge = secret::challenge().map_err(|_| Refusal::Internal)?;
        let now = Instant::now();
        let mut state = self.state();
        state.prune(now);
        if !state.devices.contains_key(&request.device_id) {
            return Err(Refusal::UnknownDevice);
        }
        if state.challenges.len() >= self.settings.max_challenges {
            return Err(Refusal::RateLimited(self.settings.challenge_ttl));
        }
        state.challenges.insert(
            challenge.clone(),
            Issued {
                device: request.device_id.clone(),
                expires: now + self.settings.challenge_ttl,
            },
        );
        Ok(ChallengeResponse {
            challenge,
            expires_in_secs: self.settings.challenge_ttl.as_secs(),
        })
    }

    /// `POST /v1/auth/session`.
    pub(crate) fn session(
        &self,
        client: std::net::IpAddr,
        request: &SessionRequest,
    ) -> Result<SessionGrant, Refusal> {
        self.session_limit
            .check(&client)
            .map_err(Refusal::RateLimited)?;
        // Nothing proves the user verified on the device yet (`WebAuthn`
        // comes later), so a step-up session would rest on a bare claim.
        if request.step_up {
            return Err(Refusal::BadRequest);
        }
        let token = secret::random::<32>().map_err(|_| Refusal::Internal)?;
        let now = Instant::now();
        let mut state = self.state();
        state.prune(now);
        // Single use: consumed whether or not the signature verifies.
        let issued = state.challenges.remove(&request.challenge);
        let device = state
            .devices
            .get(&request.device_id)
            .ok_or(Refusal::UnknownDevice)?;
        if issued.is_none_or(|issued| issued.device != request.device_id) {
            return Err(Refusal::Unauthenticated);
        }
        let signature = secret::unhex(request.signature.as_str())
            .and_then(|bytes| Signature::from_slice(&bytes).ok())
            .ok_or(Refusal::Unauthenticated)?;
        device
            .key
            .verify(
                &signed_message(&request.device_id, &request.challenge),
                &signature,
            )
            .map_err(|_| Refusal::Unauthenticated)?;
        let device_name = device.record.name.clone();
        let ttl = self.settings.session_ttl;
        if state.sessions.len() >= self.settings.max_sessions
            && let Some(oldest) = state
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(k, _)| *k)
        {
            state.sessions.remove(&oldest);
        }
        let token = secret::hex(&token);
        state.sessions.insert(
            digest(token.as_bytes()),
            Session {
                device: request.device_id.clone(),
                expires: now + ttl,
                scope: request.scope,
            },
        );
        Ok(SessionGrant {
            token,
            expires_in_secs: ttl.as_secs(),
            device_name: Some(device_name),
            scope: request.scope,
        })
    }

    /// The principal for a live session token.
    fn session_principal(&self, token: &[u8]) -> Option<Principal> {
        let now = Instant::now();
        let state = self.state();
        let session = state
            .sessions
            .get(&digest(token))
            .filter(|s| s.expires > now)?;
        let device = state.devices.get(&session.device)?;
        Some(Principal {
            device: session.device.to_string(),
            expires: Some(session.expires),
            revocation: Revocation::watching(device.revoked.subscribe()),
            scope: session.scope,
        })
    }
}

/// A fresh device id.
fn new_device_id() -> Result<DeviceId, RandomUnavailable> {
    let bytes = secret::random::<{ DEVICE_ID_HEX / 2 }>()?;
    DeviceId::new(format!("{DEVICE_ID_PREFIX}{}", secret::hex(&bytes)))
        .map_err(|_| RandomUnavailable)
}

impl Authenticator for DeviceAuth {
    fn authenticate<'a>(&'a self, request: AuthRequest<'a>) -> AuthFuture<'a> {
        let verdict = bearer(&request)
            .and_then(|token| self.session_principal(token))
            .or_else(|| {
                self.static_token
                    .as_ref()
                    .filter(|s| s.accepts(&request))
                    .map(|_| Principal::new(crate::server::STATIC_DEVICE))
            })
            .ok_or(AuthRejection::Unauthenticated);
        Box::pin(async move { verdict })
    }

    fn public_routes(self: Arc<Self>) -> axum::Router {
        routes::router(self)
    }

    fn revocations(&self) -> Option<broadcast::Receiver<String>> {
        Some(self.revoked.subscribe())
    }

    fn known_devices(&self) -> Option<BTreeSet<String>> {
        let mut known: BTreeSet<String> = self
            .state()
            .devices
            .keys()
            .map(ToString::to_string)
            .collect();
        if self.static_token.is_some() {
            known.insert(crate::server::STATIC_DEVICE.to_owned());
        }
        Some(known)
    }
}

#[cfg(test)]
mod tests;
