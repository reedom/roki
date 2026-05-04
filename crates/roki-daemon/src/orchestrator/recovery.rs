//! Restart-time per-issue recovery: scan + 5-cell decision matrix.
//!
//! On daemon startup `RecoveryReconciler` walks the on-disk session tempdirs
//! and the worktrees registered with each configured `[[repos]]` entry, then
//! cross-references the union of distinct issue identifiers against Linear's
//! current label / state set via the read-only [`LinearClient`] +
//! [`PreAdmissionJudge`]. Each issue maps to exactly one of five cells from
//! design.md "Restart recovery"; orchestrator-internal state (turn count, last
//! action, …) is intentionally NOT persisted across restarts because the
//! orchestrator session itself never survives a daemon restart.
//!
//! Spec refs: requirements.md Req 8.5, 10.1, 10.2, 10.3, 10.4, 10.5;
//! design.md "Restart recovery".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::Regex;
use thiserror::Error;
use tracing::warn;

use crate::config::repos::RepoEntry;
use crate::exec::ghq::{GhqError, GhqTool};
use crate::exec::wt::{WtError, WtTool};
use crate::orchestrator::escalation::EscalationKind;
use crate::orchestrator::state::{IssueId, Mode};
use crate::session::sanitize_issue_id;
use crate::tracker::linear::{LinearClient, LinearError};
use crate::tracker::model::RepoId;
use crate::tracker::pre_admission::{AdmissionDecision, PreAdmissionJudge};

/// Default issue-id pattern: `^[A-Z]+-\d+$` (canonical Linear shape).
pub const DEFAULT_ISSUE_ID_PATTERN: &str = r"^[A-Z]+-\d+$";

/// Errors surfaced by [`RecoveryReconciler`].
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// Configured issue-id regex is malformed; surfaced verbatim from `regex`.
    #[error("invalid issue-id regex: {0}")]
    InvalidRegex(String),

    /// Filesystem operation against the session tempdir root failed.
    #[error("session-root scan failed at `{path}`: {source}")]
    SessionScan {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `ghq list -p` fallthrough.
    #[error(transparent)]
    Ghq(#[from] GhqError),

    /// `wt list --porcelain` fallthrough.
    #[error(transparent)]
    Wt(#[from] WtError),

    /// Read-only Linear lookup fallthrough during decide().
    #[error(transparent)]
    Linear(#[from] LinearError),
}

/// One worktree found on disk during scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredWorktree {
    pub repo_id: RepoId,
    pub path: PathBuf,
    pub branch: String,
}

/// One distinct issue id discovered by [`RecoveryReconciler::scan`], with
/// presence flags spanning the session tempdir + every configured repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredIssue {
    pub issue: IssueId,
    pub session_present: bool,
    pub worktrees: Vec<DiscoveredWorktree>,
}

impl DiscoveredIssue {
    fn empty(issue: IssueId) -> Self {
        Self {
            issue,
            session_present: false,
            worktrees: Vec::new(),
        }
    }
}

/// One of five documented restart-recovery decisions. The orchestrator
/// driver consumes this and either launches a fresh orchestrator session
/// (Pending), enqueues `EscalationKind::Orphan` (Inactive(Orphan)), or skips
/// (NoOp).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// Pre-admission passes AND the session tempdir + ≥1 worktree are on
    /// disk → start fresh orchestrator with the recomputed mode.
    ResumeActive { issue: IssueId, mode: Mode },
    /// Session present but pre-admission failed (or no Linear active state):
    /// retain residue, log, and forward `daemon_directive(orphan)` once a
    /// live orchestrator exists.
    OrphanedSession { issue: IssueId },
    /// Worktree present but pre-admission failed (or no session): same
    /// retention semantics as `OrphanedSession`.
    OrphanedWorktree { issue: IssueId },
    /// Pre-admission passes, nothing on disk → enqueue Pending and launch a
    /// fresh orchestrator (session tempdir created on entry; worktree
    /// materialized on first non-classify phase nomination).
    FreshQueued { issue: IssueId, mode: Mode },
    /// Linear issue is terminal AND nothing on disk: nothing to do.
    NoOp { issue: IssueId },
}

