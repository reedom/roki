#![allow(dead_code)]

//! Cold-start enumeration + dispatch + orphan reconcile (fr:07 §Cold start).
//!
//! Runs once at every daemon launch before `daemon_ready` is emitted.
//! Walks Linear's paginated `issues` query, populates `DiffCache`, spawns
//! per-ticket cycles via `Dispatcher::admit_for_cold_start`, then deletes
//! orphan session tempdirs. Cycles dispatched here run async on the
//! existing per-ticket task model — cold start does not block on cycle
//! completion.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::admission;
use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::daemon::cache::DiffCache;
use crate::daemon::dispatcher::Dispatcher;
use crate::daemon::orphan::{self, OrphanScan};
use crate::daemon::ticket_task::CycleRunner;
use crate::engine::dispatch::DispatchMode;
use crate::events::{Event, EventWriter, WebhookSkipReason, WebhookSkipSource, now_rfc3339};
use crate::linear::client::MeId;
use crate::linear::graphql::{
    EnumerateRequest, EnumeratedTicket, LinearGraphqlClient, StatusFilter,
};
use crate::linear::ticket::NormalizedTicket;

#[derive(Debug, Default)]
pub struct ColdStartReport {
    pub enumerated: usize,
    pub admitted: usize,
    pub cycles_spawned: usize,
    pub orphans_deleted: usize,
    pub enum_partial: bool,
    pub partial_reason: Option<String>,
    pub partial_error_text: Option<String>,
}

pub struct ColdStart<R: CycleRunner + 'static> {
    pub cfg: Arc<RokiConfig>,
    pub workflow: Arc<WorkflowConfig>,
    pub me: Option<MeId>,
    pub cache: Arc<DiffCache>,
    pub dispatcher: Arc<Dispatcher<R>>,
    pub graphql: Arc<LinearGraphqlClient>,
    pub mode: DispatchMode,
}

/// Compute the status-union narrowing for the GraphQL filter.
///
/// Returns `(union, dropped_entry)` where:
/// - `union` is the set of explicit `when.status` values across every
///   rule and cleanup, when ALL entries have an explicit status.
/// - `dropped_entry = Some(name)` and `union` is empty when ANY cleanup
///   omits `when.status` (per fr:07 step 2). The caller emits
///   `status_filter_dropped` in that case.
pub fn compute_status_union(workflow: &WorkflowConfig) -> (BTreeSet<String>, Option<String>) {
    let mut union: BTreeSet<String> = BTreeSet::new();

    // Rules always have explicit when_status (Rule::when_status is String, validated at load).
    for r in &workflow.rules {
        union.insert(r.when_status.clone());
    }

    // Cleanups may omit when_status. If any does, drop the filter.
    for (idx, c) in workflow.cleanups.iter().enumerate() {
        match &c.when_status {
            Some(s) => {
                union.insert(s.clone());
            }
            None => {
                return (BTreeSet::new(), Some(format!("cleanup[{idx}]")));
            }
        }
    }

    (union, None)
}

impl<R: CycleRunner + 'static> ColdStart<R> {
    pub async fn run(&self, writer: Arc<Mutex<EventWriter>>) -> ColdStartReport {
        let mut report = ColdStartReport::default();

        let assignee_id = self.resolve_assignee_id_string();
        let (status_set, dropped_entry) = compute_status_union(&self.workflow);

        if let Some(name) = dropped_entry {
            let mut w = writer.lock().await;
            let _ = w.emit(&Event::StatusFilterDropped {
                ts: now_rfc3339(),
                entry: name,
                reason: "any-state-rule".into(),
            });
        }

        let states_vec: Vec<&str> = status_set.iter().map(String::as_str).collect();
        let status_filter = if states_vec.is_empty() {
            StatusFilter::None
        } else {
            StatusFilter::Union(&states_vec)
        };

        let page_size = page_size_from_env();

        // Enumerate. Partial failure -> empty admitted set, skip orphan reconcile (§4.6).
        let enumerated = match self
            .graphql
            .enumerate(&EnumerateRequest {
                assignee_id: &assignee_id,
                status_filter,
                page_size,
            })
            .await
        {
            Ok(v) => v,
            Err(e) => {
                report.enum_partial = true;
                report.partial_reason = Some(classify_partial_reason(&e));
                report.partial_error_text = Some(e.to_string());
                Vec::new()
            }
        };
        report.enumerated = enumerated.len();

        // Admission re-eval + cache observe + dispatch.
        let mut keep_ids: HashSet<String> = HashSet::new();
        let me_for_admission = self.me.clone().unwrap_or_else(|| MeId(String::new()));

        for et in enumerated {
            let nt = synth_normalized(&et);
            match admission::accept(&nt, &self.workflow, &me_for_admission) {
                Ok(admitted) => {
                    keep_ids.insert(admitted.ticket.id.clone());
                    report.admitted += 1;

                    let _ = self.cache.observe(&admitted).await;

                    if self
                        .dispatcher
                        .admit_for_cold_start(admitted.clone())
                        .await
                        .is_ok()
                    {
                        report.cycles_spawned += 1;
                    }
                }
                Err(err) => {
                    let mut w = writer.lock().await;
                    let _ = w.emit(&Event::WebhookSkipped {
                        ts: now_rfc3339(),
                        ticket_id: et.id.clone(),
                        reason: classify_webhook_skip_reason(&err),
                        source: Some(WebhookSkipSource::ColdStart),
                    });
                }
            }
        }

        // Orphan reconcile (skip on partial enum per §4.6).
        if report.enum_partial {
            let mut w = writer.lock().await;
            let _ = w.emit(&Event::OrphanReconcileSkipped {
                ts: now_rfc3339(),
                reason: "cold_start_partial".into(),
            });
        } else {
            let scan = OrphanScan {
                session_root: &self.cfg.paths.session_root,
                keep_ids: &keep_ids,
            };
            let orphan_report = orphan::reconcile(scan, writer.clone()).await;
            report.orphans_deleted = orphan_report.deleted.len();
        }

        report
    }

    fn resolve_assignee_id_string(&self) -> String {
        match (&self.workflow.admission.assignee, &self.me) {
            (a, _) if a != "me" => a.clone(),
            (_, Some(MeId(id))) => id.clone(),
            (_, None) => String::new(),
        }
    }
}

