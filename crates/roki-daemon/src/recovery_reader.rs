//! Production [`RecoveryLinearReader`] backed by the Linear GraphQL API
//! (task 7.1e).
//!
//! The polling [`crate::tracker::linear::LinearTracker`] runs a continuous
//! cadence loop that pulls every active issue every <= 5 minutes. Recovery
//! needs a one-shot lookup keyed by `IssueId` so it can drive the 5-cell
//! decision matrix without waiting for the next poll tick. This module
//! provides a thin client that issues a single `issue(id)` GraphQL query
//! per `IssueId` discovered on disk, classifies the response into a
//! [`RecoveryIssueLifecycle`] bucket, and returns a [`NormalizedIssue`]
//! payload when one is available.
//!
//! ## Why a separate module
//!
//! The polling loop's GraphQL surface fetches the entire active-issue
//! slice in one call; recovery needs precise per-issue lookups so the
//! orchestrator does not race a fresh poll while reconciling the disk.
//! Sharing GraphQL primitives with the polling tracker is acceptable but
//! the query bodies differ enough that we keep two short, focused query
//! constants rather than threading scope filters through the polling
//! query.
//!
//! ## Lifecycle classification
//!
//! Linear's workflow-state taxonomy is collapsed onto the recovery
//! buckets:
//!
//! | Linear workflow-state `type` | Recovery bucket          |
//! | :--------------------------- | :----------------------- |
//! | `unstarted`, `started`       | `Active`                 |
//! | `completed`                  | `Terminal`               |
//! | `canceled`                   | `TerminalFailure`        |
//! | (anything else, or no state) | `Unknown`                |
//!
//! `canceled` is treated as `TerminalFailure` because Linear does not
//! distinguish a failed-and-closed issue from a cancelled one in its
//! state-type taxonomy; the operator-facing safer default is to retain
//! worktrees on `canceled` for inspection. Operators who want the more
//! aggressive cleanup can rename the workflow state in Linear; the
//! daemon's contract is to retain on terminal-failure (decision #6).

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::config::SecretString;
use crate::orchestrator::recovery::{RecoveryIssueLifecycle, RecoveryLinearReader};
use crate::orchestrator::state::IssueId;
use crate::tools::RateLimitState;
use crate::tracker::assignee::AssigneeAdmission;
use crate::tracker::model::{IssueState as TrackerIssueState, NormalizedIssue};

/// Default request timeout for the underlying reqwest client. Matches the
/// polling tracker for symmetry.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// GraphQL query that fetches a single issue by its human-readable
/// identifier (`ENG-123`). Linear's `issueVcsBranchSearch` is keyed by
/// branch name; the canonical surface for "given an identifier, return
/// the issue" is `issues(filter: { number, team })` — but Linear's API
/// also exposes a `issueSearch` query that accepts the identifier as a
/// query string, which is the simplest surface for our purposes.
///
/// We use `issueSearch` to keep the implementation small. The query
/// returns at most a handful of issues; the recovery client filters by
/// `identifier == requested_id` to find the exact match.
const ISSUE_SEARCH_QUERY: &str = "query IssueSearch($query: String!) {\n  issueSearch(query: $query, first: 5) {\n    nodes {\n      id\n      identifier\n      title\n      description\n      state { type name }\n      assignee { id }\n      labels { nodes { name } }\n    }\n  }\n}";

/// GraphQL query that fetches every active issue assigned to the resolved
/// Linear assignee. Mirrors the polling tracker's surface so recovery sees
/// the same active-issue slice the polling loop would on its next tick.
const ACTIVE_ISSUES_QUERY: &str = "query ActiveIssues($filter: IssueFilter, $first: Int) {\n  issues(filter: $filter, first: $first) {\n    nodes {\n      id\n      identifier\n      title\n      description\n      state { type name }\n      assignee { id }\n      labels { nodes { name } }\n    }\n  }\n}";

/// Production [`RecoveryLinearReader`] backed by an HTTP client against
/// the Linear GraphQL endpoint.
pub struct LinearRecoveryReader {
    endpoint: String,
    token: SecretString,
    rate_limit: Arc<dyn RateLimitState>,
    assignee: AssigneeAdmission,
    http: reqwest::Client,
}

impl LinearRecoveryReader {
    /// Construct a new reader. `endpoint` is the Linear GraphQL URL
    /// (production: `https://api.linear.app/graphql`; tests inject a
    /// wiremock URL via `[linear].endpoint`).
    pub fn new(
        endpoint: impl Into<String>,
        token: SecretString,
        rate_limit: Arc<dyn RateLimitState>,
        assignee: AssigneeAdmission,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            endpoint: endpoint.into(),
            token,
            rate_limit,
            assignee,
            http,
        }
    }
}

