//! Push: content-free wake pings to the PWA, and short encrypted notices to
//! the native app (design §6.6, decision D4).
//!
//! When an agent becomes blocked or finishes a turn ([`transitions`]), every
//! device with a push subscription gets a message, debounced and coalesced
//! per device ([`schedule`]). When a device's message is due, the changes
//! it is for are checked against the registry again
//! ([`transitions::Change::lapsed`]): those that lapsed (the dialog
//! answered, the agent working again, the finished turn read) are dropped,
//! notices included, and if none is left the message is withdrawn and
//! logged, so the phone is never woken to say that nothing needs the user.
//! A message only goes for a change since the last one, so nothing new
//! means none is sent.
//!
//! Agents run by an orchestrator can be made to ping only when blocked:
//! their finished turns are noise (another agent is waiting on them, not
//! the user). An agent is one of those if its herdr name starts with a
//! quiet prefix or it runs in a quiet workspace (defaults in
//! `crate::config`; see [`QuietDone`], which also sets the agent list's
//! `quietDone` flag). A workspace label the registry does not know yet is
//! looked up in herdr before deciding ([`lookup`]); only if that fails does the
//! finished turn ping (fail open: it may be one of the user's own agents).
//! Each decision is logged at info with the agent id and the reason, never
//! content. A ping carries only an opaque random id,
//! `{"ping":"<32 hex>"}`, encrypted to the subscription (RFC 8291, [`ece`])
//! and signed with the bridge's VAPID key (RFC 8292, [`vapid`]). The app
//! wakes and fetches the actual state over the authenticated API, so no
//! agent content ever reaches the push service.
//!
//! A subscription that opted into notices (the native app; Doze blocks the
//! fetch after a wake) also gets, in the same encrypted message, a short
//! [`protocol::Notice`] per agent with news: name, state and one line
//! ([`notice`]). The push service still sees only ciphertext. After a
//! finished turn that pinged, the dispatcher also waits (up to
//! [`DECIDE_WINDOW`]) for that turn's TL;DR: a *Decide* line in it is sent
//! at once as one more notice, to notice subscriptions only and outside
//! the per-device schedule (so it neither wakes the PWA nor delays the
//! next ping), whatever the [`CarAnnounce`] policy (which only sets
//! whether the car announces it). A TL;DR of an older turn than the one
//! that finished (asked for on the phone meanwhile) is not that decision.
//!
//! - [`subscription`]: the browser's subscription, validated.
//! - [`store`]: subscriptions per device, in a private JSON file.
//! - [`sender`]: the [`PushSender`] seam and the HTTPS implementation.
//!
//! The native app's `UnifiedPush` endpoint is the self-hosted ntfy server,
//! allowed by name with `[push] ntfy_host` ([`PushHosts::with_ntfy`]).

pub mod ece;
pub mod lookup;
mod notice;
pub mod schedule;
pub mod sender;
pub mod store;
pub mod subscription;
mod transitions;
pub mod vapid;

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{AgentEvent, AgentId, Notice, PushMessage, PushPayload, TurnIndex};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::broadcast;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;

pub use self::ece::EncryptError;
pub use self::lookup::{HerdrLookup, LookupFuture, PlacementLookup};
pub use self::notice::CarAnnounce;
use self::notice::{Batch, Wake};
pub use self::schedule::PushTiming;
use self::schedule::Schedule;
pub use self::sender::{Delivery, HttpSender, PushRequest, PushSender, SendError, SendFuture};
pub use self::store::{Put, StoreError, SubscriptionStore};
pub use self::subscription::{
    Endpoint, InvalidPushHost, InvalidSubscription, PushHost, PushHosts, Subscription, Unsubscribe,
};
use self::transitions::{Change, Transitions};
pub use self::vapid::{Subject, VapidError, VapidKey};
use crate::registry::{Hub, Placement, QuietDone, Verdict};
use crate::server::Authenticator;
use crate::shutdown::Shutdown;

/// What [`Push`] needs.
#[derive(Clone)]
pub struct PushSettings {
    /// The VAPID signing key.
    pub vapid: Arc<VapidKey>,
    /// The VAPID contact.
    pub subject: Subject,
    /// The subscription store file.
    pub store: PathBuf,
    /// Delivers the messages.
    pub sender: Arc<dyn PushSender>,
    /// The push services subscriptions may point at.
    pub hosts: PushHosts,
    /// Debounce and rate limits.
    pub timing: PushTiming,
    /// Longest wait for one push service response.
    pub send_timeout: Duration,
    /// The agents that never ping for finishing a turn, only for becoming
    /// blocked. An agent whose workspace stays unknown (herdr cannot say)
    /// pings.
    pub quiet_done: QuietDone,
    /// When Android Auto announces a finished agent's notice.
    pub car_announce: CarAnnounce,
}

impl PushSettings {
    /// The built-in push services only, default timing, a 10 s send
    /// timeout, no quiet workspaces or name prefixes, and the default
    /// announcement policy.
    #[must_use]
    pub fn new(
        vapid: Arc<VapidKey>,
        subject: Subject,
        store: impl Into<PathBuf>,
        sender: Arc<dyn PushSender>,
    ) -> Self {
        Self {
            vapid,
            subject,
            store: store.into(),
            sender,
            hosts: PushHosts::default(),
            timing: PushTiming::default(),
            send_timeout: Duration::from_secs(10),
            quiet_done: QuietDone::default(),
            car_announce: CarAnnounce::default(),
        }
    }
}

impl fmt::Debug for PushSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushSettings")
            .field("subject", &self.subject)
            .field("store", &self.store)
            .field("hosts", &self.hosts)
            .field("timing", &self.timing)
            .field("send_timeout", &self.send_timeout)
            .field("quiet_done", &self.quiet_done)
            .field("car_announce", &self.car_announce)
            .finish_non_exhaustive()
    }
}

/// The push service: subscriptions plus the dispatcher that pings them.
pub struct Push {
    store: SubscriptionStore,
    vapid: Arc<VapidKey>,
    subject: Subject,
    sender: Arc<dyn PushSender>,
    hosts: PushHosts,
    timing: PushTiming,
    send_timeout: Duration,
    quiet_done: QuietDone,
    car_announce: CarAnnounce,
    rng: SystemRandom,
}

impl fmt::Debug for Push {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Push")
            .field("store", &self.store)
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

/// Longest wait for herdr to say where an agent is before a finished turn
/// pings anyway. Well inside the debounce and the longest delay, so a
/// lookup rarely delays a ping it lets through.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// How long after a finished turn pinged its TL;DR's *Decide* line is
/// still sent as a notice. TL;DRs take a settle delay plus a model run.
pub const DECIDE_WINDOW: Duration = Duration::from_mins(15);

/// How asking herdr where an agent runs went.
type Answer = Result<Result<Placement, herdr::Error>, tokio::time::error::Elapsed>;

/// Why one ping could not be prepared for a subscription.
#[derive(Debug, thiserror::Error)]
enum PrepareError {
    #[error(transparent)]
    Encrypt(#[from] EncryptError),
    #[error("signing the VAPID token failed")]
    Sign,
}

impl Push {
    /// Opens the subscription store named in `settings`.
    pub fn open(settings: PushSettings) -> Result<Self, StoreError> {
        Ok(Self {
            store: SubscriptionStore::open(&settings.store)?,
            vapid: settings.vapid,
            subject: settings.subject,
            sender: settings.sender,
            hosts: settings.hosts,
            timing: settings.timing,
            send_timeout: settings.send_timeout,
            quiet_done: settings.quiet_done,
            car_announce: settings.car_announce,
            rng: SystemRandom::new(),
        })
    }

    /// The subscriptions.
    #[must_use]
    pub fn store(&self) -> &SubscriptionStore {
        &self.store
    }

    /// The push services subscriptions may point at.
    #[must_use]
    pub fn hosts(&self) -> &PushHosts {
        &self.hosts
    }

    /// The VAPID public key (the PWA's `applicationServerKey`).
    #[must_use]
    pub fn public_key(&self) -> String {
        self.vapid.public_key()
    }

