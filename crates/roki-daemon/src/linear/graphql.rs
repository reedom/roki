#![allow(dead_code)]

//! Paginated Linear GraphQL `issues(...)` primitive used by cold start
//! (and, in a future slice, by polling). Honors the shared
//! `RateLimitState` for 429 backoff.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::error::LinearEnumerateError;
use crate::linear::rate_limit::RateLimitState;

pub const LINEAR_GRAPHQL_URL_DEFAULT: &str = "https://api.linear.app/graphql";
pub const DEFAULT_PAGE_SIZE: u32 = 50;

const MAX_BACKOFF_RETRIES: u32 = 6;

pub enum StatusFilter<'a> {
    None,
    Union(&'a [&'a str]),
}

pub struct EnumerateRequest<'a> {
    pub assignee_id: &'a str,
    pub status_filter: StatusFilter<'a>,
    pub page_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumeratedTicket {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub state_name: String,
    pub label_names: std::collections::BTreeSet<String>,
    pub assignee_id: Option<String>,
    pub updated_at: OffsetDateTime,
}

pub struct LinearGraphqlClient {
    http: reqwest::Client,
    token: String,
    rate_limit: Arc<RateLimitState>,
}

impl LinearGraphqlClient {
    pub fn new(token: String, rate_limit: Arc<RateLimitState>) -> Self {
        Self {
            http: reqwest::Client::new(),
            token,
            rate_limit,
        }
    }

    fn endpoint() -> String {
        #[cfg(any(test, feature = "test-support"))]
        {
            if let Ok(url) = std::env::var("ROKI_LINEAR_GRAPHQL_URL") {
                return url;
            }
        }
        LINEAR_GRAPHQL_URL_DEFAULT.to_string()
    }

    pub async fn enumerate(
        &self,
        req: &EnumerateRequest<'_>,
    ) -> Result<Vec<EnumeratedTicket>, LinearEnumerateError> {
        let endpoint = Self::endpoint();
        let mut out = Vec::new();
        let mut after: Option<String> = None;

        loop {
            self.rate_limit.wait_if_backoff().await;
            let body = build_query_body(req, after.as_deref());

            let mut retries: u32 = 0;
            let response = loop {
                let resp = self
                    .http
                    .post(&endpoint)
                    .header("Authorization", &self.token)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|source| LinearEnumerateError::Http {
                        endpoint: endpoint.clone(),
                        source,
                    })?;

                if resp.status().as_u16() == 429 {
                    if retries >= MAX_BACKOFF_RETRIES {
                        return Err(LinearEnumerateError::BackoffExhausted { retries });
                    }
                    retries += 1;
                    let retry_after = resp
                        .headers()
                        .get("Retry-After")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    self.rate_limit.record_429(retry_after);
                    self.rate_limit.wait_if_backoff().await;
                    continue;
                }

                break resp;
            };

            let status = response.status();
            if !status.is_success() {
                return Err(LinearEnumerateError::NonSuccess {
                    endpoint: endpoint.clone(),
                    status: status.as_u16(),
                });
            }

            self.rate_limit.clear(); // success clears any prior 429 state

            let json: Value =
                response
                    .json()
                    .await
                    .map_err(|e| LinearEnumerateError::Malformed {
                        endpoint: endpoint.clone(),
                        reason: format!("json decode: {}", e),
                    })?;

            if let Some(errs) = json.get("errors") {
                return Err(LinearEnumerateError::GraphqlError {
                    endpoint: endpoint.clone(),
                    message: errs.to_string(),
                });
            }

            let page =
                parse_issues_page(&json).map_err(|reason| LinearEnumerateError::Malformed {
                    endpoint: endpoint.clone(),
                    reason,
                })?;

            out.extend(page.tickets);

            if !page.has_next_page {
                break;
            }
            after = page.end_cursor;
            if after.is_none() {
                return Err(LinearEnumerateError::Malformed {
                    endpoint,
                    reason: "hasNextPage with no endCursor".into(),
                });
            }
        }

        Ok(out)
    }
}

