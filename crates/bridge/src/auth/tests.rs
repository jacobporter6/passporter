//! Unit tests for [`DeviceAuth`]: the clock is tokio's, paused.

use std::net::IpAddr;

use axum::http::{HeaderMap, HeaderValue, Uri, header};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature as EcdsaSignature, SigningKey};
use protocol::auth::SIGNED_PREFIX;
use protocol::{PublicKey, Signature};

use super::*;
use crate::auth::devices::DeviceStore;
use crate::server::{ApiToken, ClientAddr};
use crate::testing::{eventually, set_mode};

const STATIC_SECRET: &str = "0123456789abcdef0123456789abcdef";

fn client() -> IpAddr {
    IpAddr::from([10, 77, 70, 2])
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_slice(&[seed; 32]).expect("valid scalar")
}

fn public(key: &SigningKey) -> PublicKey {
    PublicKey::new(secret::hex(
        key.verifying_key().to_sec1_point(false).as_bytes(),
    ))
    .expect("key")
}

fn sign(key: &SigningKey, device: &DeviceId, challenge: &Challenge) -> Signature {
    let signature: EcdsaSignature = key.sign(&signed_message(device, challenge));
    Signature::new(secret::hex(&signature.to_bytes())).expect("signature")
}

struct Fixture {
    tmp: tempfile::TempDir,
    auth: DeviceAuth,
}

fn fixture_with(settings: AuthSettings, static_token: Option<StaticToken>) -> Fixture {
    let tmp = tempfile::tempdir().expect("tmp");
    let auth = DeviceAuth::open(tmp.path(), settings, static_token).expect("open");
    Fixture { tmp, auth }
}

fn fixture() -> Fixture {
    fixture_with(AuthSettings::default(), None)
}

impl Fixture {
    async fn pair(&self, key: &SigningKey) -> DeviceId {
        let code = self.auth.start_pairing(None).expect("code").code;
        self.auth
            .pair(
                client(),
                PairRequest {
                    code,
                    public_key: public(key),
                    name: None,
                },
            )
            .await
            .expect("paired")
            .device_id
    }

    fn signed(&self, device: &DeviceId, key: &SigningKey) -> SessionRequest {
        let challenge = self
            .auth
            .challenge(
                client(),
                &ChallengeRequest {
                    device_id: device.clone(),
                },
            )
            .expect("challenge")
            .challenge;
        SessionRequest {
            device_id: device.clone(),
            signature: sign(key, device, &challenge),
            challenge,
            step_up: false,
            scope: Scope::App,
        }
    }

    async fn principal(&self, token: &str) -> Result<Principal, AuthRejection> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("header"),
        );
        let uri = Uri::from_static("/v1/agents");
        self.auth
            .authenticate(AuthRequest {
                headers: &headers,
                uri: &uri,
                client: ClientAddr(client()),
            })
            .await
    }
}

#[tokio::test(start_paused = true)]
async fn pair_sign_and_use_a_session() {
    let f = fixture();
    let key = key(7);
    let device = f.pair(&key).await;
    let grant = f
        .auth
        .session(client(), &f.signed(&device, &key))
        .expect("session");
    assert_eq!(grant.expires_in_secs, 15 * 60);
    let principal = f.principal(&grant.token).await.expect("authenticated");
    assert_eq!(principal.device, device.as_str());
    assert_eq!(
        principal.expires,
        Some(Instant::now() + Duration::from_mins(15))
    );
    assert!(!principal.revocation.is_revoked());
    let devices = f.auth.devices();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].sessions, 1);
    assert_eq!(devices[0].record.name.as_str(), DEFAULT_DEVICE_NAME);
}