#[async_trait]
impl RecoveryLinearReader for LinearRecoveryReader {
    async fn lookup_issue(
        &self,
        issue: &IssueId,
    ) -> Result<(RecoveryIssueLifecycle, Option<NormalizedIssue>), String> {
        // Cooperate with the shared rate-limit state so a 429 from the
        // polling loop defers recovery's lookups too.
        if let Err(rl) = self.rate_limit.before_call().await {
            warn!(
                target: "orchestrator.recovery",
                issue = %issue.as_str(),
                retry_after = ?rl.retry_after_seconds,
                "recovery lookup deferred by rate-limit; classifying as Unknown",
            );
            return Ok((RecoveryIssueLifecycle::Unknown, None));
        }

        let body = json!({
            "query": ISSUE_SEARCH_QUERY,
            "variables": { "query": issue.as_str() },
        });

        let response = self
            .http
            .post(&self.endpoint)
            .header(reqwest::header::AUTHORIZATION, self.token.expose())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("transport: {err}"))?;

        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        self.rate_limit
            .record_response(status.as_u16(), retry_after)
            .await;

        if status.as_u16() == 429 {
            warn!(
                target: "orchestrator.recovery",
                issue = %issue.as_str(),
                retry_after = ?retry_after,
                "recovery lookup hit rate limit; classifying as Unknown",
            );
            return Ok((RecoveryIssueLifecycle::Unknown, None));
        }
        if !status.is_success() {
            return Err(format!("linear http status {}", status.as_u16()));
        }

        let payload: GraphQlResponse = response
            .json::<GraphQlResponse>()
            .await
            .map_err(|err| format!("decode: {err}"))?;
        let nodes = payload
            .data
            .and_then(|d| d.issue_search)
            .map(|s| s.nodes)
            .unwrap_or_default();

        // Find the exact identifier match; Linear's search query is fuzzy
        // and may return adjacent issues.
        let needle = issue.as_str();
        let matched = nodes.into_iter().find(|n| n.identifier == needle);
        let Some(node) = matched else {
            return Ok((RecoveryIssueLifecycle::Unknown, None));
        };

        let lifecycle = classify_state(node.state.kind.as_deref().unwrap_or(""));
        let normalized = node_to_normalized(node);
        if lifecycle == RecoveryIssueLifecycle::Active && !self.assignee.matches_issue(&normalized)
        {
            return Ok((RecoveryIssueLifecycle::Unknown, Some(normalized)));
        }
        Ok((lifecycle, Some(normalized)))
    }

    async fn active_issues(&self) -> Result<Vec<(IssueId, NormalizedIssue)>, String> {
        if let Err(rl) = self.rate_limit.before_call().await {
            warn!(
                target: "orchestrator.recovery",
                retry_after = ?rl.retry_after_seconds,
                "active-issues bulk fetch deferred by rate-limit; returning empty slice",
            );
            return Ok(Vec::new());
        }

        let body = json!({
            "query": ACTIVE_ISSUES_QUERY,
                "variables": {
                "filter": {
                    "state": { "type": { "in": ["unstarted", "started"] } },
                    "assignee": { "id": { "eq": self.assignee.user_id() } }
                },
                "first": 100,
            },
        });

        let response = self
            .http
            .post(&self.endpoint)
            .header(reqwest::header::AUTHORIZATION, self.token.expose())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("transport: {err}"))?;

        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        self.rate_limit
            .record_response(status.as_u16(), retry_after)
            .await;

        if status.as_u16() == 429 {
            warn!(
                target: "orchestrator.recovery",
                retry_after = ?retry_after,
                "active-issues bulk fetch rate limited; returning empty slice",
            );
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(format!("linear http status {}", status.as_u16()));
        }

        let payload: ActiveIssuesResponse = response
            .json::<ActiveIssuesResponse>()
            .await
            .map_err(|err| format!("decode: {err}"))?;
        let nodes = payload
            .data
            .and_then(|d| d.issues)
            .map(|i| i.nodes)
            .unwrap_or_default();
        let mut out: Vec<(IssueId, NormalizedIssue)> = Vec::with_capacity(nodes.len());
        for node in nodes {
            let normalized = node_to_normalized(node);
            let id = normalized.issue.clone();
            out.push((id, normalized));
        }
        Ok(out)
    }
}

#[derive(Debug, Deserialize)]
struct ActiveIssuesResponse {
    data: Option<ActiveIssuesData>,
}

#[derive(Debug, Deserialize)]
struct ActiveIssuesData {
    issues: Option<ActiveIssuesEnvelope>,
}

