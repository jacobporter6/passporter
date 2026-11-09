//! A throwaway private CA and its leaves, made at runtime for the TLS
//! tests (AGENTS.md: no PEM literals in source), and a client that trusts
//! only that CA, as the edge trusts only the home CA.

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use bridge::config::{DEFAULT_TLS_NAME, TlsConfig};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, DnsName, ServerName};
use rustls::{ClientConfig, RootCertStore};
use time::{Duration, OffsetDateTime};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// The name the edge verifies, and so what leaves are issued for.
pub(crate) const NAME: &str = DEFAULT_TLS_NAME;

/// The chain file's name inside an install dir.
pub(crate) const CHAIN_FILE: &str = "leaf.pem";

/// The key file's name inside an install dir.
pub(crate) const KEY_FILE: &str = "leaf.key";

/// A self-signed root that issues leaves.
pub(crate) struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

/// A leaf as PEM text, with its DER to compare against what a client sees.
pub(crate) struct Issued {
    /// The leaf, then the root.
    pub(crate) chain: String,
    /// Its private key.
    pub(crate) key: String,
    /// The leaf alone.
    pub(crate) der: CertificateDer<'static>,
}

impl Ca {
    pub(crate) fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "passporter test root");
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate().expect("root key");
        Self {
            issuer: CertifiedIssuer::self_signed(params, key).expect("root"),
        }
    }

    /// The root certificate, for a client's trust store.
    pub(crate) fn root(&self) -> CertificateDer<'static> {
        self.issuer.der().clone()
    }

    /// A leaf for [`NAME`], valid for 90 days.
    pub(crate) fn leaf(&self) -> Issued {
        self.issue(&[NAME], OffsetDateTime::now_utc() + Duration::days(90))
    }

    /// A leaf for `names` that expires at `not_after` (and became valid
    /// well before now, or before `not_after` if that is earlier).
    pub(crate) fn issue(&self, names: &[&str], not_after: OffsetDateTime) -> Issued {
        let not_before = not_after.min(OffsetDateTime::now_utc()) - Duration::days(30);
        self.issue_from(names, not_before, not_after)
    }

    /// A leaf for `names` valid from `not_before` to `not_after`.
    pub(crate) fn issue_from(
        &self,
        names: &[&str],
        not_before: OffsetDateTime,
        not_after: OffsetDateTime,
    ) -> Issued {
        let names: Vec<String> = names.iter().map(|&name| name.to_owned()).collect();
        let mut params = CertificateParams::new(names).expect("leaf params");
        params.not_before = not_before;
        params.not_after = not_after;
        let key = KeyPair::generate().expect("leaf key");
        let cert = params.signed_by(&key, &self.issuer).expect("sign leaf");
        Issued {
            chain: cert.pem() + &self.issuer.pem(),
            key: key.serialize_pem(),
            der: cert.der().clone(),
        }
    }
}

impl Issued {
    /// Writes the chain and key (`0600`) into `dir` as [`CHAIN_FILE`] and
    /// [`KEY_FILE`], each by an atomic rename, as a renewal would, and
    /// returns the `[tls]` config naming them.
    pub(crate) fn install(&self, dir: &Path) -> TlsConfig {
        let (cert, key) = (dir.join(CHAIN_FILE), dir.join(KEY_FILE));
        replace(&key, &self.key, 0o600);
        replace(&cert, &self.chain, 0o644);
        TlsConfig {
            cert,
            key,
            name: DnsName::try_from(NAME).expect("name").to_owned(),
        }
    }
}

fn replace(path: &Path, text: &str, mode: u32) {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).expect("write");
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).expect("chmod");
    fs::rename(&tmp, path).expect("rename");
}

/// A TLS client that trusts only `root`, speaking HTTP/1.1.
pub(crate) fn client(root: CertificateDer<'static>) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots.add(root).expect("root");
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    TlsConnector::from(Arc::new(config))
}

/// Connects to `addr` and completes the handshake for [`NAME`].
pub(crate) async fn connect(
    client: &TlsConnector,
    addr: SocketAddr,
) -> std::io::Result<TlsStream<TcpStream>> {
    handshake(client, TcpStream::connect(addr).await?).await
}

/// Completes the handshake for [`NAME`] over an open connection.
pub(crate) async fn handshake(
    client: &TlsConnector,
    tcp: TcpStream,
) -> std::io::Result<TlsStream<TcpStream>> {
    let name = ServerName::try_from(NAME).expect("name").to_owned();
    client.connect(name, tcp).await
}

/// The leaf `stream`'s server presented.
pub(crate) fn presented(stream: &TlsStream<TcpStream>) -> CertificateDer<'static> {
    stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .expect("a server certificate")
        .clone()
        .into_owned()
}
