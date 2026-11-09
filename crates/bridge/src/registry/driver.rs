//! Keeps the [`Hub`] in step with herdr.
//!
//! herdr reports agent status changes **per pane**, so discovery is two-level:
//!
//! 1. one *lifecycle* subscription (`pane.created`, `pane.agent_detected`,
//!    `pane.closed`, `pane.exited`, `tab.closed`, `workspace.closed`) tells
//!    the driver when agents come and go;
//! 2. one *watcher* task per agent pane holds a `pane.agent_status_changed`
//!    subscription and asks the driver to refresh that pane on every change.
//!
//! Events are only triggers: the driver re-reads the agent with `agent.get`,
//! because events do not carry `state_change_seq`, which the stale guard
//! needs exactly. Each subscription is followed by a read, so a change
//! between reading and subscribing is never lost. Refresh requests are
//! coalesced (a pane is re-read at most once per drain, however many events
//! it produced), the reads of one drain run concurrently (at most
//! [`MAX_CONCURRENT_READS`]), and a pane waiting out a retry backoff is not
//! read early because of an event.
//!
//! Failures are contained where they happen:
//! - a pane whose `agent.get` fails (timeout, `agent_launch_pending`, ...) is
//!   retried on its own with backoff until it reads or is gone; the session
//!   carries on;
//! - a failed periodic resync is retried with backoff; the session carries on;
//! - only the lifecycle stream ending or failing (or the initial resync
//!   failing) ends the session: the driver then resubscribes with backoff and
//!   resyncs.
//!
//! A full `agent.list` resync also runs every
//! [`RegistrySettings::resync_every`] and when a tab or workspace closes.
//! That catches what herdr has no event for (e.g. a session id appearing).
//!
//! Each agent's workspace label comes from `workspace.list`, read on every
//! resync and when an agent turns up in a workspace not seen yet (at most
//! once per workspace between resyncs). It is handed to the hub with the
//! agent's herdr name ([`Placement`]) before the agent, never to clients. A
//! herdr that cannot list workspaces only leaves the labels unknown (the
//! push dispatcher then asks herdr itself).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use herdr::{ErrorCode, Event, EventStream, Herdr, PaneId, Subscription, Target, WorkspaceId};
use protocol::Timestamp;
use tokio::sync::{Notify, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, Interval, MissedTickBehavior};

use super::backoff::Backoff;
use super::hub::{Hub, Placement};
use super::mapping;
use super::quiet::QuietDone;
use super::repo::RepoCache;
use crate::shutdown::Shutdown;

/// At most this many `agent.get` calls are in flight at once.
const MAX_CONCURRENT_READS: usize = 4;

/// How the registry paces itself against herdr.
#[derive(Debug, Clone)]
pub struct RegistrySettings {
    /// Full `agent.list` resync interval, as a safety net under the events.
    pub resync_every: Duration,
    /// First retry delay after herdr fails or a stream ends.
    pub retry_min: Duration,
    /// Longest retry delay.
    pub retry_max: Duration,
    /// Which agents the list flags `quietDone`.
    pub quiet_done: QuietDone,
}

impl Default for RegistrySettings {
    fn default() -> Self {
        Self {
            resync_every: Duration::from_secs(30),
            retry_min: Duration::from_millis(250),
            retry_max: Duration::from_secs(30),
            quiet_done: QuietDone::default(),
        }
    }
}

/// Starts the registry: a task that fills `hub` from herdr and keeps it live
/// until `shutdown`.
pub fn spawn<H>(
    herdr: Arc<H>,
    hub: Arc<Hub>,
    settings: RegistrySettings,
    shutdown: Shutdown,
) -> JoinHandle<()>
where
    H: Herdr + 'static,
{
    let driver = Driver {
        herdr,
        hub,
        settings,
        watchers: HashMap::new(),
        requests: Arc::new(RefreshQueue::default()),
        retries: HashMap::new(),
        repos: RepoCache::default(),
        cwds: HashMap::new(),
        labels: HashMap::new(),
        unlabeled: HashSet::new(),
    };
    tokio::spawn(driver.run(shutdown))
}

/// The lifecycle events that add or remove agents.
fn lifecycle() -> [Subscription; 6] {
    [
        Subscription::PaneCreated,
        Subscription::AgentDetected,
        Subscription::PaneClosed,
        Subscription::PaneExited,
        Subscription::TabClosed,
        Subscription::WorkspaceClosed,
    ]
}

/// Panes waiting to be re-read, deduplicated.
#[derive(Debug, Default)]
struct RefreshQueue {
    pending: Mutex<HashSet<PaneId>>,
    ready: Notify,
}

