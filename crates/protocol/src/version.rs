//! `GET /v1/version`: which build the bridge runs. Outbound: tolerant.
//!
//! Unauthenticated, so a device that can't sign in (unpaired, a rejected
//! token, an app too old to pair) can still learn that it is out of date.

use serde::{Deserialize, Serialize};

/// The bridge's build identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeVersion {
    /// The short git commit the bridge was built from, with `-dirty` when
    /// the tree had changes, or `unknown`.
    pub bridge: String,
    /// When it was built: RFC 3339 in UTC, e.g. `2026-11-08T12:34:56Z`.
    pub built: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape() {
        let version = BridgeVersion {
            bridge: "0123abcd".into(),
            built: "2026-11-08T12:34:56Z".into(),
        };
        let json = serde_json::json!({ "bridge": "0123abcd", "built": "2026-11-08T12:34:56Z" });
        assert_eq!(serde_json::to_value(&version).unwrap(), json);
        // Tolerant: a newer bridge may say more.
        let more: BridgeVersion = serde_json::from_str(
            r#"{"bridge":"0123abcd","built":"2026-11-08T12:34:56Z","web":"x"}"#,
        )
        .unwrap();
        assert_eq!(more, version);
    }
}
