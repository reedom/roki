//! End-to-end vetoable-transition test (task 4.5).
//!
//! This test pins the observable contract documented in design.md and
//! requirements 8.3 / 8.4: a registered vetoable subscriber may deny the
//! `Queued -> Active` transition for a specific `(repo, issue)` key, and the
//! orchestrator MUST honour that denial — the actor stays in `Queued` and the
//! daemon emits the documented veto log event with structured fields naming
//! the `(previous, next, repo, issue)` and the operator-readable reason. A
//! second issue whose key does NOT match the deny rule must progress
//! normally past `Queued` so the test demonstrates the veto is per-arc /
//! per-key and not a daemon-wide stall.
//!
//! Boundary notes
//!
//! * The vetoable-subscriber registration API exposed to test code is the
//!   same one production downstream specs (roki-spec-gate, roki-review-gate,
//!   roki-distill-postmerge) consume: `EventBus::register` accepting an
//!   `Arc<dyn TransitionSubscriber>`. The trait's default `veto` returns
//!   `Allow`, so a stub that overrides `veto` for the deny-targeted key and
//!   returns `Allow` everywhere else is exactly the production shape.
//! * No real `claude` subprocess is needed. The denied issue never reaches
//!   `Active` (the EventBus blocks the commit), and the unaffected issue's
//!   acceptance only requires `(Queued, Active)` to be allowed and the actor
//!   to advance — `StubEngine` returning `WorkerOutcome::CleanExit` carries
//!   the unaffected issue from `Active` to `AwaitingReview` without any
//!   external process. This keeps the test in well under one second of
//!   real time and avoids the `cargo build --example fake_claude` cost.
//! * No real `LinearTracker` / `wiremock` is needed: the orchestrator's
//!   `tracker_inbox` is a plain `mpsc::Receiver<NormalizedIssue>`, so the
//!   test can synthesise the two tracker events directly.
//!
//! Determinism notes
//!
//! * Both tracker events are sent up-front; the test polls the read handle
//!   via `await_condition` for the unaffected issue to leave `Queued`, then
//!   asserts the denied issue's state. There are no fixed-duration sleeps
//!   that decide ordering.
//! * `tracing_test::traced_test` captures every event emitted on the test's
//!   tokio task; the veto log line is deterministically present once the
//!   vetoed transition has been published through the bus.
//!
//! Requirements: 8.3, 8.4.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use roki_daemon::engine::policy::WorkerOutcome;
use roki_daemon::engine::{SupervisedEvent, WorkerContext};
use roki_daemon::orchestrator::core::{EngineLauncher, LaunchError, Orchestrator};
use roki_daemon::orchestrator::events::{EventBus, SubscriberError, TransitionSubscriber};
use roki_daemon::orchestrator::hooks::HookRegistry;
use roki_daemon::orchestrator::read::OrchestratorRead;
use roki_daemon::orchestrator::state::{
    IssueId, RepoId, TransitionEvent, VetoDecision, WorkerState,
};
use roki_daemon::shutdown::ShutdownSignal;
use roki_daemon::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};
mod common;
use crate::common::build_workspace_manager;

const TEST_REPO: &str = "repo-a";
const DENIED_ISSUE: &str = "ENG-DENIED";
const ALLOWED_ISSUE: &str = "ENG-ALLOWED";
const DENY_REASON: &str = "spec gate not satisfied";

/// Engine stub that emits a fixed terminal outcome for every launch. The
/// vetoed issue never reaches `launch` because the bus blocks the
/// `Queued -> Active` commit before workspace ensure / engine launch run.
struct StubEngine {
    outcome: WorkerOutcome,
    launches: Arc<AtomicUsize>,
}

#[async_trait]
impl EngineLauncher for StubEngine {
    async fn launch(
        &self,
        _ctx: WorkerContext,
        events: mpsc::Sender<SupervisedEvent>,
    ) -> Result<WorkerOutcome, LaunchError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        // Mirror the real adapter's invariant: emit exactly one terminal
        // Exited event per launch.
        let _ = events.send(SupervisedEvent::Exited(self.outcome)).await;
        Ok(self.outcome)
    }
}

