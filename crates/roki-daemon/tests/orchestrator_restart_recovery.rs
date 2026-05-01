//! Integration test for the post-7.1e restart recovery walk.
//!
//! Exercises all 5 cells of the decision matrix from
//! `design-agent-driven-repo-selection.md`:
//!
//! | cell name           | session | worktree | linear lifecycle      |
//! | ------------------- | ------- | -------- | --------------------- |
//! | `ResumeActive`      | yes     | yes      | active                |
//! | `OrphanedSession`   | yes     | no       | terminal (any)        |
//! | `OrphanedWorktree`  | no      | yes      | terminal (success)    |
//! | `OrphanedWorktree†` | no      | yes      | terminal-failure      | (retain)
//! | `FreshQueued`       | no      | no       | active                |
//! | `NoOp`              | no      | no       | terminal              | (omitted)
//!
//! The test pre-seeds a real `tempfile::TempDir`-backed git repo per
//! configured repo, creates branches via `git checkout -b` and worktrees
//! via `git worktree add` so the recovery walk's `git worktree list
//! --porcelain` invocation yields real entries. Sessions are pre-seeded
//! by creating directories under a `tempfile::TempDir`-rooted
//! [`SessionManager`].
//!
//! Determinism: per the task's status-report requirement the test must
//! pass across 3 sequential `--test-threads=1` reps. Per-test setup is
//! self-contained (own tempdirs, own session manager root, own stub
//! Linear reader); no shared global state is touched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::{TempDir, tempdir};
use tokio::sync::mpsc;

use roki_daemon::orchestrator::recovery::{
    IssueBranchPattern, RecoveryDecision, RecoveryIssueLifecycle, RecoveryLinearReader,
    RecoveryRepoInput, run_recovery,
};
use roki_daemon::orchestrator::state::{IssueId, RepoId};
use roki_daemon::session::SessionManager;
use roki_daemon::tools::{RealWt, WtTool};
use roki_daemon::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
use roki_daemon::worktrees::WorktreeRegistry;

/// In-memory `RecoveryLinearReader` stub keyed by `IssueId`. The matrix
/// cells differ in the lifecycle bucket reported per issue, so the stub
/// returns whatever bucket the test pre-seeded.
struct StubLinearReader {
    entries: HashMap<String, RecoveryIssueLifecycle>,
}

impl StubLinearReader {
    fn payload_for(&self, issue: &IssueId) -> (RecoveryIssueLifecycle, NormalizedIssue) {
        let lifecycle = self
            .entries
            .get(issue.as_str())
            .copied()
            .unwrap_or(RecoveryIssueLifecycle::Unknown);
        let payload = NormalizedIssue {
            issue: issue.clone(),
            title: format!("recovery-{}", issue.as_str()),
            description: String::new(),
            state: match lifecycle {
                RecoveryIssueLifecycle::Active => TrackerIssueState::Active,
                RecoveryIssueLifecycle::Terminal | RecoveryIssueLifecycle::TerminalFailure => {
                    TrackerIssueState::Terminal
                }
                RecoveryIssueLifecycle::Unknown => TrackerIssueState::Other,
            },
            labels: Vec::new(),
        };
        (lifecycle, payload)
    }
}

#[async_trait]
impl RecoveryLinearReader for StubLinearReader {
    async fn lookup_issue(
        &self,
        issue: &IssueId,
    ) -> Result<(RecoveryIssueLifecycle, Option<NormalizedIssue>), String> {
        let (lifecycle, payload) = self.payload_for(issue);
        Ok((lifecycle, Some(payload)))
    }

    async fn active_issues(&self) -> Result<Vec<(IssueId, NormalizedIssue)>, String> {
        let mut out: Vec<(IssueId, NormalizedIssue)> = Vec::new();
        for (id, lifecycle) in &self.entries {
            if matches!(lifecycle, RecoveryIssueLifecycle::Active) {
                let issue = IssueId::new(id.clone());
                let (_, payload) = self.payload_for(&issue);
                out.push((issue, payload));
            }
        }
        Ok(out)
    }
}

