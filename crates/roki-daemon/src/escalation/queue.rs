//! In-memory escalation queue. Pushes emit `escalation_added` to the
//! daemon-scoped event log. Eviction drops cycle-bound entries by ticket id.

use std::sync::Arc;

use time::format_description::well_known::Rfc3339;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::engine::outcome::{FailureKind, PhaseKind};
use crate::escalation::entry::EscalationEntry;
use crate::escalation::ring::{PushOutcome, Ring};
use crate::events::{Event, EventWriter, FailureMetaSer};

pub struct EscalationQueue {
    inner: Mutex<Ring<EscalationEntry>>,
    daemon_writer: Arc<Mutex<EventWriter>>,
}

impl EscalationQueue {
    pub fn new(capacity: usize, daemon_writer: Arc<Mutex<EventWriter>>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Ring::new(capacity)),
            daemon_writer,
        })
    }

    pub async fn push_cycle(
        &self,
        ticket_id: String,
        cycle_id: Uuid,
        failure_kind: FailureKind,
        phase: PhaseKind,
        error_text: String,
    ) {
        let entry = EscalationEntry::cycle(
            ticket_id.clone(),
            cycle_id,
            failure_kind,
            phase,
            error_text,
        );
        self.insert_and_emit(entry).await;
    }

    pub async fn push_daemon(&self, failure_kind: FailureKind, error_text: String) {
        let entry = EscalationEntry::daemon(failure_kind, error_text);
        self.insert_and_emit(entry).await;
    }

    async fn insert_and_emit(&self, entry: EscalationEntry) {
        let snapshot = entry.clone();
        {
            let mut ring = self.inner.lock().await;
            if let PushOutcome::Overflowed { dropped } = ring.push(entry) {
                tracing::warn!(
                    dropped_kind = dropped.failure_kind.as_str(),
                    dropped_ticket_id = dropped.ticket_id.as_deref().unwrap_or("<daemon>"),
                    "escalation queue overflow; oldest entry dropped"
                );
            }
        }
        let mut w = self.daemon_writer.lock().await;
        let _ = w.emit(&Event::EscalationAdded {
            ts: snapshot
                .timestamp
                .format(&Rfc3339)
                .unwrap_or_else(|_| String::new()),
            ticket_id: snapshot.ticket_id.clone(),
            cycle_id: snapshot.cycle_id.map(|u| u.to_string()),
            failure: FailureMetaSer {
                kind: snapshot.failure_kind.as_str().to_string(),
                phase: snapshot.phase.map(|p| p.as_str().to_string()),
                iter: 0,
                exit_code: None,
                error_text: snapshot.error_text.clone(),
            },
        });
    }

    pub async fn evict_ticket(&self, ticket_id: &str) {
        let mut ring = self.inner.lock().await;
        ring.retain(|e| e.ticket_id.as_deref() != Some(ticket_id));
    }

    pub async fn snapshot(&self) -> Vec<EscalationEntry> {
        let ring = self.inner.lock().await;
        ring.iter().cloned().collect()
    }

    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn writer_for(dir: &Path) -> Arc<Mutex<EventWriter>> {
        let w = EventWriter::open(dir, "_daemon").expect("open daemon writer");
        Arc::new(Mutex::new(w))
    }

    #[tokio::test]
    async fn push_cycle_appends_entry() {
        let dir = TempDir::new().unwrap();
        let q = EscalationQueue::new(8, writer_for(dir.path()));
        q.push_cycle(
            "T-1".into(),
            Uuid::new_v4(),
            FailureKind::FsPoison,
            PhaseKind::Post,
            "boom".into(),
        )
        .await;
        assert_eq!(q.len().await, 1);
        let snap = q.snapshot().await;
        assert_eq!(snap[0].ticket_id.as_deref(), Some("T-1"));
    }

    #[tokio::test]
    async fn push_daemon_leaves_cycle_fields_none() {
        let dir = TempDir::new().unwrap();
        let q = EscalationQueue::new(4, writer_for(dir.path()));
        q.push_daemon(FailureKind::FsPoison, "no cycle".into()).await;
        let snap = q.snapshot().await;
        assert!(snap[0].ticket_id.is_none());
        assert!(snap[0].cycle_id.is_none());
        assert!(snap[0].phase.is_none());
    }

    #[tokio::test]
    async fn evict_ticket_drops_only_matching_cycle_entries() {
        let dir = TempDir::new().unwrap();
        let q = EscalationQueue::new(8, writer_for(dir.path()));
        q.push_cycle(
            "T-1".into(),
            Uuid::new_v4(),
            FailureKind::FsPoison,
            PhaseKind::Post,
            "x".into(),
        )
        .await;
        q.push_cycle(
            "T-2".into(),
            Uuid::new_v4(),
            FailureKind::FsPoison,
            PhaseKind::Post,
            "y".into(),
        )
        .await;
        q.push_daemon(FailureKind::FsPoison, "z".into()).await;
        q.evict_ticket("T-1").await;
        let snap = q.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|e| e.ticket_id.as_deref() != Some("T-1")));
        assert!(snap.iter().any(|e| e.ticket_id.is_none()));
    }

    #[tokio::test]
    async fn overflow_drops_oldest_and_writes_event() {
        let dir = TempDir::new().unwrap();
        let q = EscalationQueue::new(2, writer_for(dir.path()));
        for i in 0..3 {
            q.push_cycle(
                format!("T-{i}"),
                Uuid::new_v4(),
                FailureKind::FsPoison,
                PhaseKind::Post,
                format!("e{i}"),
            )
            .await;
        }
        let snap = q.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].ticket_id.as_deref(), Some("T-1"));
        assert_eq!(snap[1].ticket_id.as_deref(), Some("T-2"));

        let body = std::fs::read_to_string(
            crate::events::events_path(dir.path(), "_daemon"),
        )
        .unwrap();
        assert_eq!(
            body.lines().filter(|l| l.contains("\"event\":\"escalation_added\"")).count(),
            3,
            "one escalation_added per push"
        );
    }
}
