//! Device pairing and session tokens (design §8).
//!
//! The flow, all under `/v1/auth/` and unauthenticated (each endpoint is
//! rate limited per client):
//!
//! 1. `passporter pair` on the dev server shows a one-time [`PairingCode`]
//!    (single use, five minutes). The phone generates a non-extractable
//!    P-256 key pair and sends `POST /v1/auth/pair` ([`PairRequest`]) with
//!    the code and its public key; the bridge answers [`PairResponse`] with
//!    the new [`DeviceId`].
//! 2. To get a session, the phone asks `POST /v1/auth/challenge`
//!    ([`ChallengeRequest`] → [`ChallengeResponse`]), signs
//!    [`signed_message`] of its id and the challenge with ECDSA P-256 /
//!    SHA-256, and sends `POST /v1/auth/session` ([`SessionRequest`]). The
//!    bridge answers a short-lived bearer token ([`SessionGrant`]).
//!    Refreshing is the same two steps again: every token comes from a fresh
//!    signature.
//!
//! Each challenge is single use and bound to the device that asked for it.
//! Binary values travel as lowercase hex.
//!
//! Refusals are bare statuses:
//!
//! - `400`: a request the bridge will not serve (today: `stepUp`).
//! - `401`: wrong, expired or used code, challenge or signature. Not a
//!   verdict on the device: after a bridge restart, say, a challenge in
//!   flight is gone. Ask for a fresh challenge and sign again.
//! - [`DEVICE_UNKNOWN_STATUS`] (`410 Gone`): the bridge does not know this
//!   device (never paired, or revoked). The only answer after which a
//!   client should forget its key and pair again.
//! - `409`: the device limit is reached; `429`: rate limited
//!   (`Retry-After`).

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::action::Origin;
use crate::error::ValidationError;
use crate::macros::validated_string;
use crate::text::is_hidden_char;

/// Symbols in a [`PairingCode`].
pub const PAIRING_CODE_LEN: usize = 8;

/// Bytes in a SEC1 uncompressed P-256 public key (`04 || x || y`).
pub const PUBLIC_KEY_BYTES: usize = 65;

/// Bytes in a challenge.
pub const CHALLENGE_BYTES: usize = 32;

/// Bytes in a P-256 ECDSA signature in IEEE P1363 form (`r || s`), which
/// is what `WebCrypto` produces.
pub const SIGNATURE_BYTES: usize = 64;

/// Longest [`DeviceName`], in bytes.
pub const MAX_DEVICE_NAME_LEN: usize = 48;

/// The status `/v1/auth/challenge` and `/v1/auth/session` answer when the
/// bridge does not know the device (never paired, or revoked): `410 Gone`.
pub const DEVICE_UNKNOWN_STATUS: u16 = 410;

/// Prefix of every signed challenge, so a device key can never be tricked
/// into signing something that also means something else.
pub const SIGNED_PREFIX: &str = "passporter-session-v1:";

/// The exact bytes `device` signs for `challenge`:
/// `passporter-session-v1:{device}:{challenge}` ([`SIGNED_PREFIX`], the
/// device id, a colon, the challenge's hex text). Binding the id means a
/// signature only ever vouches for the device that made it.
#[must_use]
pub fn signed_message(device: &DeviceId, challenge: &Challenge) -> Vec<u8> {
    [
        SIGNED_PREFIX.as_bytes(),
        device.as_str().as_bytes(),
        b":",
        challenge.as_str().as_bytes(),
    ]
    .concat()
}

/// The Crockford base32 alphabet, without `I`, `L`, `O` and `U`.
pub const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A one-time pairing code: [`PAIRING_CODE_LEN`] upper-case symbols of
/// [`CROCKFORD_ALPHABET`], no separator. Displayed as `XXXX-XXXX`; clients
/// normalize what the user typed before sending it. `Debug` is redacted:
/// the code is a credential while it lives.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PairingCode(String);

