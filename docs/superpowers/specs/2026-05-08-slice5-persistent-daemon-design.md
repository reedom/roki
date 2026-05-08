# Slice 5 — Persistent Daemon Design

Date: 2026-05-08
Scope: Lift the binary from single-shot pipeline to persistent daemon. Webhook listener stays bound across many cycles. Per-ticket diff cache gates re-evaluation on the `(status, labels, assignee)` triple. One cycle per ticket, serial; cross-ticket cycles run concurrently. SIGINT / SIGTERM drains in-flight cycle subprocesses within `[engine].shutdown_window_seconds` and exits cleanly. After this slice the binary stays alive until signaled and matches `fr:12 §Normal startup` / `fr:12 §Normal shutdown` for the webhook-driven path; cold-start enumeration remains deferred to slice 6.

## 1. Position in the Roadmap

Slice 5 lifts the runtime layer; the cycle engine (slices 1-4) is reused unchanged. Closes:

- `roki-daemon-persistent-loop` — `fr:12 §Normal startup` listener bound across cycles; `fr:12 §Normal shutdown` SIGINT / SIGTERM drains in-flight subprocesses.
- `roki-daemon-diff-cache` — `fr:07 §Diff cache` per-ticket `(repo, workflow_path, status, labels, assignee, cycle_id, pending_recheck, last_event_at)`.
- `roki-daemon-diff-detection` — `fr:07 §Diff detection` re-eval only when the tracked triple differs.
- `roki-daemon-cycle-in-flight` — `fr:07 §Cycle in-flight semantics` one cycle per ticket; `pending_recheck` queue mode; cleanup-cycle eviction.
- `roki-daemon-cross-ticket-parallel` — `fr:07 step 4 (concurrency clause only)` distinct tickets run concurrently.

Slices 1-4 provide: cycle engine (`engine::cycle::run_cycle`), `[[on_failure]]` routing, `[[cleanup]]` cycle, worktree lifecycle, FsPoison routing, structured event writer, admission filter, webhook receiver.

Out-of-scope, deferred to later slices:

- Cold-start Linear enumeration (`fr:07 §Cold start` steps 1-4). Slice 6.
- Orphan reconcile of session tempdirs at boot (`fr:07 step 5`, `fr:05 §Cleanup` item 3). Slice 6.
- Admission-filter eviction on assignee / repo allowlist revoke (`fr:05 §Cleanup` item 2). Slice 6 — depends on the polling path that materializes the revoke signal.
- Escalation queue (`fr:06 §Escalation queue`). Slice 7. `failure_unhandled marker=cleanup_fs_error` from slice 4 surfaces only via the event log until then.
- Hot reload of `WORKFLOW.toml` and `workflow/*.md` (`fr:02`). Later slice.
- HTTP API (`fr:10`), TUI (`fr:11`), `roki repo` / `roki log` / `roki events` CLIs (`fr:09`). Later slices.
- Polling fallback (`fr:03 §Polling fallback`) and refresh nudge (`fr:10`). Later slices.

## 2. Architecture

### 2.1 Module layout

```
crates/roki-daemon/src/
├── main.rs                         // unchanged
├── cli.rs                          // unchanged
├── runtime.rs                      // shrink: boot daemon, own shutdown
├── daemon/                         // NEW
│   ├── mod.rs
│   ├── cache.rs                    // DiffCache, CacheEntry, DiffOutcome
│   ├── dispatcher.rs               // webhook intake → admission → cache observe → spawn-or-route
│   ├── ticket_task.rs              // per-ticket actor: serial cycle loop with pending_recheck
│   └── shutdown.rs                 // SIGINT / SIGTERM trap, drain coordinator
├── linear/webhook.rs               // unchanged shape; tx capacity raised
├── admission.rs                    // unchanged
├── engine/                         // unchanged
└── ...
```

`runtime::run` boots the daemon, spawns the listener + dispatcher + shutdown coordinator, and blocks until shutdown completes. Each ticket runs as one `tokio::task` — cross-ticket concurrent, intra-ticket serial. `engine::cycle::run_cycle` and `engine::cleanup::*` are reused verbatim.

