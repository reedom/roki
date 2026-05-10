#![allow(dead_code)]

//! Per-ticket actor (slice 5).
//!
//! Each admitted ticket gets one `tokio::task` running this loop. The
//! task reads webhooks from a capacity-1 mpsc inbox, dispatches against
//! the current cache snapshot, runs one cycle at a time, and re-arms
//! against `pending_recheck` after each cycle terminates. The task exits
//! on cleanup-cycle eviction or on `DispatchMsg::Shutdown`.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::admission::AdmittedTicket;
use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::daemon::cache::DiffCache;
use crate::engine::context::CycleTrigger;
use crate::engine::dispatch::{DispatchMode, DispatchTarget, evaluate_from_cache};
use crate::engine::outcome::CycleKind;

/// Message carried on a ticket task's inbox.
#[derive(Debug)]
pub enum DispatchMsg {
    Webhook(AdmittedTicket),
    /// Cold-start admission seed. The first cycle for the ticket runs
    /// with `CycleTrigger::ColdStart`; subsequent webhook-driven cycles
    /// fall back to `CycleTrigger::Runtime` via the `Webhook` variant.
    ColdStartCycle(AdmittedTicket),
    Shutdown,
}

/// Outcome of one ticket-task loop iteration. Returned by the inner step
/// function so the test harness can assert against the decision the task
/// took without reading the full event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Dispatched { kind: CycleKind, evicted: bool },
    NoMatch,
    QueuedPending,
    Shutdown,
}

/// Trait the ticket task uses to invoke a cycle. Production wires this to
/// `engine::cycle::run_cycle` via `RealCycleRunner` (Task 8); unit tests
/// substitute a mock to exercise the loop deterministically.
#[async_trait::async_trait]
pub trait CycleRunner: Send + Sync {
    async fn run_cycle(
        &self,
        admitted: &AdmittedTicket,
        target: DispatchTarget<'_>,
        cycle_id: Uuid,
        cycle_trigger: CycleTrigger,
    ) -> CycleResult;
}

#[derive(Debug, Clone)]
pub enum CycleResult {
    Completed {
        kind: CycleKind,
        iters: u32,
    },
    Failed {
        meta: crate::daemon::real_runner::LegacyFailureMeta,
        kind: CycleKind,
    },
    /// Cleanup-shorthand path — the runner already performed the
    /// immediate-delete side effect.
    ShorthandDeleted,
    /// Cleanup-time fs error pushed to the escalation queue (fr:06).
    /// The cycle is dead; the ticket must be evicted without routing
    /// through `[[on_failure]]`.
    CleanupFsError {
        ticket_id: String,
    },
}

/// Run the ticket-task loop until `inbox` closes or `Shutdown` arrives.
/// Tests instantiate this with a mock runner.
#[allow(clippy::too_many_arguments)]
pub async fn run_ticket_task<R: CycleRunner>(
    ticket_id: String,
    cache: Arc<DiffCache>,
    workflow: Arc<WorkflowConfig>,
    cfg: Arc<RokiConfig>,
    mode: DispatchMode,
    runner: Arc<R>,
    mut inbox: mpsc::Receiver<DispatchMsg>,
    inbox_self: mpsc::Sender<DispatchMsg>,
    session_root: PathBuf,
    escalation: Arc<crate::escalation::EscalationQueue>,
) {
    while let Some(msg) = inbox.recv().await {
        let outcome = match msg {
            DispatchMsg::Shutdown => StepOutcome::Shutdown,
            DispatchMsg::Webhook(admitted) => {
                step_once(
                    &ticket_id,
                    admitted,
                    cache.clone(),
                    workflow.clone(),
                    cfg.clone(),
                    mode,
                    runner.clone(),
                    &inbox_self,
                    &session_root,
                    CycleTrigger::Runtime,
                    &escalation,
                )
                .await
            }
            DispatchMsg::ColdStartCycle(admitted) => {
                step_once(
                    &ticket_id,
                    admitted,
                    cache.clone(),
                    workflow.clone(),
                    cfg.clone(),
                    mode,
                    runner.clone(),
                    &inbox_self,
                    &session_root,
                    CycleTrigger::ColdStart,
                    &escalation,
                )
                .await
            }
        };

        if matches!(
            outcome,
            StepOutcome::Shutdown | StepOutcome::Dispatched { evicted: true, .. }
        ) {
            break;
        }
    }
}