fn build_query_body(req: &EnumerateRequest<'_>, after: Option<&str>) -> Value {
    let mut filter = json!({
        "assignee": { "id": { "eq": req.assignee_id } }
    });
    if let StatusFilter::Union(states) = req.status_filter {
        filter["state"] = json!({ "name": { "in": states } });
    }

    let query = r#"
        query Enumerate($filter: IssueFilter, $first: Int, $after: String) {
            issues(filter: $filter, first: $first, after: $after) {
                pageInfo { hasNextPage endCursor }
                nodes {
                    id
                    identifier
                    title
                    description
                    state { name }
                    labels { nodes { name } }
                    assignee { id }
                    updatedAt
                }
            }
        }
    "#;

    let mut variables = json!({
        "filter": filter,
        "first": req.page_size,
    });
    if let Some(cursor) = after {
        variables["after"] = json!(cursor);
    }

    json!({ "query": query, "variables": variables })
}

struct ParsedPage {
    tickets: Vec<EnumeratedTicket>,
    has_next_page: bool,
    end_cursor: Option<String>,
}

fn parse_issues_page(json: &Value) -> Result<ParsedPage, String> {
    let issues = json
        .get("data")
        .and_then(|d| d.get("issues"))
        .ok_or_else(|| "missing data.issues".to_string())?;
    let page_info = issues
        .get("pageInfo")
        .ok_or_else(|| "missing data.issues.pageInfo".to_string())?;
    let has_next_page = page_info
        .get("hasNextPage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let end_cursor = page_info
        .get("endCursor")
        .and_then(|v| v.as_str())
        .map(String::from);

    let nodes = issues
        .get("nodes")
        .and_then(|n| n.as_array())
        .ok_or_else(|| "missing data.issues.nodes array".to_string())?;

    let mut tickets = Vec::with_capacity(nodes.len());
    for node in nodes {
        tickets.push(parse_one_node(node)?);
    }

    Ok(ParsedPage {
        tickets,
        has_next_page,
        end_cursor,
    })
}

fn parse_one_node(node: &Value) -> Result<EnumeratedTicket, String> {
    let id = node
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("issue.id missing")?
        .to_string();
    let identifier = node
        .get("identifier")
        .and_then(|v| v.as_str())
        .ok_or("issue.identifier missing")?
        .to_string();
    let title = node
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = node
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);
    let state_name = node
        .get("state")
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
        .ok_or("issue.state.name missing")?
        .to_string();
    let label_names = node
        .get("labels")
        .and_then(|l| l.get("nodes"))
        .and_then(|n| n.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|n| n.get("name").and_then(|v| v.as_str()))
                .map(String::from)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();
    let assignee_id = node
        .get("assignee")
        .and_then(|a| a.get("id"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let updated_at_raw = node
        .get("updatedAt")
        .and_then(|v| v.as_str())
        .ok_or("issue.updatedAt missing")?;
    let updated_at = OffsetDateTime::parse(
        updated_at_raw,
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|e| format!("issue.updatedAt parse: {}", e))?;

    Ok(EnumeratedTicket {
        id,
        identifier,
        title,
        description,
        state_name,
        label_names,
        assignee_id,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rl() -> Arc<RateLimitState> {
        Arc::new(RateLimitState::new())
    }

    fn page(nodes: Value, has_next: bool, cursor: Option<&str>) -> Value {
        json!({
            "data": {
                "issues": {
                    "pageInfo": {
                        "hasNextPage": has_next,
                        "endCursor": cursor,
                    },
                    "nodes": nodes
                }
            }
        })
    }

    fn issue(id: &str, identifier: &str, state: &str, assignee: &str) -> Value {
        json!({
            "id": id,
            "identifier": identifier,
            "title": "T",
            "description": null,
            "state": { "name": state },
            "labels": { "nodes": [] },
            "assignee": { "id": assignee },
            "updatedAt": "2026-05-09T00:00:00Z"
        })
    }

    // Env-var mutation is performed via `temp-env` (which encapsulates the
    // unsafe `std::env::set_var` internally) so this crate stays
    // `unsafe_code = forbid`-clean. `temp-env` also serializes env-var
    // mutations, so concurrent test execution does not race on the shared
    // `ROKI_LINEAR_GRAPHQL_URL` slot.

    #[tokio::test]
    async fn single_page_returns_all_tickets() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("Authorization", "tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(
                json!([
                    issue("a1", "TEAM-1", "Todo", "u1"),
                    issue("a2", "TEAM-2", "Todo", "u1")
                ]),
                false,
                None,
            )))
            .mount(&server)
            .await;

        let url = server.uri();
        let out =
            temp_env::async_with_vars([("ROKI_LINEAR_GRAPHQL_URL", Some(url.as_str()))], async {
                let c = LinearGraphqlClient::new("tok".into(), rl());
                c.enumerate(&EnumerateRequest {
                    assignee_id: "u1",
                    status_filter: StatusFilter::None,
                    page_size: DEFAULT_PAGE_SIZE,
                })
                .await
            })
            .await
            .expect("enumerate");

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].identifier, "TEAM-1");
        assert_eq!(out[1].identifier, "TEAM-2");
    }

    #[tokio::test]
    async fn paginates_until_has_next_page_false() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(
                json!([issue("a1", "TEAM-1", "Todo", "u1")]),
                true,
                Some("cursor-1"),
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(
                json!([issue("a2", "TEAM-2", "Todo", "u1")]),
                false,
                None,
            )))
            .mount(&server)
            .await;

        let url = server.uri();
        let out =
            temp_env::async_with_vars([("ROKI_LINEAR_GRAPHQL_URL", Some(url.as_str()))], async {
                let c = LinearGraphqlClient::new("tok".into(), rl());
                c.enumerate(&EnumerateRequest {
                    assignee_id: "u1",
                    status_filter: StatusFilter::None,
                    page_size: 1,
                })
                .await
            })
            .await
            .expect("enumerate");

        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn http_429_with_retry_after_triggers_backoff_then_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "1")
                    .set_body_string(""),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(
                json!([issue("a1", "TEAM-1", "Todo", "u1")]),
                false,
                None,
            )))
            .mount(&server)
            .await;

        let url = server.uri();
        let rl = rl();
        let rl_for_assert = rl.clone();
        let out = temp_env::async_with_vars(
            [("ROKI_LINEAR_GRAPHQL_URL", Some(url.as_str()))],
            async move {
                let c = LinearGraphqlClient::new("tok".into(), rl);
                c.enumerate(&EnumerateRequest {
                    assignee_id: "u1",
                    status_filter: StatusFilter::None,
                    page_size: DEFAULT_PAGE_SIZE,
                })
                .await
            },
        )
        .await
        .expect("enumerate after backoff");

        assert_eq!(out.len(), 1);
        assert!(!rl_for_assert.is_in_backoff(), "success clears backoff");
    }

    #[tokio::test]
    async fn graphql_errors_array_surfaces_typed_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{ "message": "bad token" }]
            })))
            .mount(&server)
            .await;

        let url = server.uri();
        let result =
            temp_env::async_with_vars([("ROKI_LINEAR_GRAPHQL_URL", Some(url.as_str()))], async {
                let c = LinearGraphqlClient::new("tok".into(), rl());
                c.enumerate(&EnumerateRequest {
                    assignee_id: "u1",
                    status_filter: StatusFilter::None,
                    page_size: DEFAULT_PAGE_SIZE,
                })
                .await
            })
            .await;

        let err = result.expect_err("should surface graphql error");
        assert!(matches!(err, LinearEnumerateError::GraphqlError { .. }));
    }

    #[test]
    fn status_filter_present_includes_state_in_filter() {
        let req = EnumerateRequest {
            assignee_id: "u1",
            status_filter: StatusFilter::Union(&["Todo", "InProgress"]),
            page_size: 50,
        };
        let body = build_query_body(&req, None);
        let filter = &body["variables"]["filter"];
        assert!(filter.get("state").is_some());
        assert_eq!(filter["assignee"]["id"]["eq"], "u1");
    }

    #[test]
    fn status_filter_absent_omits_state_from_filter() {
        let req = EnumerateRequest {
            assignee_id: "u1",
            status_filter: StatusFilter::None,
            page_size: 50,
        };
        let body = build_query_body(&req, None);
        assert!(body["variables"]["filter"].get("state").is_none());
    }
}
