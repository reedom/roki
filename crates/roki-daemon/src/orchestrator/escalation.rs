//! Escalation queue + `daemon_directive` routing.
//!
//! `EscalationQueue` is the canonical in-memory record of operator-visible
//! escalation events the daemon has detected. The daemon never writes to
//! Linear directly — for live orchestrators it forwards a `daemon_directive`
//! event onto the orchestrator's stdin so the orchestrator can post the
//! human-facing comment via its Linear MCP. For orchestrator-dead reasons
//! the queue plus the structured log + TUI snapshot are the only surfaces.
//!
//! Spec refs: requirements.md Req 4.12, 12.1, 12.2, 12.3, 12.4, 12.5, 12.6,
//! 12.7.

use std::collections::HashMap;

use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::RwLock;
use tracing::warn;

use crate::engine::orchestrator_session::adapter::OrchestratorSessionHandle;
use crate::engine::orchestrator_session::events::{DaemonDirectivePayload, DaemonEvent};
use crate::orchestrator::state::IssueId;

/// Escalation kind taxonomy. Mirrors design.md "Escalation pipeline".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EscalationKind {
    PhaseStall,
    RetryExhausted,
    FsPoison,
    Orphan,
    OrchestratorCrash,
    OrchestratorUnparseable,
    OrchestratorBudgetExhausted,
}

impl EscalationKind {
    /// Wire-form kind string used inside `daemon_directive` payloads. Keep
    /// in sync with the orchestrator response schema's directive vocabulary.
    pub fn wire(self) -> &'static str {
        match self {
            Self::PhaseStall => "phase_stall",
            Self::RetryExhausted => "retry_exhausted",
            Self::FsPoison => "fs_poison",
            Self::Orphan => "orphan",
            Self::OrchestratorCrash => "orchestrator_crash",
            Self::OrchestratorUnparseable => "orchestrator_unparseable",
            Self::OrchestratorBudgetExhausted => "orchestrator_budget_exhausted",
        }
    }

    /// True iff the kind describes an orchestrator-dead failure mode. The
    /// daemon must NOT attempt a Linear write through the directive path
    /// for these — there is no live orchestrator to consume it.
    pub fn is_orchestrator_dead(self) -> bool {
        matches!(
            self,
            Self::OrchestratorCrash
                | Self::OrchestratorUnparseable
                | Self::OrchestratorBudgetExhausted
        )
    }
}

/// One entry in the queue. `structured_fields` captures the daemon-side
/// payload (paths, errnos, attempts, etc.) without ever embedding a Linear
/// API token, webhook secret, or other operator-declared secret.
#[derive(Debug, Clone)]
pub struct EscalationEntry {
    pub issue: IssueId,
    pub repo: Option<String>,
    pub kind: EscalationKind,
    pub correlation_id: String,
    pub timestamp: OffsetDateTime,
    pub structured_fields: Value,
}

/// In-memory escalation queue keyed by `IssueId`. New entries replace older
/// ones for the same issue id (latest-wins) so stale entries never mask the
/// most recent failure cause.
#[derive(Debug, Default)]
pub struct EscalationQueue {
    entries: RwLock<HashMap<IssueId, EscalationEntry>>,
}

impl EscalationQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert / replace the entry for an issue. Latest wins.
    pub async fn enqueue(&self, entry: EscalationEntry) {
        let mut entries = self.entries.write().await;
        entries.insert(entry.issue.clone(), entry);
    }

    /// Snapshot the queue. Returned vector is a deterministic-ordered copy
    /// (timestamp ascending; issue id breaks ties); the caller cannot mutate
    /// the queue through the returned `Vec`.
    pub async fn snapshot(&self) -> Vec<EscalationEntry> {
        let entries = self.entries.read().await;
        let mut out: Vec<EscalationEntry> = entries.values().cloned().collect();
        out.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.issue.0.cmp(&b.issue.0))
        });
        out
    }

    /// Number of distinct issues currently in the queue.
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// True iff no entries are queued.
    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }

    /// Test/inspection helper: read one entry by id.
    pub async fn get(&self, issue: &IssueId) -> Option<EscalationEntry> {
        self.entries.read().await.get(issue).cloned()
    }

    /// Remove the entry for an issue (operator closed the ticket and the
    /// reconciliation layer dropped the escalation record). No-op when the
    /// issue has no entry.
    pub async fn clear(&self, issue: &IssueId) {
        self.entries.write().await.remove(issue);
    }
}

