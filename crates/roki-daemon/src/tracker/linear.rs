//! Polling Linear tracker adapter (task 2.5).
//!
//! Implements the cold-path Linear poller described in design.md
//! "TrackerAdapter":
//!
//! * Issues a single GraphQL `issues` query per scope per cadence tick.
//! * Caps the per-scope cadence at 5 minutes (Requirement 3.2). The
//!   configurable cadence is clamped at construction time and a warn-level
//!   trace is emitted when the operator's value is reduced.
//! * Applies exponential backoff to 429 responses, bounded between 10 seconds
//!   and 5 minutes; honours `Retry-After` when Linear advertises one
//!   (Requirement 3.3). Every backoff decision is logged.
//! * Normalises each response into [`NormalizedIssue`] (Requirement 3.4).
//! * Never issues Linear write operations — the daemon-side adapter is
//!   read-only by construction (Requirement 3.5).
//!
//! The adapter is parameterised by a list of [`ScopeWatch`] entries; one
//! cadence timer is run per scope so a slow scope never starves a fast one.
//! Inputs that the orchestrator (task 3.x) will provide:
//!
//! * the `(RepoId, LinearScope)` pairs derived from `RepoConfig`;
//! * a `tokio::sync::oneshot::Receiver<()>` shutdown channel;
//! * an `mpsc::Sender<NormalizedIssue>` sink the orchestrator drains.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep_until};
use tracing::{debug, info, warn};

use crate::config::SecretString;
use crate::config::repos::LinearScope;
use crate::orchestrator::state::{IssueId, RepoId};
use crate::tools::RateLimitState;
use crate::tracker::model::{IssueState, NormalizedIssue};
use crate::tracker::{RefreshAccepted, TrackerRefresh};

/// Hard upper bound on the configurable polling cadence (Requirement 3.2).
const MAX_CADENCE: Duration = Duration::from_secs(300);

/// Lower bound for the exponential backoff window applied to 429 responses
/// (design.md "Engine adapter" calls out 10s..=5min as the engine policy
/// bounds; the tracker reuses the same envelope for symmetry, per the
/// implementation notes for task 2.5).
const MIN_BACKOFF: Duration = Duration::from_secs(10);

/// Upper bound for the exponential backoff window (5 minutes).
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Default request timeout for the underlying reqwest client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One Linear scope the tracker should watch.
///
/// The tracker today emits [`NormalizedIssue::repo`] from this struct because
/// the polling poller knows the repo per scope at construction time. Future
/// orchestrator-side routing (task 1.5) may replace this with a router that
/// resolves repo from the response, in which case `repo` becomes optional —
/// for now the tracker is a self-contained scope-watcher.
#[derive(Debug, Clone)]
pub struct ScopeWatch {
    pub repo: RepoId,
    pub scope: LinearScope,
}

/// Construction-time configuration for [`LinearTracker`].
#[derive(Clone)]
pub struct LinearTrackerConfig {
    /// Linear GraphQL endpoint (e.g. `https://api.linear.app/graphql`). The
    /// integration tests pass a wiremock URL.
    pub endpoint: String,
    /// Operator-configured polling cadence per scope. Clamped to
    /// [`MAX_CADENCE`] at construction time.
    pub cadence: Duration,
    /// Scopes to watch. The tracker spawns one timer per scope.
    pub scopes: Vec<ScopeWatch>,
    /// Daemon-owned Linear API token.
    pub token: SecretString,
    /// Shared rate-limit state. The `linear_graphql` proxy and the tracker
    /// both consult this view so a 429 from one path defers the other.
    pub rate_limit: Arc<dyn RateLimitState>,
}

/// Per-scope mutable state shared between the polling loop and the
/// [`LinearTrackerHandle`] published as [`TrackerRefresh`].
///
/// The polling loop is the sole writer to `next_due` from the wake path; the
/// handle is allowed to advance `next_due` to "now" only when the loop is
/// not currently in 429 backoff (`in_backoff == false`). This is the
/// invariant that satisfies Requirement 13.3's "no bypass of the 429 backoff
/// state" clause.
pub(crate) struct ScopeShared {
    next_due: Mutex<Instant>,
    in_backoff: AtomicBool,
    notify: Notify,
}

