//! Which agents' finished turns are not news for the user: those an
//! orchestrator runs, which another agent waits on (configured with
//! `push.quiet_done_*`; defaults in `crate::config`). One classification, used
//! both for the agent list's `quietDone` flag (so the notification leaves
//! them out) and by the push dispatcher (so they never ping for it).

use std::collections::BTreeSet;

use super::hub::Placement;

/// The agents whose finished turns are quiet: those in a quiet herdr
/// workspace, and those whose herdr name starts with a quiet prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuietDone {
    /// herdr workspace labels.
    pub workspaces: BTreeSet<String>,
    /// herdr agent-name prefixes (such as `impl-`, `rev-`).
    pub agent_prefixes: BTreeSet<String>,
}

/// Why an agent's finished turn is quiet or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Quiet: the agent's herdr name has a quiet prefix.
    QuietPrefix,
    /// Quiet: the agent runs in a quiet workspace.
    QuietWorkspace,
    /// News: one of the user's agents.
    UserAgent,
    /// News: where the agent runs could not be found out (fail open).
    WorkspaceUnknown,
}

impl Verdict {
    /// Whether the finished turn is quiet.
    #[must_use]
    pub fn is_quiet(self) -> bool {
        matches!(self, Self::QuietPrefix | Self::QuietWorkspace)
    }

    /// The reason, for logs.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::QuietPrefix => "quiet name prefix",
            Self::QuietWorkspace => "quiet workspace",
            Self::UserAgent => "user agent",
            Self::WorkspaceUnknown => "workspace unknown",
        }
    }
}

impl QuietDone {
    /// The verdict on the agent at `placement`; `None` if it hangs on a
    /// workspace label that is not known.
    #[must_use]
    pub fn judge(&self, placement: &Placement) -> Option<Verdict> {
        let quiet_name = placement.name.as_deref().is_some_and(|name| {
            self.agent_prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix.as_str()))
        });
        if quiet_name {
            return Some(Verdict::QuietPrefix);
        }
        match &placement.workspace {
            Some(label) if self.workspaces.contains(label) => Some(Verdict::QuietWorkspace),
            Some(_) => Some(Verdict::UserAgent),
            None if self.workspaces.is_empty() => Some(Verdict::UserAgent),
            None => None,
        }
    }

    /// Whether the agent at `placement` is known to be quiet when done: an
    /// unknown label counts as not quiet.
    #[must_use]
    pub fn is_quiet(&self, placement: &Placement) -> bool {
        self.judge(placement).is_some_and(Verdict::is_quiet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(workspace: Option<&str>, name: Option<&str>) -> Placement {
        Placement {
            workspace: workspace.map(str::to_owned),
            name: name.map(str::to_owned),
        }
    }

    fn swarm() -> QuietDone {
        QuietDone {
            workspaces: BTreeSet::from(["agent-swarm".to_owned()]),
            agent_prefixes: BTreeSet::from(["impl-".to_owned(), "rev-".to_owned()]),
        }
    }

    #[test]
    fn names_then_workspaces_decide() {
        let q = swarm();
        let judge = |workspace, name| q.judge(&at(workspace, name));
        assert_eq!(judge(None, Some("impl-t26f")), Some(Verdict::QuietPrefix));
        assert_eq!(
            judge(Some("passporter"), Some("rev-t26f-r1")),
            Some(Verdict::QuietPrefix)
        );
        assert_eq!(
            judge(Some("agent-swarm"), None),
            Some(Verdict::QuietWorkspace)
        );
        assert_eq!(
            judge(Some("passporter"), Some("pp-1a2b")),
            Some(Verdict::UserAgent)
        );
        assert_eq!(judge(None, Some("circus")), None, "needs the label");
        assert!(!q.is_quiet(&at(None, Some("circus"))), "unknown: not quiet");
        assert!(q.is_quiet(&at(Some("agent-swarm"), None)));
    }

    #[test]
    fn nothing_configured_quiets_nobody_and_needs_no_label() {
        let q = QuietDone::default();
        assert_eq!(
            q.judge(&at(None, Some("impl-t1"))),
            Some(Verdict::UserAgent)
        );
        assert_eq!(
            q.judge(&at(Some("agent-swarm"), None)),
            Some(Verdict::UserAgent)
        );
    }
}
