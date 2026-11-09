//! `WS /v1/stream`: the live [`AgentEvent`] feed.
//!
//! The client's first message must be a [`StreamAuth`] carrying its bearer
//! token (browsers cannot set headers on a WebSocket); until it verifies,
//! nothing is sent, and without it the socket is closed (code 1008) after
//! [`StreamSettings::auth_timeout`].
//!
//! Once authenticated the client gets a `snapshot` and one `pending_changed`
//! per blocked agent with a parsed pending input, then `agent_updated` /
//! `agent_removed` as the registry changes, `pending_changed` as dialogs
//! appear, change and clear, and `thread_items` as transcripts the bridge
//! follows grow, and `tldr_ready` / `headline_ready` / `status_line_ready`
//! as summaries are made, and `runbook_updated` as runbook runs start,
//! print and end. Every agent sent carries its latest turn headline
//! and, while it works, its status line (see [`Tldrs::decorate`]); when a
//! new headline arrives, the `headline_ready` is followed by an
//! `agent_updated` re-sending each agent of that session as last sent (same
//! `seq`: its state did not change), with the new headline, and a
//! `status_line_ready` likewise re-sends that agent with its new line.
//! A blocked agent also carries what it asks ([`Agent::ask`]), and a
//! `pending_changed` that changes it is followed by an `agent_updated`
//! re-sending that agent with the new ask.
//! Each connection reads from its
//! own bounded queue (see [`Hub`]); a client too slow to keep up is sent a
//! fresh snapshot instead of an ever-growing backlog, and one whose socket
//! stops draining for [`StreamSettings::write_timeout`] is dropped.
//!
//! The bridge pings every [`StreamSettings::ping_every`]; a client that has
//! sent nothing (not even a pong) for two intervals is disconnected.
//! After the auth message, messages from the client are ignored: it only
//! listens. When the authenticated device is revoked, or its session
//! expires, the socket is closed at once (code 1008); the client renews
//! its session and reconnects.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, HeaderValue, Uri, header};
use axum::response::Response;
use protocol::{Agent, AgentEvent, AgentId, AgentStatus, StreamAuth};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

use super::AppState;
use super::auth::{AuthRequest, Authenticator, Principal};
use super::client_addr::ClientAddr;
use crate::registry::{Feed, Hub, ask};
use crate::shutdown::Shutdown;
use crate::tldr::Tldrs;

/// The largest message accepted from a client. It has nothing to say beyond
/// pongs and close frames.
const MAX_CLIENT_MESSAGE: usize = 4 * 1024;

/// Keepalive and backpressure limits for stream connections.
#[derive(Debug, Clone)]
pub struct StreamSettings {
    /// Interval between pings.
    pub ping_every: Duration,
    /// How long one send may wait on a socket that is not draining.
    pub write_timeout: Duration,
    /// How long a new connection may take to send its [`StreamAuth`].
    pub auth_timeout: Duration,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(20),
            write_timeout: Duration::from_secs(10),
            auth_timeout: Duration::from_secs(5),
        }
    }
}

/// Counts open stream connections, so shutdown can wait for their close
/// frames.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenStreams(Arc<watch::Sender<usize>>);

/// Marks one stream as open until dropped.
#[derive(Debug)]
struct OpenStream(Arc<watch::Sender<usize>>);

impl OpenStreams {
    fn open(&self) -> OpenStream {
        self.0.send_modify(|open| *open += 1);
        OpenStream(Arc::clone(&self.0))
    }

    /// Resolves once no stream is open.
    pub(crate) async fn all_closed(&self) {
        let mut open = self.0.subscribe();
        // The sender lives in `self`, so this cannot fail.
        let _ = open.wait_for(|open| *open == 0).await;
    }
}

impl Drop for OpenStream {
    fn drop(&mut self) {
        self.0.send_modify(|open| *open = open.saturating_sub(1));
    }
}

/// Upgrades a request to the event stream. Nothing is sent until the
/// client's first message, a [`StreamAuth`], passes the [`Authenticator`]
/// (as `Authorization: Bearer <token>`); a missing, malformed, wrong or late
/// one closes the socket with a policy-violation close frame.
pub(crate) async fn upgrade(
    State(state): State<AppState>,
    Extension(client): Extension<ClientAddr>,
    OriginalUri(uri): OriginalUri,
    ws: WebSocketUpgrade,
) -> Response {
    let hub = Arc::clone(&state.hub);
    let tldrs = Arc::clone(&state.tldrs);
    let auth = Arc::clone(&state.auth);
    let settings = state.settings.stream.clone();
    let shutdown = state.shutdown.clone();
    // Counted from the upgrade response on, so shutdown cannot miss it.
    let open = state.streams.open();
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .max_frame_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |mut socket| async move {
            let gate = Gate {
                auth: auth.as_ref(),
                client,
                uri: &uri,
                timeout: settings.auth_timeout,
            };
            if let Some(principal) = gate.admit(&mut socket).await {
                let stamps = Stamps::new(tldrs);
                run(socket, hub, stamps, settings, shutdown, principal).await;
            } else {
                tracing::debug!(%client, "stream refused: no valid auth message");
                close(&mut socket, close_code::POLICY, settings.write_timeout).await;
            }
            drop(open);
        })
}

