# Slice 6 Cold Start and Admission Eviction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Linear cold-start enumeration that rebuilds the diff cache from scratch on every daemon launch, dispatches matching cycles with `cycle.trigger = cold_start`, reconciles orphan session tempdirs, and adds webhook-driven admission-filter eviction (cache-only — worktree + session_tempdir are retained until the ticket reaches a terminal state). Move `daemon_ready` emission to after `cold_start_completed` and land the paginated GraphQL primitive that polling will reuse later.

**Architecture:** New `daemon::cold_start` orchestrator drives `linear::graphql::LinearGraphqlClient::enumerate` (paginated `issues` query), populates `DiffCache`, and dispatches per-ticket cycles through a new `Dispatcher::admit_for_cold_start` entry point. New `daemon::orphan::reconcile` walks `<session_root>/` and deletes session tempdirs not in the admitted set. New `linear::rate_limit::RateLimitState` is a shared 429-backoff atom held by both `LinearClient` (viewer query) and `LinearGraphqlClient`. The `CycleTrigger` enum replaces the hardcoded `"runtime"` string in `PhaseContext` and threads `cold_start` through `ROKI_CYCLE_TRIGGER`. Admission-filter eviction adds a `pending_evict: bool` field to `CacheEntry` and a post-cycle check in the ticket task — eviction never deletes worktree or session_tempdir.

**Tech Stack:** Rust 2024 (workspace edition), `tokio` async runtime, slice 1-5 deps (`liquid`, `shell-words`, `async-trait`, `serde_json`, `serde`, `tempfile`, `wiremock`, `reqwest`, `nix`, `serde_yaml_ng`, `uuid`, `time`, `clap`, `axum`, `tower`). No new crates.

**Spec:** `docs/superpowers/specs/2026-05-09-slice6-cold-start-design.md` (committed in Task 0).

**Working branch:** `slice6-cold-start-spec` (already created; spec + FR doc updates committed there in Task 0). All implementation commits land on this branch.

---

## File Structure

### Created

| Path | Responsibility |
|---|---|
| `crates/roki-daemon/src/linear/rate_limit.rs` | `RateLimitState` — shared 429 backoff atom. Exponential 1s→60s, `Retry-After` override. |
| `crates/roki-daemon/src/linear/graphql.rs` | `LinearGraphqlClient` — paginated `issues` query primitive with assignee + status-union filter. Honors `RateLimitState`. |
| `crates/roki-daemon/src/daemon/orphan.rs` | `reconcile` — pure filesystem function deleting session tempdirs not in `keep_ids`. |
| `crates/roki-daemon/src/daemon/cold_start.rs` | `ColdStart::run` — enumerate → admit → cache observe → dispatch → orphan reconcile. |
| `crates/roki-daemon/tests/e2e/cold_start_two_tickets_smoke.rs` | E2E: GraphQL serves two tickets, both dispatch with `cycle.trigger = cold_start`. |
| `crates/roki-daemon/tests/e2e/cold_start_orphan_reconcile_smoke.rs` | E2E: pre-existing dirs not in Linear get deleted with `reason: orphan`. |
| `crates/roki-daemon/tests/e2e/cold_start_partial_enum_smoke.rs` | E2E: page-2 fails after retries → `enum_partial: true`, no orphan delete. |
| `crates/roki-daemon/tests/e2e/cold_start_backoff_smoke.rs` | E2E: 429 with Retry-After triggers backoff and retry; cold start completes. |
| `crates/roki-daemon/tests/e2e/eviction_in_flight_smoke.rs` | E2E: revoke-assignee mid-cycle → cache evict, worktree+session intact. |
| `crates/roki-daemon/tests/e2e/eviction_no_cycle_smoke.rs` | E2E: revoke for cached ticket with no in-flight cycle → immediate cache evict. |
| `crates/roki-daemon/tests/e2e/eviction_readmit_cancels_smoke.rs` | E2E: revoke + re-admit before cycle ends → eviction cancelled. |
| `crates/roki-daemon/tests/e2e/eviction_readmit_reuse_smoke.rs` | E2E: post-eviction re-admit reuses retained worktree. |
| `crates/roki-daemon/tests/e2e/cold_start_listener_parked_smoke.rs` | E2E: webhooks during cold start return 503; serve after `daemon_ready`. |
| `crates/roki-daemon/tests/e2e/cold_start_cleanup_mode_smoke.rs` | E2E: `roki cleanup` cold-start dispatches only `[[cleanup]]` matches. |
| `crates/roki-daemon/tests/e2e/support/cold_start.rs` | Shared helper `await_daemon_ready` + GraphQL wiremock fixtures. |

### Modified

| Path | Change |
|---|---|
| `crates/roki-daemon/src/linear/mod.rs` | Add `pub mod graphql; pub mod rate_limit;`. |
| `crates/roki-daemon/src/daemon/mod.rs` | Add `pub mod cold_start; pub mod orphan;`. |
| `crates/roki-daemon/src/engine/context.rs` | `CycleTrigger` enum; `PhaseContext::new(...)` takes `trigger: CycleTrigger`; `CycleView::trigger` derived from enum. |
| `crates/roki-daemon/src/engine/cycle.rs` | `run_cycle` accepts `trigger` param and forwards to `PhaseContext::new`. |
| `crates/roki-daemon/src/daemon/cache.rs` | `CacheEntry::pending_evict: bool`; `set_pending_evict`, `clear_pending_evict`, `take_pending_evict` methods. |
| `crates/roki-daemon/src/daemon/dispatcher.rs` | Eviction path on cached + admission-failed; `admit_for_cold_start` method. |
| `crates/roki-daemon/src/daemon/ticket_task.rs` | Accept `CycleTrigger` per cycle; consume `pending_evict` post-cycle (cache-only evict, no delete). |
| `crates/roki-daemon/src/linear/client.rs` | `LinearClient::new` accepts `RateLimitState`; viewer query honors it. |
| `crates/roki-daemon/src/runtime.rs` | Build `RateLimitState`, build `LinearGraphqlClient`, run `ColdStart::run` before opening `ready_gate`; emit events in order. |
| `crates/roki-daemon/src/events.rs` | Add `Event` variants: `ColdStartBegan`, `ColdStartCompleted`, `OrphanReconcileSkipped`, `StatusFilterDropped`, `LinearBackoffApplied`, `SessionTempdirDeleted`. Update `WebhookSkipReason` and `WebhookSkipped` to carry optional `source: cold_start`. |
| `crates/roki-daemon/Cargo.toml` | Add `[[test]]` entries for the ten new e2e files; add `tests/e2e/support/cold_start.rs` to test-support. |
| `docs/reference/log-events.md` | Add rows for `orphan_reconcile_skipped`, `status_filter_dropped`. Update `daemon_ready` description (drop interim qualifier). Update `webhook_skipped` to mention optional `source` field. (`worktree_deleted reason=eviction` removal already committed in Task 0.) |
| All slice 1-5 e2e fixtures that watch for `daemon_ready` | Helper updated in Task 14 to wait for `daemon_ready` line in `_daemon.events.jsonl` before sending the first webhook. |

---

## Cross-Task Conventions

- **Branch:** `slice6-cold-start-spec` (created in Task 0). All commits land here.
- **Test command:** `cargo test -p roki-daemon` for unit + e2e.
- **Build verification:** `cargo build -p roki-daemon` after each task. CI also runs `cargo clippy -p roki-daemon -- -D warnings` and `cargo fmt --check`.
- **No new crates.** If you find yourself reaching for `tower-http`, `governor`, `tracing-subscriber-extras`, etc. — re-read this line.
- **GraphQL endpoint override:** `LinearGraphqlClient::endpoint()` mirrors `LinearClient::endpoint()` from slice 1 — the hardcoded URL plus a `cfg(any(test, feature = "test-support"))`-gated env override `ROKI_LINEAR_GRAPHQL_URL`.
- **Page size override:** `ROKI_COLD_START_PAGE_SIZE` is read inside `cfg(any(test, feature = "test-support"))` only.
- **Daemon-scoped events** continue to use `EventWriter::open(session_root, "_daemon")` (slice 5) → `<session_root>/_daemon.events.jsonl`.
- **Per-ticket events** continue to use `EventWriter::open(session_root, ticket_id)`.
- **Module dead-code suppression** — new modules use `#![allow(dead_code)]` matching slice 1-5 modules until the runtime calls them; remove the suppression once `runtime::run_inner` reaches the new module.
- **Atomic ordering** — `RateLimitState`'s `AtomicU64` for `backoff_until_ms` writes with `Release`, reads with `Acquire`.
- **No `cycle.trigger = "polling"` or `"refresh"`.** Slice 6 only emits `runtime` and `cold_start`. Polling-driven cycles will use `runtime` per `ref:log-events §Common context fields` ("`cycle.trigger = runtime` covers webhook delivery, polling fallback, and refresh nudge driven cycles").

---

## Task 0: Branch + Spec Commit (DONE)

**Status:** Already complete. The spec lives at `docs/superpowers/specs/2026-05-09-slice6-cold-start-design.md`, the FR-doc updates (fr:01, fr:03, fr:05, fr:08, ref:log-events) are in place, and all of it was committed on `slice6-cold-start-spec` ahead of this plan.

If you arrived here on a different branch:

```bash
git checkout slice6-cold-start-spec
git pull --ff-only origin slice6-cold-start-spec  # if pushed
```

Otherwise no action required — proceed to Task 1.

---

## Task 1: `CycleTrigger` enum + `PhaseContext::new` widening

**Files:**
- Modify: `crates/roki-daemon/src/engine/context.rs`
- Modify: `crates/roki-daemon/src/engine/cycle.rs`
- Modify: `crates/roki-daemon/src/daemon/ticket_task.rs`
- Modify: `crates/roki-daemon/src/daemon/real_runner.rs`
- Modify: `crates/roki-daemon/src/runtime.rs` (test sites only — runtime call sites move in Task 9)

- [ ] **Step 1: Add the `CycleTrigger` enum**

In `crates/roki-daemon/src/engine/context.rs`, near the top after the imports:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleTrigger {
    Runtime,
    ColdStart,
}

impl CycleTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            CycleTrigger::Runtime => "runtime",
            CycleTrigger::ColdStart => "cold_start",
        }
    }
}
```

- [ ] **Step 2: Widen `PhaseContext::new`**

Change the signature and the field initializer:

```rust
impl PhaseContext {
    pub fn new(
        admitted: &AdmittedTicket,
        cycle_id: Uuid,
        cfg: &RokiConfig,
        cycle_kind: crate::engine::outcome::CycleKind,
        cycle_trigger: CycleTrigger,
    ) -> Self {
        Self {
            ticket: TicketView::from(&admitted.ticket),
            repo: RepoView {
                ghq: admitted.ghq.clone(),
                ticket_id: admitted.ticket.id.clone(),
            },
            cycle: CycleView {
                id: cycle_id.to_string(),
                kind: cycle_kind.as_str(),
                trigger: cycle_trigger.as_str(),
                iter: 0,
            },
            // ... unchanged ...
            config: ConfigView {
                max_iterations: cfg.engine.max_iterations,
            },
            pre: None,
            post: None,
            run: None,
            failure: None,
        }
    }
    // ... unchanged ...
}
```

- [ ] **Step 3: Update every call site to pass `CycleTrigger::Runtime`**

Sites to update:

1. `crates/roki-daemon/src/engine/cycle.rs` — `run_cycle` is the entry into the engine; add a `trigger: CycleTrigger` parameter and forward it to `PhaseContext::new`. Update `run_cycle` callers.
2. `crates/roki-daemon/src/daemon/ticket_task.rs` — every `engine::cycle::run_cycle(...)` call (rule, cleanup, failure handler) gets `CycleTrigger::Runtime` for now. The cold-start trigger is plumbed in Task 5.
3. `crates/roki-daemon/src/daemon/real_runner.rs` — if it constructs `PhaseContext::new` directly, pass `CycleTrigger::Runtime`.
4. `crates/roki-daemon/src/runtime.rs` — the existing test seam at the slice-5 single-cycle test site (if any) gets `CycleTrigger::Runtime`.
5. `crates/roki-daemon/src/engine/context.rs` test module — every existing `PhaseContext::new(...)` call inside `#[cfg(test)] mod tests` (lines 327, 369, 396, 414, 436, 454, 469, etc. per pre-edit grep) gets `CycleTrigger::Runtime` appended.

