#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

#[derive(Clone)]
pub struct ShutdownToken {
    notified: Arc<Notify>,
    flag: Arc<AtomicBool>,
}

impl ShutdownToken {
    pub fn new() -> Self {
        Self {
            notified: Arc::new(Notify::new()),
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn fire(&self) {
        self.flag.store(true, Ordering::Release);
        self.notified.notify_waiters();
    }

    pub async fn wait(&self) {
        if self.flag.load(Ordering::Acquire) {
            return;
        }
        self.notified.notified().await;
    }

    pub fn is_fired(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

impl Default for ShutdownToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn fire_wakes_waiter() {
        let tok = ShutdownToken::new();
        let tok2 = tok.clone();
        let waiter = tokio::spawn(async move { tok2.wait().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!tok.is_fired());
        tok.fire();
        timeout(Duration::from_millis(200), waiter)
            .await
            .expect("waiter should wake within 200ms")
            .expect("join");
        assert!(tok.is_fired());
    }

    #[tokio::test]
    async fn wait_returns_immediately_if_already_fired() {
        let tok = ShutdownToken::new();
        tok.fire();
        timeout(Duration::from_millis(50), tok.wait())
            .await
            .expect("wait should return immediately when flag already set");
    }

    #[tokio::test]
    async fn double_fire_is_idempotent() {
        let tok = ShutdownToken::new();
        tok.fire();
        tok.fire();
        assert!(tok.is_fired());
    }
}
