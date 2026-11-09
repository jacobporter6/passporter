//! HTTPS for the edge: the API and the event stream over TLS from a
//! runtime-generated CA, leaf reloads (file change and SIGHUP), and the
//! loader's refusals.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use bridge::tls::{self, CertStore, Poller, TlsError};
use herdr::AgentStatus;
use protocol::AgentEvent;
use rustls::pki_types::CertificateDer;
use time::OffsetDateTime;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio_rustls::TlsConnector;

use crate::harness::{self, Bridge, CuttableHerdr, PATIENCE, Stream, TOKEN};
use crate::pki::{self, Ca, Issued};

/// A CA, its first leaf installed in a temp dir, and a store serving it.
struct Pki {
    ca: Ca,
    first: Issued,
    dir: tempfile::TempDir,
    store: Arc<CertStore>,
}

impl Pki {
    fn new() -> Self {
        let ca = Ca::new();
        let first = ca.leaf();
        let dir = tempfile::tempdir().expect("tmp");
        let config = first.install(dir.path());
        let store = Arc::new(CertStore::load(&config).expect("loads"));
        Self {
            ca,
            first,
            dir,
            store,
        }
    }

    fn client(&self) -> TlsConnector {
        pki::client(self.ca.root())
    }
}

/// This process's SIGHUP stream, installed before anything sends one.
fn hangups() -> Signal {
    signal(SignalKind::hangup()).expect("SIGHUP handler")
}

