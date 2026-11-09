//! Web Push message encryption (RFC 8291) in the `aes128gcm` content coding
//! (RFC 8188): one record, sealed to the subscription's P-256 key and auth
//! secret with a fresh sender key per message.

use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};
use ring::hkdf::{self, HKDF_SHA256, KeyType, Salt};
use ring::rand::{SecureRandom, SystemRandom};

use super::subscription::{AuthSecret, P256dh};

/// Record size advertised in the header. A ping is one short record.
const RECORD_SIZE: u32 = 4096;
/// Length of an uncompressed P-256 point.
const POINT_LEN: usize = 65;
/// Header: salt (16) + record size (4) + key id length (1) + key id (65).
const HEADER_LEN: usize = 16 + 4 + 1 + POINT_LEN;
/// Largest plaintext that fits one record (delimiter and GCM tag aside).
pub(crate) const MAX_PLAINTEXT: usize = RECORD_SIZE as usize - 1 - 16;
/// Largest plaintext whose whole message body (header, record, delimiter
/// and tag) stays within the 4096 bytes push services accept (RFC 8030
/// §7.2 lets them refuse more).
pub(crate) const MAX_PUSH_PLAINTEXT: usize = 4096 - HEADER_LEN - 1 - 16;

/// Why a message could not be encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EncryptError {
    /// The subscription's public key is not a valid P-256 point.
    #[error("the subscription's p256dh key is not a valid P-256 point")]
    InvalidKey,
    /// The plaintext does not fit one record.
    #[error("the push payload is larger than one record")]
    TooLarge,
    /// The system random source or a primitive failed.
    #[error("the crypto backend failed")]
    Crypto,
}

impl From<ring::error::Unspecified> for EncryptError {
    fn from(_: ring::error::Unspecified) -> Self {
        Self::Crypto
    }
}

/// Encrypts `plaintext` to the subscription keys `ua_public` / `auth`.
pub(crate) fn encrypt(
    ua_public: &P256dh,
    auth: &AuthSecret,
    plaintext: &[u8],
    rng: &SystemRandom,
) -> Result<Vec<u8>, EncryptError> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(EncryptError::TooLarge);
    }
    let private = EphemeralPrivateKey::generate(&ECDH_P256, rng)?;
    let public = private.compute_public_key()?;
    let mut as_public = [0u8; POINT_LEN];
    as_public.copy_from_slice(public.as_ref());
    let mut salt = [0u8; 16];
    rng.fill(&mut salt)?;
    let peer = UnparsedPublicKey::new(&ECDH_P256, ua_public.as_bytes());
    agree_ephemeral(private, &peer, |ecdh_secret| {
        seal(
            &Keys {
                ecdh_secret,
                ua_public: ua_public.as_bytes(),
                as_public: &as_public,
                auth: auth.as_bytes(),
                salt: &salt,
            },
            plaintext,
        )
    })
    .map_err(|_| EncryptError::InvalidKey)?
}

/// The inputs of the key schedule, once the ECDH secret is known.
struct Keys<'a> {
    ecdh_secret: &'a [u8],
    ua_public: &'a [u8; POINT_LEN],
    as_public: &'a [u8; POINT_LEN],
    auth: &'a [u8; 16],
    salt: &'a [u8; 16],
}

/// The content encryption key and nonce (RFC 8291 §3.4, RFC 8188 §2.2).
fn content_keys(keys: &Keys<'_>) -> Result<([u8; 16], [u8; 12]), EncryptError> {
    // Mix the auth secret and both public keys into the IKM.
    let mut key_info = Vec::with_capacity(14 + 2 * POINT_LEN);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(keys.ua_public);
    key_info.extend_from_slice(keys.as_public);
    let prk_key = Salt::new(HKDF_SHA256, keys.auth).extract(keys.ecdh_secret);
    let ikm: [u8; 32] = expand(&prk_key, &key_info)?;
    let prk = Salt::new(HKDF_SHA256, keys.salt).extract(&ikm);
    let cek = expand(&prk, b"Content-Encoding: aes128gcm\0")?;
    let nonce = expand(&prk, b"Content-Encoding: nonce\0")?;
    Ok((cek, nonce))
}

/// The deterministic half of [`encrypt`]: key schedule, header and record.
fn seal(keys: &Keys<'_>, plaintext: &[u8]) -> Result<Vec<u8>, EncryptError> {
    let (cek, nonce) = content_keys(keys)?;
    let mut body = Vec::with_capacity(HEADER_LEN + plaintext.len() + 1 + 16);
    body.extend_from_slice(keys.salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(u8::try_from(POINT_LEN).map_err(|_| EncryptError::Crypto)?);
    body.extend_from_slice(keys.as_public);
    let mut record = Vec::with_capacity(plaintext.len() + 1 + 16);
    record.extend_from_slice(plaintext);
    // The last (and only) record's padding delimiter.
    record.push(0x02);
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek)?);
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::empty(),
        &mut record,
    )?;
    body.extend_from_slice(&record);
    Ok(body)
}

/// An HKDF output length, as `ring` wants it.
struct Len(usize);

