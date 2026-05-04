//! Section 13.2 — SPEC_DRIVEN happy path end-to-end.
//!
//! Drives the orchestrator core through one full SPEC_DRIVEN session:
//!
//! - Admit with `Mode::SpecDriven`.
//! - Orchestrator's first turn nominates `implement` → `review` →
//!   `validate` → `open_pr` → `finalize_review`.
//! - Each phase returns `phase_complete(success)` and the orchestrator's
//!   final turn emits `action=stop outcome=success`.
//! - The actor lands in `Inactive(AwaitingLinear)` per design.md.
//! - Worktree is preserved (no Cleaning trigger from `outcome=success`).
//!
//! The orchestrator-stub mirrors the `orchestrator_happy` fake_claude mode
//! the integration brief calls for: a pre-canned action queue per launch.
//!
//! Spec refs: requirements.md 4.3, 5.6, 5.11.
//!
//! Runtime-wiring TODO: when `runtime::run_with_shutdown` composes
//! orchestrator + tracker + recovery, replace the direct
//! `orchestrator.send(TrackerAdmit, …)` with a signed webhook POST.

mod common;

use common::{run_phase_action, stop_action, OrchHarness};
use roki_daemon::engine::orchestrator_session::action_parser::{Outcome, PhaseName};
use roki_daemon::orchestrator::core::{ActorMessage, OrchestratorActionEvent};
use roki_daemon::orchestrator::state::{InactiveReason, IssueId, Mode};
use roki_daemon::tracker::model::RepoId;

#[tokio::test]
async fn spec_driven_full_lifecycle_lands_in_awaiting_linear() {
    let h = OrchHarness::new();
    h.engine
        .push_stream(vec![
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Implement)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Review)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::Validate)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::OpenPr)),
            OrchestratorActionEvent::Action(run_phase_action(PhaseName::FinalizeReview)),
            OrchestratorActionEvent::Action(stop_action(Outcome::Success)),
        ])
        .await;

    let issue = IssueId::from("ENG-100");
    let repo = RepoId::from("github.com/owner/repo");
    h.orchestrator
        .send(
            issue.clone(),
            ActorMessage::TrackerAdmit {
                mode: Mode::SpecDriven,
                repo: Some(repo.clone()),
            },
        )
        .await
        .expect("admit");

    let snap = h
        .wait_for_inactive(&issue, InactiveReason::AwaitingLinear)
        .await;
    assert_eq!(snap.mode, Some(Mode::SpecDriven));

    // Phase invocations: 5 non-classify phases.
    let invs = h.phase.invocations.lock().await;
    let phases: Vec<PhaseName> = invs.iter().map(|i| i.phase).collect();
    assert_eq!(
        phases,
        vec![
            PhaseName::Implement,
            PhaseName::Review,
            PhaseName::Validate,
            PhaseName::OpenPr,
            PhaseName::FinalizeReview,
        ]
    );
    assert!(invs.iter().all(|i| i.worktree_path.is_some()));

    // Worktree cleanup must NOT have been invoked (success → AwaitingLinear,
    // operator finalizes via Linear close).
    let cleanup = h.worktree.cleanup_calls.lock().await;
    assert!(cleanup.is_empty(), "AwaitingLinear retains worktree");

    // Worktree ensured exactly five times (one per non-classify phase).
    let ensure = h.worktree.ensure_calls.lock().await;
    assert_eq!(ensure.len(), 5);
    assert!(ensure.iter().all(|(i, r)| i == &issue && r == &repo));

    // Orchestrator session shutdown invoked exactly once after action=stop.
    let shutdown = *h.engine.shutdown_calls.lock().await;
    assert_eq!(shutdown, 1, "shutdown invoked exactly once on stop");
}