/// Initialize a git repo under `parent/<name>` and add a feature branch
/// worktree at `parent/<name>.<branch>` so `git worktree list
/// --porcelain` enumerates both. Returns the repo's main checkout path.
fn init_repo_with_worktree(parent: &Path, name: &str, branch: &str) -> PathBuf {
    let repo_path = parent.join(name);
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    git(&repo_path, &["init", "--quiet", "--initial-branch=main"]);
    // Configure the local repo so commits succeed without inheriting
    // possibly-missing global config.
    git(&repo_path, &["config", "user.email", "test@example.com"]);
    git(&repo_path, &["config", "user.name", "test"]);
    git(&repo_path, &["config", "commit.gpgsign", "false"]);
    // Create a baseline commit so the branch can be checked out cleanly.
    std::fs::write(repo_path.join("README.md"), "test\n").expect("write readme");
    git(&repo_path, &["add", "README.md"]);
    git(&repo_path, &["commit", "--quiet", "-m", "initial"]);
    // Create the feature branch and add a worktree at the documented
    // sibling-layout path so recovery's parser surfaces it.
    let worktree_path = parent.join(format!("{name}.{branch}"));
    git(
        &repo_path,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            worktree_path.to_str().expect("utf-8 worktree path"),
        ],
    );
    repo_path
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git command must spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed at {cwd:?}: {}",
        String::from_utf8_lossy(&out.stderr),
    );
}

fn pre_seed_session(manager: &SessionManager, issue: &str) {
    manager
        .create_session(&IssueId::new(issue))
        .expect("pre-seed session");
}

fn lifecycle_map<'a>(
    entries: &'a [(&'a str, RecoveryIssueLifecycle)],
) -> HashMap<String, RecoveryIssueLifecycle> {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_string(), *v))
        .collect()
}

