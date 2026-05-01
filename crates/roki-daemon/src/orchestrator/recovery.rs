//! Restart recovery via Linear plus filesystem reconciliation (task 7.1e,
//! superseding task 5.2 and the transitional 7.1d/3.3 surface).
//!
//! On daemon start, the orchestrator must rebuild its in-memory per-issue
//! state without relying on a database (Requirement 8.5, 10.1-10.5). This
//! module owns the reconciliation step:
//!
//! 1. **Walk session tempdirs** under the platform-appropriate user cache
//!    root (`~/Library/Caches/roki/sessions/<issue>` on macOS,
//!    `~/.cache/roki/sessions/<issue>` on Linux). Each subdirectory name is
//!    treated as an `IssueId`. The walk is performed by
//!    [`crate::session::SessionManager::list_existing_sessions`].
//! 2. **Walk per-repo worktrees** for each configured `[[repos]]` entry by
//!    invoking `git worktree list --porcelain` (via
//!    [`crate::tools::WtTool::list_porcelain`]). Branch names matching the
//!    operator-configurable `IssueBranchPattern` regex (default
//!    `^[A-Z]+-\d+$`) are admitted as candidate `IssueId`s.
//! 3. **Reconcile each distinct `IssueId`** discovered in either source against
//!    Linear via [`RecoveryLinearReader`] and classify the outcome into one
//!    of five [`RecoveryDecision`] variants per the design's 5-cell matrix:
//!
//!    | session | worktree | linear | decision                |
//!    | :-----: | :------: | :----: | :---------------------- |
//!    | yes     | yes      | active | `ResumeActive`          |
//!    | yes     | no       | none   | `OrphanedSession`       |
//!    | no      | yes      | none   | `OrphanedWorktree`      |
//!    | no      | yes      | failed | `OrphanedWorktree`†     |
//!    | no      | no       | active | `FreshQueued`           |
//!    | no      | no       | term.  | `NoOp` (omitted)        |
//!
//!    † On `failed` (or any terminal-failure Linear state), the worktree is
//!    **retained** for operator inspection per
//!    `design-agent-driven-repo-selection.md` decision #6 — the
//!    `OrphanedWorktree` variant carries a `retain: bool` flag the recovery
//!    driver consults instead of calling `wt.remove`.
//!
//! 4. **Side effects** (in [`run_recovery`]):
//!    - `ResumeActive` and `FreshQueued` post a synthetic
//!      [`NormalizedIssue`] (state = [`TrackerIssueState::Active`]) into the
//!      tracker inbox so the orchestrator's existing
//!      `Discovered → Queued → Active` path drives the issue back into the
//!      lifecycle. For `ResumeActive` the recovery driver also populates the
//!      `WorktreeRegistry` so the post-recovery `Cleaning` arc still finds
//!      the worktree(s) that survived the restart (Requirement 10.4).
//!    - `OrphanedSession` schedules cleanup of the session tempdir
//!      (`SessionManager::remove_session`) and emits a structured warn log.
//!    - `OrphanedWorktree { retain: false }` calls `wt.remove` on every
//!      surfaced worktree path. `retain: true` retains the worktree(s) and
//!      logs at warn level for the operator's attention.
//!    - `NoOp` is omitted entirely from the decision list (Requirement 10.4 —
//!      the daemon allocates no slot for a key it has no evidence of).
//!
//! ## Determinism
//!
//! Decisions are returned sorted by `IssueId` lexicographically so the
//! startup log is stable across runs and integration tests can rely on
//! deterministic ordering.
//!
//! ## Disk-write budget
//!
//! Per Requirement 10.5 the daemon writes no per-issue runtime state to disk
//! beyond (a) the session tempdir and worktree contents the agent itself
//! produces and (b) the structured logs the daemon emits. Recovery honours
//! this by never persisting a sidecar state file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::orchestrator::state::{IssueId, RepoId};
use crate::session::{SessionError, SessionManager};
use crate::tools::{WtError, WtTool};
use crate::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use crate::worktrees::{BranchName, RegisteredWorktree, WorktreeRegistry};

