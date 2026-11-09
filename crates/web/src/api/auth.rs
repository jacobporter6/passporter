//! Bearer-token handling, behind a small trait, and the device-auth calls
//! that mint tokens.
//!
//! The client only needs two things from auth: the current token, and a way
//! to say "the bridge rejected *this* token". [`TokenSource`] is exactly
//! that; `crate::session` implements it with device sessions (or the
//! pasted MVP token).
//!
//! Device auth (design §8): [`pair_device`] registers this device's P-256
//! public key with a pairing code, and [`sign_in`] trades a signature over
//! the device id and a fresh server challenge for a short-lived
//! [`SessionGrant`]. Signing
//! goes through [`DeviceSigner`], so the flow is tested on the host and the
//! key itself stays a non-extractable `WebCrypto` key in the browser.

use core::cell::RefCell;
use core::fmt;
use core::future::Future;

use protocol::{
    ChallengeRequest, ChallengeResponse, DEVICE_UNKNOWN_STATUS, DeviceId, PairRequest,
    PairResponse, SessionGrant, SessionRequest, Signature, signed_message,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::Transport;
use super::request::{HttpRequest, Method};
use super::response::{ApiError, HttpResponse};

/// Maximum length of a bearer token, in bytes. Generous for a signed token,
/// small enough that a corrupted store can't make every request huge.
pub const MAX_TOKEN_LEN: usize = 4096;

/// A bearer token, validated so it can go into an `Authorization` header
/// as-is: 1..=[`MAX_TOKEN_LEN`] bytes of the RFC 6750 `b64token` charset
/// (ASCII letters, digits and `-._~+/`, with optional trailing `=`). No
/// spaces, control characters or line breaks, so no header injection.
///
/// `Debug` is redacted: tokens never reach logs.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    /// Validates and wraps `token`.
    pub fn new(token: impl Into<String>) -> Result<Self, InvalidToken> {
        let token = token.into();
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            return Err(InvalidToken);
        }
        let body = token.trim_end_matches('=');
        let charset_ok = !body.is_empty()
            && body.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
            });
        if charset_ok {
            Ok(Self(token))
        } else {
            Err(InvalidToken)
        }
    }

    /// The raw token, for the stream's first message and for storage.
    /// Never log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The `Authorization` header value: `Bearer <token>`.
    #[must_use]
    pub fn header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(<redacted>)")
    }
}

/// Turns what the user pasted (from `deploy/bridge/setup.sh`) into a token.
///
/// Forgiving about the wrapping, strict about the token: surrounding
/// whitespace and quotes, a `Bearer ` prefix and a shell-style `NAME=`
/// prefix (e.g. `PASSPORTER_TOKEN=…`) are removed; what remains must be a
/// valid [`BearerToken`].
pub fn parse_pasted_token(input: &str) -> Result<BearerToken, InvalidToken> {
    if input.len() > MAX_TOKEN_LEN * 2 {
        return Err(InvalidToken);
    }
    let mut token = input.trim().trim_matches(['"', '\'', '`']).trim();
    if let Some(rest) = token
        .strip_prefix("Bearer ")
        .or_else(|| token.strip_prefix("bearer "))
    {
        token = rest.trim();
    }
    if let Some((name, value)) = token.split_once('=')
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !value.is_empty()
    {
        token = value.trim().trim_matches(['"', '\'', '`']);
    }
    BearerToken::new(token)
}

/// A token was empty, too long or outside the `b64token` charset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("bearer token is empty, too long or contains disallowed characters")]
pub struct InvalidToken;

/// Where the API client gets its bearer token.
pub trait TokenSource {
    /// The token to send now, or `None` when the device is not paired (the
    /// request then goes out without `Authorization` and the bridge answers
    /// 401).
    fn current(&self) -> Option<BearerToken>;

    /// Called when the bridge answered 401 to a request sent with
    /// `rejected`. Implementations drop or refresh the token, but only if
    /// `rejected` is still the current one: a late 401 for a request sent
    /// before a refresh must not wipe the newer token. The default does
    /// nothing.
    fn unauthorized(&self, rejected: &BearerToken) {
        let _ = rejected;
    }
}

/// No credentials: the state of an unpaired device.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unpaired;

impl TokenSource for Unpaired {
    fn current(&self) -> Option<BearerToken> {
        None
    }
}

/// A token held in memory and forgotten on the first 401. A stand-in until
/// Device pairing's signed, refreshing tokens; never persisted.
#[derive(Debug, Default)]
pub struct MemoryToken(RefCell<Option<BearerToken>>);

impl MemoryToken {
    /// Holds `token` until the bridge rejects it.
    #[must_use]
    pub fn new(token: BearerToken) -> Self {
        Self(RefCell::new(Some(token)))
    }

