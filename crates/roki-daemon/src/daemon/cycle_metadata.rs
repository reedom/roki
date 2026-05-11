//! Atomic write of per-cycle metadata (`cycle.json`) for slice 9 HTTP API
//! consumption. fr:10 §GET /api/tickets/{id}/cycles + spec §3.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::engine::outcome::{CycleKind, FailureKind};
use crate::events::now_rfc3339;

fn cycle_json_path(session_root: &Path, ticket_id: &str, cycle_id: Uuid) -> PathBuf {
    session_root
        .join(ticket_id)
        .join(format!("cycle-{cycle_id}"))
        .join("cycle.json")
}

pub fn write_cycle_start(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
    kind: CycleKind,
    trigger: &str,
    states: Vec<String>,
) -> std::io::Result<()> {
    let path = cycle_json_path(session_root, ticket_id, cycle_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = json!({
        "cycle_id": cycle_id.to_string(),
        "ticket_id": ticket_id,
        "kind": cycle_kind_str(kind),
        "trigger": trigger,
        "started_at": now_rfc3339(),
        "ended_at": null,
        "terminal_id": null,
        "failure_kind": null,
        "total_visits": 0,
        "states": states,
    });
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&body)?)?;
    std::fs::rename(tmp, path)
}

pub struct CycleEndPayload {
    pub terminal_id: Option<String>,
    pub failure_kind: Option<FailureKind>,
    pub total_visits: u32,
}

pub fn write_cycle_end(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
    result: CycleEndPayload,
) -> std::io::Result<()> {
    let path = cycle_json_path(session_root, ticket_id, cycle_id);
    let mut body: Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::Object(Map::new())),
        Err(_) => Value::Object(Map::new()),
    };
    if let Value::Object(m) = &mut body {
        m.insert("ended_at".into(), Value::String(now_rfc3339()));
        m.insert(
            "terminal_id".into(),
            result.terminal_id.map(Value::String).unwrap_or(Value::Null),
        );
        m.insert(
            "failure_kind".into(),
            result
                .failure_kind
                .map(|k| Value::String(failure_kind_str(k).into()))
                .unwrap_or(Value::Null),
        );
        m.insert("total_visits".into(), Value::from(result.total_visits));
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&body)?)?;
    std::fs::rename(tmp, path)
}

fn cycle_kind_str(k: CycleKind) -> &'static str {
    match k {
        CycleKind::Rule => "rule",
        CycleKind::Cleanup => "cleanup",
        CycleKind::Failure => "failure",
    }
}

fn failure_kind_str(k: FailureKind) -> &'static str {
    match k {
        FailureKind::ProcessCrash => "process_crash",
        FailureKind::Unparseable => "unparseable",
        FailureKind::SchemaDrift => "schema_drift",
        FailureKind::FsPoison => "fs_poison",
        FailureKind::Stall => "stall",
        FailureKind::RecursionBound => "recursion_bound",
        FailureKind::TemplateError => "template_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn cycle_json_round_trip_through_start_and_end() {
        let dir = tempdir().unwrap();
        let cycle = Uuid::new_v4();
        write_cycle_start(
            dir.path(),
            "ENG-1",
            cycle,
            CycleKind::Rule,
            "runtime",
            vec!["a".into()],
        )
        .unwrap();
        let mid: Value = serde_json::from_slice(
            &std::fs::read(
                dir.path()
                    .join("ENG-1")
                    .join(format!("cycle-{cycle}"))
                    .join("cycle.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(mid.get("ended_at").unwrap().is_null());
        assert_eq!(mid.get("kind").unwrap(), "rule");
        assert_eq!(mid.get("trigger").unwrap(), "runtime");
        write_cycle_end(
            dir.path(),
            "ENG-1",
            cycle,
            CycleEndPayload {
                terminal_id: Some("__success__".into()),
                failure_kind: None,
                total_visits: 1,
            },
        )
        .unwrap();
        let end: Value = serde_json::from_slice(
            &std::fs::read(
                dir.path()
                    .join("ENG-1")
                    .join(format!("cycle-{cycle}"))
                    .join("cycle.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(end["terminal_id"], Value::String("__success__".into()));
        assert!(!end["ended_at"].is_null());
        assert_eq!(end["total_visits"], Value::from(1u32));
    }
}
