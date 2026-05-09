# Slice 7 — Escalation Queue Design

Date: 2026-05-09
Scope: Implement the in-memory escalation queue per `fr:06 §Escalation queue`. Reroute daemon-stuck failures (recursive failure cycles, cleanup-time fs errors, daemon-internal cycle-less errors) from `failure_unhandled` to `escalation_added`. Reconcile FR drift introduced by slices 3–5 interim wording.

## 1. Position in the Roadmap

Slice 7 closes:

- `roki-daemon-escalation-queue` — `fr:06 §Escalation queue` triggers 1 and 2. In-memory bounded queue; `escalation_added` event; cycle-bound entries cleared on ticket eviction; cycle-less entries persist until daemon restart.
- `roki-daemon-failure-unhandled-narrowing` — `fr:01 §Failure handling` step 4 + `fr:06 §Failure-handler cycle`. `failure_unhandled` carries `marker = none` only. Recursive failures (`recursion_bound`) and cleanup-time fs errors (`cleanup_fs_error`) become `escalation_added` instead.
- `roki-daemon-cleanup-fs-routing` — `fr:06 §Daemon-detected failure kinds` table for `fs_poison`: cleanup-time fs errors do not enter `[[on_failure]]`; they go to the queue directly. Closes the slice-3/4 interim that routed them through `handle_failed_cycle`.
- `roki-doc-failure-routing-reconcile` — drop "Daemon exits 1" from `fr:08 §Event catalog` and `ref:log-events §Cycle engine events`; drop `recursion_bound` and `cleanup_fs_error` markers from `failure_unhandled`; add `escalation_added` row carrying the new payload.

Slices 1–6 provide: cycle engine, `[[on_failure]]` routing, `[[cleanup]]` cycle, worktree lifecycle, FsPoison routing, structured event writer (per-ticket and `_daemon` scoped), admission filter, webhook receiver, persistent dispatcher with diff cache, ticket-task registry, SIGINT/SIGTERM drain, cold-start enumeration, admission-eviction, orphan reconcile, paginated GraphQL primitive.

Out of scope, deferred to later slices:

- **HTTP `GET /api/escalations`** (`fr:10 §GET /api/escalations`). Depends on the HTTP API server. The queue type is exported on a stable surface ready for the HTTP slice to consume.
- **TUI escalations view** (`fr:11 §Escalations`). Depends on HTTP API.
- **`POST /api/refresh` and refresh-nudge polling fallback** (`fr:03`, `fr:10`). Unchanged from slice 6 deferral.
- **Hot reload of `WORKFLOW.toml`** (`fr:02`). Restart-required. The `WORKFLOW.toml` hot-reload validation failure case named in `fr:06 §Escalation queue` trigger 2 is therefore unreachable in this slice; the queue type accepts it once hot reload lands.
- **Persistent escalation queue** (`fr:06 §Boundaries`). Out of scope by FR — queue is in-memory only and resets on daemon restart.

## 2. Architecture

### 2.1 Module layout

```
crates/roki-daemon/src/
├── escalation/
│   ├── mod.rs                   // NEW: EscalationQueue, EscalationEntry
│   └── ring.rs                  // NEW: bounded ring (newest-wins eviction)
├── events.rs                    // extend: Event::EscalationAdded; FailureMarker enum trims unused variants in §6.1
├── runtime.rs                   // wire EscalationQueue at boot, before cold_start
├── daemon/
│   ├── dispatcher.rs            // owns Arc<EscalationQueue>; ticket-eviction calls queue.evict_ticket
│   ├── ticket_task.rs           // accepts queue handle; passes to runner; calls queue.evict_ticket on cleanup-cycle terminal
│   ├── real_runner.rs           // recursion + handler-failed paths push to queue + emit escalation_added (was: failure_unhandled)
│   ├── cold_start.rs            // orphan-reconcile fs errors push cycle-less entries (was: warn-log only per slice 6 §5.4)
│   └── orphan.rs                // surface fs errors to caller; cold_start enqueues
└── engine/
    └── cleanup.rs               // cleanup_fs_error pushes to queue + emits escalation_added (was: failure_unhandled marker=cleanup_fs_error)
```

### 2.2 Types

