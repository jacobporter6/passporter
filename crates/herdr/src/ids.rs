//! Opaque herdr identifiers and validated agent names.
//!
//! herdr IDs (`w1`, `w1:t1`, `w1:p1`) are opaque handles: never parse or
//! construct them from parts, only pass back what the server returned.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! opaque_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wraps an ID returned by herdr.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// The raw ID, as herdr expects it on the wire.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

opaque_id!(
    /// A workspace, e.g. `w1`.
    WorkspaceId
);
opaque_id!(
    /// A tab, e.g. `w1:t1`. Closed tab IDs are never reused.
    TabId
);
opaque_id!(
    /// A pane, e.g. `w1:p1`. Closed pane IDs are never reused; a moved pane gets a new ID.
    PaneId
);
opaque_id!(
    /// A terminal process, stable across pane moves.
    TerminalId
);

/// A live agent name: `[a-z][a-z0-9_-]{0,31}`, unique among live agents.
///
/// [`AgentName::new`] validates names we choose. Names *from* herdr are
/// trusted as-is when deserializing, so one odd name can't fail a whole
/// `agent.list`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentName(String);

/// Why a string is not a valid [`AgentName`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid agent name {name:?}: must match [a-z][a-z0-9_-]{{0,31}}")]
pub struct InvalidAgentName {
    name: String,
}

impl AgentName {
    /// The longest name herdr accepts.
    pub const MAX_LEN: usize = 32;

    /// Validates `name` against herdr's naming rule.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidAgentName> {
        let name = name.into();
        let mut chars = name.chars();
        let valid_first = chars.next().is_some_and(|c| c.is_ascii_lowercase());
        let valid_rest =
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if valid_first && valid_rest && name.len() <= Self::MAX_LEN {
            Ok(Self(name))
        } else {
            Err(InvalidAgentName { name })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who an agent command is aimed at: a live agent name or the pane hosting it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Name(AgentName),
    Pane(PaneId),
}

impl Target {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Name(name) => name.as_str(),
            Self::Pane(pane) => pane.as_str(),
        }
    }
}

impl From<AgentName> for Target {
    fn from(name: AgentName) -> Self {
        Self::Name(name)
    }
}

impl From<PaneId> for Target {
    fn from(pane: PaneId) -> Self {
        Self::Pane(pane)
    }
}

impl From<&PaneId> for Target {
    fn from(pane: &PaneId) -> Self {
        Self::Pane(pane.clone())
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_follow_herdr_rule() {
        for ok in ["a", "impl-t11", "rev-t11-r2", "x_y", &"a".repeat(32)] {
            assert!(AgentName::new(ok).is_ok(), "{ok} should be valid");
        }
        for bad in ["", "1abc", "Impl", "-x", "a b", "a.b", "é", &"a".repeat(33)] {
            assert!(AgentName::new(bad).is_err(), "{bad} should be invalid");
        }
    }

    #[test]
    fn names_from_the_server_are_trusted() {
        let odd: AgentName = serde_json::from_str("\"Legacy Name\"").expect("trusted");
        assert_eq!(odd.as_str(), "Legacy Name");
    }

    #[test]
    fn ids_are_transparent_on_the_wire() {
        let pane = PaneId::new("w1:p2");
        assert_eq!(serde_json::to_string(&pane).expect("ser"), "\"w1:p2\"");
        assert_eq!(Target::from(&pane).as_str(), "w1:p2");
    }
}