Run a grep to find them all:

```bash
grep -n "PhaseContext::new" crates/roki-daemon/src/
```

Expected result list: `engine/context.rs` (def + test sites), `engine/cycle.rs`, possibly one site in `daemon/`. Update each by adding `, CycleTrigger::Runtime` as the last argument.

- [ ] **Step 4: Add a unit test for the cold_start variant**

In `crates/roki-daemon/src/engine/context.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn cold_start_trigger_renders_into_cycle_view_and_env() {
    let cfg = stub_cfg();
    let id = Uuid::new_v4();
    let ctx = PhaseContext::new(
        &admitted(),
        id,
        &cfg,
        crate::engine::outcome::CycleKind::Rule,
        CycleTrigger::ColdStart,
    );
    assert_eq!(ctx.cycle.trigger, "cold_start");

    let env = ctx.env_pairs();
    assert!(
        env.iter()
            .any(|(k, v)| k == "ROKI_CYCLE_TRIGGER" && v == "cold_start")
    );
}
```

(Reuse whatever `stub_cfg()` and `admitted()` helpers the existing test module already defines. Adapt names if they differ.)

- [ ] **Step 5: Run the engine context tests**

```bash
cargo test -p roki-daemon engine::context
```

Expected: existing tests still pass (with the appended `Runtime` arg) plus the new `cold_start_trigger_renders_into_cycle_view_and_env` test passes.

- [ ] **Step 6: Build everything to flush remaining call-site mismatches**

```bash
cargo build -p roki-daemon --tests
```

Fix any compile errors by appending `CycleTrigger::Runtime` to the offending `PhaseContext::new` or `run_cycle` call. Repeat until clean.

- [ ] **Step 7: Run the full test suite**

```bash
cargo test -p roki-daemon
```

Expected: every slice 1-5 test still passes. No behavior change.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/engine/context.rs \
        crates/roki-daemon/src/engine/cycle.rs \
        crates/roki-daemon/src/daemon/ticket_task.rs \
        crates/roki-daemon/src/daemon/real_runner.rs \
        crates/roki-daemon/src/runtime.rs
git commit -m "refactor(engine): CycleTrigger enum threads trigger value through PhaseContext"
```

---

## Task 2: `linear::rate_limit::RateLimitState`

**Files:**
- Create: `crates/roki-daemon/src/linear/rate_limit.rs`
- Modify: `crates/roki-daemon/src/linear/mod.rs`

- [ ] **Step 1: Add the module declaration**

In `crates/roki-daemon/src/linear/mod.rs`, append:

```rust
pub mod rate_limit;
```

- [ ] **Step 2: Write the failing tests + implementation**

```rust
// crates/roki-daemon/src/linear/rate_limit.rs
#![allow(dead_code)]

//! Shared 429 backoff state for every Linear-bound request the daemon
//! makes.
//!
//! Both `LinearClient::resolve_viewer` and `LinearGraphqlClient::enumerate`
//! await `wait_if_backoff` before issuing a request, and call `record_429`
//! when Linear returns HTTP 429. Backoff is exponential (1s → 60s) with
//! `Retry-After` header overrides taking precedence when present.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use time::OffsetDateTime;
use tokio::time::sleep;

const MIN_BACKOFF_SECONDS: u64 = 1;
const MAX_BACKOFF_SECONDS: u64 = 60;

#[derive(Default, Clone)]
pub struct RateLimitState {
    /// Unix epoch milliseconds at which the backoff window ends. `0` =
    /// no backoff.
    backoff_until_ms: Arc<AtomicU64>,
    /// Last applied backoff in seconds (used for exponential growth).
    last_backoff_seconds: Arc<AtomicU64>,
}

impl RateLimitState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_in_backoff(&self) -> bool {
        let now = now_ms();
        self.backoff_until_ms.load(Ordering::Acquire) > now
    }

    pub async fn wait_if_backoff(&self) {
        let now = now_ms();
        let until = self.backoff_until_ms.load(Ordering::Acquire);
        if until > now {
            sleep(Duration::from_millis(until - now)).await;
        }
    }

    /// Record a 429 response. `retry_after` overrides the doubled value
    /// when supplied.
    pub fn record_429(&self, retry_after: Option<Duration>) -> Duration {
        let prior = self.last_backoff_seconds.load(Ordering::Acquire);
        let next_seconds = match retry_after {
            Some(d) => d.as_secs().clamp(MIN_BACKOFF_SECONDS, MAX_BACKOFF_SECONDS),
            None => {
                let doubled = (prior.max(MIN_BACKOFF_SECONDS / 2)) * 2;
                doubled.clamp(MIN_BACKOFF_SECONDS, MAX_BACKOFF_SECONDS)
            }
        };
        self.last_backoff_seconds
            .store(next_seconds, Ordering::Release);
        let until = now_ms() + next_seconds * 1000;
        self.backoff_until_ms.store(until, Ordering::Release);
        Duration::from_secs(next_seconds)
    }

    pub fn clear(&self) {
        self.backoff_until_ms.store(0, Ordering::Release);
        self.last_backoff_seconds.store(0, Ordering::Release);
    }
}

