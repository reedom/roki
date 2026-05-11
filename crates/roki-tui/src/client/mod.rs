//! Thin reqwest::Client wrapper. Every JSON response runs through
//! `sanitize::clean_json` before reaching the model. Every text-stream
//! response runs through `ansi_strip` + `control_strip`.

pub mod cycles;
pub mod escalations;
pub mod events;
pub mod refresh;
pub mod tickets;

use std::time::Duration;

use reqwest::Url;
use thiserror::Error;

use crate::sanitize;

#[derive(Debug, Clone)]
pub struct ApiClient {
    base: Url,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        let base = Url::parse(base_url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .pool_idle_timeout(Some(Duration::from_secs(120)))
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .build()?;
        Ok(Self { base, http })
    }

    pub(crate) fn url(&self, path: &str) -> Result<Url, ClientError> {
        self.base
            .join(path)
            .map_err(|e| ClientError::InvalidUrl(e.to_string()))
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("invalid_url: {0}")]
    InvalidUrl(String),
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),
    #[error("invalid_utf8 in text response")]
    InvalidUtf8,
}

pub(crate) async fn get_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: Url,
) -> Result<T, ClientError> {
    let resp = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(ClientError::Http {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).to_string(),
        });
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
    sanitize::clean_json(&mut value);
    let parsed: T = serde_json::from_value(value)?;
    Ok(parsed)
}

pub(crate) async fn get_text(http: &reqwest::Client, url: Url) -> Result<String, ClientError> {
    let resp = http.get(url).send().await?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(ClientError::Http {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).to_string(),
        });
    }
    let raw = std::str::from_utf8(&bytes).map_err(|_| ClientError::InvalidUtf8)?;
    Ok(crate::sanitize::control_strip(
        &crate::sanitize::ansi_strip(raw),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use roki_api_types::{
        ApiEscalation, ApiEvent, CycleSummary, EventsPage, RefreshAck, TicketDetail, TicketSummary,
    };
    use time::OffsetDateTime;
    use uuid::Uuid;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ticket(id: &str) -> TicketSummary {
        TicketSummary {
            ticket_id: id.into(),
            repo: "github.com/x/y".into(),
            status: "in_progress".into(),
            labels: vec!["urgent".into()],
            assignee: "u".into(),
            in_flight_cycle_id: None,
            last_event_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        }
    }

    #[tokio::test]
    async fn fetch_tickets_ok() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tickets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![ticket("ENG-1")]))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let got = c.fetch_tickets().await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].ticket_id, "ENG-1");
    }

    #[tokio::test]
    async fn fetch_tickets_http_404() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tickets"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let err = c.fetch_tickets().await.unwrap_err();
        match err {
            ClientError::Http { status: 404, .. } => {}
            other => panic!("wrong err: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_events_since_appends_query() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .and(query_param("since", "42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(EventsPage {
                events: vec![],
                gap: false,
                next_since: Some(42),
            }))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let page = c.fetch_events_since(Some(42)).await.unwrap();
        assert_eq!(page.next_since, Some(42));
    }

    #[tokio::test]
    async fn fetch_visit_stdout_strips_ansi() {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/api/tickets/ENG-1/cycles/00000000-0000-0000-0000-000000000000/visits/1/post0/stdout",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("\x1b[31mred\x1b[0m\nplain"))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let body = c
            .fetch_visit_stdout("ENG-1", Uuid::nil(), 1, "post0")
            .await
            .unwrap();
        assert_eq!(body, "red\nplain");
    }

    #[tokio::test]
    async fn fetch_escalations_sanitizes_payload() {
        let srv = MockServer::start().await;
        let esc = ApiEscalation {
            ticket_id: Some("ENG-1".into()),
            cycle_id: None,
            kind: "recursion_bound".into(),
            state_id: None,
            visit_n: None,
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            error_text: "\x1b[31mboom".into(),
            marker: "recursion_bound".into(),
        };
        Mock::given(method("GET"))
            .and(path("/api/escalations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![esc]))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let got = c.fetch_escalations().await.unwrap();
        assert_eq!(got[0].error_text, "boom");
    }

    #[tokio::test]
    async fn post_refresh_returns_ack() {
        let srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/refresh"))
            .respond_with(ResponseTemplate::new(202).set_body_json(RefreshAck {
                coalesced: true,
                earliest_fire_at: None,
                backoff_active: false,
            }))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let ack = c.post_refresh().await.unwrap();
        assert!(ack.coalesced);
    }

    #[tokio::test]
    async fn fetch_cycles_round_trip() {
        let srv = MockServer::start().await;
        let cyc = CycleSummary {
            cycle_id: Uuid::nil(),
            kind: roki_api_types::CycleKind::Rule,
            trigger: roki_api_types::CycleTrigger::Runtime,
            started_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            ended_at: None,
            terminal_id: None,
            failure_kind: None,
            last_state_id: Some("post0".into()),
            total_visits: 1,
        };
        Mock::given(method("GET"))
            .and(path("/api/tickets/ENG-1/cycles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![cyc]))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let got = c.fetch_cycles("ENG-1").await.unwrap();
        assert_eq!(got[0].last_state_id.as_deref(), Some("post0"));
    }

    #[tokio::test]
    async fn fetch_ticket_detail_round_trip() {
        let srv = MockServer::start().await;
        let detail = TicketDetail {
            summary: ticket("ENG-1"),
            recent_events: vec![ApiEvent {
                seq: 1,
                ts: OffsetDateTime::from_unix_timestamp(0).unwrap(),
                event: "cycle_started".into(),
                ticket_id: Some("ENG-1".into()),
                cycle_id: None,
                payload: serde_json::json!({}),
            }],
            truncated: false,
        };
        Mock::given(method("GET"))
            .and(path("/api/tickets/ENG-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(detail))
            .mount(&srv)
            .await;
        let c = ApiClient::new(&srv.uri()).unwrap();
        let got = c.fetch_ticket_detail("ENG-1").await.unwrap();
        assert_eq!(got.summary.ticket_id, "ENG-1");
        assert_eq!(got.recent_events.len(), 1);
    }
}
