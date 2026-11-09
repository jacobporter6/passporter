//! When to ping a device: debounced, coalesced and rate limited.
//!
//! A trigger (an agent became blocked or finished) arms the device's
//! [`Schedule`]. The ping goes out once things have been quiet for
//! [`PushTiming::quiet`], but no later than [`PushTiming::max_delay`] after
//! the first trigger, and never sooner than [`PushTiming::min_interval`]
//! after the previous ping. Everything that happens before it fires rides
//! on the same ping: the app fetches the current state when it wakes. A
//! due ping whose reasons have all lapsed is withdrawn, which does not
//! count toward the rate limit: only [`Schedule::sent`] does.

use std::time::Duration;

use tokio::time::Instant;

/// Debounce and rate-limit windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushTiming {
    /// Quiet time after the last trigger before pinging.
    pub quiet: Duration,
    /// Longest wait after the first trigger, however busy it stays.
    pub max_delay: Duration,
    /// Shortest gap between two pings to one device.
    pub min_interval: Duration,
}

impl Default for PushTiming {
    /// 2 s quiet, at most 10 s late, at most one ping per 15 s.
    fn default() -> Self {
        Self {
            quiet: Duration::from_secs(2),
            max_delay: Duration::from_secs(10),
            min_interval: Duration::from_secs(15),
        }
    }
}

/// One device's pending ping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Schedule {
    /// The first trigger since the last ping.
    first: Option<Instant>,
    /// When the pending ping goes out.
    due: Option<Instant>,
    /// When the last ping went out.
    last_sent: Option<Instant>,
}

impl Schedule {
    /// Records a trigger at `now`.
    pub(crate) fn trigger(&mut self, now: Instant, timing: &PushTiming) {
        let first = *self.first.get_or_insert(now);
        let mut due = (now + timing.quiet).min(first + timing.max_delay);
        if let Some(last) = self.last_sent {
            due = due.max(last + timing.min_interval);
        }
        self.due = Some(due);
    }

    /// When the pending ping is due, if there is one.
    pub(crate) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Takes the pending ping if it is due at `now`. Call [`Self::sent`]
    /// if it goes out.
    pub(crate) fn fire(&mut self, now: Instant) -> bool {
        if self.due.is_none_or(|due| due > now) {
            return false;
        }
        self.first = None;
        self.due = None;
        true
    }

    /// Records that a ping went out at `now`, for the rate limit.
    pub(crate) fn sent(&mut self, now: Instant) {
        self.last_sent = Some(now);
    }

    /// Whether forgetting this schedule changes nothing: no ping pending
    /// and the rate limit has passed.
    pub(crate) fn is_idle(&self, now: Instant, timing: &PushTiming) -> bool {
        self.due.is_none()
            && self
                .last_sent
                .is_none_or(|last| now >= last + timing.min_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMING: PushTiming = PushTiming {
        quiet: Duration::from_secs(2),
        max_delay: Duration::from_secs(10),
        min_interval: Duration::from_secs(15),
    };

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_lone_trigger_fires_after_the_quiet_window() {
        let t0 = Instant::now();
        let mut schedule = Schedule::default();
        assert_eq!(schedule.due(), None);
        assert!(!schedule.fire(t0 + secs(100)), "nothing pending");
        schedule.trigger(t0, &TIMING);
        assert_eq!(schedule.due(), Some(t0 + secs(2)));
        assert!(!schedule.fire(t0 + secs(1)));
        assert!(schedule.fire(t0 + secs(2)));
        assert!(!schedule.fire(t0 + secs(3)), "fires once");
    }

    #[test]
    fn a_burst_coalesces_into_one_ping_after_it_settles() {
        let t0 = Instant::now();
        let mut schedule = Schedule::default();
        for offset in [0, 1, 2, 3] {
            schedule.trigger(t0 + Duration::from_millis(offset * 500), &TIMING);
        }
        // Debounced from the last trigger (1.5 s) + 2 s quiet.
        assert_eq!(schedule.due(), Some(t0 + Duration::from_millis(3500)));
        assert!(schedule.fire(t0 + Duration::from_millis(3500)));
        assert_eq!(schedule.due(), None);
    }

    #[test]
    fn a_steady_stream_still_pings_by_the_max_delay() {
        let t0 = Instant::now();
        let mut schedule = Schedule::default();
        for s in 0..20 {
            schedule.trigger(t0 + secs(s), &TIMING);
        }
        assert_eq!(schedule.due(), Some(t0 + secs(10)));
    }

    #[test]
    fn pings_are_rate_limited_per_device() {
        let t0 = Instant::now();
        let mut schedule = Schedule::default();
        schedule.trigger(t0, &TIMING);
        assert!(schedule.fire(t0 + secs(2)));
        schedule.sent(t0 + secs(2));
        assert!(!schedule.is_idle(t0 + secs(3), &TIMING));
        schedule.trigger(t0 + secs(3), &TIMING);
        // Not before 15 s after the last ping.
        assert_eq!(schedule.due(), Some(t0 + secs(17)));
        assert!(!schedule.fire(t0 + secs(16)));
        assert!(schedule.fire(t0 + secs(17)));
        schedule.sent(t0 + secs(17));
        assert!(!schedule.is_idle(t0 + secs(20), &TIMING));
        assert!(schedule.is_idle(t0 + secs(32), &TIMING));
    }

    #[test]
    fn a_withdrawn_ping_does_not_hold_back_the_next() {
        let t0 = Instant::now();
        let mut schedule = Schedule::default();
        schedule.trigger(t0, &TIMING);
        assert!(schedule.fire(t0 + secs(2)), "due, but not sent");
        assert!(schedule.is_idle(t0 + secs(2), &TIMING));
        schedule.trigger(t0 + secs(3), &TIMING);
        assert_eq!(schedule.due(), Some(t0 + secs(5)), "no rate limit");
    }
}