fn now_ms() -> u64 {
    let now = OffsetDateTime::now_utc().unix_timestamp_nanos();
    if now < 0 {
        0
    } else {
        (now / 1_000_000) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_state_has_no_backoff() {
        let s = RateLimitState::new();
        assert!(!s.is_in_backoff());
        // wait_if_backoff returns immediately
        tokio::time::timeout(Duration::from_millis(50), s.wait_if_backoff())
            .await
            .expect("should not block");
    }

    #[tokio::test]
    async fn record_429_without_retry_after_doubles() {
        let s = RateLimitState::new();
        let d1 = s.record_429(None);
        assert!(d1.as_secs() >= 1);
        let d2 = s.record_429(None);
        assert!(d2 >= d1);
        let d3 = s.record_429(None);
        assert!(d3 >= d2);
    }

    #[tokio::test]
    async fn record_429_caps_at_60_seconds() {
        let s = RateLimitState::new();
        // Force ten consecutive 429s — should saturate at 60.
        for _ in 0..10 {
            s.record_429(None);
        }
        let last = s.last_backoff_seconds.load(Ordering::Acquire);
        assert!(last <= 60);
        assert!(last >= 32);
    }

    #[tokio::test]
    async fn retry_after_overrides_doubled_value() {
        let s = RateLimitState::new();
        s.record_429(None);
        s.record_429(None);
        let d = s.record_429(Some(Duration::from_secs(5)));
        assert_eq!(d, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn retry_after_is_clamped_to_max() {
        let s = RateLimitState::new();
        let d = s.record_429(Some(Duration::from_secs(600)));
        assert_eq!(d, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn clear_resets_state() {
        let s = RateLimitState::new();
        s.record_429(None);
        assert!(s.is_in_backoff());
        s.clear();
        assert!(!s.is_in_backoff());
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p roki-daemon linear::rate_limit
```

Expected: 6 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/linear/rate_limit.rs \
        crates/roki-daemon/src/linear/mod.rs
git commit -m "feat(linear): RateLimitState with shared 429 backoff atom"
```

---

## Task 3: `linear::graphql::LinearGraphqlClient::enumerate`

**Files:**
- Create: `crates/roki-daemon/src/linear/graphql.rs`
- Modify: `crates/roki-daemon/src/linear/mod.rs`
- Modify: `crates/roki-daemon/src/error.rs` (add `LinearEnumerateError` variants)

- [ ] **Step 1: Add the module declaration**

In `crates/roki-daemon/src/linear/mod.rs`, append:

```rust
pub mod graphql;
```

- [ ] **Step 2: Add error variants**

In `crates/roki-daemon/src/error.rs`, find the section that defines `LinearClientError` and add a sibling enum:

```rust
#[derive(Debug, thiserror::Error)]
pub enum LinearEnumerateError {
    #[error("HTTP error talking to Linear at {endpoint}: {source}")]
    Http {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("Linear returned non-success status {status} from {endpoint}")]
    NonSuccess { endpoint: String, status: u16 },
    #[error("Malformed Linear response from {endpoint}: {reason}")]
    Malformed { endpoint: String, reason: String },
    #[error("GraphQL errors from Linear at {endpoint}: {message}")]
    GraphqlError { endpoint: String, message: String },
    #[error("Backoff exceeded retry budget after {retries} 429 responses")]
    BackoffExhausted { retries: u32 },
}
```

(If the existing `LinearClientError` already covers Http / NonSuccess / Malformed, prefer extending that one. Don't duplicate.)

- [ ] **Step 3: Write the failing wiremock-backed test scaffold**

Use the existing wiremock dev-dep pattern (search for `wiremock::MockServer` in slice 1-5 tests for the closest sibling). The shape:

```rust
// crates/roki-daemon/src/linear/graphql.rs
#![allow(dead_code)]

//! Paginated Linear GraphQL `issues(...)` primitive used by cold start
//! (and, in a future slice, by polling). Honors the shared
//! `RateLimitState` for 429 backoff.
//!
//! Returns `Vec<EnumeratedTicket>` after walking every page.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
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

            let json: Value = response
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

            let page = parse_issues_page(&json).map_err(|reason| {
                LinearEnumerateError::Malformed {
                    endpoint: endpoint.clone(),
                    reason,
                }
            })?;

            out.extend(page.tickets);

            if !page.has_next_page {
                break;
            }
            after = page.end_cursor;
            if after.is_none() {
                // Defensive: hasNextPage true but no cursor — abort to
                // avoid infinite loop. Treat as malformed.
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
    // Linear `issues(filter, first, after)` shape. The filter input shape
    // follows Linear's published IssueFilter type; integration tests in
    // this module assert the request body so any future Linear breaking
    // change surfaces here.
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
    let updated_at = OffsetDateTime::parse(updated_at_raw, &time::format_description::well_known::Rfc3339)
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

    #[tokio::test]
    async fn single_page_returns_all_tickets() {
        let server = MockServer::start().await;
        unsafe { std::env::set_var("ROKI_LINEAR_GRAPHQL_URL", &server.uri()) };

        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("Authorization", "tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(
                json!([issue("a1", "TEAM-1", "Todo", "u1"), issue("a2", "TEAM-2", "Todo", "u1")]),
                false,
                None,
            )))
            .mount(&server)
            .await;

        let c = LinearGraphqlClient::new("tok".into(), rl());
        let out = c
            .enumerate(&EnumerateRequest {
                assignee_id: "u1",
                status_filter: StatusFilter::None,
                page_size: DEFAULT_PAGE_SIZE,
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
        unsafe { std::env::set_var("ROKI_LINEAR_GRAPHQL_URL", &server.uri()) };

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

        let c = LinearGraphqlClient::new("tok".into(), rl());
        let out = c
            .enumerate(&EnumerateRequest {
                assignee_id: "u1",
                status_filter: StatusFilter::None,
                page_size: 1,
            })
            .await
            .expect("enumerate");
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn http_429_with_retry_after_triggers_backoff_then_retry() {
        let server = MockServer::start().await;
        unsafe { std::env::set_var("ROKI_LINEAR_GRAPHQL_URL", &server.uri()) };

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

        let rl = rl();
        let c = LinearGraphqlClient::new("tok".into(), rl.clone());
        let out = c
            .enumerate(&EnumerateRequest {
                assignee_id: "u1",
                status_filter: StatusFilter::None,
                page_size: DEFAULT_PAGE_SIZE,
            })
            .await
            .expect("enumerate after backoff");
        assert_eq!(out.len(), 1);
        assert!(!rl.is_in_backoff(), "success clears backoff");
    }

    #[tokio::test]
    async fn graphql_errors_array_surfaces_typed_error() {
        let server = MockServer::start().await;
        unsafe { std::env::set_var("ROKI_LINEAR_GRAPHQL_URL", &server.uri()) };

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{ "message": "bad token" }]
            })))
            .mount(&server)
            .await;

        let c = LinearGraphqlClient::new("tok".into(), rl());
        let err = c
            .enumerate(&EnumerateRequest {
                assignee_id: "u1",
                status_filter: StatusFilter::None,
                page_size: DEFAULT_PAGE_SIZE,
            })
            .await
            .expect_err("should surface graphql error");
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
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p roki-daemon linear::graphql
```

Expected: 6 tests pass. The 429 test takes ~1s (sleeps for `Retry-After: 1`).

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/linear/graphql.rs \
        crates/roki-daemon/src/linear/mod.rs \
        crates/roki-daemon/src/error.rs
git commit -m "feat(linear): paginated GraphQL issues primitive with 429 backoff"
```

---

## Task 4: `daemon::orphan::reconcile`

**Files:**
- Create: `crates/roki-daemon/src/daemon/orphan.rs`
- Modify: `crates/roki-daemon/src/daemon/mod.rs`

- [ ] **Step 1: Add the module declaration**

In `crates/roki-daemon/src/daemon/mod.rs`, append:

```rust
pub mod orphan;
```

- [ ] **Step 2: Write the failing tests + implementation**

```rust
// crates/roki-daemon/src/daemon/orphan.rs
#![allow(dead_code)]

//! Session-tempdir orphan reconcile (fr:07 §Cold start step 5).
//!
//! Walks `<session_root>/`, deletes every directory whose name is not in
//! `keep_ids`, and emits one `session_tempdir_deleted { reason: "orphan" }`
//! per deletion. The reserved `_daemon/` directory is skipped.

use std::collections::HashSet;
use std::path::Path;

use crate::events::{Event, EventWriter, SessionTempdirDeleteReason, now_rfc3339};

pub struct OrphanScan<'a> {
    pub session_root: &'a Path,
    pub keep_ids: &'a HashSet<String>,
}

#[derive(Debug, Default)]
pub struct OrphanReport {
    pub deleted: Vec<String>,
    pub fs_errors: Vec<(String, std::io::Error)>,
}

pub async fn reconcile(scan: OrphanScan<'_>, writer: &mut EventWriter) -> OrphanReport {
    let mut report = OrphanReport::default();

    let read_dir = match tokio::fs::read_dir(scan.session_root).await {
        Ok(d) => d,
        Err(_) => return report,
    };
    tokio::pin!(read_dir);

    let mut entries = read_dir;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let file_name = entry.file_name();
        let name = match file_name.to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        if name == "_daemon" || name.starts_with("_daemon.") {
            continue;
        }

        let ft = match entry.file_type().await {
            Ok(ft) => ft,
            Err(e) => {
                report.fs_errors.push((name, e));
                continue;
            }
        };
        if !ft.is_dir() {
            // Per-ticket *.events.jsonl files live next to the dirs;
            // they are not orphans on their own — they are deleted
            // alongside the matching dir if needed. A future slice may
            // sweep stale .events.jsonl when no dir exists; out of slice 6.
            continue;
        }

        if scan.keep_ids.contains(&name) {
            continue;
        }

        let path = entry.path();
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {
                let _ = writer.emit(&Event::SessionTempdirDeleted {
                    ts: now_rfc3339(),
                    ticket_id: name.clone(),
                    path: path.display().to_string(),
                    reason: SessionTempdirDeleteReason::Orphan,
                });
                report.deleted.push(name);
            }
            Err(e) => {
                report.fs_errors.push((name, e));
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn writer_in(root: &Path) -> EventWriter {
        EventWriter::open(root, "_daemon").expect("open writer")
    }

    fn keep(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn empty_session_root_is_noop() {
        let tmp = TempDir::new().unwrap();
        let mut w = writer_in(tmp.path());
        let report = reconcile(
            OrphanScan {
                session_root: tmp.path(),
                keep_ids: &keep(&[]),
            },
            &mut w,
        )
        .await;
        assert!(report.deleted.is_empty());
        assert!(report.fs_errors.is_empty());
    }

    #[tokio::test]
    async fn orphans_deleted_kept_preserved() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("ticket-keep")).unwrap();
        fs::create_dir_all(tmp.path().join("ticket-orphan")).unwrap();
        let mut w = writer_in(tmp.path());

        let report = reconcile(
            OrphanScan {
                session_root: tmp.path(),
                keep_ids: &keep(&["ticket-keep"]),
            },
            &mut w,
        )
        .await;

        assert_eq!(report.deleted, vec!["ticket-orphan".to_string()]);
        assert!(tmp.path().join("ticket-keep").is_dir());
        assert!(!tmp.path().join("ticket-orphan").exists());
    }

    #[tokio::test]
    async fn daemon_directory_is_skipped() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("_daemon")).unwrap();
        let mut w = writer_in(tmp.path());

        let report = reconcile(
            OrphanScan {
                session_root: tmp.path(),
                keep_ids: &keep(&[]),
            },
            &mut w,
        )
        .await;
        assert!(report.deleted.is_empty());
        assert!(tmp.path().join("_daemon").exists());
    }

    #[tokio::test]
    async fn non_directory_entries_are_skipped() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("loose.jsonl"), b"").unwrap();
        let mut w = writer_in(tmp.path());

        let report = reconcile(
            OrphanScan {
                session_root: tmp.path(),
                keep_ids: &keep(&[]),
            },
            &mut w,
        )
        .await;
        assert!(report.deleted.is_empty());
        assert!(tmp.path().join("loose.jsonl").exists());
    }
}
```

(`Event::SessionTempdirDeleted` and `SessionTempdirDeleteReason` are added in Task 8. Until Task 8 lands the tests in Task 4 will fail to compile — that's fine: complete Task 4 implementation first, then return after Task 8 to run the test suite. Alternatively swap the task ordering with Task 8. The plan is presented in dependency order; agents that prefer compile-on-each-task should do Task 8 before Task 4's `cargo test`.)

- [ ] **Step 3: Build to verify the module declares cleanly (compile errors expected from missing Event variants are OK at this point)**

```bash
cargo build -p roki-daemon --lib 2>&1 | tail -20
```

Expected: errors mentioning `Event::SessionTempdirDeleted not found` — that's the cross-task linkage. Continue to Task 5; tests run after Task 8.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/daemon/orphan.rs \
        crates/roki-daemon/src/daemon/mod.rs
git commit -m "feat(daemon): orphan session-tempdir reconcile primitive"
```

---

## Task 5: `daemon::cold_start::ColdStart`

**Files:**
- Create: `crates/roki-daemon/src/daemon/cold_start.rs`
- Modify: `crates/roki-daemon/src/daemon/mod.rs`
- Modify: `crates/roki-daemon/src/daemon/dispatcher.rs` (`admit_for_cold_start` extension; full implementation in Task 6)
- Modify: `crates/roki-daemon/src/linear/ticket.rs` (relax `pub(crate) fn new` if needed for cold-start synth — see Step 4)

- [ ] **Step 1: Add the module declaration**

In `crates/roki-daemon/src/daemon/mod.rs`, append:

```rust
pub mod cold_start;
```

- [ ] **Step 2: Compute the status union from WorkflowConfig**

This is a pure function. Write it first:

```rust
// crates/roki-daemon/src/daemon/cold_start.rs (top of file)
#![allow(dead_code)]

//! Cold-start enumeration + dispatch + orphan reconcile (fr:07 §Cold start).
//!
//! Runs once at every daemon launch before `daemon_ready` is emitted.
//! Walks Linear's paginated `issues` query, populates `DiffCache`, spawns
//! per-ticket cycles via `Dispatcher::admit_for_cold_start`, then deletes
//! orphan session tempdirs. Cycles dispatched here run async on the
//! existing per-ticket task model — cold start does not block on cycle
//! completion.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use time::OffsetDateTime;

use crate::admission::{self, AdmittedTicket};
use crate::config::roki::RokiConfig;
use crate::config::workflow::{RuleEntry, WorkflowConfig};
use crate::daemon::cache::DiffCache;
use crate::daemon::dispatcher::Dispatcher;
use crate::daemon::orphan::{self, OrphanScan};
use crate::engine::dispatch::DispatchMode;
use crate::events::{Event, EventWriter, WebhookSkipReason, WebhookSkipSource, now_rfc3339};
use crate::linear::client::MeId;
use crate::linear::graphql::{
    EnumerateRequest, EnumeratedTicket, LinearGraphqlClient, StatusFilter,
};
use crate::linear::ticket::NormalizedTicket;

#[derive(Debug, Default)]
pub struct ColdStartReport {
    pub enumerated: usize,
    pub admitted: usize,
    pub cycles_spawned: usize,
    pub orphans_deleted: usize,
    pub enum_partial: bool,
    pub partial_reason: Option<String>,
    pub partial_error_text: Option<String>,
}

pub struct ColdStart {
    pub cfg: Arc<RokiConfig>,
    pub workflow: Arc<WorkflowConfig>,
    pub me: Option<MeId>,
    pub cache: Arc<DiffCache>,
    pub dispatcher: Arc<Dispatcher>,
    pub graphql: Arc<LinearGraphqlClient>,
    pub mode: DispatchMode,
}

/// Compute the status-union narrowing for the GraphQL filter.
///
/// Returns `None` when any rule or cleanup entry omits `when.status`
/// (the filter is dropped per `fr:07 step 2`); the caller emits
/// `status_filter_dropped` in that case.
pub fn compute_status_union(workflow: &WorkflowConfig) -> Option<(BTreeSet<String>, Option<String>)> {
    let mut union: BTreeSet<String> = BTreeSet::new();
    let mut all_have_status = true;
    let mut first_dropping_entry: Option<String> = None;

    for entry in iter_all_rule_entries(workflow) {
        match entry.when_status() {
            Some(s) => {
                union.insert(s.to_string());
            }
            None => {
                if first_dropping_entry.is_none() {
                    first_dropping_entry = Some(entry.name().to_string());
                }
                all_have_status = false;
            }
        }
    }

    if all_have_status {
        Some((union, None))
    } else {
        Some((BTreeSet::new(), first_dropping_entry))
    }
}

// `iter_all_rule_entries` and the `when_status` / `name` accessors below
// adapt to the existing slice-1 WorkflowConfig shape. If the entry types
// already expose these accessors directly, drop the trait shim and use
// them. Inspect `crates/roki-daemon/src/config/workflow.rs` first.

trait RuleLike {
    fn when_status(&self) -> Option<&str>;
    fn name(&self) -> &str;
}
// Implementations live next to the WorkflowConfig types — implement them
// in `config/workflow.rs` for whatever struct represents a `[[rule]]` /
// `[[cleanup]]` entry. The existing slice-1 `RuleEntry` likely already
// has analogous fields; if not, expose them.

fn iter_all_rule_entries(_w: &WorkflowConfig) -> impl Iterator<Item = &dyn RuleLike> {
    // Iterate every [[rule]] then every [[cleanup]]. Per-repo TOML
    // entries are flattened into the same iteration. The exact shape
    // depends on `WorkflowConfig`'s slice-1 layout; mirror what
    // `engine::dispatch::evaluate` already iterates.
    std::iter::empty::<&dyn RuleLike>()
}
```

(The shim trait is a placeholder. The real implementation will mirror however slice 1's `engine::dispatch::evaluate` walks `[[rule]]` and `[[cleanup]]` entries. Look at `crates/roki-daemon/src/engine/dispatch.rs` first to understand the existing iteration; copy that pattern into `compute_status_union`.)

- [ ] **Step 3: Implement `ColdStart::run`**

```rust
impl ColdStart {
    pub async fn run(&self, writer: &mut EventWriter) -> ColdStartReport {
        let mut report = ColdStartReport::default();

        // 1. Compute status union.
        let assignee_id = self.resolve_assignee_id_string();
        let (status_set, dropped_entry) = match compute_status_union(&self.workflow) {
            Some(s) => s,
            None => (BTreeSet::new(), None),
        };

        if let Some(name) = dropped_entry {
            let _ = writer.emit(&Event::StatusFilterDropped {
                ts: now_rfc3339(),
                entry: name,
                reason: "any-state-rule".into(),
            });
        }

        let states_vec: Vec<&str> = status_set.iter().map(String::as_str).collect();
        let status_filter = if states_vec.is_empty() {
            StatusFilter::None
        } else {
            StatusFilter::Union(&states_vec)
        };

        let page_size = page_size_from_env();

        // 2. Enumerate.
        let enumerated = match self
            .graphql
            .enumerate(&EnumerateRequest {
                assignee_id: &assignee_id,
                status_filter,
                page_size,
            })
            .await
        {
            Ok(v) => v,
            Err(e) => {
                report.enum_partial = true;
                report.partial_reason = Some(classify_partial_reason(&e));
                report.partial_error_text = Some(e.to_string());
                Vec::new() // proceed with empty set; orphan reconcile is skipped (§4.6).
            }
        };
        report.enumerated = enumerated.len();

        // 3. Cache populate + admission re-eval + dispatch.
        let mut keep_ids: HashSet<String> = HashSet::new();

        for et in enumerated {
            let nt = synth_normalized(&et);
            match admission::accept(&nt, &self.workflow, self.me.as_ref()) {
                Ok(admitted) => {
                    keep_ids.insert(admitted.ticket.id.clone());
                    report.admitted += 1;

                    let outcome = self.cache.observe(&admitted).await;
                    let _ = outcome; // NewEntry on a fresh cold start; ignored

                    if self
                        .dispatcher
                        .admit_for_cold_start(admitted.clone())
                        .await
                        .is_ok()
                    {
                        report.cycles_spawned += 1;
                    }
                }
                Err(err) => {
                    let _ = writer.emit(&Event::WebhookSkipped {
                        ts: now_rfc3339(),
                        ticket_id: et.id.clone(),
                        reason: classify_webhook_skip_reason(&err),
                        source: Some(WebhookSkipSource::ColdStart),
                    });
                }
            }
        }

        // 4. Orphan reconcile (skip on partial enum per §4.6).
        if report.enum_partial {
            let _ = writer.emit(&Event::OrphanReconcileSkipped {
                ts: now_rfc3339(),
                reason: "cold_start_partial".into(),
            });
        } else {
            let scan = OrphanScan {
                session_root: &self.cfg.paths.session_root,
                keep_ids: &keep_ids,
            };
            let orphan_report = orphan::reconcile(scan, writer).await;
            report.orphans_deleted = orphan_report.deleted.len();
        }

        report
    }

    fn resolve_assignee_id_string(&self) -> String {
        match (&self.workflow.admission.assignee, &self.me) {
            (a, _) if a != "me" => a.clone(),
            (_, Some(MeId(id))) => id.clone(),
            (_, None) => String::new(), // workflow validation should have caught this
        }
    }
}

fn synth_normalized(et: &EnumeratedTicket) -> NormalizedTicket {
    NormalizedTicket::new_for_cold_start(
        et.id.clone(),
        et.assignee_id.clone(),
        et.state_name.clone(),
        et.label_names.iter().cloned().collect(),
        et.title.clone(),
        et.description.clone().unwrap_or_default(),
    )
}

fn page_size_from_env() -> u32 {
    #[cfg(any(test, feature = "test-support"))]
    {
        if let Ok(s) = std::env::var("ROKI_COLD_START_PAGE_SIZE") {
            if let Ok(n) = s.parse::<u32>() {
                if n > 0 {
                    return n;
                }
            }
        }
    }
    crate::linear::graphql::DEFAULT_PAGE_SIZE
}

fn classify_partial_reason(err: &crate::error::LinearEnumerateError) -> String {
    use crate::error::LinearEnumerateError::*;
    match err {
        GraphqlError { .. } => "graphql_error".into(),
        Http { .. } | NonSuccess { .. } | Malformed { .. } | BackoffExhausted { .. } => {
            "linear_unreachable".into()
        }
    }
}

fn classify_webhook_skip_reason(err: &crate::error::AdmissionError) -> WebhookSkipReason {
    use crate::error::AdmissionError;
    match err {
        AdmissionError::AssigneeMismatch { .. } => WebhookSkipReason::AssigneeMismatch,
        AdmissionError::RepoUnresolvable { .. } => WebhookSkipReason::RepoUnresolvable,
        _ => WebhookSkipReason::AssigneeMismatch, // be conservative
    }
}
```

- [ ] **Step 4: Add `NormalizedTicket::new_for_cold_start`**

`NormalizedTicket::new` is `pub(crate)` and only used by webhook normalization. Add a sibling cold-start constructor with the same body but a different name to make the call site auditable:

```rust
// in crates/roki-daemon/src/linear/ticket.rs
impl NormalizedTicket {
    /// Cold-start constructor. Identical to `new` but the distinct name
    /// keeps cold-start synthesized tickets greppable.
    pub(crate) fn new_for_cold_start(
        id: String,
        assignee_id: Option<String>,
        status: String,
        labels: Vec<String>,
        title: String,
        body: String,
    ) -> Self {
        Self::new(id, assignee_id, status, labels, title, body)
    }
}
```

- [ ] **Step 5: Add a stub `Dispatcher::admit_for_cold_start`**

In `crates/roki-daemon/src/daemon/dispatcher.rs`, add a method stub returning `Ok(())` for now; the real implementation lands in Task 6:

```rust
impl Dispatcher {
    /// Cold-start admission entry point. Spawns a per-ticket task with
    /// the given admitted ticket and binds `CycleTrigger::ColdStart` for
    /// the first cycle. Subsequent webhook-driven cycles for the same
    /// ticket use `CycleTrigger::Runtime`.
    pub async fn admit_for_cold_start(&self, admitted: AdmittedTicket) -> Result<(), DispatchError> {
        // Stub. Real impl in Task 6.
        let _ = admitted;
        Ok(())
    }
}
```

(Pick a `DispatchError` type or `()` consistent with whatever the dispatcher already uses. Look at the existing return types in `dispatcher.rs` first.)

- [ ] **Step 6: Write unit tests for `compute_status_union`**

Once the `RuleLike` shim is replaced with the real iteration over `WorkflowConfig`'s `[[rule]]` / `[[cleanup]]` lists, add:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn workflow_with(statuses: Vec<Option<&str>>) -> WorkflowConfig {
        // Build a WorkflowConfig with one [[rule]] per entry in `statuses`.
        // None means the rule omits when.status. Mirror the slice-1 test
        // helper for WorkflowConfig construction.
        unimplemented!("mirror slice-1 workflow_with helper from existing tests")
    }

    #[test]
    fn all_explicit_statuses_form_union() {
        let w = workflow_with(vec![Some("Todo"), Some("InProgress"), Some("Todo")]);
        let (set, dropped) = compute_status_union(&w).unwrap();
        assert_eq!(set.len(), 2);
        assert!(dropped.is_none());
    }

    #[test]
    fn missing_status_drops_filter() {
        let w = workflow_with(vec![Some("Todo"), None, Some("InProgress")]);
        let (set, dropped) = compute_status_union(&w).unwrap();
        assert!(set.is_empty());
        assert!(dropped.is_some());
    }
}
```

The `workflow_with` helper is the only piece you need to copy from the existing test scaffolding in `engine/dispatch.rs` or `config/workflow.rs`.

- [ ] **Step 7: Build (compile errors from missing Event variants still expected)**

```bash
cargo build -p roki-daemon --lib 2>&1 | tail -30
```

Address any compile errors that are NOT about the missing Event variants — those are linkage with Tasks 6/7/8.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/daemon/cold_start.rs \
        crates/roki-daemon/src/daemon/mod.rs \
        crates/roki-daemon/src/daemon/dispatcher.rs \
        crates/roki-daemon/src/linear/ticket.rs
git commit -m "feat(daemon): ColdStart orchestrator with status-union narrowing"
```

---

## Task 6: Dispatcher cold-start path + ticket task accepts trigger param

**Files:**
- Modify: `crates/roki-daemon/src/daemon/dispatcher.rs`
- Modify: `crates/roki-daemon/src/daemon/ticket_task.rs`

- [ ] **Step 1: Implement `Dispatcher::admit_for_cold_start`**

Replace the Task 5 stub. Mirror the existing webhook intake spawn path; differences:

1. Skip the `cache.observe` step (cold-start caller already did it).
2. Bind the first cycle's trigger to `CycleTrigger::ColdStart` via a per-call inbox seed message: introduce a new `DispatchMsg::ColdStartCycle(AdmittedTicket)` variant alongside the existing `DispatchMsg::Webhook(AdmittedTicket)` from slice 5.

Open `crates/roki-daemon/src/daemon/ticket_task.rs` and add the variant:

```rust
pub enum DispatchMsg {
    Webhook(AdmittedTicket),
    ColdStartCycle(AdmittedTicket),
    Shutdown,
}
```

Update the loop's `match` to handle `ColdStartCycle` identically to `Webhook` *except* the `CycleTrigger::ColdStart` argument it threads into `engine::cycle::run_cycle`. After the cold-start cycle ends, the loop continues normally — subsequent inbox messages are `Webhook` variants which use `CycleTrigger::Runtime`.

```rust
// inside the ticket task loop body
let (admitted, trigger) = match msg {
    DispatchMsg::Shutdown => break,
    DispatchMsg::Webhook(a) => (a, CycleTrigger::Runtime),
    DispatchMsg::ColdStartCycle(a) => (a, CycleTrigger::ColdStart),
};
// ... existing slice-5 dispatch path, passing `trigger` into run_cycle.
```

- [ ] **Step 2: Wire the dispatcher entry point**

```rust
impl Dispatcher {
    pub async fn admit_for_cold_start(&self, admitted: AdmittedTicket) -> Result<(), DispatchError> {
        let ticket_id = admitted.ticket.id.clone();
        let mut tickets = self.tickets.lock().await;

        // Slice-5 invariant: at most one ticket task per ticket.
        // Cold start runs before the listener accepts traffic, so the
        // registry is empty for this ticket; assert it.
        debug_assert!(!tickets.contains_key(&ticket_id), "cold_start before listener");

        let handle = self.spawn_ticket_task(ticket_id.clone()); // existing slice-5 helper
        handle
            .inbox
            .send(DispatchMsg::ColdStartCycle(admitted))
            .await
            .map_err(|_| DispatchError::InboxClosed)?;
        tickets.insert(ticket_id, handle);
        Ok(())
    }
}
```

If the slice-5 dispatcher does not have a `spawn_ticket_task` helper, the existing inline spawn in `Dispatcher::on_webhook` should be lifted into one. Mirror the slice-5 spawn site exactly so the per-ticket actor lifecycle stays identical.

- [ ] **Step 3: Add unit tests in `dispatcher.rs`**

```rust
#[tokio::test]
async fn admit_for_cold_start_spawns_task_and_runs_first_cycle_with_cold_start_trigger() {
    let (cfg, workflow, me, cache, dispatcher, mut events_rx) = test_harness().await;
    let admitted = stub_admitted("ticket-1", "Todo", &[], "u1");

    cache.observe(&admitted).await;
    dispatcher
        .admit_for_cold_start(admitted)
        .await
        .expect("admit");

    let started = wait_for_event(&mut events_rx, "cycle_started").await;
    assert_eq!(started["cycle"]["trigger"], "cold_start");
}

#[tokio::test]
async fn webhook_after_cold_start_uses_runtime_trigger() {
    let (cfg, workflow, me, cache, dispatcher, mut events_rx) = test_harness().await;
    let admitted = stub_admitted("ticket-2", "Todo", &[], "u1");
    cache.observe(&admitted).await;
    dispatcher.admit_for_cold_start(admitted.clone()).await.unwrap();

    // Wait for cold-start cycle to complete, then send a webhook update
    let _ = wait_for_event(&mut events_rx, "cycle_completed").await;
    let next = stub_admitted("ticket-2", "InProgress", &[], "u1");
    dispatcher.on_webhook(next).await; // existing slice-5 entry
    let started2 = wait_for_event(&mut events_rx, "cycle_started").await;
    assert_eq!(started2["cycle"]["trigger"], "runtime");
}
```

(`test_harness`, `stub_admitted`, `wait_for_event` are slice-5 helpers in the same module — reuse them.)

- [ ] **Step 4: Run dispatcher + ticket_task tests**

```bash
cargo test -p roki-daemon daemon::dispatcher daemon::ticket_task
```

Expected: every existing slice-5 test still passes; two new tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/daemon/dispatcher.rs \
        crates/roki-daemon/src/daemon/ticket_task.rs
git commit -m "feat(daemon): cold-start dispatch entry plus ColdStartCycle inbox variant"
```

---

## Task 7: Admission-filter eviction (cache-only, worktree retained)

**Files:**
- Modify: `crates/roki-daemon/src/daemon/cache.rs`
- Modify: `crates/roki-daemon/src/daemon/dispatcher.rs`
- Modify: `crates/roki-daemon/src/daemon/ticket_task.rs`

- [ ] **Step 1: Add `pending_evict` to `CacheEntry`**

In `crates/roki-daemon/src/daemon/cache.rs`:

```rust
pub struct CacheEntry {
    /* ...slice 5 fields... */
    pub pending_evict: bool,
}
```

Initialize to `false` in the slice-5 `observe` insert path. Then add accessors:

```rust
impl DiffCache {
    pub async fn set_pending_evict(&self, ticket_id: &str) {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            e.pending_evict = true;
        }
    }

    pub async fn clear_pending_evict(&self, ticket_id: &str) {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            e.pending_evict = false;
        }
    }

    pub async fn take_pending_evict(&self, ticket_id: &str) -> bool {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            let prior = e.pending_evict;
            e.pending_evict = false;
            prior
        } else {
            false
        }
    }
}
```

- [ ] **Step 2: Add cache unit tests**

```rust
#[tokio::test]
async fn set_then_take_pending_evict_clears_flag() {
    let c = DiffCache::new();
    c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
    assert!(!c.take_pending_evict("t1").await);
    c.set_pending_evict("t1").await;
    assert!(c.take_pending_evict("t1").await);
    assert!(!c.take_pending_evict("t1").await);
}

