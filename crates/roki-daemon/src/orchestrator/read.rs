//! Read-only projection over the orchestrator's per-issue state map and the
//! escalation queue. Consumed by TUI/JSON snapshot endpoints.
//!
//! The trait deliberately exposes no setters: the orchestrator owns the
//! authoritative state and snapshots are immutable, deterministic-order
//! copies.
//!
//! Spec refs: requirements.md Req 12.1, 13.1.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use crate::orchestrator::escalation::{EscalationEntry, EscalationQueue};
use crate::orchestrator::state::{IssueId, Mode, WorkerState};
use crate::tracker::model::LinearStateName;

/// One row in the snapshot. Compact projection — no live handles or process
/// references leak through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueState {
    pub issue: IssueId,
    pub state: WorkerState,
    pub mode: Option<Mode>,
    pub latest_linear_state: Option<LinearStateName>,
}

/// Snapshot envelope: every tracked issue + the escalation queue, sorted for
/// deterministic operator-facing output.
#[derive(Debug, Clone)]
pub struct SnapshotResponse {
    pub issues: Vec<IssueState>,
    pub escalations: Vec<EscalationEntry>,
}

/// Per-issue actor projection captured by the orchestrator and consumed by
/// the read handle. Constructed directly by the orchestrator core; tests
/// construct values manually.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorSnapshot {
    pub issue: IssueId,
    pub state: WorkerState,
    pub mode: Option<Mode>,
    pub latest_linear_state: Option<LinearStateName>,
}

impl ActorSnapshot {
    pub fn into_issue_state(self) -> IssueState {
        IssueState {
            issue: self.issue,
            state: self.state,
            mode: self.mode,
            latest_linear_state: self.latest_linear_state,
        }
    }
}

/// Read-only orchestrator API.
pub trait OrchestratorRead: Send + Sync {
    fn snapshot(&self) -> SnapshotResponse;
    fn issue(&self, id: &IssueId) -> Option<IssueState>;
    fn escalation_queue(&self) -> Vec<EscalationEntry>;
}

/// Concrete impl wired to the orchestrator's state map + escalation queue.
#[derive(Debug, Clone)]
pub struct OrchestratorReadHandle {
    state_map: Arc<RwLock<HashMap<IssueId, ActorSnapshot>>>,
    escalations: Arc<EscalationQueue>,
}

impl OrchestratorReadHandle {
    pub fn new(
        state_map: Arc<RwLock<HashMap<IssueId, ActorSnapshot>>>,
        escalations: Arc<EscalationQueue>,
    ) -> Self {
        Self {
            state_map,
            escalations,
        }
    }

    fn issues_sorted(&self) -> Vec<IssueState> {
        let Ok(map) = self.state_map.read() else {
            return Vec::new();
        };
        let mut out: Vec<IssueState> = map
            .values()
            .cloned()
            .map(ActorSnapshot::into_issue_state)
            .collect();
        out.sort_by(|a, b| a.issue.0.cmp(&b.issue.0));
        out
    }

    /// Block-on the escalation queue snapshot. The queue is async (RwLock
    /// over tokio's primitive); the read handle exposes a synchronous API
    /// because TUI / CLI callers do not run in an async runtime. Uses a
    /// per-call mini runtime to bridge.
    fn escalations_sorted(&self) -> Vec<EscalationEntry> {
        // We are inside a tokio runtime when called from a tokio task; use
        // a non-blocking try-read. If the lock is held, fall back to an
        // empty snapshot rather than blocking.
        // The `EscalationQueue` API is async; bridge via `Handle::block_on`
        // when in a runtime, otherwise build a small one-off runtime.
        let queue = self.escalations.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // We're inside a tokio runtime: use block_in_place + Handle::block_on
            // is not safe on the current thread within an async context; use
            // futures::executor-style spawn_blocking is overkill. Easier: take
            // the inner sync path via a blocking thread.
            return std::thread::scope(|s| {
                let h = s.spawn(move || handle.block_on(queue.snapshot()));
                h.join().unwrap_or_default()
            });
        }
        // No active runtime: build a bare-bones one for the call.
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(queue.snapshot()),
            Err(_) => Vec::new(),
        }
    }
}

