//! Live-subprocess registry consulted at drain time to identify processes
//! that did not honour SIGTERM within the shutdown window.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::Mutex;

use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inflight {
    pub ticket_id: String,
    pub cycle_id: Uuid,
    pub state_id: String,
    pub visit: u32,
    /// `None` when the OS pid was not observable at registration (post-exit
    /// race in `Child::id()`); the registry still tracks the entry so the
    /// drain can report it, but the SIGKILL path skips unobservable pids.
    pub pid: Option<NonZeroU32>,
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
            pid: NonZeroU32::new(pid),
        }
    }

    #[tokio::test]
    async fn register_then_snapshot_includes_entry() {
        let reg = InflightRegistry::new();
        reg.register(sample("ENG-1", 1234)).await;
        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].pid, NonZeroU32::new(1234));
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
        assert_eq!(snap[0].pid, NonZeroU32::new(2));
    }
}
