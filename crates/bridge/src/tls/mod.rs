//! HTTPS served by the bridge itself (design §3, §8).
//!
//! With `[tls]` in the config, the bridge presents a leaf certificate (say,
//! from a private CA) to a reverse proxy on another host (the "edge"),
//! which trusts only that CA and proxies `https://passporter.example.com`
//! here. Without `[tls]` the bridge stays on loopback behind a local proxy.
//! rustls with the ring provider, the stack push already uses.
//!
//! - [`Leaf`]: a chain and its key, read from PEM files and checked to
//!   match. The key file must be private (`0600`), as the VAPID key must.
//!   [`Leaf::check`] adds what the edge will check: the leaf covers
//!   `tls.name` and is inside its validity period.
//! - [`CertStore`]: the leaf being served. Only a leaf that passes both is
//!   ever served; a reload that fails keeps the old one.
//! - [`spawn_reloader`] and its [`Poller`]: reload on SIGHUP, and when
//!   either file's content changes and then holds still for one poll, so a
//!   renewed leaf needs no restart and a half-installed renewal is not
//!   mistaken for a broken one. They also warn as the served leaf nears
//!   expiry.
//! - [`TlsListener`]: an axum listener that hands HTTP/1.1 (and so
//!   WebSocket upgrades) to the router only after the TLS handshake.

mod listener;
mod reload;
mod x509;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::SystemTime;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, DnsName, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{InconsistentKeys, ServerConfig};
use sha2::{Digest, Sha256};

pub use self::listener::{
    HANDSHAKE_TIMEOUT, HttpsListener, MAX_HANDSHAKES, MAX_HANDSHAKES_PER_PEER, TlsListener,
};
pub use self::reload::{EXPIRY_WARNING, Poller, RELOAD_POLL, spawn_reloader};
pub use self::x509::{CertTime, Validity};
use crate::config::{ReadError, TlsConfig, read_bounded, read_private, shell_quote};

/// Larger chain files are refused rather than read.
const MAX_CHAIN_BYTES: u64 = 64 * 1024;

/// Larger key files are refused rather than read.
const MAX_KEY_BYTES: u64 = 16 * 1024;

/// Why the leaf could not be loaded or served. Never quotes key material.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A file could not be read.
    #[error("TLS file {}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A file is implausibly large.
    #[error("TLS file {} is larger than {max} bytes", path.display())]
    TooLarge {
        /// The file.
        path: PathBuf,
        /// The limit.
        max: u64,
    },
    /// Group or other users could access the key file.
    #[error("TLS key {} is accessible by other users (mode {mode:04o}); run `chmod 600 {}`", path.display(), shell_quote(path))]
    TooOpen {
        /// The key file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The chain file holds no PEM certificate, or a broken one.
    #[error("TLS certificate {} is not a PEM certificate chain (leaf first)", path.display())]
    BadChain {
        /// The chain file.
        path: PathBuf,
    },
    /// The key file holds no usable PEM private key.
    #[error("TLS key {} is not a PEM private key rustls can use (PKCS#8, SEC1 or PKCS#1; ECDSA P-256/P-384, Ed25519 or RSA)", path.display())]
    BadKey {
        /// The key file.
        path: PathBuf,
    },
    /// The key is not the leaf's.
    #[error("TLS key {} does not match the leaf in {}", key.display(), cert.display())]
    KeyMismatch {
        /// The chain file.
        cert: PathBuf,
        /// The key file.
        key: PathBuf,
    },
    /// The leaf's subject alternative names do not cover `tls.name`.
    #[error("TLS leaf {} is not valid for {name} (no matching DNS SAN), so the edge would refuse it", cert.display())]
    WrongName {
        /// The chain file.
        cert: PathBuf,
        /// The name the edge verifies.
        name: String,
    },
    /// The leaf's validity period does not parse.
    #[error("cannot read the validity period of the TLS leaf in {}", cert.display())]
    NoValidity {
        /// The chain file.
        cert: PathBuf,
    },
    /// The leaf is not valid yet (clock skew on the CA, or on this host).
    #[error("TLS leaf {} is not valid until {from}", cert.display())]
    NotYetValid {
        /// The chain file.
        cert: PathBuf,
        /// Its `notBefore`.
        from: CertTime,
    },
    /// The leaf has expired.
    #[error("TLS leaf {} expired at {at}", cert.display())]
    Expired {
        /// The chain file.
        cert: PathBuf,
        /// Its `notAfter`.
        at: CertTime,
    },
    /// rustls refused to build a server config (no usable protocol version).
    #[error("building the TLS server config: {0}")]
    Config(#[source] rustls::Error),
}

impl TlsError {
    /// Whether the same files could load later as they are: the failure
    /// comes from outside their content (permissions, a read error, the
    /// clock). A reload that fails so is retried at every poll; one that
    /// fails on content waits for the files to change.
    fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Io { .. }
                | Self::TooOpen { .. }
                | Self::TooLarge { .. }
                | Self::NotYetValid { .. }
                | Self::Expired { .. }
        )
    }
}