fn synth_normalized(et: &EnumeratedTicket) -> NormalizedTicket {
    NormalizedTicket::new_for_cold_start(
        et.id.clone(),
        et.assignee_id.clone(),
        et.state_name.clone(),
        et.label_names.iter().cloned().collect(),
        et.identifier.clone(), // title fallback to identifier
        et.description.clone().unwrap_or_default(),
    )
}

fn page_size_from_env() -> u32 {
    #[cfg(any(test, feature = "test-support"))]
    {
        if let Ok(s) = std::env::var("ROKI_COLD_START_PAGE_SIZE") {
            if let Ok(n) = s.parse::<u32>() {
                if n > 0 {
                    return n;
                }
            }
        }
    }
    crate::linear::graphql::DEFAULT_PAGE_SIZE
}

fn classify_partial_reason(err: &crate::error::LinearEnumerateError) -> String {
    use crate::error::LinearEnumerateError::*;
    match err {
        GraphqlError { .. } => "graphql_error".into(),
        Http { .. } | NonSuccess { .. } | Malformed { .. } | BackoffExhausted { .. } => {
            "linear_unreachable".into()
        }
    }
}

fn classify_webhook_skip_reason(err: &crate::error::AdmissionError) -> WebhookSkipReason {
    use crate::error::AdmissionError;
    match err {
        AdmissionError::AssigneeMismatch { .. } => WebhookSkipReason::AssigneeMismatch,
        AdmissionError::NoRepos => WebhookSkipReason::RepoUnresolvable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::workflow::{AdmissionRepo, AdmissionSection, Cleanup, Rule, WorkflowConfig};
    use crate::engine::outcome::PhaseBody;

    fn workflow_with(
        rules: Vec<Option<&str>>, // None means rule omits when_status (impossible — Rule.when_status is String)
        cleanups: Vec<Option<&str>>,
    ) -> WorkflowConfig {
        let rules = rules
            .into_iter()
            .map(|s| Rule {
                when_status: s.unwrap_or("").to_string(),
                when_labels_has_all: vec![],
                pre: None,
                run: PhaseBody::InlineCmd { cmd: "true".into() },
                post: None,
            })
            .collect();
        let cleanups = cleanups
            .into_iter()
            .map(|s| Cleanup {
                when_status: s.map(String::from),
                when_labels_has_all: vec![],
                pre: None,
                run: None,
                post: None,
            })
            .collect();
        WorkflowConfig {
            admission: AdmissionSection {
                assignee: "me".into(),
            },
            repo: Some(AdmissionRepo {
                ghq: "github.com/example/r".into(),
            }),
            rules,
            cleanups,
            on_failures: vec![],
        }
    }

    #[test]
    fn all_explicit_statuses_form_union() {
        let w = workflow_with(
            vec![Some("Todo"), Some("InProgress"), Some("Todo")],
            vec![Some("Done")],
        );
        let (set, dropped) = compute_status_union(&w);
        assert_eq!(set.len(), 3);
        assert!(set.contains("Todo"));
        assert!(set.contains("InProgress"));
        assert!(set.contains("Done"));
        assert!(dropped.is_none());
    }

    #[test]
    fn cleanup_without_status_drops_filter() {
        let w = workflow_with(vec![Some("Todo")], vec![None]);
        let (set, dropped) = compute_status_union(&w);
        assert!(set.is_empty());
        assert!(dropped.is_some());
        assert!(dropped.unwrap().starts_with("cleanup["));
    }

    #[test]
    fn no_rules_no_cleanups_yields_empty_union_no_drop() {
        let w = workflow_with(vec![], vec![]);
        let (set, dropped) = compute_status_union(&w);
        assert!(set.is_empty());
        assert!(dropped.is_none());
    }
}
