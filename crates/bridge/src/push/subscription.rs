//! A browser's push subscription, validated on the way in.
//!
//! The wire shape is what `PushSubscription.toJSON()` produces in the PWA:
//!
//! ```json
//! {"endpoint": "https://fcm.googleapis.com/fcm/send/…",
//!  "expirationTime": null,
//!  "keys": {"p256dh": "<base64url P-256 point>", "auth": "<base64url 16 bytes>"}}
//! ```
//!
//! The native app adds `"payload": "notice"` ([`PushPayload::Notice`]) to
//! get short encrypted notices instead of the bare ping; without it (the
//! PWA) nothing changes.
//!
//! Unknown fields are refused, and a value of [`Subscription`] is always
//! usable: an `https` endpoint on a DNS name, port 443, and keys of the
//! right shape (the public key a point on P-256).
//!
//! Which push services the bridge talks to at all is [`PushHosts`]: the
//! browsers' own services, any configured extras, and the self-hosted ntfy
//! server (`[push] ntfy_host`, that exact name). The subscribe route
//! and the dispatcher check it, so a subscription cannot point the bridge at
//! the LAN or anywhere else.

use std::fmt;
use std::str::FromStr;

use axum::http::Uri;
use axum::http::uri::Scheme;
use base64::Engine;
use base64::alphabet::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use protocol::PushPayload;
use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};
use ring::rand::SystemRandom;
use serde::{Deserialize, Serialize};

/// Unpadded base64url out, padded or not in (browsers differ).
pub(crate) const BASE64URL: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Longest endpoint URL accepted. Real ones are a few hundred characters.
pub const MAX_ENDPOINT_CHARS: usize = 2048;

/// Why a subscription was refused. Never quotes the keys.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidSubscription {
    /// The endpoint is not an absolute `https` URL on a DNS name at port 443.
    #[error("endpoint must be an https URL on a host name at port 443 (no IP addresses)")]
    Endpoint,
    /// The endpoint's host is not a known push service.
    #[error("endpoint is not on a known push service")]
    UnknownPushService,
    /// The endpoint is longer than [`MAX_ENDPOINT_CHARS`].
    #[error("endpoint is longer than {MAX_ENDPOINT_CHARS} characters")]
    EndpointTooLong,
    /// `keys.p256dh` is not a base64url uncompressed point on P-256.
    #[error("keys.p256dh must be a base64url uncompressed P-256 point (65 bytes)")]
    P256dh,
    /// `keys.auth` is not 16 base64url bytes.
    #[error("keys.auth must be 16 base64url-encoded bytes")]
    Auth,
}

/// A push service URL: absolute `https`, a DNS name (no IP address in any
/// spelling, no `localhost`), no credentials, port 443.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Endpoint(String);

impl Endpoint {
    /// The URL.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `https://host`, lowercase: the VAPID audience (RFC 8292 §2). The
    /// port is always 443, so it is never spelled out.
    #[must_use]
    pub fn origin(&self) -> String {
        format!("https://{}", self.host())
    }

    /// The host, lowercase and without a trailing dot. Also what logs show:
    /// the full URL is a capability and stays out of them.
    #[must_use]
    pub fn host(&self) -> String {
        let uri: Uri = self.0.parse().unwrap_or_default();
        canonical_host(uri.host().unwrap_or_default())
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Endpoint({}/…)", self.origin())
    }
}

impl FromStr for Endpoint {
    type Err = InvalidSubscription;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.chars().count() > MAX_ENDPOINT_CHARS {
            return Err(InvalidSubscription::EndpointTooLong);
        }
        let uri: Uri = text.parse().map_err(|_| InvalidSubscription::Endpoint)?;
        let authority = uri.authority().ok_or(InvalidSubscription::Endpoint)?;
        let usable = uri.scheme() == Some(&Scheme::HTTPS)
            && !authority.as_str().contains('@')
            && authority.port_u16().is_none_or(|port| port == 443)
            && is_dns_name(&canonical_host(authority.host()));
        if !usable {
            return Err(InvalidSubscription::Endpoint);
        }
        Ok(Self(text.to_owned()))
    }
}