### 2.2 Types

```rust
// daemon::cache

pub struct CacheEntry {
    pub repo: AdmissionRepo,
    pub workflow_path: PathBuf,
    pub status: String,
    pub labels: BTreeSet<String>,
    pub assignee: String,
    pub cycle_id: Option<Uuid>,
    pub pending_recheck: bool,
    pub last_event_at: OffsetDateTime,
}

pub struct DiffCache {
    inner: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

pub enum DiffOutcome {
    Unchanged,
    Changed,        // tracked triple differs from prior value
    NewEntry,       // first observation
}

impl DiffCache {
    pub async fn observe(&self, t: &AdmittedTicket) -> DiffOutcome;
    pub async fn snapshot(&self, ticket_id: &str) -> Option<CacheEntry>;
    pub async fn set_cycle_id(&self, ticket_id: &str, id: Uuid);
    pub async fn clear_cycle_id(&self, ticket_id: &str);
    pub async fn set_pending_recheck(&self, ticket_id: &str);
    pub async fn take_pending_recheck(&self, ticket_id: &str) -> bool;
    pub async fn evict(&self, ticket_id: &str);
}
```

```rust
// daemon::dispatcher

pub struct Dispatcher {
    cache: Arc<DiffCache>,
    tickets: Mutex<HashMap<String, TicketHandle>>,
    workflow: Arc<WorkflowConfig>,
    cfg: Arc<RokiConfig>,
    me: Option<MeId>,
    mode: DispatchMode,
    shutdown: ShutdownToken,
}

struct TicketHandle {
    inbox: mpsc::Sender<DispatchMsg>,   // capacity 1; back-pressure surfaces as pending_recheck
    join: JoinHandle<()>,
}

enum DispatchMsg {
    Webhook(AdmittedTicket),
    Shutdown,
}
```

```rust
// daemon::shutdown

#[derive(Clone)]
pub struct ShutdownToken {
    notified: Arc<Notify>,
    flag: Arc<AtomicBool>,
}

impl ShutdownToken {
    pub fn fire(&self);
    pub async fn wait(&self);
    pub fn is_fired(&self) -> bool;
}
```

`AdmittedTicket`, `WorkflowConfig`, `RokiConfig`, `MeId`, `DispatchMode`, `engine::dispatch::evaluate`, `engine::cycle::run_cycle`, `engine::cleanup::post_cycle_delete`, `engine::on_failure::route` are unchanged from slices 1-4.

### 2.3 Concurrency model

| Task | Count | Owns | Talks to |
|---|---|---|---|
| Listener | 1 | bound `axum` server | sends `AdmittedTicket` → dispatcher |
| Dispatcher | 1 | `tickets` registry, `cache` writes for triple | sends `DispatchMsg::Webhook` → ticket task |
| Ticket task | N | per-ticket cycle loop, owns `cycle_id` / `pending_recheck` writes for its entry | calls `engine::cycle::run_cycle` |
| Shutdown | 1 | signal trap, drain timeout | flips `ShutdownToken`, sends `DispatchMsg::Shutdown` to every ticket task |

`DiffCache` uses `tokio::sync::RwLock`. Dispatcher writes `(status, labels, assignee, last_event_at)`. Ticket task writes `cycle_id`, `pending_recheck`. Field segregation removes write-write conflicts on the same entry; the lock is held only for the read or write itself, never across `await` boundaries that wait on cycle work.

### 2.4 Test seam

Reuse the slice 1-4 binary-as-subprocess scaffolding. Two new env overrides:

| Var | Effect |
|---|---|
| `ROKI_SHUTDOWN_WINDOW_OVERRIDE_SECONDS` | Override `[engine].shutdown_window_seconds` for tests that need a tight drain timeout. |
| `ROKI_DAEMON_EVENTS_DIR_OVERRIDE` | Override the daemon-scoped event log directory (default `<session_root>/_daemon/`). |

Both honored only in `cfg(test)` and in debug builds; release builds ignore them.

## 3. Diff Cache