impl RefreshQueue {
    /// Asks for `pane` to be re-read; a no-op if it already is pending.
    fn request(&self, pane: &PaneId) {
        let added = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pane.clone());
        if added {
            self.ready.notify_one();
        }
    }

    /// Every pending pane, emptying the queue.
    fn take(&self) -> Vec<PaneId> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *pending).into_iter().collect()
    }
}

/// A pane whose last `agent.get` failed, and when to try it again.
#[derive(Debug)]
struct Retry {
    backoff: Backoff,
    due: Instant,
}

struct Driver<H> {
    herdr: Arc<H>,
    hub: Arc<Hub>,
    settings: RegistrySettings,
    watchers: HashMap<PaneId, Watcher>,
    requests: Arc<RefreshQueue>,
    retries: HashMap<PaneId, Retry>,
    repos: RepoCache,
    /// Each known pane's cwd, to re-map it when a repo lookup lands late.
    cwds: HashMap<PaneId, std::path::PathBuf>,
    /// Each workspace's label, as of the last `workspace.list`.
    labels: HashMap<WorkspaceId, String>,
    /// Workspaces missing from `labels` that were already looked up since
    /// the last resync, so an unlabeled one is not re-listed on every read.
    unlabeled: HashSet<WorkspaceId>,
}

/// A per-pane status watcher, aborted when dropped.
struct Watcher(JoinHandle<()>);

impl Drop for Watcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What an event asks of the session beyond its own handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowUp {
    None,
    Resync,
}