impl PairingCode {
    /// Validates `value`.
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ValidationError::Empty {
                field: "pairingCode",
            });
        }
        if value.len() != PAIRING_CODE_LEN {
            return Err(ValidationError::TooLong {
                field: "pairingCode",
                max: PAIRING_CODE_LEN,
                actual: value.len(),
            });
        }
        if !value.bytes().all(|b| CROCKFORD_ALPHABET.contains(&b)) {
            return Err(ValidationError::InvalidCharacter {
                field: "pairingCode",
            });
        }
        Ok(Self(value))
    }

    /// The code, without separator.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PairingCode {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PairingCode> for String {
    fn from(value: PairingCode) -> Self {
        value.0
    }
}

/// Displays as `XXXX-XXXX`.
impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (a, b) = self.0.split_at(PAIRING_CODE_LEN / 2);
        write!(f, "{a}-{b}")
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(<redacted>)")
    }
}

/// Checks `value` is exactly `bytes` bytes of lowercase hex.
fn check_hex(field: &'static str, value: &str, bytes: usize) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(ValidationError::Empty { field });
    }
    if value.len() != bytes * 2 {
        return Err(ValidationError::TooLong {
            field,
            max: bytes * 2,
            actual: value.len(),
        });
    }
    if value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(ValidationError::InvalidCharacter { field })
    }
}

/// Prefix of every [`DeviceId`].
pub const DEVICE_ID_PREFIX: &str = "dev-";

/// Hex digits after [`DEVICE_ID_PREFIX`].
pub const DEVICE_ID_HEX: usize = 16;

validated_string!(
    /// A paired device's id: `dev-` and 16 lowercase hex digits, assigned by
    /// the bridge. Never a credential.
    DeviceId,
    |v: &str| match v.strip_prefix(DEVICE_ID_PREFIX) {
        Some(hex) => check_hex("deviceId", hex, DEVICE_ID_HEX / 2),
        None => Err(ValidationError::InvalidCharacter { field: "deviceId" }),
    }
);

validated_string!(
    /// A human label for a device ("Pixel 8"): 1..=[`MAX_DEVICE_NAME_LEN`]
    /// bytes, not blank, without control or invisible characters. Shown by
    /// `passporter devices list`, so it must not be able to drive a terminal.
    DeviceName,
    |v: &str| {
        if v.trim().is_empty() {
            Err(ValidationError::Empty { field: "name" })
        } else if v.len() > MAX_DEVICE_NAME_LEN {
            Err(ValidationError::TooLong {
                field: "name",
                max: MAX_DEVICE_NAME_LEN,
                actual: v.len(),
            })
        } else if v.chars().any(|c| c.is_control() || is_hidden_char(c)) {
            Err(ValidationError::ControlCharacter { field: "name" })
        } else {
            Ok(())
        }
    }
);

validated_string!(
    /// A P-256 public key: SEC1 uncompressed (`04 || x || y`,
    /// [`PUBLIC_KEY_BYTES`] bytes) in lowercase hex, as `WebCrypto`'s
    /// `exportKey("raw")` gives it. Whether it is a point on the curve is
    /// checked by the bridge.
    PublicKey,
    |v: &str| {
        check_hex("publicKey", v, PUBLIC_KEY_BYTES)?;
        if v.starts_with("04") {
            Ok(())
        } else {
            Err(ValidationError::InvalidCharacter { field: "publicKey" })
        }
    }
);

validated_string!(
    /// A server challenge: [`CHALLENGE_BYTES`] random bytes in lowercase hex.
    Challenge,
    |v: &str| check_hex("challenge", v, CHALLENGE_BYTES)
);

validated_string!(
    /// An ECDSA P-256 / SHA-256 signature, `r || s`
    /// ([`SIGNATURE_BYTES`] bytes), in lowercase hex.
    Signature,
    |v: &str| check_hex("signature", v, SIGNATURE_BYTES)
);

/// `POST /v1/auth/pair`: register this device's public key with a pairing
/// code. Inbound, so strict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairRequest {
    /// The code `passporter pair` showed.
    pub code: PairingCode,
    /// The device's public key.
    pub public_key: PublicKey,
    /// A label for `passporter devices list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<DeviceName>,
}

