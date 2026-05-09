#![allow(dead_code)]

//! Webhook intake -> admission -> cache observe -> spawn-or-route.
//!
//! The dispatcher does NOT execute cycles. It owns the per-ticket task
//! registry and the daemon-scoped event writer. Cycle execution lives
//! in `daemon::ticket_task` via the `CycleRunner` trait.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::admission::{self, AdmittedTicket};
use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::daemon::cache::{DiffCache, DiffOutcome};
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::ticket_task::{CycleRunner, DispatchMsg};
use crate::engine::dispatch::DispatchMode;
use crate::events::{Event, EventWriter, WebhookSkipReason, now_rfc3339};
use crate::linear::client::MeId;
use crate::linear::ticket::NormalizedTicket;

pub struct Dispatcher<R: CycleRunner + 'static> {
    cache: Arc<DiffCache>,
    tickets: Arc<Mutex<HashMap<String, TicketHandle>>>,
    workflow: Arc<WorkflowConfig>,
    cfg: Arc<RokiConfig>,
    me: Option<MeId>,
    mode: DispatchMode,
    shutdown: ShutdownToken,
    runner: Arc<R>,
    daemon_events: Arc<Mutex<EventWriter>>,
}

pub struct TicketHandle {
    pub inbox: mpsc::Sender<DispatchMsg>,
    pub join: JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchAction {
    Routed,
    Spawned,
    BackPressureSetPending,
    Skipped(WebhookSkipReason),
    AdmissionRejected,
}

impl<R: CycleRunner + 'static> Dispatcher<R> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cache: Arc<DiffCache>,
        workflow: Arc<WorkflowConfig>,
        cfg: Arc<RokiConfig>,
        me: Option<MeId>,
        mode: DispatchMode,
        shutdown: ShutdownToken,
        runner: Arc<R>,
        daemon_events: Arc<Mutex<EventWriter>>,
    ) -> Self {
        Self {
            cache,
            tickets: Arc::new(Mutex::new(HashMap::new())),
            workflow,
            cfg,
            me,
            mode,
            shutdown,
            runner,
            daemon_events,
        }
    }

    pub fn tickets(&self) -> Arc<Mutex<HashMap<String, TicketHandle>>> {
        self.tickets.clone()
    }

    /// Drain `rx` until the listener side closes or shutdown fires. Each
    /// ticket is routed to its per-ticket task; new tickets cause a fresh
    /// task to spawn.
    pub async fn drain(&self, mut rx: mpsc::Receiver<NormalizedTicket>) {
        while let Some(ticket) = rx.recv().await {
            let _ = self.on_webhook(ticket).await;
            if self.shutdown.is_fired() {
                break;
            }
        }
    }

    pub async fn on_webhook(&self, ticket: NormalizedTicket) -> DispatchAction {
        let ticket_id = ticket.id.clone();

        let me_ref = self.me.clone().unwrap_or_else(|| MeId(String::new()));
        let admitted = match admission::accept(&ticket, &self.workflow, &me_ref) {
            Ok(a) => a,
            Err(err) => {
                let reason = match err {
                    crate::error::AdmissionError::AssigneeMismatch { .. } => {
                        WebhookSkipReason::AssigneeMismatch
                    }
                    crate::error::AdmissionError::NoRepos => WebhookSkipReason::RepoUnresolvable,
                };
                self.emit_skip(&ticket_id, reason).await;
                // Admission-revoke (slice 6 fr:01/fr:03/fr:05): if the
                // rejected ticket was previously cached, mark it for
                // cache-only eviction. Worktree + session_tempdir are
                // intentionally retained — reclamation happens via a
                // future cleanup-cycle on terminal-state re-admission or
                // via cold-start orphan reconcile.
                if self.cache.snapshot(&ticket_id).await.is_some() {
                    self.cache.set_pending_evict(&ticket_id).await;
                    let map = self.tickets.lock().await;
                    if !map.contains_key(&ticket_id) {
                        // No in-flight ticket task to drain the flag —
                        // reclaim cache immediately.
                        drop(map);
                        self.cache.evict(&ticket_id).await;
                    }
                }
                return DispatchAction::AdmissionRejected;
            }
        };

        // Re-admission cancels any pending eviction set by a previous
        // admission-revoking webhook so the entry stays alive.
        if let Some(snap) = self.cache.snapshot(&admitted.ticket.id).await {
            if snap.pending_evict {
                self.cache.clear_pending_evict(&admitted.ticket.id).await;
            }
        }

        let outcome = self.cache.observe(&admitted).await;
        if matches!(outcome, DiffOutcome::Unchanged) {
            self.emit_skip(&admitted.ticket.id, WebhookSkipReason::NoDiff)
                .await;
            return DispatchAction::Skipped(WebhookSkipReason::NoDiff);
        }

        let mut map = self.tickets.lock().await;

        if let Some(handle) = map.get(&admitted.ticket.id) {
            match handle
                .inbox
                .try_send(DispatchMsg::Webhook(admitted.clone()))
            {
                Ok(()) => DispatchAction::Routed,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    drop(map);
                    self.cache.set_pending_recheck(&admitted.ticket.id).await;
                    DispatchAction::BackPressureSetPending
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    map.remove(&admitted.ticket.id);
                    self.spawn_and_route(&mut map, admitted).await;
                    DispatchAction::Spawned
                }
            }
        } else {
            self.spawn_and_route(&mut map, admitted).await;
            DispatchAction::Spawned
        }
    }

    async fn spawn_and_route(
        &self,
        map: &mut HashMap<String, TicketHandle>,
        admitted: AdmittedTicket,
    ) {
        self.spawn_ticket_task(map, admitted, DispatchMsg::Webhook);
    }

    /// Cold-start admission entry point. Spawns a per-ticket task and
    /// seeds the inbox with `DispatchMsg::ColdStartCycle`, so the first
    /// cycle for the ticket runs with `CycleTrigger::ColdStart`. Any
    /// subsequent webhook-driven cycles for the same ticket use
    /// `CycleTrigger::Runtime` (entered via the `Webhook` arm).
    ///
    /// Cold start runs before the listener accepts traffic, so the
    /// per-ticket registry is asserted empty for `admitted.ticket.id`
    /// in debug builds.
    pub async fn admit_for_cold_start(&self, admitted: AdmittedTicket) -> Result<(), ()> {
        let mut map = self.tickets.lock().await;
        debug_assert!(
            !map.contains_key(&admitted.ticket.id),
            "cold_start admit must run before listener accepts traffic"
        );
        self.spawn_ticket_task(&mut map, admitted, DispatchMsg::ColdStartCycle);
        Ok(())
    }

    /// Shared spawn helper used by both the webhook path
    /// (`spawn_and_route`) and the cold-start path
    /// (`admit_for_cold_start`). The caller picks which `DispatchMsg`
    /// variant seeds the inbox via the `seed` closure.
    fn spawn_ticket_task<F>(
        &self,
        map: &mut HashMap<String, TicketHandle>,
        admitted: AdmittedTicket,
        seed: F,
    ) where
        F: FnOnce(AdmittedTicket) -> DispatchMsg,
    {
        let (tx, rx) = mpsc::channel::<DispatchMsg>(1);
        let tx_self = tx.clone();
        let ticket_id = admitted.ticket.id.clone();
        let cache = self.cache.clone();
        let wf = self.workflow.clone();
        let cfg = self.cfg.clone();
        let mode = self.mode;
        let runner = self.runner.clone();
        let session_root = self.cfg.paths.session_root.clone();
        let id_for_task = ticket_id.clone();

        let join = tokio::spawn(async move {
            crate::daemon::ticket_task::run_ticket_task(
                id_for_task,
                cache,
                wf,
                cfg,
                mode,
                runner,
                rx,
                tx_self,
                session_root,
            )
            .await;
        });

        // Seed the inbox before inserting the handle so the task has
        // something to do as soon as the scheduler picks it up.
        let _ = tx.try_send(seed(admitted));
        map.insert(ticket_id, TicketHandle { inbox: tx, join });
    }

    async fn emit_skip(&self, ticket_id: &str, reason: WebhookSkipReason) {
        let _ = self
            .daemon_events
            .lock()
            .await
            .emit(&Event::WebhookSkipped {
                ts: now_rfc3339(),
                ticket_id: ticket_id.to_string(),
                reason,
                source: None,
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::workflow::{AdmissionRepo, AdmissionSection, Rule};
    use crate::daemon::ticket_task::{CycleResult, CycleRunner};
    use crate::engine::context::CycleTrigger;
    use crate::engine::dispatch::DispatchTarget;
    use crate::engine::outcome::{CycleKind, PhaseBody};
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    struct CountingRunner {
        invocations: Arc<StdMutex<u32>>,
        triggers: Arc<StdMutex<Vec<CycleTrigger>>>,
    }

    impl CountingRunner {
        fn new(invocations: Arc<StdMutex<u32>>) -> Self {
            Self {
                invocations,
                triggers: Arc::new(StdMutex::new(vec![])),
            }
        }

        fn triggers(&self) -> Arc<StdMutex<Vec<CycleTrigger>>> {
            self.triggers.clone()
        }
    }

    #[async_trait::async_trait]
    impl CycleRunner for CountingRunner {
        async fn run_cycle(
            &self,
            _a: &AdmittedTicket,
            _t: DispatchTarget<'_>,
            _id: uuid::Uuid,
            trigger: CycleTrigger,
        ) -> CycleResult {
            *self.invocations.lock().unwrap() += 1;
            self.triggers.lock().unwrap().push(trigger);
            // Hold the task busy briefly so a second webhook in the same
            // test sees the inbox full / task alive.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            CycleResult::Completed {
                kind: CycleKind::Rule,
                iters: 1,
            }
        }
    }

    fn workflow() -> Arc<WorkflowConfig> {
        Arc::new(WorkflowConfig {
            admission: AdmissionSection {
                assignee: "u1".into(),
            },
            repo: Some(AdmissionRepo {
                ghq: "github.com/example/repo".into(),
            }),
            rules: vec![Rule {
                when_status: "InProgress".into(),
                when_labels_has_all: vec![],
                pre: None,
                run: PhaseBody::InlineCmd { cmd: "true".into() },
                post: None,
            }],
            cleanups: vec![],
            on_failures: vec![],
        })
    }

    fn ticket(id: &str, status: &str) -> NormalizedTicket {
        NormalizedTicket::new(
            id.into(),
            Some("u1".into()),
            status.into(),
            vec![],
            String::new(),
            String::new(),
        )
    }

    fn dispatcher_with(
        runner: Arc<CountingRunner>,
        work: &std::path::Path,
    ) -> Dispatcher<CountingRunner> {
        let cfg = Arc::new(RokiConfig::test_default(work));
        let events = Arc::new(Mutex::new(
            EventWriter::open(work, "_daemon").expect("open events"),
        ));
        Dispatcher::new(
            Arc::new(DiffCache::new()),
            workflow(),
            cfg,
            Some(MeId("u1".into())),
            DispatchMode::Default,
            ShutdownToken::new(),
            runner,
            events,
        )
    }

    #[tokio::test]
    async fn first_webhook_spawns_task() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner::new(count.clone())), work.path());
        let action = d.on_webhook(ticket("t1", "InProgress")).await;
        assert_eq!(action, DispatchAction::Spawned);
        assert!(d.tickets().lock().await.contains_key("t1"));
    }

    #[tokio::test]
    async fn duplicate_unchanged_triple_is_skipped() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner::new(count.clone())), work.path());
        d.on_webhook(ticket("t1", "InProgress")).await;
        let action = d.on_webhook(ticket("t1", "InProgress")).await;
        assert_eq!(action, DispatchAction::Skipped(WebhookSkipReason::NoDiff));
    }

    fn workflow_no_repos() -> Arc<WorkflowConfig> {
        Arc::new(WorkflowConfig {
            admission: AdmissionSection {
                assignee: "u1".into(),
            },
            repo: None,
            rules: vec![Rule {
                when_status: "InProgress".into(),
                when_labels_has_all: vec![],
                pre: None,
                run: PhaseBody::InlineCmd { cmd: "true".into() },
                post: None,
            }],
            cleanups: vec![],
            on_failures: vec![],
        })
    }

    fn dispatcher_no_repos(
        runner: Arc<CountingRunner>,
        work: &std::path::Path,
    ) -> Dispatcher<CountingRunner> {
        let cfg = Arc::new(RokiConfig::test_default(work));
        let events = Arc::new(Mutex::new(
            EventWriter::open(work, "_daemon").expect("open events"),
        ));
        Dispatcher::new(
            Arc::new(DiffCache::new()),
            workflow_no_repos(),
            cfg,
            Some(MeId("u1".into())),
            DispatchMode::Default,
            ShutdownToken::new(),
            runner,
            events,
        )
    }

    #[tokio::test]
    async fn admission_rejection_assignee_mismatch_emits_correct_reason() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner::new(count.clone())), work.path());
        let bad = NormalizedTicket::new(
            "t1".into(),
            Some("intruder".into()),
            "InProgress".into(),
            vec![],
            String::new(),
            String::new(),
        );
        let action = d.on_webhook(bad).await;
        assert_eq!(action, DispatchAction::AdmissionRejected);
        assert!(d.tickets().lock().await.is_empty());

        let events_path = work.path().join("_daemon.events.jsonl");
        let body = std::fs::read_to_string(&events_path).unwrap();
        let line = body.lines().last().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["reason"], "assignee_mismatch");
    }

    #[tokio::test]
    async fn admission_rejection_no_repos_emits_repo_unresolvable() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_no_repos(Arc::new(CountingRunner::new(count.clone())), work.path());
        let action = d.on_webhook(ticket("t1", "InProgress")).await;
        assert_eq!(action, DispatchAction::AdmissionRejected);
        assert!(d.tickets().lock().await.is_empty());

        let events_path = work.path().join("_daemon.events.jsonl");
        let body = std::fs::read_to_string(&events_path).unwrap();
        let line = body.lines().last().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["reason"], "repo_unresolvable");
    }

    fn admitted_for(id: &str, status: &str) -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
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

    #[tokio::test]
    async fn admission_rejection_on_cached_ticket_evicts_cache_when_no_task() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner::new(count.clone())), work.path());

        // Pre-populate the cache without spawning a ticket task.
        let admitted = admitted_for("t1", "InProgress");
        d.cache.observe(&admitted).await;
        assert!(d.cache.snapshot("t1").await.is_some());

        // Webhook with mismatched assignee — admission rejects.
        let bad = NormalizedTicket::new(
            "t1".into(),
            Some("intruder".into()),
            "InProgress".into(),
            vec![],
            String::new(),
            String::new(),
        );
        let action = d.on_webhook(bad).await;
        assert_eq!(action, DispatchAction::AdmissionRejected);

        // No ticket task was running, so the dispatcher should have
        // reclaimed the cache immediately.
        assert!(d.cache.snapshot("t1").await.is_none());
    }

    #[tokio::test]
    async fn admission_rejection_on_cached_ticket_with_task_marks_pending_evict() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner::new(count.clone())), work.path());

        // First webhook spawns a ticket task and seeds the cache.
        let action = d.on_webhook(ticket("t1", "InProgress")).await;
        assert_eq!(action, DispatchAction::Spawned);

        // Second webhook with bad assignee — admission rejects.
        let bad = NormalizedTicket::new(
            "t1".into(),
            Some("intruder".into()),
            "InProgress".into(),
            vec![],
            String::new(),
            String::new(),
        );
        let action = d.on_webhook(bad).await;
        assert_eq!(action, DispatchAction::AdmissionRejected);

        // A ticket task is in-flight, so the dispatcher must NOT evict
        // the cache directly. Instead it sets pending_evict, which the
        // ticket task will drain post-cycle.
        let snap = d.cache.snapshot("t1").await;
        // The ticket task may or may not have evicted by the time we
        // observe — accept either pending_evict=true (task still running)
        // or absent (task drained the flag and evicted).
        if let Some(snap) = snap {
            assert!(snap.pending_evict, "expected pending_evict to be set");
        }
    }

    #[tokio::test]
    async fn re_admission_clears_pending_evict() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner::new(count.clone())), work.path());

        let admitted = admitted_for("t1", "InProgress");
        d.cache.observe(&admitted).await;
        d.cache.set_pending_evict("t1").await;

        // Re-admit via the on_webhook path with a passing assignee.
        let _ = d.on_webhook(ticket("t1", "InProgress")).await;

        let snap = d.cache.snapshot("t1").await.expect("cache entry present");
        assert!(!snap.pending_evict, "re-admit should clear pending_evict");
    }

    #[tokio::test]
    async fn admit_for_cold_start_runs_first_cycle_with_cold_start_trigger() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let runner = Arc::new(CountingRunner::new(count.clone()));
        let triggers = runner.triggers();
        let d = dispatcher_with(runner, work.path());

        // Pre-populate the cache so the ticket task can snapshot the
        // ticket on first iteration. In production this is done by
        // `cold_start::orchestrate` before calling `admit_for_cold_start`.
        let admitted = admitted_for("t1", "InProgress");
        d.cache.observe(&admitted).await;

        d.admit_for_cold_start(admitted)
            .await
            .expect("cold start admit");

        // Wait for the runner to record the first invocation.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if *count.lock().unwrap() >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(*count.lock().unwrap(), 1, "first cycle must run");
        let observed = triggers.lock().unwrap().clone();
        assert_eq!(observed, vec![CycleTrigger::ColdStart]);
        assert!(d.tickets().lock().await.contains_key("t1"));
    }
}