```rust
// escalation::entry

#[derive(Debug, Clone)]
pub struct EscalationEntry {
    pub ticket_id: Option<String>,        // None for cycle-less daemon errors
    pub cycle_id: Option<Uuid>,           // None for cycle-less daemon errors
    pub failure_kind: FailureKind,        // engine::outcome::FailureKind
    pub phase: Option<PhaseKind>,         // None for cycle-less daemon errors
    pub timestamp: OffsetDateTime,
    pub error_text: String,               // sanitized at construction (ANSI strip + UTF-8 replace)
}
```

```rust
// escalation::mod

pub struct EscalationQueue {
    inner: Arc<Mutex<Ring<EscalationEntry>>>,
    capacity: usize,                       // from RokiConfig::escalation::queue_size
    daemon_writer: Arc<Mutex<EventWriter>>,
}

impl EscalationQueue {
    pub fn new(capacity: usize, daemon_writer: Arc<Mutex<EventWriter>>) -> Arc<Self>;

    /// Cycle-bound entry. Emits `escalation_added` to the daemon-scoped event log.
    pub fn push_cycle(
        &self,
        ticket_id: String,
        cycle_id: Uuid,
        failure_kind: FailureKind,
        phase: PhaseKind,
        error_text: String,
    );

    /// Cycle-less daemon-internal entry. ticket_id, cycle_id, phase all None.
    pub fn push_daemon(
        &self,
        failure_kind: FailureKind,
        error_text: String,
    );

    /// Drop every cycle-bound entry whose ticket_id matches. Cycle-less entries
    /// are unaffected. Used at cleanup-cycle terminal, admission-revoke, and
    /// orphan reconcile.
    pub fn evict_ticket(&self, ticket_id: &str);

    /// Snapshot for HTTP API (slice 10) and tests. Order: oldest first.
    pub fn snapshot(&self) -> Vec<EscalationEntry>;
}
```

```rust
// escalation::ring

/// Bounded ring with newest-wins eviction. Capacity is fixed at construction.
/// On overflow the oldest entry is dropped; the daemon emits a warn-severity
/// log with the dropped entry's failure_kind and ticket_id.
struct Ring<T> {
    buf: VecDeque<T>,
    capacity: usize,
}
```

```rust
// events.rs additions

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    // ... existing variants
    EscalationAdded {
        ts: String,
        ticket_id: Option<String>,
        cycle_id: Option<String>,
        failure: FailureMetaSer,           // failure.kind, phase, error_text; iter omitted
    },
}
```

`FailureMarker` keeps only `None` after slice 7 (see §6.1). The variant is preserved for one release to keep `failure_unhandled` payloads parseable; `RecursionBound` and `CleanupFsError` are removed in the same commit that lands slice 7.

### 2.3 Wiring at boot

`runtime::run_inner`:

1. Load config + workflow.
2. Open daemon-scoped `EventWriter` at `<session_root>/_daemon.events.jsonl`.
3. Construct `Arc<EscalationQueue>` with `cfg.escalation.queue_size` and the daemon writer.
4. Construct `Dispatcher::new(... escalation: queue.clone() ...)`.
5. Construct `ColdStart::new(... escalation: queue.clone() ...)`.
6. Cold start runs (cold-start config-load failure during enumeration is the slice's cycle-less push site — see §3.4).
7. Emit `daemon_ready`. Begin webhook accept.

The queue's `Arc` is cloned into the dispatcher (which forwards to `RealCycleRunner` via the same path that already carries `cfg`), and into `ColdStart` and `engine::cleanup::*` callers. No global state.

### 2.4 Concurrency model

Single `tokio::sync::Mutex` around the ring. Push and snapshot are sub-millisecond — no contention concern at the volumes implied by the failure surface (handler-cycle-fail is rare; cleanup-fs-error is rarer). Eviction holds the mutex once per ticket-eviction event, not per cycle.

`escalation_added` event emission happens inside the push call after the mutex is released. Emission failure is logged but does not unwind the push (queue state and event log can drift on disk-full; accepted, same policy as `EventWriter` callers throughout the daemon).

## 3. Behavior

### 3.1 Trigger 1 — failure cycle itself fails (`recursion_bound`)

`daemon::real_runner::handle_failed_cycle`:

| Current (slice 3–5) | Slice 7 |
|---|---|
| `failed_kind == CycleKind::Failure` → emit `failure_unhandled marker=recursion_bound`, return `Unhandled` | `failed_kind == CycleKind::Failure` → `queue.push_cycle(ticket, cycle_id, kind, phase, error_text)`, return `Unhandled` |
| handler cycle returned `Failed` → emit `failure_unhandled marker=recursion_bound`, return `Unhandled` | same → `queue.push_cycle(...)` for the handler cycle's id, return `Unhandled` |
| handler cycle `Err(infra)` → emit `failure_unhandled marker=recursion_bound`, return `Unhandled` | same → `queue.push_cycle(...)` for the original failed cycle's id with `failure_kind = FsPoison` synthesized for the infra error | 

The ticket task remains alive on the same admission. The next webhook / cold-start trigger is admitted normally; the failed cycle's worktree and session tempdir are retained per `fr:06 §Worktree retention`.

### 3.2 Trigger 1 — cleanup-time fs error (`cleanup_fs_error`)

`engine::cleanup::{delete_immediate, post_cycle_delete}`:

| Current (slice 3) | Slice 7 |
|---|---|
| `remove_dir_all` Err → emit `failure_unhandled marker=cleanup_fs_error`, return `CleanupError::FsError` | accept new `&EscalationQueue` arg → `queue.push_cycle(ticket, cycle_id, FsPoison, /*phase*/ none-equivalent, error_text)`. Return `CleanupError::FsError` unchanged. |
| `worktree::remove` Err → same | same |

Cleanup-cycle scope: `phase` is recorded as `PhaseKind::Post` for `post_cycle_delete` (post-terminal cleanup) and as a sentinel `Post` for `delete_immediate` shorthand. Rationale: cleanup deletion runs outside the cycle's pre/run/post phases but `fr:06 §Escalation queue` requires cycle-routed entries to carry a concrete `phase`. `Post` is the closest fit because both paths run after the cycle's terminal directive.

`real_runner` no longer treats `CleanupError::FsError` as a normal cycle failure that flows into `handle_failed_cycle`. Concrete change in `real_runner.rs:57–72` and `:114–123`:

| Before | After |
|---|---|
| `delete_immediate` Err → `CycleResult::Failed { meta: boot_path_failure(), kind: Cleanup }` (which flows to `handle_failed_cycle`, which then matches `[[on_failure]]` if `kind=fs_poison`) | `delete_immediate` Err → escalation queue already pushed inside `cleanup.rs`. Real runner returns `CycleResult::CleanupFsError` (new variant). Ticket task drops the cycle and resumes — does NOT route to `[[on_failure]]`. |
| `post_cycle_delete` result discarded (`let _ = ... .await`) | identical event path; queue push happens inside `cleanup.rs`. The discard is preserved (cleanup-cycle terminal already happened; the only externally-visible artifact is the queue entry + `escalation_added` event). |

This implements `fr:06 §Daemon-detected failure kinds` row for `fs_poison`: "Cleanup-time fs errors land in the escalation queue, not `[[on_failure]]`."

### 3.3 Trigger 2 — daemon-internal cycle-less errors

In-scope sites:

- **Cold-start orphan reconcile fs error**. `daemon::orphan::reconcile` already collects `(ticket_id, io::Error)` pairs (slice 6 §5.4 carries them as `OrphanReport::fs_errors`). Slice 6 emits a warn log only. Slice 7 pushes a cycle-less entry per pair — `failure_kind = FsPoison`, `error_text = format!("orphan reconcile {ticket_id}: {err}")`. The warn log is preserved as a complement (queue is in-memory; the log persists across restarts).
- **Cold-start config load failure that does not refuse startup**. Slice 6 §5.4 drops this case ("the daemon refuses startup"). Slice 7 introduces no new path — the queue type accepts it but no emitter exists yet. Documented as a forward-compatibility hook for hot reload.
- **Liquid render failure before subprocess spawn**. Per `fr:06 §Escalation queue` trigger 2. In current code Liquid render happens inside `engine::phase::*` after the subprocess context is constructed; the failure already carries cycle association and surfaces as `FailureKind::TemplateError` on a normal cycle, which `[[on_failure]]` can match. **No-op for slice 7**: this trigger remains a normal cycle failure because Liquid render is always inside a cycle in the current architecture. Documented divergence from `fr:06`'s wording — see §7.

Cycle-less entries persist in the ring until daemon restart. `evict_ticket` does not touch them.

### 3.4 Eviction

| Eviction trigger | Source | Queue action |
|---|---|---|
| Cleanup-cycle terminal directive (`fr:05 §Cleanup` item 1) | `daemon::ticket_task` after `CycleResult::Completed { kind: Cleanup, .. }` and `post_cycle_delete` | `queue.evict_ticket(&ticket_id)` |
| Cleanup shorthand path | `daemon::ticket_task` after `CycleResult::ShorthandDeleted` | `queue.evict_ticket(&ticket_id)` |
| Admission revoked (slice 6) | `daemon::dispatcher::evict_admission` | `queue.evict_ticket(&ticket_id)` |
| Orphan reconcile (cold-start; slice 6) | `daemon::cold_start` after orphan-delete loop | `queue.evict_ticket(&ticket_id)` per deleted orphan |

Eviction races: a ticket task can be in-flight when the dispatcher revokes admission (slice 6 contract retains the worktree until natural cycle end). The ticket task may push to the queue after `evict_ticket` runs. This is accepted: `fr:06` describes the queue as an operator-attention surface, not a transactional log. The orphaned entry will be cleared on the next admission cleanup or on daemon restart (queue is in-memory only).

### 3.5 Ordering and overflow

- Push order is insertion order (oldest first on snapshot).
- Capacity overflow drops the oldest entry. A warn-severity tracing log is emitted with `(dropped_kind, dropped_ticket_id)`. No structured event for drops — adding one would itself be a daemon-stuck signal that could overflow the queue under sustained pressure.
- Default capacity: `[escalation].queue_size = 64`. Rationale: failures that reach the queue are rare (recursive handler fail; cleanup fs error; daemon-internal fs error). 64 covers a multi-day outage of the operator's escalation reading without lossy overflow on a healthy daemon.

## 4. Configuration

`roki.toml` adds a new section:

```toml
[escalation]
queue_size = 64                  # default 64; min 1; max 1024
```

`ref:config` adds:

| Key | Required | Type | Default | Validation | Linked FR |
|---|---|---|---|---|---|
| `[escalation].queue_size` | no | int | `64` | `1..=1024` | `fr:06 §Escalation queue` |

`fr:02` adds the section to its example block. No CLI flag — restart-only setting.

## 5. Event surface

### 5.1 New event

```jsonl
{"event":"escalation_added","ts":"2026-05-09T12:34:56Z","ticket_id":"TEAM-123","cycle_id":"...","failure":{"kind":"fs_poison","phase":"post","error_text":"cleanup remove_dir_all failed: ..."}}
```

For cycle-less daemon errors, `ticket_id`, `cycle_id` are absent (`Option::None` skipped via `serde(skip_serializing_if = "Option::is_none")`); `failure.phase` is also absent.

### 5.2 `failure_unhandled` narrowing

After slice 7 the only emit site of `failure_unhandled` is `real_runner::handle_failed_cycle` line 200–209 (no `[[on_failure]]` match path). All payloads carry `marker = none`. The `marker` field is retained on the wire for one release to keep `roki events --kind failure_unhandled` parsers stable; `fr:08` and `ref:log-events` are updated to document the narrowed surface.

## 6. Doc updates

### 6.1 `docs/fr/08-observability-logs.md` line 66

Replace:

> `failure_unhandled` | A cycle failure was not recovered: no `[[on_failure]]` match (`marker = none`), handler cycle itself failed (`marker = recursion_bound`), or handler cycle hit an infra error (`marker = recursion_bound`). Carries `(ticket_id, cycle_id, cycle_kind, failure.kind, phase, error_text, marker)`. Daemon exits 1. No escalation queue entry ([06-failure-handling §Failure-handler cycle](06-failure-handling.md)).

With:

> `failure_unhandled` | A cycle failure with no `[[on_failure]]` match (`marker = none`). Carries `(ticket_id, cycle_id, cycle_kind, failure.kind, phase, error_text, marker)`. Daemon stays alive; the ticket task drops the cycle and waits for the next admission ([06-failure-handling §Failure-handler cycle](06-failure-handling.md)). Recursive failure-cycle failures and cleanup-time fs errors enter the escalation queue instead — see `escalation_added`.

Same edit applied verbatim to `docs/reference/log-events.md` line 43.

### 6.2 `docs/reference/log-events.md` `escalation_added` row

Replace the existing one-liner with:

> `escalation_added` | Escalation queue entry added | Daemon-stuck failure: failure-handler cycle that itself failed, cleanup-time fs error, or daemon-internal error with no cycle association. Carries `(ticket_id?, cycle_id?, failure.kind, phase?, error_text)`. Cycle-less entries omit `ticket_id`, `cycle_id`, `phase` ([fr:06 §Escalation queue](../fr/06-failure-handling.md)).

### 6.3 `docs/fr/01-engine-model.md`

Line 180 already correctly states the escalation routing for recursive failures. No edit. The line 167 row for `process_crash` and line 170 row for `fs_poison` are unchanged.

### 6.4 `docs/fr/06-failure-handling.md`

No edit. `fr:06` is the canonical source slice 7 implements. Verify after implementation that the `phase` field semantics in §Escalation queue match §3.2 (slice 7 records `Post` for cleanup-fs entries; `fr:06 §Escalation queue` says "Cycle-routed failures... always carry concrete `phase` ∈ {`pre`, `run`, `post`}"). `Post` satisfies that constraint.

### 6.5 `docs/fr/02-configuration.md` and `docs/reference/config.md`

Add `[escalation].queue_size` per §4.

### 6.6 `docs/fr/10-http-api.md`

`fr:10 §GET /api/escalations` already describes the queue snapshot shape. The slice-7 `EscalationEntry` matches the documented payload (ticket_id?, cycle_id?, kind, phase?, timestamp, error_text). No edit. Slice 7 ships the `roki-api-types` projection of `EscalationEntry` into the shared crate so the HTTP API slice can wire it in without reshaping.

## 7. Documented divergence

`fr:06 §Escalation queue` lists "Liquid render failure before any subprocess is spawned" as a cycle-less daemon-internal trigger. In the current architecture (slices 1–6), Liquid render runs inside `engine::phase::*` after `PhaseContext::new`, which always carries cycle association. Slice 7 therefore does not produce cycle-less entries from Liquid render: such failures already surface as `FailureKind::TemplateError` on a normal cycle and route through `[[on_failure]]` if matched.

The FR wording suggests a daemon-startup-time Liquid render path (e.g. validating templates at load) which does not exist in the current implementation. Slice 7 adds the queue's `push_daemon` API so a future slice can wire that path without re-architecting.

## 8. Tests

### 8.1 New e2e

- `escalation_recursion_smoke.rs` — `[[rule]]` matches; rule cycle fails (process_crash); `[[on_failure]]` matches; handler cycle fails. Expect: zero `failure_unhandled`, one `escalation_added` carrying handler cycle id + `marker`-equivalent payload, ticket task remains in registry.
- `escalation_cleanup_fs_error_smoke.rs` — `[[cleanup]]` matches; `worktree::remove` is forced to fail (read-only parent dir). Expect: zero `failure_unhandled`, one `escalation_added` with `failure.kind = fs_poison`. Verify `[[on_failure]]` did NOT match (no `cycle_started kind=failure` event).
- `escalation_orphan_reconcile_smoke.rs` — Cold start; orphan session tempdir present; tempdir parent dir made read-only between enumeration and reconcile. Expect: cycle-less `escalation_added` (no ticket_id, no cycle_id), warn log, `cold_start_completed` still fires.
- `escalation_evicted_on_cleanup_smoke.rs` — Push a recursion entry for ticket A; later dispatch a cleanup-cycle on A; verify the entry is gone after cleanup terminal directive.
- `escalation_capacity_overflow_smoke.rs` — Configure `queue_size = 2`; push 3 entries; verify oldest is dropped and warn log mentions `(dropped_kind, dropped_ticket_id)`.

### 8.2 Updated existing e2e

- `recursion_bound_smoke.rs` — current expectation: one `failure_unhandled` event with `marker=recursion_bound`. After slice 7: one `escalation_added` event, zero `failure_unhandled`. Rename file to `escalation_recursion_smoke.rs` (replaces it).
- `worktree_cleanup_fs_error_smoke.rs` — current: `failure_unhandled marker=cleanup_fs_error`. After slice 7: `escalation_added`. Rename to `escalation_cleanup_fs_error_smoke.rs`.
- `failure_unhandled_smoke.rs` — current: `marker=none`. Unchanged.
- `on_failure_smoke.rs` — current: zero `failure_unhandled`. Add: zero `escalation_added`.
- `worktree_fs_poison_smoke.rs` — handler matched + succeeded path. Unchanged.

### 8.3 Unit

- `escalation::ring` — capacity, FIFO order, overflow drop signal.
- `escalation::EscalationQueue::evict_ticket` — leaves cycle-less entries; drops cycle-bound entries with the matching ticket_id only.
- `events.rs` — `escalation_added` JSON shape; `Option::None` fields elided.

## 9. Implementation sequence

The order minimizes a window where the daemon emits inconsistent events.

1. Land `escalation::{ring, mod}` types + unit tests. No call sites yet.
2. Add `Event::EscalationAdded` + `events.rs` test.
3. Wire `Arc<EscalationQueue>` through `runtime → dispatcher → ticket_task → real_runner` and `runtime → cold_start`. No behavior change yet (queue exists but nothing pushes).
4. Migrate `engine::cleanup::*` push sites: queue + `escalation_added` instead of `failure_unhandled`. Update `worktree_cleanup_fs_error_smoke` to expect new event in the same commit.
5. Migrate `real_runner::handle_failed_cycle` recursion paths: queue + `escalation_added`. Update `recursion_bound_smoke` in the same commit.
6. Add cold-start orphan reconcile cycle-less push. Update `cold_start.rs` and slice-6 orphan tests.
7. Add eviction calls in `dispatcher`, `ticket_task`, `cold_start::orphan`. Add `escalation_evicted_on_cleanup_smoke`.
8. Add config key `[escalation].queue_size` + validation.
9. Remove `FailureMarker::RecursionBound` and `::CleanupFsError` variants; `FailureMarker::None` is the only remaining variant. Update doctests + serialization tests.
10. Apply doc updates §6.1 / §6.2 / §6.5.

Each step compiles and passes its own e2e in isolation. Steps 4–7 each fix one fr:06 contract; reverting any single step rolls back to the slice-6 surface for that contract only.

## 10. Boundaries / non-goals

- No HTTP API endpoint. `GET /api/escalations` is a later slice.
- No TUI. Slice 11.
- No persistence. The queue is in-memory; restart resets it. `fr:06 §Boundaries`.
- No Slack / email / Linear write from the daemon for escalations. `fr:06 §Boundaries`.
- No retroactive replay of prior daemon-stuck failures from `failure_unhandled` event log entries — operators that need the historic surface continue to grep `roki events --kind failure_unhandled` over the on-disk log.
- No deferred Liquid-render-at-startup wiring — only the API surface for it.

## 11. Traceability

- **Roadmap**: `roadmap.md` > Specs > `roki-skeleton` (failure surface). `Boundary Strategy > "Failure routing"`.
- **FR**: `fr:06 §Escalation queue`, `fr:06 §Daemon-detected failure kinds`, `fr:06 §Failure-handler cycle`, `fr:01 §Failure handling` step 4 + line 182, `fr:08 §Event catalog`, `fr:10 §GET /api/escalations`.
- **Reference**: `ref:log-events §Cycle engine events` (`failure_unhandled`, `escalation_added`), `ref:config §[escalation]`.
- **Prior slices**: Slice 3 introduced `[[on_failure]]` and `failure_unhandled`. Slice 4 added `cleanup_fs_error` interim. Slice 6 added orphan reconcile fs-error warn-log interim. Slice 7 closes all three interims by routing them to the queue.

## 12. Open questions

None survive the upstream constraints.

- Marker-routing canonicality: resolved by `fr:06`/`fr:01` per §1.
- `failure_unhandled` daemon-exit policy: resolved — daemon stays alive (drops "Daemon exits 1" wording from `fr:08` + `ref:log-events`).
- Cleanup-fs `phase` field: forced by `fr:06 §Escalation queue` ("phase ∈ {pre, run, post}"). `Post` is the only fit.
- Liquid-render-at-startup: forced deferral. Architecture currently has no cycle-less Liquid render path; queue API ready when one lands.