/// Restart-recovery driver. Holds enough state to enumerate the on-disk
/// world (session root + per-repo worktrees) but performs no Linear writes.
#[derive(Debug)]
pub struct RecoveryReconciler<W: WtTool, G: GhqTool> {
    session_root: PathBuf,
    repos: Vec<RepoEntry>,
    wt: Arc<W>,
    ghq: Arc<G>,
    issue_id_regex: Regex,
}

impl<W: WtTool, G: GhqTool> RecoveryReconciler<W, G> {
    /// Construct with the canonical `^[A-Z]+-\d+$` issue-id pattern.
    pub fn new(
        session_root: PathBuf,
        repos: Vec<RepoEntry>,
        wt: Arc<W>,
        ghq: Arc<G>,
    ) -> Result<Self, RecoveryError> {
        Self::with_pattern(session_root, repos, wt, ghq, DEFAULT_ISSUE_ID_PATTERN)
    }

    /// Construct with an operator-supplied pattern (anchored is the caller's
    /// responsibility).
    pub fn with_pattern(
        session_root: PathBuf,
        repos: Vec<RepoEntry>,
        wt: Arc<W>,
        ghq: Arc<G>,
        pattern: &str,
    ) -> Result<Self, RecoveryError> {
        let issue_id_regex = Regex::new(pattern).map_err(|err| {
            RecoveryError::InvalidRegex(err.to_string())
        })?;
        Ok(Self {
            session_root,
            repos,
            wt,
            ghq,
            issue_id_regex,
        })
    }

    /// Enumerate every distinct issue id observable on disk.
    ///
    /// Walks the session tempdir under the configured root, then every
    /// configured `[[repos]]` for git worktrees whose branch matches
    /// `issue_id_regex`. Returns one [`DiscoveredIssue`] per distinct id with
    /// presence flags filled in.
    pub async fn scan(&self) -> Result<Vec<DiscoveredIssue>, RecoveryError> {
        use std::collections::BTreeMap;

        // BTreeMap so the resulting order is deterministic across runs;
        // tests rely on this for stable assertions.
        let mut by_issue: BTreeMap<String, DiscoveredIssue> = BTreeMap::new();

        for raw in self.scan_session_dirs()? {
            let issue = IssueId::from(raw.clone());
            by_issue
                .entry(raw)
                .or_insert_with(|| DiscoveredIssue::empty(issue))
                .session_present = true;
        }

        for entry in &self.repos {
            let repo_id = RepoId(entry.ghq.clone());
            // A repo declared in `[[repos]]` may not yet be cloned locally;
            // treat that as "no worktrees" rather than an error so the scan
            // tolerates partial environments (Req 10.1).
            let Some(repo_path) = self.ghq.list_path(&repo_id.0).await? else {
                continue;
            };
            if !repo_path.exists() {
                continue;
            }
            let entries = self.wt.list_porcelain(&repo_path).await?;
            for wt_entry in entries {
                let Some(branch) = wt_entry.branch.as_ref() else {
                    continue;
                };
                if !self.issue_id_regex.is_match(branch) {
                    continue;
                }
                // Belt-and-braces against a regex that admits a hostile
                // string: discard anything sanitize_issue_id rejects so
                // downstream filesystem ops cannot escape the session root.
                if sanitize_issue_id(branch).is_err() {
                    continue;
                }
                let key = branch.clone();
                let entry = by_issue
                    .entry(key)
                    .or_insert_with(|| DiscoveredIssue::empty(IssueId::from(branch.clone())));
                entry.worktrees.push(DiscoveredWorktree {
                    repo_id: repo_id.clone(),
                    path: wt_entry.path,
                    branch: branch.clone(),
                });
            }
        }

        Ok(by_issue.into_values().collect())
    }

