//! Plain-data types crossing the [`Store`](crate::Store) boundary.
//!
//! Kept deliberately free of `rusqlite` types so the trait stays
//! implementable by an in-memory fake.

use serde::{Deserialize, Serialize};

pub type UnixMillis = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleKind {
    Rule,
    Cleanup,
    Failure,
}

impl CycleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CycleKind::Rule => "rule",
            CycleKind::Cleanup => "cleanup",
            CycleKind::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleOutcome {
    Success,
    Failure,
    NoAction,
    Cancelled,
}

impl CycleOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            CycleOutcome::Success => "success",
            CycleOutcome::Failure => "failure",
            CycleOutcome::NoAction => "no_action",
            CycleOutcome::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub id: String,
    pub repo: String,
    pub admitted_at: UnixMillis,
    pub evicted_at: Option<UnixMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    pub id: i64,
    pub ticket_id: String,
    pub kind: CycleKind,
    pub entry_name: String,
    pub started_at: UnixMillis,
    pub ended_at: Option<UnixMillis>,
    pub outcome: Option<CycleOutcome>,
    pub current_state: Option<String>,
    pub iter: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCycle {
    pub ticket_id: String,
    pub kind: CycleKind,
    pub entry_name: String,
    pub started_at: UnixMillis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateVisit {
    pub cycle_id: i64,
    pub state_id: String,
    pub visits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub seq: i64,
    pub ticket_id: String,
    pub cycle_id: Option<i64>,
    pub ts: UnixMillis,
    pub kind: String,
    /// Structured payload. Stored as JSON text; surfaced to callers as
    /// already-parsed [`serde_json::Value`] so hot readers (TUI tail) can
    /// re-serialize selectively rather than re-parsing per row.
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub ticket_id: String,
    pub cycle_id: Option<i64>,
    pub ts: UnixMillis,
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubprocessRun {
    pub cycle_id: i64,
    pub state_id: String,
    pub visit: u32,
    pub started_at: UnixMillis,
    pub ended_at: Option<UnixMillis>,
    pub exit_code: Option<i32>,
    /// Relative path under `session_root` pointing to the FS-backed capture
    /// directory. The store never reads or writes this dir; it only records
    /// the pointer for orphan-reconcile to cross-check at cold start.
    pub capture_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    pub id: i64,
    pub ticket_id: Option<String>,
    pub reason: String,
    pub created_at: UnixMillis,
    pub ack_at: Option<UnixMillis>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEscalation {
    pub ticket_id: Option<String>,
    pub reason: String,
    pub created_at: UnixMillis,
}
