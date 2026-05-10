use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshAck {
    pub coalesced: bool,
    #[serde(default, with = "time::serde::rfc3339::option", skip_serializing_if = "Option::is_none")]
    pub earliest_fire_at: Option<OffsetDateTime>,
    pub backoff_active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_and_without_earliest_fire_at() {
        for earliest in [None, Some(OffsetDateTime::from_unix_timestamp(1).unwrap())] {
            let r = RefreshAck {
                coalesced: true,
                earliest_fire_at: earliest,
                backoff_active: false,
            };
            let s = serde_json::to_string(&r).unwrap();
            let parsed: RefreshAck = serde_json::from_str(&s).unwrap();
            assert_eq!(r, parsed);
        }
    }
}
