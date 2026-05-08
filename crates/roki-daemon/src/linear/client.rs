// Walking-skeleton tasks land in dependency order: this client (task 3.2)
// precedes the runtime wiring that will call `LinearClient::resolve_viewer`
// at startup. Until that wiring lands, the client and its leaf API are
// exercised only by the integration tests below, which triggers `dead_code`
// for the leaf surface. Allow it module-locally instead of leaking the
// relaxation crate-wide.
#![allow(dead_code)]

//! One-shot Linear GraphQL client used by the runtime to resolve the
//! `[admission].assignee = "me"` shorthand to a Linear user id at startup.
//!
//! Covers Req 4.2. The endpoint is hardcoded; the env-var override
//! (`ROKI_LINEAR_GRAPHQL_URL`) is gated behind
//! `#[cfg(any(test, feature = "test-support"))]` so the release binary
//! always targets `https://api.linear.app/graphql` per design
//! `linear::client` block.

use crate::error::LinearClientError;

/// Hardcoded Linear GraphQL endpoint. The release binary always targets
/// this URL — the env-var override below is compiled out unless the
/// `test-support` feature (or `cfg(test)`) is active.
const LINEAR_GRAPHQL_URL_DEFAULT: &str = "https://api.linear.app/graphql";

/// Resolved Linear viewer id. The skeleton runtime holds one of these
/// for the lifetime of the cycle to compare against `NormalizedTicket`
/// assignees during admission (Req 4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeId(pub String);

/// One-shot Linear GraphQL client.
///
/// Holds the raw API token verbatim — Linear personal API tokens are
/// passed in `Authorization` without a `Bearer` prefix per the
/// upstream GraphQL contract.
pub struct LinearClient {
    http: reqwest::Client,
    token: String,
}

impl LinearClient {
    pub fn new(token: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            token,
        }
    }

    /// Endpoint URL the next `resolve_viewer` call will POST to.
    ///
    /// In release builds (no `test-support` feature, no `cfg(test)`) this
    /// resolves to the hardcoded constant unconditionally — the env-var
    /// read below is `#[cfg]`-removed entirely.
    fn endpoint() -> String {
        #[cfg(any(test, feature = "test-support"))]
        {
            if let Ok(url) = std::env::var("ROKI_LINEAR_GRAPHQL_URL") {
                return url;
            }
        }
        LINEAR_GRAPHQL_URL_DEFAULT.to_string()
    }

    /// Issue one `viewer { id }` GraphQL query and return the resolved id.
    ///
    /// Failure modes per design `linear::client`:
    /// - Transport error → `LinearClientError::Http`.
    /// - Non-200 status → `ViewerResolveFailed { reason: "non-success status N" }`.
    /// - Malformed body (json deserialization failure) →
    ///   `ViewerResolveFailed { reason: "malformed body: ..." }`.
    /// - Missing `data.viewer.id` →
    ///   `ViewerResolveFailed { reason: "missing data.viewer.id" }`.
    ///
    /// The endpoint string is carried on every error so the
    /// `tracing::error!` line at the call site identifies the URL the
    /// daemon was talking to (release vs. test-override).
    pub async fn resolve_viewer(&self) -> Result<MeId, LinearClientError> {
        let endpoint = Self::endpoint();
        // Static query body — the skeleton sends one shape only.
        let body = serde_json::json!({"query": "query { viewer { id } }"});

        let resp = self
            .http
            .post(&endpoint)
            // Token applied verbatim, no `Bearer` prefix (Linear contract).
            .header("Authorization", &self.token)
            .json(&body)
            .send()
            .await
            .map_err(|source| LinearClientError::Http {
                endpoint: endpoint.clone(),
                source,
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(LinearClientError::ViewerResolveFailed {
                endpoint,
                reason: format!("non-success status {}", status.as_u16()),
            });
        }

        let parsed: serde_json::Value =
            resp.json()
                .await
                .map_err(|source| LinearClientError::ViewerResolveFailed {
                    endpoint: endpoint.clone(),
                    reason: format!("malformed body: {source}"),
                })?;

        let id = parsed
            .pointer("/data/viewer/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LinearClientError::ViewerResolveFailed {
                endpoint: endpoint.clone(),
                reason: "missing data.viewer.id".into(),
            })?;

        Ok(MeId(id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    //! Integration tests for `resolve_viewer` against a `wiremock` stub.
    //!
    //! Env-var mutation is performed via `temp-env` (which encapsulates
    //! the unsafe `std::env::set_var` internally) so this crate stays
    //! `unsafe_code = forbid`-clean. `temp-env` also serializes env-var
    //! mutations, so concurrent test execution does not race on the
    //! shared `ROKI_LINEAR_GRAPHQL_URL` slot.

    use super::*;
    use crate::error::LinearClientError;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn resolve_viewer_success_returns_u1() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("Authorization", "token-abc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data":{"viewer":{"id":"u1"}}})),
            )
            .mount(&server)
            .await;
        let url = format!("{}/graphql", server.uri());

        let me =
            temp_env::async_with_vars([("ROKI_LINEAR_GRAPHQL_URL", Some(url.as_str()))], async {
                let client = LinearClient::new("token-abc".into());
                client.resolve_viewer().await
            })
            .await
            .expect("resolve_viewer should succeed against the stub");

        assert_eq!(me, MeId("u1".into()));
    }

    #[tokio::test]
    async fn resolve_viewer_non_200_returns_viewer_resolve_failed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let url = format!("{}/graphql", server.uri());

        let result =
            temp_env::async_with_vars([("ROKI_LINEAR_GRAPHQL_URL", Some(url.as_str()))], async {
                let client = LinearClient::new("t".into());
                client.resolve_viewer().await
            })
            .await;

        match result {
            Err(LinearClientError::ViewerResolveFailed { endpoint, reason }) => {
                assert!(
                    endpoint.contains("/graphql"),
                    "endpoint should carry the override url, got {endpoint}"
                );
                assert!(
                    reason.contains("500"),
                    "reason should carry the upstream status, got {reason}"
                );
            }
            other => panic!("expected ViewerResolveFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_viewer_malformed_body_returns_viewer_resolve_failed() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;
        let url = format!("{}/graphql", server.uri());
        temp_env::async_with_vars([("ROKI_LINEAR_GRAPHQL_URL", Some(url.as_str()))], async {
            let client = LinearClient::new("token-abc".into());
            match client.resolve_viewer().await {
                Err(LinearClientError::ViewerResolveFailed { endpoint, reason }) => {
                    assert!(endpoint.contains("/graphql"));
                    assert!(reason.starts_with("malformed body"));
                }
                other => panic!("expected ViewerResolveFailed, got {other:?}"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn resolve_viewer_missing_id_returns_viewer_resolve_failed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":{"viewer":{}}})),
            )
            .mount(&server)
            .await;
        let url = format!("{}/graphql", server.uri());

        let result =
            temp_env::async_with_vars([("ROKI_LINEAR_GRAPHQL_URL", Some(url.as_str()))], async {
                let client = LinearClient::new("t".into());
                client.resolve_viewer().await
            })
            .await;

        match result {
            Err(LinearClientError::ViewerResolveFailed { endpoint, reason }) => {
                assert!(endpoint.contains("/graphql"));
                assert!(
                    reason.contains("viewer"),
                    "reason should mention viewer, got {reason}"
                );
            }
            other => panic!("expected ViewerResolveFailed, got {other:?}"),
        }
    }
}
