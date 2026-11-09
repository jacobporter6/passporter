//! Fixed-window rate limits: per client IP for the unauthenticated
//! endpoints, per device for `POST /v1/actions`.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// Clients tracked at most; past this, expired windows are dropped, and if
/// none have expired the oldest is.
const MAX_CLIENTS: usize = 1024;

/// `requests` per `window`, per client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Requests allowed in one window.
    pub requests: u32,
    /// The window.
    pub window: Duration,
}

impl RateLimit {
    /// `requests` per minute.
    #[must_use]
    pub const fn per_minute(requests: u32) -> Self {
        Self {
            requests,
            window: Duration::from_secs(60),
        }
    }
}

/// One client's current window.
#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    used: u32,
}

/// Counts requests per client (an IP, or a device id).
#[derive(Debug)]
pub(crate) struct RateLimiter<K = IpAddr> {
    limit: RateLimit,
    clients: Mutex<HashMap<K, Window>>,
}

impl<K: Hash + Eq + Clone> RateLimiter<K> {
    pub(crate) fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Counts one request from `client`. `Err` carries how long until the
    /// client may try again.
    pub(crate) fn check(&self, client: &K) -> Result<(), Duration> {
        let now = Instant::now();
        let window = self.limit.window;
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if clients.len() >= MAX_CLIENTS && !clients.contains_key(client) {
            clients.retain(|_, w| now.duration_since(w.started) < window);
            if clients.len() >= MAX_CLIENTS
                && let Some(oldest) = clients
                    .iter()
                    .min_by_key(|(_, w)| w.started)
                    .map(|(key, _)| key.clone())
            {
                clients.remove(&oldest);
            }
        }
        let entry = clients.entry(client.clone()).or_insert(Window {
            started: now,
            used: 0,
        });
        if now.duration_since(entry.started) >= window {
            *entry = Window {
                started: now,
                used: 0,
            };
        }
        if entry.used >= self.limit.requests {
            return Err(window.saturating_sub(now.duration_since(entry.started)));
        }
        entry.used += 1;
        Ok(())
    }

    /// Gives back one request [`check`](Self::check) counted for `client`,
    /// for a request that turned out to do nothing (e.g. its backend was
    /// down), so retrying it never uses up the budget.
    pub(crate) fn refund(&self, client: &K) {
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = clients.get_mut(client) {
            entry.used = entry.used.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    #[test]
    fn a_refund_gives_one_request_back() {
        let limiter: RateLimiter = RateLimiter::new(RateLimit::per_minute(1));
        assert!(limiter.check(&ip(1)).is_ok());
        assert!(limiter.check(&ip(1)).is_err());
        limiter.refund(&ip(1));
        assert!(limiter.check(&ip(1)).is_ok());
        limiter.refund(&ip(9)); // unknown: nothing to give back
    }

    #[tokio::test(start_paused = true)]
    async fn limits_each_client_per_window() {
        let limiter: RateLimiter = RateLimiter::new(RateLimit::per_minute(2));
        assert!(limiter.check(&ip(1)).is_ok());
        assert!(limiter.check(&ip(1)).is_ok());
        let wait = limiter.check(&ip(1)).expect_err("third is limited");
        assert_eq!(wait, Duration::from_secs(60));
        assert!(limiter.check(&ip(2)).is_ok(), "other clients are separate");
        tokio::time::advance(Duration::from_secs(45)).await;
        assert_eq!(limiter.check(&ip(1)), Err(Duration::from_secs(15)));
        tokio::time::advance(Duration::from_secs(15)).await;
        assert!(limiter.check(&ip(1)).is_ok(), "a new window");
    }

    #[tokio::test(start_paused = true)]
    async fn the_client_table_is_bounded() {
        let limiter: RateLimiter = RateLimiter::new(RateLimit::per_minute(1));
        for i in 0..=u32::try_from(MAX_CLIENTS).expect("small") {
            let addr = IpAddr::from((10 << 24 | i).to_be_bytes());
            assert!(limiter.check(&addr).is_ok());
        }
        assert!(limiter.clients.lock().expect("lock").len() <= MAX_CLIENTS);
    }
}