#[tokio::test]
async fn clear_pending_evict_resets_without_taking() {
    let c = DiffCache::new();
    c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
    c.set_pending_evict("t1").await;
    c.clear_pending_evict("t1").await;
    assert!(!c.take_pending_evict("t1").await);
}

#[tokio::test]
async fn pending_evict_on_missing_ticket_is_noop() {
    let c = DiffCache::new();
    c.set_pending_evict("missing").await;
    assert!(!c.take_pending_evict("missing").await);
}
```

- [ ] **Step 3: Run cache tests**

```bash
cargo test -p roki-daemon daemon::cache
```

Expected: all pass.

- [ ] **Step 4: Update dispatcher path on admission-failed-while-cached**

Find the slice-5 `Dispatcher::on_webhook` (or equivalent) admission-rejection branch. After emitting `webhook_skipped`:

```rust
// existing slice-5 emission of webhook_skipped continues
if cache.snapshot(&ticket_id).await.is_some() {
    cache.set_pending_evict(&ticket_id).await;
    let tickets = self.tickets.lock().await;
    if !tickets.contains_key(&ticket_id) {
        // No in-flight cycle and no ticket task: reclaim cache
        // immediately. Worktree + session_tempdir are retained.
        drop(tickets);
        cache.evict(&ticket_id).await;
    }
}
```

On the admission-success branch, before continuing with `cache.observe`:

```rust
// Re-admission cancels any pending eviction.
if let Some(snap) = cache.snapshot(&ticket_id).await {
    if snap.pending_evict {
        cache.clear_pending_evict(&ticket_id).await;
    }
}
```

- [ ] **Step 5: Update ticket task post-cycle handling**

Locate the slice-5 ticket-task loop body. After `engine::cycle::run_cycle` returns and after the slice-5 cleanup-cycle delete branch handles its own eviction, BEFORE the slice-5 `take_pending_recheck` check, insert:

```rust
if cache.take_pending_evict(&ticket_id).await {
    // Cache-only evict per fr:03 + fr:05 (slice 6). Worktree and
    // session_tempdir are retained; reclaim happens on cleanup-cycle
    // completion or cold-start orphan reconcile.
    cache.evict(&ticket_id).await;
    break;
}
```

- [ ] **Step 6: Add ticket-task unit test**

```rust
#[tokio::test]
async fn pending_evict_after_cycle_evicts_cache_and_exits_task() {
    let (cache, dispatcher, _events) = harness().await;
    let admitted = stub_admitted("t1", "Todo", &[], "u1");
    cache.observe(&admitted).await;
    dispatcher.on_webhook(admitted).await;          // queues first cycle

    // While first cycle is in flight, set pending_evict.
    cache.set_pending_evict("t1").await;

    // After cycle returns, ticket task observes the flag and exits.
    wait_for_task_exit("t1").await;
    assert!(cache.snapshot("t1").await.is_none());
}

