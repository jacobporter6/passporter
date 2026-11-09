//! What a blocked agent is asking: Claude Code dialogs, from Claude Code
//! hooks when they report one and parsed from the screen otherwise
//! (`GET /v1/agents/:id/pending`, `pending_changed` events, and the
//! `choose` action's view of the current options). Design §6.2.
//!
//! - [`parse`](self::parse) turns screen text into a [`PendingInput`] with
//!   `source: "screen"`, plus the key that selects each option.
//! - [`hook`] decodes the payloads `passporter-hook` forwards over the
//!   [`socket`](self::socket), and [`store`](self::store) keeps recent
//!   permission requests per Claude session id. When one of them is
//!   exactly the dialog on screen (the whole command, plan or question),
//!   its prompt and detail replace the screen's. Options, keys, the id and
//!   `source: "screen"` stay the screen's: the screen is what keys act on.
//! - [`Pendings::current`] reads and parses the screen of an agent herdr
//!   reports `blocked`, fresh on every call (the API and the actions
//!   endpoint never trust a cached parse), and records the result in the
//!   [`Hub`], which broadcasts `pending_changed` when the dialog differs
//!   (not when only the rest of the screen moved).
//! - [`Pendings::spawn_poller`] re-parses blocked agents every
//!   [`PendingSettings::poll_every`], and at once when an agent turns (or
//!   stays) blocked with a new `seq`, so a dialog appearing, changing or
//!   clearing reaches the stream. Agents that are not blocked are never
//!   read. Leaving `blocked` clears the input in the hub directly.
//!
//! The pending id is `screen-<state_change_seq>-<12 hex>`, the hex a hash of
//! the dialog's content (kind, prompt, detail, and each option's id, label
//! and key). herdr does not always bump `seq` between back-to-back dialogs,
//! and option ids are generic (`yes`), so the hash is what ties an answer to
//! the dialog the user saw: another dialog at the same `seq` has another id,
//! and `choose` or `keys` carrying the old one is stale (409).

pub mod hook;
mod parse;
mod socket;
mod store;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use herdr::{AgentStatus, ReadSource};
use protocol::{AgentEvent, AgentId, SessionId};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use self::hook::{HookEvent, HookEventKind, Shown};
pub(crate) use self::parse::{Dialog, parsed_at};
use self::store::HookStore;
use crate::control::{AgentControl, Resolved, resolve};
use crate::registry::Hub;
use crate::shutdown::Shutdown;

/// Pacing for pending-input parsing and the feedback flow.
#[derive(Debug, Clone)]
pub struct PendingSettings {
    /// How often blocked agents' screens are re-parsed.
    pub poll_every: Duration,
    /// After choosing an option that takes feedback, how long to wait for
    /// Claude to return to its prompt before giving up on the feedback.
    pub feedback_wait: Duration,
    /// How often to check during that wait.
    pub feedback_poll: Duration,
    /// The hook socket to listen on ([`hook::default_socket_path`] for the
    /// service); `None` for screen parsing only.
    pub hook_socket: Option<PathBuf>,
}

impl Default for PendingSettings {
    /// Re-parse every second; wait up to 3 s for the feedback prompt,
    /// checking every 100 ms. No hook socket.
    fn default() -> Self {
        Self {
            poll_every: Duration::from_secs(1),
            feedback_wait: Duration::from_secs(3),
            feedback_poll: Duration::from_millis(100),
            hook_socket: None,
        }
    }
}

/// Pending inputs, from hooks and the screen.
pub struct Pendings {
    settings: PendingSettings,
    control: Arc<dyn AgentControl>,
    hub: Arc<Hub>,
    hooks: HookStore,
    /// Sessions with a new permission request, not yet re-checked.
    hook_sessions: Mutex<HashSet<SessionId>>,
    /// Wakes the poller when a permission request arrives.
    hook_arrived: Notify,
}

