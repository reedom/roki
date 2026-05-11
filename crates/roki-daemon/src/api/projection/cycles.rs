//! Projection: on-disk `cycle.json` → `CycleSummary` (fr:10 §/tickets/{id}/cycles).
//!
//! The cycle metadata file is written by the runner/cleanup paths under
//! `<session_root>/<ticket_id>/cycle-<uuid>/cycle.json`. This module owns the
//! deserialization shape so the wire schema stays decoupled from any on-disk
//! evolutions.

use std::io;
use std::path::Path;

use roki_api_types::{CycleKind, CycleSummary, CycleTrigger};
use serde::Deserialize;
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;

#[derive(Deserialize)]
struct OnDisk {
    cycle_id: Uuid,
    kind: CycleKind,
    trigger: CycleTrigger,
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    ended_at: Option<OffsetDateTime>,
    #[serde(default)]
    terminal_id: Option<String>,
    #[serde(default)]
    failure_kind: Option<String>,
    #[serde(default)]
    total_visits: u32,
    #[serde(default)]
    states: Vec<String>,
}

/// Read every `cycle-*/cycle.json` under `<session_root>/<ticket_id>/`, sort
/// newest-first, truncate to `window`, and report whether truncation happened.
///
/// A missing ticket directory yields an empty list silently. Any other I/O or
/// parse failure is logged via `tracing::warn!` and the offending entry is
/// skipped so a single corrupt file cannot hide the rest.
pub fn list_cycles(
    session_root: &Path,
    ticket_id: &str,
    window: usize,
) -> (Vec<CycleSummary>, bool) {
    let dir = session_root.join(ticket_id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (vec![], false),
        Err(e) => {
            warn!(path = %dir.display(), error = %e, "list_cycles_read_dir_failed");
            return (vec![], false);
        }
    };
    let mut summaries: Vec<CycleSummary> = entries
        .filter_map(|entry| match entry {
            Ok(e) => Some(e),
            Err(e) => {
                warn!(dir = %dir.display(), error = %e, "list_cycles_dir_entry_failed");
                None
            }
        })
        .filter(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .filter_map(|e| read_on_disk(&e.path().join("cycle.json")).map(parse))
        .collect();
    summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    let truncated = summaries.len() > window;
    summaries.truncate(window);
    (summaries, truncated)
}

/// Return the ordered `states` array recorded for a specific cycle.
///
/// Returns `None` when the file is absent. Logs and returns `None` on read or
/// parse failure so an operator sees the corruption.
pub fn read_cycle_states(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
) -> Option<Vec<String>> {
    let path = session_root
        .join(ticket_id)
        .join(format!("cycle-{cycle_id}"))
        .join("cycle.json");
    read_on_disk(&path).map(|d| d.states)
}

fn read_on_disk(path: &Path) -> Option<OnDisk> {
    let body = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "cycle_json_read_failed");
            return None;
        }
    };
    match serde_json::from_str::<OnDisk>(&body) {
        Ok(d) => Some(d),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "cycle_json_parse_failed");
            None
        }
    }
}

fn parse(d: OnDisk) -> CycleSummary {
    let last_state_id = d.states.last().cloned();
    CycleSummary {
        cycle_id: d.cycle_id,
        kind: d.kind,
        trigger: d.trigger,
        started_at: d.started_at,
        ended_at: d.ended_at,
        terminal_id: d.terminal_id,
        failure_kind: d.failure_kind,
        last_state_id,
        total_visits: d.total_visits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn lists_cycles_descending_by_started_at_and_populates_last_state_id() {
        let dir = TempDir::new().unwrap();
        let ticket = dir.path().join("ENG-1");
        let mut ids = vec![];
        for (i, ts) in ["2026-05-01T00:00:00Z", "2026-05-02T00:00:00Z"]
            .iter()
            .enumerate()
        {
            let id = Uuid::new_v4();
            ids.push(id);
            let cycle = ticket.join(format!("cycle-{id}"));
            std::fs::create_dir_all(&cycle).unwrap();
            let body = format!(
                r#"{{"cycle_id":"{id}","kind":"rule","trigger":"runtime","started_at":"{ts}","total_visits":1,"states":["pre","post{i}"]}}"#
            );
            std::fs::write(cycle.join("cycle.json"), body).unwrap();
        }
        let (cycles, truncated) = list_cycles(dir.path(), "ENG-1", 10);
        assert_eq!(cycles.len(), 2);
        assert!(cycles[0].started_at > cycles[1].started_at);
        assert_eq!(cycles[0].last_state_id.as_deref(), Some("post1"));
        assert_eq!(cycles[1].last_state_id.as_deref(), Some("post0"));
        assert!(!truncated);
    }

    #[test]
    fn last_state_id_is_none_when_states_array_is_empty() {
        let dir = TempDir::new().unwrap();
        let ticket = dir.path().join("ENG-2");
        let id = Uuid::new_v4();
        let cycle = ticket.join(format!("cycle-{id}"));
        std::fs::create_dir_all(&cycle).unwrap();
        let body = format!(
            r#"{{"cycle_id":"{id}","kind":"rule","trigger":"runtime","started_at":"2026-05-01T00:00:00Z","total_visits":0,"states":[]}}"#
        );
        std::fs::write(cycle.join("cycle.json"), body).unwrap();
        let (cycles, _) = list_cycles(dir.path(), "ENG-2", 10);
        assert_eq!(cycles.len(), 1);
        assert!(cycles[0].last_state_id.is_none());
    }

    #[test]
    fn corrupt_cycle_json_is_skipped_and_does_not_hide_siblings() {
        let dir = TempDir::new().unwrap();
        let ticket = dir.path().join("ENG-3");
        let good_id = Uuid::new_v4();
        let good = ticket.join(format!("cycle-{good_id}"));
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(
            good.join("cycle.json"),
            format!(
                r#"{{"cycle_id":"{good_id}","kind":"rule","trigger":"runtime","started_at":"2026-05-01T00:00:00Z","total_visits":1,"states":["s0"]}}"#
            ),
        )
        .unwrap();
        let bad_id = Uuid::new_v4();
        let bad = ticket.join(format!("cycle-{bad_id}"));
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("cycle.json"), "{ this is not json").unwrap();
        let (cycles, _) = list_cycles(dir.path(), "ENG-3", 10);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].cycle_id, good_id);
    }

    #[test]
    fn unknown_kind_or_trigger_is_skipped() {
        let dir = TempDir::new().unwrap();
        let ticket = dir.path().join("ENG-4");
        let id = Uuid::new_v4();
        let cycle = ticket.join(format!("cycle-{id}"));
        std::fs::create_dir_all(&cycle).unwrap();
        std::fs::write(
            cycle.join("cycle.json"),
            format!(
                r#"{{"cycle_id":"{id}","kind":"future_variant","trigger":"runtime","started_at":"2026-05-01T00:00:00Z","total_visits":0,"states":[]}}"#
            ),
        )
        .unwrap();
        let (cycles, _) = list_cycles(dir.path(), "ENG-4", 10);
        assert!(cycles.is_empty());
    }

    #[test]
    fn missing_ticket_dir_returns_empty() {
        let dir = TempDir::new().unwrap();
        let (cycles, truncated) = list_cycles(dir.path(), "MISSING", 10);
        assert!(cycles.is_empty());
        assert!(!truncated);
    }
}