/// Vetoable subscriber that denies `Queued -> Active` for a single
/// configured `(repo, issue)` key and allows every other vetoable
/// transition. Mirrors the shape downstream specs (e.g. roki-spec-gate)
/// will use to gate the same arc — a typed deny carrying an
/// operator-readable reason.
struct DenyingVetoSubscriber {
    id: &'static str,
    deny_issue: String,
    deny_reason: String,
    veto_invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl TransitionSubscriber for DenyingVetoSubscriber {
    fn id(&self) -> &str {
        self.id
    }

    async fn on_transition(&self, _event: &TransitionEvent) -> Result<(), SubscriberError> {
        Ok(())
    }

    async fn veto(&self, event: &TransitionEvent) -> Result<VetoDecision, SubscriberError> {
        self.veto_invocations.fetch_add(1, Ordering::SeqCst);
        let matches_target = event.previous == WorkerState::Queued
            && event.next == WorkerState::Active
            && event.issue.as_str() == self.deny_issue;
        if matches_target {
            Ok(VetoDecision::deny(self.deny_reason.clone()))
        } else {
            Ok(VetoDecision::Allow)
        }
    }
}

/// Records every transition event the orchestrator publishes, in order.
struct RecordingObserver {
    log: Arc<Mutex<Vec<TransitionEvent>>>,
}

#[async_trait]
impl TransitionSubscriber for RecordingObserver {
    fn id(&self) -> &str {
        "e2e-veto-recorder"
    }

    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
        self.log.lock().await.push(event.clone());
        Ok(())
    }
}

fn issue_event(repo: &str, issue: &str, state: TrackerIssueState) -> NormalizedIssue {
    NormalizedIssue {
        repo: RepoId::new(repo),
        issue: IssueId::new(issue),
        title: "vetoable transition test".to_string(),
        description: String::new(),
        state,
        labels: Vec::new(),
    }
}

