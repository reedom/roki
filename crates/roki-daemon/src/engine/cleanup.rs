//! Cleanup cycle deletion logic.
//!
//! Two entry points:
//! - `delete_immediate`: shorthand path. Emits `cycle_completed kind=cleanup
//!   iters=0`, emits `worktree_delete_requested reason=cleanup_shorthand`,
//!   removes `<session_root>/<ticket-id>/`. Used when the matched
//!   `[[cleanup]]` entry has no phases.
//! - `post_cycle_delete`: called after a non-shorthand cleanup cycle
//!   completes. Emits `worktree_delete_requested reason=cleanup_terminal`,
//!   removes `<session_root>/<ticket-id>/`.
//!
//! Both routes treat `NotFound` on `<ticket-id>/` as success. Other fs errors
//! push to the escalation queue (fr:06 §Escalation queue) and propagate as Err
//! so the ticket task tears down the cycle without `[[on_failure]]` routing.

#![allow(dead_code)]

use std::path::Path;

use uuid::Uuid;

use crate::engine::outcome::{FailureKind, PhaseKind};
use crate::events::{Event, EventWriter, WorktreeDeleteReason, now_rfc3339};

#[derive(Debug)]
pub enum CleanupError {
    /// An fs error occurred; the escalation queue was pushed and the ticket
    /// task should tear down the cycle without `[[on_failure]]` routing.
    FsError(std::io::Error),
}

impl std::fmt::Display for CleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CleanupError::FsError(e) => write!(f, "cleanup fs error: {e}"),
        }
    }
}
impl std::error::Error for CleanupError {}

/// Shorthand path. `cycle_id` is provided by the caller for a stable id.
pub async fn delete_immediate(
    ticket_id: &str,
    ghq: &str,
    session_root: &Path,
    cycle_id: Uuid,
    events: &mut EventWriter,
    escalation: &crate::escalation::EscalationQueue,
) -> Result<(), CleanupError> {
    let _ = events.emit(&Event::CycleCompleted {
        ts: now_rfc3339(),
        cycle_id: cycle_id.to_string(),
        cycle_kind: "cleanup".into(),
        iters: 0,
        outcome: None,
    });
    let _ = events.emit(&Event::WorktreeDeleteRequested {
        ts: now_rfc3339(),
        ticket_id: ticket_id.to_string(),
        cycle_id: Some(cycle_id.to_string()),
        reason: WorktreeDeleteReason::CleanupShorthand,
    });
    if let Err(err) = crate::engine::worktree::remove(ghq, ticket_id).await {
        return Err(emit_wt_remove_error(escalation, ticket_id, cycle_id, &err).await);
    }
    remove_ticket_dir(session_root, ticket_id, Some(cycle_id), escalation).await
}

/// Post-cycle delete. Called only after a non-shorthand cleanup cycle
/// completes. `cycle_id` is the cleanup cycle's UUID.
pub async fn post_cycle_delete(
    ticket_id: &str,
    ghq: &str,
    session_root: &Path,
    cycle_id: Uuid,
    events: &mut EventWriter,
    escalation: &crate::escalation::EscalationQueue,
) -> Result<(), CleanupError> {
    let _ = events.emit(&Event::WorktreeDeleteRequested {
        ts: now_rfc3339(),
        ticket_id: ticket_id.to_string(),
        cycle_id: Some(cycle_id.to_string()),
        reason: WorktreeDeleteReason::CleanupTerminal,
    });
    if let Err(err) = crate::engine::worktree::remove(ghq, ticket_id).await {
        return Err(emit_wt_remove_error(escalation, ticket_id, cycle_id, &err).await);
    }
    remove_ticket_dir(session_root, ticket_id, Some(cycle_id), escalation).await
}

async fn remove_ticket_dir(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Option<Uuid>,
    escalation: &crate::escalation::EscalationQueue,
) -> Result<(), CleanupError> {
    let dir = session_root.join(sanitize_ticket(ticket_id));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            let err_text = format!("cleanup remove_dir_all failed: {e}");
            if let Some(cid) = cycle_id {
                escalation
                    .push_cycle(
                        ticket_id.to_string(),
                        cid,
                        FailureKind::FsPoison,
                        PhaseKind::Post,
                        err_text,
                    )
                    .await;
            } else {
                escalation
                    .push_daemon(FailureKind::FsPoison, err_text)
                    .await;
            }
            Err(CleanupError::FsError(e))
        }
    }
}

/// Push a cycle escalation entry for a `wt remove` failure during cleanup and
/// return the `CleanupError`. Caller is expected to early-return with the result.
async fn emit_wt_remove_error(
    escalation: &crate::escalation::EscalationQueue,
    ticket_id: &str,
    cycle_id: Uuid,
    err: &crate::engine::worktree::WorktreeError,
) -> CleanupError {
    let err_text = format!("cleanup wt remove failed: {err}");
    escalation
        .push_cycle(
            ticket_id.to_string(),
            cycle_id,
            FailureKind::FsPoison,
            PhaseKind::Post,
            err_text,
        )
        .await;
    CleanupError::FsError(std::io::Error::other(err.to_string()))
}