// ---------------------------------------------------------------------------
// daemon_directive routing
// ---------------------------------------------------------------------------

/// Outcome of routing a daemon-detected escalation through the directive
/// pipeline. Maps to the structured log event + TUI snapshot semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Directive successfully written to the orchestrator's stdin. The
    /// caller is responsible for consuming the orchestrator's
    /// `action=linear_update_done` follow-up.
    Delivered,
    /// Daemon-detected failure on a still-alive orchestrator, but the stdin
    /// write failed (closed channel). Caller routes to
    /// `Inactive(orchestrator_crash)` while retaining the escalation entry.
    DeliveryFailed,
    /// Orchestrator-dead reason: by contract no directive is sent and the
    /// caller surfaces the entry through the queue + structured log only.
    OrchestratorDeadNoDelivery,
    /// The kind requires a live orchestrator handle but none was supplied.
    /// Caller routes the appropriate Inactive reason and retains the entry.
    NoLiveHandle,
}

/// Route a daemon-detected escalation. The caller is responsible for
/// enqueueing the [`EscalationEntry`] on the [`EscalationQueue`] BEFORE
/// invoking this function so the queue snapshot reflects the failure even
/// when delivery fails downstream.
pub async fn route_daemon_directive(
    orchestrator_alive: bool,
    kind: EscalationKind,
    fields: Value,
    correlation_id: String,
    session_handle: Option<&OrchestratorSessionHandle>,
) -> RouteOutcome {
    if kind.is_orchestrator_dead() {
        // Orchestrator-dead reasons must not attempt a Linear write — the
        // directive path requires a live consumer to post the comment.
        warn!(
            target: "orchestrator.escalation",
            kind = kind.wire(),
            correlation_id = %correlation_id,
            "orchestrator-dead escalation surfaced via queue + log only"
        );
        return RouteOutcome::OrchestratorDeadNoDelivery;
    }

    if !orchestrator_alive {
        warn!(
            target: "orchestrator.escalation",
            kind = kind.wire(),
            correlation_id = %correlation_id,
            "no live orchestrator; directive cannot be delivered"
        );
        return RouteOutcome::NoLiveHandle;
    }

    let Some(handle) = session_handle else {
        return RouteOutcome::NoLiveHandle;
    };

    let payload = DaemonDirectivePayload {
        kind: kind.wire().to_owned(),
        correlation_id: correlation_id.clone(),
        repos: collect_repos(&fields),
        worktree_path: extract_path(&fields, "worktree_path"),
        last_subtype: extract_str(&fields, "last_subtype"),
        attempts: extract_u32(&fields, "attempts"),
        window_ms: extract_u64(&fields, "window_ms"),
        errno: extract_i32(&fields, "errno"),
        timestamp: OffsetDateTime::now_utc(),
    };

    match handle
        .stdin_tx
        .send(DaemonEvent::DaemonDirective(payload))
        .await
    {
        Ok(()) => RouteOutcome::Delivered,
        Err(_closed) => {
            warn!(
                target: "orchestrator.escalation",
                kind = kind.wire(),
                correlation_id = %correlation_id,
                "directive delivery failed: orchestrator stdin channel closed"
            );
            RouteOutcome::DeliveryFailed
        }
    }
}

