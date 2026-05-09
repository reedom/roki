# Slice 6 — Cold Start and Admission Eviction Design

Date: 2026-05-09
Scope: Boot-time Linear enumeration that rebuilds the diff cache from scratch, dispatches matching cycles with `trigger = cold_start`, and reconciles orphan session tempdirs. Adds webhook-driven admission-filter eviction for previously-cached tickets that now fail admission. Moves `daemon_ready` emission to after `cold_start_completed`. Lands the paginated GraphQL primitive that a later slice will reuse for polling and refresh nudge.

## 1. Position in the Roadmap

Slice 6 closes:

- `roki-daemon-cold-start-enumeration` — `fr:07 §Cold start` steps 1-4. Linear paginated enumeration with assignee + status-union filter; cache populate; per-ticket `[[cleanup]]` then `[[rule]]` first-match dispatch with `cycle.trigger = cold_start`.
- `roki-daemon-orphan-reconcile` — `fr:07 §Cold start` step 5 + `fr:05 §Cleanup` item 3. Session-tempdir orphan delete; worktree orphans delegated to `worktrunk` (out of scope per `fr:07 step 5`).
- `roki-daemon-admission-eviction` — `fr:03 §Reassignment` + `fr:05 §Cleanup` item 2 + `fr:01 §Cycle dispatch`. Webhook-driven eviction: previously cached ticket → assignee or repo no longer admits → cache evicted (after any in-flight cycle finishes naturally). Worktree + session_tempdir are **retained** for re-admission reuse; reclamation is via cleanup-cycle on re-admission or cold-start orphan reconcile when the ticket is no longer enumerable. This slice updates fr:01 / fr:03 / fr:05 wording to remove the prior "delete on admission revoke" contract.
- `roki-daemon-cycle-trigger-plumbing` — propagates `cycle.trigger ∈ {runtime, cold_start}` through `PhaseContext` and `ROKI_CYCLE_TRIGGER` env var.

Slices 1-5 provide: cycle engine, `[[on_failure]]` routing, `[[cleanup]]` cycle, worktree lifecycle, FsPoison routing, structured event writer (per-ticket and `_daemon` scoped), admission filter, webhook receiver, persistent dispatcher with diff cache, ticket-task registry, SIGINT/SIGTERM drain.

Out of scope, deferred to later slices:

- **Polling background loop** (`fr:03 §Polling fallback`). The paginated GraphQL primitive lands here so cold start can use it; the periodic loop, the outage detector, and the `polling_started` / `polling_completed` events stay out. The cadence cap and 429 backoff state are implemented and exercised by cold start; reused by polling unchanged when polling lands.
- **Refresh nudge** (`fr:03 §Refresh nudge`, `fr:10 §POST /api/refresh`). Depends on HTTP API.
- **Escalation queue** (`fr:06 §Escalation queue`). Slice 7. Cold-start-time fs errors during orphan reconcile do not enter an escalation queue in this slice; they emit a warn-severity structured log per the orphan-error decision (§5.4).
- **Hot reload** (`fr:02`). Restart-required.
- **HTTP API** (`fr:10`), **TUI** (`fr:11`), **`roki repo` / `roki log` / `roki events` CLIs** (`fr:09`). Later slices.

## 2. Architecture

### 2.1 Module layout

```
crates/roki-daemon/src/
├── runtime.rs                         // wire cold_start before listener-ready
├── linear/
│   ├── client.rs                      // existing viewer query
│   ├── graphql.rs                     // NEW: paginated issues query primitive
│   ├── rate_limit.rs                  // NEW: shared 429 backoff state
│   └── ...
├── daemon/
│   ├── cold_start.rs                  // NEW: enumerator + dispatcher + reconciler
│   ├── orphan.rs                      // NEW: session-tempdir reconcile
│   ├── dispatcher.rs                  // extend: eviction path on admission failure
│   ├── ticket_task.rs                 // accept cold_start trigger; eviction msg
│   └── ...
├── engine/
│   ├── context.rs                     // accept CycleTrigger param
│   └── ...
└── ...
```

`runtime::run` boots: load config → load workflow → resolve `me` → bind listener (not-yet-accepting) → run cold start → emit `daemon_ready` → start dispatcher accepting traffic. Cold start is awaited in full (enumeration + cache populate + cycle launch + reconcile) before `daemon_ready`. Cycles spawned by cold start run async on the existing per-ticket task model — cold start does not block on cycle completion.

### 2.2 Types

