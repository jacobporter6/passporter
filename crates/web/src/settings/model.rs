//! What the page feeds the Settings view ([`crate::extras`]), as JSON:
//! the app state the groups show.

use serde::{Deserialize, Serialize};

use super::prefs::Pref;
use crate::live::Conn;
use crate::session::Credential;
use crate::version::{BridgeBuild, Freshness};

/// The app state the Settings groups show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// What the device signs in with.
    pub credential: Credential,
    /// Whether the device holds a token now.
    pub paired: bool,
    /// The stream's connection.
    pub conn: Conn,
    /// Whether storage is persistent; `None` until checked.
    pub storage: Option<bool>,
    /// Whether the stored credentials have loaded.
    pub token_loaded: bool,
    /// The preferences offered on this device, and their values.
    pub prefs: Vec<(Pref, bool)>,
    /// What the bridge said of its build.
    pub bridge: BridgeBuild,
    /// How this app compares with the bridge.
    pub freshness: Freshness,
}

impl Snapshot {
    /// The value of `pref`, if it is offered on this device.
    #[must_use]
    pub fn pref(&self, pref: Pref) -> Option<bool> {
        self.prefs
            .iter()
            .find(|(p, _)| *p == pref)
            .map(|(_, on)| *on)
    }
}

impl Default for Snapshot {
    /// Nothing known yet.
    fn default() -> Self {
        Self {
            credential: Credential::None,
            paired: false,
            conn: Conn::Off,
            storage: None,
            token_loaded: false,
            prefs: Vec::new(),
            bridge: BridgeBuild::Checking,
            freshness: Freshness::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::DeviceName;

    use super::*;

    #[test]
    fn the_snapshot_crosses_the_module_boundary_intact() {
        let snapshot = Snapshot {
            credential: Credential::Device {
                name: Some(DeviceName::new("Pixel").expect("name")),
            },
            paired: true,
            conn: Conn::Live,
            storage: Some(true),
            token_loaded: true,
            prefs: vec![(Pref::ReviewDictation, true)],
            bridge: BridgeBuild::Known("0123abcd".to_owned()),
            freshness: Freshness::Current,
        };
        let json = serde_json::to_string(&snapshot).expect("serializes");
        let back: Snapshot = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, snapshot);
        assert_eq!(back.pref(Pref::ReviewDictation), Some(true));
    }

    #[test]
    fn a_pref_not_offered_has_no_value() {
        assert_eq!(Snapshot::default().pref(Pref::ReviewDictation), None);
    }
}