    /// Holds `token`, if any, until the bridge rejects it.
    #[must_use]
    pub fn maybe(token: Option<BearerToken>) -> Self {
        Self(RefCell::new(token))
    }
}

impl TokenSource for MemoryToken {
    fn current(&self) -> Option<BearerToken> {
        self.0.borrow().clone()
    }

    fn unauthorized(&self, rejected: &BearerToken) {
        let mut current = self.0.borrow_mut();
        if current.as_ref() == Some(rejected) {
            *current = None;
        }
    }
}

/// Why pairing or signing in failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceAuthError {
    /// The bridge said no (401): a wrong, expired or used code, challenge
    /// or signature. Says nothing about the device itself.
    #[error("the bridge refused the request")]
    Refused,
    /// The bridge does not know this device (410): never paired, or
    /// revoked. The only error after which the device key is useless.
    #[error("the bridge no longer knows this device; pair it again")]
    Forgotten,
    /// Too many attempts (429); wait a minute.
    #[error("too many attempts; wait a minute and try again")]
    RateLimited,
    /// The bridge has as many devices as it allows (409).
    #[error("the bridge has too many paired devices; revoke one with `passporter devices revoke`")]
    TooManyDevices,
    /// The device key could not sign.
    #[error("this device could not sign: {0}")]
    Signing(String),
    /// Anything else: no response, an unexpected status or body.
    #[error(transparent)]
    Api(#[from] ApiError),
}

impl DeviceAuthError {
    /// Whether trying again later may work without pairing again.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited => true,
            Self::Api(error) => error.is_retryable(),
            Self::Refused | Self::Forgotten | Self::TooManyDevices | Self::Signing(_) => false,
        }
    }
}

/// A paired device's key: its id, and a way to sign with it.
pub trait DeviceSigner {
    /// The device's id.
    fn device_id(&self) -> &DeviceId;

    /// An ECDSA P-256 / SHA-256 signature over `message`, as the 64 raw
    /// bytes `r || s` (what `WebCrypto` produces).
    fn sign(&self, message: &[u8]) -> impl Future<Output = Result<Vec<u8>, String>>;
}

/// `POST /v1/auth/pair`: registers this device's public key.
pub async fn pair_device<T: Transport>(
    transport: &T,
    request: &PairRequest,
) -> Result<PairResponse, DeviceAuthError> {
    post(transport, "/v1/auth/pair", request).await
}

/// Signs in as `signer`'s device: asks for a challenge, signs it, and trades
/// the signature for a session. Refreshing is calling this again.
///
/// A 401 for the signature usually means the challenge is gone (the bridge
/// restarted mid-sign-in, say), so it is retried once with a fresh one.
pub async fn sign_in<T: Transport, S: DeviceSigner>(
    transport: &T,
    signer: &S,
) -> Result<SessionGrant, DeviceAuthError> {
    match sign_in_once(transport, signer).await {
        Err(DeviceAuthError::Refused) => sign_in_once(transport, signer).await,
        outcome => outcome,
    }
}

/// One challenge, signature and session exchange.
async fn sign_in_once<T: Transport, S: DeviceSigner>(
    transport: &T,
    signer: &S,
) -> Result<SessionGrant, DeviceAuthError> {
    let device_id = signer.device_id().clone();
    let ChallengeResponse { challenge, .. } = post(
        transport,
        "/v1/auth/challenge",
        &ChallengeRequest {
            device_id: device_id.clone(),
        },
    )
    .await?;
    let raw = signer
        .sign(&signed_message(&device_id, &challenge))
        .await
        .map_err(DeviceAuthError::Signing)?;
    let signature =
        Signature::new(hex(&raw)).map_err(|e| DeviceAuthError::Signing(e.to_string()))?;
    post(
        transport,
        "/v1/auth/session",
        &SessionRequest {
            device_id,
            challenge,
            signature,
            step_up: false,
            scope: protocol::Scope::App,
        },
    )
    .await
}

/// The bearer token in `grant`, validated for the `Authorization` header.
pub fn grant_token(grant: &SessionGrant) -> Result<BearerToken, DeviceAuthError> {
    BearerToken::new(grant.token.clone())
        .map_err(|_| DeviceAuthError::Api(ApiError::Decode("session token".to_owned())))
}

/// One unauthenticated JSON `POST` to `/v1/auth/*`.
async fn post<T: Transport, B: Serialize, R: DeserializeOwned>(
    transport: &T,
    path: &str,
    body: &B,
) -> Result<R, DeviceAuthError> {
    let body = serde_json::to_string(body).map_err(|e| ApiError::Encode(e.to_string()))?;
    let response = transport
        .send(HttpRequest {
            method: Method::Post,
            path: path.to_owned(),
            headers: vec![("Content-Type", "application/json")],
            auth: None,
            body: Some(body),
        })
        .await?;
    interpret(&response)
}