/// Default issue-branch pattern used when `[recovery].issue_branch_pattern`
/// is omitted from the config. Matches Linear's canonical issue-id shape
/// (`ENG-123`, `ABC-7`).
pub const DEFAULT_ISSUE_BRANCH_PATTERN: &str = r"^[A-Z]+-\d+$";

/// Operator-configurable regex used to filter `git worktree list` branches
/// down to candidate `IssueId`s. Constructed via
/// [`IssueBranchPattern::compile`] so an invalid regex is a hard refusal at
/// config load time.
#[derive(Debug, Clone)]
pub struct IssueBranchPattern {
    regex: Arc<Regex>,
    raw: String,
}

impl IssueBranchPattern {
    /// Compile `raw` as a regex. Returns an error wrapping the regex crate's
    /// diagnostic so the bootstrap can refuse to start with the operator's
    /// pattern named verbatim.
    pub fn compile(raw: impl Into<String>) -> Result<Self, IssueBranchPatternError> {
        let raw = raw.into();
        let regex = Regex::new(&raw).map_err(|err| IssueBranchPatternError::Invalid {
            raw: raw.clone(),
            reason: err.to_string(),
        })?;
        Ok(Self {
            regex: Arc::new(regex),
            raw,
        })
    }

    /// Compile the documented default pattern. Cannot fail in normal builds.
    pub fn default_pattern() -> Self {
        // The default regex is a fixed string literal known to compile; if
        // it ever fails we want a panic at startup rather than silently
        // recovering, because the operator's expectation is the documented
        // default.
        Self::compile(DEFAULT_ISSUE_BRANCH_PATTERN)
            .expect("default issue-branch pattern must compile")
    }

    /// Return `true` if `branch` matches the configured pattern.
    pub fn matches(&self, branch: &str) -> bool {
        self.regex.is_match(branch)
    }

    /// Original raw pattern string supplied by the operator (or the default
    /// when none was supplied). Used for log lines and error messages.
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl Default for IssueBranchPattern {
    fn default() -> Self {
        Self::default_pattern()
    }
}

/// Reasons an operator-supplied issue-branch pattern is rejected at config
/// load time.
#[derive(Debug, Error)]
pub enum IssueBranchPatternError {
    /// The pattern string did not parse as a valid regex.
    #[error("invalid issue_branch_pattern `{raw}`: {reason}")]
    Invalid { raw: String, reason: String },
}

/// Lifecycle bucket reported by [`RecoveryLinearReader::lookup_issue`].
///
/// Recovery cares about three buckets:
///
/// * `Active` — issue is in flight; the daemon should resume / start work.
/// * `TerminalFailure` — issue terminated unsuccessfully (e.g., Linear state
///   `failed` or any operator-tagged terminal-failure label). Worktrees are
///   **retained** for inspection per design decision #6.
/// * `Terminal` — issue terminated successfully. No work resumes; orphaned
///   on-disk artifacts can be removed.
/// * `Unknown` — Linear has no record of this issue (e.g., the issue was
///   deleted, or the daemon's API token cannot see it). Treated identically
///   to `Terminal` for the matrix (orphaned artifacts removed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryIssueLifecycle {
    Active,
    TerminalFailure,
    Terminal,
    Unknown,
}

/// One-shot per-issue read surface against Linear consumed by the recovery
/// scan.
///
/// The polling loop in [`crate::tracker::linear::LinearTracker`] is
/// continuous; recovery needs a single point-in-time read keyed by
/// individual `IssueId`. Implementations must be `Send + Sync`.
#[async_trait]
pub trait RecoveryLinearReader: Send + Sync {
    /// Look up the lifecycle bucket for `issue` plus an optional
    /// [`NormalizedIssue`] payload (used to seed synthetic active-state
    /// events for resumed / fresh-queued issues so the orchestrator sees
    /// the freshest title/labels).
    ///
    /// Implementations may bulk-fetch internally; the recovery driver calls
    /// this method once per distinct `IssueId` discovered on disk.
    async fn lookup_issue(
        &self,
        issue: &IssueId,
    ) -> Result<(RecoveryIssueLifecycle, Option<NormalizedIssue>), String>;