/// The answer to a [`PairRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    /// The new device's id; keep it to ask for challenges.
    pub device_id: DeviceId,
}

/// `POST /v1/auth/challenge`. Inbound, so strict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChallengeRequest {
    /// The paired device asking.
    pub device_id: DeviceId,
}

/// The answer to a [`ChallengeRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeResponse {
    /// Sign [`signed_message`] of this.
    pub challenge: Challenge,
    /// Seconds until the challenge expires.
    pub expires_in_secs: u64,
}

/// What a session may do, chosen when it is made ([`SessionRequest::scope`]).
/// The bridge derives every action's [`Origin`] from it ([`Scope::origin`]),
/// so a token can never claim more than it was granted (design §6.6, §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The app (PWA or APK) and its notifications: the whole API.
    #[default]
    App,
    /// Android Auto: only the car-safe endpoints (the agent list, pending
    /// inputs, TL;DRs, `POST /v1/agents/:id/car-reply` and car-origin
    /// actions), and every action is judged as `origin: "car"`.
    Car,
}

impl Scope {
    /// True for [`Scope::App`] (the wire default, so it is left out).
    #[must_use]
    pub const fn is_app(&self) -> bool {
        matches!(self, Self::App)
    }

    /// The origin an action sent with a session of this scope is judged
    /// and audited as. A car session is always [`Origin::Car`], whatever
    /// the body says. An app session keeps the body's claim: `app`,
    /// `notification` and `car` each allow at most what `app` does, so the
    /// claim can only narrow what the token may do, never widen it.
    #[must_use]
    pub const fn origin(self, claimed: Origin) -> Origin {
        match self {
            Self::App => claimed,
            Self::Car => Origin::Car,
        }
    }
}

/// `POST /v1/auth/session`: trade a signed challenge for a session token.
/// Inbound, so strict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRequest {
    /// The device that asked for the challenge.
    pub device_id: DeviceId,
    /// The challenge, as received.
    pub challenge: Challenge,
    /// The signature over [`signed_message`] of the device id and the
    /// challenge.
    pub signature: Signature,
    /// Reserved for step-up sessions (design §8). The signature does not
    /// prove the user verified on the device, so until real user
    /// verification exists (`WebAuthn`) the bridge refuses `true` with 400.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub step_up: bool,
    /// What the session may do; `app` when absent. Android Auto asks for
    /// `car`. Not signed: a car session can do less than an app session the
    /// same signature could get, so nothing is gained by changing it.
    #[serde(default, skip_serializing_if = "Scope::is_app")]
    pub scope: Scope,
}

/// A session token. `Debug` is redacted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionGrant {
    /// The bearer token for `Authorization: Bearer` and the stream's first
    /// message.
    pub token: String,
    /// Seconds until it expires; get a new one before then. Open streams
    /// authenticated with it are closed (1008) when it does.
    pub expires_in_secs: u64,
    /// The device's name on the bridge (as `passporter devices list`
    /// shows it), for the app to show. Absent from older bridges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<DeviceName>,
    /// The scope granted (as asked); `app` when absent.
    #[serde(default, skip_serializing_if = "Scope::is_app")]
    pub scope: Scope,
}