/// The first-message authentication of a stream.
struct Gate<'a> {
    auth: &'a dyn Authenticator,
    client: ClientAddr,
    uri: &'a Uri,
    timeout: Duration,
}

impl Gate<'_> {
    /// Who the client is, if its first message authenticates it in time.
    async fn admit(&self, socket: &mut WebSocket) -> Option<Principal> {
        let Ok(Some(text)) = tokio::time::timeout(self.timeout, first_text(socket)).await else {
            return None;
        };
        let message = serde_json::from_str::<StreamAuth>(&text).ok()?;
        let mut value = HeaderValue::from_str(&format!("Bearer {}", message.token)).ok()?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value);
        self.auth
            .authenticate(AuthRequest {
                headers: &headers,
                uri: self.uri,
                client: self.client,
            })
            .await
            .ok()
            // The car reads notices, never threads: app sessions only.
            .filter(|principal| principal.scope == protocol::Scope::App)
    }
}

/// The first data message, skipping pings and pongs; `None` if the client
/// sends anything else first or goes away.
async fn first_text(socket: &mut WebSocket) -> Option<Utf8Bytes> {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => return Some(text),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            _ => return None,
        }
    }
}

async fn close(socket: &mut WebSocket, code: u16, limit: Duration) {
    let frame = CloseFrame {
        code,
        reason: "".into(),
    };
    // Best effort: the client may already be gone.
    let _ = tokio::time::timeout(limit, socket.send(Message::Close(Some(frame)))).await;
}

/// Why a connection ended, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    ClientClosed,
    SocketError,
    WriteTimeout,
    KeepaliveTimeout,
    Revoked,
    Expired,
    Shutdown,
}

async fn run(
    mut socket: WebSocket,
    hub: Arc<Hub>,
    mut stamps: Stamps,
    settings: StreamSettings,
    shutdown: Shutdown,
    principal: Principal,
) {
    tracing::debug!("stream connected");
    let feed = hub.subscribe();
    let end = pump(
        &mut socket,
        feed,
        &mut stamps,
        &settings,
        shutdown,
        principal,
    )
    .await;
    if matches!(
        end,
        End::Shutdown | End::KeepaliveTimeout | End::Revoked | End::Expired
    ) {
        let code = if end == End::Shutdown {
            close_code::AWAY
        } else {
            close_code::POLICY
        };
        close(&mut socket, code, settings.write_timeout).await;
    }
    tracing::debug!(?end, "stream disconnected");
}

async fn pump(
    socket: &mut WebSocket,
    mut feed: Feed,
    stamps: &mut Stamps,
    settings: &StreamSettings,
    mut shutdown: Shutdown,
    principal: Principal,
) -> End {
    let Principal {
        expires,
        mut revocation,
        ..
    } = principal;
    let mut ping =
        tokio::time::interval_at(Instant::now() + settings.ping_every, settings.ping_every);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            event = feed.next() => {
                for event in stamps.outgoing(event) {
                    if let Err(end) = send_event(socket, &event, settings.write_timeout).await {
                        return end;
                    }
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None => return End::ClientClosed,
                Some(Err(error)) => {
                    tracing::debug!(%error, "stream read failed");
                    return End::SocketError;
                }
                Some(Ok(_)) => last_heard = Instant::now(),
            },
            _ = ping.tick() => {
                if last_heard.elapsed() > settings.ping_every * 2 {
                    return End::KeepaliveTimeout;
                }
                if let Err(end) = send(socket, Message::Ping(Bytes::new()), settings.write_timeout).await {
                    return end;
                }
            }
            () = shutdown.wait() => return End::Shutdown,
            () = revocation.revoked() => return End::Revoked,
            () = deadline(expires) => return End::Expired,
        }
    }
}

/// One connection's view of what the bridge adds to agents: stamps every
/// agent it sends with its latest headline and status line, and each
/// blocked one with what it asks, and remembers the agents as sent, so a
/// new headline, line or ask is delivered by re-sending the agent the
/// client already has (never a newer state that would overtake events
/// still queued).
///
/// Asks come from this connection's own feed: the snapshot carries them
/// ([`Hub::cards`]), and each `pending_changed` after it updates them, in
/// order with the agent updates. Nothing here reads a screen.
struct Stamps {
    tldrs: Arc<Tldrs>,
    sent: HashMap<AgentId, Agent>,
    /// The ask of each agent with a known pending input.
    asks: HashMap<AgentId, String>,
}

impl Stamps {
    fn new(tldrs: Arc<Tldrs>) -> Self {
        Self {
            tldrs,
            sent: HashMap::new(),
            asks: HashMap::new(),
        }
    }

