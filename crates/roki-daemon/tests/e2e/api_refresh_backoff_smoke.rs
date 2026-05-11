//! Slice 9 e2e: `POST /api/refresh` must return `backoff_active: true`
//! while the Linear rate-limiter holds a 429 backoff. Spec fr:10
//! §`POST /api/refresh` + fr:09 §rate-limit gating.
//!
//! TODO(slice9): requires a test-only seam to inject a 429 into the
//! `RateLimitState` shared with the polling tracker. The viewer/issues
//! wiremock returns 200 in the cold-start path and never feeds the
//! rate-limiter directly, so there is no surface today for an e2e
//! fixture to assert the backoff branch. Skipped until slice 10 wires
//! the seam.

#[tokio::test]
#[ignore = "needs RateLimitState test seam"]
async fn api_refresh_returns_backoff_active_during_429() {
    // intentionally empty
}