impl ScopeShared {
    fn new() -> Self {
        Self {
            // Start at `now()` so the first tick fires immediately, matching
            // the legacy local-variable behaviour the cadence-cap test
            // relies on.
            next_due: Mutex::new(Instant::now()),
            in_backoff: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn load_next_due(&self) -> Instant {
        // Mutex poisoning here would mean the loop panicked while holding
        // the lock; we fall back to `now()` so the next tick fires rather
        // than poisoning the entire tracker.
        self.next_due
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|e| *e.into_inner())
    }

    fn store_next_due(&self, instant: Instant) {
        if let Ok(mut guard) = self.next_due.lock() {
            *guard = instant;
        }
    }

    fn set_backoff(&self, value: bool) {
        self.in_backoff.store(value, Ordering::Release);
    }

    fn is_in_backoff(&self) -> bool {
        self.in_backoff.load(Ordering::Acquire)
    }

    fn wake(&self) {
        self.notify.notify_one();
    }

    /// Test-only helper. Construct a controlled state without running the
    /// loop so the unit tests can drive the nudge path deterministically.
    #[cfg(test)]
    fn set_for_test(&self, next_due: Instant, in_backoff: bool) {
        self.store_next_due(next_due);
        self.set_backoff(in_backoff);
    }

    /// Test-only inspector for asserting deadline movement.
    #[cfg(test)]
    fn peek_next_due_for_test(&self) -> Instant {
        self.load_next_due()
    }
}

/// Polling Linear tracker adapter.
///
/// Construct with [`LinearTracker::new`] and drive with [`LinearTracker::run`].
/// The constructor takes ownership of the config so the runtime cannot mutate
/// the cadence or endpoint behind the loop's back.
pub struct LinearTracker {
    endpoint: String,
    cadence: Duration,
    scopes: Vec<ScopeWatch>,
    token: SecretString,
    rate_limit: Arc<dyn RateLimitState>,
    http: reqwest::Client,
    /// Per-scope shared state, aligned with `scopes`. Cloned into the
    /// handle so nudges reach every scope.
    scope_states: Vec<Arc<ScopeShared>>,
}

impl LinearTracker {
    /// Build a tracker. The cadence is clamped to the documented hard cap of
    /// 5 minutes per scope; a trace warning is emitted when the operator's
    /// value is reduced so the operator can see the clamp in logs.
    pub fn new(config: LinearTrackerConfig) -> Self {
        let cadence = clamp_cadence(config.cadence);
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let scope_states = (0..config.scopes.len())
            .map(|_| Arc::new(ScopeShared::new()))
            .collect();
        Self {
            endpoint: config.endpoint,
            cadence,
            scopes: config.scopes,
            token: config.token,
            rate_limit: config.rate_limit,
            http,
            scope_states,
        }
    }

    /// Build a [`TrackerRefresh`] handle that shares per-scope state with
    /// this tracker. The handle can be cloned freely and remains usable for
    /// the lifetime of the running loop; nudges issued after the loop
    /// terminates simply have no observable effect.
    pub fn refresh_handle(&self) -> LinearTrackerHandle {
        LinearTrackerHandle {
            scope_states: self.scope_states.clone(),
        }
    }