```rust
// engine::context  (extension)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleTrigger {
    Runtime,
    ColdStart,
}

impl CycleTrigger {
    pub fn as_str(self) -> &'static str { /* "runtime" | "cold_start" */ }
}

// PhaseContext::new takes `trigger: CycleTrigger`.
// CycleView.trigger becomes `&'static str` derived from the enum.
// ROKI_CYCLE_TRIGGER env injection unchanged in shape.
```

```rust
// linear::graphql

pub struct LinearGraphqlClient {
    http: reqwest::Client,
    token: String,
    rate_limit: Arc<RateLimitState>,
}

pub struct EnumerateRequest<'a> {
    pub assignee_id: &'a str,
    pub status_filter: StatusFilter<'a>,  // None | Union(&[&str])
    pub page_size: u32,                   // bounded; see §4.1
}

pub struct EnumeratedTicket {
    pub id: String,
    pub identifier: String,           // "TEAM-123"
    pub title: String,
    pub description: Option<String>,
    pub state_name: String,
    pub label_names: BTreeSet<String>,
    pub assignee_id: Option<String>,
    pub updated_at: OffsetDateTime,
}

impl LinearGraphqlClient {
    pub async fn enumerate(
        &self,
        req: &EnumerateRequest<'_>,
    ) -> Result<Vec<EnumeratedTicket>, LinearEnumerateError>;
}
```

```rust
// linear::rate_limit

pub struct RateLimitState {
    backoff_until: AtomicU64,   // unix ms; 0 = clear
}

impl RateLimitState {
    pub fn record_429(&self, retry_after: Duration);  // exponential within bounds
    pub async fn wait_if_backoff(&self);
    pub fn is_in_backoff(&self) -> bool;
    pub fn clear(&self);
}
```

```rust
// daemon::cold_start

pub struct ColdStart {
    cfg: Arc<RokiConfig>,
    workflow: Arc<WorkflowConfig>,
    me: Option<MeId>,
    cache: Arc<DiffCache>,
    dispatcher: Arc<Dispatcher>,
    graphql: Arc<LinearGraphqlClient>,
    daemon_writer: Arc<EventWriter>,
    mode: DispatchMode,
}

pub struct ColdStartReport {
    pub enumerated: usize,
    pub admitted: usize,
    pub cycles_spawned: usize,
    pub orphans_deleted: usize,
    pub enum_partial: bool,   // true when enumeration aborted mid-pagination
}

impl ColdStart {
    pub async fn run(&self) -> ColdStartReport;
}
```

```rust
// daemon::orphan

pub struct OrphanScan<'a> {
    pub session_root: &'a Path,
    pub keep_ids: &'a HashSet<String>,
}

pub struct OrphanReport {
    pub deleted: Vec<String>,   // ticket ids
    pub fs_errors: Vec<(String, std::io::Error)>,
}