#[tokio::test]
async fn re_admission_clears_pending_evict() {
    let (cache, dispatcher, _events) = harness().await;
    let admitted = stub_admitted("t1", "Todo", &[], "u1");
    cache.observe(&admitted).await;
    dispatcher.on_webhook(admitted.clone()).await;
    cache.set_pending_evict("t1").await;

    // Re-admit by sending another webhook with the same passing assignee.
    dispatcher.on_webhook(admitted).await;
    let snap = cache.snapshot("t1").await.unwrap();
    assert!(!snap.pending_evict);
}
```

- [ ] **Step 7: Run tests**

```bash
cargo test -p roki-daemon daemon::dispatcher daemon::ticket_task daemon::cache
```

Expected: all slice-5 + new tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/daemon/cache.rs \
        crates/roki-daemon/src/daemon/dispatcher.rs \
        crates/roki-daemon/src/daemon/ticket_task.rs
git commit -m "feat(daemon): admission-filter eviction (cache-only, worktree retained)"
```

---

## Task 8: Event variants + WebhookSkipped source field

**Files:**
- Modify: `crates/roki-daemon/src/events.rs`

- [ ] **Step 1: Add new variants and supporting enums**

```rust
// in crates/roki-daemon/src/events.rs, inside `pub enum Event` and supporting types:

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebhookSkipSource {
    Webhook,    // default; omitted when the event row has no `source` field
    ColdStart,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionTempdirDeleteReason {
    Cleanup,
    Orphan,
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /* existing variants */

    ColdStartBegan {
        ts: String,
        roki_toml_path: String,
        workflow_toml_path: String,
    },
    ColdStartCompleted {
        ts: String,
        enumerated: usize,
        admitted: usize,
        cycles_spawned: usize,
        orphans_deleted: usize,
        enum_partial: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial_error_text: Option<String>,
    },
    OrphanReconcileSkipped {
        ts: String,
        reason: String,
    },
    StatusFilterDropped {
        ts: String,
        entry: String,
        reason: String,
    },
    LinearBackoffApplied {
        ts: String,
        backoff_seconds: u64,
    },
    SessionTempdirDeleted {
        ts: String,
        ticket_id: String,
        path: String,
        reason: SessionTempdirDeleteReason,
    },
}
```

- [ ] **Step 2: Update `WebhookSkipped` to carry optional `source`**

```rust
WebhookSkipped {
    ts: String,
    ticket_id: String,
    reason: WebhookSkipReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<WebhookSkipSource>,
},
```

Update every existing emission site (slice 1-5) to pass `source: None`. Webhook-driven emissions stay unchanged at the JSON level — the field is omitted when None.

- [ ] **Step 3: Add tests**