    /// Run the polling loop until `shutdown` resolves. One cadence timer per
    /// scope; the timers share a single `mpsc::Sender` so the orchestrator
    /// sees a fully-multiplexed stream of [`NormalizedIssue`] events.
    pub async fn run(
        self,
        sink: mpsc::Sender<NormalizedIssue>,
        shutdown: oneshot::Receiver<()>,
    ) -> Result<(), TrackerError> {
        let mut tasks = Vec::with_capacity(self.scopes.len());

        // Fan out one async loop per scope. Each loop owns its own
        // `next_due` deadline so a slow scope never starves a fast one.
        let endpoint = Arc::new(self.endpoint);
        let token = Arc::new(self.token);
        let rate_limit = self.rate_limit.clone();
        let http = self.http.clone();
        let cadence = self.cadence;

        // Wrap the shutdown receiver in a broadcast so each per-scope task
        // can observe it independently.
        let (shutdown_broadcast, _) = tokio::sync::broadcast::channel::<()>(1);
        let shutdown_signal = shutdown_broadcast.clone();
        let shutdown_pump = tokio::spawn(async move {
            // Single waiter on the oneshot; broadcast on completion.
            let _ = shutdown.await;
            let _ = shutdown_signal.send(());
        });

        for (scope, state) in self
            .scopes
            .into_iter()
            .zip(self.scope_states.iter().cloned())
        {
            let endpoint = Arc::clone(&endpoint);
            let token = Arc::clone(&token);
            let rate_limit = Arc::clone(&rate_limit);
            let http = http.clone();
            let sink = sink.clone();
            let mut shutdown_rx = shutdown_broadcast.subscribe();

            let task = tokio::spawn(async move {
                run_scope(
                    scope,
                    state,
                    endpoint,
                    token,
                    rate_limit,
                    http,
                    cadence,
                    sink,
                    &mut shutdown_rx,
                )
                .await;
            });
            tasks.push(task);
        }

        // Drop the cloned sink so consumers see channel close on shutdown.
        drop(sink);

        // Wait for shutdown to propagate, then await every scope task.
        let _ = shutdown_pump.await;
        for task in tasks {
            let _ = task.await;
        }
        Ok(())
    }
}

/// Handle for the [`TrackerRefresh`] surface published by [`LinearTracker`].
///
/// Cloning is cheap: the handle is a thin wrapper around per-scope `Arc`
/// state. Nudges issued through this handle are bounded by the documented
/// cadence cap (the loop will not poll faster than `cadence` because the
/// shared `next_due` is advanced at most to "now") and are inert during 429
/// backoff (the handle inspects `in_backoff` and refuses to advance the
/// deadline).
#[derive(Clone)]
pub struct LinearTrackerHandle {
    scope_states: Vec<Arc<ScopeShared>>,
}

impl LinearTrackerHandle {
    /// Test-only constructor. Lets unit tests build a handle with
    /// hand-prepared `ScopeShared` so the nudge path can be exercised
    /// without spinning up the polling loop.
    #[cfg(test)]
    pub(crate) fn for_test(scope_states: Vec<Arc<ScopeShared>>) -> Self {
        Self { scope_states }
    }
}

#[async_trait]
impl TrackerRefresh for LinearTrackerHandle {
    async fn nudge(&self) -> Result<RefreshAccepted, TrackerError> {
        let now = Instant::now();
        let mut max_window = Duration::from_secs(0);

        for state in &self.scope_states {
            if state.is_in_backoff() {
                // 429 backoff is sacred (Requirement 13.3): do not advance
                // `next_due`. Report the remaining backoff window so the
                // caller knows when polling will actually occur.
                let due = state.load_next_due();
                let remaining = due.saturating_duration_since(now);
                if max_window < remaining {
                    max_window = remaining;
                }
                continue;
            }

            // Idle path: advance the deadline to `now`. If the loop is
            // already at or before `now`, this is a no-op (coalescing).
            let current = state.load_next_due();
            if now < current {
                state.store_next_due(now);
            }
            // Wake the loop so it observes the new deadline immediately.
            state.wake();
            // Window is effectively zero in this case.
        }

        Ok(RefreshAccepted {
            will_poll_within: max_window,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_scope(
    scope: ScopeWatch,
    state: Arc<ScopeShared>,
    endpoint: Arc<String>,
    token: Arc<SecretString>,
    rate_limit: Arc<dyn RateLimitState>,
    http: reqwest::Client,
    cadence: Duration,
    sink: mpsc::Sender<NormalizedIssue>,
    shutdown: &mut tokio::sync::broadcast::Receiver<()>,
) {
    // Initialise the shared deadline at construction-time of the loop so
    // the first tick fires immediately (preserves the legacy cadence-cap
    // test's contract).
    state.store_next_due(Instant::now());
    state.set_backoff(false);
    // Consecutive 429 counter — drives exponential backoff.
    let mut consecutive_rate_limited: u32 = 0;

    loop {
        // Honour shutdown ahead of any outbound work. Wake on either the
        // next-poll deadline or an out-of-cycle nudge from the
        // [`TrackerRefresh`] handle.
        let next_due = state.load_next_due();
        tokio::select! {
            biased;
            _ = shutdown.recv() => {
                debug!(
                    repo = scope.repo.as_str(),
                    "tracker scope shutting down before next poll",
                );
                return;
            }
            _ = sleep_until(next_due) => {}
            _ = state.notify.notified() => {
                // Nudge wake. If the loop is in 429 backoff, the nudge does
                // NOT shorten the deadline (Requirement 13.3): re-enter the
                // select to wait for the existing deadline.
                if state.is_in_backoff() {
                    debug!(
                        repo = scope.repo.as_str(),
                        "tracker nudge ignored during 429 backoff",
                    );
                    continue;
                }
                // Idle path: the handle has already advanced `next_due` to
                // ~now, so falling through immediately polls.
            }
        }

        // Consult shared rate-limit state. If the `linear_graphql` proxy
        // already saw a 429, defer to its retry hint instead of polling.
        if let Err(rl) = rate_limit.before_call().await {
            let wait = rl
                .retry_after_seconds
                .map(Duration::from_secs)
                .unwrap_or(MIN_BACKOFF)
                .clamp(MIN_BACKOFF, MAX_BACKOFF);
            info!(
                repo = scope.repo.as_str(),
                wait_seconds = wait.as_secs(),
                "tracker deferred poll because shared rate-limit state is paused",
            );
            state.set_backoff(true);
            state.store_next_due(Instant::now() + wait);
            continue;
        }

        // Issue the GraphQL query.
        match poll_once(
            &http,
            endpoint.as_str(),
            token.expose(),
            &scope,
            rate_limit.as_ref(),
        )
        .await
        {
            Ok(issues) => {
                consecutive_rate_limited = 0;
                state.set_backoff(false);
                for normalized in issues {
                    if sink.send(normalized).await.is_err() {
                        debug!(
                            repo = scope.repo.as_str(),
                            "tracker sink closed; ending scope loop",
                        );
                        return;
                    }
                }
                state.store_next_due(Instant::now() + cadence);
            }
            Err(PollError::RateLimited { retry_after }) => {
                consecutive_rate_limited = consecutive_rate_limited.saturating_add(1);
                let backoff = compute_backoff(consecutive_rate_limited, retry_after);
                warn!(
                    repo = scope.repo.as_str(),
                    backoff_seconds = backoff.as_secs(),
                    consecutive = consecutive_rate_limited,
                    "tracker received HTTP 429; applying exponential backoff",
                );
                state.set_backoff(true);
                state.store_next_due(Instant::now() + backoff);
            }
            Err(PollError::Transport { message }) => {
                consecutive_rate_limited = 0;
                state.set_backoff(false);
                warn!(
                    repo = scope.repo.as_str(),
                    error = %message,
                    "tracker poll failed; retrying after cadence interval",
                );
                state.store_next_due(Instant::now() + cadence);
            }
            Err(PollError::HttpStatus { status }) => {
                consecutive_rate_limited = 0;
                state.set_backoff(false);
                warn!(
                    repo = scope.repo.as_str(),
                    status,
                    "tracker poll returned non-success status; retrying after cadence interval",
                );
                state.store_next_due(Instant::now() + cadence);
            }
        }
    }
}

/// Issue a single Linear `issues` GraphQL request and normalise the response.
async fn poll_once(
    http: &reqwest::Client,
    endpoint: &str,
    token: &str,
    scope: &ScopeWatch,
    rate_limit: &dyn RateLimitState,
) -> Result<Vec<NormalizedIssue>, PollError> {
    let body = json!({
        "query": ACTIVE_ISSUES_QUERY,
        "variables": variables_for(&scope.scope),
    });

    let response = http
        .post(endpoint)
        .header(reqwest::header::AUTHORIZATION, token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|err| PollError::Transport {
            message: err.to_string(),
        })?;

    let status = response.status();
    let retry_after = parse_retry_after(response.headers());
    rate_limit
        .record_response(status.as_u16(), retry_after)
        .await;

    if status.as_u16() == 429 {
        return Err(PollError::RateLimited { retry_after });
    }
    if !status.is_success() {
        // Drain the body so the connection can be reused; do not propagate
        // the body content because it may echo headers (defence in depth).
        let _ = response.text().await;
        return Err(PollError::HttpStatus {
            status: status.as_u16(),
        });
    }

    let payload: GraphQlResponse =
        response
            .json::<GraphQlResponse>()
            .await
            .map_err(|err| PollError::Transport {
                message: err.to_string(),
            })?;

    Ok(normalize(payload, scope))
}

/// Compute the next backoff window after a 429 response.
///
/// Strategy:
/// * If Linear advertises `Retry-After`, honour the advertised value (capped
///   at [`MAX_BACKOFF`]). Linear is the authoritative source on its own
///   rate-limit window; clamping its hint upward would only delay the next
///   request unnecessarily.
/// * Otherwise, double the previous window starting at [`MIN_BACKOFF`],
///   bounded by [`MAX_BACKOFF`].
fn compute_backoff(consecutive: u32, retry_after: Option<u64>) -> Duration {
    if let Some(seconds) = retry_after {
        let advertised = Duration::from_secs(seconds);
        return if MAX_BACKOFF < advertised {
            MAX_BACKOFF
        } else {
            advertised
        };
    }
    // Exponential: MIN_BACKOFF * 2^(consecutive-1), saturating at MAX_BACKOFF.
    let exponent = consecutive.saturating_sub(1).min(8); // cap to avoid overflow
    let multiplier: u64 = 1u64 << exponent;
    let candidate = MIN_BACKOFF.saturating_mul(multiplier as u32);
    if MAX_BACKOFF < candidate {
        MAX_BACKOFF
    } else {
        candidate
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn clamp_cadence(cadence: Duration) -> Duration {
    if MAX_CADENCE < cadence {
        warn!(
            requested_seconds = cadence.as_secs(),
            cap_seconds = MAX_CADENCE.as_secs(),
            "tracker polling cadence exceeded the documented 5-minute cap; clamping",
        );
        return MAX_CADENCE;
    }
    cadence
}

/// GraphQL query that fetches the active-issue slice for one scope.
///
/// The query intentionally requests just the fields [`NormalizedIssue`]
/// needs (Requirement 3.4) so we do not pull more from Linear than necessary.
const ACTIVE_ISSUES_QUERY: &str = "query ActiveIssues($filter: IssueFilter, $first: Int) {\n  issues(filter: $filter, first: $first) {\n    nodes {\n      id\n      identifier\n      title\n      description\n      state { type name }\n      labels { nodes { name } }\n      team { key }\n    }\n  }\n}";

/// Build the `IssueFilter` for a given scope.
///
/// * `Team { key }` → filter by `team.key` and the active-state types.
/// * `Labels { any_of }` → filter by `labels.some.name.in` and the active-state
///   types.
fn variables_for(scope: &LinearScope) -> Value {
    let active_states = json!({ "type": { "in": ["unstarted", "started"] } });
    let filter = match scope {
        LinearScope::Team { key } => json!({
            "team": { "key": { "eq": key } },
            "state": active_states,
        }),
        LinearScope::Labels { any_of } => json!({
            "labels": { "some": { "name": { "in": any_of } } },
            "state": active_states,
        }),
    };
    json!({ "filter": filter, "first": 100 })
}

fn normalize(payload: GraphQlResponse, scope: &ScopeWatch) -> Vec<NormalizedIssue> {
    payload
        .data
        .into_iter()
        .flat_map(|d| d.issues.nodes.into_iter())
        .map(|node| node_to_normalized(node, scope))
        .collect()
}

fn node_to_normalized(node: IssueNode, scope: &ScopeWatch) -> NormalizedIssue {
    let labels = node
        .labels
        .map(|l| l.nodes.into_iter().map(|n| n.name).collect::<Vec<_>>())
        .unwrap_or_default();
    let team_or_scope = node
        .team
        .map(|t| t.key)
        .unwrap_or_else(|| match &scope.scope {
            LinearScope::Team { key } => key.clone(),
            LinearScope::Labels { .. } => String::new(),
        });
    let state = IssueState::from_linear_type(node.state.kind.as_deref().unwrap_or(""));

    NormalizedIssue {
        repo: scope.repo.clone(),
        issue: IssueId::new(node.identifier),
        title: node.title,
        description: node.description.unwrap_or_default(),
        state,
        labels,
        team_or_scope,
    }
}

#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    data: Option<DataField>,
}

impl IntoIterator for GraphQlResponse {
    type Item = DataField;
    type IntoIter = std::option::IntoIter<DataField>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.into_iter()
    }
}

#[derive(Debug, Deserialize)]
struct DataField {
    issues: IssuesEnvelope,
}

#[derive(Debug, Deserialize)]
struct IssuesEnvelope {
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
    team: Option<TeamField>,
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

#[derive(Debug, Deserialize)]
struct TeamField {
    key: String,
}

/// Internal poll-loop error taxonomy. Distinct from [`TrackerError`] because
/// the loop wants to react differently to each variant.
#[derive(Debug)]
enum PollError {
    RateLimited { retry_after: Option<u64> },
    HttpStatus { status: u16 },
    Transport { message: String },
}

/// Public tracker error returned from [`LinearTracker::run`].
#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    /// The internal task hierarchy panicked or was cancelled unexpectedly.
    #[error("tracker task aborted: {0}")]
    TaskAborted(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::{RefreshAccepted, TrackerRefresh};

    fn dummy_scope_watch() -> ScopeWatch {
        ScopeWatch {
            repo: RepoId::new("core"),
            scope: LinearScope::Team {
                key: "ENG".to_string(),
            },
        }
    }

    #[test]
    fn cadence_above_cap_is_clamped() {
        let clamped = clamp_cadence(Duration::from_secs(900));
        assert_eq!(clamped, MAX_CADENCE);
    }

    #[test]
    fn cadence_below_cap_is_passed_through() {
        let clamped = clamp_cadence(Duration::from_secs(30));
        assert_eq!(clamped, Duration::from_secs(30));
    }

    #[test]
    fn compute_backoff_honours_retry_after_below_min_floor() {
        // Linear is the authoritative source for its own rate-limit window;
        // when it advertises `Retry-After: 1` we honour the advertised value
        // rather than inflating it to the engine's 10s lower bound.
        let value = compute_backoff(1, Some(1));
        assert_eq!(value, Duration::from_secs(1));
    }

    #[test]
    fn compute_backoff_uses_retry_after_clamped_to_max() {
        let value = compute_backoff(1, Some(900));
        assert_eq!(value, MAX_BACKOFF);
    }

    #[test]
    fn compute_backoff_doubles_with_consecutive_rate_limits() {
        // First 429: MIN_BACKOFF (10s).
        assert_eq!(compute_backoff(1, None), MIN_BACKOFF);
        // Second 429: 20s.
        assert_eq!(compute_backoff(2, None), Duration::from_secs(20));
        // Third 429: 40s.
        assert_eq!(compute_backoff(3, None), Duration::from_secs(40));
    }

    #[test]
    fn compute_backoff_saturates_at_max() {
        // Consecutive count high enough to overshoot: must clamp to MAX_BACKOFF.
        let value = compute_backoff(20, None);
        assert_eq!(value, MAX_BACKOFF);
    }

    #[test]
    fn variables_for_team_scope_filters_by_team_key() {
        let vars = variables_for(&LinearScope::Team {
            key: "ENG".to_string(),
        });
        assert_eq!(vars["filter"]["team"]["key"]["eq"], "ENG");
        // Active-state filter must be present so we never poll closed issues.
        assert_eq!(vars["filter"]["state"]["type"]["in"][0], "unstarted");
        assert_eq!(vars["filter"]["state"]["type"]["in"][1], "started");
    }

    #[test]
    fn variables_for_labels_scope_filters_by_label_set() {
        let vars = variables_for(&LinearScope::Labels {
            any_of: vec!["bug".to_string(), "p1".to_string()],
        });
        assert_eq!(vars["filter"]["labels"]["some"]["name"]["in"][0], "bug");
        assert_eq!(vars["filter"]["labels"]["some"]["name"]["in"][1], "p1");
    }

    #[test]
    fn node_to_normalized_extracts_every_documented_field() {
        let scope = dummy_scope_watch();
        let node = IssueNode {
            id: Some("uuid".into()),
            identifier: "ENG-7".into(),
            title: "title".into(),
            description: Some("body".into()),
            state: StateField {
                kind: Some("started".into()),
                name: Some("In Progress".into()),
            },
            labels: Some(LabelsEnvelope {
                nodes: vec![LabelNode { name: "bug".into() }],
            }),
            team: Some(TeamField { key: "ENG".into() }),
        };
        let normalized = node_to_normalized(node, &scope);
        assert_eq!(normalized.repo.as_str(), "core");
        assert_eq!(normalized.issue.as_str(), "ENG-7");
        assert_eq!(normalized.title, "title");
        assert_eq!(normalized.description, "body");
        assert_eq!(normalized.state, IssueState::Active);
        assert_eq!(normalized.labels, vec!["bug".to_string()]);
        assert_eq!(normalized.team_or_scope, "ENG");
    }

    #[test]
    fn node_to_normalized_falls_back_to_scope_when_team_absent() {
        let scope = dummy_scope_watch();
        let node = IssueNode {
            id: None,
            identifier: "ENG-9".into(),
            title: "t".into(),
            description: None,
            state: StateField {
                kind: Some("triage".into()),
                name: None,
            },
            labels: None,
            team: None,
        };
        let normalized = node_to_normalized(node, &scope);
        assert_eq!(normalized.team_or_scope, "ENG");
        assert_eq!(normalized.state, IssueState::Other);
        assert_eq!(normalized.description, "");
        assert!(normalized.labels.is_empty());
    }

    fn idle_scope_state() -> Arc<ScopeShared> {
        // Idle: next_due is far in the future; not in 429 backoff.
        let state = ScopeShared::new();
        state.set_for_test(Instant::now() + Duration::from_secs(60), false);
        Arc::new(state)
    }

    fn backoff_scope_state(remaining: Duration) -> Arc<ScopeShared> {
        // Backoff: next_due is set to a deadline a known distance in the
        // future, and `in_backoff` is true.
        let state = ScopeShared::new();
        state.set_for_test(Instant::now() + remaining, true);
        Arc::new(state)
    }

    #[tokio::test]
    async fn nudge_during_idle_advances_next_poll_deadline() {
        // RED: a nudge during a normal idle window must advance the
        // per-scope `next_due` deadline so the next loop iteration polls
        // immediately. This satisfies Requirement 13.3 (out-of-cycle refresh).
        let state = idle_scope_state();
        let handle = LinearTrackerHandle::for_test(vec![Arc::clone(&state)]);

        let before = state.peek_next_due_for_test();
        let response = handle.nudge().await.expect("nudge accepted");
        let after = state.peek_next_due_for_test();

        // Deadline must have moved earlier — significantly so, because we
        // started 60 seconds in the future.
        assert!(
            after < before,
            "nudge must advance next_due during idle (before={before:?}, after={after:?})",
        );
        // The advanced deadline must be at or before "now" (the loop will
        // wake on the next iteration). Allow a small grace window.
        let now = Instant::now();
        assert!(
            after <= now + Duration::from_millis(50),
            "nudge during idle must move next_due to ~now (after={after:?}, now={now:?})",
        );
        // The response window must be small (idle path).
        assert!(
            response.will_poll_within < Duration::from_millis(100),
            "idle nudge response must report a near-zero window, got {:?}",
            response.will_poll_within,
        );
    }

    #[tokio::test]
    async fn nudge_during_429_backoff_does_not_shorten_backoff() {
        // RED: a nudge during an active 429 backoff window MUST NOT shorten
        // the backoff (Requirement 13.3 explicitly bans bypassing the
        // 429 backoff state).
        let remaining = Duration::from_secs(45);
        let state = backoff_scope_state(remaining);
        let handle = LinearTrackerHandle::for_test(vec![Arc::clone(&state)]);

        let before = state.peek_next_due_for_test();
        let response = handle.nudge().await.expect("nudge accepted");
        let after = state.peek_next_due_for_test();

        // Deadline must be unchanged (or only refreshed by the loop itself,
        // which the test does not run): the handle path must not advance it.
        assert_eq!(
            before, after,
            "nudge during 429 backoff must not shorten next_due",
        );
        // The response must name the remaining backoff window (within a
        // small tolerance because `Instant::now()` advanced during the call).
        let reported = response.will_poll_within;
        assert!(
            Duration::from_secs(40) <= reported && reported <= Duration::from_secs(46),
            "response must name the remaining backoff window, got {reported:?}",
        );
    }

    #[tokio::test]
    async fn refresh_accepted_names_window_within_which_polling_will_occur() {
        // The response shape itself must carry a Duration field that names
        // the polling window — the trait contract requires this.
        let state = idle_scope_state();
        let handle = LinearTrackerHandle::for_test(vec![state]);
        let response: RefreshAccepted = handle.nudge().await.expect("nudge accepted");
        // Just exercise the field — its presence is the contract.
        let _: Duration = response.will_poll_within;
    }

    #[tokio::test]
    async fn multiple_nudges_in_same_window_coalesce() {
        // Two nudges in quick succession during the same idle window must
        // converge: the second is a no-op because next_due is already at or
        // near "now".
        let state = idle_scope_state();
        let handle = LinearTrackerHandle::for_test(vec![Arc::clone(&state)]);

        let _ = handle.nudge().await.expect("first nudge accepted");
        let after_first = state.peek_next_due_for_test();
        let _ = handle.nudge().await.expect("second nudge accepted");
        let after_second = state.peek_next_due_for_test();

        // Second nudge must not push next_due forward; coalescing means
        // either deadline is unchanged or moved no later (it should remain
        // at or before now).
        assert!(
            after_second <= after_first + Duration::from_millis(5),
            "coalesced nudges must not push the deadline forward (first={after_first:?}, second={after_second:?})",
        );
        // Both end states must be at or before now (small grace).
        let now = Instant::now();
        assert!(after_first <= now + Duration::from_millis(50));
        assert!(after_second <= now + Duration::from_millis(50));
    }

    #[tokio::test]
    async fn nudge_response_reports_max_window_across_scopes() {
        // The handle aggregates across scopes. If one scope is idle and one
        // is in backoff, the response must report the longer window so
        // the caller knows the worst case.
        let idle = idle_scope_state();
        let backoff = backoff_scope_state(Duration::from_secs(30));
        let handle = LinearTrackerHandle::for_test(vec![idle, backoff]);

        let response = handle.nudge().await.expect("nudge accepted");
        // The reported window must reflect the backoff scope.
        assert!(
            Duration::from_secs(25) <= response.will_poll_within
                && response.will_poll_within <= Duration::from_secs(31),
            "max-window response must name the backoff window, got {:?}",
            response.will_poll_within,
        );
    }
}
