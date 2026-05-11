use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Healthz {
    pub version: String,
    pub uptime_seconds: u64,
    pub configured_repositories: Vec<String>,
    pub api_request_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let h = Healthz {
            version: "0.1.0".into(),
            uptime_seconds: 42,
            configured_repositories: vec!["github.com/x/y".into()],
            api_request_count: 7,
        };
        let s = serde_json::to_string(&h).unwrap();
        assert_eq!(h, serde_json::from_str(&s).unwrap());
    }
}