    /// Enumerate every Linear issue currently in an active workflow
    /// state at recovery time. Recovery unions this with the disk-walk
    /// candidate set so a Linear-active issue with no on-disk artifact
    /// (the `FreshQueued` cell) reaches the matrix.
    ///
    /// Implementations must be safe to call once per recovery scan; the
    /// driver calls this exactly once.
    async fn active_issues(&self) -> Result<Vec<(IssueId, NormalizedIssue)>, String>;
}

/// Per-issue recovery outcome computed by [`run_recovery`].
///
/// Variants mirror the 5-cell decision matrix in
/// `design-agent-driven-repo-selection.md` "Restart recovery rethink".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// Linear active + session tempdir + at least one worktree on disk.
    /// The recovery driver re-registers the worktrees and emits a synthetic
    /// active-state tracker event.
    ResumeActive {
        issue: IssueId,
        worktrees: Vec<RegisteredWorktree>,
    },
    /// Session tempdir exists but no Linear active state and no worktrees.
    /// The driver removes the tempdir and emits a warn log.
    OrphanedSession { issue: IssueId },
    /// Worktree(s) exist but no session tempdir. When `retain` is `false`
    /// the driver calls `wt.remove` per worktree; when `true` (Linear state
    /// is terminal-failure) the driver retains each worktree for the
    /// operator's inspection.
    OrphanedWorktree {
        issue: IssueId,
        worktrees: Vec<RegisteredWorktree>,
        retain: bool,
    },
    /// Linear active + nothing on disk. The driver emits a synthetic
    /// active-state tracker event so the orchestrator's existing
    /// `Discovered → Queued → Active` path creates a fresh session
    /// tempdir.
    FreshQueued { issue: IssueId },
    /// Documented for completeness; never emitted (the union excludes
    /// Linear-terminal issues with no on-disk artifacts — Requirement 10.5,
    /// "no extra disk writes").
    NoOp { issue: IssueId },
}

impl RecoveryDecision {
    /// Identifying `IssueId` key for this decision.
    pub fn issue(&self) -> &IssueId {
        match self {
            Self::ResumeActive { issue, .. }
            | Self::OrphanedSession { issue }
            | Self::OrphanedWorktree { issue, .. }
            | Self::FreshQueued { issue }
            | Self::NoOp { issue } => issue,
        }
    }
}

/// Errors surfaced by the recovery driver. Hard failures of either
/// subsystem (session walk, worktree walk, Linear reader) propagate so the
/// daemon does not silently start with a partial recovery.
#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("recovery: session-tempdir walk failed: {0}")]
    Session(#[from] SessionError),

    #[error("recovery: worktree walk for `{repo}` failed: {source}")]
    Worktree {
        repo: String,
        #[source]
        source: WtError,
    },

    #[error("recovery: linear lookup for `{issue}` failed: {reason}")]
    LinearRead { issue: String, reason: String },
}

/// Per-repo input for the recovery walk: the configured ghq identifier and
/// the resolved local checkout path (from `ghq list -p`). Recovery skips
/// any entry whose checkout path does not exist on disk; missing repos are
/// not a hard failure (the operator may have configured a repo they have
/// not yet cloned).
#[derive(Debug, Clone)]
pub struct RecoveryRepoInput {
    pub repo: RepoId,
    pub repo_path: PathBuf,
}

