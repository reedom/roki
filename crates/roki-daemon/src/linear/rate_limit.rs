#![allow(dead_code)]

//! Shared 429 backoff state for every Linear-bound request the daemon
//! makes.
//!
//! Both `LinearClient::resolve_viewer` and `LinearGraphqlClient::enumerate`
//! await `wait_if_backoff` before issuing a request, and call `record_429`
//! when Linear returns HTTP 429. Backoff is exponential (1s -> 60s) with
//! `Retry-After` header overrides taking precedence when present.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use time::OffsetDateTime;
use tokio::time::sleep;

const MIN_BACKOFF_SECONDS: u64 = 1;
const MAX_BACKOFF_SECONDS: u64 = 60;

#[derive(Default, Clone)]
pub struct RateLimitState {
    /// Unix epoch milliseconds at which the backoff window ends. `0` =
    /// no backoff.
    backoff_until_ms: Arc<AtomicU64>,
    /// Last applied backoff in seconds (used for exponential growth).
    last_backoff_seconds: Arc<AtomicU64>,
}

impl RateLimitState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_in_backoff(&self) -> bool {
        let now = now_ms();
        self.backoff_until_ms.load(Ordering::Acquire) > now
    }

    pub async fn wait_if_backoff(&self) {
        let now = now_ms();
        let until = self.backoff_until_ms.load(Ordering::Acquire);
        if until > now {
            sleep(Duration::from_millis(until - now)).await;
        }
    }

    /// Record a 429 response. `retry_after` overrides the doubled value
    /// when supplied.
    pub fn record_429(&self, retry_after: Option<Duration>) -> Duration {
        let prior = self.last_backoff_seconds.load(Ordering::Acquire);
        let next_seconds = match retry_after {
            Some(d) => d.as_secs().clamp(MIN_BACKOFF_SECONDS, MAX_BACKOFF_SECONDS),
            None => {
                let doubled = prior.saturating_mul(2);
                doubled.clamp(MIN_BACKOFF_SECONDS, MAX_BACKOFF_SECONDS)
            }
        };
        self.last_backoff_seconds
            .store(next_seconds, Ordering::Release);
        let until = now_ms() + next_seconds * 1000;
        self.backoff_until_ms.store(until, Ordering::Release);
        Duration::from_secs(next_seconds)
    }

    pub fn clear(&self) {
        self.backoff_until_ms.store(0, Ordering::Release);
        self.last_backoff_seconds.store(0, Ordering::Release);
    }
}

fn now_ms() -> u64 {
    let now = OffsetDateTime::now_utc().unix_timestamp_nanos();
    if now < 0 { 0 } else { (now / 1_000_000) as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_state_has_no_backoff() {
        let s = RateLimitState::new();
        assert!(!s.is_in_backoff());
        // wait_if_backoff returns immediately
        tokio::time::timeout(Duration::from_millis(50), s.wait_if_backoff())
            .await
            .expect("should not block");
    }

    #[tokio::test]
    async fn record_429_without_retry_after_doubles() {
        let s = RateLimitState::new();
        let d1 = s.record_429(None);
        assert!(d1.as_secs() >= 1);
        let d2 = s.record_429(None);
        assert!(d2 >= d1);
        let d3 = s.record_429(None);
        assert!(d3 >= d2);
    }

    #[tokio::test]
    async fn record_429_caps_at_60_seconds() {
        let s = RateLimitState::new();
        // Force ten consecutive 429s -- should saturate at 60.
        for _ in 0..10 {
            s.record_429(None);
        }
        let last = s.last_backoff_seconds.load(Ordering::Acquire);
        assert!(last <= 60);
        assert!(last >= 32);
    }

    #[tokio::test]
    async fn retry_after_overrides_doubled_value() {
        let s = RateLimitState::new();
        s.record_429(None);
        s.record_429(None);
        let d = s.record_429(Some(Duration::from_secs(5)));
        assert_eq!(d, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn retry_after_is_clamped_to_max() {
        let s = RateLimitState::new();
        let d = s.record_429(Some(Duration::from_secs(600)));
        assert_eq!(d, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn clear_resets_state() {
        let s = RateLimitState::new();
        s.record_429(None);
        assert!(s.is_in_backoff());
        s.clear();
        assert!(!s.is_in_backoff());
    }
}
