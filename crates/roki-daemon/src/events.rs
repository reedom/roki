//! Structured event JSONL writer.
//!
//! One file per ticket at `<session_root>/<ticket-id>.events.jsonl` (sibling
//! of the ticket dir, not a child — survives cleanup-cycle deletion).
//! Append-only NDJSON; one event per line; flush after each line.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureMarker {
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeDeleteReason {
    CleanupTerminal,
    CleanupShorthand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookSkipReason {
    NoDiff,
    SignatureInvalid,
    AssigneeMismatch,
    RepoUnresolvable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownSignal {
    Sigint,
    Sigterm,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebhookSkipSource {
    Webhook,
    ColdStart,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionTempdirDeleteReason {
    Cleanup,
    Orphan,
}

#[derive(Debug, Serialize)]
pub struct FailureMetaSer {
    pub kind: String,
    /// Slice 8: state-machine identifier of the state that emitted the
    /// failure. `None` for daemon-internal failures with no associated
    /// state (e.g. orphan-reconcile fs error). Replaces the legacy `phase`
    /// field; the legacy phase names ("pre", "run", "post") flow through
    /// here via `from_meta` until the legacy cycle driver is removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_id: Option<String>,
    /// Slice 8: per-state visit counter. Defaults to 0 for legacy callers
    /// that produced a `FailureMeta` without a visit notion; `from_meta`
    /// maps the legacy `iter` field through.
    pub visit_n: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub error_text: String,
}

impl FailureMetaSer {
    /// Slice 8: build from the canonical state-machine failure metadata.
    pub fn from_state_metadata(meta: &crate::engine::cycle_state::FailureMetadata) -> Self {
        Self {
            kind: meta.kind.as_str().to_string(),
            state_id: Some(meta.state_id.clone()),
            visit_n: meta.visit_n,
            exit_code: None,
            error_text: meta.error_text.clone(),
        }
    }

    /// Build from the legacy meta surfaced by the per-ticket task in
    /// `CycleResult::Failed`. Used by `daemon::dispatcher` when a runtime
    /// teardown path needs an event entry for an already-emitted failure.
    pub fn from_legacy(meta: &crate::daemon::real_runner::LegacyFailureMeta) -> Self {
        Self {
            kind: meta.kind.as_str().to_string(),
            state_id: Some(meta.state_id.clone()),
            visit_n: meta.visit_n,
            exit_code: None,
            error_text: meta.error_text.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    CycleCompleted {
        ts: String,
        cycle_id: String,
        cycle_kind: String,
        iters: u32,
        /// Slice 8: state-machine terminal id reached by this cycle.
        /// Absent for the cleanup-shorthand path where no cycle ran.
        #[serde(skip_serializing_if = "Option::is_none")]
        terminal_id: Option<String>,
        /// Slice 8: terminal outcome label (or directive-supplied override).
        outcome: Option<String>,
    },
    FailureUnhandled {
        ts: String,
        cycle_id: String,
        cycle_kind: String,
        failure: FailureMetaSer,
        marker: FailureMarker,
    },
    EscalationAdded {
        ts: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ticket_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cycle_id: Option<String>,
        failure: FailureMetaSer,
    },
    WorktreeDeleteRequested {
        ts: String,
        ticket_id: String,
        cycle_id: Option<String>,
        reason: WorktreeDeleteReason,
    },
    DaemonStarted {
        ts: String,
        config_path: String,
        schema_version: u32,
    },
    DaemonReady {
        ts: String,
        webhook_bind_addr: String,
    },
    DaemonShutdownBegan {
        ts: String,
        signal: ShutdownSignal,
        in_flight: usize,
    },
    DaemonShutdownCompleted {
        ts: String,
        drained: usize,
        aborted: usize,
    },
    ShutdownWindowExceeded {
        ts: String,
        aborted: usize,
        aborted_ticket_ids: Vec<String>,
    },
    WebhookSkipped {
        ts: String,
        ticket_id: String,
        reason: WebhookSkipReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<WebhookSkipSource>,
    },
    ColdStartBegan {
        ts: String,
        roki_toml_path: String,
        workflow_toml_path: String,
    },
    ColdStartCompleted {
        ts: String,
        enumerated: usize,
        admitted: usize,
        cycles_spawned: usize,
        orphans_deleted: usize,
        enum_partial: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial_error_text: Option<String>,
    },
    OrphanReconcileSkipped {
        ts: String,
        reason: String,
    },
    StatusFilterDropped {
        ts: String,
        entry: String,
        reason: String,
    },
    LinearBackoffApplied {
        ts: String,
        backoff_seconds: u64,
    },
    SessionTempdirDeleted {
        ts: String,
        ticket_id: String,
        path: String,
        reason: SessionTempdirDeleteReason,
    },
}

pub fn events_path(session_root: &Path, ticket_id: &str) -> PathBuf {
    session_root.join(format!("{}.events.jsonl", sanitize_ticket(ticket_id)))
}

/// Sanitize the ticket id for use as a path component. Mirrors
/// `capture::sanitize` so the events file's stem matches the ticket dir name.
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

pub struct EventWriter {
    file: BufWriter<File>,
    path: PathBuf,
}

impl EventWriter {
    pub fn open(session_root: &Path, ticket_id: &str) -> std::io::Result<Self> {
        let path = events_path(session_root, ticket_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            file: BufWriter::new(file),
            path,
        })
    }

    pub fn emit(&mut self, event: &Event) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.file, event).map_err(std::io::Error::other)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn now_rfc3339() -> String {
    use time::OffsetDateTime;
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_path_is_sibling_of_ticket_dir() {
        let root = Path::new("/tmp/sessions");
        let p = events_path(root, "OPS-123");
        assert_eq!(p, Path::new("/tmp/sessions/OPS-123.events.jsonl"));
    }

    #[test]
    fn event_writer_appends_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut w = EventWriter::open(root, "OPS-7").unwrap();
        w.emit(&Event::CycleCompleted {
            ts: "2026-05-08T00:00:00Z".into(),
            cycle_id: uuid::Uuid::nil().to_string(),
            cycle_kind: "rule".into(),
            iters: 1,
            terminal_id: Some("__success__".into()),
            outcome: Some("success".into()),
        })
        .unwrap();
        w.emit(&Event::WorktreeDeleteRequested {
            ts: "2026-05-08T00:00:01Z".into(),
            ticket_id: "OPS-7".into(),
            cycle_id: None,
            reason: WorktreeDeleteReason::CleanupShorthand,
        })
        .unwrap();
        drop(w);

        let path = events_path(root, "OPS-7");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"cycle_completed\""));
        assert!(lines[0].contains("\"cycle_kind\":\"rule\""));
        assert!(lines[1].contains("\"event\":\"worktree_delete_requested\""));
        assert!(lines[1].contains("\"reason\":\"cleanup_shorthand\""));
    }

    #[test]
    fn event_writer_creates_file_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let _ = EventWriter::open(root, "OPS-9").unwrap();
        let p = events_path(root, "OPS-9");
        assert!(p.exists());
    }

    #[test]
    fn ticket_id_with_special_chars_sanitized() {
        let p = events_path(Path::new("/r"), "team/abc#1");
        assert_eq!(p, Path::new("/r/team_abc_1.events.jsonl"));
    }

    use serde_json::Value;

    #[test]
    fn daemon_started_serializes_with_event_tag() {
        let ev = Event::DaemonStarted {
            ts: "2026-05-08T00:00:00Z".into(),
            config_path: "/tmp/roki.toml".into(),
            schema_version: 1,
        };
        let v: Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["event"], "daemon_started");
        assert_eq!(v["config_path"], "/tmp/roki.toml");
        assert_eq!(v["schema_version"], 1);
    }

    #[test]
    fn webhook_skipped_no_diff_serializes() {
        let ev = Event::WebhookSkipped {
            ts: "2026-05-08T00:00:00Z".into(),
            ticket_id: "ENG-1".into(),
            reason: WebhookSkipReason::NoDiff,
            source: None,
        };
        let v: Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["event"], "webhook_skipped");
        assert_eq!(v["reason"], "no_diff");
        assert_eq!(v["ticket_id"], "ENG-1");
    }

    #[test]
    fn cold_start_completed_serializes_partial_fields_when_present() {
        let e = Event::ColdStartCompleted {
            ts: "2026-05-09T00:00:00Z".into(),
            enumerated: 5,
            admitted: 3,
            cycles_spawned: 3,
            orphans_deleted: 0,
            enum_partial: true,
            partial_reason: Some("linear_unreachable".into()),
            partial_error_text: Some("timeout".into()),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["event"], "cold_start_completed");
        assert_eq!(v["enum_partial"], true);
        assert_eq!(v["partial_reason"], "linear_unreachable");
    }

    #[test]
    fn cold_start_completed_omits_partial_fields_on_success() {
        let e = Event::ColdStartCompleted {
            ts: "2026-05-09T00:00:00Z".into(),
            enumerated: 5,
            admitted: 5,
            cycles_spawned: 5,
            orphans_deleted: 2,
            enum_partial: false,
            partial_reason: None,
            partial_error_text: None,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("partial_reason").is_none());
    }

    #[test]
    fn webhook_skipped_omits_source_when_none() {
        let e = Event::WebhookSkipped {
            ts: "2026-05-09T00:00:00Z".into(),
            ticket_id: "t1".into(),
            reason: WebhookSkipReason::AssigneeMismatch,
            source: None,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("source").is_none());
    }

    #[test]
    fn webhook_skipped_with_cold_start_source_serializes_field() {
        let e = Event::WebhookSkipped {
            ts: "2026-05-09T00:00:00Z".into(),
            ticket_id: "t1".into(),
            reason: WebhookSkipReason::AssigneeMismatch,
            source: Some(WebhookSkipSource::ColdStart),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["source"], "cold_start");
    }

    #[test]
    fn shutdown_window_exceeded_carries_aborted_ids() {
        let ev = Event::ShutdownWindowExceeded {
            ts: "2026-05-08T00:00:00Z".into(),
            aborted: 2,
            aborted_ticket_ids: vec!["ENG-1".into(), "ENG-2".into()],
        };
        let v: Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["event"], "shutdown_window_exceeded");
        assert_eq!(v["aborted"], 2);
        assert_eq!(v["aborted_ticket_ids"][1], "ENG-2");
    }

    #[test]
    fn failure_unhandled_serializes_marker_and_failure() {
        let ev = Event::FailureUnhandled {
            ts: "t".into(),
            cycle_id: "c".into(),
            cycle_kind: "rule".into(),
            failure: FailureMetaSer {
                kind: "stall".into(),
                state_id: Some("run".into()),
                visit_n: 2,
                exit_code: None,
                error_text: "stalled".into(),
            },
            marker: FailureMarker::None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"event\":\"failure_unhandled\""));
        assert!(s.contains("\"marker\":\"none\""));
        assert!(s.contains("\"kind\":\"stall\""));
        assert!(s.contains("\"state_id\":\"run\""));
        assert!(s.contains("\"visit_n\":2"));
        assert!(!s.contains("\"phase\""), "phase field must be gone");
        assert!(!s.contains("exit_code"), "None exit_code should be omitted");
    }

    #[test]
    fn escalation_added_serializes_cycle_bound_entry() {
        let ev = Event::EscalationAdded {
            ts: "2026-05-09T12:34:56Z".to_string(),
            ticket_id: Some("TEAM-1".to_string()),
            cycle_id: Some("00000000-0000-0000-0000-000000000001".to_string()),
            failure: FailureMetaSer {
                kind: "fs_poison".to_string(),
                state_id: Some("post".to_string()),
                visit_n: 0,
                exit_code: None,
                error_text: "cleanup remove_dir_all failed".to_string(),
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"event\":\"escalation_added\""), "{s}");
        assert!(s.contains("\"ticket_id\":\"TEAM-1\""), "{s}");
        assert!(
            s.contains("\"cycle_id\":\"00000000-0000-0000-0000-000000000001\""),
            "{s}"
        );
        assert!(s.contains("\"kind\":\"fs_poison\""), "{s}");
    }

    #[test]
    fn escalation_added_omits_cycle_fields_for_daemon_entry() {
        let ev = Event::EscalationAdded {
            ts: "2026-05-09T12:34:56Z".to_string(),
            ticket_id: None,
            cycle_id: None,
            failure: FailureMetaSer {
                kind: "fs_poison".to_string(),
                state_id: None,
                visit_n: 0,
                exit_code: None,
                error_text: "orphan reconcile failed".to_string(),
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"event\":\"escalation_added\""), "{s}");
        assert!(
            !s.contains("\"ticket_id\""),
            "ticket_id must be elided: {s}"
        );
        assert!(!s.contains("\"cycle_id\""), "cycle_id must be elided: {s}");
        assert!(!s.contains("\"state_id\""), "state_id must be elided: {s}");
        assert!(!s.contains("\"phase\""), "phase must never appear: {s}");
    }
}