/// Compare the orchestrator's `linear_update_done.linear_writes` ack against
/// the expected ack set. Any expected entry missing from the actual list is
/// a partial write; the caller logs and retains the escalation entry.
pub fn detect_partial_writes(expected: &[String], actual: &[String]) -> Vec<String> {
    expected
        .iter()
        .filter(|e| !actual.iter().any(|a| a == *e))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers: structured-fields extraction
// ---------------------------------------------------------------------------

fn collect_repos(fields: &Value) -> Vec<String> {
    fields
        .get("repos")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn extract_str(fields: &Value, key: &str) -> Option<String> {
    fields.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn extract_path(fields: &Value, key: &str) -> Option<std::path::PathBuf> {
    extract_str(fields, key).map(std::path::PathBuf::from)
}

fn extract_u32(fields: &Value, key: &str) -> Option<u32> {
    fields
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}

fn extract_u64(fields: &Value, key: &str) -> Option<u64> {
    fields.get(key).and_then(Value::as_u64)
}

fn extract_i32(fields: &Value, key: &str) -> Option<i32> {
    fields
        .get(key)
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    fn entry(
        issue: &str,
        kind: EscalationKind,
        ts: OffsetDateTime,
        fields: Value,
    ) -> EscalationEntry {
        EscalationEntry {
            issue: IssueId::from(issue),
            repo: Some("github.com/owner/repo".to_owned()),
            kind,
            correlation_id: format!("corr-{issue}"),
            timestamp: ts,
            structured_fields: fields,
        }
    }

    #[tokio::test]
    async fn enqueue_each_kind_is_recorded() {
        let q = EscalationQueue::new();
        let kinds = [
            EscalationKind::PhaseStall,
            EscalationKind::RetryExhausted,
            EscalationKind::FsPoison,
            EscalationKind::Orphan,
            EscalationKind::OrchestratorCrash,
            EscalationKind::OrchestratorUnparseable,
            EscalationKind::OrchestratorBudgetExhausted,
        ];
        for (i, kind) in kinds.iter().enumerate() {
            q.enqueue(entry(
                &format!("ENG-{i}"),
                *kind,
                datetime!(2026-01-01 0:00 UTC),
                json!({}),
            ))
            .await;
        }
        assert_eq!(q.len().await, kinds.len());
    }

    #[tokio::test]
    async fn latest_entry_replaces_older_for_same_issue() {
        let q = EscalationQueue::new();
        q.enqueue(entry(
            "ENG-9",
            EscalationKind::PhaseStall,
            datetime!(2026-01-01 0:00 UTC),
            json!({"version": 1}),
        ))
        .await;
        q.enqueue(entry(
            "ENG-9",
            EscalationKind::RetryExhausted,
            datetime!(2026-01-02 0:00 UTC),
            json!({"version": 2}),
        ))
        .await;
        assert_eq!(q.len().await, 1);
        let snap = q.snapshot().await;
        assert_eq!(snap[0].kind, EscalationKind::RetryExhausted);
        assert_eq!(snap[0].structured_fields["version"], json!(2));
    }

    #[tokio::test]
    async fn snapshot_returns_deterministic_ordered_copy() {
        let q = EscalationQueue::new();
        q.enqueue(entry(
            "ENG-2",
            EscalationKind::PhaseStall,
            datetime!(2026-01-02 0:00 UTC),
            json!({}),
        ))
        .await;
        q.enqueue(entry(
            "ENG-1",
            EscalationKind::FsPoison,
            datetime!(2026-01-01 0:00 UTC),
            json!({}),
        ))
        .await;
        q.enqueue(entry(
            "ENG-3",
            EscalationKind::Orphan,
            datetime!(2026-01-03 0:00 UTC),
            json!({}),
        ))
        .await;
        let snap = q.snapshot().await;
        let ids: Vec<_> = snap.iter().map(|e| e.issue.0.clone()).collect();
        assert_eq!(ids, vec!["ENG-1", "ENG-2", "ENG-3"]);

        // Mutating the returned Vec must not affect the in-memory queue.
        let mut local = snap;
        local.clear();
        assert_eq!(q.len().await, 3);
    }

    #[tokio::test]
    async fn route_directive_for_orchestrator_dead_skips_delivery() {
        let outcome = route_daemon_directive(
            true,
            EscalationKind::OrchestratorCrash,
            json!({}),
            "corr-x".to_owned(),
            None,
        )
        .await;
        assert_eq!(outcome, RouteOutcome::OrchestratorDeadNoDelivery);
    }

    #[tokio::test]
    async fn route_directive_with_no_live_handle_returns_no_live_handle() {
        let outcome = route_daemon_directive(
            false,
            EscalationKind::PhaseStall,
            json!({}),
            "corr-y".to_owned(),
            None,
        )
        .await;
        assert_eq!(outcome, RouteOutcome::NoLiveHandle);
    }

    #[test]
    fn detect_partial_writes_returns_missing_subset() {
        let expected = vec!["label:roki:impl".to_owned(), "comment_posted:1".to_owned()];
        let actual = vec!["label:roki:impl".to_owned()];
        let missing = detect_partial_writes(&expected, &actual);
        assert_eq!(missing, vec!["comment_posted:1".to_owned()]);
    }

    #[test]
    fn detect_partial_writes_empty_when_complete() {
        let expected = vec!["a".to_owned(), "b".to_owned()];
        let actual = vec!["b".to_owned(), "a".to_owned()];
        assert!(detect_partial_writes(&expected, &actual).is_empty());
    }
}
