//! Tabs in the spawn workspace: new agents ([`crate::spawn`]), runbook
//! runs ([`crate::runbooks`]) and commands run in a new terminal
//! ([`crate::commands`]) each get one, so what the phone starts sits
//! together in herdr, visible like any pane.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use herdr::{ErrorCode, Pane, PaneId, TabId, WorkspaceId};
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::SpawnControl;

/// How many lines of a fresh tab to read while waiting for its shell.
const SHELL_READ_LINES: u32 = 40;

/// How often to look at a fresh tab while waiting for its shell.
const SHELL_POLL: Duration = Duration::from_millis(200);

/// Opens tabs in one herdr workspace (by label), making it on first use.
pub(crate) struct Tabs {
    control: Arc<dyn SpawnControl>,
    workspace: String,
    /// The workspace's id once known; held while a tab is opened, so two
    /// openings never make two workspaces.
    known: Mutex<Option<WorkspaceId>>,
}

impl std::fmt::Debug for Tabs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tabs")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl Tabs {
    /// Tabs in the workspace labelled `workspace`.
    pub(crate) fn new(control: Arc<dyn SpawnControl>, workspace: impl Into<String>) -> Self {
        Self {
            control,
            workspace: workspace.into(),
            known: Mutex::new(None),
        }
    }

    /// The workspace's label.
    pub(crate) fn workspace(&self) -> &str {
        &self.workspace
    }

    /// A fresh, unfocused tab labelled `label` with its shell in `cwd`, in
    /// the workspace (made, with this as its first tab, if herdr has none
    /// by that label). Returns the tab and its pane.
    pub(crate) async fn open(&self, label: &str, cwd: &Path) -> herdr::Result<(TabId, Pane)> {
        let control = self.control.as_ref();
        let mut known = self.known.lock().await;
        if let Some(workspace) = known.clone() {
            match control.create_tab(&workspace, label, cwd).await {
                Ok(created) => return Ok((created.tab.tab_id, created.root_pane)),
                Err(error) if error.code() == Some(&ErrorCode::WorkspaceNotFound) => *known = None,
                Err(error) => return Err(error),
            }
        }
        let listed = control.workspaces().await?;
        if let Some(workspace) = listed.into_iter().find(|w| w.label == self.workspace) {
            let created = control
                .create_tab(&workspace.workspace_id, label, cwd)
                .await?;
            *known = Some(workspace.workspace_id);
            return Ok((created.tab.tab_id, created.root_pane));
        }
        let created = control.create_workspace(&self.workspace, cwd).await?;
        *known = Some(created.workspace.workspace_id);
        Ok((created.tab.tab_id, created.root_pane))
    }

    /// Closes a tab that was opened and could not be used. Best effort.
    pub(crate) async fn close(&self, tab: &TabId) {
        match self.control.close_tab(tab).await {
            Ok(()) => {}
            Err(error) if error.code().is_some_and(ErrorCode::is_not_found) => {}
            Err(error) => tracing::warn!(%error, %tab, "closing an unused tab failed"),
        }
    }
}

/// Waits until `pane` shows something that stops changing (a fresh tab's
/// shell at its prompt, its rc files done), for at most `limit`.
pub(crate) async fn wait_for_shell(control: &dyn SpawnControl, pane: &PaneId, limit: Duration) {
    let deadline = Instant::now() + limit;
    let mut last = None::<String>;
    while Instant::now() < deadline {
        let now = control.read_recent(pane, SHELL_READ_LINES).await.ok();
        if let Some(text) = &now
            && !text.trim().is_empty()
            && last.as_ref() == Some(text)
        {
            return;
        }
        last = now;
        tokio::time::sleep(SHELL_POLL).await;
    }
}