pub async fn reconcile(scan: OrphanScan<'_>, writer: &EventWriter) -> OrphanReport;
```

`AdmittedTicket`, `WorkflowConfig`, `RokiConfig`, `MeId`, `DispatchMode`, `Dispatcher`, `DiffCache`, `engine::dispatch::evaluate*`, `engine::cycle::run_cycle`, `engine::cleanup::*` are unchanged from slices 1-5.

### 2.3 Concurrency model

| Phase | Task | What it does |
|---|---|---|
| Boot | runtime | Bind listener with `with_graceful_shutdown` configured but axum router parked behind a `ready_gate` until cold start completes. Holds incoming sockets in OS accept queue. |
| Boot | cold_start | Single task. Paginates GraphQL serially. Inserts into cache. Dispatches per ticket through `Dispatcher::on_cold_start_admit` which spawns the same per-ticket actor used at runtime. |
| Boot | cold_start | After enumeration, calls `orphan::reconcile`. |
| Boot | runtime | Emits `cold_start_completed` then `daemon_ready` then opens `ready_gate`. Listener begins serving webhook POSTs. |
| Runtime | unchanged from slice 5 | |

The cold-start cycles spawned in step 2 run concurrently across distinct tickets — they use the existing ticket-task registry and inbox. `daemon_ready` does not wait for those cycles; it waits only for enumeration and reconcile.

### 2.4 Listener-park strategy

axum's `with_graceful_shutdown` is used to bind early but the router is wrapped in a `tower::Layer` that returns `503 Service Unavailable` while `ready_gate` is closed. This keeps the bind-failure detection at boot time (fr:12 contract) without delivering webhooks before cold start finishes — webhook delivery during cold start would create races between cold-start cycle dispatch and webhook-driven dispatch on the same ticket.

Linear retries 503 per its delivery policy (1 m / 1 h / 6 h). Cold start is expected to complete in seconds-to-minutes for typical assigned-ticket counts; a long cold start that exceeds Linear's first retry window is a deployment problem, not a slice-6 design concern.

### 2.5 Test seam

| Var | Effect |
|---|---|
| `ROKI_LINEAR_GRAPHQL_URL` | Override the GraphQL endpoint (already gated to `cfg(test)` / `feature = "test-support"` in slice 1's `LinearClient`; reused by `LinearGraphqlClient`). |
| `ROKI_COLD_START_PAGE_SIZE` | Override the GraphQL `first:` argument so e2e tests can force multi-page pagination with a small fixture set. `cfg(any(test, feature = "test-support"))` gated. |
| `ROKI_DAEMON_EVENTS_DIR_OVERRIDE` | Existing slice 5 daemon-scoped event dir override. |
| `ROKI_SHUTDOWN_WINDOW_OVERRIDE_SECONDS` | Existing slice 5 drain timeout override. |

## 3. Cycle Trigger Plumbing

`PhaseContext::new` currently hardcodes `trigger: "runtime"`. Slice 6 widens the signature:

```rust
pub fn new(
    admitted: &AdmittedTicket,
    cycle_id: Uuid,
    cfg: &RokiConfig,
    cycle_kind: CycleKind,
    cycle_trigger: CycleTrigger,
) -> Self;
```

Every existing call site at runtime (dispatcher → ticket task → `engine::cycle::run_cycle`) passes `CycleTrigger::Runtime`. The cold-start path constructs the same `AdmittedTicket` (synthesized from the GraphQL response, see §4.3) and passes `CycleTrigger::ColdStart`.

`cycle.trigger` propagates through:

- `CycleView.trigger` in `PhaseContext` (rendered into Liquid templates).
- `ROKI_CYCLE_TRIGGER` env var on every phase subprocess.
- `cycle_started` / `cycle_completed` / `cycle_aborted` event rows (`cycle.trigger` field, see `ref:log-events §Common context fields`).

The unit test at `engine/context.rs:353` is updated to assert both values.

## 4. Cold Start Procedure

Per `fr:07 §Cold start` and `fr:12 §Normal startup`.

### 4.1 Linear enumeration

`linear::graphql::LinearGraphqlClient::enumerate` issues a paginated `issues(first: N, after: cursor, filter: ...)` query against `https://api.linear.app/graphql` (override-gated for tests).

**Filter shape** (concrete shape verified via Linear GraphQL docs):

- Always: `assignee.id.eq` = the resolved viewer id (or the literal admission assignee when not `me`).
- Conditional: `state.name.in` = the union of every `[[rule]]` and `[[cleanup]]` entry's `when.status` across `WORKFLOW.toml` and any per-repo TOMLs reachable via `[[admission.repos]] workflow`. Dropped if any entry omits `when.status` per `fr:07 step 2`.

If the union is dropped, an info log fires once at startup:

```
status_filter_dropped { reason = "any-state-rule", entry = "<rule-or-cleanup-name>" }
```

The exact GraphQL filter input shape is not over-specified here — it follows whatever Linear's published `IssueFilter` type accepts. The wrapper `LinearGraphqlClient::enumerate` is the one place that concretizes the shape, with an integration test against `wiremock` asserting the request body.

**Pagination**: cursor-based with `pageInfo.hasNextPage` / `pageInfo.endCursor`. Page size defaults to 50 (override via `ROKI_COLD_START_PAGE_SIZE` for tests). The enumerator walks every page and concatenates results. No client-side limit on total enumerated count.

**Status field source**: `state.name` (string). `fr:07 §Diff cache` stores `status` as the operator-facing state name; this matches `WORKFLOW.toml when.status` which is matched by name (slices 1-4 admission code).

**Label field source**: `labels.nodes[].name` collected into a `BTreeSet<String>`.

**Assignee field source**: `assignee.id` (nullable; tickets where the assignee fell off keep their last-known cached value at runtime, but cold start treats absence as "not assigned" and the admission filter rejects with `assignee_mismatch` reason).

### 4.2 Rate limit and 429

`RateLimitState` is created at runtime boot, shared with `LinearClient` (the existing `viewer` query) and `LinearGraphqlClient`. `wait_if_backoff` is awaited before every request; on `429`, `record_429` updates the backoff window and the request retries after the wait.

Backoff: doubles from 1 s up to 60 s per consecutive `429`. `Retry-After` header (when present) overrides the doubled value. Backoff clears on the first successful response.

Cold-start enumeration is governed by the same `RateLimitState`. There is no per-call timeout cap separate from backoff; a 429 storm just slows enumeration without aborting it.

`linear_backoff_applied` event fires once per `record_429` call (`ref:log-events §Linear admission`).

### 4.3 Cache populate + admission re-eval

For each `EnumeratedTicket`:

1. Build a `NormalizedTicket` (the slice-1 type that webhook intake also produces) from the GraphQL fields.
2. Run `admission::accept(&ticket, &workflow, &me)`. On reject: emit `webhook_skipped { reason = ... }` with the existing assignee_mismatch / repo_unresolvable reasons (cold start reuses the same eviction-reason vocabulary; `webhook_skipped` is a misnomer for the cold-start path but reuse keeps the consumer surface — `roki events` filtering, ref:log-events row — single).

   Decision: do NOT introduce a separate event name. Add `source: cold_start` field to the existing `webhook_skipped` row so consumers can distinguish if they want; existing consumers ignoring the field continue to work. The ref:log-events row gets a `source ∈ webhook / cold_start` annotation (see §6).

3. On accept: `cache.observe(&admitted)` per slice 5 semantics. Fresh cold start → cache is empty → every ticket returns `NewEntry`. Restart with disk residue → cache is still empty (in-memory) → every ticket returns `NewEntry`. The `NewEntry` outcome is what the dispatch path keys on.

4. After step 3 inserts the entry, immediately spawn a ticket task via `Dispatcher::admit_for_cold_start(&admitted)`. The dispatcher path is the same as the webhook one with the trigger value bound to `ColdStart`.

The cycle is dispatched per-ticket inside its own ticket task. Cross-ticket parallelism is the existing slice-5 semantics (one tokio task per ticket).

### 4.4 Cleanup-cycle eviction at cold start

If the dispatched cycle is a `[[cleanup]]` first-match (i.e. `engine::dispatch::evaluate` returns `Cycle { kind: Cleanup, .. }` or `CleanupShorthand`), normal slice-3/4/5 semantics apply: cycle terminates → `engine::cleanup::post_cycle_delete` → `cache.evict` → ticket task exits. The `keep_ids` snapshot used by orphan reconcile (§5) is taken **before** any cycle is dispatched (see §4.5), so a cleanup cycle that runs concurrently with reconcile does not race.

### 4.5 Orphan reconcile

After enumeration completes (or is aborted, see §4.6) and after cache populate, `daemon::orphan::reconcile` runs:

1. Build `keep_ids: HashSet<String>` = every ticket id that admitted (regardless of whether a cycle was dispatched). Tickets that enumerated but were rejected by admission are NOT kept — their session tempdir, if any, is an orphan.
2. Walk `<session_root>/`. Skip the reserved `_daemon/` directory. For every remaining entry:
   - If it is not a directory: skip with a debug log.
   - If its name is in `keep_ids`: skip.
   - Otherwise: `tokio::fs::remove_dir_all`. On success emit `session_tempdir_deleted { ticket.id, path, reason: "orphan" }` (`ref:log-events §Worktree / session lifecycle`). On failure emit a warn-severity event with `(ticket.id, path, error)` and continue with the next entry.

Worktrees are owned by `worktrunk`; orphan-worktree reconciliation is delegated and not invoked from this slice (`fr:07 step 5`).

### 4.6 Enumeration failure handling

Enumeration calls fail in three distinct ways:

| Failure | Source | Decision |
|---|---|---|
| Transport / 5xx after backoff retries exhausted | network | abort enumeration; carry `partial_reason = "linear_unreachable"` and `partial_error_text` into `cold_start_completed` |
| GraphQL `errors` array non-empty (auth, schema mismatch) | Linear | abort enumeration; `partial_reason = "graphql_error"` |
| `assignee.id.eq` rejected by Linear (e.g. unknown viewer) | Linear | refuse to start (treated as a config / token error, exit 1) |

On partial enumeration (i.e. `cold_start_completed { enum_partial: true }`):

1. The cache is populated only with whatever pages succeeded.
2. Cycles dispatched for those pages run normally.
3. **Orphan reconcile is skipped**: `keep_ids` would be incomplete and the reconcile would mass-delete legitimate state. Emit `orphan_reconcile_skipped { reason: "cold_start_partial" }` (warn).
4. `cold_start_completed` still fires with `enum_partial: true` so consumers can tell.
5. `daemon_ready` fires after `cold_start_completed`. The daemon proceeds to accept webhooks; future webhook arrivals re-admit and re-cache as normal.

Decision rationale: matches operator intent — never destroy disk state on a partial Linear view; the next successful cold start (next daemon restart) will reconcile.

### 4.7 Cleanup-only mode

`DispatchMode::CleanupOnly` (existing slice 3) is propagated to `engine::dispatch::evaluate` unchanged. Cold start in cleanup-only mode enumerates the same status filter, populates the cache, but `evaluate` returns `NoMatch` for any ticket whose only viable match was a `[[rule]]`. Those tickets stay cached (so a webhook update later can re-evaluate against the cleanup list when the operator transitions the ticket) but no cycle is dispatched at cold start.

## 5. Admission-Filter Eviction (Webhook-Driven)

Per `fr:03 §Reassignment`, `fr:05 §Cleanup` item 2, and `fr:01 §Cycle dispatch` mid-cycle exception.

Slice 5's `Dispatcher::on_webhook` runs `admission::accept`. On reject, it emits `webhook_skipped` and returns. Slice 5 does NOT check whether the rejected ticket is currently cached.

**Eviction semantics (worktree-preserving)**: admission-failure for a cached ticket evicts the cache entry only. The worktree and session tempdir are retained for re-admission reuse and reclaimed by either a future `[[cleanup]]` cycle on re-admission or by cold-start orphan reconcile when the ticket is no longer enumerable. This matches the FR contract updated alongside this slice (fr:01 §Cycle dispatch, fr:03 §Reassignment, fr:05 §Cleanup item 2).

### 5.1 Cache field addition

`CacheEntry` gains one boolean:

```rust
pub struct CacheEntry {
    /* ...slice 5 fields... */
    pub pending_evict: bool,
}
```

`pending_evict` is owned by the dispatcher (set) and the ticket task (consume + clear). Independent atomic transitions on a `bool`; no lock held across `await`.

### 5.2 Dispatcher path on admission-failed-while-cached

After a webhook fails admission, before returning:

```rust
if cache.snapshot(ticket_id).await.is_some() {
    cache.set_pending_evict(ticket_id).await;
    if !registry_has_ticket_task(ticket_id) {
        cache.evict(ticket_id).await;   // no in-flight cycle; reclaim now
    }
    // Inbox not signaled. Ticket task consumes the flag at end-of-cycle.
}
```

The `webhook_skipped { reason: assignee_mismatch | repo_unresolvable }` event still fires. No worktree / session_tempdir delete. No `worktree_deleted` / `session_tempdir_deleted` event for the eviction path.

### 5.3 Re-admission cancels pending eviction

If a webhook for a cached ticket with `pending_evict = true` later passes admission, the dispatcher clears the flag before continuing the normal `cache.observe` flow. Re-admission supersedes pending eviction without any special-case spawn or wait. The ticket task — if still alive — sees `pending_evict = false` after its in-flight cycle ends and proceeds normally (or processes `pending_recheck` if set).

### 5.4 Ticket task post-cycle handling

After every cycle (rule, cleanup, or failure-handler) returns:

```rust
if cache.take_pending_evict(ticket_id).await {
    cache.evict(ticket_id).await;
    break;   // exit task; worktree + session_tempdir untouched
}
// existing slice-5 cleanup-cycle path unchanged
// existing slice-5 pending_recheck loop-back unchanged
```

`pending_evict` precedence rules:

- **Over cleanup-cycle eviction**: not applicable — cleanup-cycle completion already deletes worktree + session and evicts cache (slice 5 path); the `pending_evict` check below the cleanup path is dead code in that branch.
- **Over `pending_recheck`**: if both are set, `pending_evict` wins (the entry is going away; recheck would re-spawn).
- **Failure-handler completion**: the handler runs to natural end; `pending_evict` is checked after the failure cycle resolves. Behavior matches non-failure cycles.

### 5.5 Re-admission after eviction

`fr:03 §Re-admission`: an evicted ticket that later passes admission re-admits as a fresh entry. Slice 6 wires this through the existing dispatcher path: `cache.observe` returns `NewEntry`, dispatcher spawns a fresh ticket task. The retained worktree is reused via slice-4's idempotent `wt switch-create` (existing worktree detected via `wt list`); the retained session tempdir is appended to. Cycle id is fresh; per-iter directories live alongside any pre-eviction iter directories under the same `<session_root>/<ticket-id>/`.

### 5.6 Worktree/session reclaim paths

After this slice, only three paths delete worktree + session tempdir:

1. Cleanup-cycle completion (slice 4 / slice 5 path, unchanged).
2. Cold-start orphan reconcile (slice 6 §4.5; session tempdir only — worktree owned by `worktrunk`).
3. (Future slice) Operator-explicit delete via CLI / TUI.

Mid-runtime admission-revoke is no longer a delete trigger.

## 6. Events

No new event names. Slice 6 wires emission for the cold-start-specific names that `ref:log-events` already lists, and adds two field annotations.

| Event | Slice 6 wiring |
|---|---|
| `cold_start_began` | Fires from `runtime::run` immediately before invoking `ColdStart::run`. Carries `roki.toml` path, `WORKFLOW.toml` path. Daemon-scoped event log. |
| `cold_start_completed` | Fires at the end of `ColdStart::run`. Carries `enumerated`, `admitted`, `cycles_spawned`, `orphans_deleted`, `enum_partial`, and (when `enum_partial: true`) `partial_reason ∈ {linear_unreachable, graphql_error}` + `partial_error_text`. Daemon-scoped event log. |
| `daemon_ready` | Slice 5 fires this immediately after dispatcher boot; slice 6 moves the call site to AFTER `cold_start_completed`. The `ref:log-events §Daemon lifecycle` description ("All subsystems up + cold start complete") is now the actual emission point. The slice-5 §6 NOTE about description drift is resolved. |
| `worktree_deleted` | No new reason emitted in slice 6. The `cleanup` reason from slice 4 is unchanged. Admission-revoke no longer deletes worktrees (§5). The `orphan` reason fires only when worktree-side reconcile lands (delegated to `worktrunk`). `ref:log-events` is updated alongside this slice to drop `eviction` from the reason enum. |
| `session_tempdir_deleted` | Slice 6 adds the `reason: "orphan"` case (cold-start reconcile). The slice-4 `cleanup` reason is unchanged. No `eviction` reason — admission-revoke does not delete. |
| `webhook_skipped` | Slice 6 adds an optional `source: "cold_start"` field for cold-start-side rejections. Webhook-side emissions omit the field (treated as `source: "webhook"` by default). `ref:log-events §Linear admission` row updated. |
| `linear_backoff_applied` | Slice 6 wires emission via the shared `RateLimitState`. Slice 5 had the row in `ref:log-events` but no emitter (no Linear API call besides one-shot viewer resolve). |
| `orphan_reconcile_skipped` | NEW. Warn severity. Carries `reason: "cold_start_partial"`. Daemon-scoped event log. Add row to `ref:log-events §Cold start`. |
| `status_filter_dropped` | NEW. Info severity. Carries `entry: "<rule-or-cleanup-name>"`. Daemon-scoped event log. Add row to `ref:log-events §Linear admission`. |

`webhook_received` is not emitted for cold-start enumeration — the source is GraphQL, not a webhook delivery.

## 7. Config Additions

None for slice 6. `[linear].polling.cadence_seconds` already exists in `ref:config` and `RokiConfig` parsing (slice 1). Slice 6 reuses the value indirectly only as a future hook for the polling loop — no key consults it during cold start.

`WORKFLOW.toml` schema unchanged.

## 8. Implementation Order

Tasks listed so each compiles and the test suite is green before the next starts.

1. **`CycleTrigger` enum + `PhaseContext` widening.** Add the enum, thread it through `PhaseContext::new`, update every call site to pass `CycleTrigger::Runtime`, update the unit test at `engine/context.rs:353`. No behavioral change.

2. **`linear::rate_limit::RateLimitState`.** Atomic-backed backoff state. Unit tests: clear → record_429 → wait_if_backoff respects window, exponential growth bounds, `Retry-After` override.

3. **`LinearGraphqlClient::enumerate`.** Paginated GraphQL primitive. Unit + wiremock integration tests: single-page result, multi-page (verify cursor follow-through), 429 with `Retry-After` triggers backoff and retry, GraphQL `errors` array → typed error, status filter present / absent shapes.

4. **`daemon::orphan::reconcile`.** Pure filesystem function with a `keep_ids` set. Unit tests with `tempfile`: no orphans, mixed orphans + kept, fs-error per entry isolated, `_daemon/` skipped.

5. **`daemon::cold_start::ColdStart`.** Wires graphql + admission + cache.observe + dispatcher.admit_for_cold_start + orphan.reconcile. Unit tests with a mock `LinearGraphqlClient` and an in-memory `Dispatcher` stub: full success, partial enumeration → reconcile skipped, all-rejected-by-admission, mixed admitted + rejected.

6. **`Dispatcher::admit_for_cold_start`.** Spawns a ticket task in the registry with `CycleTrigger::ColdStart` bound for the first `run_cycle`. Subsequent webhooks for the same ticket use `CycleTrigger::Runtime` (the trigger is per-cycle, not per-ticket). Unit test: cold-start dispatch, then a webhook arrives mid-cycle → `pending_recheck` set → next cycle fires with `Runtime` trigger.

7. **Admission-filter eviction.** New `CacheEntry::pending_evict` flag (no new `DispatchMsg` variant — the flag carries the signal). Dispatcher path on `cache.snapshot.is_some()` AND admission reject sets the flag (and immediately evicts if no ticket task in registry). Re-admission clears the flag. Ticket task consumes the flag post-cycle. Unit tests: cached ticket → assignee changes → cycle in flight → cycle completes → cache evicted, **worktree + session_tempdir intact**; cached ticket → no in-flight cycle → immediate cache evict, no disk delete; re-admission after eviction reuses worktree; re-admission *during* pending_evict cancels the eviction.

8. **`runtime::run` rewire.** Park the listener behind a `ready_gate` (axum middleware returning 503), run cold start, fire `cold_start_began` and `cold_start_completed`, then `daemon_ready`, then open the gate. Slice 5's existing dispatcher / shutdown paths unchanged.

9. **Daemon-scoped events wiring.** `cold_start_began`, `cold_start_completed`, `orphan_reconcile_skipped`, `status_filter_dropped`, `linear_backoff_applied` written via the existing `_daemon/events.jsonl` writer. Per-ticket `worktree_deleted` / `session_tempdir_deleted` with the new reasons routed through the per-ticket writer.

10. **`ref:log-events` doc edits.** Add the new rows + the `source: cold_start` annotation. Edit the `daemon_ready` description note from slice 5 (drop the interim qualifier).

11. **`ref:cli` doc audit.** No CLI flag changes in slice 6; verify nothing leaked.

12. **E2E: cold start with two assigned tickets.** wiremock GraphQL backend serves two pages; assert both `cycle_started` events fire with `cycle.trigger = "cold_start"`, both ticket dirs exist, `cold_start_completed { enumerated: 2, admitted: 2, cycles_spawned: 2, orphans_deleted: 0, enum_partial: false }`, then `daemon_ready`.

13. **E2E: cold start with orphan reconcile.** Pre-create `<session_root>/old-ticket-1/` and `<session_root>/old-ticket-2/`. wiremock returns one ticket (`new-ticket-1`). Assert `orphans_deleted: 2`, both old dirs gone, `new-ticket-1` dir present.

14. **E2E: cold start partial.** wiremock returns 200 on page 1, 500 on page 2 after retries exhausted. Assert `cold_start_completed { enum_partial: true }`, `orphan_reconcile_skipped`, no orphan dirs deleted, listener accepting webhooks afterward.

15. **E2E: 429 backoff.** wiremock returns 429 with `Retry-After: 1` once, then 200. Assert single retry, `linear_backoff_applied` emitted once, cold start completes.

16. **E2E: webhook eviction with in-flight cycle.** Webhook A (admitted) → cycle dispatched → webhook B for same ticket but assignee changed off-operator → `pending_evict` set → cycle completes → cache empty, **worktree directory still present, session tempdir still present** under `<session_root>/<ticket-id>/`. Verify by stat'ing the paths after `cycle_completed`.

17. **E2E: webhook eviction without in-flight cycle.** Cached ticket post-cycle (non-cleanup, ticket task exited) → webhook with new assignee fails admission → cache evicted immediately → no `worktree_deleted` / `session_tempdir_deleted` events fire → disk paths intact.

17b. **E2E: re-admission cancels pending eviction.** Webhook A (admitted) → cycle dispatched → webhook B (revoke) sets `pending_evict` → webhook C (re-admit) clears it → cycle finishes normally → cache still has the entry, ticket task still alive, second cycle dispatches if rule re-eval matches.

17c. **E2E: re-admission after eviction reuses worktree.** Sequence of #16, then send webhook D (re-admit) → fresh ticket task spawns → `wt switch-create` is detected as no-op (worktree already exists) → cycle runs in the same tree.

18. **E2E: cold-start `cleanup` mode.** Same setup as #12 but invoke `roki cleanup`. Assert tickets enumerated and cached but no `[[rule]]`-driven cycle dispatched; `[[cleanup]]`-matching tickets dispatch cleanup cycles which then evict.

19. **E2E: listener parked during cold start.** Start daemon with a 5-second simulated cold start (test-only sleep hook); send webhook within that window; expect 503; after `daemon_ready`, send the same webhook; expect 200 + cycle dispatched.

20. **Slice 1-5 backwards-compat sweep.** Existing fixtures send webhook then SIGTERM; assert `daemon_ready` fires *before* the test's first webhook send (i.e. the `await_cycle_then_sigterm` helper now also waits for `daemon_ready` before sending). All existing per-ticket events unchanged; no `cycle.trigger` regression.

## 9. Testing Strategy

**Unit tests (in-crate):**

- `engine::context`: `CycleTrigger::ColdStart` populates `cycle.trigger` and `ROKI_CYCLE_TRIGGER` correctly.
- `linear::rate_limit`: backoff growth, `Retry-After` override, clear on success.
- `linear::graphql`: paginated assembly, status-filter present/absent, GraphQL `errors` array surfacing.
- `daemon::orphan`: `keep_ids` set semantics, `_daemon/` skip, fs-error isolation.
- `daemon::cold_start`: mocked graphql + dispatcher, full / partial / all-rejected / mixed scenarios.
- `daemon::dispatcher`: eviction path with cached entry, eviction path with no in-flight cycle, re-admit after eviction.
- `daemon::ticket_task`: `EvictAfterCycle` precedence over `pending_recheck`, eviction after failure-handler completion.

**E2E tests (binary-as-subprocess):**

The eight scenarios in §8 tasks 12-19. Driver: `wiremock` for both Linear webhook (slice 1+) AND the GraphQL endpoint (`POST /graphql`), real `roki-daemon` binary, slice-4 `wt` / `ghq` overrides, fake `claude` cli.

Concurrency assertion for #12: parse `<session_root>/<ticket-id>/events.jsonl` for both tickets, assert each ticket's `cycle_started` carries `cycle.trigger = "cold_start"` and that both fire before any webhook arrives.

Listener-parked assertion for #19: connect via TCP, send minimal webhook POST, expect 503 status before `daemon_ready` line in `_daemon/events.jsonl`, expect 200 after.

## 10. Backwards Compatibility

`PhaseContext::new` signature change is internal — no published binary API. Every call site is updated in task 1.

Slice 1-5 e2e fixtures invoke `await_cycle_then_sigterm`. Slice 6 changes the daemon to bind the listener early but reject with 503 until cold start completes. The helper is updated to first wait for `daemon_ready` in `_daemon/events.jsonl`, then send the webhook. Slice 1-5 fixtures use trivial WORKFLOW.toml that produces no Linear-side admission match (their `[admission].assignee` is `me` and their wiremock GraphQL is unconfigured) — slice 6 must give those fixtures a wiremock GraphQL responder that returns an empty `nodes: []` page so cold start completes cleanly with `enumerated: 0`.

**FR contract change**: fr:01 §Cycle dispatch (mid-cycle exception), fr:03 §Reassignment, and fr:05 §Cleanup item 2 are updated alongside this slice to specify that admission-revoke evicts only the cache entry, not the worktree or session tempdir. Operators relying on the prior "delete worktree+session_tempdir on revoke" contract see retained directories instead. Reclamation paths are unchanged for the cleanup-cycle and cold-start-orphan-reconcile cases.

`worktree_delete_requested` and `failure_unhandled` event lines from slice 4 are unchanged.

`webhook_skipped` adds an optional `source` field. Consumers that do not key on the field are unaffected.

`daemon_ready` emission point changes. Slice 5 fixtures that watch for the line still see it — only the timing shifts. Tests that asserted `daemon_ready` before any cold-start work no longer hold; the helper update covers them.

## 11. Dependency Additions

None. `reqwest` (slice 1), `serde_json`, `tokio` are already in workspace deps. `wiremock` is already a dev-dep for webhook fixtures.

## 12. Open Questions Deferred to Slice 7+

- **Background polling loop** (`fr:03 §Polling fallback` cadence-driven path). Activates the GraphQL primitive landed in slice 6 on a periodic cadence. Outage detection mechanism (heuristic vs. explicit signal) is itself a deferred decision — slice 7 must address it before polling can activate.
- **Refresh nudge** (`fr:03 §Refresh nudge`, `fr:10 §POST /api/refresh`). Depends on HTTP API.
- **Escalation queue** (`fr:06 §Escalation queue`). The slice-4 `failure_unhandled marker=cleanup_fs_error` and the slice-6 `orphan_reconcile_skipped` events remain forensic-only until then.
- **Hot reload of `WORKFLOW.toml` and `workflow/*.md`** (`fr:02`). Later slice.
- **HTTP API** (`fr:10`), **TUI** (`fr:11`), **`roki repo` / `roki log` / `roki events` CLIs** (`fr:09`). Later slices.
- **Worktree orphan reconcile**. Delegated to `worktrunk` per `fr:07 step 5`. Out of slice 6 by spec; if `worktrunk` integration ever changes, slice N+ revisits.