#[derive(Debug, Deserialize)]
struct ActiveIssuesEnvelope {
    #[serde(default)]
    nodes: Vec<IssueNode>,
}

/// Map a Linear `state.type` string to a [`RecoveryIssueLifecycle`].
///
/// `unstarted` / `started` → `Active`; `completed` → `Terminal`; `canceled`
/// → `TerminalFailure` (safer default — operator may have cancelled to
/// abandon work). Anything else → `Unknown`.
fn classify_state(linear_type: &str) -> RecoveryIssueLifecycle {
    match linear_type {
        "unstarted" | "started" => RecoveryIssueLifecycle::Active,
        "completed" => RecoveryIssueLifecycle::Terminal,
        "canceled" => RecoveryIssueLifecycle::TerminalFailure,
        _ => RecoveryIssueLifecycle::Unknown,
    }
}

fn node_to_normalized(node: IssueNode) -> NormalizedIssue {
    let labels = node
        .labels
        .map(|l| l.nodes.into_iter().map(|n| n.name).collect::<Vec<_>>())
        .unwrap_or_default();
    let state = TrackerIssueState::from_linear_type(node.state.kind.as_deref().unwrap_or(""));
    NormalizedIssue {
        issue: IssueId::new(node.identifier),
        title: node.title,
        description: node.description.unwrap_or_default(),
        state,
        labels,
        assignee_user_id: node.assignee.map(|assignee| assignee.id),
    }
}

#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    data: Option<DataField>,
}

#[derive(Debug, Deserialize)]
struct DataField {
    #[serde(rename = "issueSearch")]
    issue_search: Option<IssueSearchEnvelope>,
}

#[derive(Debug, Deserialize)]
struct IssueSearchEnvelope {
    #[serde(default)]
    nodes: Vec<IssueNode>,
}

#[derive(Debug, Deserialize)]
struct IssueNode {
    #[serde(default)]
    #[allow(dead_code)]
    id: Option<String>,
    identifier: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    state: StateField,
    #[serde(default)]
    labels: Option<LabelsEnvelope>,
    #[serde(default)]
    assignee: Option<AssigneeField>,
}

#[derive(Debug, Deserialize)]
struct AssigneeField {
    id: String,
}

#[derive(Debug, Deserialize)]
struct StateField {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LabelsEnvelope {
    #[serde(default)]
    nodes: Vec<LabelNode>,
}

#[derive(Debug, Deserialize)]
struct LabelNode {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::tools::NoopRateLimit;

    #[test]
    fn classify_state_maps_active_buckets() {
        assert_eq!(classify_state("unstarted"), RecoveryIssueLifecycle::Active);
        assert_eq!(classify_state("started"), RecoveryIssueLifecycle::Active);
    }

    #[test]
    fn classify_state_distinguishes_terminal_success_from_failure() {
        assert_eq!(
            classify_state("completed"),
            RecoveryIssueLifecycle::Terminal
        );
        assert_eq!(
            classify_state("canceled"),
            RecoveryIssueLifecycle::TerminalFailure
        );
    }

    #[test]
    fn classify_state_unknown_for_other_types() {
        assert_eq!(classify_state("triage"), RecoveryIssueLifecycle::Unknown);
        assert_eq!(classify_state("backlog"), RecoveryIssueLifecycle::Unknown);
        assert_eq!(classify_state(""), RecoveryIssueLifecycle::Unknown);
    }

    #[tokio::test]
    async fn lookup_issue_treats_active_issue_assigned_elsewhere_as_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "issueSearch": {
                        "nodes": [
                            {
                                "id": "linear-issue-1",
                                "identifier": "ENG-1",
                                "title": "Implement admission",
                                "description": "Active but assigned away.",
                                "state": { "type": "started", "name": "In Progress" },
                                "assignee": { "id": "user-other" },
                                "labels": { "nodes": [] }
                            }
                        ]
                    }
                }
            })))
            .mount(&server)
            .await;
        let reader = LinearRecoveryReader::new(
            server.uri(),
            SecretString::new("lin_test"),
            Arc::new(NoopRateLimit),
            AssigneeAdmission::new("user-me").unwrap(),
        );

        let (lifecycle, issue) = reader
            .lookup_issue(&IssueId::new("ENG-1"))
            .await
            .expect("lookup issue");

        assert_eq!(lifecycle, RecoveryIssueLifecycle::Unknown);
        assert_eq!(
            issue
                .expect("normalized issue is retained for diagnostics")
                .assignee_user_id
                .as_deref(),
            Some("user-other"),
        );
    }
}