#[tokio::test(start_paused = true)]
async fn a_session_keeps_the_scope_it_asked_for() {
    let f = fixture();
    let key = key(7);
    let device = f.pair(&key).await;
    let app = f
        .auth
        .session(client(), &f.signed(&device, &key))
        .expect("app session");
    assert_eq!(app.scope, Scope::App);
    assert_eq!(
        f.principal(&app.token).await.expect("app").scope,
        Scope::App
    );
    let car = f
        .auth
        .session(
            client(),
            &SessionRequest {
                scope: Scope::Car,
                ..f.signed(&device, &key)
            },
        )
        .expect("car session");
    assert_eq!(car.scope, Scope::Car);
    let principal = f.principal(&car.token).await.expect("car");
    assert_eq!(principal.scope, Scope::Car);
    assert_eq!(principal.device, device.as_str(), "the same device");
    assert_eq!(f.auth.devices()[0].sessions, 2);
}

#[tokio::test(start_paused = true)]
async fn a_replayed_challenge_is_refused() {
    let f = fixture();
    let key = key(7);
    let device = f.pair(&key).await;
    let request = f.signed(&device, &key);
    f.auth.session(client(), &request).expect("first use");
    assert_eq!(
        f.auth.session(client(), &request).err(),
        Some(Refusal::Unauthenticated)
    );
}

#[tokio::test(start_paused = true)]
async fn a_bad_signature_burns_the_challenge() {
    let f = fixture();
    let key = key(7);
    let device = f.pair(&key).await;
    let mut request = f.signed(&device, &key);
    let good = request.signature.clone();
    request.signature = sign(&self::key(8), &device, &request.challenge);
    assert_eq!(
        f.auth.session(client(), &request).err(),
        Some(Refusal::Unauthenticated)
    );
    request.signature = good;
    assert_eq!(
        f.auth.session(client(), &request).err(),
        Some(Refusal::Unauthenticated),
        "the challenge was used up by the failed attempt"
    );
}

#[tokio::test(start_paused = true)]
async fn a_challenge_is_bound_to_its_device_and_expires() {
    let f = fixture();
    let (a, b) = (key(7), key(8));
    let device_a = f.pair(&a).await;
    let device_b = f.pair(&b).await;
    let mut stolen = f.signed(&device_a, &a);
    stolen.signature = sign(&b, &device_b, &stolen.challenge);
    stolen.device_id = device_b;
    assert_eq!(
        f.auth.session(client(), &stolen).err(),
        Some(Refusal::Unauthenticated)
    );

    let late = f.signed(&device_a, &a);
    tokio::time::advance(Duration::from_secs(61)).await;
    assert_eq!(
        f.auth.session(client(), &late).err(),
        Some(Refusal::Unauthenticated)
    );
}

#[tokio::test(start_paused = true)]
async fn an_expired_pairing_code_is_refused() {
    let f = fixture();
    let code = f.auth.start_pairing(None).expect("code").code;
    tokio::time::advance(Duration::from_mins(5)).await;
    let request = PairRequest {
        code,
        public_key: public(&key(7)),
        name: None,
    };
    assert_eq!(
        f.auth.pair(client(), request).await.err(),
        Some(Refusal::Unauthenticated)
    );
    assert_eq!(f.auth.devices(), Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_pairing_code_is_single_use() {
    let f = fixture();
    let code = f.auth.start_pairing(None).expect("code").code;
    let request = |seed| PairRequest {
        code: code.clone(),
        public_key: public(&key(seed)),
        name: Some(DeviceName::new("Pixel").expect("name")),
    };
    f.auth.pair(client(), request(7)).await.expect("first use");
    assert_eq!(
        f.auth.pair(client(), request(8)).await.err(),
        Some(Refusal::Unauthenticated)
    );
    let devices = f.auth.devices();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].record.name.as_str(), "Pixel");
}