/// `host` lowercase, without a trailing dot.
fn canonical_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// True for a lowercase DNS name of at least two labels whose last label
/// (the TLD) is letters only. That refuses every IPv4 spelling (`127.1`,
/// `0x7f000001`, `2130706433`), IPv6 literals and `localhost`.
fn is_dns_name(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let label_ok = |label: &&str| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(label_ok)
        && labels
            .last()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_lowercase()))
        && host != "localhost"
        && !host.ends_with(".localhost")
}

/// The push services browsers use: Chrome and other Chromium browsers
/// (FCM), Firefox (Mozilla autopush), Safari (Apple) and Edge (WNS).
pub const BUILTIN_PUSH_HOSTS: [&str; 4] = [
    "fcm.googleapis.com",
    "updates.push.services.mozilla.com",
    "push.apple.com",
    "notify.windows.com",
];

/// A push service host the bridge may send to, it and its subdomains (for
/// the `[push] extra_hosts` config list, such as a self-hosted service).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct PushHost(String);

/// Why a configured push host was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("push host {0:?} is not a DNS name (no scheme, port, path or IP address)")]
pub struct InvalidPushHost(String);

impl FromStr for PushHost {
    type Err = InvalidPushHost;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let host = canonical_host(text);
        if is_dns_name(&host) {
            Ok(Self(host))
        } else {
            Err(InvalidPushHost(text.to_owned()))
        }
    }
}

impl TryFrom<String> for PushHost {
    type Error = InvalidPushHost;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

/// True if `host` is `suffix` or one of its subdomains. Matches only at a
/// label boundary: `xfcm.googleapis.com` is not under `fcm.googleapis.com`.
fn is_under(host: &str, suffix: &str) -> bool {
    host.strip_suffix(suffix)
        .is_some_and(|rest| rest.is_empty() || rest.ends_with('.'))
}

/// The push services the bridge talks to: [`BUILTIN_PUSH_HOSTS`] and the
/// configured extras, each with its subdomains, plus the self-hosted ntfy
/// server by its exact name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushHosts {
    extra: Vec<PushHost>,
    ntfy: Option<PushHost>,
}

impl PushHosts {
    /// The built-in services plus `extra`.
    #[must_use]
    pub fn new(extra: Vec<PushHost>) -> Self {
        Self { extra, ntfy: None }
    }

    /// Also allows the self-hosted ntfy server (`UnifiedPush` for the native
    /// app) at exactly `host`: not its subdomains, unlike `extra`.
    #[must_use]
    pub fn with_ntfy(self, host: Option<PushHost>) -> Self {
        Self { ntfy: host, ..self }
    }

    /// True if `endpoint` is on an allowed push service.
    #[must_use]
    pub fn permits(&self, endpoint: &Endpoint) -> bool {
        let host = endpoint.host();
        self.ntfy.as_ref().is_some_and(|ntfy| ntfy.0 == host)
            || BUILTIN_PUSH_HOSTS
                .iter()
                .copied()
                .chain(self.extra.iter().map(|h| h.0.as_str()))
                .any(|suffix| is_under(&host, suffix))
    }

    /// `Ok` if `endpoint` is on an allowed push service.
    pub fn check(&self, endpoint: &Endpoint) -> Result<(), InvalidSubscription> {
        if self.permits(endpoint) {
            Ok(())
        } else {
            Err(InvalidSubscription::UnknownPushService)
        }
    }
}

/// The subscription's public key: an uncompressed point on P-256.
#[derive(Clone, PartialEq, Eq)]
pub struct P256dh([u8; 65]);

impl fmt::Debug for P256dh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("P256dh(…)")
    }
}

impl P256dh {
    pub(crate) fn as_bytes(&self) -> &[u8; 65] {
        &self.0
    }
}