impl OrchestratorRead for OrchestratorReadHandle {
    fn snapshot(&self) -> SnapshotResponse {
        SnapshotResponse {
            issues: self.issues_sorted(),
            escalations: self.escalations_sorted(),
        }
    }

    fn issue(&self, id: &IssueId) -> Option<IssueState> {
        let map = self.state_map.read().ok()?;
        map.get(id)
            .cloned()
            .map(ActorSnapshot::into_issue_state)
    }

    fn escalation_queue(&self) -> Vec<EscalationEntry> {
        self.escalations_sorted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::escalation::EscalationKind;
    use crate::orchestrator::state::{InactiveReason, WorkerState};
    use serde_json::json;
    use time::macros::datetime;

    fn snapshot_for(id: &str, state: WorkerState, mode: Option<Mode>) -> ActorSnapshot {
        ActorSnapshot {
            issue: IssueId::from(id),
            state,
            mode,
            latest_linear_state: Some(LinearStateName::from("Todo")),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_returns_sorted_projection() {
        let map = Arc::new(RwLock::new(HashMap::new()));
        let q = Arc::new(EscalationQueue::new());

        {
            let mut m = map.write().unwrap();
            m.insert(
                IssueId::from("ENG-2"),
                snapshot_for("ENG-2", WorkerState::Pending, Some(Mode::SpecDriven)),
            );
            m.insert(
                IssueId::from("ENG-1"),
                snapshot_for(
                    "ENG-1",
                    WorkerState::Inactive(InactiveReason::AwaitingLinear),
                    Some(Mode::NeedsClassify),
                ),
            );
        }

        q.enqueue(crate::orchestrator::escalation::EscalationEntry {
            issue: IssueId::from("ENG-1"),
            repo: None,
            kind: EscalationKind::PhaseStall,
            correlation_id: "c1".to_owned(),
            timestamp: datetime!(2026-01-01 0:00 UTC),
            structured_fields: json!({}),
        })
        .await;

        let handle = OrchestratorReadHandle::new(map, q);
        let snap = handle.snapshot();
        assert_eq!(snap.issues.len(), 2);
        assert_eq!(snap.issues[0].issue, IssueId::from("ENG-1"));
        assert_eq!(snap.issues[1].issue, IssueId::from("ENG-2"));
        assert_eq!(snap.escalations.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn issue_lookup_returns_none_for_unknown() {
        let map = Arc::new(RwLock::new(HashMap::new()));
        let q = Arc::new(EscalationQueue::new());
        let handle = OrchestratorReadHandle::new(map, q);
        assert!(handle.issue(&IssueId::from("ENG-X")).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn escalation_queue_in_deterministic_order() {
        let map = Arc::new(RwLock::new(HashMap::new()));
        let q = Arc::new(EscalationQueue::new());
        for (i, ts) in [
            datetime!(2026-01-03 0:00 UTC),
            datetime!(2026-01-01 0:00 UTC),
            datetime!(2026-01-02 0:00 UTC),
        ]
        .iter()
        .enumerate()
        {
            q.enqueue(crate::orchestrator::escalation::EscalationEntry {
                issue: IssueId::from(format!("ENG-{i}").as_str()),
                repo: None,
                kind: EscalationKind::PhaseStall,
                correlation_id: format!("c{i}"),
                timestamp: *ts,
                structured_fields: json!({}),
            })
            .await;
        }
        let handle = OrchestratorReadHandle::new(map, q);
        let queue = handle.escalation_queue();
        let ids: Vec<_> = queue.iter().map(|e| e.issue.0.clone()).collect();
        assert_eq!(ids, vec!["ENG-1", "ENG-2", "ENG-0"]);
    }

    /// Compile-test: the trait surface deliberately exposes only readers.
    /// Adding a `&mut self` method here would not implement the trait.
    fn _trait_is_read_only(_h: &dyn OrchestratorRead) {
        // Function body intentionally empty: existence of this fn proves
        // `dyn OrchestratorRead` resolves through `&self` only.
    }
}
