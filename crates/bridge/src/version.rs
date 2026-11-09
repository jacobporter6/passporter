//! This build's identity, stamped at compile time by `build.rs` (see
//! `build/version.rs`): logged at startup, served on `GET /v1/version`
//! and shown by `passporter doctor`.

use core::fmt;

/// The build this binary is.
pub const BUILD: Build = Build {
    sha: env!("PASSPORTER_GIT_SHA"),
    built: env!("PASSPORTER_BUILT"),
};

/// A build's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Build {
    /// The short git commit, with `-dirty` when the tree had changes, or
    /// `unknown`.
    pub sha: &'static str,
    /// When it was built: RFC 3339 in UTC.
    pub built: &'static str,
}

impl Build {
    /// As `GET /v1/version` serves it.
    #[must_use]
    pub fn wire(self) -> protocol::BridgeVersion {
        protocol::BridgeVersion {
            bridge: self.sha.to_owned(),
            built: self.built.to_owned(),
        }
    }
}

impl fmt::Display for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}, built {}", self.sha, self.built)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_is_stamped() {
        let sha = BUILD.sha.strip_suffix("-dirty").unwrap_or(BUILD.sha);
        assert!(
            sha == "unknown" || (sha.len() >= 7 && sha.bytes().all(|b| b.is_ascii_hexdigit())),
            "{}",
            BUILD.sha
        );
        // RFC 3339, UTC: 2026-11-08T12:34:56Z.
        let built = BUILD.built.as_bytes();
        assert_eq!(built.len(), 20, "{}", BUILD.built);
        assert_eq!((built[4], built[10], built[19]), (b'-', b'T', b'Z'));
    }

    #[test]
    fn it_reads_and_serves_as_one() {
        let build = Build {
            sha: "0123abcd",
            built: "2026-11-08T12:34:56Z",
        };
        assert_eq!(build.to_string(), "0123abcd, built 2026-11-08T12:34:56Z");
        assert_eq!(build.wire().bridge, "0123abcd");
        assert_eq!(build.wire().built, "2026-11-08T12:34:56Z");
    }
}