/// Waits until a fresh connection to `bridge` is shown `want`.
async fn wait_for_leaf(bridge: &Bridge, client: &TlsConnector, want: &CertificateDer<'_>) {
    let poll = async {
        loop {
            let stream = pki::connect(client, bridge.addr).await.expect("handshake");
            if pki::presented(&stream) == *want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(PATIENCE, poll)
        .await
        .expect("the new leaf is served in time");
}

#[tokio::test]
async fn https_serves_the_api_and_upgrades_the_stream() {
    let pki = Pki::new();
    let herdr = CuttableHerdr::new();
    herdr.fake.add_agent("worker", "claude", AgentStatus::Idle);
    let bridge = Bridge::start_https(herdr, Arc::clone(&pki.store)).await;
    let client = pki.client();

    let tls = pki::connect(&client, bridge.addr).await.expect("handshake");
    assert_eq!(pki::presented(&tls), pki.first.der);
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    let auth = format!("Bearer {TOKEN}");
    let response = harness::http(tls, "GET", "/v1/agents", &[("authorization", &auth)]).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(response.body.contains("worker"), "{}", response.body);

    let tls = pki::connect(&client, bridge.addr).await.expect("handshake");
    let mut stream = Stream::connect(tls, Some(TOKEN))
        .await
        .expect("WSS upgrade");
    match stream.next_event().await {
        AgentEvent::Snapshot { agents } => assert_eq!(agents.len(), 1),
        other => panic!("expected a snapshot, got {other:?}"),
    }
    stream.close().await;
    bridge.stop().await.expect("clean stop");
}

#[tokio::test]
async fn plain_http_and_other_cas_get_nothing() {
    let pki = Pki::new();
    let bridge = Bridge::start_https(CuttableHerdr::new(), Arc::clone(&pki.store)).await;

    let mut plain = TcpStream::connect(bridge.addr).await.expect("connect");
    plain
        .write_all(b"GET /v1/agents HTTP/1.1\r\nhost: bridge\r\n\r\n")
        .await
        .expect("send");
    let mut reply = Vec::new();
    let _ = tokio::time::timeout(PATIENCE, plain.read_to_end(&mut reply))
        .await
        .expect("closed in time");
    assert!(
        !String::from_utf8_lossy(&reply).contains("HTTP/1.1"),
        "plain HTTP must not be answered"
    );

    let stranger = pki::client(Ca::new().root());
    assert!(
        pki::connect(&stranger, bridge.addr).await.is_err(),
        "a client trusting another CA must refuse the leaf"
    );
    // A failed handshake does not stop the listener.
    assert!(pki::connect(&pki.client(), bridge.addr).await.is_ok());
}

#[tokio::test]
async fn a_renewed_leaf_is_served_without_a_restart() {
    let pki = Pki::new();
    let bridge = Bridge::start_https(CuttableHerdr::new(), Arc::clone(&pki.store)).await;
    let client = pki.client();
    let (_trigger, shutdown) = bridge::shutdown::channel();
    let reloader = tls::spawn_reloader(
        Arc::clone(&pki.store),
        Duration::from_millis(20),
        hangups(),
        shutdown,
    );

    let open = pki::connect(&client, bridge.addr).await.expect("handshake");
    assert_eq!(pki::presented(&open), pki.first.der);
    let renewed = pki.ca.leaf();
    renewed.install(pki.dir.path());
    wait_for_leaf(&bridge, &client, &renewed.der).await;

    // An open connection keeps working on the leaf it was made with.
    let auth = format!("Bearer {TOKEN}");
    let response = harness::http(open, "GET", "/v1/agents", &[("authorization", &auth)]).await;
    assert_eq!(response.status, 200);
    reloader.abort();
}

#[tokio::test]
async fn sighup_reloads_the_leaf() {
    let pki = Pki::new();
    let bridge = Bridge::start_https(CuttableHerdr::new(), Arc::clone(&pki.store)).await;
    let client = pki.client();
    let (_trigger, shutdown) = bridge::shutdown::channel();
    // Polling so rarely that only the signal can explain a reload.
    let reloader = tls::spawn_reloader(
        Arc::clone(&pki.store),
        Duration::from_secs(3600),
        hangups(),
        shutdown,
    );

    let renewed = pki.ca.leaf();
    renewed.install(pki.dir.path());
    let status = Command::new("kill")
        .args(["-HUP", &std::process::id().to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success());
    wait_for_leaf(&bridge, &client, &renewed.der).await;
    reloader.abort();
}

#[tokio::test]
async fn a_broken_renewal_keeps_the_served_leaf() {
    let pki = Pki::new();
    let bridge = Bridge::start_https(CuttableHerdr::new(), Arc::clone(&pki.store)).await;
    let client = pki.client();

    let mut poller = Poller::new(Arc::clone(&pki.store));

    // A new chain whose key never arrives.
    let renewed = pki.ca.leaf();
    fs::write(pki.dir.path().join(pki::CHAIN_FILE), &renewed.chain).expect("write chain");
    assert!(matches!(poller.poll(), Ok(None)), "settling");
    assert!(matches!(poller.poll(), Err(TlsError::KeyMismatch { .. })));
    let tls = pki::connect(&client, bridge.addr).await.expect("handshake");
    assert_eq!(pki::presented(&tls), pki.first.der, "still the old leaf");
    // Reported once, not at every poll.
    assert!(matches!(poller.poll(), Ok(None)));

    // The key lands: once the files settle, the pair is picked up.
    renewed.install(pki.dir.path());
    assert!(matches!(poller.poll(), Ok(None)), "settling");
    assert!(matches!(poller.poll(), Ok(Some(_))));
    let tls = pki::connect(&client, bridge.addr).await.expect("handshake");
    assert_eq!(pki::presented(&tls), renewed.der);
}

#[tokio::test]
async fn a_renewal_the_edge_would_refuse_keeps_the_served_leaf() {
    let pki = Pki::new();
    let bridge = Bridge::start_https(CuttableHerdr::new(), Arc::clone(&pki.store)).await;
    let client = pki.client();
    let now = OffsetDateTime::now_utc();
    let days = time::Duration::days;
    for (renewal, refused) in [
        (
            pki.ca.issue(&["devbox.example.test"], now + days(90)),
            "WrongName",
        ),
        (pki.ca.issue(&[pki::NAME], now - days(1)), "Expired"),
        (
            pki.ca
                .issue_from(&[pki::NAME], now + days(2), now + days(90)),
            "NotYetValid",
        ),
    ] {
        renewal.install(pki.dir.path());
        let error = pki.store.reload().expect_err(refused);
        assert!(format!("{error:?}").starts_with(refused), "{error}");
        let tls = pki::connect(&client, bridge.addr).await.expect("handshake");
        assert_eq!(pki::presented(&tls), pki.first.der, "still the old leaf");
    }
}

/// Opens a silent TCP connection to `addr` from the loopback address `from`.
async fn silent_from(from: &str, addr: std::net::SocketAddr) -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket
        .bind(format!("{from}:0").parse().expect("addr"))
        .expect("bind");
    socket.connect(addr).await.expect("connect")
}

#[tokio::test]
async fn a_flooding_host_cannot_starve_or_evict_the_edge() {
    let pki = Pki::new();
    let bridge = Bridge::start_https(CuttableHerdr::new(), Arc::clone(&pki.store)).await;
    let client = pki.client();
    // The edge (127.0.0.1) has a handshake in flight...
    let in_flight = silent_from("127.0.0.1", bridge.addr).await;
    // ...when another host floods the port with twice the global limit.
    let mut flood = Vec::new();
    for _ in 0..2 * tls::MAX_HANDSHAKES {
        flood.push(silent_from("127.0.0.2", bridge.addr).await);
    }
    // The edge's handshake was not evicted, and new ones get through,
    // well inside the handshake timeout (a starved one would wait for it).
    let quick = tls::HANDSHAKE_TIMEOUT / 2;
    let finished = tokio::time::timeout(quick, pki::handshake(&client, in_flight))
        .await
        .expect("not starved")
        .expect("the in-flight handshake survives the flood");
    assert_eq!(pki::presented(&finished), pki.first.der);
    let edge = tokio::time::timeout(quick, pki::connect(&client, bridge.addr))
        .await
        .expect("not starved")
        .expect("handshake");
    let auth = format!("Bearer {TOKEN}");
    let response = harness::http(edge, "GET", "/v1/agents", &[("authorization", &auth)]).await;
    assert_eq!(response.status, 200);
    drop(flood);
}

#[tokio::test]
async fn a_peer_flooding_itself_only_evicts_its_own_oldest() {
    let pki = Pki::new();
    let bridge = Bridge::start_https(CuttableHerdr::new(), Arc::clone(&pki.store)).await;
    let mut own = Vec::new();
    for _ in 0..2 * tls::MAX_HANDSHAKES_PER_PEER {
        own.push(silent_from("127.0.0.1", bridge.addr).await);
    }
    // Its newest connection still completes.
    tokio::time::timeout(
        tls::HANDSHAKE_TIMEOUT / 2,
        pki::connect(&pki.client(), bridge.addr),
    )
    .await
    .expect("not starved")
    .expect("handshake");
    drop(own);
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

#[test]
fn the_loader_refuses_open_mismatched_and_malformed_files() {
    let ca = Ca::new();
    let dir = tempfile::tempdir().expect("tmp");
    let leaf = ca.leaf();
    let config = leaf.install(dir.path());
    assert!(CertStore::load(&config).is_ok());

    for mode in [0o640, 0o604] {
        chmod(&config.key, mode);
        let error = CertStore::load(&config).expect_err("open key");
        assert!(matches!(error, TlsError::TooOpen { .. }), "{error}");
        assert!(error.to_string().contains("chmod 600"), "{error}");
    }
    chmod(&config.key, 0o600);

    let other = ca.leaf();
    fs::write(&config.key, &other.key).expect("write key");
    let error = CertStore::load(&config).expect_err("mismatch");
    assert!(matches!(error, TlsError::KeyMismatch { .. }), "{error}");
    assert!(!error.to_string().contains("PRIVATE"), "{error}");

    fs::write(&config.cert, "not a certificate").expect("write");
    assert!(matches!(
        CertStore::load(&config),
        Err(TlsError::BadChain { .. })
    ));
    leaf.install(dir.path());
    fs::write(&config.key, "not a key").expect("write");
    assert!(matches!(
        CertStore::load(&config),
        Err(TlsError::BadKey { .. })
    ));
    // The edge would refuse an expired leaf, so the bridge does not start with one.
    let expired = ca.issue(
        &[pki::NAME],
        OffsetDateTime::now_utc() - time::Duration::days(1),
    );
    expired.install(dir.path());
    assert!(matches!(
        CertStore::load(&config),
        Err(TlsError::Expired { .. })
    ));
    fs::remove_file(&config.key).expect("rm");
    assert!(matches!(CertStore::load(&config), Err(TlsError::Io { .. })));
}