```rust
#[test]
fn cold_start_completed_serializes_partial_fields_when_present() {
    let e = Event::ColdStartCompleted {
        ts: "2026-05-09T00:00:00Z".into(),
        enumerated: 5,
        admitted: 3,
        cycles_spawned: 3,
        orphans_deleted: 0,
        enum_partial: true,
        partial_reason: Some("linear_unreachable".into()),
        partial_error_text: Some("timeout".into()),
    };
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["event"], "cold_start_completed");
    assert_eq!(v["enum_partial"], true);
    assert_eq!(v["partial_reason"], "linear_unreachable");
}

#[test]
fn cold_start_completed_omits_partial_fields_on_success() {
    let e = Event::ColdStartCompleted {
        ts: "2026-05-09T00:00:00Z".into(),
        enumerated: 5,
        admitted: 5,
        cycles_spawned: 5,
        orphans_deleted: 2,
        enum_partial: false,
        partial_reason: None,
        partial_error_text: None,
    };
    let v = serde_json::to_value(&e).unwrap();
    assert!(v.get("partial_reason").is_none());
}

#[test]
fn webhook_skipped_omits_source_when_none() {
    let e = Event::WebhookSkipped {
        ts: "2026-05-09T00:00:00Z".into(),
        ticket_id: "t1".into(),
        reason: WebhookSkipReason::AssigneeMismatch,
        source: None,
    };
    let v = serde_json::to_value(&e).unwrap();
    assert!(v.get("source").is_none());
}

#[test]
fn webhook_skipped_with_cold_start_source_serializes_field() {
    let e = Event::WebhookSkipped {
        ts: "2026-05-09T00:00:00Z".into(),
        ticket_id: "t1".into(),
        reason: WebhookSkipReason::AssigneeMismatch,
        source: Some(WebhookSkipSource::ColdStart),
    };
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["source"], "cold_start");
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p roki-daemon events
```

Expected: existing tests still pass; four new tests pass.

- [ ] **Step 5: Build the whole crate to confirm Task 4 / Task 5 modules now compile**

```bash
cargo build -p roki-daemon
```

Expected: clean. If `WorkflowConfig` accessors in Task 5's `iter_all_rule_entries` shim are still TODO, fix them now by mirroring `engine::dispatch::evaluate`'s iteration.

```bash
cargo test -p roki-daemon daemon::orphan daemon::cold_start
```

Expected: orphan tests pass; `compute_status_union` tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/events.rs
git commit -m "feat(events): cold-start, orphan, backoff event variants"
```

---

## Task 9: `runtime::run` rewire — cold start gates `daemon_ready`

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`
- Modify: `crates/roki-daemon/src/linear/webhook.rs` (axum router 503 gate)
- Modify: `crates/roki-daemon/src/linear/client.rs` (`LinearClient::new` accepts `RateLimitState`)

- [ ] **Step 1: Thread `RateLimitState` through `LinearClient`**

```rust
// crates/roki-daemon/src/linear/client.rs
use crate::linear::rate_limit::RateLimitState;

pub struct LinearClient {
    http: reqwest::Client,
    token: String,
    rate_limit: Arc<RateLimitState>,
}

impl LinearClient {
    pub fn new(token: String, rate_limit: Arc<RateLimitState>) -> Self {
        Self { http: reqwest::Client::new(), token, rate_limit }
    }

    pub async fn resolve_viewer(&self) -> Result<MeId, LinearClientError> {
        // ... existing body, but call `self.rate_limit.wait_if_backoff().await`
        // before the request and `self.rate_limit.record_429(retry_after)`
        // on a 429 status. Mirror the loop in graphql::enumerate.
    }
}
```

Update every call site of `LinearClient::new` (currently in `runtime.rs`).

- [ ] **Step 2: Build a `ready_gate` middleware for axum**

In `crates/roki-daemon/src/linear/webhook.rs`, expose a shared `Arc<AtomicBool>` named `ready_gate`. Wrap the axum router with a layer that returns `503 Service Unavailable` when the flag is `false`.

```rust
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;

pub async fn ready_gate_middleware(
    state: ReadyGate,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !state.is_open() {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(axum::body::Body::empty())
            .unwrap();
    }
    next.run(request).await
}

#[derive(Clone)]
pub struct ReadyGate {
    flag: Arc<AtomicBool>,
}

impl ReadyGate {
    pub fn new() -> Self {
        Self { flag: Arc::new(AtomicBool::new(false)) }
    }
    pub fn open(&self) {
        self.flag.store(true, Ordering::Release);
    }
    pub fn is_open(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}
```

Mount the middleware on the axum `Router` next to the existing webhook handler.

- [ ] **Step 3: Boot order in `runtime::run_inner`**

```rust
pub(crate) async fn run_inner(config_path: &Path, mode: DispatchMode) -> Result<(), SkeletonError> {
    let cfg = Arc::new(RokiConfig::load(config_path)?);
    let workflow = Arc::new(WorkflowConfig::load(&cfg.paths.workflow)?);

    let rate_limit = Arc::new(RateLimitState::new());

    let me = if workflow.admission.assignee == "me" {
        let client = LinearClient::new(cfg.linear.token.clone(), rate_limit.clone());
        Some(client.resolve_viewer().await?)
    } else { None };

    let cache = Arc::new(DiffCache::new());
    let ready_gate = ReadyGate::new();

    // Open the daemon-scoped event writer.
    let mut daemon_writer = EventWriter::open(&cfg.paths.session_root, "_daemon")?;
    daemon_writer.emit(&Event::DaemonStarted { /* ... */ })?;

    // Bind listener with ready_gate middleware.
    let webhook_state = WebhookState::new(/* ... */);
    let app = build_axum_app(webhook_state.clone(), ready_gate.clone());
    let listener = tokio::net::TcpListener::bind(/* cfg.linear.webhook.bind */)?;
    let listener_task = tokio::spawn(serve_with_shutdown(listener, app, shutdown.clone()));

    // Spawn dispatcher (slice 5).
    let dispatcher = Arc::new(Dispatcher::new(/* ... */));
    let dispatcher_task = spawn_dispatcher_loop(dispatcher.clone(), webhook_state.clone(), shutdown.clone());

    // 1) Emit cold_start_began.
    daemon_writer.emit(&Event::ColdStartBegan {
        ts: now_rfc3339(),
        roki_toml_path: config_path.display().to_string(),
        workflow_toml_path: cfg.paths.workflow.display().to_string(),
    })?;

    // 2) Run cold start.
    let graphql = Arc::new(LinearGraphqlClient::new(cfg.linear.token.clone(), rate_limit.clone()));
    let cs = ColdStart {
        cfg: cfg.clone(),
        workflow: workflow.clone(),
        me: me.clone(),
        cache: cache.clone(),
        dispatcher: dispatcher.clone(),
        graphql,
        mode,
    };
    let report = cs.run(&mut daemon_writer).await;

    // 3) Emit cold_start_completed.
    daemon_writer.emit(&Event::ColdStartCompleted {
        ts: now_rfc3339(),
        enumerated: report.enumerated,
        admitted: report.admitted,
        cycles_spawned: report.cycles_spawned,
        orphans_deleted: report.orphans_deleted,
        enum_partial: report.enum_partial,
        partial_reason: report.partial_reason.clone(),
        partial_error_text: report.partial_error_text.clone(),
    })?;

    // 4) Emit daemon_ready and open the gate.
    daemon_writer.emit(&Event::DaemonReady { /* ... */ })?;
    ready_gate.open();

    // 5) Block on shutdown (existing slice-5 path).
    shutdown.wait().await;
    /* drain logic from slice 5, unchanged */
    Ok(())
}
```

- [ ] **Step 4: Optional test seam — fake graphql injection**

Add a build flag–gated constructor `ColdStart::new_with_client(...)` so e2e tests can plug in a mock `LinearGraphqlClient`. Or rely entirely on the `ROKI_LINEAR_GRAPHQL_URL` env override; both paths work. Pick the env-override path for consistency with slice 1.

- [ ] **Step 5: Build + run unit tests**

```bash
cargo build -p roki-daemon
cargo test -p roki-daemon
```

Expected: clean build; every existing test still passes; the new dispatcher + cache + cold_start unit tests all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs \
        crates/roki-daemon/src/linear/webhook.rs \
        crates/roki-daemon/src/linear/client.rs
git commit -m "feat(runtime): cold-start gates daemon_ready; listener parked behind ready_gate"
```

---

## Task 10: `ref:log-events` doc edits

**Files:**
- Modify: `docs/reference/log-events.md`

- [ ] **Step 1: Add new rows**

Under `## Cold start`:

```markdown
| `cold_start_began` | Daemon process start, after config validation | `roki.toml` path, `WORKFLOW.toml` path |
| `cold_start_completed` | Cold-start enumeration + reconciliation finished | `enumerated`, `admitted`, `cycles_spawned`, `orphans_deleted`, `enum_partial`, optional `partial_reason ∈ {linear_unreachable, graphql_error}` and `partial_error_text` when `enum_partial: true` |
| `orphan_reconcile_skipped` | Orphan reconcile skipped because cold-start enumeration was partial | `reason: cold_start_partial` (warn severity) |
```

(Replace the existing two `cold_start_began` / `cold_start_completed` rows with the expanded versions.)

Under `## Linear admission`, add:

```markdown
| `status_filter_dropped` | At cold start when any rule/cleanup entry omits `when.status` and the status union narrowing is dropped | `entry`, `reason: any-state-rule` (info severity) |
```

Update the existing `webhook_skipped` row to mention the optional source field:

```markdown
| `webhook_skipped` | Admission failed or no diff. Field `source ∈ {webhook, cold_start}` (defaults to `webhook` when omitted) distinguishes the trigger | `reason ∈ signature_invalid / assignee_mismatch / repo_unresolvable / no_diff`, optional `source` |
```

- [ ] **Step 2: Update `daemon_ready` description**

Under `## Daemon lifecycle`, the existing row already reads "All subsystems up + cold start complete". Confirm — this matches the new emission point. No change needed unless the row currently carries a slice-5 interim qualifier.

- [ ] **Step 3: Run validate**

```bash
kusara validate
```

Expected: `OK (22 docs)` (or whatever count the project shows).

- [ ] **Step 4: Commit**

```bash
git add docs/reference/log-events.md
git commit -m "docs(ref:log-events): cold-start, orphan, status-filter rows + source field"
```

---

## Task 11: E2E — cold start with two assigned tickets

**Files:**
- Create: `crates/roki-daemon/tests/e2e/cold_start_two_tickets_smoke.rs`
- Create: `crates/roki-daemon/tests/e2e/support/cold_start.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Write the e2e support helpers**

```rust
// crates/roki-daemon/tests/e2e/support/cold_start.rs
//! Shared helpers for slice-6 cold-start e2e tests.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::fs;

