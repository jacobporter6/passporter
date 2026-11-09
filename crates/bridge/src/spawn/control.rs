//! The herdr calls a spawn, a runbook run or a command run makes, behind an object-safe trait so the server
//! state need not be generic over the [`Herdr`] implementation (as for
//! [`AgentControl`](crate::control::AgentControl), which covers the calls
//! on the new agent itself: `agent.get` and `agent.prompt`).

use std::path::Path;
use std::time::Duration;

use herdr::{
    AgentName, AgentStatus, CreatedTab, CreatedWorkspace, Herdr, Pane, PaneId, ReadSource,
    ShellWait, StartAgent, TabId, Target, Workspace, WorkspaceId,
};

use crate::control::{HerdrControl, HerdrFuture};

/// What starting an agent, running a runbook or a command needs from herdr.
pub trait SpawnControl: Send + Sync + 'static {
    /// `workspace.list`.
    fn workspaces(&self) -> HerdrFuture<'_, Vec<Workspace>>;
    /// `workspace.create`, unfocused, its first shell in `cwd`.
    fn create_workspace<'a>(
        &'a self,
        label: &'a str,
        cwd: &'a Path,
    ) -> HerdrFuture<'a, CreatedWorkspace>;
    /// `tab.create`, unfocused, its shell in `cwd`.
    fn create_tab<'a>(
        &'a self,
        workspace: &'a WorkspaceId,
        label: &'a str,
        cwd: &'a Path,
    ) -> HerdrFuture<'a, CreatedTab>;
    /// `agent.start`, once the pane's shell is at its prompt
    /// ([`Herdr::start_agent_at_shell`]).
    fn start<'a>(&'a self, spec: &'a StartAgent, shell: ShellWait)
    -> HerdrFuture<'a, herdr::Agent>;
    /// `agent.get` by name.
    fn agent_named<'a>(&'a self, name: &'a AgentName) -> HerdrFuture<'a, herdr::Agent>;
    /// `agent.wait` for the agent in `pane` to settle (idle, done or
    /// blocked), for at most `timeout`.
    fn wait_settled<'a>(
        &'a self,
        pane: &'a PaneId,
        timeout: Duration,
    ) -> HerdrFuture<'a, herdr::Agent>;
    /// `tab.close`, for a tab this spawn created and failed to use.
    fn close_tab<'a>(&'a self, tab: &'a TabId) -> HerdrFuture<'a, ()>;
    /// `pane.send_text` of `line` plus Enter, into a shell pane (a runbook
    /// run's launch line, which only ever holds bridge-made paths).
    fn type_line<'a>(&'a self, pane: &'a PaneId, line: &'a str) -> HerdrFuture<'a, ()>;
    /// `pane.get`: the pane now, with the terminal it runs.
    fn pane<'a>(&'a self, pane: &'a PaneId) -> HerdrFuture<'a, Pane>;
    /// `pane.read` of the pane's recent output, at most `lines` lines.
    fn read_recent<'a>(&'a self, pane: &'a PaneId, lines: u32) -> HerdrFuture<'a, String>;
    /// `pane.send_keys` of allowlisted keys, into a terminal a command run
    /// opened.
    fn send_keys<'a>(&'a self, pane: &'a PaneId, keys: &'a [&'a str]) -> HerdrFuture<'a, ()>;
}

impl<H: Herdr + 'static> SpawnControl for HerdrControl<H> {
    fn workspaces(&self) -> HerdrFuture<'_, Vec<Workspace>> {
        Box::pin(self.0.workspaces())
    }

    fn create_workspace<'a>(
        &'a self,
        label: &'a str,
        cwd: &'a Path,
    ) -> HerdrFuture<'a, CreatedWorkspace> {
        Box::pin(self.0.create_workspace(label, Some(cwd)))
    }

    fn create_tab<'a>(
        &'a self,
        workspace: &'a WorkspaceId,
        label: &'a str,
        cwd: &'a Path,
    ) -> HerdrFuture<'a, CreatedTab> {
        Box::pin(self.0.create_tab(workspace, label, Some(cwd)))
    }

    fn start<'a>(
        &'a self,
        spec: &'a StartAgent,
        shell: ShellWait,
    ) -> HerdrFuture<'a, herdr::Agent> {
        Box::pin(self.0.start_agent_at_shell(spec, shell))
    }

    fn agent_named<'a>(&'a self, name: &'a AgentName) -> HerdrFuture<'a, herdr::Agent> {
        Box::pin(async move { self.0.agent(&Target::Name(name.clone())).await })
    }

    fn wait_settled<'a>(
        &'a self,
        pane: &'a PaneId,
        timeout: Duration,
    ) -> HerdrFuture<'a, herdr::Agent> {
        const SETTLED: [AgentStatus; 3] =
            [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
        Box::pin(async move {
            self.0
                .wait(&Target::from(pane), &SETTLED, Some(timeout))
                .await
        })
    }

    fn close_tab<'a>(&'a self, tab: &'a TabId) -> HerdrFuture<'a, ()> {
        Box::pin(self.0.close_tab(tab))
    }

    fn type_line<'a>(&'a self, pane: &'a PaneId, line: &'a str) -> HerdrFuture<'a, ()> {
        Box::pin(self.0.run_in_pane(pane, line))
    }

    fn send_keys<'a>(&'a self, pane: &'a PaneId, keys: &'a [&'a str]) -> HerdrFuture<'a, ()> {
        Box::pin(self.0.send_pane_keys(pane, keys))
    }

    fn pane<'a>(&'a self, pane: &'a PaneId) -> HerdrFuture<'a, Pane> {
        Box::pin(self.0.pane(pane))
    }

    fn read_recent<'a>(&'a self, pane: &'a PaneId, lines: u32) -> HerdrFuture<'a, String> {
        Box::pin(async move {
            let screen = self
                .0
                .read_pane(pane, ReadSource::RecentUnwrapped, Some(lines))
                .await?;
            Ok(screen.text)
        })
    }
}
