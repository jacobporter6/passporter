//! Pacing shared by TL;DRs and reply suggestions: the account-wide
//! usage-limit pause, the failure backoff, and the settle loop for a turn
//! that may still be being written.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// Most extra reads, a settle pause apart, while waiting for a
/// just-finished turn to stop changing.
pub(crate) const MAX_SETTLE_ROUNDS: usize = 5;

/// The account hit its usage limit: one pause for every generation, TL;DR
/// or suggestion, since they share the account.
#[derive(Debug, Clone, Default)]
pub(crate) struct UsageLimit(Arc<Mutex<Option<Instant>>>);

impl UsageLimit {
    /// Pauses every generation until `until`.
    pub(crate) fn pause_until(&self, until: Instant) {
        *lock(&self.0) = Some(until);
    }

    /// When the pause ends, if it is still on at `now`. A past pause is
    /// no pause: nothing may treat "was ever limited" as limited.
    pub(crate) fn active(&self, now: Instant) -> Option<Instant> {
        lock(&self.0).filter(|until| *until > now)
    }
}

/// The wait after the `attempts`-th failure in a row: `retry_after`,
/// doubling per failure, at most `max`.
pub(crate) fn backoff(retry_after: Duration, max: Duration, attempts: u32) -> Duration {
    let factor = 1_u32
        .checked_shl(attempts.saturating_sub(1))
        .unwrap_or(u32::MAX);
    retry_after.saturating_mul(factor).min(max)
}

/// How generations back off after failing: shared by TL;DRs, suggestions
/// and status lines.
#[derive(Debug, Clone)]
pub(crate) struct Pacing {
    /// The account-wide usage-limit pause.
    pub(crate) limit: UsageLimit,
    /// The wait after a first failure; doubles per failure.
    pub(crate) retry_after: Duration,
    /// The longest wait, and how long a usage limit pauses everything.
    pub(crate) max_retry_after: Duration,
}

impl Pacing {
    /// Records the `attempts`-th failure in a row and returns how long to
    /// wait: a usage limit pauses every generation for the longest wait,
    /// anything else backs off ([`backoff`]).
    pub(crate) fn after_failure(&self, attempts: u32, usage_limit: bool) -> Duration {
        if usage_limit {
            self.limit
                .pause_until(Instant::now() + self.max_retry_after);
            self.max_retry_after
        } else {
            backoff(self.retry_after, self.max_retry_after, attempts)
        }
    }
}

/// Reads with `read` until two reads `settle` apart agree (at most
/// [`MAX_SETTLE_ROUNDS`] extra reads), and returns the last. Something
/// read while lines are still arriving would be read partly.
pub(crate) async fn settled<T, F, Fut>(settle: Duration, mut read: F) -> T
where
    T: PartialEq,
    F: FnMut() -> Fut,
    Fut: Future<Output = T>,
{
    let mut last = read().await;
    for _ in 0..MAX_SETTLE_ROUNDS {
        tokio::time::sleep(settle).await;
        let now = read().await;
        if now == last {
            break;
        }
        last = now;
    }
    last
}

/// Locks a mutex, recovering from poisoning: no critical section in these
/// services can panic midway.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let s = Duration::from_secs;
        assert_eq!(backoff(s(60), s(1800), 1), s(60));
        assert_eq!(backoff(s(60), s(1800), 2), s(120));
        assert_eq!(backoff(s(60), s(1800), 6), s(1800));
        assert_eq!(backoff(s(60), s(1800), 99), s(1800));
    }

    #[tokio::test]
    async fn settled_waits_for_two_reads_to_agree() {
        let mut reads = vec![1, 2, 2, 3].into_iter();
        let got = settled(Duration::ZERO, || {
            let next = reads.next();
            async move { next }
        })
        .await;
        assert_eq!(got, Some(2));
    }

    #[test]
    fn a_usage_limit_pauses_everything_and_other_failures_back_off() {
        let pacing = Pacing {
            limit: UsageLimit::default(),
            retry_after: Duration::from_mins(1),
            max_retry_after: Duration::from_mins(30),
        };
        assert_eq!(pacing.after_failure(2, false), Duration::from_mins(2));
        assert_eq!(pacing.limit.active(Instant::now()), None);
        assert_eq!(pacing.after_failure(1, true), Duration::from_mins(30));
        assert!(pacing.limit.active(Instant::now()).is_some());
    }

    #[test]
    fn a_usage_limit_is_shared() {
        let limit = UsageLimit::default();
        let shared = limit.clone();
        let now = Instant::now();
        assert_eq!(shared.active(now), None);
        limit.pause_until(now + Duration::from_secs(5));
        assert!(shared.active(now).is_some());
        assert_eq!(shared.active(now + Duration::from_secs(6)), None);
    }
}