#[cfg(test)]
impl P256dh {
    /// `bytes`, unchecked: a key no parsed subscription can hold (tests).
    pub(crate) fn unchecked(bytes: [u8; 65]) -> Self {
        Self(bytes)
    }
}

impl FromStr for P256dh {
    type Err = InvalidSubscription;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bytes: [u8; 65] = BASE64URL
            .decode(text)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(InvalidSubscription::P256dh)?;
        if bytes[0] != 0x04 || !on_curve(&bytes) {
            return Err(InvalidSubscription::P256dh);
        }
        Ok(Self(bytes))
    }
}

/// True if `point` is a valid P-256 public key: ring validates it in a
/// throwaway key agreement.
fn on_curve(point: &[u8; 65]) -> bool {
    let rng = SystemRandom::new();
    EphemeralPrivateKey::generate(&ECDH_P256, &rng).is_ok_and(|private| {
        agree_ephemeral(private, &UnparsedPublicKey::new(&ECDH_P256, point), |_| ()).is_ok()
    })
}

/// The subscription's auth secret.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthSecret([u8; 16]);

impl fmt::Debug for AuthSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthSecret(…)")
    }
}

impl AuthSecret {
    pub(crate) fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl FromStr for AuthSecret {
    type Err = InvalidSubscription;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        BASE64URL
            .decode(text)
            .ok()
            .and_then(|b| b.try_into().ok())
            .map(Self)
            .ok_or(InvalidSubscription::Auth)
    }
}