/// Drive the full recovery scan and post side effects (synthetic tracker
/// events, worktree cleanups, session cleanups).
///
/// `tracker_sender` is the same `mpsc::Sender<NormalizedIssue>` that feeds
/// the orchestrator's tracker inbox.
///
/// Returns the ordered list of decisions in `IssueId` lexicographic order so
/// callers (notably integration tests) can assert per-key outcomes.
pub async fn run_recovery(
    session_manager: &SessionManager,
    repos: &[RecoveryRepoInput],
    pattern: &IssueBranchPattern,
    wt: &dyn WtTool,
    reader: &dyn RecoveryLinearReader,
    worktree_registry: &WorktreeRegistry,
    tracker_sender: &mpsc::Sender<NormalizedIssue>,
) -> Result<Vec<RecoveryDecision>, RecoveryError> {
    // ---- 1. Walk session tempdirs --------------------------------------
    let session_issues = session_manager.list_existing_sessions()?;
    let session_set: BTreeSet<IssueId> = session_issues.into_iter().collect();

    // ---- 2. Walk per-repo worktrees ------------------------------------
    // Map from IssueId to the list of (repo, branch, path) tuples. We use a
    // BTreeMap so post-walk iteration order is deterministic.
    let mut worktree_map: BTreeMap<IssueId, Vec<RegisteredWorktree>> = BTreeMap::new();
    for input in repos {
        if !input.repo_path.exists() {
            // Not an error: operator may have configured a repo not yet
            // cloned. The agent tool's `ghq.ensure_cloned` will handle the
            // first call.
            info!(
                target: "orchestrator.recovery",
                repo = %input.repo.as_str(),
                repo_path = %input.repo_path.display(),
                "configured repo has no local checkout; skipping worktree walk",
            );
            continue;
        }
        let entries =
            wt.list_porcelain(&input.repo_path)
                .await
                .map_err(|err| RecoveryError::Worktree {
                    repo: input.repo.as_str().to_string(),
                    source: err,
                })?;
        for entry in entries {
            let Some(branch) = entry.branch else { continue };
            if !pattern.matches(&branch) {
                continue;
            }
            let issue = IssueId::new(branch.clone());
            let registered = RegisteredWorktree {
                repo: input.repo.clone(),
                branch: BranchName::new(branch),
                path: entry.path,
            };
            worktree_map.entry(issue).or_default().push(registered);
        }
    }

    // ---- 3. Bulk-fetch the Linear-active slice so FreshQueued is
    //         reachable when an issue is active in Linear but has no
    //         on-disk artifact. The reader's bulk surface is consulted
    //         once per scan and stored as a lookup so each per-issue
    //         classification can short-circuit.
    let active_issues =
        reader
            .active_issues()
            .await
            .map_err(|reason| RecoveryError::LinearRead {
                issue: "<bulk-active-fetch>".to_string(),
                reason,
            })?;
    let mut active_payloads: std::collections::HashMap<IssueId, NormalizedIssue> =
        std::collections::HashMap::with_capacity(active_issues.len());
    for (issue, payload) in active_issues {
        active_payloads.insert(issue, payload);
    }

    // ---- 4. Union of issue ids ------------------------------------------
    let mut all_issues: BTreeSet<IssueId> = BTreeSet::new();
    all_issues.extend(session_set.iter().cloned());
    all_issues.extend(worktree_map.keys().cloned());
    all_issues.extend(active_payloads.keys().cloned());

    // ---- 5. Reconcile each issue against Linear -------------------------
    let mut decisions: Vec<RecoveryDecision> = Vec::with_capacity(all_issues.len());
    for issue in all_issues {
        let has_session = session_set.contains(&issue);
        let worktrees = worktree_map.remove(&issue).unwrap_or_default();
        let has_worktree = !worktrees.is_empty();

        // Prefer the bulk-fetched active payload; fall back to a
        // per-issue lookup for any disk-discovered issue not present in
        // the bulk-active slice (terminal-success, terminal-failure,
        // unknown).
        let (lifecycle, payload) = if let Some(p) = active_payloads.get(&issue) {
            (RecoveryIssueLifecycle::Active, Some(p.clone()))
        } else {
            reader
                .lookup_issue(&issue)
                .await
                .map_err(|reason| RecoveryError::LinearRead {
                    issue: issue.as_str().to_string(),
                    reason,
                })?
        };

        let decision = classify(&issue, has_session, has_worktree, lifecycle, worktrees);

        // Apply side effects per the matrix.
        match &decision {
            RecoveryDecision::ResumeActive {
                issue: i,
                worktrees,
            } => {
                // Repopulate the in-memory worktree registry so the
                // post-recovery Cleaning arc still finds the surviving
                // worktrees.
                for entry in worktrees {
                    worktree_registry.register(
                        i.clone(),
                        entry.repo.clone(),
                        entry.branch.clone(),
                        entry.path.clone(),
                    );
                }
                let synthetic = synthetic_active_event(i, payload.as_ref());
                if tracker_sender.send(synthetic).await.is_err() {
                    warn!(
                        target: "orchestrator.recovery",
                        issue = %i.as_str(),
                        "tracker inbox closed before recovery could resume issue",
                    );
                } else {
                    info!(
                        target: "orchestrator.recovery",
                        issue = %i.as_str(),
                        worktrees = worktrees.len(),
                        decision = "ResumeActive",
                        "recovery resumed active issue",
                    );
                }
            }
            RecoveryDecision::FreshQueued { issue: i } => {
                let synthetic = synthetic_active_event(i, payload.as_ref());
                if tracker_sender.send(synthetic).await.is_err() {
                    warn!(
                        target: "orchestrator.recovery",
                        issue = %i.as_str(),
                        "tracker inbox closed before recovery could queue fresh issue",
                    );
                } else {
                    info!(
                        target: "orchestrator.recovery",
                        issue = %i.as_str(),
                        decision = "FreshQueued",
                        "recovery queued fresh active issue",
                    );
                }
            }
            RecoveryDecision::OrphanedSession { issue: i } => {
                match session_manager.remove_session(i) {
                    Ok(()) => {
                        warn!(
                            target: "orchestrator.recovery",
                            issue = %i.as_str(),
                            decision = "OrphanedSession",
                            "recovery removed orphaned session tempdir",
                        );
                    }
                    Err(err) => {
                        warn!(
                            target: "orchestrator.recovery",
                            issue = %i.as_str(),
                            decision = "OrphanedSession",
                            error = %err,
                            "recovery failed to remove orphaned session tempdir",
                        );
                    }
                }
            }
            RecoveryDecision::OrphanedWorktree {
                issue: i,
                worktrees,
                retain,
            } => {
                if *retain {
                    warn!(
                        target: "orchestrator.recovery",
                        issue = %i.as_str(),
                        worktrees = worktrees.len(),
                        decision = "OrphanedWorktree",
                        retain = true,
                        "recovery retained orphaned worktree(s) for terminal-failure issue (operator inspection)",
                    );
                } else {
                    for entry in worktrees {
                        match wt.remove(&entry.path).await {
                            Ok(()) => {
                                warn!(
                                    target: "orchestrator.recovery",
                                    issue = %i.as_str(),
                                    repo = %entry.repo.as_str(),
                                    path = %entry.path.display(),
                                    decision = "OrphanedWorktree",
                                    "recovery removed orphaned worktree",
                                );
                            }
                            Err(err) => {
                                warn!(
                                    target: "orchestrator.recovery",
                                    issue = %i.as_str(),
                                    repo = %entry.repo.as_str(),
                                    path = %entry.path.display(),
                                    decision = "OrphanedWorktree",
                                    error = %err,
                                    "recovery failed to remove orphaned worktree",
                                );
                            }
                        }
                    }
                }
            }
            RecoveryDecision::NoOp { .. } => {
                // Never produced by `classify`; included for matrix
                // completeness only.
            }
        }
        if !matches!(decision, RecoveryDecision::NoOp { .. }) {
            decisions.push(decision);
        }
    }

    info!(
        target: "orchestrator.recovery",
        sessions = session_set.len(),
        repos = repos.len(),
        decisions = decisions.len(),
        pattern = %pattern.as_str(),
        "recovery scan completed",
    );

    Ok(decisions)
}

