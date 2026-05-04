//! Bridge between the tracker (webhook + poller) and the orchestrator core.
//!
//! Owns the dedup index that absorbs duplicate observations of the same
//! issue and decides whether each new normalized event should:
//! - launch a fresh orchestrator session (`LaunchFresh`),
//! - update the in-flight session's snapshot in place (`UpdateInPlace`),
//! - silently drop because pre-admission failed (`Drop`),
//! - terminate the in-flight session because the daemon-side stop conditions
//!   fired mid-flight (`TerminateInFlight`).
//!
//! The retry-budget interaction lives in 6.9; this module only emits the
//! `TerminationReason` so the budget bookkeeping can act on it.
//!
//! Spec refs: requirements.md Req 3.10, 3.11, 3.12, 3.13, 3.14.

use std::collections::HashMap;

use tokio::sync::RwLock;

use crate::orchestrator::state::{IssueId, Mode, WorkerState};
#[cfg(test)]
use crate::orchestrator::state::InactiveReason;
use crate::tracker::model::NormalizedIssue;
use crate::tracker::pre_admission::{
    AdmissionDecision, assignment_lost, roki_ready_removed,
};

/// One row in the dedup index: the latest known projection of the issue plus
/// the orchestrator-side bookkeeping (state, mode, in-flight handle ids).
#[derive(Debug, Clone)]
pub struct DedupEntry {
    pub state: WorkerState,
    pub mode: Option<Mode>,
    pub latest_normalized: NormalizedIssue,
    pub in_flight_orch: Option<u64>,
    pub in_flight_phase: Option<u64>,
}

impl DedupEntry {
    pub fn new(state: WorkerState, mode: Option<Mode>, issue: NormalizedIssue) -> Self {
        Self {
            state,
            mode,
            latest_normalized: issue,
            in_flight_orch: None,
            in_flight_phase: None,
        }
    }

    /// True iff the entry is in a state where mid-flight stop signals can
    /// terminate work (Pending / Active / Backoff). Cleaning is already on
    /// the wind-down path and Inactive holds no live worker.
    pub fn is_in_flight(&self) -> bool {
        matches!(
            self.state,
            WorkerState::Pending | WorkerState::Active | WorkerState::Backoff
        )
    }
}

/// Why the bridge fired `TerminateInFlight`. Mirrors the
/// `TransitionTrigger` taxonomy on the daemon side without coupling to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationReason {
    AssignmentLost,
    RokiReadyRemoved,
}

/// Outcome of `DedupIndex::observe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserveOutcome {
    LaunchFresh {
        issue: NormalizedIssue,
        mode: Mode,
    },
    UpdateInPlace,
    Drop,
    TerminateInFlight {
        reason: TerminationReason,
    },
}

/// Per-issue dedup table guarding double-launch and absorbing duplicate
/// webhook / poll events.
#[derive(Debug, Default)]
pub struct DedupIndex {
    entries: RwLock<HashMap<IssueId, DedupEntry>>,
}

