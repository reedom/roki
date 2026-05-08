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
    RecursionBound,
    CleanupFsError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeDeleteReason {
    CleanupTerminal,
    CleanupShorthand,
}

#[derive(Debug, Serialize)]
pub struct FailureMetaSer {
    pub kind: String,
    pub phase: Option<String>,
    pub iter: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub error_text: String,
}

impl FailureMetaSer {
    pub fn from_meta(meta: &crate::engine::outcome::FailureMeta) -> Self {
        Self {
            kind: meta.kind.as_str().to_string(),
            phase: Some(meta.phase.as_str().to_string()),
            iter: meta.iter,
            exit_code: meta.exit_code,
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
        outcome: Option<String>,
    },
    FailureUnhandled {
        ts: String,
        cycle_id: String,
        cycle_kind: String,
        failure: FailureMetaSer,
        marker: FailureMarker,
    },
    WorktreeDeleteRequested {
        ts: String,
        ticket_id: String,
        cycle_id: Option<String>,
        reason: WorktreeDeleteReason,
    },
}

pub fn events_path(session_root: &Path, ticket_id: &str) -> PathBuf {
    session_root.join(format!("{}.events.jsonl", sanitize_ticket(ticket_id)))
}

/// Sanitize the ticket id for use as a path component. Mirrors
/// `capture::sanitize` so the events file's stem matches the ticket dir name.
fn sanitize_ticket(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
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
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            file: BufWriter::new(file),
            path,
        })
    }

    pub fn emit(&mut self, event: &Event) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.file, event)
            .map_err(std::io::Error::other)?;
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

    #[test]
    fn failure_unhandled_serializes_marker_and_failure() {
        let ev = Event::FailureUnhandled {
            ts: "t".into(),
            cycle_id: "c".into(),
            cycle_kind: "rule".into(),
            failure: FailureMetaSer {
                kind: "stall".into(),
                phase: Some("run".into()),
                iter: 2,
                exit_code: None,
                error_text: "stalled".into(),
            },
            marker: FailureMarker::None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"event\":\"failure_unhandled\""));
        assert!(s.contains("\"marker\":\"none\""));
        assert!(s.contains("\"kind\":\"stall\""));
        assert!(!s.contains("exit_code"), "None exit_code should be omitted");
    }
}