    /// What to send for `event`, in order.
    fn outgoing(&mut self, event: AgentEvent) -> Vec<AgentEvent> {
        match event {
            AgentEvent::Snapshot { mut agents } => {
                for agent in &mut agents {
                    self.tldrs.decorate(agent);
                }
                self.asks = agents
                    .iter()
                    .filter_map(|a| Some((a.id.clone(), a.ask.clone()?)))
                    .collect();
                self.sent = agents.iter().map(|a| (a.id.clone(), a.clone())).collect();
                vec![AgentEvent::Snapshot { agents }]
            }
            AgentEvent::AgentUpdated {
                agent: Some(mut agent),
            } => {
                self.tldrs.decorate(&mut agent);
                // The hub drops a pending input with its `seq` (and sends a
                // `pending_changed` to `null` after this): so does the ask.
                if self
                    .sent
                    .get(&agent.id)
                    .is_some_and(|sent| sent.seq != agent.seq)
                {
                    self.asks.remove(&agent.id);
                }
                agent.ask = ask_of(&self.asks, &agent);
                self.sent.insert(agent.id.clone(), agent.clone());
                vec![AgentEvent::AgentUpdated { agent: Some(agent) }]
            }
            AgentEvent::AgentRemoved { agent_id } => {
                self.sent.remove(&agent_id);
                self.asks.remove(&agent_id);
                vec![AgentEvent::AgentRemoved { agent_id }]
            }
            AgentEvent::PendingChanged {
                ref agent_id,
                ref pending,
            } => {
                match pending.as_ref().and_then(ask) {
                    Some(line) => self.asks.insert(agent_id.clone(), line),
                    None => self.asks.remove(agent_id),
                };
                let mut out = Vec::new();
                if let Some(agent) = self.sent.get_mut(agent_id) {
                    let now = ask_of(&self.asks, agent);
                    if agent.ask != now {
                        agent.ask = now;
                        out.push(AgentEvent::AgentUpdated {
                            agent: Some(agent.clone()),
                        });
                    }
                }
                out.insert(0, event);
                out
            }
            AgentEvent::HeadlineReady { ref agent_id, .. } => {
                // Every agent of the session, as sent: it may have moved
                // panes (a new id) since the job started. One without a
                // session in its id (matched by folder) is the agent itself.
                let session = agent_id.session();
                let mut resend: Vec<AgentEvent> = Vec::new();
                for agent in self.sent.values_mut() {
                    let same = match session {
                        Some(session) => agent.id.session() == Some(session),
                        None => agent.id == *agent_id,
                    };
                    if !same {
                        continue;
                    }
                    let before = agent.headline.clone();
                    self.tldrs.add_headline(agent);
                    if agent.headline != before {
                        resend.push(AgentEvent::AgentUpdated {
                            agent: Some(agent.clone()),
                        });
                    }
                }
                let mut out = vec![event];
                out.extend(resend);
                out
            }
            AgentEvent::StatusLineReady { ref agent_id, .. } => {
                let mut out = Vec::new();
                if let Some(agent) = self.sent.get_mut(agent_id) {
                    let before = agent.status_line.take();
                    self.tldrs.status_lines().add_status_line(agent);
                    if agent.status_line != before {
                        out.push(AgentEvent::AgentUpdated {
                            agent: Some(agent.clone()),
                        });
                    }
                }
                out.insert(0, event);
                out
            }
            other => vec![other],
        }
    }
}

/// `agent`'s ask in `asks`, while it is blocked.
fn ask_of(asks: &HashMap<AgentId, String>, agent: &Agent) -> Option<String> {
    if agent.status == AgentStatus::Blocked {
        asks.get(&agent.id).cloned()
    } else {
        None
    }
}

/// Resolves at `at`; never without one (a credential that does not expire).
async fn deadline(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn send_event(
    socket: &mut WebSocket,
    event: &AgentEvent,
    limit: Duration,
) -> Result<(), End> {
    match serde_json::to_string(event) {
        Ok(json) => send(socket, Message::Text(json.into()), limit).await,
        Err(error) => {
            // Serializing our own types cannot fail; skip the event rather than drop the client.
            tracing::error!(%error, "could not serialize stream event");
            Ok(())
        }
    }
}

async fn send(socket: &mut WebSocket, message: Message, limit: Duration) -> Result<(), End> {
    match tokio::time::timeout(limit, socket.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            tracing::debug!(%error, "stream write failed");
            Err(End::SocketError)
        }
        Err(_) => Err(End::WriteTimeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_deadline_fires_at_expiry_and_never_without_one() {
        let at = Instant::now() + Duration::from_mins(15);
        deadline(Some(at)).await;
        assert_eq!(Instant::now(), at);
        let forever = tokio::time::timeout(Duration::from_hours(24), deadline(None));
        assert!(forever.await.is_err());
    }
}
