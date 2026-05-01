//! Restart recovery via Linear plus filesystem reconciliation (task 3.3).
//!
//! On daemon start, the orchestrator must rebuild its in-memory per-`(repo,
//! issue)` state without relying on a database (Requirement 8.5, 10.1-10.4).
//! This module owns the reconciliation step:
//!
//! 1. Inventory every existing workspace under `<workspace_root>/<repo>/<issue>/`
//!    (Requirement 10.1) using [`crate::workspace::Workspace::list_existing`].
//! 2. Query Linear for the active-issue slice for each scope the daemon serves
//!    via the [`RecoveryLinearReader`] trait — a one-shot read surface that
//!    keeps the recovery path independent of the polling loop owned by
//!    [`crate::tracker::linear::LinearTracker`].
//! 3. For every key in the union of both sets, classify the `(repo, issue)`
//!    into one of four [`RecoveryDecision`] outcomes per the design.md
//!    "RecoveryReconciler" matrix:
//!    | workspace | linear active | decision           |
//!    | :-------: | :-----------: | :----------------- |
//!    | yes       | yes           | `ResumeActive`     |
//!    | yes       | no            | `OrphanedWorkspace`|
//!    | no        | yes           | `FreshQueued`      |
//!    | no        | no            | `NoOp` (absent)    |
//! 4. Emit synthetic [`NormalizedIssue`] events (state =
//!    [`TrackerIssueState::Active`]) into the orchestrator's tracker inbox for
//!    `ResumeActive` and `FreshQueued`, so the existing `WorkerActor` driver
//!    creates / re-uses the workspace and resumes the active-state lifecycle
//!    (Requirements 10.1, 10.3). For `OrphanedWorkspace`, a structured warn
//!    event names the workspace path; the directory is retained and no actor
//!    is spawned (Requirement 10.2). The "absent on both sides" case is a
//!    no-op and is not emitted as a decision (Requirement 10.4 — daemon
//!    writes nothing extra to disk).
//!
//! The reconciliation is split into a pure decision function
//! ([`reconcile_decisions`]) and a side-effect-laden driver ([`run_recovery`])
//! so unit tests can pin the matrix without booting any IO. The
//! [`RecoveryLinearReader`] trait is exposed so the integration test in
//! `tests/orchestrator_recovery.rs` can inject a stub Linear reader without
//! standing up the full polling loop.
//!
//! ## Disk-write budget
//!
//! Per Requirement 10.4 the daemon writes no per-issue runtime state to disk
//! except (a) workspace contents the agent itself produces and (b) the
//! structured logs the daemon emits. Recovery honours this by never persisting
//! a sidecar state file: the only filesystem touch this module performs is
//! through the existing [`crate::workspace::Workspace`] adapter when a
//! `FreshQueued` decision is realised (which the orchestrator handles in its
//! existing `Active`-state path).

use std::collections::{BTreeSet, HashMap, HashSet};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::orchestrator::state::{IssueId, RepoId};
use crate::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use crate::worktrees::{RecoveryListing, RecoveryListingError};

/// Errors surfaced by the recovery driver.
///
/// Recovery wraps the workspace-listing error and the operator-supplied
/// [`RecoveryLinearReader`] error so callers can distinguish a filesystem
/// failure from a Linear read failure when reporting startup health.
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// Listing the workspace root failed; recovery cannot proceed because the
    /// daemon has no authoritative inventory of existing workspaces.
    #[error("recovery: workspace listing failed: {0}")]
    Workspace(#[from] RecoveryListingError),

    /// The Linear reader failed; recovery cannot determine which workspaces
    /// are still active so it refuses to make decisions to avoid mistakenly
    /// orphaning live work.
    #[error("recovery: linear read failed: {0}")]
    LinearRead(String),
}

