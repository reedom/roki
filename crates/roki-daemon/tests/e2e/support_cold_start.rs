//! Shared scaffolding for slice 6 cold-start e2e fixtures.
//!
//! Slice 6 added a cold-start enumeration step before the daemon emits
//! `daemon_ready`. The webhook listener binds early but returns
//! `503 cold_start_in_progress` until the gate opens. Slice 1-5 fixtures
//! used to POST a webhook immediately after `wait_for_listener`; with
//! slice 6 that POST races the cold-start window.
//!
//! This module provides the two helpers every fixture needs:
//!
//! - `await_daemon_ready(session_root)` polls
//!   `<session_root>/_daemon.events.jsonl` for the `daemon_ready` line.
//!   Call it after `wait_for_listener` and before the first webhook POST.
//!
//! - `stub_empty_issues(server)` mounts a high-priority wiremock matcher
//!   that returns an empty paginated `issues` page so the cold-start
//!   `LinearGraphqlClient::enumerate` call succeeds with zero tickets and
//!   no `enum_partial: true` noise. The existing per-fixture viewer-stub
//!   continues to handle the `viewer { id }` query because the issues
//!   matcher is body-scoped (`body_string_contains("issues(")`).
//!
//! Each fixture pulls these in via `mod support_cold_start;`. Existing
//! inline helpers (`wait_for_listener`, `wait_for_event_count`,
//! `sigterm_*`) are intentionally untouched — duplicating them across
//! fixtures was the prior style and changing it is out of scope.

#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

/// Poll `<session_root>/_daemon.events.jsonl` until a line whose `event`
/// field equals `name` appears, then return the parsed JSON. Panics on
/// timeout.
pub async fn await_daemon_event(session_root: &Path, name: &str, timeout: Duration) -> Value {
    let path = session_root.join("_daemon.events.jsonl");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                for line in content.lines() {
                    if let Ok(v) = serde_json::from_str::<Value>(line) {
                        if v.get("event").and_then(|e| e.as_str()) == Some(name) {
                            return v;
                        }
                    }
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "event {name:?} not seen within {timeout:?} in {}",
                path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Convenience wrapper: await `daemon_ready` with a 30-second budget,
/// the standard e2e ceiling. Cold start is bounded by the workflow's
/// status union and an empty enumerate stub, so 30s is generous.
pub async fn await_daemon_ready(session_root: &Path) -> Value {
    await_daemon_event(session_root, "daemon_ready", Duration::from_secs(30)).await
}

/// Poll `<session_root>/_daemon.events.jsonl` until at least
/// `expected_count` lines whose `event` field equals `name` are present.
/// Useful for fixtures that spawn the daemon multiple times: the events
/// file is opened in append mode, so the second spawn's `daemon_ready`
/// is the second occurrence.
pub async fn await_daemon_event_count(
    session_root: &Path,
    name: &str,
    expected_count: usize,
    timeout: Duration,
) {
    let path = session_root.join("_daemon.events.jsonl");
    let needle = format!("\"event\":\"{name}\"");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            if content.matches(needle.as_str()).count() >= expected_count {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "event {name:?} count {expected_count} not reached within {timeout:?} in {}",
                path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Convenience wrapper: await the Nth `daemon_ready` (1-indexed) with a
/// 30-second budget. Use in fixtures that spawn the daemon multiple
/// times against the same `session_root`.
pub async fn await_daemon_ready_count(session_root: &Path, expected_count: usize) {
    await_daemon_event_count(
        session_root,
        "daemon_ready",
        expected_count,
        Duration::from_secs(30),
    )
    .await
}

/// Convenience wrapper: await `cold_start_completed`.
pub async fn await_cold_start_completed(session_root: &Path) -> Value {
    await_daemon_event(
        session_root,
        "cold_start_completed",
        Duration::from_secs(30),
    )
    .await
}

/// Mount a wiremock responder that answers the cold-start
/// `LinearGraphqlClient::enumerate` GraphQL query with an empty page.
/// Body match is `body_string_contains("issues(")` — the literal
/// substring of the GraphQL operation defined in
/// `crates/roki-daemon/src/linear/graphql.rs::build_query_body`. The
/// viewer query body does not contain `issues(`, so the existing
/// per-fixture viewer-stub keeps handling `viewer { id }` traffic.
///
/// The mock is mounted with priority 1 (highest) so it is consulted
/// before the per-fixture viewer-stub, which is registered with the
/// default priority 5 and uses only `method("POST")` as its matcher.
pub async fn stub_empty_issues(server: &wiremock::MockServer) {
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("POST"))
        .and(body_string_contains("issues("))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": []
                }
            }
        })))
        .with_priority(1)
        .mount(server)
        .await;
}

/// Build a single `nodes` entry for the cold-start `issues` page,
/// shaped per the GraphQL query in
/// `crates/roki-daemon/src/linear/graphql.rs`. Slice 6 e2e fixtures use
/// this to seed enumerate results.
pub fn issue_node(id: &str, identifier: &str, state: &str, assignee: &str) -> Value {
    serde_json::json!({
        "id": id,
        "identifier": identifier,
        "title": format!("{identifier} title"),
        "description": null,
        "state": { "name": state },
        "labels": { "nodes": [] },
        "assignee": { "id": assignee },
        "updatedAt": "2026-05-09T00:00:00Z"
    })
}
