#![allow(dead_code)]

//! Per-ticket diff cache (fr:07 §Diff cache).
//!
//! Cache key = Linear issue identifier. Value = `CacheEntry` carrying the
//! tracked triple plus per-ticket runtime state (`cycle_id`,
//! `pending_recheck`).
//!
//! Field ownership:
//! - Dispatcher writes `(status, labels, assignee, last_event_at)` via
//!   `observe`.
//! - Ticket task writes `cycle_id` via `set_cycle_id` / `clear_cycle_id`,
//!   and `pending_recheck` via `take_pending_recheck`.
//! - Dispatcher additionally sets `pending_recheck` on the back-pressure
//!   path (`try_send` Full); see `daemon::dispatcher`.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use time::OffsetDateTime;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::admission::AdmittedTicket;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub repo: String,                   // ghq path of the admission-resolved repo
    pub workflow_path: Option<PathBuf>, // per-repo TOML override (None for top-level)
    pub status: String,
    pub labels: BTreeSet<String>,
    pub assignee: String,
    pub cycle_id: Option<Uuid>,
    pub pending_recheck: bool,
    pub last_event_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOutcome {
    Unchanged,
    Changed,
    NewEntry,
}

#[derive(Default, Clone)]
pub struct DiffCache {
    inner: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

impl DiffCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert / update from the freshly admitted ticket.
    /// Returns the diff classification.
    pub async fn observe(&self, admitted: &AdmittedTicket) -> DiffOutcome {
        let triple_now = (
            admitted.ticket.status.clone(),
            admitted
                .ticket
                .labels
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            admitted.ticket.assignee_id.clone().unwrap_or_default(),
        );

        // Read fast path: classify against current state.
        {
            let map = self.inner.read().await;
            if let Some(entry) = map.get(&admitted.ticket.id) {
                if entry.status == triple_now.0
                    && entry.labels == triple_now.1
                    && entry.assignee == triple_now.2
                {
                    drop(map);
                    let mut w = self.inner.write().await;
                    if let Some(e) = w.get_mut(&admitted.ticket.id) {
                        e.last_event_at = OffsetDateTime::now_utc();
                    }
                    return DiffOutcome::Unchanged;
                }
            }
        }

        // Write path: insert new or update tracked triple.
        let mut map = self.inner.write().await;
        match map.get_mut(&admitted.ticket.id) {
            Some(entry) => {
                entry.status = triple_now.0;
                entry.labels = triple_now.1;
                entry.assignee = triple_now.2;
                entry.last_event_at = OffsetDateTime::now_utc();
                DiffOutcome::Changed
            }
            None => {
                map.insert(
                    admitted.ticket.id.clone(),
                    CacheEntry {
                        repo: admitted.ghq.clone(),
                        workflow_path: None,
                        status: triple_now.0,
                        labels: triple_now.1,
                        assignee: triple_now.2,
                        cycle_id: None,
                        pending_recheck: false,
                        last_event_at: OffsetDateTime::now_utc(),
                    },
                );
                DiffOutcome::NewEntry
            }
        }
    }

    pub async fn snapshot(&self, ticket_id: &str) -> Option<CacheEntry> {
        self.inner.read().await.get(ticket_id).cloned()
    }

    pub async fn set_cycle_id(&self, ticket_id: &str, id: Uuid) {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            e.cycle_id = Some(id);
        }
    }

    pub async fn clear_cycle_id(&self, ticket_id: &str) {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            e.cycle_id = None;
        }
    }

    pub async fn set_pending_recheck(&self, ticket_id: &str) {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            e.pending_recheck = true;
        }
    }

    pub async fn take_pending_recheck(&self, ticket_id: &str) -> bool {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            let prior = e.pending_recheck;
            e.pending_recheck = false;
            prior
        } else {
            false
        }
    }

    pub async fn evict(&self, ticket_id: &str) {
        self.inner.write().await.remove(ticket_id);
    }

    pub async fn in_flight_count(&self) -> usize {
        self.inner
            .read()
            .await
            .values()
            .filter(|e| e.cycle_id.is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::ticket::NormalizedTicket;

    fn admitted(id: &str, status: &str, labels: &[&str], assignee: Option<&str>) -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                id.into(),
                assignee.map(String::from),
                status.into(),
                labels.iter().map(|s| s.to_string()).collect(),
                String::new(),
                String::new(),
            ),
            ghq: "github.com/example/repo".into(),
        }
    }

    #[tokio::test]
    async fn first_observe_is_new_entry() {
        let c = DiffCache::new();
        let r = c.observe(&admitted("t1", "Todo", &["a"], Some("u1"))).await;
        assert_eq!(r, DiffOutcome::NewEntry);
    }

    #[tokio::test]
    async fn second_observe_same_triple_is_unchanged() {
        let c = DiffCache::new();
        let a = admitted("t1", "Todo", &["a"], Some("u1"));
        c.observe(&a).await;
        let r = c.observe(&a).await;
        assert_eq!(r, DiffOutcome::Unchanged);
    }

    #[tokio::test]
    async fn status_change_returns_changed() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
        let r = c
            .observe(&admitted("t1", "InProgress", &[], Some("u1")))
            .await;
        assert_eq!(r, DiffOutcome::Changed);
    }

    #[tokio::test]
    async fn label_reorder_is_unchanged() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &["a", "b"], Some("u1")))
            .await;
        let r = c
            .observe(&admitted("t1", "Todo", &["b", "a"], Some("u1")))
            .await;
        assert_eq!(r, DiffOutcome::Unchanged);
    }

    #[tokio::test]
    async fn label_added_returns_changed() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &["a"], Some("u1"))).await;
        let r = c
            .observe(&admitted("t1", "Todo", &["a", "b"], Some("u1")))
            .await;
        assert_eq!(r, DiffOutcome::Changed);
    }

    #[tokio::test]
    async fn assignee_change_returns_changed() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
        let r = c.observe(&admitted("t1", "Todo", &[], Some("u2"))).await;
        assert_eq!(r, DiffOutcome::Changed);
    }

    #[tokio::test]
    async fn cycle_id_set_clear_round_trips() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
        let id = Uuid::new_v4();
        c.set_cycle_id("t1", id).await;
        assert_eq!(c.snapshot("t1").await.unwrap().cycle_id, Some(id));
        c.clear_cycle_id("t1").await;
        assert_eq!(c.snapshot("t1").await.unwrap().cycle_id, None);
    }

    #[tokio::test]
    async fn take_pending_recheck_clears_and_returns_prior() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
        assert!(!c.take_pending_recheck("t1").await);
        c.set_pending_recheck("t1").await;
        assert!(c.take_pending_recheck("t1").await);
        assert!(!c.take_pending_recheck("t1").await);
    }

    #[tokio::test]
    async fn evict_then_reinsert_is_new_entry() {
        let c = DiffCache::new();
        let a = admitted("t1", "Todo", &[], Some("u1"));
        c.observe(&a).await;
        c.evict("t1").await;
        let r = c.observe(&a).await;
        assert_eq!(r, DiffOutcome::NewEntry);
    }

    #[tokio::test]
    async fn missing_ticket_take_pending_returns_false() {
        let c = DiffCache::new();
        assert!(!c.take_pending_recheck("missing").await);
    }
}