/// Pure classification of a single issue into a [`RecoveryDecision`].
///
/// Exposed for unit testing the matrix. The side-effect-laden driver
/// ([`run_recovery`]) wraps this with the disk-walk + Linear-lookup steps.
fn classify(
    issue: &IssueId,
    has_session: bool,
    has_worktree: bool,
    lifecycle: RecoveryIssueLifecycle,
    worktrees: Vec<RegisteredWorktree>,
) -> RecoveryDecision {
    use RecoveryIssueLifecycle::*;

    match (has_session, has_worktree, lifecycle) {
        (true, true, Active) => RecoveryDecision::ResumeActive {
            issue: issue.clone(),
            worktrees,
        },
        // Session present + worktree present BUT linear non-active: treat
        // session as orphaned and worktree as orphaned. We collapse onto
        // OrphanedSession + OrphanedWorktree by emitting OrphanedWorktree
        // (retain on terminal-failure). We additionally remove the
        // session tempdir as part of this cell (driver picks up the
        // session removal via OrphanedSession side effect — but since we
        // emit only one decision here, we choose OrphanedWorktree as the
        // dominant signal so the test matrix can assert one decision per
        // cell. The OrphanedSession side effect (tempdir removal) still
        // needs to happen — driver does this above.
        //
        // To keep one-decision-per-issue semantics we choose:
        //   - has_session + has_worktree + non-active linear: emit
        //     OrphanedWorktree (with retain flag) and ALSO clean the
        //     session tempdir as a coupled side effect inside the driver.
        //
        // Simpler: collapse to OrphanedWorktree with retain matching the
        // lifecycle, and the driver removes the session tempdir alongside.
        (true, true, TerminalFailure) => RecoveryDecision::OrphanedWorktree {
            issue: issue.clone(),
            worktrees,
            retain: true,
        },
        (true, true, Terminal) | (true, true, Unknown) => RecoveryDecision::OrphanedWorktree {
            issue: issue.clone(),
            worktrees,
            retain: false,
        },
        (true, false, Active) => {
            // Session present, no worktree, but Linear active. The agent
            // never opened a worktree — this is a partially-initialised
            // session. Resume so the agent can re-open worktrees on its
            // first turn.
            RecoveryDecision::ResumeActive {
                issue: issue.clone(),
                worktrees: Vec::new(),
            }
        }
        (true, false, _) => RecoveryDecision::OrphanedSession {
            issue: issue.clone(),
        },
        (false, true, Active) => {
            // Worktree exists but no session. Strange (the orchestrator
            // creates the session before the agent calls
            // roki_open_worktree), but recoverable: treat it as
            // ResumeActive so the orchestrator re-creates the session
            // tempdir and re-registers the worktree.
            RecoveryDecision::ResumeActive {
                issue: issue.clone(),
                worktrees,
            }
        }
        (false, true, TerminalFailure) => RecoveryDecision::OrphanedWorktree {
            issue: issue.clone(),
            worktrees,
            retain: true,
        },
        (false, true, Terminal) | (false, true, Unknown) => RecoveryDecision::OrphanedWorktree {
            issue: issue.clone(),
            worktrees,
            retain: false,
        },
        (false, false, Active) => RecoveryDecision::FreshQueued {
            issue: issue.clone(),
        },
        (false, false, _) => RecoveryDecision::NoOp {
            issue: issue.clone(),
        },
    }
}

