#![allow(dead_code)]

//! Cadence-bounded Linear polling task with nudge channel + coalescing +
//! 429 backoff drop + outage-vs-nudge gating. See slice 9 spec §2.4.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use roki_api_types::RefreshAck;
use time::OffsetDateTime;

use crate::linear::rate_limit::RateLimitState;

pub struct NudgeHandle {
    tx: mpsc::Sender<NudgeRequest>,
}

impl Clone for NudgeHandle {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

struct NudgeRequest {
    ack: oneshot::Sender<RefreshAck>,
}

pub struct PollingTracker {
    cadence: Duration,
    rate_limit: Arc<RateLimitState>,
    last_webhook_success: Arc<AtomicI64>, // ms since epoch; 0 = never
    last_fire: tokio::sync::Mutex<Instant>,
    nudge_rx: tokio::sync::Mutex<mpsc::Receiver<NudgeRequest>>,
    on_tick: Box<dyn Fn(TickReason) + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickReason {
    Outage,
    Nudge,
}

impl PollingTracker {
    pub fn spawn(
        cadence: Duration,
        rate_limit: Arc<RateLimitState>,
        last_webhook_success: Arc<AtomicI64>,
        on_tick: Box<dyn Fn(TickReason) + Send + Sync>,
    ) -> NudgeHandle {
        let (tx, rx) = mpsc::channel(32);
        let tracker = Arc::new(Self {
            cadence,
            rate_limit,
            last_webhook_success,
            last_fire: tokio::sync::Mutex::new(Instant::now() - cadence),
            nudge_rx: tokio::sync::Mutex::new(rx),
            on_tick,
        });
        tokio::spawn(async move { tracker.run().await });
        NudgeHandle { tx }
    }

    async fn run(self: Arc<Self>) {
        loop {
            let now = Instant::now();
            let last = *self.last_fire.lock().await;
            let next = last + self.cadence;
            let sleep = if now >= next {
                Duration::ZERO
            } else {
                next - now
            };

            let mut rx = self.nudge_rx.lock().await;
            let request = match timeout(sleep, rx.recv()).await {
                Ok(Some(req)) => Some(req),
                Ok(None) => return, // sender dropped, daemon shutting down
                Err(_) => None,     // cadence wake
            };

            let mut acks: Vec<oneshot::Sender<RefreshAck>> = Vec::new();
            if let Some(r) = request {
                acks.push(r.ack);
            }
            while let Ok(r) = rx.try_recv() {
                acks.push(r.ack);
            }
            drop(rx);

            // 429 backoff?
            if let Some(legal) = self.rate_limit.next_legal_at() {
                if Instant::now() < legal {
                    let now_inst = Instant::now();
                    let remaining: std::time::Duration = legal - now_inst;
                    let earliest_offset =
                        time::Duration::try_from(remaining).unwrap_or(time::Duration::ZERO);
                    let earliest = OffsetDateTime::now_utc() + earliest_offset;
                    for ack in acks {
                        let _ = ack.send(RefreshAck {
                            coalesced: false,
                            earliest_fire_at: Some(earliest),
                            backoff_active: true,
                        });
                    }
                    continue;
                }
            }

            // Reason: nudge wins over outage.
            let reason = if !acks.is_empty() {
                TickReason::Nudge
            } else {
                TickReason::Outage
            };

            // Outage gating: only tick on outage if webhook silent for 2 * cadence.
            if reason == TickReason::Outage {
                let last_ms = self.last_webhook_success.load(Ordering::Relaxed);
                let now_ms = OffsetDateTime::now_utc().unix_timestamp() * 1000;
                if last_ms != 0 && now_ms - last_ms < (self.cadence.as_millis() as i64) * 2 {
                    continue;
                }
            }

            // Execute the tick.
            (self.on_tick)(reason);
            *self.last_fire.lock().await = Instant::now();

            let earliest = OffsetDateTime::now_utc();
            let coalesced_value = acks.len() > 1;
            for (i, ack) in acks.into_iter().enumerate() {
                let _ = ack.send(RefreshAck {
                    coalesced: coalesced_value || i > 0,
                    earliest_fire_at: Some(earliest),
                    backoff_active: false,
                });
            }
        }
    }
}

impl NudgeHandle {
    pub async fn nudge(&self) -> RefreshAck {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(NudgeRequest { ack: tx }).await.is_err() {
            return RefreshAck {
                coalesced: false,
                earliest_fire_at: None,
                backoff_active: false,
            };
        }
        rx.await.unwrap_or(RefreshAck {
            coalesced: false,
            earliest_fire_at: None,
            backoff_active: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn rate_limit_unbounded() -> Arc<RateLimitState> {
        Arc::new(RateLimitState::new())
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn nudge_triggers_tick() {
        let calls = Arc::new(StdMutex::new(Vec::<TickReason>::new()));
        let calls_cb = calls.clone();
        let last_webhook = Arc::new(AtomicI64::new(0));
        let handle = PollingTracker::spawn(
            Duration::from_secs(1),
            rate_limit_unbounded(),
            last_webhook,
            Box::new(move |r| calls_cb.lock().unwrap().push(r)),
        );
        let ack = handle.nudge().await;
        assert!(!ack.coalesced);
        assert!(!ack.backoff_active);
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(calls.lock().unwrap()[0], TickReason::Nudge);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn outage_silenced_when_webhook_recent() {
        let calls = Arc::new(StdMutex::new(Vec::<TickReason>::new()));
        let calls_cb = calls.clone();
        let now_ms = OffsetDateTime::now_utc().unix_timestamp() * 1000;
        let last_webhook = Arc::new(AtomicI64::new(now_ms));
        let _handle = PollingTracker::spawn(
            Duration::from_secs(1),
            rate_limit_unbounded(),
            last_webhook,
            Box::new(move |r| calls_cb.lock().unwrap().push(r)),
        );
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(calls.lock().unwrap().is_empty());
    }
}
