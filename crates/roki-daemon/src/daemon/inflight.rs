#![allow(dead_code)]

//! Live-subprocess registry consulted at drain time.
//!
//! `RealStateRunner::run_state` registers right after `Command::spawn` and
//! deregisters right after `child.wait()` reaps. The shutdown drain reads
//! the registry at the cumulative shutdown deadline to populate
//! `Event::ShutdownWindowExceeded.offenders`.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inflight {
    pub ticket_id: String,
    pub cycle_id: Uuid,
    pub state_id: String,
    pub visit: u32,
    pub pid: u32,
}

#[derive(Default, Clone)]
pub struct InflightRegistry {
    inner: Arc<Mutex<HashMap<String, Inflight>>>,
}

impl InflightRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, info: Inflight) {
        let mut g = self.inner.lock().await;
        g.insert(info.ticket_id.clone(), info);
    }

    pub async fn clear(&self, ticket_id: &str) {
        let mut g = self.inner.lock().await;
        g.remove(ticket_id);
    }

    pub async fn snapshot(&self) -> Vec<Inflight> {
        let g = self.inner.lock().await;
        g.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ticket: &str, pid: u32) -> Inflight {
        Inflight {
            ticket_id: ticket.into(),
            cycle_id: Uuid::nil(),
            state_id: "phase-1".into(),
            visit: 1,
            pid,
        }
    }

    #[tokio::test]
    async fn register_then_snapshot_includes_entry() {
        let reg = InflightRegistry::new();
        reg.register(sample("ENG-1", 1234)).await;
        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].pid, 1234);
    }

    #[tokio::test]
    async fn clear_removes_entry_keyed_by_ticket() {
        let reg = InflightRegistry::new();
        reg.register(sample("ENG-1", 1234)).await;
        reg.clear("ENG-1").await;
        assert!(reg.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn second_register_for_same_ticket_replaces_first() {
        let reg = InflightRegistry::new();
        reg.register(sample("ENG-1", 1)).await;
        reg.register(sample("ENG-1", 2)).await;
        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].pid, 2);
    }
}
