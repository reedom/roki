//! Linear assignee admission.
//!
//! This module owns the daemon-side admission predicate introduced by the
//! `roki-mvp` assignee requirement. The configured selector is resolved once
//! at bootstrap into a concrete Linear user id; every tracker observation can
//! then be classified with a pure id comparison before it reaches worker
//! admission.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use tracing::warn;

use crate::config::SecretString;
use crate::tools::RateLimitState;
use crate::tracker::model::NormalizedIssue;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const VIEWER_QUERY: &str = "query RokiViewer { viewer { id name email } }";

const USER_SEARCH_QUERY: &str = "query RokiAssigneeSearch($selector: String!) {\n  users(filter: { or: [\n    { id: { eq: $selector } },\n    { email: { eq: $selector } },\n    { name: { eq: $selector } }\n  ] }, first: 2) {\n    nodes { id name email }\n  }\n}";

/// Concrete admission predicate derived from `[linear].assignee`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssigneeAdmission {
    user_id: String,
}

impl AssigneeAdmission {
    pub fn new(user_id: impl Into<String>) -> Result<Self, AssigneeResolveError> {
        let user_id = user_id.into();
        if user_id.trim().is_empty() {
            return Err(AssigneeResolveError::EmptySelector);
        }
        Ok(Self { user_id })
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn matches_issue(&self, issue: &NormalizedIssue) -> bool {
        issue.assignee_user_id.as_deref() == Some(self.user_id())
    }
}

/// Resolve `[linear].assignee` against Linear.
pub async fn resolve_linear_assignee(
    endpoint: &str,
    token: &SecretString,
    selector: &str,
    rate_limit: Arc<dyn RateLimitState>,
) -> Result<AssigneeAdmission, AssigneeResolveError> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(AssigneeResolveError::EmptySelector);
    }

    let http = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    if selector == "me" {
        let body = json!({ "query": VIEWER_QUERY, "variables": {} });
        let response: ViewerResponse =
            graphql(&http, endpoint, token.expose(), body, rate_limit).await?;
        let user =
            response
                .data
                .and_then(|data| data.viewer)
                .ok_or(AssigneeResolveError::NotFound {
                    selector: selector.to_string(),
                })?;
        return AssigneeAdmission::new(user.id);
    }

    let body = json!({
        "query": USER_SEARCH_QUERY,
        "variables": { "selector": selector },
    });
    let response: UsersResponse =
        graphql(&http, endpoint, token.expose(), body, rate_limit).await?;
    let users = response
        .data
        .and_then(|data| data.users)
        .map(|users| users.nodes)
        .unwrap_or_default();
    match users.len() {
        0 => Err(AssigneeResolveError::NotFound {
            selector: selector.to_string(),
        }),
        1 => AssigneeAdmission::new(users[0].id.clone()),
        count => Err(AssigneeResolveError::Ambiguous {
            selector: selector.to_string(),
            count,
        }),
    }
}

async fn graphql<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    endpoint: &str,
    token: &str,
    body: serde_json::Value,
    rate_limit: Arc<dyn RateLimitState>,
) -> Result<T, AssigneeResolveError> {
    if let Err(rl) = rate_limit.before_call().await {
        return Err(AssigneeResolveError::RateLimited {
            retry_after_seconds: rl.retry_after_seconds,
        });
    }

    let response = http
        .post(endpoint)
        .header(reqwest::header::AUTHORIZATION, token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|err| AssigneeResolveError::Transport(err.to_string()))?;

    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    rate_limit
        .record_response(status.as_u16(), retry_after)
        .await;

    if status.as_u16() == 429 {
        warn!(
            target: "tracker.assignee",
            retry_after_seconds = ?retry_after,
            "Linear assignee resolution hit rate limit",
        );
        return Err(AssigneeResolveError::RateLimited {
            retry_after_seconds: retry_after,
        });
    }
    if !status.is_success() {
        let _ = response.text().await;
        return Err(AssigneeResolveError::HttpStatus(status.as_u16()));
    }
    response
        .json::<T>()
        .await
        .map_err(|err| AssigneeResolveError::Decode(err.to_string()))
}

#[derive(Debug, Error)]
pub enum AssigneeResolveError {
    #[error("config field `linear.assignee` is invalid: must not be empty")]
    EmptySelector,

    #[error("Linear assignee selector `{selector}` did not resolve to a user")]
    NotFound { selector: String },

    #[error("Linear assignee selector `{selector}` resolved to {count} users")]
    Ambiguous { selector: String, count: usize },

    #[error("Linear assignee resolution rate limited; retry_after_seconds={retry_after_seconds:?}")]
    RateLimited { retry_after_seconds: Option<u64> },

    #[error("Linear assignee resolution returned HTTP status {0}")]
    HttpStatus(u16),

    #[error("Linear assignee resolution transport error: {0}")]
    Transport(String),

    #[error("Linear assignee resolution decode error: {0}")]
    Decode(String),
}

#[derive(Debug, Deserialize)]
struct ViewerResponse {
    data: Option<ViewerData>,
}

#[derive(Debug, Deserialize)]
struct ViewerData {
    viewer: Option<UserNode>,
}

#[derive(Debug, Deserialize)]
struct UsersResponse {
    data: Option<UsersData>,
}

#[derive(Debug, Deserialize)]
struct UsersData {
    users: Option<UsersEnvelope>,
}

#[derive(Debug, Deserialize)]
struct UsersEnvelope {
    #[serde(default)]
    nodes: Vec<UserNode>,
}

#[derive(Debug, Deserialize)]
struct UserNode {
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    email: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::NoopRateLimit;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn resolves_me_from_viewer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"data":{"viewer":{"id":"user-me","name":"Me","email":"me@example.com"}}}),
            ))
            .mount(&server)
            .await;

        let resolved = resolve_linear_assignee(
            &format!("{}/graphql", server.uri()),
            &SecretString::new("lin_test"),
            "me",
            Arc::new(NoopRateLimit),
        )
        .await
        .expect("me resolves");

        assert_eq!(resolved.user_id(), "user-me");
    }

    #[tokio::test]
    async fn explicit_selector_must_resolve_to_exactly_one_user() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"data":{"users":{"nodes":[{"id":"user-one","name":"One"}]}}}),
            ))
            .mount(&server)
            .await;

        let resolved = resolve_linear_assignee(
            &format!("{}/graphql", server.uri()),
            &SecretString::new("lin_test"),
            "one@example.com",
            Arc::new(NoopRateLimit),
        )
        .await
        .expect("explicit selector resolves");

        assert_eq!(resolved.user_id(), "user-one");
    }
}
