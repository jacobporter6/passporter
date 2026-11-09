//! Timestamps.
//!
//! # Why unix milliseconds and not RFC 3339 strings
//!
//! - **No dependency.** Validating RFC 3339 properly (offsets, leap seconds,
//!   fractional digits) means pulling in `time`/`jiff` or hand-rolling a
//!   parser; this crate is limited to `serde`.
//! - **Every value is valid.** Any `u64` is a meaningful instant, so there is
//!   no rejection path and no "invalid timestamp" state to represent.
//! - **Native on both ends.** The browser has `Date.now()` (millis) and the
//!   bridge has `SystemTime::UNIX_EPOCH`; ordering and "time in state" are
//!   integer arithmetic.
//! - **Exact in JavaScript.** Present-day millis (~1.7e12) sit far below
//!   2^53, so JSON numbers round-trip without precision loss.
//!
//! Formatting for humans ("as of 14:02") is a UI concern, done at the edge.

use core::fmt;

use serde::{Deserialize, Serialize};

/// An instant, as milliseconds since the unix epoch (UTC).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// Builds a timestamp from unix milliseconds.
    #[must_use]
    pub const fn from_unix_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// Milliseconds since the unix epoch.
    #[must_use]
    pub const fn unix_millis(self) -> u64 {
        self.0
    }

    /// Milliseconds elapsed from `self` until `later`, saturating at zero if
    /// `later` is earlier (e.g. clock skew between phone and bridge).
    #[must_use]
    pub const fn millis_until(self, later: Self) -> u64 {
        later.0.saturating_sub(self.0)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_a_bare_number() {
        let t = Timestamp::from_unix_millis(1_762_500_000_000);
        assert_eq!(serde_json::to_string(&t).unwrap(), "1762500000000");
        assert_eq!(
            serde_json::from_str::<Timestamp>("1762500000000").unwrap(),
            t
        );
        assert_eq!(t.unix_millis(), 1_762_500_000_000);
    }

    #[test]
    fn rejects_non_integers() {
        assert!(serde_json::from_str::<Timestamp>("\"2026-11-07T00:00:00Z\"").is_err());
        assert!(serde_json::from_str::<Timestamp>("-5").is_err());
        assert!(serde_json::from_str::<Timestamp>("1.5").is_err());
    }

    #[test]
    fn millis_until_saturates() {
        let a = Timestamp(1_000);
        let b = Timestamp(4_500);
        assert_eq!(a.millis_until(b), 3_500);
        assert_eq!(b.millis_until(a), 0);
        assert_eq!(a.to_string(), "1000ms");
    }
}