Per `fr:07 §Diff cache`.

### 3.1 Insert / update path

`Dispatcher` calls `cache.observe(&admitted)`:

1. Read lock; look up entry by `admitted.ticket.id`.
2. Absent → drop read lock, take write lock, insert entry built from `admitted` (`cycle_id = None`, `pending_recheck = false`, `last_event_at = now`), return `NewEntry`.
3. Present and triple equal (`status` ∧ `labels` ∧ `assignee`) → update `last_event_at` only, return `Unchanged`.
4. Present and triple differs → upgrade to write lock, replace `(status, labels, assignee, last_event_at)`, return `Changed`. `cycle_id` and `pending_recheck` are not touched here — those are owned by the ticket task.

`labels` stored as `BTreeSet<String>` so equality is order-insensitive (`fr:07 §Diff detection` requires comparing label sets, not arrays).

### 3.2 Cycle-id field ownership

Only the ticket task writes `cycle_id`. Set before `run_cycle`, cleared after `run_cycle` returns. Dispatcher and shutdown never touch it. The field is the public observability signal "is a cycle in flight for this ticket".

### 3.3 Pending-recheck field ownership

Two writers, by design:

- **Ticket task** clears it via `take_pending_recheck` after a cycle terminates (per `fr:07 §Cycle in-flight semantics` step 3: cleared whether or not a new cycle starts).
- **Dispatcher** sets it via `set_pending_recheck` when `tickets` inbox is `Full` (back-pressure path; see §4.2).

Both operations are independent atomic transitions on a `bool`. The race where ticket task clears just as dispatcher sets is acceptable: a follow-up webhook arrives with its own `cache.observe`, and if the triple differs, the dispatcher routes it directly. The pending flag is a wakeup hint, not durable history — the cache triple is the authoritative state.

### 3.4 Eviction on cleanup-cycle completion

`fr:07 §Cycle in-flight semantics` step 2: cleanup cycle terminal directive → delete worktree + session_tempdir → evict cache entry.

Slice 5 sequence inside the ticket task:

1. `run_cycle` returns `Completed { kind: Cleanup, .. }`.
2. Call existing `engine::cleanup::post_cycle_delete` (slice 4 already deletes worktree first, session_tempdir second).
3. `cache.evict(ticket_id)`.
4. Drop the registry handle (the `tickets` mutex entry) and exit the task.

A subsequent webhook for the same ticket re-enters `Dispatcher::on_webhook`, hits `NewEntry` in `cache.observe`, and spawns a fresh ticket task.

## 4. Cycle Dispatch + Queue-Mode Preemption

Per `fr:07 §Cycle dispatch` and `fr:07 §Cycle in-flight semantics`.

### 4.1 Dispatcher path

```
recv AdmittedTicket from listener
  ↓
cache.observe(&admitted)
  ├─ Unchanged → emit `webhook_skipped reason=no_diff` (ref:log-events §Linear admission) ; return
  └─ Changed | NewEntry
      ↓
tickets registry (Mutex):
  ├─ present  → handle.inbox.try_send(Webhook(admitted))
  │              ├─ Ok      → return
  │              ├─ Full    → cache.set_pending_recheck(ticket_id) ; return
  │              └─ Closed  → tickets.remove(ticket_id) ; spawn-fresh path
  └─ absent   → spawn ticket_task(cache, cfg, workflow, mode) ; tickets.insert ; inbox.send(Webhook)
```

The dispatcher never blocks on cycle work. `try_send` short-circuits the back-pressure case to a flag write.

### 4.2 Pending-recheck via Full inbox