/// Per-`(repo, issue)` recovery outcome computed by [`reconcile_decisions`].
///
/// Variants intentionally match the matrix in design.md "RecoveryReconciler"
/// so the structured log emitted at startup can name the decision verbatim.
/// `NoOp` ("absent on both sides") is documented as an explicit variant for
/// completeness in the per-key API even though the reconciler never emits it
/// — see [`reconcile_decisions`] for the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// Workspace dir + Linear-active issue both present. Resume the
    /// `Active`-state lifecycle by emitting a synthetic
    /// [`NormalizedIssue`] with [`TrackerIssueState::Active`].
    ResumeActive { repo: RepoId, issue: IssueId },
    /// Workspace dir present but Linear has no matching active issue. The
    /// directory is retained and a warn event is emitted naming the
    /// `(repo, issue)` key (Requirement 10.2).
    OrphanedWorkspace { repo: RepoId, issue: IssueId },
    /// Linear-active issue with no workspace on disk. Spawn the worker actor
    /// and let the orchestrator's existing `Queued -> Active` path create the
    /// workspace (Requirement 10.3).
    FreshQueued { repo: RepoId, issue: IssueId },
    /// Absent on both sides. Provided for symmetry with the design.md
    /// matrix; never emitted by [`reconcile_decisions`] (Requirement 10.4
    /// "no-op").
    NoOp { repo: RepoId, issue: IssueId },
}

impl RecoveryDecision {
    /// Identifying `(repo, issue)` key for this decision.
    pub fn key(&self) -> (&RepoId, &IssueId) {
        match self {
            Self::ResumeActive { repo, issue }
            | Self::OrphanedWorkspace { repo, issue }
            | Self::FreshQueued { repo, issue }
            | Self::NoOp { repo, issue } => (repo, issue),
        }
    }
}

/// One-shot read surface published by Linear-aware adapters for the recovery
/// scan.
///
/// The polling loop in [`crate::tracker::linear::LinearTracker`] is
/// continuous; recovery needs a single point-in-time query. Rather than
/// teach the polling loop to short-circuit, recovery depends on this thin
/// trait so:
///
/// * the integration test in `tests/orchestrator_recovery.rs` can inject a
///   deterministic stub without reaching for `wiremock`;
/// * a future production reader (a small `LinearTracker::query_active_once`
///   method, or a fresh GraphQL one-shot client) can implement this trait
///   without changing the orchestrator wiring.
#[async_trait]
pub trait RecoveryLinearReader: Send + Sync {
    /// Return every active Linear issue, keyed by `(repo, issue)`.
    ///
    /// Implementations must return one entry per active `(repo, issue)`; the
    /// caller treats the keys as authoritative for the "Linear active"
    /// column of the recovery matrix. Errors must be reported as a string so
    /// implementations can report adapter-specific failure modes without
    /// coupling this trait to any concrete error type.
    async fn active_issues(&self) -> Result<HashMap<(RepoId, IssueId), NormalizedIssue>, String>;
}

