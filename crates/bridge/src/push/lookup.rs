//! Asking herdr where an agent is, for a finished turn whose quiet decision
//! needs a workspace label the registry does not know yet (the registry
//! could not list workspaces when the agent turned up, say).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use herdr::{Herdr, Target};

use crate::registry::Placement;

/// What a [`PlacementLookup`] returns.
pub type LookupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Placement, herdr::Error>> + Send + 'a>>;

/// Finds out where herdr placed the agent in a pane, straight from herdr.
pub trait PlacementLookup: Send + Sync {
    /// The agent in `pane`: its herdr name and workspace label (`None` if
    /// herdr does not list its workspace).
    fn placement<'a>(&'a self, pane: &'a protocol::PaneId) -> LookupFuture<'a>;
}

/// A [`PlacementLookup`] that asks herdr: `agent.get`, then
/// `workspace.list`. Only used when the registry's labels fall short, so it
/// keeps no cache of its own.
#[derive(Debug)]
pub struct HerdrLookup<H>(pub Arc<H>);

impl<H: Herdr + 'static> PlacementLookup for HerdrLookup<H> {
    fn placement<'a>(&'a self, pane: &'a protocol::PaneId) -> LookupFuture<'a> {
        Box::pin(async move {
            let target = Target::Pane(herdr::PaneId::new(pane.as_str()));
            let agent = self.0.agent(&target).await?;
            let workspace = self
                .0
                .workspaces()
                .await?
                .into_iter()
                .find(|ws| ws.workspace_id == agent.workspace_id)
                .map(|ws| ws.label);
            Ok(Placement {
                workspace,
                name: agent.name.map(|name| name.as_str().to_owned()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use herdr::{AgentStatus, ErrorCode, FakeHerdr, Fault, Method};

    use super::*;

    #[tokio::test]
    async fn finds_the_name_and_the_workspace_label() {
        let herdr = Arc::new(FakeHerdr::new());
        // FakeHerdr labels each `add_agent` workspace with the agent's name.
        let pane = herdr.add_agent("impl-t1", "claude", AgentStatus::Working);
        let lookup = HerdrLookup(Arc::clone(&herdr));
        let pane = protocol::PaneId::new(pane.as_str()).expect("pane");
        assert_eq!(
            lookup.placement(&pane).await.expect("found"),
            Placement {
                workspace: Some("impl-t1".to_owned()),
                name: Some("impl-t1".to_owned()),
            }
        );

        herdr.fail_next(
            Method::WorkspaceList,
            Fault::Reject(ErrorCode::InternalError),
        );
        assert!(lookup.placement(&pane).await.is_err(), "herdr failed");
        let gone = protocol::PaneId::new("w9:p9").expect("pane");
        assert!(lookup.placement(&gone).await.is_err(), "no such pane");
    }
}