/// One iteration of the ticket-task loop. Extracted so unit tests can
/// drive it directly without spawning a task or wiring an mpsc pair.
#[allow(clippy::too_many_arguments)]
pub async fn step_once<R: CycleRunner>(
    ticket_id: &str,
    _admitted_in: AdmittedTicket,
    cache: Arc<DiffCache>,
    workflow: Arc<WorkflowConfig>,
    _cfg: Arc<RokiConfig>,
    mode: DispatchMode,
    runner: Arc<R>,
    inbox_self: &mpsc::Sender<DispatchMsg>,
    _session_root: &std::path::Path,
    cycle_trigger: CycleTrigger,
    escalation: &crate::escalation::EscalationQueue,
) -> StepOutcome {
    // The dispatcher already updated the cache via `cache.observe`. We
    // re-snapshot here because additional webhooks may have arrived
    // between the dispatcher's send and the ticket task's recv.
    let snapshot = match cache.snapshot(ticket_id).await {
        Some(s) => s,
        None => return StepOutcome::NoMatch,
    };

    let target = evaluate_from_cache(ticket_id, &snapshot, &workflow, mode);
    let (kind, target_owned) = match target {
        DispatchTarget::NoMatch => return StepOutcome::NoMatch,
        DispatchTarget::CleanupShorthand => (CycleKind::Cleanup, DispatchTarget::CleanupShorthand),
        DispatchTarget::Cycle { kind, rule } => (kind, DispatchTarget::Cycle { kind, rule }),
    };

    let synthetic = synthesize_admitted(ticket_id, &snapshot);
    let cycle_id = Uuid::new_v4();
    cache.set_cycle_id(ticket_id, cycle_id).await;
    let result = runner
        .run_cycle(&synthetic, target_owned, cycle_id, cycle_trigger)
        .await;
    cache.clear_cycle_id(ticket_id).await;

    let evicted = matches!(
        &result,
        CycleResult::Completed {
            kind: CycleKind::Cleanup,
            ..
        } | CycleResult::ShorthandDeleted
    );

    if evicted {
        cache.evict(ticket_id).await;
        escalation.evict_ticket(ticket_id).await;
        return StepOutcome::Dispatched {
            kind,
            evicted: true,
        };
    }

    // Cleanup-time fs error: escalation queue was already pushed by the
    // runner (fr:06). Evict the cache entry without routing through
    // `[[on_failure]]`.
    if matches!(&result, CycleResult::CleanupFsError { .. }) {
        cache.evict(ticket_id).await;
        escalation.evict_ticket(ticket_id).await;
        return StepOutcome::Dispatched {
            kind: CycleKind::Cleanup,
            evicted: true,
        };
    }

    // Admission-revoke (slice 6 fr:01/fr:03/fr:05): if the dispatcher
    // marked this ticket for eviction while the cycle was running, drain
    // the flag and reclaim the cache entry. Worktree + session_tempdir
    // are intentionally retained — reclamation happens via a future
    // cleanup-cycle on terminal-state re-admission or via cold-start
    // orphan reconcile. `pending_evict` takes precedence over
    // `pending_recheck`.
    if cache.take_pending_evict(ticket_id).await {
        cache.evict(ticket_id).await;
        escalation.evict_ticket(ticket_id).await;
        return StepOutcome::Dispatched {
            kind,
            evicted: true,
        };
    }

    if cache.take_pending_recheck(ticket_id).await {
        if let Some(snap2) = cache.snapshot(ticket_id).await {
            let refreshed = synthesize_admitted(ticket_id, &snap2);
            // Ignore Full/Closed: Full means an already-queued webhook
            // will trigger the re-eval; Closed means the task is exiting
            // anyway.
            let _ = inbox_self.try_send(DispatchMsg::Webhook(refreshed));
        }
        return StepOutcome::QueuedPending;
    }

    StepOutcome::Dispatched {
        kind,
        evicted: false,
    }
}

