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
                return DispatchAction::AdmissionRejected;
            }
        };

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
        let (tx, rx) = mpsc::channel::<DispatchMsg>(1);
        let tx_self = tx.clone();
        let ticket_id = admitted.ticket.id.clone();
        let cache = self.cache.clone();
        let wf = self.workflow.clone();
        let cfg = self.cfg.clone();
        let mode = self.mode;
        let runner = self.runner.clone();
        let session_root = self.cfg.paths.session_root.clone();

        let join = tokio::spawn(async move {
            crate::daemon::ticket_task::run_ticket_task(
                ticket_id,
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

        // Push the first webhook into the inbox before inserting the
        // handle so the task immediately has something to do.
        let _ = tx.try_send(DispatchMsg::Webhook(admitted.clone()));
        map.insert(admitted.ticket.id.clone(), TicketHandle { inbox: tx, join });
    }

    /// Cold-start admission entry point. Stub for Task 5; real impl lands
    /// in Task 6 (spawns a per-ticket task with `CycleTrigger::ColdStart`
    /// for the first cycle).
    pub async fn admit_for_cold_start(&self, _admitted: AdmittedTicket) -> Result<(), ()> {
        Ok(())
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
    use crate::engine::dispatch::DispatchTarget;
    use crate::engine::outcome::{CycleKind, PhaseBody};
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    struct CountingRunner(Arc<StdMutex<u32>>);

    #[async_trait::async_trait]
    impl CycleRunner for CountingRunner {
        async fn run_cycle(
            &self,
            _a: &AdmittedTicket,
            _t: DispatchTarget<'_>,
            _id: uuid::Uuid,
        ) -> CycleResult {
            *self.0.lock().unwrap() += 1;
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
        let d = dispatcher_with(Arc::new(CountingRunner(count.clone())), work.path());
        let action = d.on_webhook(ticket("t1", "InProgress")).await;
        assert_eq!(action, DispatchAction::Spawned);
        assert!(d.tickets().lock().await.contains_key("t1"));
    }

    #[tokio::test]
    async fn duplicate_unchanged_triple_is_skipped() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner(count.clone())), work.path());
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
        let d = dispatcher_with(Arc::new(CountingRunner(count.clone())), work.path());
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
        let d = dispatcher_no_repos(Arc::new(CountingRunner(count.clone())), work.path());
        let action = d.on_webhook(ticket("t1", "InProgress")).await;
        assert_eq!(action, DispatchAction::AdmissionRejected);
        assert!(d.tickets().lock().await.is_empty());

        let events_path = work.path().join("_daemon.events.jsonl");
        let body = std::fs::read_to_string(&events_path).unwrap();
        let line = body.lines().last().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["reason"], "repo_unresolvable");
    }
}
