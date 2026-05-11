use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CycleKind {
    Rule,
    Cleanup,
    Failure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CycleTrigger {
    Runtime,
    ColdStart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketSummary {
    pub ticket_id: String,
    pub repo: String,
    pub status: String,
    pub labels: Vec<String>,
    pub assignee: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_cycle_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub last_event_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketDetail {
    #[serde(flatten)]
    pub summary: TicketSummary,
    pub recent_events: Vec<crate::events::ApiEvent>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleSummary {
    pub cycle_id: Uuid,
    pub kind: CycleKind,
    pub trigger: CycleTrigger,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub ended_at: Option<OffsetDateTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,
    pub total_visits: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_kind_round_trips_lowercase() {
        for k in [CycleKind::Rule, CycleKind::Cleanup, CycleKind::Failure] {
            let s = serde_json::to_string(&k).unwrap();
            let parsed: CycleKind = serde_json::from_str(&s).unwrap();
            assert_eq!(k, parsed);
            assert!(s.chars().all(|c| c.is_lowercase() || c == '"'));
        }
    }

    #[test]
    fn ticket_summary_round_trips() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let s = TicketSummary {
            ticket_id: "ENG-1".into(),
            repo: "github.com/x/y".into(),
            status: "in_progress".into(),
            labels: vec!["urgent".into()],
            assignee: "u1".into(),
            in_flight_cycle_id: Some(Uuid::nil()),
            last_event_at: now,
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: TicketSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(s, parsed);
    }
}