impl std::fmt::Debug for Pendings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pendings")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Pendings {
    /// Parses through `control`, recording results in `hub`.
    #[must_use]
    pub fn new(settings: PendingSettings, control: Arc<dyn AgentControl>, hub: Arc<Hub>) -> Self {
        Self {
            hooks: HookStore::default(),
            hook_sessions: Mutex::new(HashSet::new()),
            settings,
            control,
            hub,
            hook_arrived: Notify::new(),
        }
    }

    /// The pacing.
    pub(crate) fn settings(&self) -> &PendingSettings {
        &self.settings
    }

    /// What `agent` (as herdr reported it just now) is asking: `None` unless
    /// it is blocked; otherwise its screen, read and parsed now, described
    /// by its session's hook request when one matches. A blocked agent
    /// always has an input, without options when the screen does not parse
    /// confidently. The result is also recorded in the hub.
    pub(crate) async fn current(&self, agent: &herdr::Agent) -> herdr::Result<Option<Dialog>> {
        let agent_id = crate::registry::agent_id(agent).ok();
        if agent.agent_status != AgentStatus::Blocked {
            if let Some(agent_id) = &agent_id {
                self.hub.set_pending(agent_id, None);
            }
            return Ok(None);
        }
        let screen = self
            .control
            .read(&agent.pane_id, ReadSource::Visible, None)
            .await?;
        // `screen-<u64>-<hex>` is always a valid id.
        let dialect = parse::Dialect::for_kind(agent.kind.as_deref());
        let Some(mut dialog) = parse::parse(&screen.text, agent.state_change_seq, dialect) else {
            return Ok(None);
        };
        if let Some(session) = agent_id.as_ref().and_then(AgentId::session) {
            let shown = Shown::new(&dialog.input);
            if let Some((request, matched)) = self.hooks.describe(session, &dialog.input.id, &shown)
            {
                dialog.describe_with(&request, matched);
            }
        }
        if let Some(agent_id) = &agent_id {
            self.hub.set_pending(agent_id, Some(dialog.input.clone()));
        }
        Ok(Some(dialog))
    }

    /// Records a hook event. A permission request re-checks its session's
    /// blocked agent at once; other events change nothing.
    pub(crate) fn record_hook(&self, event: HookEvent) {
        let HookEventKind::Permission(permission) = event.kind else {
            return;
        };
        self.hooks.record(event.session.clone(), permission);
        self.hook_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(event.session);
        self.hook_arrived.notify_one();
    }

    /// Re-parses the agents the hub knows as blocked: all of them, or only
    /// those in `sessions`.
    async fn poll(&self, sessions: Option<&HashSet<SessionId>>) {
        let blocked = self.hub.agents().into_iter().filter(|agent| {
            agent.status == protocol::AgentStatus::Blocked
                && sessions.is_none_or(|sessions| {
                    agent
                        .id
                        .session()
                        .is_some_and(|session| sessions.contains(session))
                })
        });
        for known in blocked {
            match resolve(self.control.as_ref(), &known.id).await {
                Ok(Resolved::Current(agent)) => {
                    if let Err(error) = self.current(&agent).await {
                        tracing::debug!(%error, "pending screen read failed");
                    }
                }
                Ok(Resolved::Moved(_) | Resolved::Gone) => self.hub.set_pending(&known.id, None),
                Err(error) => tracing::debug!(%error, "pending agent lookup failed"),
            }
        }
    }

    /// Binds the hook socket at `path` and serves it until `shutdown`; on
    /// failure, logs that hooks are off.
    fn listen(self: &Arc<Self>, path: &Path, shutdown: &Shutdown) -> Option<AbortOnDrop> {
        match socket::bind(path) {
            Ok(bound) => {
                tracing::info!(path = %path.display(), "listening for Claude Code hooks");
                let task = tokio::spawn(socket::serve(bound, Arc::clone(self), shutdown.clone()));
                Some(AbortOnDrop(task))
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "hook socket unavailable; pending input comes from the screen only"
                );
                None
            }
        }
    }

    /// Starts the poller: every [`PendingSettings::poll_every`], as soon
    /// as the hub reports an agent blocked, and as soon as a hook event
    /// arrives. With a [`PendingSettings::hook_socket`], it also listens
    /// there; if the socket cannot be bound, hooks are off and the screen
    /// parse carries on alone.
    #[must_use]
    pub fn spawn_poller(self: &Arc<Self>, mut shutdown: Shutdown) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _listener = match &this.settings.hook_socket {
                Some(path) => this.listen(path, &shutdown),
                None => None,
            };
            let mut feed = this.hub.subscribe();
            let mut tick = tokio::time::interval(this.settings.poll_every);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                // A tick or a blocked agent re-reads everyone; a hook
                // request only its own session.
                let mut everyone = true;
                tokio::select! {
                    _ = tick.tick() => {}
                    event = feed.next() => {
                        let blocked = match &event {
                            AgentEvent::AgentUpdated { agent: Some(agent) } => {
                                agent.status == protocol::AgentStatus::Blocked
                            }
                            AgentEvent::Snapshot { .. } => true,
                            _ => false,
                        };
                        if !blocked {
                            continue;
                        }
                    }
                    () = this.hook_arrived.notified() => everyone = false,
                    () = shutdown.wait() => break,
                }
                // Taken either way: a full poll covers these sessions too,
                // and the wakeup left behind then finds nothing to do.
                let sessions = std::mem::take(
                    &mut *this
                        .hook_sessions
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner),
                );
                if everyone {
                    this.poll(None).await;
                } else if !sessions.is_empty() {
                    this.poll(Some(&sessions)).await;
                }
            }
        })
    }
}

/// Aborts the hook listener when the poller stops, so the socket file is
/// removed with it.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