/// A certificate chain and its matching private key.
#[derive(Debug, Clone)]
pub struct Leaf {
    key: Arc<CertifiedKey>,
}

impl Leaf {
    /// Reads the chain at `tls.cert` and the key at `tls.key` (refusing a
    /// key file group or other users can access) and checks that the key
    /// belongs to the chain's first certificate. See [`Self::check`] for
    /// whether the edge would accept it.
    pub fn load(tls: &TlsConfig) -> Result<Self, TlsError> {
        Self::parse(tls, &LeafFiles::read(tls)?)
    }

    /// [`Self::load`], then [`Self::check`] at `now`: a leaf fit to serve,
    /// with its validity period.
    pub fn load_servable(tls: &TlsConfig, now: SystemTime) -> Result<(Self, Validity), TlsError> {
        let leaf = Self::load(tls)?;
        let validity = leaf.check(tls, now)?;
        Ok((leaf, validity))
    }

    fn parse(tls: &TlsConfig, files: &LeafFiles) -> Result<Self, TlsError> {
        let chain = CertificateDer::pem_slice_iter(&files.chain)
            .collect::<Result<Vec<_>, _>>()
            .ok()
            .filter(|chain| !chain.is_empty())
            .ok_or_else(|| TlsError::BadChain {
                path: tls.cert.clone(),
            })?;
        let key = PrivateKeyDer::from_pem_slice(&files.key).map_err(|_| TlsError::BadKey {
            path: tls.key.clone(),
        })?;
        let certified =
            CertifiedKey::from_der(chain, key, &provider()).map_err(|error| match error {
                rustls::Error::InconsistentKeys(InconsistentKeys::KeyMismatch) => {
                    TlsError::KeyMismatch {
                        cert: tls.cert.clone(),
                        key: tls.key.clone(),
                    }
                }
                rustls::Error::InvalidCertificate(_) => TlsError::BadChain {
                    path: tls.cert.clone(),
                },
                _ => TlsError::BadKey {
                    path: tls.key.clone(),
                },
            })?;
        Ok(Self {
            key: Arc::new(certified),
        })
    }

    /// What the edge checks besides the CA: the leaf covers `tls.name`
    /// and `now` is inside its validity period, which is returned.
    pub fn check(&self, tls: &TlsConfig, now: SystemTime) -> Result<Validity, TlsError> {
        let cert = || tls.cert.clone();
        if !self.is_valid_for(&tls.name) {
            return Err(TlsError::WrongName {
                cert: cert(),
                name: tls.name.as_ref().to_owned(),
            });
        }
        let validity = self
            .validity()
            .ok_or_else(|| TlsError::NoValidity { cert: cert() })?;
        if validity.not_before.at > now {
            return Err(TlsError::NotYetValid {
                cert: cert(),
                from: validity.not_before,
            });
        }
        if validity.not_after.at < now {
            return Err(TlsError::Expired {
                cert: cert(),
                at: validity.not_after,
            });
        }
        Ok(validity)
    }

    /// The leaf itself (`parse` never accepts an empty chain).
    fn end_entity(&self) -> Option<&CertificateDer<'static>> {
        self.key.cert.first()
    }

    /// The leaf's validity period, if it parses.
    #[must_use]
    pub fn validity(&self) -> Option<Validity> {
        self.end_entity().and_then(|cert| x509::validity(cert))
    }

    /// Whether the leaf's subject alternative names cover `name`.
    #[must_use]
    pub fn is_valid_for(&self, name: &DnsName<'_>) -> bool {
        let name = ServerName::DnsName(name.to_owned());
        self.end_entity()
            .and_then(|cert| webpki::EndEntityCert::try_from(cert).ok())
            .is_some_and(|cert| cert.verify_is_valid_for_subject_name(&name).is_ok())
    }

    /// When the leaf expires, for logs.
    #[must_use]
    pub fn expiry_text(&self) -> String {
        self.validity()
            .map_or_else(|| "unknown".to_owned(), |v| v.not_after.to_string())
    }
}

/// The ring provider, as push's HTTPS client uses.
fn provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

/// The chain and key files' bytes, each read once: what is parsed is what
/// is hashed.
struct LeafFiles {
    chain: Vec<u8>,
    key: Vec<u8>,
}