impl KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand<const N: usize>(prk: &hkdf::Prk, info: &[u8]) -> Result<[u8; N], EncryptError> {
    let mut out = [0u8; N];
    prk.expand(&[info], Len(N))?.fill(&mut out)?;
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use base64::Engine;
    use ring::agreement::PublicKey;

    use super::*;
    use crate::push::subscription::BASE64URL;

    fn b64(text: &str) -> Vec<u8> {
        BASE64URL.decode(text).expect("base64url")
    }

    fn array<const N: usize>(text: &str) -> [u8; N] {
        b64(text).try_into().expect("length")
    }

    /// RFC 8291 Appendix A, from the ECDH secret on (`ring` cannot import
    /// the example's fixed private key).
    #[test]
    fn matches_the_rfc_8291_example() {
        let body = seal(
            &Keys {
                ecdh_secret: &b64("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
                ua_public: &array(
                    "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
                ),
                as_public: &array(
                    "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
                ),
                auth: &array("BTBZMqHH6r4Tts7J_aSIgg"),
                salt: &array("DGv6ra1nlYgDCS1FRnbzlw"),
            },
            b"When I grow up, I want to be a watermelon",
        )
        .expect("seals");
        let (header, ciphertext) = body.split_at(HEADER_LEN);
        assert_eq!(
            BASE64URL.encode(header),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
        assert_eq!(
            BASE64URL.encode(ciphertext),
            "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ"
        );
    }

    /// A browser's side of a subscription, for tests: decrypts what
    /// [`encrypt`] produced. Single use (its key is ephemeral).
    pub(crate) struct Receiver {
        private: EphemeralPrivateKey,
        public: PublicKey,
        pub(crate) auth: [u8; 16],
    }

    impl Receiver {
        pub(crate) fn new() -> Self {
            let rng = SystemRandom::new();
            let private = EphemeralPrivateKey::generate(&ECDH_P256, &rng).expect("key");
            let public = private.compute_public_key().expect("public");
            let mut auth = [0u8; 16];
            rng.fill(&mut auth).expect("rng");
            Self {
                private,
                public,
                auth,
            }
        }

        pub(crate) fn p256dh(&self) -> String {
            BASE64URL.encode(self.public.as_ref())
        }

        pub(crate) fn auth(&self) -> String {
            BASE64URL.encode(self.auth)
        }

        /// The plaintext of `body`, checking the header on the way.
        pub(crate) fn decrypt(self, body: &[u8]) -> Vec<u8> {
            let (header, record) = body.split_at(HEADER_LEN);
            let salt: [u8; 16] = header[..16].try_into().expect("salt");
            assert_eq!(&header[16..20], &RECORD_SIZE.to_be_bytes());
            assert_eq!(usize::from(header[20]), POINT_LEN);
            let as_public: [u8; POINT_LEN] = header[21..].try_into().expect("key id");
            let ua_public: [u8; POINT_LEN] = self.public.as_ref().try_into().expect("point");
            let auth = self.auth;
            let peer = UnparsedPublicKey::new(&ECDH_P256, as_public);
            agree_ephemeral(self.private, &peer, |ecdh_secret| {
                let keys = Keys {
                    ecdh_secret,
                    ua_public: &ua_public,
                    as_public: &as_public,
                    auth: &auth,
                    salt: &salt,
                };
                let (cek, nonce) = content_keys(&keys).expect("keys");
                let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).expect("key"));
                let mut record = record.to_vec();
                let plain = key
                    .open_in_place(
                        Nonce::assume_unique_for_key(nonce),
                        Aad::empty(),
                        &mut record,
                    )
                    .expect("authentic");
                let (last, text) = plain.split_last().expect("delimiter");
                assert_eq!(*last, 0x02, "single final record");
                text.to_vec()
            })
            .expect("agreement")
        }
    }

    #[test]
    fn round_trips_through_a_receiver() {
        let receiver = Receiver::new();
        let ua: P256dh = receiver.p256dh().parse().expect("p256dh");
        let auth: AuthSecret = receiver.auth().parse().expect("auth");
        let body =
            encrypt(&ua, &auth, b"{\"ping\":\"x\"}", &SystemRandom::new()).expect("encrypts");
        assert_eq!(receiver.decrypt(&body), b"{\"ping\":\"x\"}");
    }

    #[test]
    fn refuses_oversized_payloads() {
        // (An invalid point never gets this far: `P256dh` refuses it.)
        let mut bogus = [0u8; POINT_LEN];
        bogus[0] = 0x04;
        assert!(BASE64URL.encode(bogus).parse::<P256dh>().is_err());
        let auth: AuthSecret = BASE64URL.encode([1u8; 16]).parse().expect("auth");
        let rng = SystemRandom::new();
        let receiver = Receiver::new();
        let ua: P256dh = receiver.p256dh().parse().expect("p256dh");
        let big = vec![0u8; MAX_PLAINTEXT + 1];
        assert_eq!(encrypt(&ua, &auth, &big, &rng), Err(EncryptError::TooLarge));
    }
}
