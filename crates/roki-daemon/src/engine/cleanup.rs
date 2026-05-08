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
//! emit `failure_unhandled marker=cleanup_fs_error` and propagate as Err so
//! `runtime::run_inner` exits 1.

#![allow(dead_code)]

use std::path::Path;

use uuid::Uuid;

use crate::events::{
    Event, EventWriter, FailureMarker, FailureMetaSer, WorktreeDeleteReason, now_rfc3339,
};

#[derive(Debug)]
pub enum CleanupError {
    /// A `failure_unhandled` event was emitted; the runtime should exit 1.
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

/// Shorthand path. `cycle_id` is synthesized so the structured event has a
/// stable id.
pub async fn delete_immediate(
    ticket_id: &str,
    ghq: &str,
    session_root: &Path,
    events: &mut EventWriter,
) -> Result<(), CleanupError> {
    let cycle_id = Uuid::new_v4();
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
        let _ = events.emit(&Event::FailureUnhandled {
            ts: now_rfc3339(),
            cycle_id: cycle_id.to_string(),
            cycle_kind: "cleanup".into(),
            failure: FailureMetaSer {
                kind: "fs_poison".into(),
                phase: None,
                iter: 0,
                exit_code: err.exit_code(),
                error_text: format!("cleanup wt remove failed: {err}"),
            },
            marker: FailureMarker::CleanupFsError,
        });
        return Err(CleanupError::FsError(std::io::Error::other(format!(
            "{err}"
        ))));
    }
    remove_ticket_dir(session_root, ticket_id, Some(cycle_id), events)
}

/// Post-cycle delete. Called only after a non-shorthand cleanup cycle
/// completes. `cycle_id` is the cleanup cycle's UUID.
pub async fn post_cycle_delete(
    ticket_id: &str,
    ghq: &str,
    session_root: &Path,
    cycle_id: Uuid,
    events: &mut EventWriter,
) -> Result<(), CleanupError> {
    let _ = events.emit(&Event::WorktreeDeleteRequested {
        ts: now_rfc3339(),
        ticket_id: ticket_id.to_string(),
        cycle_id: Some(cycle_id.to_string()),
        reason: WorktreeDeleteReason::CleanupTerminal,
    });
    if let Err(err) = crate::engine::worktree::remove(ghq, ticket_id).await {
        let _ = events.emit(&Event::FailureUnhandled {
            ts: now_rfc3339(),
            cycle_id: cycle_id.to_string(),
            cycle_kind: "cleanup".into(),
            failure: FailureMetaSer {
                kind: "fs_poison".into(),
                phase: None,
                iter: 0,
                exit_code: err.exit_code(),
                error_text: format!("cleanup wt remove failed: {err}"),
            },
            marker: FailureMarker::CleanupFsError,
        });
        return Err(CleanupError::FsError(std::io::Error::other(format!(
            "{err}"
        ))));
    }
    remove_ticket_dir(session_root, ticket_id, Some(cycle_id), events)
}

fn remove_ticket_dir(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Option<Uuid>,
    events: &mut EventWriter,
) -> Result<(), CleanupError> {
    let dir = session_root.join(sanitize_ticket(ticket_id));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            let _ = events.emit(&Event::FailureUnhandled {
                ts: now_rfc3339(),
                cycle_id: cycle_id.map(|c| c.to_string()).unwrap_or_default(),
                cycle_kind: "cleanup".into(),
                failure: FailureMetaSer {
                    kind: "fs_poison".into(),
                    phase: None,
                    iter: 0,
                    exit_code: None,
                    error_text: format!("cleanup remove_dir_all failed: {e}"),
                },
                marker: FailureMarker::CleanupFsError,
            });
            Err(CleanupError::FsError(e))
        }
    }
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
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                temp_env::async_with_vars(
                    [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
                    async {
                        delete_immediate("OPS-1", "github.com/acme/widget", &root, &mut w)
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
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                temp_env::async_with_vars(
                    [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
                    async {
                        delete_immediate("OPS-2", "github.com/acme/widget", &root, &mut w)
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
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                temp_env::async_with_vars(
                    [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
                    async {
                        delete_immediate("OPS-3", "github.com/acme/widget", &root, &mut w)
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

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            temp_env::async_with_vars(
                [("ROKI_WT_ROOT_OVERRIDE", Some(wt_root.to_str().unwrap()))],
                async {
                    delete_immediate("OPS-12", "github.com/acme/widget", &session_root, &mut w)
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
