use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiEvent {
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle_id: Option<Uuid>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsPage {
    pub events: Vec<ApiEvent>,
    pub gap: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_since: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_page_round_trips() {
        let p = EventsPage {
            events: vec![ApiEvent {
                seq: 1,
                ts: OffsetDateTime::from_unix_timestamp(0).unwrap(),
                event: "webhook_received".into(),
                ticket_id: Some("ENG-1".into()),
                cycle_id: None,
                payload: serde_json::json!({"k": "v"}),
            }],
            gap: false,
            next_since: Some(1),
        };
        let json = serde_json::to_string(&p).unwrap();
        let parsed: EventsPage = serde_json::from_str(&json).unwrap();
        assert_eq!(p, parsed);
    }
}