/// Tail `_daemon.events.jsonl` until a line with `event = name` appears.
pub async fn await_daemon_event(session_root: &Path, name: &str, timeout: Duration) -> Value {
    let path = session_root.join("_daemon.events.jsonl");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            let content = fs::read_to_string(&path).await.unwrap_or_default();
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if v.get("event").and_then(|e| e.as_str()) == Some(name) {
                        return v;
                    }
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("event {} not seen within {:?}", name, timeout);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub async fn await_daemon_ready(session_root: &Path) -> Value {
    await_daemon_event(session_root, "daemon_ready", Duration::from_secs(10)).await
}

pub async fn await_cold_start_completed(session_root: &Path) -> Value {
    await_daemon_event(session_root, "cold_start_completed", Duration::from_secs(10)).await
}

pub fn issue_node(id: &str, identifier: &str, state: &str, assignee: &str) -> Value {
    serde_json::json!({
        "id": id,
        "identifier": identifier,
        "title": format!("{} title", identifier),
        "description": null,
        "state": { "name": state },
        "labels": { "nodes": [] },
        "assignee": { "id": assignee },
        "updatedAt": "2026-05-09T00:00:00Z"
    })
}
```

- [ ] **Step 2: Write the e2e test**

```rust
// crates/roki-daemon/tests/e2e/cold_start_two_tickets_smoke.rs

mod support; // includes ../support/cold_start.rs via the existing slice 1-5 mod tree

use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::cold_start::{
    await_cold_start_completed, await_daemon_ready, issue_node,
};

#[tokio::test(flavor = "multi_thread")]
async fn cold_start_dispatches_two_tickets_with_cold_start_trigger() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [
                        issue_node("a1", "TEAM-1", "Todo", "u1"),
                        issue_node("a2", "TEAM-2", "Todo", "u1"),
                    ]
                }
            }
        })))
        .mount(&server)
        .await;

    // Boot daemon with viewer = "u1", workflow that matches Todo, fake claude that finishes immediately.
    let fixture = SliceSixFixture::new()
        .with_graphql(&server.uri())
        .with_workflow_admission_me(/* matches u1 in viewer-resolve mock */)
        .with_rule_when_status("Todo")
        .start()
        .await;

    let report = await_cold_start_completed(&fixture.session_root).await;
    assert_eq!(report["enumerated"], 2);
    assert_eq!(report["admitted"], 2);
    assert_eq!(report["cycles_spawned"], 2);
    assert_eq!(report["enum_partial"], false);

    let _ready = await_daemon_ready(&fixture.session_root).await;

    // Both per-ticket events.jsonl carry cycle.trigger = cold_start.
    fixture.assert_cycle_trigger("a1", "cold_start").await;
    fixture.assert_cycle_trigger("a2", "cold_start").await;

    fixture.shutdown().await;
}
```

(`SliceSixFixture` is the new helper above. It composes slice-1's webhook fixture, slice-3's wiremock viewer-resolve fixture, and the new graphql fixture. Add it to `tests/e2e/support/cold_start.rs` mirroring the existing slice-1 to slice-5 fixtures' style.)

- [ ] **Step 3: Wire `Cargo.toml`**

```toml
[[test]]
name = "cold_start_two_tickets_smoke"
path = "tests/e2e/cold_start_two_tickets_smoke.rs"
```

(Add the same row for every e2e file in Tasks 11-19 in one Cargo.toml edit; that's faster than editing once per task.)

- [ ] **Step 4: Run the test**

```bash
cargo test -p roki-daemon --test cold_start_two_tickets_smoke
```

Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/tests/e2e/cold_start_two_tickets_smoke.rs \
        crates/roki-daemon/tests/e2e/support/cold_start.rs \
        crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): cold-start dispatches two tickets with cold_start trigger"
```

---

## Task 12: E2E — cold-start orphan reconcile

**Files:**
- Create: `crates/roki-daemon/tests/e2e/cold_start_orphan_reconcile_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
mod support;

use std::time::Duration;
use serde_json::json;
use tokio::fs;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::cold_start::{await_cold_start_completed, issue_node};

#[tokio::test(flavor = "multi_thread")]
async fn cold_start_deletes_orphan_session_tempdirs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [ issue_node("new-1", "TEAM-9", "Todo", "u1") ]
                }
            }
        })))
        .mount(&server).await;

    let fixture = SliceSixFixture::new()
        .with_graphql(&server.uri())
        .with_workflow_admission_me()
        .with_rule_when_status("Todo")
        .pre_create_session_dir("old-orphan-1")
        .pre_create_session_dir("old-orphan-2")
        .start()
        .await;

    let report = await_cold_start_completed(&fixture.session_root).await;
    assert_eq!(report["orphans_deleted"], 2);

    assert!(!fixture.session_root.join("old-orphan-1").exists());
    assert!(!fixture.session_root.join("old-orphan-2").exists());
    assert!(fixture.session_root.join("new-1").exists());

    fixture.shutdown().await;
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p roki-daemon --test cold_start_orphan_reconcile_smoke
git add crates/roki-daemon/tests/e2e/cold_start_orphan_reconcile_smoke.rs
git commit -m "test(e2e): cold-start orphan session-tempdir reconcile"
```

---

## Task 13: E2E — cold start partial enum skips orphan reconcile

**Files:**
- Create: `crates/roki-daemon/tests/e2e/cold_start_partial_enum_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
mod support;

use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::cold_start::{await_cold_start_completed, await_daemon_event, issue_node};

#[tokio::test(flavor = "multi_thread")]
async fn cold_start_partial_enum_skips_orphan_reconcile() {
    let server = MockServer::start().await;

    // Page 1 succeeds.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                    "nodes": [ issue_node("a1", "TEAM-1", "Todo", "u1") ]
                }
            }
        })))
        .up_to_n_times(1)
        .mount(&server).await;

    // Page 2 always 500s — exhausts the retry budget.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server).await;

    let fixture = SliceSixFixture::new()
        .with_graphql(&server.uri())
        .with_workflow_admission_me()
        .with_rule_when_status("Todo")
        .with_page_size(1)
        .pre_create_session_dir("old-orphan-1")
        .start()
        .await;

    let report = await_cold_start_completed(&fixture.session_root).await;
    assert_eq!(report["enum_partial"], true);
    assert!(report["partial_reason"].as_str().unwrap().contains("linear_unreachable"));
    assert_eq!(report["orphans_deleted"], 0);

    let _ = await_daemon_event(&fixture.session_root, "orphan_reconcile_skipped", std::time::Duration::from_secs(5)).await;

    assert!(fixture.session_root.join("old-orphan-1").exists(), "orphan preserved on partial");

    fixture.shutdown().await;
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p roki-daemon --test cold_start_partial_enum_smoke
git add crates/roki-daemon/tests/e2e/cold_start_partial_enum_smoke.rs
git commit -m "test(e2e): partial enum skips orphan reconcile and preserves disk state"
```

---

## Task 14: E2E — 429 backoff during cold start

**Files:**
- Create: `crates/roki-daemon/tests/e2e/cold_start_backoff_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
mod support;

use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::cold_start::{await_cold_start_completed, await_daemon_event, issue_node};

#[tokio::test(flavor = "multi_thread")]
async fn cold_start_handles_429_with_retry_after() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1").set_body_string(""))
        .up_to_n_times(1)
        .mount(&server).await;

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [ issue_node("a1", "TEAM-1", "Todo", "u1") ]
                }
            }
        })))
        .mount(&server).await;

    let fixture = SliceSixFixture::new()
        .with_graphql(&server.uri())
        .with_workflow_admission_me()
        .with_rule_when_status("Todo")
        .start()
        .await;

    let _ = await_daemon_event(&fixture.session_root, "linear_backoff_applied", std::time::Duration::from_secs(5)).await;
    let report = await_cold_start_completed(&fixture.session_root).await;
    assert_eq!(report["enumerated"], 1);
    assert_eq!(report["enum_partial"], false);

    fixture.shutdown().await;
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p roki-daemon --test cold_start_backoff_smoke
git add crates/roki-daemon/tests/e2e/cold_start_backoff_smoke.rs
git commit -m "test(e2e): cold-start retries after 429 with Retry-After"
```

---

## Task 15: E2E — webhook eviction with in-flight cycle (worktree retained)

**Files:**
- Create: `crates/roki-daemon/tests/e2e/eviction_in_flight_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
mod support;

use std::time::Duration;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn admission_revoke_during_cycle_evicts_cache_only_worktree_retained() {
    let fixture = SliceSixFixture::new()
        .with_workflow_admission_me()
        .with_rule_when_status("Todo")
        .with_long_running_run_phase(Duration::from_secs(2))  // sleep so revoke arrives mid-cycle
        .start()
        .await;

    // Webhook A: ticket-1 admitted, cycle starts.
    fixture.send_webhook_admit("ticket-1", "u1").await;
    fixture.assert_event_per_ticket("ticket-1", "cycle_started").await;

    // Webhook B: same ticket, assignee changed off-operator → admission fails.
    fixture.send_webhook_revoke("ticket-1", "stranger").await;
    let _ = fixture.assert_event_per_ticket("ticket-1", "webhook_skipped").await;

    // Cycle finishes.
    fixture.assert_event_per_ticket("ticket-1", "cycle_completed").await;

    // Cache evicted.
    assert!(fixture.cache_snapshot("ticket-1").await.is_none());

    // Worktree + session_tempdir still on disk.
    assert!(fixture.session_root.join("ticket-1").exists());
    assert!(fixture.worktree_root.join("ticket-1").exists());

    // No worktree_deleted / session_tempdir_deleted event was emitted.
    assert!(
        !fixture
            .has_event_per_ticket("ticket-1", "worktree_deleted")
            .await
    );
    assert!(
        !fixture
            .has_event_per_ticket("ticket-1", "session_tempdir_deleted")
            .await
    );

    fixture.shutdown().await;
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p roki-daemon --test eviction_in_flight_smoke
git add crates/roki-daemon/tests/e2e/eviction_in_flight_smoke.rs
git commit -m "test(e2e): admission revoke evicts cache only; worktree retained"
```

---

## Task 16: E2E — webhook eviction without in-flight cycle

**Files:**
- Create: `crates/roki-daemon/tests/e2e/eviction_no_cycle_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
mod support;

#[tokio::test(flavor = "multi_thread")]
async fn admission_revoke_after_cycle_evicts_cache_immediately() {
    let fixture = SliceSixFixture::new()
        .with_workflow_admission_me()
        .with_rule_when_status("Todo")
        .start()
        .await;

    fixture.send_webhook_admit("ticket-1", "u1").await;
    fixture.assert_event_per_ticket("ticket-1", "cycle_completed").await;
    assert!(fixture.cache_snapshot("ticket-1").await.is_some());

    fixture.send_webhook_revoke("ticket-1", "stranger").await;

    // Wait briefly then assert eviction.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(fixture.cache_snapshot("ticket-1").await.is_none());

    // Disk paths intact.
    assert!(fixture.session_root.join("ticket-1").exists());
    assert!(fixture.worktree_root.join("ticket-1").exists());

    fixture.shutdown().await;
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p roki-daemon --test eviction_no_cycle_smoke
git add crates/roki-daemon/tests/e2e/eviction_no_cycle_smoke.rs
git commit -m "test(e2e): post-cycle admission revoke evicts cache immediately"
```

---

## Task 17: E2E — re-admission cancels pending eviction

**Files:**
- Create: `crates/roki-daemon/tests/e2e/eviction_readmit_cancels_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
mod support;

use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn re_admission_cancels_pending_eviction() {
    let fixture = SliceSixFixture::new()
        .with_workflow_admission_me()
        .with_rule_when_status("Todo")
        .with_long_running_run_phase(Duration::from_secs(2))
        .start()
        .await;

    fixture.send_webhook_admit("ticket-1", "u1").await;
    fixture.assert_event_per_ticket("ticket-1", "cycle_started").await;

    fixture.send_webhook_revoke("ticket-1", "stranger").await;
    fixture.send_webhook_admit("ticket-1", "u1").await;

    fixture.assert_event_per_ticket("ticket-1", "cycle_completed").await;

    // Cache still has entry (re-admission cleared pending_evict).
    assert!(fixture.cache_snapshot("ticket-1").await.is_some());

    fixture.shutdown().await;
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p roki-daemon --test eviction_readmit_cancels_smoke
git add crates/roki-daemon/tests/e2e/eviction_readmit_cancels_smoke.rs
git commit -m "test(e2e): re-admission cancels pending eviction"
```

---

## Task 18: E2E — re-admission after eviction reuses worktree