/// Pure decision function for the recovery matrix.
///
/// Inputs:
/// * `existing_workspaces` — every `(repo, issue)` discovered via
///   [`Workspace::list_existing`]. Duplicate keys collapse to a single entry
///   (Requirement 4.5: each `(repo, issue)` maps to exactly one workspace).
/// * `active_linear_issues` — every Linear-active issue keyed by
///   `(repo, issue)` (from [`RecoveryLinearReader::active_issues`]).
///
/// Output: one [`RecoveryDecision`] per key in the union of the two input
/// sets. The "absent on both sides" case (`NoOp`) is never emitted because
/// such keys are not present in either input — Requirement 10.4 is satisfied
/// by exclusion: the daemon allocates no slot for a key it has no evidence
/// of.
///
/// Decisions are returned sorted by `(repo, issue)` lexicographically so the
/// startup log is stable across runs and the integration test can rely on
/// deterministic ordering.
pub fn reconcile_decisions(
    existing_workspaces: &[(RepoId, IssueId)],
    active_linear_issues: &HashMap<(RepoId, IssueId), NormalizedIssue>,
) -> Vec<RecoveryDecision> {
    // Use a BTreeSet of (repo_str, issue_str) for stable ordering. We
    // re-derive the typed keys from the inputs so duplicate raw entries in
    // either source do not double-count.
    let mut workspace_keys: HashSet<(String, String)> =
        HashSet::with_capacity(existing_workspaces.len());
    for (repo, issue) in existing_workspaces {
        workspace_keys.insert((repo.as_str().to_string(), issue.as_str().to_string()));
    }
    let mut linear_keys: HashSet<(String, String)> =
        HashSet::with_capacity(active_linear_issues.len());
    for (repo, issue) in active_linear_issues.keys() {
        linear_keys.insert((repo.as_str().to_string(), issue.as_str().to_string()));
    }

    let mut union: BTreeSet<(String, String)> = BTreeSet::new();
    union.extend(workspace_keys.iter().cloned());
    union.extend(linear_keys.iter().cloned());

    union
        .into_iter()
        .map(|(repo_s, issue_s)| {
            let repo = RepoId::new(repo_s.clone());
            let issue = IssueId::new(issue_s.clone());
            let has_workspace = workspace_keys.contains(&(repo_s.clone(), issue_s.clone()));
            let has_linear = linear_keys.contains(&(repo_s, issue_s));
            match (has_workspace, has_linear) {
                (true, true) => RecoveryDecision::ResumeActive { repo, issue },
                (true, false) => RecoveryDecision::OrphanedWorkspace { repo, issue },
                (false, true) => RecoveryDecision::FreshQueued { repo, issue },
                // Never emitted: the union excludes keys absent on both sides.
                (false, false) => RecoveryDecision::NoOp { repo, issue },
            }
        })
        .collect()
}

/// Drive a full recovery scan and post synthetic tracker events for the
/// decisions that resume / start work.
///
/// `tracker_sender` is the same `mpsc::Sender<NormalizedIssue>` that feeds
/// the orchestrator's tracker inbox; passing it explicitly (rather than
/// reaching into the orchestrator) keeps recovery decoupled from the
/// orchestrator's internals and makes the integration test trivial.
///
/// On `ResumeActive` and `FreshQueued` the function emits a
/// [`NormalizedIssue`] with state = [`TrackerIssueState::Active`] so the
/// orchestrator's existing `Discovered -> Queued -> Active` path takes
/// over. The orchestrator's worker actor then calls `workspace.ensure`,
/// which is idempotent for the resume case (the directory already exists)
/// and creates a fresh directory for the queued case.
///
/// On `OrphanedWorkspace` the function emits a single warn-level structured
/// log event identifying the `(repo, issue)` key and proceeds without
/// spawning an actor or deleting the directory (Requirement 10.2).
///
/// Returns the list of decisions in the same order they were emitted so
/// callers (notably the integration test) can assert the post-recovery
/// post-conditions per Requirement 10.1 / 10.2 / 10.3.
pub async fn run_recovery(
    workspace: &dyn RecoveryListing,
    reader: &dyn RecoveryLinearReader,
    tracker_sender: &mpsc::Sender<NormalizedIssue>,
) -> Result<Vec<RecoveryDecision>, RecoveryError> {
    let existing_full = workspace.list_existing().await?;
    // We discard the path component for decision-making — recovery only
    // needs the `(repo, issue)` key. The path is reported back by the
    // workspace adapter on demand for orphan logging if needed.
    let existing_keys: Vec<(RepoId, IssueId)> = existing_full
        .iter()
        .map(|(repo, issue, _path)| (repo.clone(), issue.clone()))
        .collect();
    let path_lookup: HashMap<(String, String), std::path::PathBuf> = existing_full
        .iter()
        .map(|(repo, issue, path)| {
            (
                (repo.as_str().to_string(), issue.as_str().to_string()),
                path.clone(),
            )
        })
        .collect();

    let active_linear = reader
        .active_issues()
        .await
        .map_err(RecoveryError::LinearRead)?;

    let decisions = reconcile_decisions(&existing_keys, &active_linear);

    info!(
        target: "orchestrator.recovery",
        existing_workspaces = existing_keys.len(),
        active_linear_issues = active_linear.len(),
        decisions = decisions.len(),
        "recovery scan computed reconciliation decisions",
    );

    for decision in &decisions {
        match decision {
            RecoveryDecision::ResumeActive { repo, issue } => {
                let synthetic = synthetic_active_event(repo, issue, &active_linear);
                if tracker_sender.send(synthetic).await.is_err() {
                    warn!(
                        target: "orchestrator.recovery",
                        repo = %repo.as_str(),
                        issue = %issue.as_str(),
                        "tracker inbox closed before recovery could resume issue",
                    );
                } else {
                    info!(
                        target: "orchestrator.recovery",
                        repo = %repo.as_str(),
                        issue = %issue.as_str(),
                        decision = "ResumeActive",
                        "recovery resumed active issue",
                    );
                }
            }
            RecoveryDecision::FreshQueued { repo, issue } => {
                let synthetic = synthetic_active_event(repo, issue, &active_linear);
                if tracker_sender.send(synthetic).await.is_err() {
                    warn!(
                        target: "orchestrator.recovery",
                        repo = %repo.as_str(),
                        issue = %issue.as_str(),
                        "tracker inbox closed before recovery could queue fresh issue",
                    );
                } else {
                    info!(
                        target: "orchestrator.recovery",
                        repo = %repo.as_str(),
                        issue = %issue.as_str(),
                        decision = "FreshQueued",
                        "recovery queued fresh active issue",
                    );
                }
            }
            RecoveryDecision::OrphanedWorkspace { repo, issue } => {
                let path = path_lookup
                    .get(&(repo.as_str().to_string(), issue.as_str().to_string()))
                    .cloned();
                warn!(
                    target: "orchestrator.recovery",
                    repo = %repo.as_str(),
                    issue = %issue.as_str(),
                    decision = "OrphanedWorkspace",
                    workspace_path = ?path,
                    "recovery found workspace without matching active Linear issue; retained without deletion",
                );
            }
            RecoveryDecision::NoOp { .. } => {
                // reconcile_decisions never emits this; if a future caller
                // hand-rolls a decision list with NoOp entries, treat them
                // as the documented no-op (Requirement 10.4).
            }
        }
    }

    Ok(decisions)
}

