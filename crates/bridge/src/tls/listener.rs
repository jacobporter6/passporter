//! [`TlsListener`]: TCP accepts, with each TLS handshake run off the accept
//! path so one slow client cannot hold up the others.
//!
//! Accepting never stops, and a new connection is never refused. Instead,
//! a peer with [`MAX_HANDSHAKES_PER_PEER`] unfinished handshakes loses its
//! own oldest one, and past [`MAX_HANDSHAKES`] in all, the peer with the
//! most unfinished handshakes loses its oldest. So a host that floods the
//! port with silent connections only ever evicts its own, never the
//! edge's few, and nothing holds the accept loop for [`HANDSHAKE_TIMEOUT`].
//! Finished handshakes leave the counts at once.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::serve::{Listener, ListenerExt, TapIo};
use rustls::ServerConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// Longest a client may take to finish the TLS handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Handshakes in flight at once, from all peers.
pub const MAX_HANDSHAKES: usize = 64;

/// Handshakes in flight at once from one peer address.
pub const MAX_HANDSHAKES_PER_PEER: usize = 16;

/// Finished handshakes the server has not picked up yet. axum takes each
/// at once (it spawns a task per connection), so this only fills if the
/// server has stalled; connections past it are dropped, never waited on.
const READY_QUEUE: usize = 64;

/// A connection whose handshake finished.
type Ready = (TlsStream<TcpStream>, SocketAddr);

/// How long to back off after an accept error that is not the peer's
/// (out of file descriptors), as axum's own listener does.
const ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

/// What [`crate::run`] serves HTTPS on: a [`TlsListener`] with
/// `TCP_NODELAY` set on each connection. (axum gives `ConnectInfo` of the
/// peer address to a [`TapIo`] listener, which is also why it is wrapped.)
pub type HttpsListener = TapIo<TlsListener, fn(&mut TlsStream<TcpStream>)>;

/// An axum [`Listener`] that yields connections whose TLS handshake has
/// completed. A background task accepts and handshakes; dropping the
/// listener stops it and every handshake in flight.
pub struct TlsListener {
    ready: mpsc::Receiver<Ready>,
    local: SocketAddr,
    acceptor: JoinHandle<()>,
}

impl fmt::Debug for TlsListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsListener")
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

impl TlsListener {
    /// Serves TLS with `config` on connections accepted from `tcp`.
    pub fn new(tcp: TcpListener, config: Arc<ServerConfig>) -> io::Result<Self> {
        let local = tcp.local_addr()?;
        let (tx, ready) = mpsc::channel(READY_QUEUE);
        let acceptor = tokio::spawn(accept_loop(tcp, TlsAcceptor::from(config), tx));
        Ok(Self {
            ready,
            local,
            acceptor,
        })
    }

    /// This listener as [`crate::run`] takes it.
    #[must_use]
    pub fn into_https(self) -> HttpsListener {
        self.tap_io(no_delay)
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.acceptor.abort();
    }
}

/// Small writes (stream events) go out at once.
fn no_delay(stream: &mut TlsStream<TcpStream>) {
    if let Err(error) = stream.get_ref().0.set_nodelay(true) {
        tracing::debug!(%error, "cannot set TCP_NODELAY");
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        if let Some(connection) = self.ready.recv().await {
            return connection;
        }
        // Only if the accept task died, which it does not short of a panic.
        tracing::error!("the TLS accept task stopped; no new connections");
        std::future::pending().await
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// Unfinished handshakes by peer, oldest first.
#[derive(Default)]
struct Pending(VecDeque<(IpAddr, AbortHandle)>);

impl Pending {
    /// Makes room for one more handshake from `peer` (see the module
    /// docs), then records `handshake`.
    fn admit(&mut self, peer: IpAddr, handshake: AbortHandle) {
        self.0.retain(|(_, handshake)| !handshake.is_finished());
        let count = |ip: IpAddr| self.0.iter().filter(|(other, _)| *other == ip).count();
        let victim = if count(peer) >= MAX_HANDSHAKES_PER_PEER {
            Some(peer)
        } else if self.0.len() >= MAX_HANDSHAKES {
            self.0.iter().map(|(ip, _)| *ip).max_by_key(|&ip| count(ip))
        } else {
            None
        };
        if let Some(victim) = victim
            && let Some(at) = self.0.iter().position(|(ip, _)| *ip == victim)
            && let Some((_, oldest)) = self.0.remove(at)
        {
            oldest.abort();
            tracing::debug!(peer = %victim, "TLS handshakes at the limit; dropped the peer's oldest");
        }
        self.0.push_back((peer, handshake));
    }
}

async fn accept_loop(tcp: TcpListener, acceptor: TlsAcceptor, ready: mpsc::Sender<Ready>) {
    let mut handshakes = JoinSet::new();
    let mut pending = Pending::default();
    loop {
        tokio::select! {
            accepted = tcp.accept() => match accepted {
                Ok((stream, peer)) => {
                    let handshake = handshakes.spawn(handshake(acceptor.clone(), stream, peer));
                    pending.admit(peer.ip().to_canonical(), handshake);
                }
                Err(error) => accept_error(error).await,
            },
            Some(done) = handshakes.join_next(), if !handshakes.is_empty() => {
                let Ok(Some(connection)) = done else { continue };
                match ready.try_send(connection) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        tracing::warn!("the server is not taking connections; dropped one");
                    }
                    Err(TrySendError::Closed(_)) => return,
                }
            }
            () = ready.closed() => return,
        }
    }
}

/// One handshake, bounded by [`HANDSHAKE_TIMEOUT`].
async fn handshake(acceptor: TlsAcceptor, stream: TcpStream, peer: SocketAddr) -> Option<Ready> {
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => Some((tls, peer)),
        // Worth seeing while switching over: a wrong CA on the edge shows here.
        Ok(Err(error)) => {
            tracing::info!(%peer, %error, "TLS handshake failed");
            None
        }
        Err(_) => {
            tracing::info!(%peer, "TLS handshake timed out");
            None
        }
    }
}

async fn accept_error(error: io::Error) {
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    ) {
        return;
    }
    tracing::error!(%error, "accept error");
    tokio::time::sleep(ACCEPT_BACKOFF).await;
}