    /// Apply the documented 5-cell decision matrix to a single discovered
    /// issue. Mode is RECOMPUTED from the current Linear label set on every
    /// resumable path so an operator who flipped `roki:impl` between restarts
    /// observes the new mode.
    pub async fn decide(
        &self,
        discovered: DiscoveredIssue,
        linear: &LinearClient,
        judge: &PreAdmissionJudge,
    ) -> Result<RecoveryDecision, RecoveryError> {
        let DiscoveredIssue {
            issue,
            session_present,
            worktrees,
        } = discovered;
        let on_disk = session_present || !worktrees.is_empty();

        // Read-only fetch — the daemon never writes Linear during recovery.
        let normalized = match linear.issue_by_id(&issue.0).await {
            Ok(normalized) => Some(normalized),
            Err(LinearError::Parse(reason)) => {
                // `Parse` covers both "issue node missing" (Linear returned
                // `null`, i.e., the issue was deleted) and malformed
                // responses; either way we have no way to admit. The
                // structured log captures the reason for operator review.
                warn!(
                    target: "orchestrator.recovery",
                    issue = %issue,
                    reason = %reason,
                    "linear issue lookup yielded no usable record"
                );
                None
            }
            Err(other) => return Err(RecoveryError::Linear(other)),
        };

        let admission = normalized.as_ref().map(|n| judge.evaluate(n));

        match (admission, on_disk, session_present, !worktrees.is_empty()) {
            // Pre-admission passes + something on disk → resume.
            (Some(AdmissionDecision::Admit { mode, .. }), true, true, true) => {
                Ok(RecoveryDecision::ResumeActive { issue, mode })
            }
            // Pre-admission passes + nothing on disk → fresh queue.
            (Some(AdmissionDecision::Admit { mode, .. }), false, _, _) => {
                Ok(RecoveryDecision::FreshQueued { issue, mode })
            }
            // Pre-admission passes but only one half of the session/worktree
            // pair is present → orphan; the present half drives the variant.
            (Some(AdmissionDecision::Admit { .. }), true, true, false) => {
                Ok(RecoveryDecision::OrphanedSession { issue })
            }
            (Some(AdmissionDecision::Admit { .. }), true, false, true) => {
                Ok(RecoveryDecision::OrphanedWorktree { issue })
            }
            // Pre-admission fails (skip) or no Linear record but session/
            // worktree exists → orphan.
            (_, true, true, false) => Ok(RecoveryDecision::OrphanedSession { issue }),
            (_, true, false, true) => Ok(RecoveryDecision::OrphanedWorktree { issue }),
            (_, true, true, true) => {
                // Both present but pre-admission failed (or Linear gone):
                // surface as OrphanedSession so the operator sees the older
                // residue first; the structured-fields log carries the full
                // worktree list for cleanup.
                Ok(RecoveryDecision::OrphanedSession { issue })
            }
            // Nothing on disk and Linear says terminal / unknown → noop.
            _ => Ok(RecoveryDecision::NoOp { issue }),
        }
    }

    /// Synthesize the structured-fields blob for a `daemon_directive(orphan)`
    /// that the recovery layer enqueues. Delivery to a live orchestrator is
    /// handled by the orchestrator core's escalation router (Task 4.10).
    pub fn orphan_directive_fields(discovered: &DiscoveredIssue) -> serde_json::Value {
        serde_json::json!({
            "session_present": discovered.session_present,
            "worktree_paths": discovered
                .worktrees
                .iter()
                .map(|w| w.path.display().to_string())
                .collect::<Vec<_>>(),
            "repos": discovered
                .worktrees
                .iter()
                .map(|w| w.repo_id.0.clone())
                .collect::<Vec<_>>(),
        })
    }

    /// The escalation kind orphan decisions enqueue.
    pub const ORPHAN_KIND: EscalationKind = EscalationKind::Orphan;