    /// Starts the dispatcher: follows `hub` and pings devices until
    /// `shutdown`, asking `lookup` where an agent runs when `hub` does not
    /// know yet.
    pub fn spawn(
        self: &Arc<Self>,
        hub: &Arc<Hub>,
        lookup: Arc<dyn PlacementLookup>,
        shutdown: Shutdown,
    ) -> JoinHandle<()> {
        tokio::spawn(Arc::clone(self).dispatch(Arc::clone(hub), lookup, shutdown))
    }

    /// Keeps subscriptions in step with `auth`'s devices: drops those of
    /// devices it no longer knows (revoked while the bridge was down) now,
    /// then starts dropping those of each device it revokes, until
    /// `shutdown`; missed revocations (a backlog) sweep again. Await it at
    /// startup, before [`Self::spawn`] and before serving, so no ping
    /// reaches a device that is gone. `None` (after the sweep, if any) when
    /// `auth` announces no revocations.
    pub async fn spawn_forgetter(
        self: &Arc<Self>,
        auth: Arc<dyn Authenticator>,
        shutdown: Shutdown,
    ) -> Option<JoinHandle<()>> {
        // Listening first: a device revoked from here on is either
        // announced or already missing from the list.
        let revoked = auth.revocations();
        self.sweep(&auth).await;
        revoked
            .map(|revoked| tokio::spawn(Arc::clone(self).forget_revoked(auth, revoked, shutdown)))
    }

    /// Drops the subscriptions of every device `auth` does not know, on the
    /// blocking pool. Nothing if `auth` cannot list its devices.
    async fn sweep(self: &Arc<Self>, auth: &Arc<dyn Authenticator>) {
        let push = Arc::clone(self);
        let auth = Arc::clone(auth);
        let swept =
            tokio::task::spawn_blocking(move || push.store.retain_devices(|| auth.known_devices()))
                .await;
        match swept {
            Ok(Ok(0)) => {}
            Ok(Ok(count)) => {
                tracing::info!(count, "dropped the push subscriptions of unknown devices");
            }
            Ok(Err(error)) => {
                tracing::error!(%error, "cannot drop the push subscriptions of unknown devices");
            }
            Err(error) => tracing::error!(%error, "push store task failed"),
        }
    }

