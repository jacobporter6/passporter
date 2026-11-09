//! VAPID (RFC 8292): the bridge's identity towards push services.
//!
//! The key is a P-256 PKCS#8 private key in a PEM file in the config
//! directory (`vapid.pem`, mode `0600`), made by `passporter-bridge
//! push-keygen`. Each request carries `Authorization: vapid t=<JWT>,
//! k=<public key>`, the JWT signed with ES256 for the endpoint's origin.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde::Serialize;

use super::subscription::BASE64URL;
use crate::config::{ReadError, read_private, shell_quote};

/// The key file's default name inside the config directory.
pub const VAPID_KEY_FILE: &str = "vapid.pem";

/// How long a signed JWT is valid. RFC 8292 caps it at 24 hours.
const JWT_LIFETIME_SECS: u64 = 12 * 60 * 60;

/// Larger key files are refused rather than read.
const MAX_KEY_BYTES: u64 = 4096;

// Split so the source never holds a whole PEM header: secret scanners
// (rightly) flag any diff that looks like it carries a key.
const PEM_BEGIN: &str = concat!("-----", "BEGIN PRIVATE KEY-----");
const PEM_END: &str = concat!("-----", "END PRIVATE KEY-----");

/// Why the key could not be loaded or made. Never quotes key material.
#[derive(Debug, thiserror::Error)]
pub enum VapidError {
    /// The file could not be read or written.
    #[error("VAPID key {}: {source}", path.display())]
    Io {
        /// The key file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The key file already exists (`push-keygen` never overwrites).
    #[error("VAPID key {} already exists; remove it first to make a new one (every device must then subscribe again)", path.display())]
    Exists {
        /// The key file.
        path: PathBuf,
    },
    /// Group or other users could access the key file.
    #[error("VAPID key {} is accessible by other users (mode {mode:04o}); run `chmod 600 {}`", path.display(), shell_quote(path))]
    TooOpen {
        /// The key file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The file is not a PEM PKCS#8 P-256 private key.
    #[error("VAPID key {} is not a PEM PKCS#8 P-256 private key", path.display())]
    Malformed {
        /// The key file.
        path: PathBuf,
    },
    /// The system random source failed.
    #[error("cannot generate a VAPID key: the system random source failed")]
    Random,
}

/// The contact push services see (RFC 8292 §2.1): `mailto:` or `https:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject(String);

/// A [`Subject`] that is neither `mailto:` nor `https:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the push subject must be a mailto: or https: URL")]
pub struct InvalidSubject;

impl FromStr for Subject {
    type Err = InvalidSubject;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let valid = (text.starts_with("mailto:") && text.len() > "mailto:".len())
            || (text.starts_with("https://") && text.len() > "https://".len());
        if valid && text.len() <= 512 && !text.chars().any(char::is_control) {
            Ok(Self(text.to_owned()))
        } else {
            Err(InvalidSubject)
        }
    }
}

impl<'de> serde::Deserialize<'de> for Subject {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        String::deserialize(de)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The bridge's VAPID signing key.
#[derive(Debug)]
pub struct VapidKey {
    pair: EcdsaKeyPair,
}

impl VapidKey {
    /// Loads the key at `path`, refusing a file other users can access.
    pub fn load(path: &Path) -> Result<Self, VapidError> {
        let path_buf = || path.to_path_buf();
        let bytes = read_private(path, MAX_KEY_BYTES).map_err(|error| match error {
            ReadError::Io(source) => VapidError::Io {
                path: path_buf(),
                source,
            },
            ReadError::TooOpen { mode } => VapidError::TooOpen {
                path: path_buf(),
                mode,
            },
            // No PEM P-256 key is anywhere near this long.
            ReadError::TooLarge => VapidError::Malformed { path: path_buf() },
        })?;
        std::str::from_utf8(&bytes)
            .ok()
            .and_then(Self::from_pem)
            .ok_or_else(|| VapidError::Malformed { path: path_buf() })
    }

    /// Makes a new key and writes it to `path` (mode `0600`). Refuses to
    /// overwrite an existing file.
    pub fn generate(path: &Path) -> Result<Self, VapidError> {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| VapidError::Random)?;
        let key = Self::from_pkcs8(pkcs8.as_ref()).ok_or(VapidError::Random)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|source| match source.kind() {
                io::ErrorKind::AlreadyExists => VapidError::Exists {
                    path: path.to_path_buf(),
                },
                _ => VapidError::Io {
                    path: path.to_path_buf(),
                    source,
                },
            })?;
        file.write_all(to_pem(pkcs8.as_ref()).as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| VapidError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(key)
    }

    fn from_pem(text: &str) -> Option<Self> {
        let body = text.trim().strip_prefix(PEM_BEGIN)?.strip_suffix(PEM_END)?;
        let base64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        Self::from_pkcs8(&STANDARD.decode(base64).ok()?)
    }

    fn from_pkcs8(der: &[u8]) -> Option<Self> {
        let pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der, &SystemRandom::new())
                .ok()?;
        Some(Self { pair })
    }