**Files:**
- Create: `crates/roki-daemon/tests/e2e/eviction_readmit_reuse_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
mod support;

use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn readmit_after_eviction_reuses_existing_worktree() {
    let fixture = SliceSixFixture::new()
        .with_workflow_admission_me()
        .with_rule_when_status("Todo")
        .with_long_running_run_phase(Duration::from_secs(1))
        .start()
        .await;

    // Cycle 1.
    fixture.send_webhook_admit("ticket-1", "u1").await;
    fixture.assert_event_per_ticket("ticket-1", "cycle_completed").await;
    let initial_inode = fixture.worktree_inode("ticket-1");

    // Revoke, then re-admit.
    fixture.send_webhook_revoke("ticket-1", "stranger").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    fixture.send_webhook_admit("ticket-1", "u1").await;

    fixture.assert_event_per_ticket("ticket-1", "cycle_completed").await;

    // Worktree on disk should be the same one — same inode.
    let post_inode = fixture.worktree_inode("ticket-1");
    assert_eq!(initial_inode, post_inode, "worktree was recreated rather than reused");

    fixture.shutdown().await;
}
```

(`worktree_inode` reads `metadata().ino()` on the worktree dir on Unix. Skip the inode assertion on Windows; the project is Linux/macOS only per fr:12 boundaries.)

- [ ] **Step 2: Run + commit**

```bash
cargo test -p roki-daemon --test eviction_readmit_reuse_smoke
git add crates/roki-daemon/tests/e2e/eviction_readmit_reuse_smoke.rs
git commit -m "test(e2e): readmit after eviction reuses retained worktree"
```

---

## Task 19: E2E — listener parked during cold start

**Files:**
- Create: `crates/roki-daemon/tests/e2e/cold_start_listener_parked_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
mod support;

use std::time::Duration;
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::cold_start::{await_daemon_ready, issue_node};

#[tokio::test(flavor = "multi_thread")]
async fn webhook_during_cold_start_returns_503() {
    let server = MockServer::start().await;

    // Slow GraphQL — buys ~3s to send a webhook before ready_gate opens.
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(3))
                .set_body_json(json!({
                    "data": {
                        "issues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [ issue_node("a1", "TEAM-1", "Todo", "u1") ]
                        }
                    }
                })),
        )
        .mount(&server).await;

    let fixture = SliceSixFixture::new()
        .with_graphql(&server.uri())
        .with_workflow_admission_me()
        .with_rule_when_status("Todo")
        .start()
        .await;

    // Send a webhook while cold start is still running.
    let early_status = fixture.send_raw_webhook_signed_now().await;
    assert_eq!(early_status, 503, "webhook should be parked behind ready_gate");

    let _ = await_daemon_ready(&fixture.session_root).await;

    // After daemon_ready, the same webhook is accepted (200 / 202).
    let late_status = fixture.send_raw_webhook_signed_now().await;
    assert!(late_status >= 200 && late_status < 300);

    fixture.shutdown().await;
}
```

(`send_raw_webhook_signed_now` is a helper that signs with the configured webhook secret and submits via `reqwest`. Add it to `tests/e2e/support/cold_start.rs`.)

- [ ] **Step 2: Run + commit**

```bash
cargo test -p roki-daemon --test cold_start_listener_parked_smoke
git add crates/roki-daemon/tests/e2e/cold_start_listener_parked_smoke.rs
git commit -m "test(e2e): webhook during cold-start returns 503 until daemon_ready"
```

---

## Task 20: E2E — `roki cleanup` cold-start mode dispatches only cleanup matches

**Files:**
- Create: `crates/roki-daemon/tests/e2e/cold_start_cleanup_mode_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
mod support;

use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::cold_start::{await_cold_start_completed, issue_node};

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_only_mode_dispatches_only_cleanup_matches_at_cold_start() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": [
                        issue_node("a1", "TEAM-1", "Todo", "u1"),
                        issue_node("a2", "TEAM-2", "Done", "u1"),
                    ]
                }
            }
        })))
        .mount(&server).await;

    let fixture = SliceSixFixture::new()
        .with_graphql(&server.uri())
        .with_workflow_admission_me()
        .with_rule_when_status("Todo")
        .with_cleanup_when_status("Done")
        .with_subcommand_cleanup() // launch as `roki cleanup` not `roki run`
        .start()
        .await;

    let report = await_cold_start_completed(&fixture.session_root).await;
    assert_eq!(report["enumerated"], 2);
    assert_eq!(report["admitted"], 2);
    assert_eq!(report["cycles_spawned"], 1, "only the Done ticket dispatches via [[cleanup]]");

    // a2 (Done) ran a cleanup cycle; a1 (Todo) did not.
    fixture.assert_event_per_ticket("a2", "cycle_started").await;
    assert!(!fixture.has_event_per_ticket("a1", "cycle_started").await);

    fixture.shutdown().await;
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p roki-daemon --test cold_start_cleanup_mode_smoke
git add crates/roki-daemon/tests/e2e/cold_start_cleanup_mode_smoke.rs
git commit -m "test(e2e): cold-start cleanup-only mode dispatches only [[cleanup]] matches"
```

---

## Task 21: Slice 1-5 backwards-compat sweep

**Files:**
- Modify: `crates/roki-daemon/tests/e2e/support/persistent.rs` (slice-5 helper) and any slice 1-5 fixture that watched for the wrong `daemon_ready` timing

- [ ] **Step 1: Update `await_event_then_sigterm` to wait for `daemon_ready` first**

In the slice-5 helper module, prepend a `daemon_ready` wait to the start of every fixture that sends a webhook. The simplest safe pattern: change the existing helper signature so it waits for `daemon_ready` automatically before returning the test scaffold.

```rust
// Pseudocode — adapt to the slice-5 helper's actual shape.
pub async fn boot_daemon_and_await_ready(ctx: &TestContext) -> DaemonHandle {
    let handle = boot_daemon(ctx).await;
    await_daemon_event(&ctx.session_root, "daemon_ready", Duration::from_secs(15)).await;
    handle
}
```

Update every slice 1-5 fixture that previously assumed the listener was accepting traffic immediately at boot.

- [ ] **Step 2: Configure slice 1-5 fixtures with an empty cold-start GraphQL responder**

The existing slice 1-5 fixtures have a wiremock that handles only the webhook path. Slice 6 daemon now also calls GraphQL at boot — without a responder it will hit `linear.app` (release endpoint, 401 because the test token is fake) and return `enum_partial: true`.

Add a default empty responder to the slice-5 boot harness:

```rust
Mock::given(wiremock::matchers::method("POST"))
    .and(wiremock::matchers::path("/"))  // graphql path on the same mock server, OR a separate mock server bound via ROKI_LINEAR_GRAPHQL_URL
    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "data": { "issues": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] } }
    })))
    .mount(&graphql_server)
    .await;
std::env::set_var("ROKI_LINEAR_GRAPHQL_URL", graphql_server.uri());
```

- [ ] **Step 3: Re-run the entire suite**

```bash
cargo test -p roki-daemon
```

Expected: every slice 1-5 e2e + every slice 6 e2e passes.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/tests/e2e/support/persistent.rs \
        crates/roki-daemon/tests/e2e/  # any per-fixture tweaks
git commit -m "test(e2e): slice 1-5 fixtures wait for daemon_ready and stub graphql empty"
```

---

## Task 22: Cross-task self-review

- [ ] **Step 1: Verify spec coverage**

Walk through `docs/superpowers/specs/2026-05-09-slice6-cold-start-design.md` §8 Implementation Order items 1-20 and confirm each maps to a task in this plan:

| Spec item | Plan task |
|---|---|
| 1. CycleTrigger enum + PhaseContext widening | Task 1 |
| 2. RateLimitState | Task 2 |
| 3. LinearGraphqlClient::enumerate | Task 3 |
| 4. daemon::orphan::reconcile | Task 4 |
| 5. daemon::cold_start::ColdStart | Task 5 |
| 6. Dispatcher::admit_for_cold_start | Task 6 |
| 7. Admission-filter eviction | Task 7 |
| 8. runtime::run rewire (ready_gate) | Task 9 |
| 9. Daemon-scoped events wiring | Task 8 + Task 9 |
| 10. ref:log-events doc edits | Task 10 |
| 11. ref:cli doc audit | (no flag changes — verify in Task 22 itself) |
| 12-19. E2E tests | Tasks 11-20 |
| 20. Slice 1-5 backwards compat sweep | Task 21 |

- [ ] **Step 2: Run the full suite once more**

```bash
cargo test -p roki-daemon
cargo clippy -p roki-daemon --all-targets -- -D warnings
cargo fmt --check
```

Expected: clean.

- [ ] **Step 3: Run kusara validate**

```bash
kusara validate
```

Expected: `OK`.

- [ ] **Step 4: Confirm no `cycle.trigger = "polling"` regression**

```bash
grep -rn "cycle\.trigger\|trigger.*polling\|trigger.*refresh" crates/roki-daemon/src/
```

Expected: `cycle.trigger` only ever set to `"runtime"` or `"cold_start"`. No `"polling"` or `"refresh"` strings.

- [ ] **Step 5: Confirm worktree-retention is honored**

```bash
grep -rn "post_cycle_delete\|wt remove\|remove_dir_all" crates/roki-daemon/src/
```

Each hit must be either:
- Inside `engine::cleanup` (slice-3/4 cleanup-cycle path)
- Inside `daemon::orphan::reconcile` (cold-start orphan path)
- A test helper / `tempfile` cleanup

No call site inside `Dispatcher` or `daemon::ticket_task` admission-eviction path may invoke any of these.

- [ ] **Step 6: Push branch**

```bash
git push -u origin slice6-cold-start-spec
```

(Skip if the project's flow runs the push from a separate command.)

---

## Notes for the implementing engineer

- **Linear filter shape verification**: the GraphQL filter shape (`assignee.id.eq`, `state.name.in`) follows Linear's published `IssueFilter` type. The slice-3 `linear-spec-check.md` rule applies — if a wiremock test passes locally but production gives `Field "in" not found in type "StringFilter"`, that means Linear's filter syntax has drifted. Re-verify via context7 (`mcp__context7__query-docs` on `/websites/linear_app_developers`) and update `build_query_body` accordingly.
- **`workflow_path` cache field**: the slice-5 `CacheEntry::workflow_path` is unused by slice 5 (it's a placeholder for the per-repo TOML override). Slice 6 leaves it `None` for cold-start synthesized entries. Per-repo workflow path resolution at cold start is identical to webhook admission; the existing `admission::accept` already returns the resolved path (or `None` for top-level fallback). If `AdmittedTicket` carries it, copy through — otherwise leave the field for a follow-up that wires it cleanly.
- **`config/workflow.rs` accessors for `compute_status_union`**: slice 5 didn't need to iterate every rule + cleanup entry's `when.status`. If the existing `WorkflowConfig` exposes them only via a method that takes a single ticket and returns the matching entry, add a sibling `iter_all_entries(&self) -> impl Iterator<Item = &EntryShape>` that returns all of them. Use the same ordering `engine::dispatch::evaluate` walks (`[[cleanup]]` first, then `[[rule]]`).
- **Webhook-side `webhook_skipped` field shape**: existing slice 1-5 emissions read `webhook_skipped { ts, ticket_id, reason }`. Adding `source: Option<...>` with `skip_serializing_if = "Option::is_none"` keeps existing JSON shape byte-for-byte identical when source is None.
- **No new crates**: re-read this if a temptation arises.