impl DedupIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test/inspection helper: snapshot one entry by id.
    pub async fn snapshot(&self, id: &IssueId) -> Option<DedupEntry> {
        self.entries.read().await.get(id).cloned()
    }

    /// Test helper: pre-seed an entry. The runtime layer that owns the dedup
    /// index uses the same path on restart-recovery to rehydrate state.
    pub async fn seed(&self, issue: NormalizedIssue, entry: DedupEntry) {
        self.entries.write().await.insert(issue.issue.clone(), entry);
    }

    /// Bind a freshly-launched orchestrator handle id to an entry. Called by
    /// the runtime once `LaunchFresh` has actually spawned the orchestrator.
    pub async fn bind_orchestrator(&self, id: &IssueId, handle: u64) {
        if let Some(entry) = self.entries.write().await.get_mut(id) {
            entry.in_flight_orch = Some(handle);
        }
    }

    /// Apply a normalized observation against the dedup table. Returns the
    /// `ObserveOutcome` the runtime layer should act on.
    pub async fn observe(
        &self,
        issue: NormalizedIssue,
        decision: AdmissionDecision,
    ) -> ObserveOutcome {
        let mut entries = self.entries.write().await;
        let key = issue.issue.clone();
        let prior = entries.get(&key).cloned();

        // Mid-flight termination always takes priority over admission re-evaluation:
        // assignment loss / `roki:ready` removal must stop in-flight work even
        // if the new snapshot would have admitted from scratch.
        if let Some(ref entry) = prior
            && entry.is_in_flight()
        {
            if assignment_lost(&entry.latest_normalized, &issue) {
                Self::write_snapshot(&mut entries, &key, &issue, entry);
                return ObserveOutcome::TerminateInFlight {
                    reason: TerminationReason::AssignmentLost,
                };
            }
            if roki_ready_removed(&entry.latest_normalized, &issue) {
                Self::write_snapshot(&mut entries, &key, &issue, entry);
                return ObserveOutcome::TerminateInFlight {
                    reason: TerminationReason::RokiReadyRemoved,
                };
            }
        }

        match decision {
            AdmissionDecision::Skip { .. } => {
                // If we held an entry only to wait on admission, refresh its
                // snapshot so the next observation compares against the
                // latest known state.
                if let Some(mut entry) = prior {
                    entry.latest_normalized = issue.clone();
                    entries.insert(key, entry);
                }
                ObserveOutcome::Drop
            }
            AdmissionDecision::Admit { issue: admitted, mode } => {
                match prior {
                    Some(entry) if entry.is_in_flight() => {
                        // Same admission shape — just refresh the snapshot.
                        let mut updated = entry;
                        updated.latest_normalized = admitted;
                        entries.insert(key, updated);
                        ObserveOutcome::UpdateInPlace
                    }
                    Some(entry) if matches!(entry.state, WorkerState::Inactive(_)) => {
                        // Re-admission from a terminal state. Mode is recomputed
                        // from the fresh `AdmissionDecision`.
                        let new_entry = DedupEntry::new(
                            WorkerState::Pending,
                            Some(mode),
                            admitted.clone(),
                        );
                        entries.insert(key, new_entry);
                        ObserveOutcome::LaunchFresh {
                            issue: admitted,
                            mode,
                        }
                    }
                    Some(entry) if matches!(entry.state, WorkerState::Cleaning) => {
                        // Cleaning is a non-terminal wind-down state; we
                        // refresh the snapshot but do not re-launch until the
                        // runtime confirms `Cleaning -> Inactive` completes.
                        let mut updated = entry;
                        updated.latest_normalized = admitted;
                        entries.insert(key, updated);
                        ObserveOutcome::UpdateInPlace
                    }
                    Some(_) | None => {
                        let new_entry = DedupEntry::new(
                            WorkerState::Pending,
                            Some(mode),
                            admitted.clone(),
                        );
                        entries.insert(key, new_entry);
                        ObserveOutcome::LaunchFresh {
                            issue: admitted,
                            mode,
                        }
                    }
                }
            }
        }
    }

    fn write_snapshot(
        entries: &mut HashMap<IssueId, DedupEntry>,
        key: &IssueId,
        issue: &NormalizedIssue,
        prior: &DedupEntry,
    ) {
        let mut updated = prior.clone();
        updated.latest_normalized = issue.clone();
        entries.insert(key.clone(), updated);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::tracker::model::{
        IssueId as ModelIssueId, LABEL_ROKI_IMPL, LABEL_ROKI_READY, LinearLabel,
        LinearStateName, LinearUserId,
    };

    fn issue(
        id: &str,
        assignee: Option<&str>,
        labels: &[&str],
        state: &str,
    ) -> NormalizedIssue {
        NormalizedIssue {
            issue: ModelIssueId::from(id),
            title: id.to_owned(),
            body: "".to_owned(),
            current_linear_state: LinearStateName::from(state),
            labels: labels.iter().map(|s| LinearLabel::from(*s)).collect(),
            assignee: assignee.map(LinearUserId::from),
        }
    }

    fn admit(issue: &NormalizedIssue, mode: Mode) -> AdmissionDecision {
        AdmissionDecision::Admit {
            issue: issue.clone(),
            mode,
        }
    }

    fn drop_decision() -> AdmissionDecision {
        AdmissionDecision::Skip {
            reason: crate::tracker::pre_admission::SkipReason::AssigneeMismatch,
        }
    }

    #[tokio::test]
    async fn duplicate_webhook_for_in_flight_issue_updates_snapshot_only() {
        let index = DedupIndex::new();
        let original = issue("ENG-1", Some("u1"), &[LABEL_ROKI_READY], "Todo");
        // Seed an in-flight entry mimicking an active orchestrator session.
        index
            .seed(
                original.clone(),
                DedupEntry {
                    state: WorkerState::Active,
                    mode: Some(Mode::NeedsClassify),
                    latest_normalized: original.clone(),
                    in_flight_orch: Some(7),
                    in_flight_phase: None,
                },
            )
            .await;

        let mut updated = original.clone();
        updated.title = "renamed".to_owned();
        let outcome = index
            .observe(updated.clone(), admit(&updated, Mode::NeedsClassify))
            .await;
        assert_eq!(outcome, ObserveOutcome::UpdateInPlace);

        let snap = index
            .snapshot(&ModelIssueId::from("ENG-1"))
            .await
            .expect("entry persists");
        assert_eq!(snap.latest_normalized.title, "renamed");
        assert_eq!(snap.in_flight_orch, Some(7), "in-flight handle preserved");
    }

    #[tokio::test]
    async fn inactive_entry_re_admits_and_recomputes_mode() {
        let index = DedupIndex::new();
        let prior = issue("ENG-2", Some("u1"), &[LABEL_ROKI_READY], "Todo");
        index
            .seed(
                prior.clone(),
                DedupEntry {
                    state: WorkerState::Inactive(InactiveReason::AwaitingLinear),
                    mode: Some(Mode::NeedsClassify),
                    latest_normalized: prior.clone(),
                    in_flight_orch: None,
                    in_flight_phase: None,
                },
            )
            .await;

        // New observation: spec-driven label set is now present.
        let promoted = issue(
            "ENG-2",
            Some("u1"),
            &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
            "Todo",
        );
        let outcome = index
            .observe(promoted.clone(), admit(&promoted, Mode::SpecDriven))
            .await;
        assert!(matches!(
            outcome,
            ObserveOutcome::LaunchFresh { mode: Mode::SpecDriven, .. }
        ));
        let snap = index
            .snapshot(&ModelIssueId::from("ENG-2"))
            .await
            .unwrap();
        assert_eq!(snap.state, WorkerState::Pending);
        assert_eq!(snap.mode, Some(Mode::SpecDriven));
    }

    #[tokio::test]
    async fn assignment_loss_mid_flight_returns_terminate_without_relaunch() {
        let index = DedupIndex::new();
        let prior = issue("ENG-3", Some("u1"), &[LABEL_ROKI_READY], "Todo");
        index
            .seed(
                prior.clone(),
                DedupEntry {
                    state: WorkerState::Active,
                    mode: Some(Mode::SpecDriven),
                    latest_normalized: prior.clone(),
                    in_flight_orch: Some(11),
                    in_flight_phase: Some(12),
                },
            )
            .await;

        let lost = issue("ENG-3", None, &[LABEL_ROKI_READY], "Todo");
        let outcome = index
            .observe(lost.clone(), admit(&lost, Mode::SpecDriven))
            .await;
        assert_eq!(
            outcome,
            ObserveOutcome::TerminateInFlight {
                reason: TerminationReason::AssignmentLost,
            }
        );
        // Snapshot is updated even on terminate so the next observation
        // compares against the most recent state. Retry-budget interaction
        // lives in 6.9; the bridge does not consume budget here.
        let snap = index
            .snapshot(&ModelIssueId::from("ENG-3"))
            .await
            .unwrap();
        assert!(snap.latest_normalized.assignee.is_none());
    }

    #[tokio::test]
    async fn roki_ready_removal_mid_flight_returns_terminate() {
        let index = DedupIndex::new();
        let prior = issue("ENG-4", Some("u1"), &[LABEL_ROKI_READY], "Todo");
        index
            .seed(
                prior.clone(),
                DedupEntry {
                    state: WorkerState::Pending,
                    mode: Some(Mode::NeedsClassify),
                    latest_normalized: prior.clone(),
                    in_flight_orch: Some(99),
                    in_flight_phase: None,
                },
            )
            .await;

        let unlabeled = issue("ENG-4", Some("u1"), &[], "Todo");
        let outcome = index
            .observe(unlabeled.clone(), drop_decision())
            .await;
        assert_eq!(
            outcome,
            ObserveOutcome::TerminateInFlight {
                reason: TerminationReason::RokiReadyRemoved,
            }
        );
    }

    #[tokio::test]
    async fn skip_decision_for_unknown_issue_drops() {
        let index = DedupIndex::new();
        let i = issue("ENG-5", None, &[], "Todo");
        let outcome = index.observe(i, drop_decision()).await;
        assert_eq!(outcome, ObserveOutcome::Drop);
        assert!(
            index.snapshot(&ModelIssueId::from("ENG-5")).await.is_none(),
            "no entry stored for dropped unknown issue"
        );
    }

    #[tokio::test]
    async fn first_admission_launches_fresh() {
        let index = DedupIndex::new();
        let i = issue("ENG-6", Some("u1"), &[LABEL_ROKI_READY], "Todo");
        let outcome = index
            .observe(i.clone(), admit(&i, Mode::NeedsClassify))
            .await;
        match outcome {
            ObserveOutcome::LaunchFresh { issue, mode } => {
                assert_eq!(issue.issue, ModelIssueId::from("ENG-6"));
                assert_eq!(mode, Mode::NeedsClassify);
            }
            other => panic!("expected LaunchFresh, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn label_set_round_trips_through_seed_and_snapshot() {
        // Smoke check that BTreeSet<LinearLabel> survives the seed/snapshot
        // round trip — the dedup index doesn't filter labels itself.
        let index = DedupIndex::new();
        let i = issue("ENG-7", Some("u1"), &[LABEL_ROKI_READY, LABEL_ROKI_IMPL], "Todo");
        index
            .seed(
                i.clone(),
                DedupEntry::new(WorkerState::Pending, Some(Mode::SpecDriven), i.clone()),
            )
            .await;
        let snap = index
            .snapshot(&ModelIssueId::from("ENG-7"))
            .await
            .unwrap();
        let names: BTreeSet<&str> = snap
            .latest_normalized
            .labels
            .iter()
            .map(|l| l.0.as_str())
            .collect();
        assert!(names.contains(LABEL_ROKI_READY));
        assert!(names.contains(LABEL_ROKI_IMPL));
    }
}