impl<H: Herdr + 'static> Driver<H> {
    async fn run(mut self, mut shutdown: Shutdown) {
        let mut backoff = self.backoff();
        loop {
            let started = Instant::now();
            let ended = tokio::select! {
                ended = self.session() => ended,
                () = shutdown.wait() => break,
            };
            match ended {
                Ok(()) => tracing::info!("herdr event stream ended; resubscribing"),
                Err(error) => tracing::warn!(%error, "lost herdr; will resync"),
            }
            let delay = backoff.after(started);
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = shutdown.wait() => break,
            }
        }
        tracing::debug!("registry stopped");
    }

    fn backoff(&self) -> Backoff {
        Backoff::new(self.settings.retry_min, self.settings.retry_max)
    }

    /// Subscribes, resyncs, then follows events until the lifecycle stream
    /// ends (`Ok`) or fails (`Err`).
    async fn session(&mut self) -> Result<(), herdr::Error> {
        let mut events = self.herdr.subscribe(&lifecycle()).await?;
        self.resync().await?;
        let every = self.settings.resync_every;
        let mut resync = tokio::time::interval_at(Instant::now() + every, every);
        resync.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut resync_backoff = self.backoff();
        let requests = Arc::clone(&self.requests);
        let repo_results = self.repos.completions();
        loop {
            let next_retry = self.retries.values().map(|r| r.due).min();
            let follow_up = tokio::select! {
                event = events.next() => match event {
                    None => return Ok(()),
                    Some(Err(error)) => return Err(error),
                    Some(Ok(event)) => self.on_event(event).await,
                },
                () = requests.ready.notified() => {
                    let now = Instant::now();
                    // A pane in retry backoff is read when its retry is due.
                    let panes: Vec<PaneId> = requests
                        .take()
                        .into_iter()
                        .filter(|pane| self.retries.get(pane).is_none_or(|r| r.due <= now))
                        .collect();
                    self.refresh_all(panes).await;
                    FollowUp::None
                }
                () = repo_results.notified() => {
                    let changed = self.repos.collect();
                    self.remap(&changed);
                    FollowUp::None
                }
                () = sleep_until(next_retry), if next_retry.is_some() => {
                    self.retry_due().await;
                    FollowUp::None
                }
                _ = resync.tick() => FollowUp::Resync,
            };
            if follow_up == FollowUp::Resync {
                self.resync_in_session(&mut resync, &mut resync_backoff)
                    .await;
            }
        }
    }

    async fn on_event(&mut self, event: Event) -> FollowUp {
        match event {
            Event::PaneCreated { pane } if pane.agent_kind.is_some() => {
                self.hub.note_arrival(&pane.pane_id, now());
                self.refresh_all(vec![pane.pane_id]).await;
            }
            Event::PaneAgentDetected {
                pane_id,
                released: false,
                ..
            } => {
                // When the agent started, whenever its read succeeds.
                self.hub.note_arrival(&pane_id, now());
                self.refresh_all(vec![pane_id]).await;
            }
            Event::PaneAgentDetected { pane_id, .. }
            | Event::PaneClosed { pane_id, .. }
            | Event::PaneExited { pane_id, .. } => self.forget(&pane_id),
            Event::TabClosed { .. } | Event::WorkspaceClosed { .. } => return FollowUp::Resync,
            _ => {}
        }
        FollowUp::None
    }

    /// A resync inside a running session: on failure, retry it with backoff
    /// instead of ending the session.
    async fn resync_in_session(&mut self, timer: &mut Interval, backoff: &mut Backoff) {
        match self.resync().await {
            Ok(()) => {
                *backoff = self.backoff();
                timer.reset();
            }
            Err(error) => {
                let delay = backoff.after(Instant::now());
                tracing::warn!(%error, ?delay, "resync failed; retrying");
                timer.reset_after(delay);
            }
        }
    }

    /// Re-reads `panes` concurrently (a few at a time), then applies each
    /// result: a pane that no longer hosts an agent is forgotten, any other
    /// failure is retried for that pane alone.
    async fn refresh_all(&mut self, panes: Vec<PaneId>) {
        let limit = Arc::new(Semaphore::new(MAX_CONCURRENT_READS));
        let mut reads = JoinSet::new();
        for pane in panes {
            let herdr = Arc::clone(&self.herdr);
            let limit = Arc::clone(&limit);
            reads.spawn(async move {
                // The semaphore is never closed, so a permit always comes.
                let _permit = limit.acquire_owned().await.ok();
                let read = herdr.agent(&Target::Pane(pane.clone())).await;
                (pane, read)
            });
        }
        while let Some(joined) = reads.join_next().await {
            match joined {
                Ok((pane, read)) => self.apply_read(&pane, read).await,
                Err(error) => tracing::error!(%error, "agent read task failed"),
            }
        }
    }

    async fn apply_read(&mut self, pane: &PaneId, read: Result<herdr::Agent, herdr::Error>) {
        match read {
            Ok(agent) => {
                self.retries.remove(pane);
                self.ensure_watcher(pane);
                self.record(&agent).await;
            }
            Err(error) if error.code().is_some_and(ErrorCode::is_not_found) => self.forget(pane),
            Err(error) => {
                let fresh = self.backoff();
                let retry = self.retries.entry(pane.clone()).or_insert(Retry {
                    backoff: fresh,
                    due: Instant::now(),
                });
                let delay = retry.backoff.after(Instant::now());
                retry.due = Instant::now() + delay;
                let transient = matches!(
                    error.code(),
                    Some(ErrorCode::AgentLaunchPending | ErrorCode::AgentNotReady)
                );
                if transient {
                    tracing::debug!(%pane, %error, ?delay, "agent not ready; will retry");
                } else {
                    tracing::warn!(%pane, %error, ?delay, "could not read agent; will retry");
                }
            }
        }
    }

    /// Asks for a re-read of every pane working in one of `cwds` (their repo
    /// location changed).
    fn remap(&self, cwds: &[std::path::PathBuf]) {
        if cwds.is_empty() {
            return;
        }
        for (pane, cwd) in &self.cwds {
            if cwds.contains(cwd) {
                self.requests.request(pane);
            }
        }
    }

    /// Refreshes every pane whose retry is due.
    async fn retry_due(&mut self) {
        let now = Instant::now();
        let due: Vec<PaneId> = self
            .retries
            .iter()
            .filter(|(_, retry)| retry.due <= now)
            .map(|(pane, _)| pane.clone())
            .collect();
        self.refresh_all(due).await;
    }

    /// Replaces the hub's list with herdr's, and the watchers to match.
    async fn resync(&mut self) -> Result<(), herdr::Error> {
        let agents = self.herdr.agents().await?;
        self.unlabeled.clear();
        self.refresh_labels().await;
        // Re-check every repo in the background; old answers serve meanwhile.
        let changed = self
            .repos
            .resolve(agents.iter().filter_map(cwd), true)
            .await;
        let now = now();
        let next: BTreeMap<PaneId, protocol::Agent> = agents
            .iter()
            .filter_map(|agent| Some((agent.pane_id.clone(), self.map(agent, now)?)))
            .collect();
        // Pending retries stay: a pane not listed yet (still launching) is
        // read again when due, and dropped only once herdr says it is gone.
        self.watchers.retain(|pane, _| next.contains_key(pane));
        self.cwds = agents
            .iter()
            .filter(|agent| next.contains_key(&agent.pane_id))
            .filter_map(|agent| Some((agent.pane_id.clone(), cwd(agent)?.to_path_buf())))
            .collect();
        for pane in next.keys() {
            self.ensure_watcher(pane);
        }
        for agent in agents.iter().filter(|a| next.contains_key(&a.pane_id)) {
            self.hub
                .set_placement(&agent.pane_id, self.placement(agent));
        }
        self.hub.replace_all(next);
        self.remap(&changed);
        Ok(())
    }

    async fn record(&mut self, agent: &herdr::Agent) {
        let changed = self.repos.resolve(cwd(agent), false).await;
        match cwd(agent) {
            Some(dir) => self.cwds.insert(agent.pane_id.clone(), dir.to_path_buf()),
            None => self.cwds.remove(&agent.pane_id),
        };
        self.remap(&changed);
        let workspace = &agent.workspace_id;
        if !self.labels.contains_key(workspace) && self.unlabeled.insert(workspace.clone()) {
            self.refresh_labels().await;
        }
        if let Some(mapped) = self.map(agent, now()) {
            self.hub
                .set_placement(&agent.pane_id, self.placement(agent));
            self.hub.upsert(&agent.pane_id, mapped);
        }
    }

    /// Re-reads every workspace's label; keeps the old ones if herdr cannot
    /// list them.
    async fn refresh_labels(&mut self) {
        match self.herdr.workspaces().await {
            Ok(workspaces) => {
                self.labels = workspaces
                    .into_iter()
                    .map(|ws| (ws.workspace_id, ws.label))
                    .collect();
            }
            Err(error) => tracing::debug!(%error, "cannot list herdr workspaces"),
        }
    }

    /// Where herdr placed `agent`, with the workspace label as last listed.
    fn placement(&self, agent: &herdr::Agent) -> Placement {
        Placement {
            workspace: self.labels.get(&agent.workspace_id).cloned(),
            name: agent.name.as_ref().map(|name| name.as_str().to_owned()),
        }
    }

    /// `agent` for clients, flagged `quietDone` if it is known to be one
    /// of the orchestrator's.
    fn map(&self, agent: &herdr::Agent, now: Timestamp) -> Option<protocol::Agent> {
        let location = self.repos.get(cwd(agent));
        let mut mapped = mapping::to_protocol(agent, &location, now)
            .inspect_err(|error| tracing::warn!(%error, "skipping agent"))
            .ok()?;
        mapped.quiet_done = self.settings.quiet_done.is_quiet(&self.placement(agent));
        Some(mapped)
    }

    fn forget(&mut self, pane: &PaneId) {
        self.watchers.remove(pane);
        self.retries.remove(pane);
        self.cwds.remove(pane);
        self.hub.remove(pane);
    }

    fn ensure_watcher(&mut self, pane: &PaneId) {
        if self.watchers.contains_key(pane) {
            return;
        }
        let task = tokio::spawn(watch_pane(
            Arc::clone(&self.herdr),
            pane.clone(),
            Arc::clone(&self.requests),
            self.backoff(),
        ));
        self.watchers.insert(pane.clone(), Watcher(task));
    }
}