    async fn forget_revoked(
        self: Arc<Self>,
        auth: Arc<dyn Authenticator>,
        mut revoked: broadcast::Receiver<String>,
        mut shutdown: Shutdown,
    ) {
        loop {
            let device = tokio::select! {
                () = shutdown.wait() => return,
                device = revoked.recv() => device,
            };
            match device {
                Ok(device) => {
                    let push = Arc::clone(&self);
                    let forgotten =
                        tokio::task::spawn_blocking(move || push.store.forget_device(&device))
                            .await;
                    match forgotten {
                        Ok(Ok(0)) => {}
                        Ok(Ok(count)) => {
                            tracing::info!(count, "dropped a revoked device's push subscriptions");
                        }
                        Ok(Err(error)) => {
                            tracing::error!(%error, "cannot drop a revoked device's push subscriptions");
                        }
                        Err(error) => tracing::error!(%error, "push store task failed"),
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "missed revocations; sweeping unknown devices");
                    self.sweep(&auth).await;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    async fn dispatch(
        self: Arc<Self>,
        hub: Arc<Hub>,
        lookup: Arc<dyn PlacementLookup>,
        mut shutdown: Shutdown,
    ) {
        let mut feed = hub.subscribe();
        let mut transitions = Transitions::default();
        let mut schedules: HashMap<String, Schedule> = HashMap::new();
        let mut batches: HashMap<String, Batch> = HashMap::new();
        let mut decisions = Decisions::default();
        let mut lookups: JoinSet<(AgentId, Answer)> = JoinSet::new();
        loop {
            let due = schedules.values().filter_map(Schedule::due).min();
            let woke: Vec<(AgentId, Wake)> = tokio::select! {
                () = shutdown.wait() => return,
                event = feed.next() => {
                    decisions.prune(Instant::now());
                    if let Some((agent, decide)) = decisions.observe(&event) {
                        self.send_decide(&agent, &decide, &hub);
                    }
                    transitions
                        .observe(&event)
                        .into_iter()
                        .inspect(|change| {
                            if let Change::Finished(agent) = change {
                                decisions.finishing(agent);
                            }
                        })
                        .filter_map(|change| self.on_change(change, &hub, &lookup, &mut lookups))
                        .collect()
                },
                Some(joined) = lookups.join_next(), if !lookups.is_empty() => match joined {
                    Ok((agent, looked)) => self.looked_up(agent, looked).into_iter().collect(),
                    Err(error) => {
                        tracing::error!(%error, "agent lookup task failed");
                        Vec::new()
                    }
                },
                () = sleep_until(due) => {
                    let now = Instant::now();
                    for (device, schedule) in &mut schedules {
                        if schedule.fire(now) {
                            let mut batch = batches.remove(device).unwrap_or_default();
                            if batch.keep_lasting(&hub) {
                                schedule.sent(now);
                                self.send(device, &batch, &hub);
                            }
                        }
                    }
                    schedules.retain(|_, schedule| !schedule.is_idle(now, &self.timing));
                    batches.retain(|device, _| schedules.contains_key(device));
                    Vec::new()
                }
            };
            if woke.is_empty() {
                continue;
            }
            let now = Instant::now();
            let devices = self.store.devices();
            for (agent, wake) in woke {
                if wake == Wake::Finished {
                    decisions.await_turn_of(&agent, now);
                }
                for device in &devices {
                    batches
                        .entry(device.clone())
                        .or_default()
                        .add(agent.clone(), wake);
                }
            }
            for device in devices {
                schedules
                    .entry(device)
                    .or_default()
                    .trigger(now, &self.timing);
            }
        }
    }

    /// The news `change` is now, if any. A finished turn whose verdict
    /// needs a workspace label `hub` lacks is looked up in herdr (into
    /// `lookups`) and decided when the answer comes.
    fn on_change(
        &self,
        change: Change,
        hub: &Hub,
        lookup: &Arc<dyn PlacementLookup>,
        lookups: &mut JoinSet<(AgentId, Answer)>,
    ) -> Option<(AgentId, Wake)> {
        let agent = match change {
            Change::Blocked(agent) => return Some((agent, Wake::Blocked)),
            Change::Finished(agent) => agent,
        };
        let placement = hub.placement(agent.pane()).unwrap_or_default();
        if let Some(verdict) = self.quiet_done.judge(&placement) {
            return Self::finished(&agent, verdict).then_some((agent, Wake::Finished));
        }
        let lookup = Arc::clone(lookup);
        lookups.spawn(async move {
            let looked = tokio::time::timeout(LOOKUP_TIMEOUT, lookup.placement(agent.pane())).await;
            (agent, looked)
        });
        None
    }

    /// Decides a finished turn after asking herdr where the agent runs.
    fn looked_up(&self, agent: AgentId, looked: Answer) -> Option<(AgentId, Wake)> {
        let verdict = match looked {
            Ok(Ok(placement)) => self
                .quiet_done
                .judge(&placement)
                .unwrap_or(Verdict::WorkspaceUnknown),
            Ok(Err(error)) => {
                tracing::warn!(%agent, %error, "cannot ask herdr for the agent's workspace");
                Verdict::WorkspaceUnknown
            }
            Err(_) => {
                tracing::warn!(%agent, "herdr did not say in time which workspace the agent is in");
                Verdict::WorkspaceUnknown
            }
        };
        Self::finished(&agent, verdict).then_some((agent, Wake::Finished))
    }

    /// Logs the decision on `agent`'s finished turn; true if it pings.
    fn finished(agent: &AgentId, verdict: Verdict) -> bool {
        let reason = verdict.reason();
        if verdict.is_quiet() {
            tracing::info!(%agent, reason, "finished turn is quiet");
        } else {
            tracing::info!(%agent, reason, "finished turn pings");
        }
        !verdict.is_quiet()
    }

    /// Sends one message (one fresh id) to each of `device`'s
    /// subscriptions, each in its own task so a slow push service holds up
    /// nobody: the bare ping to ping subscriptions, the ping plus
    /// `batch`'s notices to notice subscriptions.
    fn send(self: &Arc<Self>, device: &str, batch: &Batch, hub: &Hub) {
        if batch.is_empty() {
            return;
        }
        let Some(id) = self.fresh_id() else {
            return;
        };
        let mut notices: Option<Vec<Notice>> = None;
        for subscription in self.store.of(device) {
            let notices = match subscription.payload {
                PushPayload::Ping => Vec::new(),
                PushPayload::Notice => notices
                    .get_or_insert_with(|| notice::notices(batch, hub, self.car_announce))
                    .clone(),
            };
            let message = notice::message(id.clone(), notices);
            let push = Arc::clone(self);
            tokio::spawn(async move { push.deliver(subscription, &message).await });
        }
    }

    /// Sends `agent`'s *Decide* line at once, to every notice
    /// subscription, unless the agent moved on or its turn was read (the
    /// [`CarAnnounce`] policy then only sets whether the car announces it).
    /// Not scheduled: at most one per finished turn, and it must not hold
    /// back the next ping.
    fn send_decide(self: &Arc<Self>, agent: &AgentId, decide: &str, hub: &Hub) {
        // A decision on a turn the user has read (or that went on) is no
        // longer news: the car would announce it for nothing.
        let now = hub.agent_in(agent.pane());
        if let Some(lapse) = Change::Finished(agent.clone()).lapsed(now.as_ref()) {
            tracing::info!(%agent, reason = lapse.reason(), "decide notice withdrawn");
            return;
        }
        let Some(notice) = notice::decide_notice(agent, decide, hub, self.car_announce) else {
            return;
        };
        let Some(id) = self.fresh_id() else {
            return;
        };
        let message = notice::message(id, vec![notice]);
        for device in self.store.devices() {
            for subscription in self.store.of(&device) {
                if subscription.payload == PushPayload::Notice {
                    let push = Arc::clone(self);
                    let message = message.clone();
                    tokio::spawn(async move { push.deliver(subscription, &message).await });
                }
            }
        }
    }

    /// A fresh message id; `None` (logged) if the random source fails.
    fn fresh_id(&self) -> Option<String> {
        self.ping_id()
            .inspect_err(|error| tracing::error!(%error, "cannot make a ping id"))
            .ok()
    }

    async fn deliver(self: Arc<Self>, subscription: Subscription, message: &PushMessage) {
        let host = subscription.endpoint.host();
        if !self.hosts.permits(&subscription.endpoint) {
            // Stored while its host was configured; kept in case it returns.
            tracing::warn!(%host, "not pinging a push service that is no longer allowed");
            return;
        }
        let request = match self.prepare(&subscription, message) {
            Ok(request) => request,
            Err(PrepareError::Encrypt(EncryptError::InvalidKey)) => {
                tracing::warn!(%host, "dropping a push subscription with an invalid key");
                self.forget(subscription.endpoint).await;
                return;
            }
            Err(error) => {
                tracing::error!(%host, %error, "cannot prepare a push message");
                return;
            }
        };
        let sent = tokio::time::timeout(self.send_timeout, self.sender.send(request)).await;
        match sent {
            Ok(Ok(Delivery::Accepted)) => tracing::debug!(%host, "push ping accepted"),
            Ok(Ok(Delivery::Gone)) => {
                tracing::info!(%host, "push subscription expired; forgetting it");
                self.forget(subscription.endpoint).await;
            }
            Ok(Ok(Delivery::Rejected(status))) => {
                tracing::warn!(%host, %status, "push service refused a ping");
            }
            Ok(Err(error)) => tracing::warn!(%host, %error, "push ping not delivered"),
            Err(_) => tracing::warn!(%host, "push service did not answer in time"),
        }
    }

    /// The encrypted, signed request for one message. The plaintext of a
    /// ping subscription's is only the ping id, never agent content; a
    /// notice subscription's carries its notices, encrypted all the same.
    fn prepare(
        &self,
        subscription: &Subscription,
        message: &PushMessage,
    ) -> Result<PushRequest, PrepareError> {
        let payload = serde_json::to_vec(message).map_err(|_| PrepareError::Sign)?;
        let body = ece::encrypt(
            &subscription.p256dh,
            &subscription.auth,
            &payload,
            &self.rng,
        )?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let authorization = self
            .vapid
            .authorization(
                &subscription.endpoint.origin(),
                &self.subject,
                now,
                &self.rng,
            )
            .map_err(|_| PrepareError::Sign)?;
        Ok(PushRequest {
            endpoint: subscription.endpoint.clone(),
            authorization,
            body,
        })
    }

    async fn forget(self: &Arc<Self>, endpoint: Endpoint) {
        let push = Arc::clone(self);
        let forgotten = tokio::task::spawn_blocking(move || push.store.forget(&endpoint)).await;
        match forgotten {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::error!(%error, "cannot update the push subscription store"),
            Err(error) => tracing::error!(%error, "push subscription store task failed"),
        }
    }

    /// 128 random bits, hex: unguessable and meaningless.
    fn ping_id(&self) -> Result<String, ring::error::Unspecified> {
        let mut bytes = [0u8; 16];
        self.rng.fill(&mut bytes)?;
        Ok(bytes.iter().fold(String::with_capacity(32), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        }))
    }
}

/// Most agents [`Decisions`] remembers a newest turn for; past it, those
/// not waiting are forgotten (that only makes the older-turn filter laxer
/// for them until their next TL;DR or headline).
const MAX_KNOWN_TURNS: usize = 256;

/// A finished turn waiting for its TL;DR.
#[derive(Debug, Clone, Copy)]
struct Wait {
    since: Instant,
    /// Its TL;DR has a later turn than this (the newest known when it
    /// finished, or past an earlier finish still waiting).
    after: Option<TurnIndex>,
    /// Its turn, once its final headline says (a short turn's comes
    /// first): then only that turn's TL;DR is its decision.
    turn: Option<TurnIndex>,
}

/// Finished turns that pinged, waiting for their TL;DR's *Decide* line.
///
/// The bridge does not know a finished turn's number when it pings, so a
/// TL;DR counts as that turn's only if it is newer than every turn known
/// then: a TL;DR the phone asked for meanwhile, of an older turn, is not
/// the decision. Waits and known turns expire with [`DECIDE_WINDOW`] or
/// [`MAX_KNOWN_TURNS`]. Right after a restart, before any TL;DR or
/// headline of the agent was seen, any final TL;DR within the window
/// counts.
#[derive(Debug, Default)]
struct Decisions {
    awaiting: HashMap<AgentId, Wait>,
    /// The newest turn of each agent a final TL;DR or headline was made for.
    known: HashMap<AgentId, TurnIndex>,
    /// What `known` said when each agent's latest finish was observed,
    /// before deciding whether it pings (a workspace lookup can take
    /// seconds, and the turn's own headline must not raise the bar).
    finishing: HashMap<AgentId, Option<TurnIndex>>,
}

impl Decisions {
    /// `agent` just finished a turn that pinged. A finish while an earlier
    /// one still waits moves the bar past that one's turn, so the earlier
    /// turn's TL;DR, arriving late, is not taken for this turn's (a turn
    /// that finished twice then misses its Decide line: silence, not a
    /// wrong announcement).
    fn await_turn_of(&mut self, agent: &AgentId, now: Instant) {
        self.prune(now);
        let known = self
            .finishing
            .remove(agent)
            .unwrap_or_else(|| self.known.get(agent).copied());
        let earlier = self.awaiting.get(agent).map(|wait| {
            wait.turn
                .or(wait.after.map(|after| TurnIndex(after.0.saturating_add(1))))
        });
        let after = known.max(earlier.flatten());
        self.awaiting.insert(
            agent.clone(),
            Wait {
                since: now,
                after,
                turn: None,
            },
        );
    }

    /// `agent` was just seen finishing a turn, which may yet ping: what is
    /// known of its turns now is the bar for [`Self::await_turn_of`].
    fn finishing(&mut self, agent: &AgentId) {
        if self.finishing.len() >= MAX_KNOWN_TURNS {
            self.finishing.clear();
        }
        let known = self.known.get(agent).copied();
        self.finishing.insert(agent.clone(), known);
    }

    /// Takes in one hub event: the *Decide* line of a turn being waited
    /// for, from its final TL;DR within [`DECIDE_WINDOW`]. That turn's
    /// final TL;DR ends the wait, Decide line or not; a provisional one, or
    /// one of another turn, does not.
    fn observe(&mut self, event: &AgentEvent) -> Option<(AgentId, String)> {
        let (agent_id, turn, decide) = match event {
            AgentEvent::TldrReady {
                agent_id,
                turn,
                tldr,
                provisional: false,
            } => (agent_id, *turn, tldr.decide.as_ref()),
            AgentEvent::HeadlineReady {
                agent_id,
                turn,
                provisional: false,
                ..
            } => {
                if let Some(wait) = self.awaiting.get_mut(agent_id)
                    && wait.after.is_none_or(|after| *turn > after)
                    && wait.turn.is_none()
                {
                    wait.turn = Some(*turn);
                }
                self.note(agent_id, *turn);
                return None;
            }
            AgentEvent::AgentRemoved { agent_id } => {
                self.known.remove(agent_id);
                self.finishing.remove(agent_id);
                self.awaiting.remove(agent_id);
                return None;
            }
            _ => return None,
        };
        let waiting = self.awaiting.get(agent_id).copied();
        self.note(agent_id, turn);
        let wait = waiting?;
        let theirs = match wait.turn {
            Some(expected) => turn == expected,
            None => wait.after.is_none_or(|after| turn > after),
        };
        if !theirs {
            return None;
        }
        self.awaiting.remove(agent_id);
        if wait.since.elapsed() >= DECIDE_WINDOW {
            return None;
        }
        let decide = decide.filter(|line| !line.trim().is_empty())?;
        tracing::info!(agent = %agent_id, "finished turn has a decision; notice follows");
        Some((agent_id.clone(), decide.clone()))
    }

    fn note(&mut self, agent: &AgentId, turn: TurnIndex) {
        let known = self.known.entry(agent.clone()).or_default();
        *known = (*known).max(turn);
        if self.known.len() > MAX_KNOWN_TURNS {
            let awaiting = &self.awaiting;
            self.known.retain(|agent, _| awaiting.contains_key(agent));
        }
    }

    /// Drops waits older than [`DECIDE_WINDOW`].
    fn prune(&mut self, now: Instant) {
        self.awaiting
            .retain(|_, wait| now.duration_since(wait.since) < DECIDE_WINDOW);
    }
}

/// Sleeps until `due`, or forever without one.
async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;

    use herdr::{ErrorCode, FakeHerdr, Fault, Herdr, Method};
    use protocol::AgentStatus;

    use super::ece::tests::Receiver;
    use super::sender::tests::FakeSender;
    use super::store::tests::subscription;
    use super::transitions::tests::agent;
    use super::*;
    use crate::shutdown::{self, ShutdownTrigger};
    use crate::testing::eventually;

    struct Rig {
        hub: Arc<Hub>,
        push: Arc<Push>,
        sender: Arc<FakeSender>,
        _trigger: ShutdownTrigger,
        _dir: tempfile::TempDir,
    }

    const TIMING: PushTiming = PushTiming {
        quiet: Duration::from_secs(2),
        max_delay: Duration::from_secs(10),
        min_interval: Duration::from_secs(15),
    };

    /// A hub with the given agents, and a running dispatcher.
    async fn rig(agents: Vec<protocol::Agent>) -> Rig {
        rig_with(agents, &[]).await
    }

    /// [`rig`], with finished turns quiet in the `quiet` workspaces.
    async fn rig_with(agents: Vec<protocol::Agent>, quiet: &[&str]) -> Rig {
        rig_full(agents, quiet, &[], Arc::new(FakeHerdr::new())).await
    }

    /// Orchestrator-style settings: a quiet workspace and agent-name prefixes.
    async fn swarm_rig(herdr: Arc<FakeHerdr>) -> Rig {
        rig_full(Vec::new(), &["agent-swarm"], &["impl-", "rev-"], herdr).await
    }

    /// [`rig_with`], with finished turns also quiet for agents named with
    /// one of `prefixes`, and lookups asking `herdr`.
    async fn rig_full(
        agents: Vec<protocol::Agent>,
        quiet: &[&str],
        prefixes: &[&str],
        herdr: Arc<FakeHerdr>,
    ) -> Rig {
        rig_announcing(agents, quiet, prefixes, herdr, CarAnnounce::default()).await
    }

    /// [`rig_full`], with `announce` as the car announcement policy.
    async fn rig_announcing(
        agents: Vec<protocol::Agent>,
        quiet: &[&str],
        prefixes: &[&str],
        herdr: Arc<FakeHerdr>,
        announce: CarAnnounce,
    ) -> Rig {
        let dir = tempfile::tempdir().expect("tmp");
        let sender = Arc::new(FakeSender::new());
        let mut settings = PushSettings::new(
            Arc::new(vapid::tests::key()),
            "mailto:ops@example.com".parse().expect("subject"),
            dir.path().join(store::STORE_FILE),
            Arc::clone(&sender) as Arc<dyn PushSender>,
        );
        settings.timing = TIMING;
        settings.car_announce = announce;
        settings.quiet_done = QuietDone {
            workspaces: quiet.iter().map(|&label| label.to_owned()).collect(),
            agent_prefixes: prefixes.iter().map(|&prefix| prefix.to_owned()).collect(),
        };
        settings.hosts = PushHosts::new(vec!["push.example.com".parse().expect("host")]);
        let push = Arc::new(Push::open(settings).expect("opens"));
        let hub = Arc::new(Hub::new(16));
        for a in agents {
            hub.upsert(&herdr::PaneId::new(a.id.pane().as_str()), a);
        }
        let (trigger, shutdown) = shutdown::channel();
        push.spawn(&hub, Arc::new(HerdrLookup(Arc::clone(&herdr))), shutdown);
        settle().await;
        Rig {
            hub,
            push,
            sender,
            _trigger: trigger,
            _dir: dir,
        }
    }

    impl Rig {
        fn set(&self, a: protocol::Agent) {
            self.hub
                .upsert(&herdr::PaneId::new(a.id.pane().as_str()), a);
        }

        /// Puts `pane` in the herdr workspace labeled `label`.
        fn label(&self, pane: &str, label: &str) {
            self.place(pane, Some(label), None);
        }

        /// Records where the registry has `pane` (`None`: not known).
        fn place(&self, pane: &str, workspace: Option<&str>, name: Option<&str>) {
            self.hub.set_placement(
                &herdr::PaneId::new(pane),
                Placement {
                    workspace: workspace.map(str::to_owned),
                    name: name.map(str::to_owned),
                },
            );
        }

        /// Subscribes a phone, then has `pane`'s agent work and finish.
        async fn work_then_finish(&self, pane: &str) {
            self.push
                .store()
                .put("phone", subscription("a"))
                .expect("put");
            self.set(agent(pane, AgentStatus::Working, 1));
            settle().await;
            self.set(agent(pane, AgentStatus::Done, 2));
        }

        /// Then has it block.
        async fn block(&self, pane: &str) {
            self.set(agent(pane, AgentStatus::Working, 3));
            self.set(agent(pane, AgentStatus::Blocked, 4));
            advance(TIMING.quiet).await;
        }
    }

    /// Collects log lines (INFO and up) for the rest of the test thread.
    struct Logs {
        lines: Arc<std::sync::Mutex<Vec<u8>>>,
        _guard: tracing::subscriber::DefaultGuard,
    }

    impl Logs {
        fn capture() -> Self {
            let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&lines);
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .with_writer(move || LogWriter(Arc::clone(&sink)))
                .finish();
            Self {
                lines,
                _guard: tracing::subscriber::set_default(subscriber),
            }
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.lines.lock().expect("lock")).into_owned()
        }
    }

    struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// An agent herdr has in a new tab of a new `agent-swarm`
    /// workspace, as the bridge pane id.
    async fn new_swarm_tab(herdr: &FakeHerdr) -> String {
        let workspace = herdr
            .create_workspace("agent-swarm", None)
            .await
            .expect("workspace");
        let tab = herdr
            .create_tab(&workspace.workspace.workspace_id, "t26f", None)
            .await
            .expect("tab");
        let pane = tab.root_pane.pane_id;
        herdr.detect_agent(&pane, "claude", herdr::AgentStatus::Working);
        pane.as_str().to_owned()
    }

    /// Lets spawned tasks run without moving the (paused) clock.
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    async fn advance(by: Duration) {
        tokio::time::sleep(by).await;
        settle().await;
    }

    /// Knows `known` and announces revocations on a one-slot channel, so
    /// a burst of revocations lags.
    #[derive(Debug)]
    struct Revoker {
        known: std::sync::Mutex<BTreeSet<String>>,
        revoked: broadcast::Sender<String>,
    }

    impl Authenticator for Revoker {
        fn authenticate<'a>(
            &'a self,
            _: crate::server::AuthRequest<'a>,
        ) -> crate::server::AuthFuture<'a> {
            Box::pin(async { Err(crate::server::AuthRejection::Unauthenticated) })
        }

