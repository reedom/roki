//! Projection: on-disk `cycle.json` → `CycleSummary` (fr:10 §/tickets/{id}/cycles).
//!
//! The cycle metadata file is written by the runner/cleanup paths under
//! `<session_root>/<ticket_id>/cycle-<uuid>/cycle.json`. This module owns the
//! deserialization shape so the wire schema stays decoupled from any on-disk
//! evolutions.

use std::path::Path;

use roki_api_types::{CycleKind, CycleSummary, CycleTrigger};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Deserialize)]
struct OnDisk {
    cycle_id: Uuid,
    kind: String,
    trigger: String,
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
pub fn list_cycles(
    session_root: &Path,
    ticket_id: &str,
    window: usize,
) -> (Vec<CycleSummary>, bool) {
    let dir = session_root.join(ticket_id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(_) => return (vec![], false),
    };
    let mut summaries: Vec<CycleSummary> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .filter_map(|e| {
            let path = e.path().join("cycle.json");
            let body = std::fs::read_to_string(&path).ok()?;
            let on_disk: OnDisk = serde_json::from_str(&body).ok()?;
            Some(parse(on_disk))
        })
        .collect();
    summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    let truncated = summaries.len() > window;
    summaries.truncate(window);
    (summaries, truncated)
}

/// Return the ordered `states` array recorded for a specific cycle, or `None`
/// when the cycle directory or its `cycle.json` is missing/unparseable.
pub fn read_cycle_states(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
) -> Option<Vec<String>> {
    let path = session_root
        .join(ticket_id)
        .join(format!("cycle-{cycle_id}"))
        .join("cycle.json");
    let body = std::fs::read_to_string(&path).ok()?;
    let on_disk: OnDisk = serde_json::from_str(&body).ok()?;
    Some(on_disk.states)
}

fn parse(d: OnDisk) -> CycleSummary {
    CycleSummary {
        cycle_id: d.cycle_id,
        kind: match d.kind.as_str() {
            "rule" => CycleKind::Rule,
            "cleanup" => CycleKind::Cleanup,
            _ => CycleKind::Failure,
        },
        trigger: match d.trigger.as_str() {
            "cold_start" => CycleTrigger::ColdStart,
            _ => CycleTrigger::Runtime,
        },
        started_at: d.started_at,
        ended_at: d.ended_at,
        terminal_id: d.terminal_id,
        failure_kind: d.failure_kind,
        total_visits: d.total_visits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn lists_cycles_descending_by_started_at() {
        let dir = TempDir::new().unwrap();
        let ticket = dir.path().join("ENG-1");
        for ts in ["2026-05-01T00:00:00Z", "2026-05-02T00:00:00Z"].iter() {
            let id = Uuid::new_v4();
            let cycle = ticket.join(format!("cycle-{id}"));
            std::fs::create_dir_all(&cycle).unwrap();
            let body = format!(
                r#"{{"cycle_id":"{id}","kind":"rule","trigger":"runtime","started_at":"{ts}","total_visits":0,"states":[]}}"#
            );
            std::fs::write(cycle.join("cycle.json"), body).unwrap();
        }
        let (cycles, truncated) = list_cycles(dir.path(), "ENG-1", 10);
        assert_eq!(cycles.len(), 2);
        assert!(cycles[0].started_at > cycles[1].started_at);
        assert!(!truncated);
    }
}
