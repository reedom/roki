use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiEscalation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle_id: Option<Uuid>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visit_n: Option<u32>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub error_text: String,
    pub marker: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let e = ApiEscalation {
            ticket_id: Some("ENG-1".into()),
            cycle_id: Some(Uuid::nil()),
            kind: "recursion_bound".into(),
            state_id: Some("post0".into()),
            visit_n: Some(2),
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            error_text: "boom".into(),
            marker: "none".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(e, serde_json::from_str(&s).unwrap());
    }
}