/// Build a synthetic active-state [`NormalizedIssue`] for `(repo, issue)`.
///
/// Prefers the issue payload supplied by the Linear reader when present so
/// the orchestrator sees the freshest title / labels. Falls back to a
/// minimal envelope when the reader returned no entry for the key (which
/// can only happen on `ResumeActive` if the inputs are mismatched — by
/// construction the resume case has a Linear entry, but the fallback keeps
/// recovery deterministic regardless).
fn synthetic_active_event(
    repo: &RepoId,
    issue: &IssueId,
    active_linear: &HashMap<(RepoId, IssueId), NormalizedIssue>,
) -> NormalizedIssue {
    if let Some(existing) = active_linear.get(&(repo.clone(), issue.clone())) {
        // Force the state field to `Active` so the orchestrator drives the
        // `Discovered -> Queued -> Active` path even if the reader payload
        // happened to carry a different bucket.
        return NormalizedIssue {
            state: TrackerIssueState::Active,
            ..existing.clone()
        };
    }
    NormalizedIssue {
        repo: repo.clone(),
        issue: issue.clone(),
        title: String::new(),
        description: String::new(),
        state: TrackerIssueState::Active,
        labels: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure decision function.
    //!
    //! Side-effect-laden recovery (workspace listing, synthetic event
    //! emission, structured logs) is exercised by the integration test in
    //! `tests/orchestrator_recovery.rs`.

    use super::*;

    fn key(repo: &str, issue: &str) -> (RepoId, IssueId) {
        (RepoId::new(repo), IssueId::new(issue))
    }

    fn linear_entry(repo: &str, issue: &str) -> ((RepoId, IssueId), NormalizedIssue) {
        let k = key(repo, issue);
        let v = NormalizedIssue {
            repo: k.0.clone(),
            issue: k.1.clone(),
            title: format!("{repo}:{issue}"),
            description: String::new(),
            state: TrackerIssueState::Active,
            labels: Vec::new(),
        };
        (k, v)
    }

    #[test]
    fn workspace_plus_active_linear_resumes_active() {
        let workspaces = vec![key("repo-a", "ENG-1")];
        let mut linear = HashMap::new();
        let (k, v) = linear_entry("repo-a", "ENG-1");
        linear.insert(k, v);

        let decisions = reconcile_decisions(&workspaces, &linear);

        assert_eq!(decisions.len(), 1);
        assert!(matches!(
            decisions[0],
            RecoveryDecision::ResumeActive { .. }
        ));
    }

    #[test]
    fn workspace_without_linear_active_is_orphaned() {
        let workspaces = vec![key("repo-a", "ENG-2")];
        let linear = HashMap::new();

        let decisions = reconcile_decisions(&workspaces, &linear);

        assert_eq!(decisions.len(), 1);
        assert!(matches!(
            decisions[0],
            RecoveryDecision::OrphanedWorkspace { .. }
        ));
    }

    #[test]
    fn linear_active_without_workspace_is_fresh_queued() {
        let workspaces: Vec<(RepoId, IssueId)> = Vec::new();
        let mut linear = HashMap::new();
        let (k, v) = linear_entry("repo-a", "ENG-3");
        linear.insert(k, v);

        let decisions = reconcile_decisions(&workspaces, &linear);

        assert_eq!(decisions.len(), 1);
        assert!(matches!(decisions[0], RecoveryDecision::FreshQueued { .. }));
    }

    #[test]
    fn absent_on_both_sides_is_omitted() {
        // The "absent on both sides" rule is satisfied by the union semantics
        // of the input sets: nothing in either list means nothing in the
        // output. We assert by feeding the function two empty inputs.
        let workspaces: Vec<(RepoId, IssueId)> = Vec::new();
        let linear = HashMap::new();

        let decisions = reconcile_decisions(&workspaces, &linear);

        assert!(decisions.is_empty());
    }

    #[test]
    fn mixed_inputs_produce_one_decision_per_key_in_documented_order() {
        // Pre-seed the matrix's three live cells across two repos and ensure
        // the decisions are stable-ordered by (repo, issue) lexicographically
        // and that each key appears exactly once.
        let workspaces = vec![
            key("repo-a", "ENG-1"), // also active in linear -> ResumeActive
            key("repo-a", "ENG-2"), // not active in linear -> Orphaned
        ];
        let mut linear = HashMap::new();
        let (k1, v1) = linear_entry("repo-a", "ENG-1");
        linear.insert(k1, v1);
        let (k3, v3) = linear_entry("repo-a", "ENG-3"); // no workspace -> FreshQueued
        linear.insert(k3, v3);

        let decisions = reconcile_decisions(&workspaces, &linear);

        assert_eq!(decisions.len(), 3);
        // Lexicographic order on (repo, issue): ENG-1, ENG-2, ENG-3.
        assert_eq!(
            decisions[0].key(),
            (&RepoId::new("repo-a"), &IssueId::new("ENG-1"))
        );
        assert!(matches!(
            decisions[0],
            RecoveryDecision::ResumeActive { .. }
        ));
        assert_eq!(
            decisions[1].key(),
            (&RepoId::new("repo-a"), &IssueId::new("ENG-2"))
        );
        assert!(matches!(
            decisions[1],
            RecoveryDecision::OrphanedWorkspace { .. }
        ));
        assert_eq!(
            decisions[2].key(),
            (&RepoId::new("repo-a"), &IssueId::new("ENG-3"))
        );
        assert!(matches!(decisions[2], RecoveryDecision::FreshQueued { .. }));
    }

    #[test]
    fn duplicate_workspace_entries_collapse_to_single_decision() {
        // The workspace adapter's `list_existing` already deduplicates, but
        // the decision function must not double-count if a future adapter
        // surfaces duplicates.
        let workspaces = vec![key("repo-a", "ENG-1"), key("repo-a", "ENG-1")];
        let linear = HashMap::new();

        let decisions = reconcile_decisions(&workspaces, &linear);

        assert_eq!(decisions.len(), 1);
        assert!(matches!(
            decisions[0],
            RecoveryDecision::OrphanedWorkspace { .. }
        ));
    }
}