#[tokio::test(start_paused = true)]
async fn wrong_codes_burn_every_pending_code() {
    let f = fixture_with(
        AuthSettings {
            max_failed_pairs: 3,
            pair_rate: RateLimit::per_minute(100),
            ..AuthSettings::default()
        },
        None,
    );
    let code = f.auth.start_pairing(None).expect("code").code;
    let wrong = PairingCode::new(if code.as_str() == "00000000" {
        "11111111"
    } else {
        "00000000"
    })
    .expect("code");
    for _ in 0..3 {
        let guess = PairRequest {
            code: wrong.clone(),
            public_key: public(&key(9)),
            name: None,
        };
        assert!(f.auth.pair(client(), guess).await.is_err());
    }
    let late = PairRequest {
        code,
        public_key: public(&key(7)),
        name: None,
    };
    assert_eq!(
        f.auth.pair(client(), late).await.err(),
        Some(Refusal::Unauthenticated),
        "the real code was burned"
    );
}

#[tokio::test(start_paused = true)]
async fn pairing_is_rate_limited_per_client() {
    let f = fixture();
    let wrong = PairingCode::new("00000000").expect("code");
    for _ in 0..5 {
        let guess = PairRequest {
            code: wrong.clone(),
            public_key: public(&key(9)),
            name: None,
        };
        assert_eq!(
            f.auth.pair(client(), guess).await.err(),
            Some(Refusal::Unauthenticated)
        );
    }
    let sixth = PairRequest {
        code: wrong,
        public_key: public(&key(9)),
        name: None,
    };
    assert!(matches!(
        f.auth.pair(client(), sixth).await.err(),
        Some(Refusal::RateLimited(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn sessions_expire_and_refresh_by_signature() {
    let f = fixture();
    let key = key(7);
    let device = f.pair(&key).await;
    let first = f
        .auth
        .session(client(), &f.signed(&device, &key))
        .expect("session");
    tokio::time::advance(Duration::from_mins(14)).await;
    let refreshed = f
        .auth
        .session(client(), &f.signed(&device, &key))
        .expect("refresh");
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(f.principal(&first.token).await.is_err(), "expired");
    assert!(f.principal(&refreshed.token).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn the_signature_binds_the_device_id() {
    let f = fixture();
    let key = key(7);
    let device = f.pair(&key).await;
    let mut request = f.signed(&device, &key);
    // A signature over the bare challenge (the old message) is refused.
    let bare: EcdsaSignature = key.sign(
        &[
            SIGNED_PREFIX.as_bytes(),
            request.challenge.as_str().as_bytes(),
        ]
        .concat(),
    );
    request.signature = Signature::new(secret::hex(&bare.to_bytes())).expect("signature");
    assert_eq!(
        f.auth.session(client(), &request).err(),
        Some(Refusal::Unauthenticated)
    );
}

#[tokio::test(start_paused = true)]
async fn step_up_is_refused_until_user_verification_exists() {
    let f = fixture();
    let key = key(7);
    let device = f.pair(&key).await;
    let mut request = f.signed(&device, &key);
    request.step_up = true;
    assert_eq!(
        f.auth.session(client(), &request).err(),
        Some(Refusal::BadRequest)
    );
    assert_eq!(f.auth.devices()[0].sessions, 0);
}

#[tokio::test(start_paused = true)]
async fn an_unknown_device_is_told_apart_from_a_stale_challenge() {
    let f = fixture();
    let key = key(7);
    let device = f.pair(&key).await;
    let stranger = DeviceId::new("dev-0000000000000001").expect("id");
    assert_eq!(
        f.auth
            .challenge(
                client(),
                &ChallengeRequest {
                    device_id: stranger.clone()
                }
            )
            .err(),
        Some(Refusal::UnknownDevice)
    );
    // A known device whose challenge the bridge forgot (a restart, say)
    // just gets 401: sign a fresh one.
    let mut stale = f.signed(&device, &key);
    stale.challenge = Challenge::new("ab".repeat(32)).expect("challenge");
    stale.signature = sign(&key, &device, &stale.challenge);
    assert_eq!(
        f.auth.session(client(), &stale).err(),
        Some(Refusal::Unauthenticated)
    );
    stale.device_id = stranger;
    assert_eq!(
        f.auth.session(client(), &stale).err(),
        Some(Refusal::UnknownDevice)
    );
}

#[tokio::test(start_paused = true)]
async fn the_operator_label_wins_over_the_device_name() {
    let f = fixture();
    let code = f
        .auth
        .start_pairing(Some(DeviceName::new("Sam's Pixel").expect("name")))
        .expect("code")
        .code;
    f.auth
        .pair(
            client(),
            PairRequest {
                code,
                public_key: public(&key(7)),
                name: Some(DeviceName::new("Admin Laptop").expect("name")),
            },
        )
        .await
        .expect("paired");
    assert_eq!(f.auth.devices()[0].record.name.as_str(), "Sam's Pixel");
}

#[tokio::test(start_paused = true)]
async fn revocation_ends_sessions_and_fires() {
    let f = fixture();
    let key = key(7);
    let device = f.pair(&key).await;
    let grant = f
        .auth
        .session(client(), &f.signed(&device, &key))
        .expect("session");
    let mut principal = f.principal(&grant.token).await.expect("ok");
    assert_eq!(f.auth.revoke(&device).await.expect("revoked"), 1);
    assert!(principal.revocation.is_revoked());
    principal.revocation.revoked().await;
    assert!(f.principal(&grant.token).await.is_err());
    assert_eq!(
        f.auth
            .challenge(
                client(),
                &ChallengeRequest {
                    device_id: device.clone()
                }
            )
            .err(),
        Some(Refusal::UnknownDevice)
    );
    assert!(matches!(
        f.auth.revoke(&device).await,
        Err(AuthError::UnknownDevice(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn devices_survive_a_restart_but_sessions_do_not() {
    let tmp = tempfile::tempdir().expect("tmp");
    let key = key(7);
    let (device, token) = {
        let f = Fixture {
            auth: DeviceAuth::open(tmp.path(), AuthSettings::default(), None).expect("open"),
            tmp: tempfile::tempdir().expect("unused"),
        };
        let device = f.pair(&key).await;
        let token = f
            .auth
            .session(client(), &f.signed(&device, &key))
            .expect("session")
            .token;
        (device, token)
    };
    let f = Fixture {
        auth: DeviceAuth::open(tmp.path(), AuthSettings::default(), None).expect("reopen"),
        tmp: tempfile::tempdir().expect("unused"),
    };
    assert_eq!(f.auth.devices().len(), 1);
    assert!(f.principal(&token).await.is_err());
    f.auth
        .session(client(), &f.signed(&device, &key))
        .expect("signs back in");
}

#[tokio::test(start_paused = true)]
async fn the_static_token_works_only_when_opted_in() {
    let token = ApiToken::new(STATIC_SECRET).expect("token");
    let opted_in = fixture_with(AuthSettings::default(), Some(StaticToken::new(token)));
    let principal = opted_in.principal(STATIC_SECRET).await.expect("accepted");
    assert_eq!(principal.device, crate::server::STATIC_DEVICE);
    assert_eq!(principal.expires, None);

    let device_only = fixture();
    assert_eq!(
        device_only.principal(STATIC_SECRET).await.err(),
        Some(AuthRejection::Unauthenticated)
    );
}

#[tokio::test(start_paused = true)]
async fn the_device_limit_holds() {
    let f = fixture_with(
        AuthSettings {
            max_devices: 1,
            ..AuthSettings::default()
        },
        None,
    );
    f.pair(&key(7)).await;
    let code = f.auth.start_pairing(None).expect("code").code;
    let request = PairRequest {
        code,
        public_key: public(&key(8)),
        name: None,
    };
    assert_eq!(
        f.auth.pair(client(), request).await.err(),
        Some(Refusal::TooManyDevices)
    );
}

/// The device ids in the device file.
fn ids_on_disk(dir: &Path) -> Vec<DeviceId> {
    let mut ids: Vec<_> = DeviceStore::open(dir)
        .expect("open")
        .load()
        .expect("load")
        .into_iter()
        .map(|record| record.id)
        .collect();
    ids.sort();
    ids
}

#[tokio::test(start_paused = true)]
async fn concurrent_pair_and_revoke_saves_keep_both_changes() {
    let f = fixture();
    let old = f.pair(&key(1)).await;
    let watcher = f.pair(&key(3)).await;
    // As if the disk were slow: both saves wait for their turn.
    let held = f.auth.saver.store.hold_turn().await;
    let code = f.auth.start_pairing(None).expect("code").code;
    let pairing = f.auth.pair(
        client(),
        PairRequest {
            code,
            public_key: public(&key(2)),
            name: None,
        },
    );
    let revoking = f.auth.revoke(&old);
    let meanwhile = async {
        tokio::task::yield_now().await;
        // Authentication does not wait for the disk.
        let grant = f
            .auth
            .session(client(), &f.signed(&watcher, &key(3)))
            .expect("a session while saves wait");
        assert!(f.principal(&grant.token).await.is_ok());
        drop(held);
    };
    let (paired, revoked, ()) = tokio::join!(pairing, revoking, meanwhile);
    let new = paired.expect("paired").device_id;
    assert_eq!(revoked.expect("revoked"), 0);
    let mut expected = vec![new, watcher];
    expected.sort();
    assert_eq!(ids_on_disk(f.tmp.path()), expected);
}

#[tokio::test(start_paused = true)]
async fn failed_saves_undo_a_pairing_but_never_a_revocation() {
    let f = fixture();
    let mut announced = f.auth.revocations().expect("announces");
    let kept = f.pair(&key(1)).await;
    let revoked = f.pair(&key(2)).await;
    set_mode(f.tmp.path(), 0o500);
    let code = f.auth.start_pairing(None).expect("code").code;
    let refused = f
        .auth
        .pair(
            client(),
            PairRequest {
                code,
                public_key: public(&key(3)),
                name: None,
            },
        )
        .await;
    assert_eq!(refused.err(), Some(Refusal::Internal));
    assert_eq!(f.auth.devices().len(), 2, "the failed pairing is undone");
    for _ in 0..2 {
        // Revoking again retries the save.
        assert!(matches!(
            f.auth.revoke(&revoked).await,
            Err(AuthError::RevocationNotSaved { .. })
        ));
    }
    assert!(
        f.auth.devices().iter().all(|d| d.record.id != revoked),
        "revoked all the same"
    );
    // Announced at once (push stops pinging it), and only once.
    assert_eq!(
        announced.try_recv().expect("announced"),
        revoked.to_string()
    );
    assert!(
        announced.try_recv().is_err(),
        "a retry is not announced again"
    );
    set_mode(f.tmp.path(), 0o700);
    assert_eq!(f.auth.revoke(&revoked).await.expect("saved now"), 0);
    assert_eq!(ids_on_disk(f.tmp.path()), [kept]);
    assert!(matches!(
        f.auth.revoke(&revoked).await,
        Err(AuthError::UnknownDevice(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_revocation_is_saved_even_if_the_caller_gives_up() {
    let f = fixture();
    let kept = f.pair(&key(1)).await;
    let revoked = f.pair(&key(2)).await;
    let held = f.auth.saver.store.hold_turn().await;
    // As the admin socket's exchange timeout would.
    let gave_up = tokio::time::timeout(Duration::from_secs(5), f.auth.revoke(&revoked)).await;
    assert!(gave_up.is_err(), "still waiting for the disk");
    drop(held);
    eventually("the revocation reaches the disk", || {
        ids_on_disk(f.tmp.path()) == [kept.clone()]
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_device_revoked_while_it_saves_is_refused_as_unknown() {
    let f = fixture();
    let old = f.pair(&key(1)).await;
    let held = f.auth.saver.store.hold_turn().await;
    let code = f.auth.start_pairing(None).expect("code").code;
    let pairing = f.auth.pair(
        client(),
        PairRequest {
            code,
            public_key: public(&key(2)),
            name: None,
        },
    );
    let operator = async {
        tokio::task::yield_now().await;
        let new = f
            .auth
            .devices()
            .into_iter()
            .map(|d| d.record.id)
            .find(|id| id != &old)
            .expect("the new device, in memory");
        drop(held);
        f.auth.revoke(&new).await.expect("revoked");
    };
    let (paired, ()) = tokio::join!(pairing, operator);
    assert_eq!(paired.err(), Some(Refusal::UnknownDevice));
    assert_eq!(ids_on_disk(f.tmp.path()), [old]);
}

#[tokio::test(start_paused = true)]
async fn a_pairing_saved_after_its_request_is_dropped_is_undone() {
    let f = fixture();
    let old = f.pair(&key(1)).await;
    let held = f.auth.saver.store.hold_turn().await;
    let code = f.auth.start_pairing(None).expect("code").code;
    let pairing = f.auth.pair(
        client(),
        PairRequest {
            code,
            public_key: public(&key(2)),
            name: None,
        },
    );
    // The save would succeed, but nobody is left to hear the id.
    let gave_up = tokio::time::timeout(Duration::from_secs(30), pairing).await;
    assert!(gave_up.is_err(), "still waiting for the disk");
    drop(held);
    eventually("the orphaned device is unpaired", || {
        f.auth.devices().len() == 1 && ids_on_disk(f.tmp.path()) == [old.clone()]
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn unsaved_revocations_keep_their_slots() {
    let f = fixture_with(
        AuthSettings {
            max_devices: 2,
            ..AuthSettings::default()
        },
        None,
    );
    f.pair(&key(1)).await;
    let revoked = f.pair(&key(2)).await;
    set_mode(f.tmp.path(), 0o500);
    assert!(f.auth.revoke(&revoked).await.is_err(), "not saved");
    set_mode(f.tmp.path(), 0o700);
    let pair = |seed| {
        let code = f.auth.start_pairing(None).expect("code").code;
        f.auth.pair(
            client(),
            PairRequest {
                code,
                public_key: public(&key(seed)),
                name: None,
            },
        )
    };
    // Still in the device file: a restart would bring it back.
    assert_eq!(pair(3).await.err(), Some(Refusal::TooManyDevices));
    f.auth.revoke(&revoked).await.expect("saved now");
    pair(4).await.expect("its slot is free once saved");
}

#[tokio::test(start_paused = true)]
async fn a_pairing_is_undone_even_if_the_request_is_dropped() {
    let f = fixture();
    f.pair(&key(1)).await;
    set_mode(f.tmp.path(), 0o500);
    let held = f.auth.saver.store.hold_turn().await;
    let code = f.auth.start_pairing(None).expect("code").code;
    let pairing = f.auth.pair(
        client(),
        PairRequest {
            code,
            public_key: public(&key(2)),
            name: None,
        },
    );
    // As the request timeout would.
    let gave_up = tokio::time::timeout(Duration::from_secs(30), pairing).await;
    assert!(gave_up.is_err(), "still waiting for the disk");
    assert_eq!(f.auth.devices().len(), 2, "in memory while it saves");
    drop(held);
    eventually("the failed pairing is undone", || {
        f.auth.devices().len() == 1
    })
    .await;
    set_mode(f.tmp.path(), 0o700);
}

#[tokio::test(start_paused = true)]
async fn known_devices_are_the_paired_ones_and_the_static_token() {
    let device_only = fixture();
    let paired = device_only.pair(&key(1)).await;
    assert_eq!(
        device_only.auth.known_devices(),
        Some(BTreeSet::from([paired.to_string()]))
    );
    device_only.auth.revoke(&paired).await.expect("revoked");
    assert_eq!(device_only.auth.known_devices(), Some(BTreeSet::new()));

    let token = ApiToken::new(STATIC_SECRET).expect("token");
    let opted_in = fixture_with(AuthSettings::default(), Some(StaticToken::new(token)));
    assert_eq!(
        opted_in.auth.known_devices(),
        Some(BTreeSet::from([crate::server::STATIC_DEVICE.to_owned()]))
    );
}