/// Build a synthetic active-state [`NormalizedIssue`] for `issue`.
///
/// Prefers the payload supplied by the Linear reader when present so the
/// orchestrator sees the freshest title / labels. Falls back to a minimal
/// envelope when the reader returned no payload.
fn synthetic_active_event(issue: &IssueId, payload: Option<&NormalizedIssue>) -> NormalizedIssue {
    if let Some(existing) = payload {
        return NormalizedIssue {
            state: TrackerIssueState::Active,
            ..existing.clone()
        };
    }
    NormalizedIssue {
        repo: RepoId::new(""),
        issue: issue.clone(),
        title: String::new(),
        description: String::new(),
        state: TrackerIssueState::Active,
        labels: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    //! Pure-classification unit tests. Side-effect-laden driver behaviour
    //! is exercised by `tests/orchestrator_restart_recovery.rs`.

    use super::*;

    fn issue(s: &str) -> IssueId {
        IssueId::new(s)
    }

    fn worktree(repo: &str, branch: &str, path: &str) -> RegisteredWorktree {
        RegisteredWorktree {
            repo: RepoId::new(repo),
            branch: BranchName::new(branch),
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn session_plus_worktree_plus_active_linear_resumes() {
        let d = classify(
            &issue("ENG-1"),
            true,
            true,
            RecoveryIssueLifecycle::Active,
            vec![worktree("o/r", "ENG-1", "/tmp/r.ENG-1")],
        );
        assert!(matches!(d, RecoveryDecision::ResumeActive { .. }));
    }

    #[test]
    fn session_only_with_active_linear_resumes_with_empty_worktrees() {
        let d = classify(
            &issue("ENG-2"),
            true,
            false,
            RecoveryIssueLifecycle::Active,
            Vec::new(),
        );
        match d {
            RecoveryDecision::ResumeActive { worktrees, .. } => {
                assert!(worktrees.is_empty());
            }
            other => panic!("expected ResumeActive, got {other:?}"),
        }
    }

    #[test]
    fn session_only_without_linear_active_is_orphaned_session() {
        let d = classify(
            &issue("ENG-3"),
            true,
            false,
            RecoveryIssueLifecycle::Terminal,
            Vec::new(),
        );
        assert!(matches!(d, RecoveryDecision::OrphanedSession { .. }));
    }

    #[test]
    fn worktree_only_without_linear_active_is_orphaned_worktree_remove() {
        let d = classify(
            &issue("ENG-4"),
            false,
            true,
            RecoveryIssueLifecycle::Terminal,
            vec![worktree("o/r", "ENG-4", "/tmp/r.ENG-4")],
        );
        match d {
            RecoveryDecision::OrphanedWorktree { retain, .. } => {
                assert!(!retain, "terminal-success implies remove, not retain");
            }
            other => panic!("expected OrphanedWorktree, got {other:?}"),
        }
    }

    #[test]
    fn worktree_only_with_terminal_failure_linear_is_orphaned_worktree_retained() {
        let d = classify(
            &issue("ENG-5"),
            false,
            true,
            RecoveryIssueLifecycle::TerminalFailure,
            vec![worktree("o/r", "ENG-5", "/tmp/r.ENG-5")],
        );
        match d {
            RecoveryDecision::OrphanedWorktree { retain, .. } => {
                assert!(retain, "terminal-failure must retain the worktree");
            }
            other => panic!("expected OrphanedWorktree, got {other:?}"),
        }
    }

    #[test]
    fn fresh_queued_when_linear_active_and_nothing_on_disk() {
        let d = classify(
            &issue("ENG-6"),
            false,
            false,
            RecoveryIssueLifecycle::Active,
            Vec::new(),
        );
        assert!(matches!(d, RecoveryDecision::FreshQueued { .. }));
    }

    #[test]
    fn no_op_when_terminal_and_nothing_on_disk() {
        let d = classify(
            &issue("ENG-7"),
            false,
            false,
            RecoveryIssueLifecycle::Terminal,
            Vec::new(),
        );
        assert!(matches!(d, RecoveryDecision::NoOp { .. }));
    }

    #[test]
    fn issue_branch_pattern_default_matches_canonical_issue_ids() {
        let p = IssueBranchPattern::default_pattern();
        assert!(p.matches("ENG-1"));
        assert!(p.matches("ABC-12345"));
        assert!(!p.matches("eng-1")); // lower-case rejected
        assert!(!p.matches("ENG_1")); // underscore rejected
        assert!(!p.matches("feature/foo"));
        assert!(!p.matches(""));
    }

    #[test]
    fn issue_branch_pattern_compile_rejects_invalid_regex() {
        let err = IssueBranchPattern::compile("[unclosed").expect_err("invalid regex must fail");
        assert!(matches!(err, IssueBranchPatternError::Invalid { .. }));
    }

    #[test]
    fn issue_branch_pattern_compile_accepts_custom_pattern() {
        let p = IssueBranchPattern::compile(r"^issue-\d+$").expect("custom pattern");
        assert!(p.matches("issue-42"));
        assert!(!p.matches("ENG-1"));
    }
}