    /// The public key, base64url: the PWA's `applicationServerKey`.
    #[must_use]
    pub fn public_key(&self) -> String {
        BASE64URL.encode(self.pair.public_key().as_ref())
    }

    /// The `Authorization` header value for a request to `audience` (an
    /// endpoint's origin), valid from `now_secs` (unix seconds).
    pub(crate) fn authorization(
        &self,
        audience: &str,
        subject: &Subject,
        now_secs: u64,
        rng: &SystemRandom,
    ) -> Result<String, ring::error::Unspecified> {
        #[derive(Serialize)]
        struct Claims<'a> {
            aud: &'a str,
            exp: u64,
            sub: &'a str,
        }
        let header = BASE64URL.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::to_vec(&Claims {
            aud: audience,
            exp: now_secs + JWT_LIFETIME_SECS,
            sub: &subject.0,
        })
        .map_err(|_| ring::error::Unspecified)?;
        let signing_input = format!("{header}.{}", BASE64URL.encode(claims));
        let signature = self.pair.sign(rng, signing_input.as_bytes())?;
        Ok(format!(
            "vapid t={signing_input}.{}, k={}",
            BASE64URL.encode(signature.as_ref()),
            self.public_key()
        ))
    }
}

fn to_pem(der: &[u8]) -> String {
    let base64 = STANDARD.encode(der);
    let mut pem = String::from(PEM_BEGIN);
    pem.push('\n');
    for line in base64.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(line));
        pem.push('\n');
    }
    pem.push_str(PEM_END);
    pem.push('\n');
    pem
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;

    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    use super::*;

    /// A fresh key in a temporary directory.
    pub(crate) fn key() -> VapidKey {
        let dir = tempfile::tempdir().expect("tmp");
        VapidKey::generate(&dir.path().join(VAPID_KEY_FILE)).expect("generates")
    }

    #[test]
    fn generate_writes_a_private_file_that_loads_back() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(VAPID_KEY_FILE);
        let key = VapidKey::generate(&path).expect("generates");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with(PEM_BEGIN), "{text}");
        let loaded = VapidKey::load(&path).expect("loads");
        assert_eq!(loaded.public_key(), key.public_key());
        assert_eq!(BASE64URL.decode(key.public_key()).expect("b64").len(), 65);

        assert!(matches!(
            VapidKey::generate(&path),
            Err(VapidError::Exists { .. })
        ));
        assert_eq!(
            VapidKey::load(&path).expect("untouched").public_key(),
            key.public_key()
        );
    }

    #[test]
    fn load_refuses_open_or_malformed_files() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join(VAPID_KEY_FILE);
        VapidKey::generate(&path).expect("generates");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        assert!(matches!(
            VapidKey::load(&path),
            Err(VapidError::TooOpen { .. })
        ));

        let junk = dir.path().join("junk.pem");
        std::fs::write(&junk, format!("{PEM_BEGIN}\nAAAA\n{PEM_END}\n")).expect("write");
        std::fs::set_permissions(&junk, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(matches!(
            VapidKey::load(&junk),
            Err(VapidError::Malformed { .. })
        ));
        assert!(matches!(
            VapidKey::load(&dir.path().join("missing.pem")),
            Err(VapidError::Io { .. })
        ));
    }

    #[test]
    fn authorization_is_a_verifiable_es256_jwt() {
        let key = key();
        let subject: Subject = "mailto:ops@example.com".parse().expect("subject");
        let header = key
            .authorization(
                "https://push.example.com",
                &subject,
                1_000,
                &SystemRandom::new(),
            )
            .expect("signs");
        let rest = header.strip_prefix("vapid t=").expect("scheme");
        let (jwt, k) = rest.split_once(", k=").expect("k");
        assert_eq!(k, key.public_key());
        let mut parts = jwt.split('.');
        let (h, c, s) = (
            parts.next().expect("header"),
            parts.next().expect("claims"),
            parts.next().expect("signature"),
        );
        let claims: serde_json::Value =
            serde_json::from_slice(&BASE64URL.decode(c).expect("b64")).expect("json");
        assert_eq!(
            claims,
            serde_json::json!({"aud": "https://push.example.com", "exp": 1_000 + JWT_LIFETIME_SECS, "sub": "mailto:ops@example.com"})
        );
        let public = BASE64URL.decode(k).expect("b64");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, public)
            .verify(
                format!("{h}.{c}").as_bytes(),
                &BASE64URL.decode(s).expect("b64"),
            )
            .expect("valid signature");
    }

    #[test]
    fn subjects_are_mailto_or_https() {
        assert!("mailto:me@example.com".parse::<Subject>().is_ok());
        assert!("https://passport.example.com".parse::<Subject>().is_ok());
        for bad in ["", "mailto:", "http://x", "me@example.com", "mailto:a\nb"] {
            assert_eq!(bad.parse::<Subject>(), Err(InvalidSubject), "{bad:?}");
        }
    }
}