impl LeafFiles {
    /// Reads both files, bounded; the key only if it is private.
    fn read(tls: &TlsConfig) -> Result<Self, TlsError> {
        let chain = read_bounded(&tls.cert, MAX_CHAIN_BYTES)
            .map_err(|error| read_error(error, &tls.cert, MAX_CHAIN_BYTES))?;
        let key = read_private(&tls.key, MAX_KEY_BYTES)
            .map_err(|error| read_error(error, &tls.key, MAX_KEY_BYTES))?;
        Ok(Self { chain, key })
    }

    fn digests(&self) -> FileDigests {
        FileDigests {
            cert: Some(Sha256::digest(&self.chain).into()),
            key: Some(Sha256::digest(&self.key).into()),
        }
    }
}

fn read_error(error: ReadError, path: &Path, max: u64) -> TlsError {
    let path = path.to_path_buf();
    match error {
        ReadError::Io(source) => TlsError::Io { path, source },
        ReadError::TooOpen { mode } => TlsError::TooOpen { path, mode },
        ReadError::TooLarge => TlsError::TooLarge { path, max },
    }
}

/// What the chain and key files hold (SHA-256 of their content), to tell
/// when they change. Content, not metadata: an in-place rewrite within one
/// timestamp tick still counts. `None` for a file that cannot be read
/// within its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileDigests {
    cert: Option<[u8; 32]>,
    key: Option<[u8; 32]>,
}

impl FileDigests {
    fn of(config: &TlsConfig) -> Self {
        let digest = |path: &Path, max| {
            read_bounded(path, max)
                .ok()
                .map(|bytes| Sha256::digest(&bytes).into())
        };
        Self {
            cert: digest(&config.cert, MAX_CHAIN_BYTES),
            key: digest(&config.key, MAX_KEY_BYTES),
        }
    }
}

/// The leaf the bridge serves, swapped by [`Self::reload`]. Hands it to
/// rustls for every handshake ([`ResolvesServerCert`]).
pub struct CertStore {
    config: TlsConfig,
    current: RwLock<Leaf>,
    /// The files' content at the last load attempt, unless that attempt
    /// failed for a reason outside the content ([`TlsError::is_transient`]):
    /// then `None`, so the next poll tries again.
    attempted: Mutex<Option<FileDigests>>,
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertStore")
            .field("cert", &self.config.cert)
            .field("key", &self.config.key)
            .finish_non_exhaustive()
    }
}

impl CertStore {
    /// Loads the leaf named by `config`, refusing one the edge would
    /// refuse ([`Leaf::check`]).
    pub fn load(config: &TlsConfig) -> Result<Self, TlsError> {
        let files = LeafFiles::read(config)?;
        let leaf = Leaf::parse(config, &files)?;
        leaf.check(config, SystemTime::now())?;
        Ok(Self {
            config: config.clone(),
            current: RwLock::new(leaf),
            attempted: Mutex::new(Some(files.digests())),
        })
    }

    /// The `[tls]` config it serves.
    #[must_use]
    pub fn config(&self) -> &TlsConfig {
        &self.config
    }

    /// The leaf served now.
    #[must_use]
    pub fn current(&self) -> Leaf {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn attempted(&self) -> MutexGuard<'_, Option<FileDigests>> {
        self.attempted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads the files again and, if they hold a leaf fit to serve, serves
    /// it from the next handshake on. On error the current leaf stays.
    pub fn reload(&self) -> Result<Leaf, TlsError> {
        let (digests, loaded) = match LeafFiles::read(&self.config) {
            Ok(files) => {
                let loaded = Leaf::parse(&self.config, &files).and_then(|leaf| {
                    leaf.check(&self.config, SystemTime::now())?;
                    Ok(leaf)
                });
                (Some(files.digests()), loaded)
            }
            Err(error) => (None, Err(error)),
        };
        *self.attempted() = match &loaded {
            Err(error) if error.is_transient() => None,
            _ => digests,
        };
        let leaf = loaded?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = leaf.clone();
        Ok(leaf)
    }

    /// The files' content now, if it differs from the last load attempt
    /// (always, after a transient failure).
    #[must_use]
    pub fn pending_change(&self) -> Option<FileDigests> {
        let now = FileDigests::of(&self.config);
        (*self.attempted() != Some(now)).then_some(now)
    }

    /// A rustls server config that serves this store's current leaf,
    /// speaking HTTP/1.1 only (what the router serves).
    pub fn server_config(self: &Arc<Self>) -> Result<Arc<ServerConfig>, TlsError> {
        let mut config = ServerConfig::builder_with_provider(Arc::new(provider()))
            .with_safe_default_protocol_versions()
            .map_err(TlsError::Config)?
            .with_no_client_auth()
            .with_cert_resolver(Arc::clone(self) as Arc<dyn ResolvesServerCert>);
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(
            &self
                .current
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .key,
        ))
    }
}
