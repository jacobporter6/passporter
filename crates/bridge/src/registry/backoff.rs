//! Capped exponential backoff for reconnecting to herdr.

use std::time::Duration;

use tokio::time::Instant;

/// Doubles from `min` to `max`. A connection that stayed up for at least
/// `max` counts as healthy and starts the next retry from `min` again.
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub(crate) fn new(min: Duration, max: Duration) -> Self {
        let min = min.min(max);
        Self {
            min,
            max,
            next: min,
        }
    }

    /// The delay before the next attempt, given when the attempt that just
    /// ended started.
    pub(crate) fn after(&mut self, attempt_started: Instant) -> Duration {
        if attempt_started.elapsed() >= self.max {
            self.next = self.min;
        }
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.max);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn doubles_up_to_the_cap() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_millis(500));
        let now = Instant::now();
        let delays: Vec<u64> = (0..5)
            .map(|_| u64::try_from(backoff.after(now).as_millis()).expect("small"))
            .collect();
        assert_eq!(delays, [100, 200, 400, 500, 500]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_healthy_attempt_resets() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_millis(500));
        let started = Instant::now();
        let _ = backoff.after(started);
        let _ = backoff.after(started);
        tokio::time::advance(Duration::from_millis(500)).await;
        assert_eq!(backoff.after(started), Duration::from_millis(100));
    }
}