        fn revocations(&self) -> Option<broadcast::Receiver<String>> {
            Some(self.revoked.subscribe())
        }

        fn known_devices(&self) -> Option<BTreeSet<String>> {
            Some(self.known.lock().expect("lock").clone())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_forgetter_sweeps_at_start_and_after_missed_revocations() {
        let r = rig(Vec::new()).await;
        for device in ["kept", "gone", "a", "b", "c"] {
            r.push
                .store()
                .put(device, subscription(device))
                .expect("put");
        }
        let names = |list: &[&str]| list.iter().map(|&d| d.to_owned()).collect();
        let auth = Arc::new(Revoker {
            known: std::sync::Mutex::new(names(&["kept", "a", "b", "c"])),
            revoked: broadcast::Sender::new(1),
        });
        let (_trigger, shutdown) = shutdown::channel();
        let forgetter = r
            .push
            .spawn_forgetter(Arc::clone(&auth) as Arc<dyn Authenticator>, shutdown)
            .await;
        assert!(forgetter.is_some());
        assert_eq!(
            r.push.store().devices(),
            ["a", "b", "c", "kept"],
            "swept at start"
        );

        // Three revocations before the forgetter runs: it lags, and sweeps.
        *auth.known.lock().expect("lock") = names(&["kept"]);
        for device in ["a", "b", "c"] {
            let _ = auth.revoked.send(device.to_owned());
        }
        eventually("the backlog is swept", || {
            r.push.store().devices() == ["kept"]
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_payload_is_an_opaque_ping_and_never_agent_text() {
        let secret = |s: &str| format!("SECRET-{s}-rm -rf ~/project");
        let mut working = agent("w1:p1", AgentStatus::Working, 1);
        working.title = secret("title");
        working.repo = secret("repo");
        working.worktree = Some(secret("worktree"));
        working.headline = Some(secret("headline"));
        let r = rig(vec![working.clone()]).await;
        let receiver = Receiver::new();
        let sub: Subscription = serde_json::from_value(serde_json::json!({
            "endpoint": "https://push.example.com/phone",
            "keys": {"p256dh": receiver.p256dh(), "auth": receiver.auth()},
        }))
        .expect("valid");
        r.push.store().put("phone", sub).expect("put");

        let mut blocked = working;
        blocked.status = AgentStatus::Blocked;
        blocked.seq = protocol::Seq(2);
        r.set(blocked);
        advance(TIMING.quiet).await;

        let sent = r.sender.sent();
        assert_eq!(sent.len(), 1);
        let request = &sent[0];
        assert_eq!(request.endpoint.as_str(), "https://push.example.com/phone");
        let plaintext = receiver.decrypt(&request.body);
        let text = String::from_utf8(plaintext).expect("utf-8");
        let json: serde_json::Value = serde_json::from_str(&text).expect("json");
        let object = json.as_object().expect("object");
        assert_eq!(object.keys().collect::<Vec<_>>(), ["ping"]);
        let id = object["ping"].as_str().expect("string");
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        let headers = format!("{:?}", request.headers());
        for needle in ["SECRET", "rm -rf", "w1:p1", "title", "repo", "blocked"] {
            assert!(!text.contains(needle), "payload leaks {needle:?}: {text}");
        }
        for needle in ["SECRET", "rm -rf"] {
            assert!(
                !headers.contains(needle),
                "headers leak {needle:?}: {headers}"
            );
        }
        assert!(headers.contains("aes128gcm"), "{headers}");
    }

    /// Subscribes the phone at `https://push.example.com/<name>` with
    /// `payload` and fresh keys (replacing that endpoint's subscription):
    /// the receiver that decrypts the next message sent there.
    fn subscribe(r: &Rig, name: &str, payload: &str) -> Receiver {
        let receiver = Receiver::new();
        let sub = serde_json::from_value(serde_json::json!({
            "endpoint": format!("https://push.example.com/{name}"),
            "keys": {"p256dh": receiver.p256dh(), "auth": receiver.auth()},
            "payload": payload,
        }))
        .expect("valid");
        r.push.store().put("phone", sub).expect("put");
        receiver
    }

    /// The requests sent to `name`'s endpoint.
    fn sent_to(r: &Rig, name: &str) -> Vec<PushRequest> {
        let endpoint = format!("https://push.example.com/{name}");
        r.sender
            .sent()
            .into_iter()
            .filter(|request| request.endpoint.as_str() == endpoint)
            .collect()
    }

    /// The latest message to `name`, decrypted.
    fn latest(r: &Rig, name: &str, receiver: Receiver) -> PushMessage {
        let request = sent_to(r, name).pop().expect("a message");
        assert!(format!("{:?}", request.headers()).contains("aes128gcm"));
        serde_json::from_slice(&receiver.decrypt(&request.body)).expect("a message")
    }

    fn tldr_ready(pane: &str, decide: Option<&str>, provisional: bool) -> AgentEvent {
        tldr_of_turn(pane, 1, decide, provisional)
    }

    fn tldr_of_turn(pane: &str, turn: u32, decide: Option<&str>, provisional: bool) -> AgentEvent {
        AgentEvent::TldrReady {
            agent_id: pane.parse().expect("id"),
            turn: TurnIndex(turn),
            tldr: protocol::Tldr {
                outcome: "Tests pass.".to_owned(),
                changed: Vec::new(),
                decide: decide.map(str::to_owned),
                risks: Vec::new(),
            },
            provisional,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn notice_subscriptions_get_encrypted_notices_and_pings_stay_bare() {
        let mut working = agent("w1:p1", AgentStatus::Working, 1);
        working.title = "widget".to_owned();
        let r = rig(vec![working.clone()]).await;
        let native = subscribe(&r, "native", "notice");
        let pwa = subscribe(&r, "pwa", "ping");

        let mut blocked = working;
        blocked.status = AgentStatus::Blocked;
        blocked.seq = protocol::Seq(2);
        r.set(blocked);
        let mut pending = protocol::PendingInput::unparsed_dialog(
            protocol::PendingId::new("screen-2-abc").expect("id"),
        );
        pending.prompt = "Run `pnpm test`?".to_owned();
        r.hub
            .set_pending(&"w1:p1".parse().expect("id"), Some(pending));
        advance(TIMING.quiet).await;

        assert_eq!(sent_to(&r, "native").len(), 1);
        let body = sent_to(&r, "native")[0].body.clone();
        assert!(
            !String::from_utf8_lossy(&body).contains("pnpm"),
            "only ciphertext on the wire"
        );
        let message = latest(&r, "native", native);
        assert_eq!(message.ping.len(), 32);
        let notice = &message.notices[0];
        assert_eq!(notice.name, "widget");
        assert_eq!(notice.state, protocol::NoticeState::Blocked);
        assert_eq!(notice.line.as_deref(), Some("Run `pnpm test`?"));
        assert_eq!(
            notice.pending_id.as_ref().map(protocol::PendingId::as_str),
            Some("screen-2-abc")
        );
        assert!(notice.announce);

        assert_eq!(sent_to(&r, "pwa").len(), 1);
        let ping = serde_json::to_value(latest(&r, "pwa", pwa)).expect("json");
        assert_eq!(
            ping.as_object().expect("object").keys().collect::<Vec<_>>(),
            ["ping"],
            "the PWA gets the bare ping"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_decide_line_follows_a_finished_turn_for_notice_subscriptions_only() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Working, 1)]).await;
        let native = subscribe(&r, "native", "notice");
        subscribe(&r, "pwa", "ping");

        let mut done = agent("w1:p1", AgentStatus::Done, 2);
        done.headline = Some("Fixed the flaky test".to_owned());
        r.set(done);
        advance(TIMING.quiet).await;
        let finished = latest(&r, "native", native);
        assert_eq!(finished.notices[0].state, protocol::NoticeState::Finished);
        assert!(
            !finished.notices[0].announce,
            "silent in the car until it has a decision"
        );
        assert_eq!(sent_to(&r, "pwa").len(), 1);

        let native = subscribe(&r, "native", "notice");
        // A provisional TL;DR is not the turn's: still waiting.
        r.hub.publish(tldr_ready("w1:p1", Some("Ignore me"), true));
        r.hub
            .publish(tldr_ready("w1:p1", Some("Merge PR #12?"), false));
        advance(TIMING.min_interval).await;
        assert_eq!(sent_to(&r, "native").len(), 2);
        let decided = latest(&r, "native", native);
        let notice = &decided.notices[0];
        assert_eq!(notice.decide.as_deref(), Some("Merge PR #12?"));
        assert_eq!(notice.line.as_deref(), Some("Fixed the flaky test"));
        assert!(notice.announce);
        assert_eq!(sent_to(&r, "pwa").len(), 1, "no second ping for the PWA");

        // Once per finished turn, and never for an agent that did not ping.
        r.hub
            .publish(tldr_ready("w1:p1", Some("Merge PR #12?"), false));
        r.hub.publish(tldr_ready("w1:p2", Some("Unrelated"), false));
        advance(TIMING.min_interval * 2).await;
        assert_eq!(sent_to(&r, "native").len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_older_turns_tldr_is_not_the_decision_of_the_turn_that_finished() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Working, 1)]).await;
        // Turn 5's headline came with the previous finish.
        r.hub.publish(AgentEvent::HeadlineReady {
            agent_id: "w1:p1".parse().expect("id"),
            turn: TurnIndex(5),
            headline: "Earlier work".to_owned(),
            provisional: false,
        });
        subscribe(&r, "native", "notice");
        r.set(agent("w1:p1", AgentStatus::Done, 2));
        advance(TIMING.quiet).await;
        assert_eq!(sent_to(&r, "native").len(), 1);
        // Fresh keys for the next message (a receiver decrypts only once).
        let native = subscribe(&r, "native", "notice");
        // The phone asks for turn 3's TL;DR meanwhile: not this decision.
        r.hub
            .publish(tldr_of_turn("w1:p1", 3, Some("Old question?"), false));
        r.hub
            .publish(tldr_of_turn("w1:p1", 5, Some("Older still?"), false));
        settle().await;
        assert_eq!(sent_to(&r, "native").len(), 1, "older turns are ignored");
        r.hub
            .publish(tldr_of_turn("w1:p1", 6, Some("Merge PR #12?"), false));
        settle().await;
        assert_eq!(sent_to(&r, "native").len(), 2);
        let message = latest(&r, "native", native);
        assert_eq!(message.notices[0].decide.as_deref(), Some("Merge PR #12?"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_decide_line_does_not_hold_back_the_next_ping() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Working, 1)]).await;
        subscribe(&r, "native", "notice");
        subscribe(&r, "pwa", "ping");
        r.set(agent("w1:p1", AgentStatus::Done, 2));
        advance(TIMING.quiet).await;
        assert_eq!(sent_to(&r, "pwa").len(), 1);
        advance(TIMING.min_interval).await;
        r.hub
            .publish(tldr_ready("w1:p1", Some("Merge PR #12?"), false));
        settle().await;
        assert_eq!(sent_to(&r, "native").len(), 2, "sent at once");
        // A dialog right after still pings after the usual quiet time.
        r.set(agent("w1:p1", AgentStatus::Working, 3));
        r.set(agent("w1:p1", AgentStatus::Blocked, 4));
        advance(TIMING.quiet).await;
        assert_eq!(sent_to(&r, "pwa").len(), 2, "not delayed by the decision");
        assert_eq!(sent_to(&r, "native").len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn never_announcing_still_brings_the_decision_to_the_phone() {
        let r = rig_announcing(
            vec![agent("w1:p1", AgentStatus::Working, 1)],
            &[],
            &[],
            Arc::new(FakeHerdr::new()),
            CarAnnounce::Never,
        )
        .await;
        subscribe(&r, "native", "notice");
        r.set(agent("w1:p1", AgentStatus::Done, 2));
        advance(TIMING.quiet).await;
        let native = subscribe(&r, "native", "notice");
        r.hub
            .publish(tldr_ready("w1:p1", Some("Merge PR #12?"), false));
        settle().await;
        let message = latest(&r, "native", native);
        let notice = &message.notices[0];
        assert_eq!(notice.decide.as_deref(), Some("Merge PR #12?"));
        assert!(!notice.announce, "silent in the car");
    }

    fn headline_of_turn(pane: &str, turn: u32) -> AgentEvent {
        AgentEvent::HeadlineReady {
            agent_id: pane.parse().expect("id"),
            turn: TurnIndex(turn),
            headline: "Did a thing".to_owned(),
            provisional: false,
        }
    }

    fn decided(decisions: &mut Decisions, event: &AgentEvent) -> Option<String> {
        decisions.observe(event).map(|(_, decide)| decide)
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_tldr_of_an_earlier_finish_is_not_the_newer_turns_decision() {
        let a: AgentId = "w1:p1".parse().expect("id");
        let mut d = Decisions::default();
        d.observe(&headline_of_turn("w1:p1", 4));
        // Turn 5 finishes, then turn 6, before turn 5's TL;DR is ready.
        d.await_turn_of(&a, Instant::now());
        d.await_turn_of(&a, Instant::now());
        assert_eq!(
            decided(&mut d, &tldr_of_turn("w1:p1", 5, Some("Old?"), false)),
            None
        );
        assert_eq!(
            decided(&mut d, &tldr_of_turn("w1:p1", 6, Some("New?"), false)).as_deref(),
            Some("New?")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_turns_headline_names_the_turn_its_decision_must_be_of() {
        let a: AgentId = "w1:p1".parse().expect("id");
        let mut d = Decisions::default();
        d.observe(&headline_of_turn("w1:p1", 2));
        d.await_turn_of(&a, Instant::now());
        // A short turn: its headline comes, no TL;DR of its own.
        d.observe(&headline_of_turn("w1:p1", 7));
        assert_eq!(
            decided(&mut d, &tldr_of_turn("w1:p1", 5, Some("Old?"), false)),
            None
        );
        assert_eq!(
            decided(&mut d, &tldr_of_turn("w1:p1", 7, Some("This one?"), false)).as_deref(),
            Some("This one?")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_turns_own_headline_during_a_workspace_lookup_does_not_raise_the_bar() {
        let a: AgentId = "w1:p1".parse().expect("id");
        let mut d = Decisions::default();
        d.observe(&headline_of_turn("w1:p1", 4));
        d.finishing(&a);
        // While herdr is asked where the agent runs, turn 5's headline lands.
        d.observe(&headline_of_turn("w1:p1", 5));
        d.await_turn_of(&a, Instant::now());
        assert_eq!(
            decided(&mut d, &tldr_of_turn("w1:p1", 5, Some("Merge?"), false)).as_deref(),
            Some("Merge?")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waits_and_known_turns_stay_bounded() {
        let mut d = Decisions::default();
        for n in 0..(MAX_KNOWN_TURNS + 10) {
            d.observe(&headline_of_turn(&format!("w{n}:p1"), 1));
        }
        assert!(d.known.len() <= MAX_KNOWN_TURNS + 1, "{}", d.known.len());
        let a: AgentId = "w1:p1".parse().expect("id");
        d.await_turn_of(&a, Instant::now());
        tokio::time::advance(DECIDE_WINDOW).await;
        d.prune(Instant::now());
        assert!(d.awaiting.is_empty(), "expired waits are dropped");
    }

    #[tokio::test(start_paused = true)]
    async fn a_turn_without_a_decision_is_never_announced_by_default() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Working, 1)]).await;
        let native = subscribe(&r, "native", "notice");
        r.set(agent("w1:p1", AgentStatus::Done, 2));
        advance(TIMING.quiet).await;
        r.hub.publish(tldr_ready("w1:p1", None, false));
        advance(TIMING.min_interval * 2).await;
        assert_eq!(
            sent_to(&r, "native").len(),
            1,
            "no decision, no second notice"
        );
        let message = latest(&r, "native", native);
        assert!(!message.notices[0].announce);
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_changes_is_one_ping_per_subscription() {
        let r = rig(vec![
            agent("w1:p1", AgentStatus::Working, 1),
            agent("w1:p2", AgentStatus::Working, 1),
        ])
        .await;
        r.push.store().put("phone", subscription("a")).expect("put");
        r.push.store().put("phone", subscription("b")).expect("put");
        r.push
            .store()
            .put("laptop", subscription("c"))
            .expect("put");

        r.set(agent("w1:p1", AgentStatus::Blocked, 2));
        advance(Duration::from_millis(500)).await;
        r.set(agent("w1:p2", AgentStatus::Done, 2));
        advance(Duration::from_millis(500)).await;
        r.set(agent("w1:p1", AgentStatus::Blocked, 3));
        advance(Duration::from_millis(1900)).await;
        assert!(r.sender.sent().is_empty(), "still debouncing");
        advance(Duration::from_millis(200)).await;

        let mut endpoints: Vec<String> = r
            .sender
            .sent()
            .iter()
            .map(|req| req.endpoint.as_str().to_owned())
            .collect();
        endpoints.sort();
        assert_eq!(
            endpoints,
            [
                "https://push.example.com/a",
                "https://push.example.com/b",
                "https://push.example.com/c"
            ]
        );
        advance(Duration::from_secs(60)).await;
        assert_eq!(r.sender.sent().len(), 3, "nothing more without a trigger");
    }

    #[tokio::test(start_paused = true)]
    async fn silent_changes_and_unsubscribed_devices_send_nothing() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Blocked, 1)]).await;
        // Blocked before the dispatcher started: the baseline, no ping.
        r.set(agent("w1:p1", AgentStatus::Blocked, 2));
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "nobody subscribed");

        r.push.store().put("phone", subscription("a")).expect("put");
        r.set(agent("w1:p1", AgentStatus::Working, 3));
        let mut renamed = agent("w1:p1", AgentStatus::Working, 3);
        renamed.title = "renamed".to_owned();
        r.set(renamed);
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "working and renames are silent");
    }

    #[tokio::test(start_paused = true)]
    async fn agents_already_blocked_at_startup_do_not_ping() {
        // As after a bridge restart: the dispatcher's first snapshot is
        // empty and herdr's first sync arrives as updates.
        let r = rig(Vec::new()).await;
        r.push.store().put("phone", subscription("a")).expect("put");
        r.set(agent("w1:p1", AgentStatus::Blocked, 7));
        r.set(agent("w1:p2", AgentStatus::Done, 3));
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "baselines only");

        r.set(agent("w1:p1", AgentStatus::Blocked, 8));
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1, "a new dialog pings");
    }