fn sanitize_ticket(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn writer_for(dir: &std::path::Path) -> Arc<Mutex<EventWriter>> {
        let w = EventWriter::open(dir, "_daemon").expect("open");
        Arc::new(Mutex::new(w))
    }

    fn queue_for(dir: &std::path::Path) -> Arc<crate::escalation::EscalationQueue> {
        crate::escalation::EscalationQueue::new(64, writer_for(dir))
    }

    #[test]
    fn delete_immediate_removes_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        let root = tmp.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&wt_root).unwrap();
        let dir = root.join("OPS-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.txt"), "hi").unwrap();

        let mut w = EventWriter::open(&root, "OPS-1").unwrap();
        let q = queue_for(&root);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                temp_env::async_with_vars(
                    [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
                    async {
                        delete_immediate(
                            "OPS-1",
                            "github.com/acme/widget",
                            &root,
                            Uuid::new_v4(),
                            &mut w,
                            &q,
                        )
                        .await
                        .unwrap();
                    },
                )
                .await
            });
        assert!(!dir.exists());
    }

    #[test]
    fn delete_immediate_succeeds_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        let root = tmp.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&wt_root).unwrap();
        let mut w = EventWriter::open(&root, "OPS-2").unwrap();
        let q = queue_for(&root);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                temp_env::async_with_vars(
                    [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
                    async {
                        delete_immediate(
                            "OPS-2",
                            "github.com/acme/widget",
                            &root,
                            Uuid::new_v4(),
                            &mut w,
                            &q,
                        )
                        .await
                        .unwrap();
                    },
                )
                .await
            });
    }

    #[test]
    fn delete_immediate_emits_two_events() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        let root = tmp.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&wt_root).unwrap();
        let mut w = EventWriter::open(&root, "OPS-3").unwrap();
        let q = queue_for(&root);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                temp_env::async_with_vars(
                    [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
                    async {
                        delete_immediate(
                            "OPS-3",
                            "github.com/acme/widget",
                            &root,
                            Uuid::new_v4(),
                            &mut w,
                            &q,
                        )
                        .await
                        .unwrap();
                    },
                )
                .await
            });
        drop(w);

        let body = std::fs::read_to_string(crate::events::events_path(&root, "OPS-3")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"cycle_completed\""));
        assert!(lines[0].contains("\"cycle_kind\":\"cleanup\""));
        assert!(lines[0].contains("\"iters\":0"));
        assert!(lines[1].contains("\"event\":\"worktree_delete_requested\""));
        assert!(lines[1].contains("\"reason\":\"cleanup_shorthand\""));
    }

    #[test]
    fn post_cycle_delete_emits_one_event_then_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        let root = tmp.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&wt_root).unwrap();
        let dir = root.join("OPS-4");
        std::fs::create_dir_all(&dir).unwrap();

        let cycle_id = Uuid::new_v4();
        let mut w = EventWriter::open(&root, "OPS-4").unwrap();
        let q = queue_for(&root);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                temp_env::async_with_vars(
                    [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
                    async {
                        post_cycle_delete(
                            "OPS-4",
                            "github.com/acme/widget",
                            &root,
                            cycle_id,
                            &mut w,
                            &q,
                        )
                        .await
                        .unwrap();
                    },
                )
                .await
            });
        drop(w);

        assert!(!dir.exists());
        let body = std::fs::read_to_string(crate::events::events_path(&root, "OPS-4")).unwrap();
        assert!(body.contains("\"reason\":\"cleanup_terminal\""));
        assert!(body.contains(&cycle_id.to_string()));
    }

    #[test]
    fn delete_immediate_removes_worktree_then_session_dir() {
        // Single-threaded — env muts are local.
        let tmp = tempfile::tempdir().unwrap();
        let wt_root = tmp.path().join("wts");
        let session_root = tmp.path().join("sessions");
        std::fs::create_dir_all(wt_root.join("OPS-12")).unwrap();
        std::fs::create_dir_all(session_root.join("OPS-12")).unwrap();
        std::fs::write(session_root.join("OPS-12").join("data"), "x").unwrap();

        let mut w = EventWriter::open(&session_root, "OPS-12").unwrap();
        let q = queue_for(&session_root);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            temp_env::async_with_vars(
                [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
                async {
                    delete_immediate(
                        "OPS-12",
                        "github.com/acme/widget",
                        &session_root,
                        Uuid::new_v4(),
                        &mut w,
                        &q,
                    )
                    .await
                    .unwrap();
                },
            )
            .await
        });

        assert!(!wt_root.join("OPS-12").exists(), "worktree must be removed");
        assert!(
            !session_root.join("OPS-12").exists(),
            "session dir must be removed"
        );
    }
}