async fn await_condition<F>(timeout: Duration, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    while !condition() {
        if timeout <= start.elapsed() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    true
}

/// The end-to-end vetoable-transition test pinned by tasks.md task 4.5.
#[tokio::test]
#[tracing_test::traced_test]
async fn e2e_vetoable_transition_blocks_denied_issue_and_lets_others_progress() {
    // ---- Workspace ----------------------------------------------------
    // Per-issue workspaces are created under this root. Only the unaffected
    // issue advances past `Queued`, so only that workspace is materialised.
    let parent = tempdir().expect("workspace tempdir");
    let (manager, _parent_keep, _wt, _ghq) =
        build_workspace_manager(parent, &[(TEST_REPO, "owner/repo-a", "repo-a")]);
    let workspace = Arc::new(manager);

    // ---- EventBus and subscribers ------------------------------------
    let event_bus = Arc::new(EventBus::with_default_capacity());
    let hook_registry = Arc::new(HookRegistry::new());
    let shutdown = ShutdownSignal::new();

    // Recording observer captures the published transition log so we can
    // assert the unaffected issue's full path and the absence of any
    // committed `Queued -> Active` for the denied issue.
    let recorded: Arc<Mutex<Vec<TransitionEvent>>> = Arc::new(Mutex::new(Vec::new()));
    event_bus.register(Arc::new(RecordingObserver {
        log: Arc::clone(&recorded),
    }));

    // Vetoable subscriber that denies `Queued -> Active` for the targeted
    // (repo, issue) and allows every other vetoable arc.
    let veto_invocations = Arc::new(AtomicUsize::new(0));
    event_bus.register(Arc::new(DenyingVetoSubscriber {
        id: "stub-spec-gate",
        deny_issue: DENIED_ISSUE.to_string(),
        deny_reason: DENY_REASON.to_string(),
        veto_invocations: Arc::clone(&veto_invocations),
    }));

    // ---- Engine + Orchestrator wiring -------------------------------
    let launches = Arc::new(AtomicUsize::new(0));
    let engine = Arc::new(StubEngine {
        outcome: WorkerOutcome::CleanExit,
        launches: Arc::clone(&launches),
    });

    let (inbox_tx, inbox_rx) = mpsc::channel::<NormalizedIssue>(8);

    let orchestrator = Orchestrator::new(
        Arc::clone(&workspace) as Arc<_>,
        engine,
        Arc::clone(&event_bus),
        Arc::clone(&hook_registry),
        shutdown.clone(),
        inbox_rx,
    );
    let read_handle = orchestrator.read_handle();
    let orch_handle = tokio::spawn(async move { orchestrator.run().await });

    // ---- Drive both issues ------------------------------------------
    // Send the denied issue first so its veto is recorded before the
    // unaffected one's progress masks the failing assertion in test logs.
    inbox_tx
        .send(issue_event(
            TEST_REPO,
            DENIED_ISSUE,
            TrackerIssueState::Active,
        ))
        .await
        .expect("send denied tracker event");
    inbox_tx
        .send(issue_event(
            TEST_REPO,
            ALLOWED_ISSUE,
            TrackerIssueState::Active,
        ))
        .await
        .expect("send allowed tracker event");

    // ---- Wait for the unaffected issue to leave Queued --------------
    // With the StubEngine returning CleanExit, the actor advances
    // Discovered -> Queued -> Active -> AwaitingReview. Waiting on
    // `AwaitingReview` proves both that the bus did not block this
    // unaffected issue and that the orchestrator has had a real chance
    // to evaluate the denied issue's vetoable transition by the time we
    // assert below.
    let allowed_issue = IssueId::new(ALLOWED_ISSUE);
    let allowed_progressed = await_condition(Duration::from_secs(5), || {
        match read_handle.issue(&allowed_issue) {
            Some(state) => matches!(
                state.state,
                WorkerState::Active | WorkerState::AwaitingReview
            ),
            None => false,
        }
    })
    .await;
    assert!(
        allowed_progressed,
        "unaffected issue must progress past Queued; recorded so far: {:?}",
        recorded.lock().await,
    );

    // ---- Assert the denied issue is veto-blocked --------------------
    // The denying veto subscriber MUST have been consulted for the
    // denied issue's `Queued -> Active` arc, and the orchestrator must
    // NOT have committed that transition.
    let denied_issue = IssueId::new(DENIED_ISSUE);
    let denied_state = read_handle
        .issue(&denied_issue)
        .expect("denied issue must have a state-map record after dispatch");
    assert_eq!(
        denied_state.state,
        WorkerState::Queued,
        "veto must keep the denied issue in Queued; observed {:?}",
        denied_state.state,
    );

    // The recorded transition log must contain BOTH issues' vetoable
    // `Queued -> Active` events because the EventBus publishes through
    // the broadcast path before the veto chain runs (design.md
    // "EventBus, SubscriberHooks": "Always publish through the broadcast
    // path so observers see the event regardless of veto outcome").
    // What MUST differ between the two issues is what happens AFTER the
    // veto evaluation: the unaffected issue advances to `Active ->
    // AwaitingReview`; the denied issue must NOT have any post-Queued
    // commit because `commit_transition` returns false on Deny and the
    // actor returns without writing state.
    let log = recorded.lock().await.clone();
    let allowed_progress_committed = log.iter().any(|ev| {
        ev.issue.as_str() == ALLOWED_ISSUE
            && ev.previous == WorkerState::Active
            && ev.next == WorkerState::AwaitingReview
    });
    assert!(
        allowed_progress_committed,
        "the unaffected issue must commit `Active -> AwaitingReview` after the engine clean-exit; got {log:?}",
    );
    let denied_post_queued = log
        .iter()
        .any(|ev| ev.issue.as_str() == DENIED_ISSUE && ev.previous == WorkerState::Active);
    assert!(
        !denied_post_queued,
        "the denied issue must never commit any `Active -> ...` transition (its Queued -> Active was vetoed); got {log:?}",
    );
    // Sanity: the denied issue's `Discovered -> Queued` (non-vetoable)
    // commit still appears in the log; that is what the read-handle
    // assertion above confirms via the projected state.
    let denied_queued_published = log.iter().any(|ev| {
        ev.issue.as_str() == DENIED_ISSUE
            && ev.previous == WorkerState::Discovered
            && ev.next == WorkerState::Queued
    });
    assert!(
        denied_queued_published,
        "the denied issue must still publish its Discovered -> Queued commit; got {log:?}",
    );

    // The vetoable subscriber must have been invoked for both issues'
    // `Queued -> Active` arcs (one Deny, one Allow). The bus consults the
    // chain on every vetoable publish.
    let denied_after = veto_invocations.load(Ordering::SeqCst);
    assert!(
        2 <= denied_after,
        "vetoable subscriber must be consulted for both issues' Queued -> Active arcs; observed {denied_after}",
    );

    // Engine must have launched exactly once: only for the unaffected
    // issue. The denied issue's `Queued -> Active` was vetoed before the
    // workspace ensure / engine launch step, so no subprocess work
    // happened for it.
    let total_launches = launches.load(Ordering::SeqCst);
    assert_eq!(
        total_launches, 1,
        "engine must launch only for the unaffected issue; observed {total_launches} launches",
    );

    // ---- Assert the documented veto log event ----------------------
    // `tracing_test::traced_test` captures all events emitted on this
    // test task. The orchestrator emits a single info-level event per
    // denied vetoable transition with the structured fields named in
    // design.md (`vetoable transition denied; staying in previous
    // state`) and including the deny reason from the subscriber.
    assert!(
        logs_contain("vetoable transition denied"),
        "the documented veto log event must be emitted with the canonical message",
    );
    assert!(
        logs_contain(DENY_REASON),
        "the veto log must include the operator-readable deny reason",
    );
    assert!(
        logs_contain(DENIED_ISSUE),
        "the veto log must carry the denied issue identifier",
    );
    // Per task 7.1b the veto log no longer carries `repo=...`: the
    // state-machine key collapsed to `(issue,)` and `TransitionEvent.repo`
    // was removed. Repo association moves onto the (post-7.1d)
    // `WorktreeRegistry` per opened worktree.
    assert!(
        logs_contain("Queued") && logs_contain("Active"),
        "the veto log must name the (previous, next) transition pair (Queued -> Active)",
    );

    // ---- Tear down --------------------------------------------------
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(5), orch_handle).await;
}