#[tokio::test]
async fn recovery_exercises_all_five_matrix_cells() {
    // Skip the test gracefully if `git` is not on PATH so this suite
    // does not become a CI prerequisite blocker. Recovery's production
    // path requires git anyway; this guard is for laptop-CI parity.
    if Command::new("git")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: git binary not available on PATH");
        return;
    }

    let parent = tempdir().expect("repo parent tempdir");
    let session_root: TempDir = tempdir().expect("session root tempdir");
    let session_manager = SessionManager::with_root(session_root.path());
    let registry = WorktreeRegistry::new();
    let wt: Arc<dyn WtTool> = Arc::new(RealWt::new());
    let pattern = IssueBranchPattern::default_pattern();

    // ---- Cell 1: ResumeActive — session + worktree + Linear active ----
    pre_seed_session(&session_manager, "ENG-1");
    let repo_resume = init_repo_with_worktree(parent.path(), "repo-resume", "ENG-1");

    // ---- Cell 2: OrphanedSession — session present, no worktree, Linear terminal ----
    pre_seed_session(&session_manager, "ENG-2");

    // ---- Cell 3: OrphanedWorktree (remove) — no session, worktree present, Linear terminal ----
    let repo_orphan_remove = init_repo_with_worktree(parent.path(), "repo-orphan-remove", "ENG-3");

    // ---- Cell 4: OrphanedWorktree (retain) — no session, worktree present, Linear terminal-failure ----
    let repo_orphan_retain = init_repo_with_worktree(parent.path(), "repo-orphan-retain", "ENG-4");

    // ---- Cell 5: FreshQueued — Linear active, nothing on disk ----
    // ENG-5 is reported active by the stub's `active_issues` bulk fetch
    // but has no session and no worktree; the recovery walk seeds it
    // into the union and produces FreshQueued.

    // ---- Bonus: NoOp — Linear terminal, nothing on disk — must NOT
    //      produce a decision (verified by absence in the result list).
    //      ENG-6 is intentionally omitted from the lifecycle map; it
    //      would never be seeded on disk regardless.

    let lifecycles = lifecycle_map(&[
        ("ENG-1", RecoveryIssueLifecycle::Active),
        ("ENG-2", RecoveryIssueLifecycle::Terminal),
        ("ENG-3", RecoveryIssueLifecycle::Terminal),
        ("ENG-4", RecoveryIssueLifecycle::TerminalFailure),
        ("ENG-5", RecoveryIssueLifecycle::Active),
    ]);
    let reader = StubLinearReader {
        entries: lifecycles,
    };

    let recovery_repos = vec![
        RecoveryRepoInput {
            repo: RepoId::new("repo-resume"),
            repo_path: repo_resume.clone(),
        },
        RecoveryRepoInput {
            repo: RepoId::new("repo-orphan-remove"),
            repo_path: repo_orphan_remove.clone(),
        },
        RecoveryRepoInput {
            repo: RepoId::new("repo-orphan-retain"),
            repo_path: repo_orphan_retain.clone(),
        },
    ];

    let (tracker_tx, mut tracker_rx) = mpsc::channel::<NormalizedIssue>(16);

    let decisions = run_recovery(
        &session_manager,
        &recovery_repos,
        &pattern,
        wt.as_ref(),
        &reader,
        &registry,
        &tracker_tx,
    )
    .await
    .expect("recovery scan must succeed");

    // ---- Assertions on the decision shape -------------------------------
    // Decisions are sorted by IssueId lexicographically: ENG-1..ENG-5.
    // ENG-6 (NoOp) is omitted by construction.
    assert_eq!(
        decisions.len(),
        5,
        "expected exactly one decision per matrix cell (5); got {decisions:?}",
    );

    // ENG-1: ResumeActive with one worktree
    match &decisions[0] {
        RecoveryDecision::ResumeActive { issue, worktrees } => {
            assert_eq!(issue.as_str(), "ENG-1");
            assert_eq!(worktrees.len(), 1, "ENG-1 has one worktree on disk");
            assert_eq!(worktrees[0].repo.as_str(), "repo-resume");
            assert_eq!(worktrees[0].branch.as_str(), "ENG-1");
        }
        other => panic!("ENG-1 expected ResumeActive; got {other:?}"),
    }

    // ENG-2: OrphanedSession (terminal Linear, session-only on disk)
    match &decisions[1] {
        RecoveryDecision::OrphanedSession { issue } => {
            assert_eq!(issue.as_str(), "ENG-2");
        }
        other => panic!("ENG-2 expected OrphanedSession; got {other:?}"),
    }

    // ENG-3: OrphanedWorktree (remove)
    match &decisions[2] {
        RecoveryDecision::OrphanedWorktree {
            issue,
            worktrees,
            retain,
        } => {
            assert_eq!(issue.as_str(), "ENG-3");
            assert_eq!(worktrees.len(), 1);
            assert!(
                !retain,
                "Linear terminal-success implies remove, not retain"
            );
        }
        other => panic!("ENG-3 expected OrphanedWorktree(remove); got {other:?}"),
    }

    // ENG-4: OrphanedWorktree (retain on terminal-failure)
    match &decisions[3] {
        RecoveryDecision::OrphanedWorktree {
            issue,
            worktrees,
            retain,
        } => {
            assert_eq!(issue.as_str(), "ENG-4");
            assert_eq!(worktrees.len(), 1);
            assert!(retain, "Linear terminal-failure must retain the worktree");
        }
        other => panic!("ENG-4 expected OrphanedWorktree(retain); got {other:?}"),
    }

    // ENG-5: FreshQueued (Linear active + nothing on disk). Recovery
    // unions the bulk-active Linear slice into the candidate set so this
    // cell is reachable.
    match &decisions[4] {
        RecoveryDecision::FreshQueued { issue } => {
            assert_eq!(issue.as_str(), "ENG-5");
        }
        other => panic!("ENG-5 expected FreshQueued; got {other:?}"),
    }

    // ---- Side effects ---------------------------------------------------

    // ResumeActive (ENG-1) registered the worktree in the in-memory
    // registry so the post-recovery Cleaning arc still finds it.
    let registered = registry.list_for_issue(&IssueId::new("ENG-1"));
    assert_eq!(registered.len(), 1, "ENG-1 worktree must be re-registered");
    assert_eq!(registered[0].repo.as_str(), "repo-resume");
    assert_eq!(registered[0].branch.as_str(), "ENG-1");

    // OrphanedSession (ENG-2) drove SessionManager::remove_session;
    // confirm the directory is gone.
    assert!(
        !session_root.path().join("ENG-2").exists(),
        "ENG-2 session tempdir must be removed by recovery",
    );

    // ResumeActive (ENG-1) preserves the session tempdir; FreshQueued
    // (ENG-5) had no session pre-seeded so nothing to preserve.
    assert!(
        session_root.path().join("ENG-1").is_dir(),
        "ENG-1 session tempdir must be retained for resume",
    );

    // OrphanedWorktree (ENG-3, remove) drove `wt.remove`; the worktree
    // path on disk must be gone. (The repo's main checkout remains.)
    let removed_worktree = parent.path().join("repo-orphan-remove.ENG-3");
    assert!(
        !removed_worktree.exists(),
        "ENG-3 worktree must be removed; still at {removed_worktree:?}",
    );

    // OrphanedWorktree (ENG-4, retain) preserves the worktree on disk.
    let retained_worktree = parent.path().join("repo-orphan-retain.ENG-4");
    assert!(
        retained_worktree.exists(),
        "ENG-4 worktree must be retained for terminal-failure inspection",
    );

    // ResumeActive arms posted synthetic Active-state tracker events.
    // ENG-1 and ENG-5 should both have produced events; ENG-2/ENG-3/ENG-4
    // must not.
    let mut received_issues: Vec<String> = Vec::new();
    while let Ok(ev) = tracker_rx.try_recv() {
        assert_eq!(ev.state, TrackerIssueState::Active);
        received_issues.push(ev.issue.as_str().to_string());
    }
    received_issues.sort();
    assert_eq!(
        received_issues,
        vec!["ENG-1".to_string(), "ENG-5".to_string()]
    );

    // ---- NoOp cell verification -----------------------------------------
    // The NoOp cell (Linear terminal, nothing on disk) is verified by
    // omission: ENG-6 was never seeded on disk and never appears in the
    // decision list. Re-running recovery against an empty disk and a
    // Linear-terminal stub must yield zero decisions.
    let empty_session_root = tempdir().expect("empty session root");
    let empty_session_manager = SessionManager::with_root(empty_session_root.path());
    let empty_registry = WorktreeRegistry::new();
    let empty_lifecycles = lifecycle_map(&[("ENG-6", RecoveryIssueLifecycle::Terminal)]);
    let empty_reader = StubLinearReader {
        entries: empty_lifecycles,
    };
    let (empty_tx, _empty_rx) = mpsc::channel::<NormalizedIssue>(4);
    let empty_decisions = run_recovery(
        &empty_session_manager,
        &[],
        &pattern,
        wt.as_ref(),
        &empty_reader,
        &empty_registry,
        &empty_tx,
    )
    .await
    .expect("empty recovery scan must succeed");
    assert!(
        empty_decisions.is_empty(),
        "Linear-terminal issue with no disk artifacts must be NoOp (omitted); got {empty_decisions:?}",
    );

    // Determinism guard: pause briefly so any background-spawned
    // tracing tasks finish before the tempdirs drop. Recovery itself is
    // synchronous within the await, so this is belt-and-suspenders.
    tokio::time::sleep(Duration::from_millis(10)).await;
}