Inbox capacity = 1. On `Full`, the dispatcher knows the ticket task already has one queued message (the in-flight cycle's "wake up next" signal). Dropping the duplicate is safe because:

1. The cache triple was already updated by `cache.observe`. The triple is the state-of-truth.
2. The pending flag forces re-eval after the in-flight cycle terminates against the latest cache snapshot — that re-eval consumes the dropped webhook's update.

This implements `fr:07 §Cycle in-flight semantics` "If a cycle is already in flight for the ticket, the daemon sets `pending_recheck = true` instead of starting a new one".

### 4.3 Ticket task loop

```rust
loop {
    let msg = inbox.recv().await;
    match msg {
        None | Some(DispatchMsg::Shutdown) => break,
        Some(DispatchMsg::Webhook(admitted)) => {
            // Re-snapshot the cache: the triple may have moved since the dispatcher
            // observed it (multiple Full-Full webhooks queued one pending_recheck).
            let snapshot = cache.snapshot(&admitted.ticket.id).await
                .expect("ticket task entry exists while task is alive");

            let target = engine::dispatch::evaluate_from_cache(&snapshot, &workflow, mode);
            let (kind, dispatched) = match target {
                NoMatch => continue,
                Cycle { kind, rule, cleanup } => (kind, /* Rule | Cleanup */),
                CleanupShorthand => /* immediate-delete; emit events; evict; break */,
            };

            let cycle_id = Uuid::new_v4();
            cache.set_cycle_id(&admitted.ticket.id, cycle_id).await;
            let outcome = run_cycle(&executor, &admitted, &dispatched, ..., kind, None).await;
            cache.clear_cycle_id(&admitted.ticket.id).await;

            // Failure routing (slice-3 surface, unchanged)
            let outcome = handle_failed_cycle_if_needed(outcome, &workflow, ...).await;

            // Cleanup-cycle eviction
            if kind == CycleKind::Cleanup && outcome.is_completed() {
                engine::cleanup::post_cycle_delete(...).await?;
                cache.evict(&admitted.ticket.id).await;
                break;
            }

            // Pending-recheck loop-back
            if cache.take_pending_recheck(&admitted.ticket.id).await {
                // Build a synthetic AdmittedTicket from the current cache snapshot
                // and re-enter the loop via inbox. Pushing through the inbox
                // (rather than a tail-call) keeps shutdown signals fair.
                let refreshed = build_admitted_from_snapshot(&snapshot_after_cycle);
                let _ = inbox_self.try_send(DispatchMsg::Webhook(refreshed));
            }
        }
    }
}
```

`engine::dispatch::evaluate_from_cache` is a thin wrapper added in slice 5 that takes a `CacheEntry`-shaped view instead of an `AdmittedTicket`, since post-cycle re-eval has only the cache snapshot, not a fresh webhook payload. The wrapper feeds the snapshot's `(status, labels, assignee, repo, workflow_path)` into the same first-match logic slice 1-4 use. The original `evaluate(AdmittedTicket)` keeps working for the dispatcher path.

`build_admitted_from_snapshot` reconstructs an `AdmittedTicket` shape from the cache entry. The synthetic admitted carries the cached repo and triple — admission has already passed once for this entry, so re-running `admission::accept` is unnecessary and would in fact fail the polling path that slice 6 uses (admission depends on Linear-side state which the cache mirrors).

### 4.4 Failure cycle integration

The slice-3 `handle_failed_cycle` helper is moved out of `runtime.rs` into `daemon::ticket_task`. Behavior unchanged: `[[on_failure]]` first-match runs as `CycleKind::Failure`, recursion bounded to one level, `failure_unhandled` events emitted on no-match / recursion / handler infra error. Failure-handler success is treated as a non-cleanup completion — the ticket task continues serving subsequent webhooks. Failure-handler unhandled state retains worktree + session_tempdir per `fr:05 §Failure mode retention`.

### 4.5 Cleanup-shorthand

`engine::cleanup::delete_immediate` (slice 3 + slice 4) is invoked from the ticket task on `DispatchTarget::CleanupShorthand`. After delete, `cache.evict` and the task exits — same path as cleanup-cycle eviction.

### 4.6 Mode (run vs cleanup-only)

`DispatchMode` (slice 3) is captured by the dispatcher at boot from the CLI subcommand and forwarded into `evaluate` / `evaluate_from_cache` unchanged.

## 5. SIGINT / SIGTERM Shutdown

Per `fr:12 §Normal shutdown` and `fr:07 §Stop / shutdown`.

### 5.1 Sequence

1. `tokio::signal::unix` (SIGINT and SIGTERM) trapped in the shutdown task. First signal calls `ShutdownToken::fire`.
2. Listener observes the token via `axum`'s `with_graceful_shutdown` future, stops accepting new POSTs, and finishes any in-flight HTTP request.
3. Dispatcher inbox closes (listener side dropped).
4. Shutdown task iterates `tickets` registry under the mutex: `inbox.send(DispatchMsg::Shutdown).await` for each handle, then drops the sender.
5. Each ticket task: if no cycle in flight, exits immediately. If in flight, the existing per-cycle subprocess drop-guard from slice 2 / slice 4 sends SIGTERM to the active pre / run / post subprocess.
6. Shutdown task awaits every `JoinHandle` with `tokio::time::timeout(cfg.engine.shutdown_window_seconds)`.
7. On timeout, emit `shutdown_window_exceeded` (warn severity) per `ref:log-events §Daemon lifecycle`, then `JoinHandle::abort` the remaining tasks. Worktrees + session_tempdirs are not deleted — `fr:07 §Stop / shutdown` step 4 forbids it; the next slice-6 cold start reconciles.
8. Emit `daemon_shutdown_completed { drained, aborted }`.
9. Final exit code: `0` if `aborted == 0`, `1` otherwise.

### 5.2 Second signal

A second SIGINT / SIGTERM during drain bypasses the timeout and aborts every remaining ticket task immediately. Exit code `1`.

### 5.3 In-flight subprocess SIGTERM behavior

Slice 2 session-shape subprocesses already SIGTERM their managed child on `Drop`. Slice 4 worktree spawns honor the same drop-guard (run-phase subprocesses inherit it from `engine::phase`). Slice 5 adds nothing — dropping the per-ticket task's `Cycle` future propagates the cancel through the existing futures, the drop-guards fire, and the subprocesses receive SIGTERM. The shutdown window applies uniformly to session-shape and command-shape (`fr:12 §Cycle integration §Forced termination`).

## 6. Events

No new event names beyond those `ref:log-events §Daemon lifecycle` already lists. Slice 5 wires emission for:

| Event | When | Carries (slice 5 payload) |
|---|---|---|
| `daemon_started` | After config validate, before listener bind | `roki.toml` path, schema version |
| `daemon_ready` | Listener bound, dispatcher up | `webhook.bind_addr`. NOTE: `ref:log-events` description says "All subsystems up + cold start complete"; slice 5 fires before cold start because cold start is deferred to slice 6. The event still carries the same fields; the description in `ref:log-events` is correct as the long-term contract — slice 5 documents this as an interim. |
| `daemon_shutdown_began` | First SIGINT / SIGTERM | `signal` ∈ `SIGINT` / `SIGTERM`, `in_flight: usize` count of ticket tasks with `cycle_id.is_some()` |
| `daemon_shutdown_completed` | After drain (clean or aborted) | `drained: usize`, `aborted: usize` |
| `shutdown_window_exceeded` | Drain timeout fired | `aborted: usize`, ticket ids of aborted tasks |
| `webhook_skipped` (`reason = no_diff`) | `cache.observe` returned `Unchanged` | `ticket_id` (existing event row in `ref:log-events §Linear admission`) |
| `webhook_skipped` (`reason = signature_invalid` / `assignee_mismatch` / `repo_unresolvable`) | Existing slice 1 / 3 surface | unchanged |

`daemon_started`, `daemon_ready`, `daemon_shutdown_began`, `daemon_shutdown_completed`, `shutdown_window_exceeded` are written to a daemon-scoped event log at `<session_root>/_daemon/events.jsonl`. Per-ticket events (`cycle_started`, `cycle_completed`, `cycle_aborted`, `worktree_delete_requested`, `failure_unhandled`, etc.) keep writing to `<session_root>/<ticket-id>/events.jsonl` via the existing per-ticket `EventWriter`.

The `_daemon/` directory is reserved — the leading underscore avoids collision with any Linear ticket id (Linear identifiers do not start with `_`).

## 7. Config Additions

One new key.

| Block / Key | Type | Default | Range | Used by |
|---|---|---|---|---|
| `[engine].shutdown_window_seconds` | int | `30` | min `1`, max `600` | `fr:12 §Normal shutdown` |

`fr:12 §Normal shutdown` references "the configured shutdown window" without naming the key; slice 5 names it `[engine].shutdown_window_seconds`. Placed under `[engine]` because the window bounds engine cycle subprocesses' graceful drain — sibling to `[engine].max_iterations`.

`ref:config` row added in slice 5. `WORKFLOW.toml` schema unchanged.

## 8. Implementation Order

Tasks are listed so each one compiles and the test suite is green before the next starts.

1. **`daemon::shutdown`.** `ShutdownToken` (`Notify` + `AtomicBool`). Unit tests for `fire`, `wait`, `is_fired`, double-fire idempotency.
2. **`daemon::cache`.** `CacheEntry`, `DiffCache`, `DiffOutcome`. Unit tests: `observe` matrix (status only / labels only / assignee only / multi-field / unchanged / new entry), `BTreeSet` order-insensitivity, `set_cycle_id` / `clear_cycle_id`, `set_pending_recheck` / `take_pending_recheck`, `evict` then re-insert.
3. **`engine::dispatch::evaluate_from_cache`.** Wrapper consuming `CacheEntry`. Unit tests covering the same matrix as `evaluate(AdmittedTicket)` with cache-shaped input.
4. **`daemon::ticket_task`.** Per-ticket actor with a mock executor. Unit tests: dispatch on first webhook, queue on mid-cycle webhook (`pending_recheck` set/take), evict on cleanup-cycle completion, exit on `Shutdown`, `[[on_failure]]` handler reuse path delegates to slice 3.
5. **`daemon::dispatcher`.** Webhook intake, `cache.observe`, registry spawn / lookup, `try_send` Full → set pending, `try_send` Closed → respawn. Unit tests with a fake admitted-ticket stream.
6. **`runtime::run` rewire.** Replace single-shot loop with dispatcher boot + shutdown coordinator. Slice 1-4 single-cycle e2e tests adapted: send ticket → wait for cycle event → send SIGTERM → expect clean exit.
7. **`[engine].shutdown_window_seconds` config.** `RokiConfig` parse + default + range validation. Tests for missing block (default), out-of-range refusal.
8. **Daemon-scoped event log.** `<session_root>/_daemon/events.jsonl`. `EventWriter::open_daemon`. Wire `daemon_started`, `daemon_ready`, `daemon_shutdown_began`, `daemon_shutdown_completed`, `shutdown_window_exceeded`. `ref:log-events` already lists every name; slice 5 only adds emission.
9. **E2E: persistent across two cycles, same ticket.** Webhook A (`status=Todo`) → cycle runs → webhook B (`status=InProgress`) mid-cycle → `pending_recheck` set → cycle ends → re-eval → second cycle dispatched. Single binary instance.
10. **E2E: cross-ticket parallel.** Webhook A (`ticket-1`) + webhook B (`ticket-2`) within ms → both cycles run concurrently (verify via overlapping `cycle_started` event timestamps and both ticket dirs created).
11. **E2E: cleanup eviction + re-admit.** Cleanup cycle for `ticket-1` → cache evicted → webhook for `ticket-1` again → fresh ticket task spawned (verify via `daemon_*` event ordering plus a second `cycle_started` after `worktree_deleted`).
12. **E2E: SIGINT drains.** In-flight cycle + send SIGINT → `daemon_shutdown_began` → cycle completes within window → `daemon_shutdown_completed { aborted: 0 }` → exit 0. Variant: long-running fake `claude` (`sleep 600`) exceeds window → `shutdown_window_exceeded` → exit 1.
13. **E2E: unchanged-triple webhook is no-op.** Send the same payload twice → second observation emits `webhook_skipped reason=no_diff`, no `cycle_started` event line.
14. **Slice 1-4 backwards compat sweep.** Existing single-cycle e2e tests adapted to send one webhook then SIGTERM. Helper `await_cycle_then_sigterm` added to the test scaffolding.

## 9. Testing Strategy

**Unit tests (in-crate):**

- `daemon::cache`: triple diff matrix, `BTreeSet` order-insensitivity, `pending_recheck` set/take race (two-task interleave via `tokio::test`), evict then re-insert.
- `daemon::dispatcher`: admission rejection logged + skipped; `Unchanged` skipped; `Changed` routes; `Full` inbox sets pending; `Closed` inbox respawns; cross-ticket isolation.
- `daemon::ticket_task`: serial loop with fake `run_cycle`; mid-cycle webhook → pending; cleanup cycle → evict; failure cycle delegates to slice-3 handler; shutdown msg breaks the loop; pending-recheck loop-back fires after non-cleanup completion.
- `daemon::shutdown`: token fire / wait / is_fired; double-fire idempotency.
- `engine::dispatch::evaluate_from_cache`: same coverage as `evaluate(AdmittedTicket)` with cache-shaped input.

**E2E tests (binary-as-subprocess):**

The seven scenarios in §8 tasks 9-13. Driver: `wiremock` Linear webhook, real `roki-daemon` binary, slice-4 `wt` / `ghq` overrides, fake `claude` cli.

Concurrency assertion: parse `<session_root>/<ticket-id>/events.jsonl` for both tickets, assert each ticket's `cycle_started` has `ts` < the other ticket's `cycle_completed`.

Long-running cycle for SIGINT timeout: fake `claude` script that `sleep 600`. `ROKI_SHUTDOWN_WINDOW_OVERRIDE_SECONDS=2` keeps the test fast. Expected: `shutdown_window_exceeded` event, exit 1.

## 10. Backwards Compatibility

Slice 1-4 e2e fixtures send one webhook then expect process exit. After slice 5, the binary stays alive. Two adapter changes:

- Test scaffolding adds `await_cycle_then_sigterm(child, expected_event)` that waits for the named event line then sends SIGTERM.
- Existing fixtures call the helper instead of `child.wait().await`.

`WORKFLOW.toml` unchanged. `roki.toml` adds optional `[engine].shutdown_window_seconds` with default `30`. Fixtures that omit the key still load.

`worktree_delete_requested` / `failure_unhandled marker=cleanup_fs_error` event lines from slice 4 are unchanged. Slice 5 adds daemon-scoped event lines but does not modify per-ticket events.

## 11. Dependency Additions

None. `tokio::signal::unix`, `tokio::sync::{RwLock, Mutex, Notify, mpsc}`, `tokio::task::JoinHandle`, `tokio::time::timeout` are already in the workspace `tokio` dep. No new crates.

## 12. Open Questions Deferred to Slice 6+

- **Cold-start enumeration** (`fr:07 §Cold start` steps 1-4). Slice 6 wires the Linear pagination + cycle dispatch with `trigger = cold_start`.
- **Orphan reconcile of session tempdirs** (`fr:07 step 5`, `fr:05 §Cleanup` item 3). Slice 6. Worktree orphans are delegated to `worktrunk` per `fr:07 step 5`.
- **Admission-filter eviction** (`fr:05 §Cleanup` item 2). Slice 6. Depends on the polling path.
- **Polling fallback** (`fr:03 §Polling fallback`). Slice 6 — required to materialize the revoke signal.
- **Escalation queue** (`fr:06 §Escalation queue`). Slice 7. The slice-4 `failure_unhandled marker=cleanup_fs_error` and `escalation_added` event row in `ref:log-events` remain dormant until then.
- **Hot reload of `WORKFLOW.toml` and `workflow/*.md`** (`fr:02`). Later slice.
- **HTTP API** (`fr:10`), **TUI** (`fr:11`), **`roki repo` / `roki log` / `roki events` CLIs** (`fr:09`). Later slices.
- **`daemon_ready` description drift** (`ref:log-events §Daemon lifecycle`). Long-term contract is "all subsystems up + cold start complete"; slice 5 fires before cold start. Slice 6 brings the emission point in line with the canonical description.