/// A browser push subscription.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "RawSubscription", into = "RawSubscription")]
pub struct Subscription {
    /// Where messages go.
    pub endpoint: Endpoint,
    /// What they are encrypted to.
    pub p256dh: P256dh,
    /// Mixed into the encryption keys.
    pub auth: AuthSecret,
    /// What its messages carry: the bare ping (the default) or notices.
    pub payload: PushPayload,
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawSubscription {
    endpoint: String,
    /// Sent by browsers (usually `null`); the bridge learns about expiry
    /// from the push service instead (404/410).
    #[serde(default, skip_serializing)]
    #[expect(dead_code, reason = "accepted for wire compatibility, not used")]
    expiration_time: Option<f64>,
    keys: RawKeys,
    /// Absent (the PWA): the ping. Kept in the store, so a notice
    /// subscription stays one across restarts.
    #[serde(default, skip_serializing_if = "PushPayload::is_ping")]
    payload: PushPayload,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawKeys {
    p256dh: String,
    auth: String,
}

impl TryFrom<RawSubscription> for Subscription {
    type Error = InvalidSubscription;

    fn try_from(raw: RawSubscription) -> Result<Self, Self::Error> {
        Ok(Self {
            endpoint: raw.endpoint.parse()?,
            p256dh: raw.keys.p256dh.parse()?,
            auth: raw.keys.auth.parse()?,
            payload: raw.payload,
        })
    }
}

impl From<Subscription> for RawSubscription {
    fn from(sub: Subscription) -> Self {
        Self {
            endpoint: sub.endpoint.0,
            expiration_time: None,
            keys: RawKeys {
                p256dh: BASE64URL.encode(sub.p256dh.0),
                auth: BASE64URL.encode(sub.auth.0),
            },
            payload: sub.payload,
        }
    }
}

/// `DELETE /v1/push/subscriptions`: which subscription to drop.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unsubscribe {
    /// The subscription's endpoint.
    #[serde(deserialize_with = "endpoint")]
    pub endpoint: Endpoint,
}

fn endpoint<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Endpoint, D::Error> {
    String::deserialize(de)?
        .parse()
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P256DH: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";

    fn parse(json: serde_json::Value) -> Result<Subscription, serde_json::Error> {
        serde_json::from_value(json)
    }

    #[test]
    fn accepts_what_browsers_send() {
        let sub = parse(serde_json::json!({
            "endpoint": "https://fcm.googleapis.com/fcm/send/abc:def",
            "expirationTime": null,
            "keys": {"p256dh": P256DH, "auth": "BTBZMqHH6r4Tts7J_aSIgg=="},
        }))
        .expect("valid");
        assert_eq!(sub.endpoint.origin(), "https://fcm.googleapis.com");
        assert_eq!(sub.endpoint.host(), "fcm.googleapis.com");
        // Round-trips through the stored form.
        let stored = serde_json::to_value(&sub).expect("serializes");
        assert_eq!(sub.payload, PushPayload::Ping, "a browser's is a ping");
        assert!(stored.get("payload").is_none(), "{stored}");
        let again = parse(stored).expect("valid");
        assert_eq!(again, sub);
    }

    #[test]
    fn the_native_app_opts_into_notices() {
        let json = |payload: &str| {
            serde_json::json!({
                "endpoint": "https://push.app.example.org/up/abc",
                "keys": {"p256dh": P256DH, "auth": "BTBZMqHH6r4Tts7J_aSIgg"},
                "payload": payload,
            })
        };
        let sub = parse(json("notice")).expect("valid");
        assert_eq!(sub.payload, PushPayload::Notice);
        let stored = serde_json::to_value(&sub).expect("serializes");
        assert_eq!(stored["payload"], "notice");
        assert_eq!(parse(stored).expect("valid"), sub, "kept across restarts");
        assert!(parse(json("plaintext")).is_err());
    }

    #[test]
    fn refuses_bad_endpoints() {
        for endpoint in [
            "http://push.example.com/x",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "https://localhost/x",
            "https://a.localhost./x",
            "https://user:pw@push.example.com/x",
            "/relative",
            "push.example.com/x",
        ] {
            assert_eq!(
                endpoint.parse::<Endpoint>(),
                Err(InvalidSubscription::Endpoint),
                "{endpoint}"
            );
        }
        let long = format!(
            "https://push.example.com/{}",
            "a".repeat(MAX_ENDPOINT_CHARS)
        );
        assert_eq!(
            long.parse::<Endpoint>(),
            Err(InvalidSubscription::EndpointTooLong)
        );
    }

    #[test]
    fn refuses_ip_addresses_in_any_spelling_and_other_ports() {
        for endpoint in [
            "https://0x7f000001/x",
            "https://2130706433/x",
            "https://127.1/x",
            "https://0177.0.0.1/x",
            "https://10.0.0.1./x",
            "https://fcm.googleapis.com:8443/x",
            "https://fcm.googleapis.com:80/x",
            "https://-bad.example.com/x",
            "https://bad_label.example.com/x",
        ] {
            assert_eq!(
                endpoint.parse::<Endpoint>(),
                Err(InvalidSubscription::Endpoint),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn the_vapid_audience_is_canonical() {
        for endpoint in [
            "https://FCM.GoogleAPIs.com/fcm/send/x",
            "https://fcm.googleapis.com:443/fcm/send/x",
            "https://fcm.googleapis.com./fcm/send/x",
        ] {
            let endpoint: Endpoint = endpoint.parse().expect("valid");
            assert_eq!(endpoint.origin(), "https://fcm.googleapis.com");
            assert_eq!(endpoint.host(), "fcm.googleapis.com");
        }
    }

    #[test]
    fn only_known_push_services_are_permitted() {
        let hosts = PushHosts::default();
        let permits = |hosts: &PushHosts, url: &str| {
            let endpoint: Endpoint = url.parse().expect(url);
            hosts.permits(&endpoint)
        };
        for url in [
            "https://fcm.googleapis.com/fcm/send/x",
            "https://FCM.GOOGLEAPIS.COM/fcm/send/x",
            "https://updates.push.services.mozilla.com/wpush/v2/x",
            "https://web.push.apple.com/x",
            "https://api.push.apple.com/x",
            "https://wns2-par02p.notify.windows.com/w/?token=x",
        ] {
            assert!(permits(&hosts, url), "{url}");
            assert_eq!(hosts.check(&url.parse().expect(url)), Ok(()), "{url}");
        }
        for url in [
            "https://xfcm.googleapis.com/x",
            "https://evil-fcm.googleapis.com.attacker.com/x",
            "https://fcm.googleapis.com.attacker.com/x",
            "https://googleapis.com/x",
            "https://notpush.apple.com/x",
            "https://push.example.com/x",
        ] {
            assert!(!permits(&hosts, url), "{url}");
            assert_eq!(
                hosts.check(&url.parse().expect(url)),
                Err(InvalidSubscription::UnknownPushService),
                "{url}"
            );
        }
        let extra = PushHosts::new(vec!["Push.Example.COM.".parse().expect("host")]);
        assert!(permits(&extra, "https://push.example.com/x"));
        assert!(permits(&extra, "https://eu.push.example.com/x"));
        assert!(permits(&extra, "https://fcm.googleapis.com/x"));
        assert!(!permits(&extra, "https://evilpush.example.com/x"));
        // The self-hosted ntfy server: that exact name only.
        let ntfy =
            PushHosts::default().with_ntfy(Some("push.app.example.org".parse().expect("host")));
        assert!(permits(&ntfy, "https://push.app.example.org/up/x"));
        assert!(permits(&ntfy, "https://fcm.googleapis.com/x"));
        assert!(!permits(&ntfy, "https://eu.push.app.example.org/x"));
        assert!(!permits(&ntfy, "https://app.example.org/x"));
    }

    #[test]
    fn a_configured_push_host_must_be_a_dns_name() {
        assert_eq!(
            "ntfy.example.org".parse::<PushHost>(),
            Ok(PushHost("ntfy.example.org".to_owned()))
        );
        for bad in [
            "",
            "localhost",
            "127.0.0.1",
            "0x7f000001",
            "https://ntfy.example.org",
            "ntfy.example.org:8443",
            "ntfy.example.org/path",
            "*.example.org",
        ] {
            assert!(bad.parse::<PushHost>().is_err(), "{bad}");
        }
    }

    #[test]
    fn refuses_bad_keys_and_unknown_fields() {
        let base = |p256dh: &str, auth: &str| {
            serde_json::json!({
                "endpoint": "https://push.example.com/x",
                "keys": {"p256dh": p256dh, "auth": auth},
            })
        };
        assert!(parse(base(P256DH, "BTBZMqHH6r4Tts7J_aSIgg")).is_ok());
        assert!(parse(base(P256DH, "BTBZMqHH6r4T")).is_err());
        assert!(parse(base(&P256DH[..40], "BTBZMqHH6r4Tts7J_aSIgg")).is_err());
        let compressed = format!("A{}", &P256DH[1..]);
        assert!(parse(base(&compressed, "BTBZMqHH6r4Tts7J_aSIgg")).is_err());
        // Right shape, but y no longer matches x: not on the curve.
        let mut point = BASE64URL.decode(P256DH).expect("base64");
        point[64] ^= 1;
        let off_curve = BASE64URL.encode(&point);
        assert_eq!(
            off_curve.parse::<P256dh>(),
            Err(InvalidSubscription::P256dh)
        );
        assert!(parse(base(&off_curve, "BTBZMqHH6r4Tts7J_aSIgg")).is_err());
        let mut extra = base(P256DH, "BTBZMqHH6r4Tts7J_aSIgg");
        extra["extra"] = serde_json::json!(1);
        assert!(parse(extra).is_err());
        let err = parse(base(P256DH, "not base64!")).expect_err("bad auth");
        assert!(!err.to_string().contains("not base64!"), "{err}");
    }

    #[test]
    fn debug_never_shows_the_full_endpoint_or_keys() {
        let sub = parse(serde_json::json!({
            "endpoint": "https://push.example.com/secret-token",
            "keys": {"p256dh": P256DH, "auth": "BTBZMqHH6r4Tts7J_aSIgg"},
        }))
        .expect("valid");
        let shown = format!("{sub:?}");
        assert!(!shown.contains("secret-token"), "{shown}");
        assert!(!shown.contains("BTBZ"), "{shown}");
    }
}