fn interpret<R: DeserializeOwned>(response: &HttpResponse) -> Result<R, DeviceAuthError> {
    match response.status {
        401 => Err(DeviceAuthError::Refused),
        DEVICE_UNKNOWN_STATUS => Err(DeviceAuthError::Forgotten),
        409 => Err(DeviceAuthError::TooManyDevices),
        429 => Err(DeviceAuthError::RateLimited),
        200..=299 => {
            let body = response
                .body
                .as_deref()
                .ok_or_else(|| ApiError::Transport("response body lost".to_owned()))?;
            serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()).into())
        }
        status => Err(ApiError::Status { status }.into()),
    }
}

/// Lowercase hex, as device-auth binary values travel.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 0xf)]])
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use super::*;

    /// Answers each request with the next canned response, recording it.
    #[derive(Default)]
    struct Script {
        responses: RefCell<Vec<HttpResponse>>,
        sent: RefCell<Vec<HttpRequest>>,
    }

    impl Script {
        fn new(responses: &[(u16, &str)]) -> Self {
            Self {
                responses: RefCell::new(
                    responses
                        .iter()
                        .map(|(status, body)| HttpResponse::new(*status, *body))
                        .collect(),
                ),
                sent: RefCell::default(),
            }
        }
    }

    impl Transport for &Script {
        fn send(
            &self,
            request: HttpRequest,
        ) -> impl Future<Output = Result<HttpResponse, ApiError>> {
            self.sent.borrow_mut().push(request);
            core::future::ready(Ok(self.responses.borrow_mut().remove(0)))
        }
    }

    struct FixedSigner(DeviceId, RefCell<Vec<Vec<u8>>>);

    impl DeviceSigner for FixedSigner {
        fn device_id(&self) -> &DeviceId {
            &self.0
        }

        fn sign(&self, message: &[u8]) -> impl Future<Output = Result<Vec<u8>, String>> {
            self.1.borrow_mut().push(message.to_vec());
            core::future::ready(Ok(vec![0xab; 64]))
        }
    }

    fn now<F: Future>(future: F) -> F::Output {
        match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("fake future was pending"),
        }
    }

    fn device() -> DeviceId {
        DeviceId::new("dev-0123456789abcdef").unwrap()
    }

    #[test]
    fn sign_in_signs_the_challenge_and_returns_the_grant() {
        let challenge = "0f".repeat(32);
        let script = Script::new(&[
            (
                200,
                &format!(r#"{{"challenge":"{challenge}","expiresInSecs":60}}"#),
            ),
            (200, r#"{"token":"abc123","expiresInSecs":900}"#),
        ]);
        let signer = FixedSigner(device(), RefCell::default());
        let grant = now(sign_in(&&script, &signer)).unwrap();
        assert_eq!(grant_token(&grant).unwrap().expose(), "abc123");
        assert_eq!(grant.expires_in_secs, 900);

        let messages = signer.1.borrow();
        assert_eq!(
            messages[0],
            format!("passporter-session-v1:dev-0123456789abcdef:{challenge}").into_bytes()
        );
        let sent = script.sent.borrow();
        assert_eq!(sent[0].path, "/v1/auth/challenge");
        assert_eq!(sent[0].auth, None);
        assert_eq!(sent[1].path, "/v1/auth/session");
        let body = sent[1].body.as_deref().unwrap();
        assert!(body.contains(&"ab".repeat(64)), "{body}");
        assert!(body.contains(&challenge), "{body}");
        assert!(!body.contains("stepUp"), "{body}");
    }

    fn challenge_json(byte: &str) -> String {
        format!(
            r#"{{"challenge":"{}","expiresInSecs":60}}"#,
            byte.repeat(32)
        )
    }

    #[test]
    fn a_refused_signature_is_retried_once_with_a_fresh_challenge() {
        let script = Script::new(&[
            (200, &challenge_json("01")),
            (401, ""),
            (200, &challenge_json("02")),
            (200, r#"{"token":"abc123","expiresInSecs":900}"#),
        ]);
        let signer = FixedSigner(device(), RefCell::default());
        let grant = now(sign_in(&&script, &signer)).unwrap();
        assert_eq!(grant.token, "abc123");
        {
            let messages = signer.1.borrow();
            assert_eq!(messages.len(), 2);
            assert_ne!(
                messages[0], messages[1],
                "the retry signs the new challenge"
            );
        }
        assert_eq!(script.sent.borrow().len(), 4);

        // Refused twice: give up, but as `Refused`, which keeps the key.
        let script = Script::new(&[
            (200, &challenge_json("01")),
            (401, ""),
            (200, &challenge_json("02")),
            (401, ""),
        ]);
        assert_eq!(
            now(sign_in(&&script, &signer)),
            Err(DeviceAuthError::Refused)
        );
    }

    #[test]
    fn a_gone_device_is_not_retried() {
        let script = Script::new(&[(200, &challenge_json("01")), (410, "")]);
        let signer = FixedSigner(device(), RefCell::default());
        assert_eq!(
            now(sign_in(&&script, &signer)),
            Err(DeviceAuthError::Forgotten)
        );
        assert_eq!(script.sent.borrow().len(), 2);
    }

    #[test]
    fn refusals_map_to_typed_errors() {
        let signer = FixedSigner(device(), RefCell::default());
        for (status, want) in [
            (410, DeviceAuthError::Forgotten),
            (429, DeviceAuthError::RateLimited),
            (409, DeviceAuthError::TooManyDevices),
            (503, DeviceAuthError::Api(ApiError::Status { status: 503 })),
        ] {
            let script = Script::new(&[(status, "")]);
            assert_eq!(now(sign_in(&&script, &signer)), Err(want.clone()));
        }
        assert!(DeviceAuthError::RateLimited.is_retryable());
        assert!(!DeviceAuthError::Refused.is_retryable());
        assert!(!DeviceAuthError::Forgotten.is_retryable());
        assert!(
            signer.1.borrow().is_empty(),
            "nothing signed without a challenge"
        );
    }

    #[test]
    fn pairing_posts_the_public_key() {
        let script = Script::new(&[(200, r#"{"deviceId":"dev-0123456789abcdef"}"#)]);
        let request = PairRequest {
            code: protocol::PairingCode::new("ABCD2345").unwrap(),
            public_key: protocol::PublicKey::new(format!("04{}", "11".repeat(64))).unwrap(),
            name: None,
        };
        let paired = now(pair_device(&&script, &request)).unwrap();
        assert_eq!(paired.device_id, device());
        let sent = script.sent.borrow();
        assert_eq!(sent[0].method, Method::Post);
        assert_eq!(sent[0].headers, vec![("Content-Type", "application/json")]);
        assert!(
            sent[0]
                .body
                .as_deref()
                .unwrap()
                .contains("\"code\":\"ABCD2345\"")
        );
    }

    #[test]
    fn accepts_b64token_shapes() {
        for ok in ["abc", "a.b.c", "A-Z_0~9+/", "dG9rZW4=", "x=="] {
            assert!(BearerToken::new(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_header_injection_and_junk() {
        let long = "a".repeat(MAX_TOKEN_LEN + 1);
        for bad in [
            "",
            "=",
            "a b",
            "a\r\nX-Evil: 1",
            "tok\u{e9}n",
            "a=b",
            long.as_str(),
        ] {
            assert_eq!(BearerToken::new(bad), Err(InvalidToken), "{bad:?}");
        }
    }

    #[test]
    fn header_value_and_redacted_debug() {
        let token = BearerToken::new("s3cret").unwrap();
        assert_eq!(token.header_value(), "Bearer s3cret");
        assert!(!format!("{token:?}").contains("s3cret"));
    }

    #[test]
    fn memory_token_is_dropped_on_unauthorized() {
        let token = BearerToken::new("t").unwrap();
        let source = MemoryToken::new(token.clone());
        assert!(source.current().is_some());
        source.unauthorized(&token);
        assert_eq!(source.current(), None);
        assert_eq!(Unpaired.current(), None);
    }

    #[test]
    fn pasted_tokens_are_unwrapped() {
        let want = BearerToken::new("abc.DEF-123_~+/=").unwrap();
        for pasted in [
            "abc.DEF-123_~+/=",
            "  abc.DEF-123_~+/=\n",
            "\"abc.DEF-123_~+/=\"",
            "Bearer abc.DEF-123_~+/=",
            "PASSPORTER_TOKEN=abc.DEF-123_~+/=",
            "PASSPORTER_TOKEN='abc.DEF-123_~+/='",
        ] {
            assert_eq!(parse_pasted_token(pasted), Ok(want.clone()), "{pasted:?}");
        }
    }

    #[test]
    fn pasted_junk_is_rejected() {
        for pasted in ["", "   ", "two words", "lower=abc", "a\nb"] {
            assert_eq!(parse_pasted_token(pasted), Err(InvalidToken), "{pasted:?}");
        }
    }

    #[test]
    fn late_401_for_an_old_token_keeps_the_new_one() {
        let old = BearerToken::new("old").unwrap();
        let new = BearerToken::new("new").unwrap();
        let source = MemoryToken::new(new.clone());
        source.unauthorized(&old);
        assert_eq!(source.current(), Some(new));
    }
}