    #[tokio::test(start_paused = true)]
    async fn services_that_are_no_longer_allowed_are_not_pinged() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Working, 1)]).await;
        let stored: Subscription = serde_json::from_value(serde_json::json!({
            "endpoint": "https://push.removed.example/x",
            "keys": {"p256dh": Receiver::new().p256dh(), "auth": Receiver::new().auth()},
        }))
        .expect("valid");
        r.push.store().put("phone", stored).expect("put");
        r.set(agent("w1:p1", AgentStatus::Blocked, 2));
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "not an allowed host");
        assert_eq!(r.push.store().devices(), ["phone"], "kept, not forgotten");
    }

    #[tokio::test(start_paused = true)]
    async fn pings_to_one_device_are_rate_limited() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Working, 1)]).await;
        r.push.store().put("phone", subscription("a")).expect("put");
        r.set(agent("w1:p1", AgentStatus::Blocked, 2));
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1);
        r.set(agent("w1:p1", AgentStatus::Working, 3));
        r.set(agent("w1:p1", AgentStatus::Done, 4));
        advance(Duration::from_secs(10)).await;
        assert_eq!(r.sender.sent().len(), 1, "within the min interval");
        advance(TIMING.min_interval).await;
        assert_eq!(r.sender.sent().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dialog_answered_before_the_ping_is_due_sends_nothing() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Working, 1)]).await;
        r.push.store().put("phone", subscription("a")).expect("put");
        let logs = Logs::capture();
        r.set(agent("w1:p1", AgentStatus::Blocked, 2));
        advance(Duration::from_millis(500)).await;
        r.set(agent("w1:p1", AgentStatus::Working, 3));
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "answered: nothing to tell");
        let text = logs.text();
        assert!(text.contains("ping reason lapsed"), "{text}");
        assert!(text.contains("working again"), "{text}");
        assert!(text.contains("ping withdrawn"), "{text}");
        assert!(!text.contains("title of"), "no agent content: {text}");

        // Flapping blocked → working inside the window: still nothing.
        for seq in [4, 6, 8] {
            r.set(agent("w1:p1", AgentStatus::Blocked, seq));
            advance(Duration::from_millis(300)).await;
            r.set(agent("w1:p1", AgentStatus::Working, seq + 1));
            advance(Duration::from_millis(300)).await;
        }
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "a flap is not news");

        // A withdrawn ping holds nothing back: a real block pings at once.
        r.set(agent("w1:p1", AgentStatus::Blocked, 10));
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1, "still blocked: pings");
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_turn_taken_up_again_or_read_sends_nothing() {
        let r = rig(vec![
            agent("w1:p1", AgentStatus::Working, 1),
            agent("w1:p2", AgentStatus::Working, 1),
        ])
        .await;
        r.push.store().put("phone", subscription("a")).expect("put");
        r.set(agent("w1:p1", AgentStatus::Done, 2));
        advance(Duration::from_secs(1)).await;
        r.set(agent("w1:p1", AgentStatus::Working, 3));
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "prompted again before the ping");

        r.set(agent("w1:p2", AgentStatus::Done, 2));
        advance(Duration::from_secs(1)).await;
        r.set(agent("w1:p2", AgentStatus::Idle, 3));
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "read at the desk");

        r.set(agent("w1:p2", AgentStatus::Working, 4));
        r.set(agent("w1:p2", AgentStatus::Idle, 5));
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "finished in view: seen already");

        r.set(agent("w1:p1", AgentStatus::Done, 4));
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1, "an unread finished turn pings");
    }

    #[tokio::test(start_paused = true)]
    async fn one_lasting_reason_carries_the_ping() {
        let r = rig(vec![
            agent("w1:p1", AgentStatus::Working, 1),
            agent("w1:p2", AgentStatus::Working, 1),
        ])
        .await;
        r.push.store().put("phone", subscription("a")).expect("put");
        r.set(agent("w1:p1", AgentStatus::Blocked, 2));
        r.set(agent("w1:p2", AgentStatus::Blocked, 2));
        advance(Duration::from_millis(500)).await;
        r.set(agent("w1:p1", AgentStatus::Working, 3));
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1, "w1:p2 still waits");

        // Nothing new since: no repeat, however long w1:p2 waits.
        advance(Duration::from_secs(120)).await;
        assert_eq!(r.sender.sent().len(), 1, "no repeat without a change");
    }

    #[tokio::test(start_paused = true)]
    async fn lapsed_news_sends_no_notice_either() {
        let r = rig(vec![
            agent("w1:p1", AgentStatus::Working, 1),
            agent("w1:p2", AgentStatus::Working, 1),
        ])
        .await;
        subscribe(&r, "native", "notice");
        r.set(agent("w1:p1", AgentStatus::Blocked, 2));
        advance(Duration::from_millis(500)).await;
        r.set(agent("w1:p1", AgentStatus::Working, 3));
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "neither ping nor notice");

        // Of two dialogs, only the one still open is a notice.
        let native = subscribe(&r, "native", "notice");
        r.set(agent("w1:p1", AgentStatus::Blocked, 4));
        r.set(agent("w1:p2", AgentStatus::Blocked, 2));
        advance(Duration::from_millis(500)).await;
        r.set(agent("w1:p1", AgentStatus::Working, 5));
        advance(TIMING.quiet).await;
        let message = latest(&r, "native", native);
        let named: Vec<String> = message
            .notices
            .iter()
            .map(|n| n.agent_id.to_string())
            .collect();
        assert_eq!(named, ["w1:p2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_decision_on_a_turn_already_read_is_not_announced() {
        let r = rig_announcing(
            vec![agent("w1:p1", AgentStatus::Working, 1)],
            &[],
            &[],
            Arc::new(FakeHerdr::new()),
            CarAnnounce::Decide,
        )
        .await;
        subscribe(&r, "native", "notice");
        r.set(agent("w1:p1", AgentStatus::Done, 2));
        advance(TIMING.quiet).await;
        assert_eq!(sent_to(&r, "native").len(), 1, "the finished turn");

        // Read at the desk before its TL;DR came: the decision is not news.
        let logs = Logs::capture();
        r.set(agent("w1:p1", AgentStatus::Idle, 3));
        r.hub
            .publish(tldr_ready("w1:p1", Some("Merge PR #12?"), false));
        advance(TIMING.min_interval).await;
        assert_eq!(sent_to(&r, "native").len(), 1, "no decide notice");
        let text = logs.text();
        assert!(text.contains("decide notice withdrawn"), "{text}");
        assert!(!text.contains("Merge PR"), "no content: {text}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_gone_before_the_ping_sends_nothing() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Working, 1)]).await;
        r.push.store().put("phone", subscription("a")).expect("put");
        r.set(agent("w1:p1", AgentStatus::Blocked, 2));
        r.hub.remove(&herdr::PaneId::new("w1:p1"));
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "gone: nothing to tell");
    }

    #[tokio::test(start_paused = true)]
    async fn a_gone_subscription_is_forgotten() {
        let r = rig(vec![agent("w1:p1", AgentStatus::Working, 1)]).await;
        r.push.store().put("phone", subscription("a")).expect("put");
        *r.sender.answer.lock().expect("lock") = Delivery::Gone;
        r.set(agent("w1:p1", AgentStatus::Blocked, 2));
        advance(TIMING.quiet).await;
        // The store update runs on the blocking pool, outside paused time.
        for _ in 0..100 {
            if r.push.store().devices().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(r.sender.sent().len(), 1);
        assert_eq!(r.push.store().devices(), Vec::<String>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn swarm_agents_ping_when_blocked_but_not_when_finishing() {
        let r = rig_with(
            vec![agent("w2:p1", AgentStatus::Working, 1)],
            &["agent-swarm"],
        )
        .await;
        r.label("w2:p1", "agent-swarm");
        r.push.store().put("phone", subscription("a")).expect("put");

        r.set(agent("w2:p1", AgentStatus::Done, 2));
        advance(Duration::from_secs(30)).await;
        assert!(
            r.sender.sent().is_empty(),
            "a swarm agent finishing is quiet"
        );

        r.set(agent("w2:p1", AgentStatus::Blocked, 3));
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1, "a blocked swarm agent pings");
    }

    #[tokio::test(start_paused = true)]
    async fn agents_in_other_or_unknown_workspaces_ping_when_finishing() {
        let r = rig_with(
            vec![
                agent("w1:p1", AgentStatus::Working, 1),
                agent("w3:p1", AgentStatus::Working, 1),
            ],
            &["agent-swarm"],
        )
        .await;
        r.label("w1:p1", "home");
        r.push.store().put("phone", subscription("a")).expect("put");

        r.set(agent("w1:p1", AgentStatus::Done, 2));
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1, "another workspace pings");

        r.set(agent("w3:p1", AgentStatus::Done, 2));
        advance(TIMING.min_interval).await;
        assert_eq!(
            r.sender.sent().len(),
            2,
            "a workspace herdr cannot tell either pings"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_quiet_workspaces_means_every_finish_pings() {
        let r = rig_with(vec![agent("w2:p1", AgentStatus::Working, 1)], &[]).await;
        r.label("w2:p1", "agent-swarm");
        r.push.store().put("phone", subscription("a")).expect("put");
        r.set(agent("w2:p1", AgentStatus::Done, 2));
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_swarm_tab_finishing_before_its_label_is_known_is_quiet() {
        let herdr = Arc::new(FakeHerdr::new());
        let pane = new_swarm_tab(&herdr).await;
        let r = swarm_rig(herdr).await;
        // The registry has the agent but no label for it yet (no full
        // refresh since the tab appeared), and no name to go by.
        let logs = Logs::capture();
        r.work_then_finish(&pane).await;
        assert_eq!(
            r.hub
                .placement(&protocol::PaneId::new(pane.as_str()).expect("pane")),
            Some(Placement::default())
        );
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "looked up: a swarm agent");
        let text = logs.text();
        assert!(text.contains("finished turn is quiet"), "{text}");
        assert!(text.contains("quiet workspace"), "{text}");
        assert!(text.contains(&pane), "{text}");

        r.block(&pane).await;
        assert_eq!(r.sender.sent().len(), 1, "blocked: pings");
    }

    #[tokio::test(start_paused = true)]
    async fn orchestrator_named_agents_are_quiet_whatever_their_workspace() {
        // herdr has neither pane: nothing to look up, the name decides.
        let r = swarm_rig(Arc::new(FakeHerdr::new())).await;
        let logs = Logs::capture();
        for (pane, name) in [("w8:p1", "impl-t26f"), ("w8:p2", "rev-t26f-r1")] {
            r.place(pane, None, Some(name));
            r.work_then_finish(pane).await;
        }
        advance(Duration::from_secs(30)).await;
        assert!(r.sender.sent().is_empty(), "orchestrator agents finishing");
        let text = logs.text();
        assert_eq!(text.matches("quiet name prefix").count(), 2, "{text}");

        r.block("w8:p1").await;
        assert_eq!(r.sender.sent().len(), 1, "a blocked implementer pings");
        advance(TIMING.min_interval).await;
        r.block("w8:p2").await;
        assert_eq!(r.sender.sent().len(), 2, "a blocked reviewer pings");
    }

    #[tokio::test(start_paused = true)]
    async fn the_users_agents_ping_when_finishing() {
        let r = swarm_rig(Arc::new(FakeHerdr::new())).await;
        let logs = Logs::capture();
        // Started from the phone, and one of the user's own sessions.
        r.place("w7:p1", Some("passporter"), Some("pp-1a2b3c"));
        r.place("w5:p1", Some("gadget"), Some("circus"));
        r.work_then_finish("w7:p1").await;
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1, "a phone-started agent pings");
        r.work_then_finish("w5:p1").await;
        advance(TIMING.min_interval).await;
        assert_eq!(r.sender.sent().len(), 2, "a named session pings");
        let text = logs.text();
        assert_eq!(text.matches("finished turn pings").count(), 2, "{text}");
        assert_eq!(text.matches("user agent").count(), 2, "{text}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_label_herdr_cannot_settle_pings_and_says_so() {
        let herdr = Arc::new(FakeHerdr::new());
        let pane = new_swarm_tab(&herdr).await;
        let r = swarm_rig(Arc::clone(&herdr)).await;
        let logs = Logs::capture();
        herdr.fail_next(Method::AgentGet, Fault::Reject(ErrorCode::InternalError));
        r.work_then_finish(&pane).await;
        advance(TIMING.quiet).await;
        assert_eq!(r.sender.sent().len(), 1, "fails open");
        let text = logs.text();
        assert!(
            text.contains("cannot ask herdr for the agent's workspace"),
            "{text}"
        );
        assert!(text.contains("workspace unknown"), "{text}");
        assert!(!text.contains("title of"), "no agent content: {text}");
    }
}
