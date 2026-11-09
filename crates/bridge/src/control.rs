//! The herdr calls the HTTP handlers make, behind an object-safe trait so the
//! server state need not be generic over the [`Herdr`] implementation.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use herdr::{Herdr, PaneId, ReadSource, Screen, Target};

/// A boxed herdr call.
pub type HerdrFuture<'a, T> = Pin<Box<dyn Future<Output = herdr::Result<T>> + Send + 'a>>;

/// What the API needs from herdr, per pane.
pub trait AgentControl: Send + Sync + 'static {
    /// `agent.get`.
    fn agent<'a>(&'a self, pane: &'a PaneId) -> HerdrFuture<'a, herdr::Agent>;
    /// `agent.read`.
    fn read<'a>(
        &'a self,
        pane: &'a PaneId,
        source: ReadSource,
        lines: Option<u32>,
    ) -> HerdrFuture<'a, Screen>;
    /// `agent.prompt` (bracketed paste plus Enter).
    fn prompt<'a>(&'a self, pane: &'a PaneId, text: &'a str) -> HerdrFuture<'a, herdr::Agent>;
    /// `agent.send_keys` with herdr logical key names.
    fn send_keys<'a>(&'a self, pane: &'a PaneId, keys: &'a [&'a str]) -> HerdrFuture<'a, ()>;
}

/// [`AgentControl`] over any [`Herdr`].
pub struct HerdrControl<H>(pub Arc<H>);

impl<H> fmt::Debug for HerdrControl<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HerdrControl")
    }
}

impl<H: Herdr + 'static> AgentControl for HerdrControl<H> {
    fn agent<'a>(&'a self, pane: &'a PaneId) -> HerdrFuture<'a, herdr::Agent> {
        Box::pin(async move { self.0.agent(&Target::from(pane)).await })
    }

    fn read<'a>(
        &'a self,
        pane: &'a PaneId,
        source: ReadSource,
        lines: Option<u32>,
    ) -> HerdrFuture<'a, Screen> {
        Box::pin(async move { self.0.read_agent(&Target::from(pane), source, lines).await })
    }

    fn prompt<'a>(&'a self, pane: &'a PaneId, text: &'a str) -> HerdrFuture<'a, herdr::Agent> {
        Box::pin(async move { self.0.prompt(&Target::from(pane), text).await })
    }

    fn send_keys<'a>(&'a self, pane: &'a PaneId, keys: &'a [&'a str]) -> HerdrFuture<'a, ()> {
        Box::pin(async move { self.0.send_keys(&Target::from(pane), keys).await })
    }
}

/// The herdr agent an API [`AgentId`](protocol::AgentId) names, if it is
/// still there.
#[derive(Debug)]
pub enum Resolved {
    /// Same pane, same session.
    Current(herdr::Agent),
    /// The pane hosts another session (or none) now.
    Moved(herdr::Agent),
    /// No agent in that pane.
    Gone,
}

/// Looks `id` up in herdr. herdr failures other than "not found" are errors.
pub async fn resolve(
    control: &dyn AgentControl,
    id: &protocol::AgentId,
) -> herdr::Result<Resolved> {
    let pane = PaneId::new(id.pane().as_str());
    match control.agent(&pane).await {
        Ok(agent) => {
            let same = crate::registry::agent_id(&agent).is_ok_and(|current| current == *id);
            Ok(if same {
                Resolved::Current(agent)
            } else {
                Resolved::Moved(agent)
            })
        }
        Err(error) if error.code().is_some_and(herdr::ErrorCode::is_not_found) => {
            Ok(Resolved::Gone)
        }
        Err(error) => Err(error),
    }
}