/// The agent process's cwd, else the pane's.
fn cwd(agent: &herdr::Agent) -> Option<&Path> {
    agent.foreground_cwd.as_deref().or(agent.cwd.as_deref())
}

/// Sleeps until `deadline`; callers guard the `None` case.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Follows one pane's status changes, asking the driver to refresh the pane
/// after subscribing and on every change. Resubscribes with backoff; ends
/// when aborted (its pane was forgotten, or the driver stopped).
async fn watch_pane<H: Herdr>(
    herdr: Arc<H>,
    pane: PaneId,
    requests: Arc<RefreshQueue>,
    mut backoff: Backoff,
) {
    let subscription = [Subscription::agent_status(pane.clone())];
    loop {
        let started = Instant::now();
        match herdr.subscribe(&subscription).await {
            Ok(mut events) => {
                requests.request(&pane);
                loop {
                    match events.next().await {
                        Some(Ok(Event::PaneAgentStatusChanged { .. })) => requests.request(&pane),
                        Some(Ok(_)) => {}
                        Some(Err(error)) => {
                            tracing::debug!(%pane, %error, "status stream failed");
                            break;
                        }
                        None => {
                            tracing::debug!(%pane, "status stream ended");
                            break;
                        }
                    }
                }
            }
            Err(error) => tracing::debug!(%pane, %error, "status subscription failed"),
        }
        tokio::time::sleep(backoff.after(started)).await;
    }
}

/// The current wall-clock time.
pub(crate) fn now() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    Timestamp::from_unix_millis(millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_requests_are_coalesced() {
        let queue = RefreshQueue::default();
        let (a, b) = (PaneId::new("w1:p1"), PaneId::new("w1:p2"));
        for _ in 0..5 {
            queue.request(&a);
        }
        queue.request(&b);
        tokio::time::timeout(Duration::from_secs(1), queue.ready.notified())
            .await
            .expect("woken");
        let mut taken = queue.take();
        taken.sort();
        assert_eq!(taken, [a.clone(), b]);
        assert_eq!(queue.take(), Vec::<PaneId>::new());
        queue.request(&a);
        assert_eq!(queue.take(), [a], "a drained pane can be requested again");
    }
}