fn synthesize_admitted(ticket_id: &str, snap: &crate::daemon::cache::CacheEntry) -> AdmittedTicket {
    AdmittedTicket {
        ticket: crate::linear::ticket::NormalizedTicket::new(
            ticket_id.into(),
            Some(snap.assignee.clone()),
            snap.status.clone(),
            snap.labels.iter().cloned().collect(),
            String::new(),
            String::new(),
        ),
        ghq: snap.repo.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    use crate::config::workflow::{WorkflowConfig, workflow_config_for_test};
    use crate::workflow::canonical::test_helpers as h;
    use crate::workflow::canonical::{
        EdgeTarget, RuleEntry, ScalarMatcher, StateMachine, Terminal, WhenClause,
    };

    struct MockCycleRunner {
        next: StdMutex<Vec<CycleResult>>,
        invocations: StdMutex<u32>,
        triggers: StdMutex<Vec<CycleTrigger>>,
    }

    impl MockCycleRunner {
        fn new(next: Vec<CycleResult>) -> Self {
            Self {
                next: StdMutex::new(next),
                invocations: StdMutex::new(0),
                triggers: StdMutex::new(vec![]),
            }
        }
    }

    #[async_trait::async_trait]
    impl CycleRunner for MockCycleRunner {
        async fn run_cycle(
            &self,
            _a: &AdmittedTicket,
            _t: DispatchTarget<'_>,
            _id: Uuid,
            trigger: CycleTrigger,
        ) -> CycleResult {
            *self.invocations.lock().unwrap() += 1;
            self.triggers.lock().unwrap().push(trigger);
            self.next
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(CycleResult::Completed {
                    kind: CycleKind::Rule,
                    iters: 1,
                })
        }
    }

    fn dummy_sm() -> StateMachine {
        let mut sm = h::state_machine();
        sm.start = "a".into();
        let mut a = h::state("a", "true");
        a.on_done = EdgeTarget::Terminal("__success__".into());
        sm.states.insert("a".into(), a);
        sm.terminals.insert(
            "__success__".into(),
            Terminal {
                id: "__success__".into(),
                outcome: "success".into(),
            },
        );
        sm
    }

    fn rule_for(status: &str) -> RuleEntry {
        let mut when = WhenClause::default();
        when.status = Some(ScalarMatcher::Eq(status.into()));
        RuleEntry {
            when: Some(when),
            state_machine: dummy_sm(),
        }
    }

    fn cleanup_for(status: &str) -> RuleEntry {
        let mut when = WhenClause::default();
        when.status = Some(ScalarMatcher::Eq(status.into()));
        RuleEntry {
            when: Some(when),
            state_machine: dummy_sm(),
        }
    }

    fn workflow_with_rule(status: &str) -> WorkflowConfig {
        workflow_config_for_test(
            "u1",
            Some("github.com/example/repo"),
            vec![rule_for(status)],
            vec![],
            vec![],
        )
    }

    fn workflow_with_cleanup(status: &str) -> WorkflowConfig {
        workflow_config_for_test(
            "u1",
            Some("github.com/example/repo"),
            vec![],
            vec![cleanup_for(status)],
            vec![],
        )
    }

    fn admitted(id: &str, status: &str) -> AdmittedTicket {
        AdmittedTicket {
            ticket: crate::linear::ticket::NormalizedTicket::new(
                id.into(),
                Some("u1".into()),
                status.into(),
                vec![],
                String::new(),
                String::new(),
            ),
            ghq: "github.com/example/repo".into(),
        }
    }

    fn cfg_for(path: &std::path::Path) -> Arc<RokiConfig> {
        Arc::new(RokiConfig::test_default(path))
    }

    fn escalation_for(path: &std::path::Path) -> Arc<crate::escalation::EscalationQueue> {
        use std::sync::Arc;
        use tokio::sync::Mutex;
        let writer = Arc::new(Mutex::new(
            crate::events::EventWriter::open(path, "_daemon").expect("open events"),
        ));
        crate::escalation::EscalationQueue::new(64, writer)
    }

    #[tokio::test]
    async fn dispatch_on_first_webhook_runs_cycle() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_rule("InProgress"));
        let runner = Arc::new(MockCycleRunner::new(vec![CycleResult::Completed {
            kind: CycleKind::Rule,
            iters: 1,
        }]));

        let a = admitted("t1", "InProgress");
        cache.observe(&a).await;

        let (tx, _rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache.clone(),
            wf,
            cfg_for(work.path()),
            DispatchMode::Default,
            runner.clone(),
            &tx,
            work.path(),
            CycleTrigger::Runtime,
            &escalation_for(work.path()),
        )
        .await;

        assert!(matches!(
            outcome,
            StepOutcome::Dispatched {
                kind: CycleKind::Rule,
                evicted: false
            }
        ));
        assert_eq!(*runner.invocations.lock().unwrap(), 1);
        assert_eq!(
            runner.triggers.lock().unwrap().as_slice(),
            &[CycleTrigger::Runtime]
        );
    }

    #[tokio::test]
    async fn step_once_with_cold_start_trigger_forwards_to_runner() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_rule("InProgress"));
        let runner = Arc::new(MockCycleRunner::new(vec![CycleResult::Completed {
            kind: CycleKind::Rule,
            iters: 1,
        }]));

        let a = admitted("t1", "InProgress");
        cache.observe(&a).await;

        let (tx, _rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache.clone(),
            wf,
            cfg_for(work.path()),
            DispatchMode::Default,
            runner.clone(),
            &tx,
            work.path(),
            CycleTrigger::ColdStart,
            &escalation_for(work.path()),
        )
        .await;

        assert!(matches!(
            outcome,
            StepOutcome::Dispatched {
                kind: CycleKind::Rule,
                evicted: false
            }
        ));
        assert_eq!(
            runner.triggers.lock().unwrap().as_slice(),
            &[CycleTrigger::ColdStart]
        );
    }

    #[tokio::test]
    async fn cleanup_completion_evicts_cache_entry() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_cleanup("Done"));
        let runner = Arc::new(MockCycleRunner::new(vec![CycleResult::Completed {
            kind: CycleKind::Cleanup,
            iters: 1,
        }]));

        let a = admitted("t1", "Done");
        cache.observe(&a).await;

        let (tx, _rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache.clone(),
            wf,
            cfg_for(work.path()),
            DispatchMode::Default,
            runner,
            &tx,
            work.path(),
            CycleTrigger::Runtime,
            &escalation_for(work.path()),
        )
        .await;

        assert!(matches!(
            outcome,
            StepOutcome::Dispatched {
                kind: CycleKind::Cleanup,
                evicted: true
            }
        ));
        assert!(cache.snapshot("t1").await.is_none());
    }

    #[tokio::test]
    async fn pending_recheck_loops_back_via_inbox() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_rule("InProgress"));
        let runner = Arc::new(MockCycleRunner::new(vec![CycleResult::Completed {
            kind: CycleKind::Rule,
            iters: 1,
        }]));

        let a = admitted("t1", "InProgress");
        cache.observe(&a).await;
        cache.set_pending_recheck("t1").await;

        let (tx, mut rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache.clone(),
            wf,
            cfg_for(work.path()),
            DispatchMode::Default,
            runner,
            &tx,
            work.path(),
            CycleTrigger::Runtime,
            &escalation_for(work.path()),
        )
        .await;

        assert!(matches!(outcome, StepOutcome::QueuedPending));
        let queued = rx.try_recv().expect("loop-back msg present");
        assert!(matches!(queued, DispatchMsg::Webhook(_)));
        assert!(!cache.snapshot("t1").await.unwrap().pending_recheck);
    }

    #[tokio::test]
    async fn pending_evict_after_cycle_evicts_cache_no_disk_delete() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_rule("InProgress"));
        // Rule cycle (NOT cleanup): the only path to eviction is via
        // pending_evict. ShorthandDeleted / Cleanup-completed paths are
        // explicitly ruled out so we know the disk-delete path was NOT
        // taken.
        let runner = Arc::new(MockCycleRunner::new(vec![CycleResult::Completed {
            kind: CycleKind::Rule,
            iters: 1,
        }]));

        let a = admitted("t1", "InProgress");
        cache.observe(&a).await;
        cache.set_pending_evict("t1").await;

        let (tx, _rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache.clone(),
            wf,
            cfg_for(work.path()),
            DispatchMode::Default,
            runner.clone(),
            &tx,
            work.path(),
            CycleTrigger::Runtime,
            &escalation_for(work.path()),
        )
        .await;

        // Rule-kind cycle that nevertheless evicts the cache because of
        // pending_evict. The Dispatched.kind reflects the cycle that
        // just ran, not the cleanup path.
        assert!(matches!(
            outcome,
            StepOutcome::Dispatched {
                kind: CycleKind::Rule,
                evicted: true
            }
        ));
        assert!(cache.snapshot("t1").await.is_none());
        assert_eq!(*runner.invocations.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn pending_evict_takes_precedence_over_pending_recheck() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_rule("InProgress"));
        let runner = Arc::new(MockCycleRunner::new(vec![CycleResult::Completed {
            kind: CycleKind::Rule,
            iters: 1,
        }]));

        let a = admitted("t1", "InProgress");
        cache.observe(&a).await;
        cache.set_pending_evict("t1").await;
        cache.set_pending_recheck("t1").await;

        let (tx, mut rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache.clone(),
            wf,
            cfg_for(work.path()),
            DispatchMode::Default,
            runner,
            &tx,
            work.path(),
            CycleTrigger::Runtime,
            &escalation_for(work.path()),
        )
        .await;

        assert!(matches!(
            outcome,
            StepOutcome::Dispatched {
                kind: CycleKind::Rule,
                evicted: true
            }
        ));
        // No loop-back webhook should have been queued.
        assert!(rx.try_recv().is_err());
        assert!(cache.snapshot("t1").await.is_none());
    }

    #[tokio::test]
    async fn no_match_returns_no_match() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_rule("InProgress"));
        let runner = Arc::new(MockCycleRunner::new(vec![]));

        let a = admitted("t1", "Triage");
        cache.observe(&a).await;

        let (tx, _rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache,
            wf,
            cfg_for(work.path()),
            DispatchMode::Default,
            runner.clone(),
            &tx,
            work.path(),
            CycleTrigger::Runtime,
            &escalation_for(work.path()),
        )
        .await;

        assert_eq!(outcome, StepOutcome::NoMatch);
        assert_eq!(*runner.invocations.lock().unwrap(), 0);
    }
}