    fn scan_session_dirs(&self) -> Result<Vec<String>, RecoveryError> {
        let mut out = Vec::new();
        let read = match std::fs::read_dir(&self.session_root) {
            Ok(r) => r,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(err) => {
                return Err(RecoveryError::SessionScan {
                    path: self.session_root.clone(),
                    source: err,
                });
            }
        };
        for entry in read {
            let entry = entry.map_err(|err| RecoveryError::SessionScan {
                path: self.session_root.clone(),
                source: err,
            })?;
            let file_type = entry.file_type().map_err(|err| RecoveryError::SessionScan {
                path: entry.path(),
                source: err,
            })?;
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Refuse anything sanitize_issue_id would reject so downstream
            // path joins remain rooted under `session_root` (Req 10.5).
            if sanitize_issue_id(&name).is_err() {
                continue;
            }
            if !self.issue_id_regex.is_match(&name) {
                continue;
            }
            out.push(name);
        }
        Ok(out)
    }

    /// Operator-visible scan summary helper for log emission.
    pub fn session_root(&self) -> &Path {
        &self.session_root
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretValue;
    use crate::exec::ghq::{MockGhq, seed_mock_repo};
    use crate::exec::wt::{MockWt, WorktreeEntry};
    use crate::tracker::model::{
        LABEL_ROKI_IMPL, LABEL_ROKI_READY, LinearStateName, LinearUserId,
    };
    use std::collections::BTreeSet;
    use std::time::Duration;
    use tempfile::TempDir;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn allowlist(ids: &[&str]) -> Vec<RepoEntry> {
        ids.iter()
            .map(|id| RepoEntry { ghq: (*id).to_owned() })
            .collect()
    }

    fn judge_for(user: &str) -> PreAdmissionJudge {
        PreAdmissionJudge::new(
            LinearUserId::from(user),
            BTreeSet::from([LinearStateName::from("Todo")]),
        )
    }

    /// Build a wiremock backed `LinearClient` that yields a single
    /// `issue_by_id` response.
    async fn linear_with_issue(issue_node: serde_json::Value) -> (MockServer, LinearClient) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "issue": issue_node }
            })))
            .mount(&server)
            .await;
        let client = LinearClient::new(server.uri(), SecretValue::new("tok"))
            .with_backoff_floor(Duration::from_millis(5));
        (server, client)
    }

    fn issue_node(
        id: &str,
        state: &str,
        labels: &[&str],
        assignee: Option<&str>,
    ) -> serde_json::Value {
        let label_nodes: Vec<serde_json::Value> = labels
            .iter()
            .map(|name| serde_json::json!({ "name": name }))
            .collect();
        let assignee = match assignee {
            Some(id) => serde_json::json!({ "id": id }),
            None => serde_json::Value::Null,
        };
        serde_json::json!({
            "identifier": id,
            "title": "title",
            "description": "body",
            "state": { "name": state },
            "labels": { "nodes": label_nodes },
            "assignee": assignee,
        })
    }

    /// Build a reconciler with a tempdir-rooted session root and one configured
    /// `[[repos]]` entry. Returns the reconciler plus the pre-resolved repo
    /// path for follow-up worktree seeding.
    fn make_reconciler(
        tmp: &TempDir,
        wt: Arc<MockWt>,
        ghq: Arc<MockGhq>,
    ) -> (RecoveryReconciler<MockWt, MockGhq>, PathBuf) {
        let repo_path = seed_mock_repo(&ghq, tmp.path(), "github.com/owner/repo");
        let session_root = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_root).unwrap();
        let reconciler = RecoveryReconciler::new(
            session_root,
            allowlist(&["github.com/owner/repo"]),
            wt,
            ghq,
        )
        .unwrap();
        (reconciler, repo_path)
    }

    fn touch_session(reconciler: &RecoveryReconciler<MockWt, MockGhq>, issue: &str) {
        std::fs::create_dir_all(reconciler.session_root().join(issue)).unwrap();
    }

    fn seed_worktree(
        wt: &MockWt,
        repo_path: &Path,
        worktree_path: &Path,
        branch: &str,
    ) {
        std::fs::create_dir_all(worktree_path).unwrap();
        wt.seed_list(
            repo_path,
            vec![WorktreeEntry {
                path: worktree_path.to_path_buf(),
                branch: Some(branch.to_owned()),
            }],
        );
    }

    // ----- 9.1 scan tests -----

    #[tokio::test]
    async fn scan_finds_session_only_issue() {
        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, _repo_path) = make_reconciler(&tmp, wt.clone(), ghq.clone());

        touch_session(&reconciler, "ENG-1");

        let found = reconciler.scan().await.unwrap();
        assert_eq!(found.len(), 1);
        let only = &found[0];
        assert_eq!(only.issue, IssueId::from("ENG-1"));
        assert!(only.session_present);
        assert!(only.worktrees.is_empty());
    }

    #[tokio::test]
    async fn scan_finds_worktree_only_issue() {
        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, repo_path) = make_reconciler(&tmp, wt.clone(), ghq.clone());

        let worktree_path = tmp.path().join("repo.ENG-2");
        seed_worktree(&wt, &repo_path, &worktree_path, "ENG-2");

        let found = reconciler.scan().await.unwrap();
        assert_eq!(found.len(), 1);
        let only = &found[0];
        assert_eq!(only.issue, IssueId::from("ENG-2"));
        assert!(!only.session_present);
        assert_eq!(only.worktrees.len(), 1);
        assert_eq!(only.worktrees[0].branch, "ENG-2");
        assert_eq!(only.worktrees[0].path, worktree_path);
    }

    #[tokio::test]
    async fn scan_finds_both_session_and_worktree_for_same_issue() {
        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, repo_path) = make_reconciler(&tmp, wt.clone(), ghq.clone());

        touch_session(&reconciler, "ENG-3");
        let worktree_path = tmp.path().join("repo.ENG-3");
        seed_worktree(&wt, &repo_path, &worktree_path, "ENG-3");

        let found = reconciler.scan().await.unwrap();
        assert_eq!(found.len(), 1);
        let only = &found[0];
        assert!(only.session_present);
        assert_eq!(only.worktrees.len(), 1);
    }

    #[tokio::test]
    async fn scan_returns_empty_when_neither_present() {
        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, _) = make_reconciler(&tmp, wt.clone(), ghq.clone());

        let found = reconciler.scan().await.unwrap();
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn scan_filters_branches_outside_issue_id_regex() {
        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, repo_path) = make_reconciler(&tmp, wt.clone(), ghq.clone());

        // `main` does not match `^[A-Z]+-\d+$`; must be ignored.
        let main_path = tmp.path().join("repo");
        seed_worktree(&wt, &repo_path, &main_path, "main");

        let found = reconciler.scan().await.unwrap();
        assert!(found.is_empty(), "non-issue branch must be ignored");
    }

    #[tokio::test]
    async fn scan_tolerates_repo_not_locally_cloned() {
        // A repo declared in `[[repos]]` but never cloned via ghq is a normal
        // partial-environment scenario; the scan must not error.
        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let session_root = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_root).unwrap();
        let reconciler = RecoveryReconciler::new(
            session_root,
            allowlist(&["github.com/owner/never-cloned"]),
            wt,
            ghq,
        )
        .unwrap();
        let found = reconciler.scan().await.unwrap();
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn scan_combines_session_and_worktree_into_distinct_issues() {
        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, repo_path) = make_reconciler(&tmp, wt.clone(), ghq.clone());

        touch_session(&reconciler, "ENG-1");
        let two_path = tmp.path().join("repo.ENG-2");
        let three_path = tmp.path().join("repo.ENG-3");
        std::fs::create_dir_all(&two_path).unwrap();
        std::fs::create_dir_all(&three_path).unwrap();
        wt.seed_list(
            &repo_path,
            vec![
                WorktreeEntry {
                    path: two_path.clone(),
                    branch: Some("ENG-2".to_owned()),
                },
                WorktreeEntry {
                    path: three_path.clone(),
                    branch: Some("ENG-3".to_owned()),
                },
            ],
        );

        let found = reconciler.scan().await.unwrap();
        let ids: Vec<&str> = found.iter().map(|d| d.issue.0.as_str()).collect();
        assert_eq!(ids, vec!["ENG-1", "ENG-2", "ENG-3"]);
    }

    // ----- 9.2 decide tests -----

    #[tokio::test]
    async fn decide_resume_active_admits_with_recomputed_mode() {
        // Issue admitted with roki:impl → SpecDriven mode recomputed live.
        let (_server, linear) = linear_with_issue(issue_node(
            "ENG-1",
            "Todo",
            &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
            Some("u1"),
        ))
        .await;
        let judge = judge_for("u1");

        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, _) = make_reconciler(&tmp, wt, ghq);

        let discovered = DiscoveredIssue {
            issue: IssueId::from("ENG-1"),
            session_present: true,
            worktrees: vec![DiscoveredWorktree {
                repo_id: RepoId::from("github.com/owner/repo"),
                path: tmp.path().join("repo.ENG-1"),
                branch: "ENG-1".to_owned(),
            }],
        };
        let decision = reconciler.decide(discovered, &linear, &judge).await.unwrap();
        match decision {
            RecoveryDecision::ResumeActive { issue, mode } => {
                assert_eq!(issue, IssueId::from("ENG-1"));
                assert_eq!(mode, Mode::SpecDriven);
            }
            other => panic!("expected ResumeActive, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_resume_active_recomputes_mode_when_label_flipped_off() {
        // Pre-restart: `roki:impl` was set; post-restart: only `roki:ready`.
        // Mode must observe the live label set (NeedsClassify), not the
        // operator's prior choice.
        let (_server, linear) = linear_with_issue(issue_node(
            "ENG-7",
            "Todo",
            &[LABEL_ROKI_READY],
            Some("u1"),
        ))
        .await;
        let judge = judge_for("u1");

        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, _) = make_reconciler(&tmp, wt, ghq);

        let discovered = DiscoveredIssue {
            issue: IssueId::from("ENG-7"),
            session_present: true,
            worktrees: vec![DiscoveredWorktree {
                repo_id: RepoId::from("github.com/owner/repo"),
                path: tmp.path().join("repo.ENG-7"),
                branch: "ENG-7".to_owned(),
            }],
        };
        let decision = reconciler.decide(discovered, &linear, &judge).await.unwrap();
        match decision {
            RecoveryDecision::ResumeActive { mode, .. } => {
                assert_eq!(mode, Mode::NeedsClassify);
            }
            other => panic!("expected ResumeActive, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_orphaned_session_when_session_only_and_admission_fails() {
        // Linear says the issue is `Done` (state not admitted); session
        // tempdir still on disk → OrphanedSession.
        let (_server, linear) = linear_with_issue(issue_node(
            "ENG-2",
            "Done",
            &[LABEL_ROKI_READY],
            Some("u1"),
        ))
        .await;
        let judge = judge_for("u1");

        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, _) = make_reconciler(&tmp, wt, ghq);

        let discovered = DiscoveredIssue {
            issue: IssueId::from("ENG-2"),
            session_present: true,
            worktrees: vec![],
        };
        let decision = reconciler.decide(discovered, &linear, &judge).await.unwrap();
        assert!(matches!(decision, RecoveryDecision::OrphanedSession { .. }));
    }

    #[tokio::test]
    async fn decide_orphaned_worktree_when_worktree_only_and_admission_fails() {
        let (_server, linear) = linear_with_issue(issue_node(
            "ENG-3",
            "Done",
            &[],
            Some("u1"),
        ))
        .await;
        let judge = judge_for("u1");

        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, _) = make_reconciler(&tmp, wt, ghq);

        let discovered = DiscoveredIssue {
            issue: IssueId::from("ENG-3"),
            session_present: false,
            worktrees: vec![DiscoveredWorktree {
                repo_id: RepoId::from("github.com/owner/repo"),
                path: tmp.path().join("repo.ENG-3"),
                branch: "ENG-3".to_owned(),
            }],
        };
        let decision = reconciler.decide(discovered, &linear, &judge).await.unwrap();
        assert!(matches!(decision, RecoveryDecision::OrphanedWorktree { .. }));
    }

    #[tokio::test]
    async fn decide_fresh_queued_when_admission_passes_with_nothing_on_disk() {
        let (_server, linear) = linear_with_issue(issue_node(
            "ENG-4",
            "Todo",
            &[LABEL_ROKI_READY, LABEL_ROKI_IMPL],
            Some("u1"),
        ))
        .await;
        let judge = judge_for("u1");

        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, _) = make_reconciler(&tmp, wt, ghq);

        let discovered = DiscoveredIssue::empty(IssueId::from("ENG-4"));
        let decision = reconciler.decide(discovered, &linear, &judge).await.unwrap();
        match decision {
            RecoveryDecision::FreshQueued { issue, mode } => {
                assert_eq!(issue, IssueId::from("ENG-4"));
                assert_eq!(mode, Mode::SpecDriven);
            }
            other => panic!("expected FreshQueued, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decide_noop_when_terminal_and_nothing_on_disk() {
        let (_server, linear) = linear_with_issue(issue_node(
            "ENG-5",
            "Done",
            &[],
            Some("u1"),
        ))
        .await;
        let judge = judge_for("u1");

        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, _) = make_reconciler(&tmp, wt, ghq);

        let discovered = DiscoveredIssue::empty(IssueId::from("ENG-5"));
        let decision = reconciler.decide(discovered, &linear, &judge).await.unwrap();
        assert!(matches!(decision, RecoveryDecision::NoOp { .. }));
    }

    #[tokio::test]
    async fn decide_treats_missing_linear_record_as_skip() {
        // Linear returns `null` for the issue node — treat as no admission.
        // With residue still on disk we expect Orphan; with nothing, NoOp.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "issue": serde_json::Value::Null }
            })))
            .mount(&server)
            .await;
        let linear = LinearClient::new(server.uri(), SecretValue::new("tok"))
            .with_backoff_floor(Duration::from_millis(5));
        let judge = judge_for("u1");

        let tmp = TempDir::new().unwrap();
        let wt = Arc::new(MockWt::new());
        let ghq = Arc::new(MockGhq::new());
        let (reconciler, _) = make_reconciler(&tmp, wt, ghq);

        let discovered_with_session = DiscoveredIssue {
            issue: IssueId::from("ENG-6"),
            session_present: true,
            worktrees: vec![],
        };
        let decision = reconciler
            .decide(discovered_with_session, &linear, &judge)
            .await
            .unwrap();
        assert!(matches!(decision, RecoveryDecision::OrphanedSession { .. }));

        let discovered_empty = DiscoveredIssue::empty(IssueId::from("ENG-7"));
        let decision = reconciler
            .decide(discovered_empty, &linear, &judge)
            .await
            .unwrap();
        assert!(matches!(decision, RecoveryDecision::NoOp { .. }));
    }

    #[test]
    fn orphan_directive_fields_carry_session_and_worktree_evidence() {
        let discovered = DiscoveredIssue {
            issue: IssueId::from("ENG-9"),
            session_present: true,
            worktrees: vec![DiscoveredWorktree {
                repo_id: RepoId::from("github.com/owner/repo"),
                path: PathBuf::from("/tmp/repo.ENG-9"),
                branch: "ENG-9".to_owned(),
            }],
        };
        let fields = RecoveryReconciler::<MockWt, MockGhq>::orphan_directive_fields(&discovered);
        assert_eq!(fields["session_present"], serde_json::Value::Bool(true));
        assert_eq!(
            fields["worktree_paths"],
            serde_json::json!(["/tmp/repo.ENG-9"])
        );
        assert_eq!(
            fields["repos"],
            serde_json::json!(["github.com/owner/repo"])
        );
        assert_eq!(
            RecoveryReconciler::<MockWt, MockGhq>::ORPHAN_KIND,
            EscalationKind::Orphan
        );
    }
}