impl fmt::Debug for SessionGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionGrant")
            .field("token", &"<redacted>")
            .field("expires_in_secs", &self.expires_in_secs)
            .field("device_name", &self.device_name)
            .field("scope", &self.scope)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "04aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\
                       bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn pairing_codes_are_crockford() {
        let code = PairingCode::new("ABCD2345").unwrap();
        assert_eq!(code.to_string(), "ABCD-2345");
        assert!(!format!("{code:?}").contains("ABCD"));
        for bad in [
            "",
            "ABCD-234",
            "abcd2345",
            "ABCDI345",
            "ABCDU345",
            "ABCD23456",
        ] {
            assert!(PairingCode::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ids_and_hex_are_strict() {
        assert!(DeviceId::new("dev-0123456789abcdef").is_ok());
        for bad in ["0123456789abcdef", "dev-0123", "dev-0123456789ABCDEF"] {
            assert!(DeviceId::new(bad).is_err(), "{bad}");
        }
        assert!(PublicKey::new(KEY).is_ok());
        assert!(PublicKey::new(KEY.replacen("04", "02", 1)).is_err());
        assert!(Challenge::new("ab".repeat(CHALLENGE_BYTES)).is_ok());
        assert!(Challenge::new("ab".repeat(CHALLENGE_BYTES - 1)).is_err());
        assert!(Signature::new("0g".repeat(SIGNATURE_BYTES)).is_err());
    }

    #[test]
    fn device_names_cannot_drive_a_terminal() {
        assert!(DeviceName::new("Pixel 8 Pro").is_ok());
        for bad in ["", "  ", "a\u{1b}[2J", "a\nb", "a\u{202e}b"] {
            assert!(DeviceName::new(bad).is_err(), "{bad:?}");
        }
        assert!(DeviceName::new("x".repeat(MAX_DEVICE_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn requests_are_strict_camel_case() {
        let json = format!(r#"{{"code":"ABCD2345","publicKey":"{KEY}","name":"Pixel"}}"#);
        let request: PairRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(request.name.unwrap().as_str(), "Pixel");
        let extra = format!(r#"{{"code":"ABCD2345","publicKey":"{KEY}","admin":true}}"#);
        assert!(serde_json::from_str::<PairRequest>(&extra).is_err());

        let session = format!(
            r#"{{"deviceId":"dev-0123456789abcdef","challenge":"{}","signature":"{}"}}"#,
            "00".repeat(CHALLENGE_BYTES),
            "11".repeat(SIGNATURE_BYTES)
        );
        let session: SessionRequest = serde_json::from_str(&session).unwrap();
        assert!(!session.step_up);
        assert_eq!(session.scope, Scope::App);
        let wire = serde_json::to_string(&session).unwrap();
        assert!(
            !wire.contains("stepUp") && !wire.contains("scope"),
            "{wire}"
        );
        let car: SessionRequest =
            serde_json::from_str(&wire.replace('}', r#","scope":"car"}"#)).unwrap();
        assert_eq!(car.scope, Scope::Car);
        assert!(
            serde_json::from_str::<SessionRequest>(&wire.replace('}', r#","scope":"admin"}"#))
                .is_err()
        );
    }

    #[test]
    fn a_car_scope_is_always_the_car_origin() {
        for claimed in [Origin::App, Origin::Notification, Origin::Car] {
            assert_eq!(Scope::Car.origin(claimed), Origin::Car);
            assert_eq!(Scope::App.origin(claimed), claimed);
        }
    }

    #[test]
    fn the_signed_message_binds_the_device() {
        let challenge = Challenge::new("0f".repeat(CHALLENGE_BYTES)).unwrap();
        let device = DeviceId::new("dev-0123456789abcdef").unwrap();
        let message = signed_message(&device, &challenge);
        assert_eq!(
            message,
            format!("passporter-session-v1:dev-0123456789abcdef:{challenge}").into_bytes()
        );
        let other = DeviceId::new("dev-fedcba9876543210").unwrap();
        assert_ne!(signed_message(&other, &challenge), message);
    }

    #[test]
    fn grants_redact_the_token() {
        let grant = SessionGrant {
            token: "s3cret".to_owned(),
            expires_in_secs: 900,
            device_name: Some(DeviceName::new("Pixel").unwrap()),
            scope: Scope::App,
        };
        assert!(!format!("{grant:?}").contains("s3cret"));
        let json = serde_json::to_string(&grant).unwrap();
        assert!(json.contains("\"expiresInSecs\":900"), "{json}");
        assert!(json.contains("\"deviceName\":\"Pixel\""), "{json}");
        // Older bridges send no name.
        let old: SessionGrant =
            serde_json::from_str(r#"{"token":"t","expiresInSecs":900}"#).unwrap();
        assert_eq!(old.device_name, None);
    }
}
