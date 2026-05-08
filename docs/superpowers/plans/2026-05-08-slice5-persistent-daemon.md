# Slice 5 Persistent Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lift the binary from single-shot pipeline to persistent daemon. Webhook listener stays bound across many cycles. Per-ticket diff cache gates re-evaluation on `(status, labels, assignee)`. One cycle per ticket, serial; cross-ticket cycles run concurrently. SIGINT / SIGTERM drains in-flight subprocesses within `[engine].shutdown_window_seconds`.

**Architecture:** New `daemon::` module tree owns the runtime layer: `daemon::cache` (per-ticket diff cache), `daemon::dispatcher` (webhook intake → admission → cache observe → spawn-or-route), `daemon::ticket_task` (per-ticket actor running a serial cycle loop with `pending_recheck`), `daemon::shutdown` (SIGINT / SIGTERM trap, drain coordinator). `runtime::run` shrinks to a boot-and-block orchestrator. The slice 1-4 cycle engine (`engine::cycle::run_cycle`, `engine::cleanup`, `engine::on_failure`) is reused unchanged.

**Tech Stack:** Rust 2024 (workspace edition), `tokio` async runtime, slice 1-4 deps (`liquid`, `shell-words`, `async-trait`, `serde_json`, `serde`, `tempfile`, `wiremock`, `reqwest`, `nix`, `serde_yaml_ng`, `uuid`, `time`, `clap`). No new crates.

**Spec:** `docs/superpowers/specs/2026-05-08-slice5-persistent-daemon-design.md` (committed in Task 0).

**Working branch:** `slice5-persistent-daemon-spec` (already created; spec committed there in Task 0). All implementation commits land on this branch.

---

## File Structure

### Created

| Path | Responsibility |
|---|---|
| `crates/roki-daemon/src/daemon/mod.rs` | Module root: `pub mod cache; pub mod dispatcher; pub mod shutdown; pub mod ticket_task;`. |
| `crates/roki-daemon/src/daemon/shutdown.rs` | `ShutdownToken` — `Notify` + `AtomicBool` pair. `fire`, `wait`, `is_fired`. |
| `crates/roki-daemon/src/daemon/cache.rs` | `CacheEntry`, `DiffCache`, `DiffOutcome`. Per-ticket diff cache with field-segregated writes. |
| `crates/roki-daemon/src/daemon/ticket_task.rs` | Per-ticket actor: serial cycle loop with `pending_recheck`; cleanup-cycle eviction; failure handler delegation. |
| `crates/roki-daemon/src/daemon/dispatcher.rs` | Webhook intake → `admission::accept` → `cache.observe` → spawn-or-route. Owns the `tickets` registry. |
| `crates/roki-daemon/tests/e2e/persistent_two_cycles_smoke.rs` | E2E: same-ticket persistent re-eval after pending_recheck. |
| `crates/roki-daemon/tests/e2e/persistent_parallel_smoke.rs` | E2E: two tickets cycle concurrently. |
| `crates/roki-daemon/tests/e2e/persistent_cleanup_evict_readmit_smoke.rs` | E2E: cleanup eviction then fresh re-admit. |
| `crates/roki-daemon/tests/e2e/persistent_sigint_drain_smoke.rs` | E2E: SIGINT during in-flight cycle drains within window. |
| `crates/roki-daemon/tests/e2e/persistent_sigint_timeout_smoke.rs` | E2E: SIGINT window exceeded → `shutdown_window_exceeded` → exit 1. |
| `crates/roki-daemon/tests/e2e/persistent_no_diff_smoke.rs` | E2E: duplicate webhook with unchanged triple is a no-op. |
| `crates/roki-daemon/tests/e2e/support/persistent.rs` | Shared helper `await_event_then_sigterm` for converting slice 1-4 single-shot fixtures. |

### Modified

| Path | Change |
|---|---|
| `crates/roki-daemon/src/lib.rs` (or `main.rs` if no `lib.rs`) | Declare `pub mod daemon;`. |
| `crates/roki-daemon/Cargo.toml` | Add `[[test]]` entries for the six new e2e files; add `tests/e2e/support/persistent.rs` to the existing test-support module list. |
| `crates/roki-daemon/src/runtime.rs` | Replace single-shot loop with daemon boot. `handle_failed_cycle` and `on_failure_to_rule` move to `daemon::ticket_task`. `cleanup_to_rule` moves to `daemon::ticket_task`. |
| `crates/roki-daemon/src/engine/dispatch.rs` | Add `evaluate_from_cache(snapshot: &CacheSnapshot, workflow, mode)` mirroring `evaluate(AdmittedTicket, ...)`. |
| `crates/roki-daemon/src/config/roki.rs` | Add `shutdown_window_seconds: u32` field to `EngineSection` (default `30`, range `[1, 600]`). |
| `crates/roki-daemon/src/events.rs` | Add `Event` variants `DaemonStarted`, `DaemonReady`, `DaemonShutdownBegan`, `DaemonShutdownCompleted`, `ShutdownWindowExceeded`, `WebhookSkipped`. |
| `crates/roki-daemon/src/linear/webhook.rs` | Raise `mpsc` channel capacity (currently 1, used as a "first-cycle lock") and remove the `cycle_started` `AtomicBool` (no longer needed — dispatcher owns this state). |
| `docs/reference/config.md` | Add row for `[engine].shutdown_window_seconds`. |
| `docs/reference/log-events.md` | No new rows — events already listed; update emission notes if needed. |
| All slice 1-4 e2e fixtures (`crates/roki-daemon/tests/e2e/*.rs`) | Replace `child.wait().await` terminal expectations with `await_event_then_sigterm`. |

---

## Cross-Task Conventions

- **Branch:** `slice5-persistent-daemon-spec` (created in Task 0). All commits land here. Push when done with each task.
- **Test command:** `cargo test -p roki-daemon` for unit + e2e.
- **Build verification:** `cargo build -p roki-daemon` after each task. CI also runs `cargo clippy -p roki-daemon -- -D warnings` and `cargo fmt --check`.
- **No new crates.** Any new dependency suggestion is wrong — re-read this line.
- **Daemon-scoped events** are written via `EventWriter::open(session_root, "_daemon")`, producing `<session_root>/_daemon.events.jsonl`. The leading `_` is preserved by `events::sanitize_ticket` and never collides with a Linear identifier.
- **Per-ticket events** continue to use `EventWriter::open(session_root, ticket_id)` per slice 1-4.
- **Failure routing** (slice 3 `[[on_failure]]`) is invoked from inside the ticket task. Behavior unchanged: first-match handler runs as `CycleKind::Failure`, recursion bound 1 level, `failure_unhandled` events on no-match / recursion.
- **Module dead-code suppression** — new modules use the `#![allow(dead_code)]` comment block at the top of the file matching `admission.rs` / `runtime.rs`'s opening pattern. Once `runtime::run` calls them, remove the suppression.
- **Atomic ordering** — `ShutdownToken::flag` writes `Release`, reads `Acquire`. Matches the slice-1 `cycle_started` pattern in `runtime.rs`.

---

## Task 0: Branch + Spec Commit (DONE)

**Status:** Already complete. The spec lives at `docs/superpowers/specs/2026-05-08-slice5-persistent-daemon-design.md` and was committed on `slice5-persistent-daemon-spec` ahead of this plan.

If you arrived here on a different branch, run:

```bash
git checkout slice5-persistent-daemon-spec
git pull --ff-only origin slice5-persistent-daemon-spec  # if the branch was pushed
```

Otherwise no action required — proceed to Task 1.

---

## Task 1: `daemon::shutdown` — `ShutdownToken`

**Files:**
- Create: `crates/roki-daemon/src/daemon/mod.rs`
- Create: `crates/roki-daemon/src/daemon/shutdown.rs`
- Modify: `crates/roki-daemon/src/main.rs` (declare `pub mod daemon;`) — or `lib.rs` if it exists

- [ ] **Step 1: Create the `daemon` module root**

```rust
// crates/roki-daemon/src/daemon/mod.rs
//! Persistent-daemon runtime layer (slice 5).
//!
//! Per-ticket diff cache, per-ticket actor task, dispatcher, and
//! shutdown coordinator. The cycle engine in `engine::*` is reused
//! unchanged.

pub mod shutdown;
// `cache`, `dispatcher`, `ticket_task` are added in subsequent tasks.
```

- [ ] **Step 2: Declare the module in the binary crate**

In `crates/roki-daemon/src/main.rs`, find the existing `mod` lines (`mod admission;`, `mod runtime;`, etc.) and add:

```rust
mod daemon;
```

(Place it alphabetically among the existing `mod` lines.)

- [ ] **Step 3: Write the failing test**

```rust
// crates/roki-daemon/src/daemon/shutdown.rs
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

#[derive(Clone)]
pub struct ShutdownToken {
    notified: Arc<Notify>,
    flag: Arc<AtomicBool>,
}

impl ShutdownToken {
    pub fn new() -> Self {
        Self {
            notified: Arc::new(Notify::new()),
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn fire(&self) {
        self.flag.store(true, Ordering::Release);
        self.notified.notify_waiters();
    }

    pub async fn wait(&self) {
        if self.flag.load(Ordering::Acquire) {
            return;
        }
        self.notified.notified().await;
    }

    pub fn is_fired(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

impl Default for ShutdownToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn fire_wakes_waiter() {
        let tok = ShutdownToken::new();
        let tok2 = tok.clone();
        let waiter = tokio::spawn(async move { tok2.wait().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!tok.is_fired());
        tok.fire();
        timeout(Duration::from_millis(200), waiter)
            .await
            .expect("waiter should wake within 200ms")
            .expect("join");
        assert!(tok.is_fired());
    }

    #[tokio::test]
    async fn wait_returns_immediately_if_already_fired() {
        let tok = ShutdownToken::new();
        tok.fire();
        timeout(Duration::from_millis(50), tok.wait())
            .await
            .expect("wait should return immediately when flag already set");
    }

    #[tokio::test]
    async fn double_fire_is_idempotent() {
        let tok = ShutdownToken::new();
        tok.fire();
        tok.fire();
        assert!(tok.is_fired());
    }
}
```

- [ ] **Step 4: Run tests and verify they pass**

```bash
cargo test -p roki-daemon daemon::shutdown
```

Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/daemon/mod.rs \
        crates/roki-daemon/src/daemon/shutdown.rs \
        crates/roki-daemon/src/main.rs
git commit -m "feat(daemon): ShutdownToken with notify+atomic flag"
```

---

## Task 2: `daemon::cache` — `DiffCache` + `CacheEntry` + `DiffOutcome`

**Files:**
- Create: `crates/roki-daemon/src/daemon/cache.rs`
- Modify: `crates/roki-daemon/src/daemon/mod.rs` (add `pub mod cache;`)

- [ ] **Step 1: Add the module declaration**

In `crates/roki-daemon/src/daemon/mod.rs` add the line `pub mod cache;` after `pub mod shutdown;`.

- [ ] **Step 2: Write the failing tests + implementation skeleton**

```rust
// crates/roki-daemon/src/daemon/cache.rs
#![allow(dead_code)]

//! Per-ticket diff cache (fr:07 §Diff cache).
//!
//! Cache key = Linear issue identifier. Value = `CacheEntry` carrying the
//! tracked triple plus per-ticket runtime state (`cycle_id`,
//! `pending_recheck`).
//!
//! Field ownership:
//! - Dispatcher writes `(status, labels, assignee, last_event_at)` via
//!   `observe`.
//! - Ticket task writes `cycle_id` via `set_cycle_id` / `clear_cycle_id`,
//!   and `pending_recheck` via `take_pending_recheck`.
//! - Dispatcher additionally sets `pending_recheck` on the back-pressure
//!   path (`try_send` Full); see `daemon::dispatcher`.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use time::OffsetDateTime;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::admission::AdmittedTicket;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub repo: String,                       // ghq path of the admission-resolved repo
    pub workflow_path: Option<PathBuf>,     // per-repo TOML override (None for top-level)
    pub status: String,
    pub labels: BTreeSet<String>,
    pub assignee: String,
    pub cycle_id: Option<Uuid>,
    pub pending_recheck: bool,
    pub last_event_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOutcome {
    Unchanged,
    Changed,
    NewEntry,
}

#[derive(Default, Clone)]
pub struct DiffCache {
    inner: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

impl DiffCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert / update from the freshly admitted ticket.
    /// Returns the diff classification.
    pub async fn observe(&self, admitted: &AdmittedTicket) -> DiffOutcome {
        let triple_now = (
            admitted.ticket.status.clone(),
            admitted
                .ticket
                .labels
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            admitted.ticket.assignee_id.clone().unwrap_or_default(),
        );

        // Read fast path: classify against current state.
        {
            let map = self.inner.read().await;
            if let Some(entry) = map.get(&admitted.ticket.id) {
                if entry.status == triple_now.0
                    && entry.labels == triple_now.1
                    && entry.assignee == triple_now.2
                {
                    drop(map);
                    let mut w = self.inner.write().await;
                    if let Some(e) = w.get_mut(&admitted.ticket.id) {
                        e.last_event_at = OffsetDateTime::now_utc();
                    }
                    return DiffOutcome::Unchanged;
                }
            }
        }

        // Write path: insert new or update tracked triple.
        let mut map = self.inner.write().await;
        match map.get_mut(&admitted.ticket.id) {
            Some(entry) => {
                entry.status = triple_now.0;
                entry.labels = triple_now.1;
                entry.assignee = triple_now.2;
                entry.last_event_at = OffsetDateTime::now_utc();
                DiffOutcome::Changed
            }
            None => {
                map.insert(
                    admitted.ticket.id.clone(),
                    CacheEntry {
                        repo: admitted.ghq.clone(),
                        workflow_path: None,
                        status: triple_now.0,
                        labels: triple_now.1,
                        assignee: triple_now.2,
                        cycle_id: None,
                        pending_recheck: false,
                        last_event_at: OffsetDateTime::now_utc(),
                    },
                );
                DiffOutcome::NewEntry
            }
        }
    }

    pub async fn snapshot(&self, ticket_id: &str) -> Option<CacheEntry> {
        self.inner.read().await.get(ticket_id).cloned()
    }

    pub async fn set_cycle_id(&self, ticket_id: &str, id: Uuid) {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            e.cycle_id = Some(id);
        }
    }

    pub async fn clear_cycle_id(&self, ticket_id: &str) {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            e.cycle_id = None;
        }
    }

    pub async fn set_pending_recheck(&self, ticket_id: &str) {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            e.pending_recheck = true;
        }
    }

    pub async fn take_pending_recheck(&self, ticket_id: &str) -> bool {
        if let Some(e) = self.inner.write().await.get_mut(ticket_id) {
            let prior = e.pending_recheck;
            e.pending_recheck = false;
            prior
        } else {
            false
        }
    }

    pub async fn evict(&self, ticket_id: &str) {
        self.inner.write().await.remove(ticket_id);
    }

    pub async fn in_flight_count(&self) -> usize {
        self.inner
            .read()
            .await
            .values()
            .filter(|e| e.cycle_id.is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::ticket::NormalizedTicket;

    fn admitted(id: &str, status: &str, labels: &[&str], assignee: Option<&str>) -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                id.into(),
                assignee.map(String::from),
                status.into(),
                labels.iter().map(|s| s.to_string()).collect(),
                String::new(),
                String::new(),
            ),
            ghq: "github.com/example/repo".into(),
        }
    }

    #[tokio::test]
    async fn first_observe_is_new_entry() {
        let c = DiffCache::new();
        let r = c.observe(&admitted("t1", "Todo", &["a"], Some("u1"))).await;
        assert_eq!(r, DiffOutcome::NewEntry);
    }

    #[tokio::test]
    async fn second_observe_same_triple_is_unchanged() {
        let c = DiffCache::new();
        let a = admitted("t1", "Todo", &["a"], Some("u1"));
        c.observe(&a).await;
        let r = c.observe(&a).await;
        assert_eq!(r, DiffOutcome::Unchanged);
    }

    #[tokio::test]
    async fn status_change_returns_changed() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
        let r = c
            .observe(&admitted("t1", "InProgress", &[], Some("u1")))
            .await;
        assert_eq!(r, DiffOutcome::Changed);
    }

    #[tokio::test]
    async fn label_reorder_is_unchanged() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &["a", "b"], Some("u1")))
            .await;
        let r = c
            .observe(&admitted("t1", "Todo", &["b", "a"], Some("u1")))
            .await;
        assert_eq!(r, DiffOutcome::Unchanged);
    }

    #[tokio::test]
    async fn label_added_returns_changed() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &["a"], Some("u1"))).await;
        let r = c
            .observe(&admitted("t1", "Todo", &["a", "b"], Some("u1")))
            .await;
        assert_eq!(r, DiffOutcome::Changed);
    }

    #[tokio::test]
    async fn assignee_change_returns_changed() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
        let r = c.observe(&admitted("t1", "Todo", &[], Some("u2"))).await;
        assert_eq!(r, DiffOutcome::Changed);
    }

    #[tokio::test]
    async fn cycle_id_set_clear_round_trips() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
        let id = Uuid::new_v4();
        c.set_cycle_id("t1", id).await;
        assert_eq!(c.snapshot("t1").await.unwrap().cycle_id, Some(id));
        c.clear_cycle_id("t1").await;
        assert_eq!(c.snapshot("t1").await.unwrap().cycle_id, None);
    }

    #[tokio::test]
    async fn take_pending_recheck_clears_and_returns_prior() {
        let c = DiffCache::new();
        c.observe(&admitted("t1", "Todo", &[], Some("u1"))).await;
        assert!(!c.take_pending_recheck("t1").await);
        c.set_pending_recheck("t1").await;
        assert!(c.take_pending_recheck("t1").await);
        assert!(!c.take_pending_recheck("t1").await);
    }

    #[tokio::test]
    async fn evict_then_reinsert_is_new_entry() {
        let c = DiffCache::new();
        let a = admitted("t1", "Todo", &[], Some("u1"));
        c.observe(&a).await;
        c.evict("t1").await;
        let r = c.observe(&a).await;
        assert_eq!(r, DiffOutcome::NewEntry);
    }

    #[tokio::test]
    async fn missing_ticket_take_pending_returns_false() {
        let c = DiffCache::new();
        assert!(!c.take_pending_recheck("missing").await);
    }
}
```

- [ ] **Step 3: Run tests and verify they pass**

```bash
cargo test -p roki-daemon daemon::cache
```

Expected: 9 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/daemon/cache.rs \
        crates/roki-daemon/src/daemon/mod.rs
git commit -m "feat(daemon): DiffCache with field-segregated writes"
```

---

## Task 3: `engine::dispatch::evaluate_from_cache`

**Files:**
- Modify: `crates/roki-daemon/src/engine/dispatch.rs`

- [ ] **Step 1: Inspect the existing `evaluate` function**

Open `crates/roki-daemon/src/engine/dispatch.rs`. The existing `evaluate(admitted: &AdmittedTicket, workflow, mode)` calls `crate::rule::first_cleanup_match(admitted, ...)` and `crate::rule::first_match(admitted, ...)`.

Both `first_match` and `first_cleanup_match` only consult `admitted.ticket.status` and `admitted.ticket.labels`. We construct a synthetic `AdmittedTicket` from the cache snapshot — admission has already passed once, and the admission-resolved repo is already cached.

- [ ] **Step 2: Write the failing test**

Append to `crates/roki-daemon/src/engine/dispatch.rs` `#[cfg(test)] mod tests`:

```rust
    use crate::daemon::cache::CacheEntry;
    use std::collections::BTreeSet;
    use time::OffsetDateTime;

    fn snapshot_for(status: &str, labels: &[&str]) -> CacheEntry {
        CacheEntry {
            repo: "github.com/example/repo".into(),
            workflow_path: None,
            status: status.into(),
            labels: labels.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
            assignee: "u1".into(),
            cycle_id: None,
            pending_recheck: false,
            last_event_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn evaluate_from_cache_dispatches_cleanup_first() {
        let wf = workflow_with(vec![rule_for("Done")], vec![cleanup_for(Some("Done"))]);
        let snap = snapshot_for("Done", &[]);
        match evaluate_from_cache("t1", &snap, &wf, DispatchMode::Default) {
            DispatchTarget::Cycle { kind: CycleKind::Cleanup, .. } => {}
            other => panic!("expected Cleanup cycle, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_from_cache_no_match_when_unchanged_falls_through() {
        let wf = workflow_with(vec![rule_for("InProgress")], vec![cleanup_for(Some("Done"))]);
        let snap = snapshot_for("Triage", &[]);
        match evaluate_from_cache("t1", &snap, &wf, DispatchMode::Default) {
            DispatchTarget::NoMatch => {}
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }
```

- [ ] **Step 3: Add the wrapper**

Append to `crates/roki-daemon/src/engine/dispatch.rs` (above the `#[cfg(test)]` block):

```rust
/// Like `evaluate`, but takes a cache snapshot instead of a freshly admitted
/// ticket. Used by the per-ticket task to re-dispatch after a cycle ends
/// when `pending_recheck` was set. Admission has already passed for this
/// entry; we synthesize an `AdmittedTicket` from the snapshot fields so the
/// existing rule-matching helpers (`first_match`, `first_cleanup_match`) can
/// be reused unchanged.
pub fn evaluate_from_cache<'a>(
    ticket_id: &str,
    snap: &crate::daemon::cache::CacheEntry,
    workflow: &'a crate::config::workflow::WorkflowConfig,
    mode: DispatchMode,
) -> DispatchTarget<'a> {
    let synthetic = AdmittedTicket {
        ticket: crate::linear::ticket::NormalizedTicket::new(
            ticket_id.to_string(),
            Some(snap.assignee.clone()),
            snap.status.clone(),
            snap.labels.iter().cloned().collect(),
            String::new(),
            String::new(),
        ),
        ghq: snap.repo.clone(),
    };
    evaluate(&synthetic, workflow, mode)
}
```

Note: `NormalizedTicket::new` is `pub(crate)` (per `linear::ticket`). `engine::dispatch` lives in the same crate, so the call compiles.

- [ ] **Step 4: Run tests**

```bash
cargo test -p roki-daemon engine::dispatch
```

Expected: existing 5 + new 2 = 7 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/engine/dispatch.rs
git commit -m "feat(engine): evaluate_from_cache wrapper for ticket-task re-dispatch"
```

---

## Task 4: Extend `events::Event` with daemon-lifecycle variants

**Files:**
- Modify: `crates/roki-daemon/src/events.rs`

These variants are listed in `docs/reference/log-events.md §Daemon lifecycle` and `§Linear admission`. Slice 5 wires the emitter; the canonical names are fixed.

- [ ] **Step 1: Write the failing test**

Append to `crates/roki-daemon/src/events.rs` `#[cfg(test)] mod tests`:

```rust
    use serde_json::Value;

    #[test]
    fn daemon_started_serializes_with_event_tag() {
        let ev = Event::DaemonStarted {
            ts: "2026-05-08T00:00:00Z".into(),
            config_path: "/tmp/roki.toml".into(),
            schema_version: 1,
        };
        let v: Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["event"], "daemon_started");
        assert_eq!(v["config_path"], "/tmp/roki.toml");
    }

    #[test]
    fn webhook_skipped_no_diff_serializes() {
        let ev = Event::WebhookSkipped {
            ts: "2026-05-08T00:00:00Z".into(),
            ticket_id: "ENG-1".into(),
            reason: WebhookSkipReason::NoDiff,
        };
        let v: Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["event"], "webhook_skipped");
        assert_eq!(v["reason"], "no_diff");
    }

    #[test]
    fn shutdown_window_exceeded_carries_aborted_ids() {
        let ev = Event::ShutdownWindowExceeded {
            ts: "2026-05-08T00:00:00Z".into(),
            aborted: 2,
            aborted_ticket_ids: vec!["ENG-1".into(), "ENG-2".into()],
        };
        let v: Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["event"], "shutdown_window_exceeded");
        assert_eq!(v["aborted"], 2);
        assert_eq!(v["aborted_ticket_ids"][1], "ENG-2");
    }
```

- [ ] **Step 2: Add the variants**

Modify `crates/roki-daemon/src/events.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookSkipReason {
    NoDiff,
    SignatureInvalid,
    AssigneeMismatch,
    RepoUnresolvable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownSignal {
    Sigint,
    Sigterm,
}
```

Then extend the `Event` enum:

```rust
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    // existing variants unchanged ...
    CycleCompleted { /* ... */ },
    FailureUnhandled { /* ... */ },
    WorktreeDeleteRequested { /* ... */ },

    // NEW (slice 5):
    DaemonStarted {
        ts: String,
        config_path: String,
        schema_version: u32,
    },
    DaemonReady {
        ts: String,
        webhook_bind_addr: String,
    },
    DaemonShutdownBegan {
        ts: String,
        signal: ShutdownSignal,
        in_flight: usize,
    },
    DaemonShutdownCompleted {
        ts: String,
        drained: usize,
        aborted: usize,
    },
    ShutdownWindowExceeded {
        ts: String,
        aborted: usize,
        aborted_ticket_ids: Vec<String>,
    },
    WebhookSkipped {
        ts: String,
        ticket_id: String,
        reason: WebhookSkipReason,
    },
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p roki-daemon events
```

Expected: 3 new tests + existing pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/events.rs
git commit -m "feat(events): daemon lifecycle + webhook_skipped variants"
```

---

## Task 5: `[engine].shutdown_window_seconds` config field

**Files:**
- Modify: `crates/roki-daemon/src/config/roki.rs`

- [ ] **Step 1: Locate the existing `EngineSection`**

```bash
grep -n "EngineSection\|max_iterations\|engine" crates/roki-daemon/src/config/roki.rs | head
```

Find the struct and its `Default` impl. Note the validation pattern (range checks usually live in a `validate()` method or in the `try_from` for the loaded TOML).

- [ ] **Step 2: Write the failing test**

Append to the existing test module in `crates/roki-daemon/src/config/roki.rs`:

```rust
    #[test]
    fn shutdown_window_seconds_defaults_to_30() {
        let cfg = parse_minimal_roki_toml();
        assert_eq!(cfg.engine.shutdown_window_seconds, 30);
    }

    #[test]
    fn shutdown_window_seconds_below_min_is_rejected() {
        let body = roki_toml_with_engine_extra("shutdown_window_seconds = 0");
        let err = RokiConfig::load_from_str(&body).expect_err("expected validation error");
        let msg = format!("{err}");
        assert!(msg.contains("shutdown_window_seconds"), "got: {msg}");
    }

    #[test]
    fn shutdown_window_seconds_above_max_is_rejected() {
        let body = roki_toml_with_engine_extra("shutdown_window_seconds = 601");
        let err = RokiConfig::load_from_str(&body).expect_err("expected validation error");
        let msg = format!("{err}");
        assert!(msg.contains("shutdown_window_seconds"), "got: {msg}");
    }
```

If `parse_minimal_roki_toml` / `roki_toml_with_engine_extra` / `RokiConfig::load_from_str` do not exist, mirror whatever helpers the existing `[engine].max_iterations` tests use; copy the smallest valid TOML literal those tests use and append `shutdown_window_seconds = N` to its `[engine]` block.

- [ ] **Step 3: Add the field with default + range**

In `EngineSection`:

```rust
#[serde(default = "default_shutdown_window_seconds")]
pub shutdown_window_seconds: u32,

// ...

fn default_shutdown_window_seconds() -> u32 {
    30
}
```

In whatever validation function `EngineSection` already runs (mirror `max_iterations`'s `min` check), add:

```rust
if !(1..=600).contains(&self.shutdown_window_seconds) {
    return Err(RokiConfigError::SchemaValidation {
        key: "[engine].shutdown_window_seconds".into(),
        message: format!(
            "must be in 1..=600, got {}",
            self.shutdown_window_seconds
        ),
    });
}
```

(Use the actual error variant the existing `max_iterations` validation uses; the snippet above shows the shape, not necessarily the exact variant.)

- [ ] **Step 4: Run tests**

```bash
cargo test -p roki-daemon config::roki
```

Expected: existing tests + 3 new pass.

- [ ] **Step 5: Update `docs/reference/config.md`**

Add a row to the `roki.toml` schema table (same table where `[engine].max_iterations` lives), in alphabetical order within `[engine]`:

```markdown
| `[engine].shutdown_window_seconds` | no | int | `30` | min `1`, max `600` | [fr:12 §Normal shutdown](../fr/12-daemon-lifecycle.md) |
```

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/config/roki.rs docs/reference/config.md
git commit -m "feat(config): [engine].shutdown_window_seconds (default 30s)"
```

---

## Task 6: `daemon::ticket_task` — per-ticket actor

**Files:**
- Create: `crates/roki-daemon/src/daemon/ticket_task.rs`
- Modify: `crates/roki-daemon/src/daemon/mod.rs` (add `pub mod ticket_task;`)

This task does not yet wire the ticket task into the runtime — that happens in Task 8 (`runtime::run` rewire). It only defines the actor's loop with a mock executor so the unit tests can exercise the dispatch / pending / eviction paths.

- [ ] **Step 1: Add module declaration**

In `crates/roki-daemon/src/daemon/mod.rs`:

```rust
pub mod cache;
pub mod shutdown;
pub mod ticket_task;
```

- [ ] **Step 2: Write the module skeleton + tests together**

```rust
// crates/roki-daemon/src/daemon/ticket_task.rs
#![allow(dead_code)]

//! Per-ticket actor (slice 5).
//!
//! Each admitted ticket gets one `tokio::task` running this loop. The
//! task reads webhooks from a capacity-1 mpsc inbox, dispatches against
//! the current cache snapshot, runs one cycle at a time, and re-arms
//! against `pending_recheck` after each cycle terminates. The task exits
//! on cleanup-cycle eviction or on `DispatchMsg::Shutdown`.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::admission::AdmittedTicket;
use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::daemon::cache::DiffCache;
use crate::engine::dispatch::{DispatchMode, DispatchTarget, evaluate_from_cache};
use crate::engine::outcome::CycleKind;
use crate::events::{Event, EventWriter, FailureMarker, FailureMetaSer, now_rfc3339};

/// Message carried on a ticket task's inbox.
#[derive(Debug)]
pub enum DispatchMsg {
    Webhook(AdmittedTicket),
    Shutdown,
}

/// Outcome of one ticket-task loop iteration. Returned by the inner step
/// function so the test harness can assert against the decision the task
/// took without reading the full event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Dispatched { kind: CycleKind, evicted: bool },
    NoMatch,
    QueuedPending,
    Shutdown,
}

/// Trait the ticket task uses to invoke a cycle. Production wires this to
/// `engine::cycle::run_cycle` via `RealCycleRunner` (Task 8); unit tests
/// substitute `MockCycleRunner` to exercise the loop deterministically.
#[async_trait::async_trait]
pub trait CycleRunner: Send + Sync {
    async fn run_cycle(
        &self,
        admitted: &AdmittedTicket,
        target: DispatchTarget<'_>,
        cycle_id: Uuid,
    ) -> CycleResult;
}

#[derive(Debug, Clone)]
pub enum CycleResult {
    Completed {
        kind: CycleKind,
        iters: u32,
    },
    Failed {
        meta: crate::engine::outcome::FailureMeta,
        kind: CycleKind,
    },
    /// Cleanup-shorthand path — already deleted dirs as a side effect.
    ShorthandDeleted,
}

/// Run the ticket-task loop until `inbox` closes or `Shutdown` arrives.
/// Tests instantiate this with a `MockCycleRunner`.
pub async fn run_ticket_task<R: CycleRunner>(
    ticket_id: String,
    cache: Arc<DiffCache>,
    workflow: Arc<WorkflowConfig>,
    cfg: Arc<RokiConfig>,
    mode: DispatchMode,
    runner: Arc<R>,
    mut inbox: mpsc::Receiver<DispatchMsg>,
    inbox_self: mpsc::Sender<DispatchMsg>,
    session_root: PathBuf,
) {
    while let Some(msg) = inbox.recv().await {
        let outcome = match msg {
            DispatchMsg::Shutdown => StepOutcome::Shutdown,
            DispatchMsg::Webhook(admitted) => {
                step_once(
                    &ticket_id,
                    admitted,
                    cache.clone(),
                    workflow.clone(),
                    cfg.clone(),
                    mode,
                    runner.clone(),
                    &inbox_self,
                    &session_root,
                )
                .await
            }
        };

        if matches!(
            outcome,
            StepOutcome::Shutdown | StepOutcome::Dispatched { evicted: true, .. }
        ) {
            break;
        }
    }
}

/// One iteration of the ticket-task loop. Extracted so unit tests can
/// drive it directly without spawning a task or wiring an mpsc pair.
pub async fn step_once<R: CycleRunner>(
    ticket_id: &str,
    admitted: AdmittedTicket,
    cache: Arc<DiffCache>,
    workflow: Arc<WorkflowConfig>,
    _cfg: Arc<RokiConfig>,
    mode: DispatchMode,
    runner: Arc<R>,
    inbox_self: &mpsc::Sender<DispatchMsg>,
    session_root: &std::path::Path,
) -> StepOutcome {
    let snapshot = match cache.snapshot(ticket_id).await {
        Some(s) => s,
        None => return StepOutcome::NoMatch,
    };

    let target = evaluate_from_cache(ticket_id, &snapshot, &workflow, mode);
    let (kind, target) = match target {
        DispatchTarget::NoMatch => return StepOutcome::NoMatch,
        DispatchTarget::CleanupShorthand => (CycleKind::Cleanup, DispatchTarget::CleanupShorthand),
        DispatchTarget::Cycle { kind, rule, cleanup } => {
            (kind, DispatchTarget::Cycle { kind, rule, cleanup })
        }
    };

    let cycle_id = Uuid::new_v4();
    cache.set_cycle_id(ticket_id, cycle_id).await;
    let result = runner.run_cycle(&admitted, target, cycle_id).await;
    cache.clear_cycle_id(ticket_id).await;

    let evicted = match &result {
        CycleResult::Completed { kind: CycleKind::Cleanup, .. } | CycleResult::ShorthandDeleted => {
            cache.evict(ticket_id).await;
            // Per-ticket cleanup events are emitted inside `engine::cleanup::*`
            // when the runner is `RealCycleRunner`; the mock leaves dir state
            // untouched.
            true
        }
        _ => false,
    };

    // Failure-handler dispatch goes here in Task 8's wiring; the mock
    // path returns `Failed` directly so the test exercises the decision
    // tree, not the slice-3 routing.
    let _ = (&result, session_root);

    if !evicted {
        let pending = cache.take_pending_recheck(ticket_id).await;
        if pending {
            // Loop back through the inbox so a Shutdown message can win the
            // race fairly (the inbox is select-fair via tokio mpsc).
            let snapshot_after = cache
                .snapshot(ticket_id)
                .await
                .expect("entry exists pre-eviction");
            let refreshed = synthesize_admitted(ticket_id, &snapshot_after);
            let _ = inbox_self.try_send(DispatchMsg::Webhook(refreshed));
            return StepOutcome::QueuedPending;
        }
    }

    StepOutcome::Dispatched { kind, evicted }
}

fn synthesize_admitted(ticket_id: &str, snap: &crate::daemon::cache::CacheEntry) -> AdmittedTicket {
    AdmittedTicket {
        ticket: crate::linear::ticket::NormalizedTicket::new(
            ticket_id.into(),
            Some(snap.assignee.clone()),
            snap.status.clone(),
            snap.labels.iter().cloned().collect(),
            String::new(),
            String::new(),
        ),
        ghq: snap.repo.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::workflow::{
        AdmissionSection, AdmissionRepo, Cleanup, Rule, WorkflowConfig,
    };
    use crate::engine::outcome::PhaseBody;
    use std::sync::Mutex;
    use tempfile::TempDir;

    struct MockCycleRunner {
        next: Mutex<Vec<CycleResult>>,
        invocations: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl CycleRunner for MockCycleRunner {
        async fn run_cycle(
            &self,
            _a: &AdmittedTicket,
            _t: DispatchTarget<'_>,
            _id: Uuid,
        ) -> CycleResult {
            *self.invocations.lock().unwrap() += 1;
            self.next
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(CycleResult::Completed {
                    kind: CycleKind::Rule,
                    iters: 1,
                })
        }
    }

    fn workflow_with_rule(status: &str) -> WorkflowConfig {
        WorkflowConfig {
            admission: AdmissionSection { assignee: "u1".into() },
            repo: Some(AdmissionRepo {
                ghq: "github.com/example/repo".into(),
            }),
            rules: vec![Rule {
                when_status: status.into(),
                when_labels_has_all: vec![],
                pre: None,
                run: PhaseBody::InlineCmd { cmd: "true".into() },
                post: None,
            }],
            cleanups: vec![],
            on_failures: vec![],
        }
    }

    fn workflow_with_cleanup(status: &str) -> WorkflowConfig {
        WorkflowConfig {
            admission: AdmissionSection { assignee: "u1".into() },
            repo: Some(AdmissionRepo {
                ghq: "github.com/example/repo".into(),
            }),
            rules: vec![],
            cleanups: vec![Cleanup {
                when_status: Some(status.into()),
                when_labels_has_all: vec![],
                pre: None,
                run: Some(PhaseBody::InlineCmd { cmd: "true".into() }),
                post: None,
            }],
            on_failures: vec![],
        }
    }

    fn admitted(id: &str, status: &str) -> AdmittedTicket {
        AdmittedTicket {
            ticket: crate::linear::ticket::NormalizedTicket::new(
                id.into(),
                Some("u1".into()),
                status.into(),
                vec![],
                String::new(),
                String::new(),
            ),
            ghq: "github.com/example/repo".into(),
        }
    }

    fn cfg(session_root: &std::path::Path) -> Arc<RokiConfig> {
        // Use whatever `RokiConfig::test_default` (or the equivalent
        // helper) exists in the codebase; if none exists, build the
        // smallest legal value here. For the ticket_task tests only
        // session_root is read.
        Arc::new(RokiConfig::test_default(session_root))
    }

    #[tokio::test]
    async fn dispatch_on_first_webhook_runs_cycle() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_rule("InProgress"));
        let runner = Arc::new(MockCycleRunner {
            next: Mutex::new(vec![CycleResult::Completed {
                kind: CycleKind::Rule,
                iters: 1,
            }]),
            invocations: Mutex::new(0),
        });

        let a = admitted("t1", "InProgress");
        cache.observe(&a).await;

        let (tx, _rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache.clone(),
            wf,
            cfg(work.path()),
            DispatchMode::Default,
            runner.clone(),
            &tx,
            work.path(),
        )
        .await;

        assert!(matches!(
            outcome,
            StepOutcome::Dispatched { kind: CycleKind::Rule, evicted: false }
        ));
        assert_eq!(*runner.invocations.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn cleanup_completion_evicts_cache_entry() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_cleanup("Done"));
        let runner = Arc::new(MockCycleRunner {
            next: Mutex::new(vec![CycleResult::Completed {
                kind: CycleKind::Cleanup,
                iters: 1,
            }]),
            invocations: Mutex::new(0),
        });

        let a = admitted("t1", "Done");
        cache.observe(&a).await;

        let (tx, _rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache.clone(),
            wf,
            cfg(work.path()),
            DispatchMode::Default,
            runner,
            &tx,
            work.path(),
        )
        .await;

        assert!(matches!(
            outcome,
            StepOutcome::Dispatched { kind: CycleKind::Cleanup, evicted: true }
        ));
        assert!(cache.snapshot("t1").await.is_none());
    }

    #[tokio::test]
    async fn pending_recheck_loops_back_via_inbox() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_rule("InProgress"));
        let runner = Arc::new(MockCycleRunner {
            next: Mutex::new(vec![CycleResult::Completed {
                kind: CycleKind::Rule,
                iters: 1,
            }]),
            invocations: Mutex::new(0),
        });

        let a = admitted("t1", "InProgress");
        cache.observe(&a).await;
        cache.set_pending_recheck("t1").await;

        let (tx, mut rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache.clone(),
            wf,
            cfg(work.path()),
            DispatchMode::Default,
            runner,
            &tx,
            work.path(),
        )
        .await;

        assert!(matches!(outcome, StepOutcome::QueuedPending));
        let queued = rx.try_recv().expect("loop-back msg present");
        assert!(matches!(queued, DispatchMsg::Webhook(_)));
        assert!(!cache.snapshot("t1").await.unwrap().pending_recheck);
    }

    #[tokio::test]
    async fn no_match_returns_no_match() {
        let work = TempDir::new().unwrap();
        let cache = Arc::new(DiffCache::new());
        let wf = Arc::new(workflow_with_rule("InProgress"));
        let runner = Arc::new(MockCycleRunner {
            next: Mutex::new(vec![]),
            invocations: Mutex::new(0),
        });

        let a = admitted("t1", "Triage");
        cache.observe(&a).await;

        let (tx, _rx) = mpsc::channel(1);
        let outcome = step_once(
            "t1",
            a,
            cache,
            wf,
            cfg(work.path()),
            DispatchMode::Default,
            runner.clone(),
            &tx,
            work.path(),
        )
        .await;

        assert_eq!(outcome, StepOutcome::NoMatch);
        assert_eq!(*runner.invocations.lock().unwrap(), 0);
    }
}
```

If `RokiConfig::test_default(path)` does not exist, add it as a `#[cfg(test)] pub` constructor on `RokiConfig` that returns the smallest legal value with `paths.session_root = path.into()`. Mirror the test fixtures already used in slice-1 unit tests.

If `Cleanup`'s `run` field is `Option<PhaseBody>`, the tests above already account for that. Verify against the actual struct in `config::workflow`.

- [ ] **Step 3: Run tests**

```bash
cargo test -p roki-daemon daemon::ticket_task
```

Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/daemon/ticket_task.rs \
        crates/roki-daemon/src/daemon/mod.rs \
        crates/roki-daemon/src/config/roki.rs   # if test_default added
git commit -m "feat(daemon): ticket_task per-ticket actor with mock-runner tests"
```

---

## Task 7: `daemon::dispatcher` — webhook intake → cache → spawn

**Files:**
- Create: `crates/roki-daemon/src/daemon/dispatcher.rs`
- Modify: `crates/roki-daemon/src/daemon/mod.rs`

- [ ] **Step 1: Add module declaration**

```rust
// crates/roki-daemon/src/daemon/mod.rs
pub mod cache;
pub mod dispatcher;
pub mod shutdown;
pub mod ticket_task;
```

- [ ] **Step 2: Write the dispatcher + tests**

```rust
// crates/roki-daemon/src/daemon/dispatcher.rs
#![allow(dead_code)]

//! Webhook intake → admission → cache observe → spawn-or-route.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::admission::{self, AdmittedTicket};
use crate::config::roki::RokiConfig;
use crate::config::workflow::WorkflowConfig;
use crate::daemon::cache::{DiffCache, DiffOutcome};
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::ticket_task::{CycleRunner, DispatchMsg};
use crate::engine::dispatch::DispatchMode;
use crate::events::{Event, EventWriter, WebhookSkipReason, now_rfc3339};
use crate::linear::client::MeId;
use crate::linear::ticket::NormalizedTicket;

pub struct Dispatcher<R: CycleRunner + 'static> {
    cache: Arc<DiffCache>,
    tickets: Arc<Mutex<HashMap<String, TicketHandle>>>,
    workflow: Arc<WorkflowConfig>,
    cfg: Arc<RokiConfig>,
    me: Option<MeId>,
    mode: DispatchMode,
    shutdown: ShutdownToken,
    runner: Arc<R>,
    daemon_events: Arc<Mutex<EventWriter>>,
}

pub struct TicketHandle {
    pub inbox: mpsc::Sender<DispatchMsg>,
    pub join: JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchAction {
    Routed,
    Spawned,
    BackPressureSetPending,
    Skipped(WebhookSkipReason),
    AdmissionRejected,
}

impl<R: CycleRunner + 'static> Dispatcher<R> {
    pub fn new(
        cache: Arc<DiffCache>,
        workflow: Arc<WorkflowConfig>,
        cfg: Arc<RokiConfig>,
        me: Option<MeId>,
        mode: DispatchMode,
        shutdown: ShutdownToken,
        runner: Arc<R>,
        daemon_events: Arc<Mutex<EventWriter>>,
    ) -> Self {
        Self {
            cache,
            tickets: Arc::new(Mutex::new(HashMap::new())),
            workflow,
            cfg,
            me,
            mode,
            shutdown,
            runner,
            daemon_events,
        }
    }

    pub fn tickets(&self) -> Arc<Mutex<HashMap<String, TicketHandle>>> {
        self.tickets.clone()
    }

    /// Drain `rx` until the listener side closes. Each ticket is routed
    /// to its per-ticket task; new tickets cause a fresh task to spawn.
    pub async fn drain(&self, mut rx: mpsc::Receiver<NormalizedTicket>) {
        while let Some(ticket) = rx.recv().await {
            let _ = self.on_webhook(ticket).await;
            if self.shutdown.is_fired() {
                break;
            }
        }
    }

    pub async fn on_webhook(&self, ticket: NormalizedTicket) -> DispatchAction {
        let me_ref = self.me.clone().unwrap_or_else(|| MeId(String::new()));
        let admitted = match admission::accept(&ticket, &self.workflow, &me_ref) {
            Ok(a) => a,
            Err(_) => {
                self.emit_skip(&ticket.id, WebhookSkipReason::AssigneeMismatch).await;
                return DispatchAction::AdmissionRejected;
            }
        };

        let outcome = self.cache.observe(&admitted).await;
        if matches!(outcome, DiffOutcome::Unchanged) {
            self.emit_skip(&admitted.ticket.id, WebhookSkipReason::NoDiff).await;
            return DispatchAction::Skipped(WebhookSkipReason::NoDiff);
        }

        let mut map = self.tickets.lock().await;
        let entry = map.get(&admitted.ticket.id);

        if let Some(handle) = entry {
            match handle.inbox.try_send(DispatchMsg::Webhook(admitted.clone())) {
                Ok(()) => DispatchAction::Routed,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    drop(map);
                    self.cache
                        .set_pending_recheck(&admitted.ticket.id)
                        .await;
                    DispatchAction::BackPressureSetPending
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    map.remove(&admitted.ticket.id);
                    self.spawn_and_route(&mut map, admitted).await;
                    DispatchAction::Spawned
                }
            }
        } else {
            self.spawn_and_route(&mut map, admitted).await;
            DispatchAction::Spawned
        }
    }

    async fn spawn_and_route(
        &self,
        map: &mut HashMap<String, TicketHandle>,
        admitted: AdmittedTicket,
    ) {
        let (tx, rx) = mpsc::channel::<DispatchMsg>(1);
        let tx_self = tx.clone();
        let ticket_id = admitted.ticket.id.clone();
        let cache = self.cache.clone();
        let wf = self.workflow.clone();
        let cfg = self.cfg.clone();
        let mode = self.mode;
        let runner = self.runner.clone();
        let session_root = self.cfg.paths.session_root.clone();

        let join = tokio::spawn(async move {
            crate::daemon::ticket_task::run_ticket_task(
                ticket_id,
                cache,
                wf,
                cfg,
                mode,
                runner,
                rx,
                tx_self,
                session_root,
            )
            .await;
        });

        let _ = tx.try_send(DispatchMsg::Webhook(admitted.clone()));
        map.insert(admitted.ticket.id.clone(), TicketHandle { inbox: tx, join });
    }

    async fn emit_skip(&self, ticket_id: &str, reason: WebhookSkipReason) {
        let _ = self.daemon_events.lock().await.emit(&Event::WebhookSkipped {
            ts: now_rfc3339(),
            ticket_id: ticket_id.to_string(),
            reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::workflow::{AdmissionRepo, AdmissionSection, Rule};
    use crate::daemon::ticket_task::{CycleResult, CycleRunner};
    use crate::engine::outcome::{CycleKind, PhaseBody};
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    struct CountingRunner(Arc<StdMutex<u32>>);

    #[async_trait::async_trait]
    impl CycleRunner for CountingRunner {
        async fn run_cycle(
            &self,
            _a: &AdmittedTicket,
            _t: crate::engine::dispatch::DispatchTarget<'_>,
            _id: uuid::Uuid,
        ) -> CycleResult {
            *self.0.lock().unwrap() += 1;
            // Stay alive until inbox closes by holding the task busy for a moment.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            CycleResult::Completed {
                kind: CycleKind::Rule,
                iters: 1,
            }
        }
    }

    fn workflow() -> Arc<WorkflowConfig> {
        Arc::new(WorkflowConfig {
            admission: AdmissionSection { assignee: "u1".into() },
            repo: Some(AdmissionRepo {
                ghq: "github.com/example/repo".into(),
            }),
            rules: vec![Rule {
                when_status: "InProgress".into(),
                when_labels_has_all: vec![],
                pre: None,
                run: PhaseBody::InlineCmd { cmd: "true".into() },
                post: None,
            }],
            cleanups: vec![],
            on_failures: vec![],
        })
    }

    fn ticket(id: &str, status: &str) -> NormalizedTicket {
        NormalizedTicket::new(
            id.into(),
            Some("u1".into()),
            status.into(),
            vec![],
            String::new(),
            String::new(),
        )
    }

    fn dispatcher_with(
        runner: Arc<CountingRunner>,
        work: &std::path::Path,
    ) -> Dispatcher<CountingRunner> {
        let cfg = Arc::new(RokiConfig::test_default(work));
        let events = Arc::new(Mutex::new(
            EventWriter::open(work, "_daemon").expect("open events"),
        ));
        Dispatcher::new(
            Arc::new(DiffCache::new()),
            workflow(),
            cfg,
            Some(MeId("u1".into())),
            DispatchMode::Default,
            ShutdownToken::new(),
            runner,
            events,
        )
    }

    #[tokio::test]
    async fn first_webhook_spawns_task() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner(count.clone())), work.path());
        let action = d.on_webhook(ticket("t1", "InProgress")).await;
        assert_eq!(action, DispatchAction::Spawned);
        assert!(d.tickets().lock().await.contains_key("t1"));
    }

    #[tokio::test]
    async fn duplicate_unchanged_triple_is_skipped() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner(count.clone())), work.path());
        d.on_webhook(ticket("t1", "InProgress")).await;
        let action = d.on_webhook(ticket("t1", "InProgress")).await;
        assert_eq!(action, DispatchAction::Skipped(WebhookSkipReason::NoDiff));
    }

    #[tokio::test]
    async fn admission_rejection_skips_dispatch() {
        let work = TempDir::new().unwrap();
        let count = Arc::new(StdMutex::new(0u32));
        let d = dispatcher_with(Arc::new(CountingRunner(count.clone())), work.path());
        let mut bad = ticket("t1", "InProgress");
        bad.assignee_id = Some("intruder".into());
        let action = d.on_webhook(bad).await;
        assert_eq!(action, DispatchAction::AdmissionRejected);
        assert!(d.tickets().lock().await.is_empty());
    }
}
```

`NormalizedTicket.assignee_id` is currently the only field the test mutates after construction. If the field is private behind `pub(crate)`, mutate via the existing setter or build a fresh `NormalizedTicket::new` with the bad assignee directly.

- [ ] **Step 3: Run tests**

```bash
cargo test -p roki-daemon daemon::dispatcher
```

Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/daemon/dispatcher.rs \
        crates/roki-daemon/src/daemon/mod.rs
git commit -m "feat(daemon): dispatcher with admission + cache + spawn-or-route"
```

---

## Task 8: `runtime::run` rewire — single-shot → persistent daemon

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`
- Modify: `crates/roki-daemon/src/linear/webhook.rs` (raise channel capacity, drop `cycle_started` atomic)

This is the largest task in the slice. It deletes the single-shot pipeline body and replaces it with a daemon boot. The slice 3 `handle_failed_cycle` / `on_failure_to_rule` / `cleanup_to_rule` functions move into a new `RealCycleRunner` that lives near `daemon::ticket_task` so the failure-handler logic is reused unchanged.

- [ ] **Step 1: Move `handle_failed_cycle` / `on_failure_to_rule` / `cleanup_to_rule` into a `RealCycleRunner`**

Create `crates/roki-daemon/src/daemon/real_runner.rs`:

```rust
//! Production `CycleRunner` impl bridging `daemon::ticket_task` to
//! `engine::cycle::run_cycle` and slice-3's `[[on_failure]]` routing.

use std::sync::Arc;

use uuid::Uuid;

use crate::admission::AdmittedTicket;
use crate::config::roki::RokiConfig;
use crate::config::workflow::{Cleanup, Rule, WorkflowConfig};
use crate::daemon::ticket_task::{CycleResult, CycleRunner};
use crate::engine::CommandPhaseExecutor;
use crate::engine::dispatch::DispatchTarget;
use crate::engine::outcome::{CycleKind, FailureMeta};
use crate::events::{Event, EventWriter, FailureMarker, FailureMetaSer, now_rfc3339};

pub struct RealCycleRunner {
    pub workflow: Arc<WorkflowConfig>,
    pub cfg: Arc<RokiConfig>,
    pub executor: Arc<CommandPhaseExecutor>,
}

#[async_trait::async_trait]
impl CycleRunner for RealCycleRunner {
    async fn run_cycle(
        &self,
        admitted: &AdmittedTicket,
        target: DispatchTarget<'_>,
        _cycle_id: Uuid,
    ) -> CycleResult {
        let mut events =
            match EventWriter::open(&self.cfg.paths.session_root, &admitted.ticket.id) {
                Ok(w) => w,
                Err(_) => {
                    return CycleResult::Failed {
                        meta: FailureMeta::generic_fs_poison(),
                        kind: CycleKind::Rule,
                    };
                }
            };

        let (rule_view, kind) = match target {
            DispatchTarget::Cycle { kind, rule: Some(r), .. } => (r.clone(), kind),
            DispatchTarget::Cycle { kind, cleanup: Some(c), .. } => (cleanup_to_rule(c), kind),
            DispatchTarget::CleanupShorthand => {
                if let Err(_e) = crate::engine::cleanup::delete_immediate(
                    &admitted.ticket.id,
                    &admitted.ghq,
                    &self.cfg.paths.session_root,
                    &mut events,
                )
                .await
                {
                    return CycleResult::Failed {
                        meta: FailureMeta::generic_fs_poison(),
                        kind: CycleKind::Cleanup,
                    };
                }
                return CycleResult::ShorthandDeleted;
            }
            DispatchTarget::Cycle { rule: None, cleanup: None, .. } | DispatchTarget::NoMatch => {
                unreachable!("dispatcher only forwards matched targets")
            }
        };

        let outcome = match crate::engine::run_cycle(
            self.executor.as_ref(),
            admitted,
            &rule_view,
            &self.cfg.paths.session_root,
            self.cfg.as_ref(),
            kind,
            None,
        )
        .await
        {
            Ok(o) => o,
            Err(_e) => {
                return CycleResult::Failed {
                    meta: FailureMeta::generic_fs_poison(),
                    kind,
                };
            }
        };

        match outcome {
            crate::engine::CycleOutcome::Completed { iters, cycle_id } => {
                if kind == CycleKind::Cleanup {
                    let _ = crate::engine::cleanup::post_cycle_delete(
                        &admitted.ticket.id,
                        &admitted.ghq,
                        &self.cfg.paths.session_root,
                        cycle_id,
                        &mut events,
                    )
                    .await;
                }
                CycleResult::Completed { kind, iters }
            }
            crate::engine::CycleOutcome::Failed { meta } => {
                let handler_outcome = handle_failed_cycle(
                    &meta,
                    kind,
                    self.workflow.as_ref(),
                    self.executor.as_ref(),
                    admitted,
                    self.cfg.as_ref(),
                    &mut events,
                )
                .await;
                match handler_outcome {
                    HandlerDecision::Succeeded => {
                        CycleResult::Completed { kind: CycleKind::Failure, iters: 0 }
                    }
                    HandlerDecision::Unhandled => CycleResult::Failed { meta, kind },
                }
            }
        }
    }
}

enum HandlerDecision { Succeeded, Unhandled }

fn cleanup_to_rule(c: &Cleanup) -> Rule {
    Rule {
        when_status: c.when_status.clone().unwrap_or_default(),
        when_labels_has_all: c.when_labels_has_all.clone(),
        pre: c.pre.clone(),
        run: c.run.clone().expect("non-shorthand cleanup has run"),
        post: c.post.clone(),
    }
}

async fn handle_failed_cycle(
    meta: &FailureMeta,
    failed_kind: CycleKind,
    workflow: &WorkflowConfig,
    executor: &CommandPhaseExecutor,
    admitted: &AdmittedTicket,
    cfg: &RokiConfig,
    events: &mut EventWriter,
) -> HandlerDecision {
    // Body copied verbatim from the slice-3 implementation in
    // runtime.rs (which Task 8 deletes from runtime.rs). See the
    // pre-slice-5 `handle_failed_cycle` for the canonical version.
    // Behavior unchanged: recursion bound, [[on_failure]] first-match,
    // FailureMarker::None / RecursionBound emission.
    if failed_kind == CycleKind::Failure {
        let _ = events.emit(&Event::FailureUnhandled {
            ts: now_rfc3339(),
            cycle_id: meta.failed_cycle_id.to_string(),
            cycle_kind: "failure".into(),
            failure: FailureMetaSer::from_meta(meta),
            marker: FailureMarker::RecursionBound,
        });
        return HandlerDecision::Unhandled;
    }

    let Some(handler) = crate::engine::on_failure::route(&workflow.on_failures, meta) else {
        let _ = events.emit(&Event::FailureUnhandled {
            ts: now_rfc3339(),
            cycle_id: meta.failed_cycle_id.to_string(),
            cycle_kind: failed_kind.as_str().to_string(),
            failure: FailureMetaSer::from_meta(meta),
            marker: FailureMarker::None,
        });
        return HandlerDecision::Unhandled;
    };

    let handler_rule = on_failure_to_rule(handler);
    match crate::engine::run_cycle(
        executor,
        admitted,
        &handler_rule,
        &cfg.paths.session_root,
        cfg,
        CycleKind::Failure,
        Some(meta.clone()),
    )
    .await
    {
        Ok(crate::engine::CycleOutcome::Completed { iters, cycle_id }) => {
            let _ = events.emit(&Event::CycleCompleted {
                ts: now_rfc3339(),
                cycle_id: cycle_id.to_string(),
                cycle_kind: "failure".into(),
                iters,
                outcome: None,
            });
            HandlerDecision::Succeeded
        }
        _ => {
            let _ = events.emit(&Event::FailureUnhandled {
                ts: now_rfc3339(),
                cycle_id: meta.failed_cycle_id.to_string(),
                cycle_kind: "failure".into(),
                failure: FailureMetaSer::from_meta(meta),
                marker: FailureMarker::RecursionBound,
            });
            HandlerDecision::Unhandled
        }
    }
}

fn on_failure_to_rule(h: &crate::engine::on_failure::OnFailure) -> Rule {
    Rule {
        when_status: String::new(),
        when_labels_has_all: vec![],
        pre: h.pre.clone(),
        run: h.run.clone(),
        post: h.post.clone(),
    }
}
```

`FailureMeta::generic_fs_poison()` is a small helper added next to `FailureMeta` in `engine::outcome.rs` returning a `Kind::FsPoison` meta with `iter: 0`, `phase: Pre`, `error_text: "boot path"` — used as a fallback when the runner cannot even open the event writer. If `engine::outcome::FailureMeta` already has an equivalent constructor, use that instead.

Add `pub mod real_runner;` to `crates/roki-daemon/src/daemon/mod.rs`.

- [ ] **Step 2: Replace the body of `runtime::run_inner`**

Open `crates/roki-daemon/src/runtime.rs`. Delete the existing single-shot pipeline (the `let (admitted, _cycle_kind, dispatched) = loop { ... }` block, the cycle dispatch match, the failure handler, the listener shutdown coordination — i.e. lines roughly 113-360 in the current file).

Replace with:

```rust
pub(crate) async fn run_inner(config_path: &Path, mode: DispatchMode) -> Result<(), SkeletonError> {
    let cfg = RokiConfig::load(config_path)?;
    let workflow = WorkflowConfig::load(&cfg.paths.workflow)?;

    let me = if workflow.admission.assignee == "me" {
        let client = LinearClient::new(cfg.linear.token.clone());
        Some(client.resolve_viewer().await?)
    } else {
        None
    };

    let cfg = Arc::new(cfg);
    let workflow = Arc::new(workflow);

    // Daemon-scoped event log (slice 5). Reuses `EventWriter::open` with
    // ticket id `_daemon` → file `<session_root>/_daemon.events.jsonl`.
    let daemon_events = Arc::new(tokio::sync::Mutex::new(
        crate::events::EventWriter::open(&cfg.paths.session_root, "_daemon").map_err(|e| {
            SkeletonError::Capture(crate::error::CaptureError::OpenFile {
                path: crate::events::events_path(&cfg.paths.session_root, "_daemon"),
                source: e,
            })
        })?,
    ));

    let _ = daemon_events.lock().await.emit(&Event::DaemonStarted {
        ts: now_rfc3339(),
        config_path: config_path.display().to_string(),
        schema_version: 1,
    });

    // Webhook channel: capacity raised from 1 (slice 1-4 first-cycle lock)
    // to 64 so the listener never back-pressures while the dispatcher is
    // running a single cycle.
    let (tx, rx) = mpsc::channel::<NormalizedTicket>(64);

    let bind_ip = IpAddr::from_str(&cfg.linear_webhook.bind).map_err(|err| {
        SkeletonError::Webhook(WebhookError::BindFailed {
            addr: cfg.linear_webhook.bind.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, err),
        })
    })?;
    let addr = SocketAddr::from((bind_ip, cfg.linear_webhook.port));

    let state = WebhookState { sender: Arc::new(tx) };

    let shutdown = ShutdownToken::new();
    let listener_shutdown = shutdown.clone();
    let listener_handle = tokio::spawn(webhook::bind_and_serve(addr, state, async move {
        listener_shutdown.wait().await;
    }));

    let cache = Arc::new(DiffCache::new());
    let executor = Arc::new(crate::engine::CommandPhaseExecutor {
        default_cli: cfg.default_ai_command.cli.clone(),
        stall: crate::engine::phase::StallWindow::CommandDefault(
            cfg.default_ai_command.stall_seconds,
        ),
    });
    let runner = Arc::new(crate::daemon::real_runner::RealCycleRunner {
        workflow: workflow.clone(),
        cfg: cfg.clone(),
        executor,
    });

    let dispatcher = Arc::new(crate::daemon::dispatcher::Dispatcher::new(
        cache.clone(),
        workflow.clone(),
        cfg.clone(),
        me,
        mode,
        shutdown.clone(),
        runner,
        daemon_events.clone(),
    ));

    let _ = daemon_events.lock().await.emit(&Event::DaemonReady {
        ts: now_rfc3339(),
        webhook_bind_addr: addr.to_string(),
    });

    let dispatcher_drain = dispatcher.clone();
    let drain_handle = tokio::spawn(async move {
        dispatcher_drain.drain(rx).await;
    });

    // Trap SIGINT and SIGTERM. First signal flips the token; second
    // signal aborts immediately.
    let signal_shutdown = shutdown.clone();
    let signal_events = daemon_events.clone();
    let signal_cache = cache.clone();
    tokio::spawn(async move {
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("install SIGINT");
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM");
        let signal = tokio::select! {
            _ = sigint.recv() => crate::events::ShutdownSignal::Sigint,
            _ = sigterm.recv() => crate::events::ShutdownSignal::Sigterm,
        };
        let in_flight = signal_cache.in_flight_count().await;
        let _ = signal_events.lock().await.emit(&Event::DaemonShutdownBegan {
            ts: now_rfc3339(),
            signal,
            in_flight,
        });
        signal_shutdown.fire();
    });

    // Block until shutdown fires.
    shutdown.wait().await;

    // Stop the listener and dispatcher drain.
    let _ = drain_handle.await;
    let _ = listener_handle.await;

    // Drain in-flight ticket tasks within the configured window.
    let window = std::time::Duration::from_secs(cfg.engine.shutdown_window_seconds.into());
    let outcome = drain_tickets(dispatcher.tickets(), window).await;

    let _ = daemon_events.lock().await.emit(&Event::DaemonShutdownCompleted {
        ts: now_rfc3339(),
        drained: outcome.drained,
        aborted: outcome.aborted_ids.len(),
    });

    if !outcome.aborted_ids.is_empty() {
        let _ = daemon_events.lock().await.emit(&Event::ShutdownWindowExceeded {
            ts: now_rfc3339(),
            aborted: outcome.aborted_ids.len(),
            aborted_ticket_ids: outcome.aborted_ids,
        });
        return Err(SkeletonError::ShutdownWindowExceeded);
    }

    Ok(())
}

#[derive(Debug)]
struct DrainOutcome {
    drained: usize,
    aborted_ids: Vec<String>,
}

async fn drain_tickets(
    registry: Arc<tokio::sync::Mutex<std::collections::HashMap<String, crate::daemon::dispatcher::TicketHandle>>>,
    window: std::time::Duration,
) -> DrainOutcome {
    use crate::daemon::ticket_task::DispatchMsg;

    let mut handles = Vec::new();
    {
        let mut map = registry.lock().await;
        for (ticket_id, handle) in map.drain() {
            let _ = handle.inbox.send(DispatchMsg::Shutdown).await;
            drop(handle.inbox);
            handles.push((ticket_id, handle.join));
        }
    }

    let mut drained = 0usize;
    let mut aborted_ids = Vec::new();
    let deadline = tokio::time::Instant::now() + window;

    for (ticket_id, join) in handles {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, join).await {
            Ok(Ok(())) => drained += 1,
            Ok(Err(_join_err)) => aborted_ids.push(ticket_id),
            Err(_) => aborted_ids.push(ticket_id),
        }
    }

    DrainOutcome { drained, aborted_ids }
}
```

- [ ] **Step 3: Add `SkeletonError::ShutdownWindowExceeded` if not present**

In `crates/roki-daemon/src/error.rs`, extend the `SkeletonError` enum:

```rust
#[error("shutdown window exceeded; aborted in-flight ticket tasks")]
ShutdownWindowExceeded,
```

- [ ] **Step 4: Rip the `cycle_started` atomic out of `webhook::WebhookState`**

In `crates/roki-daemon/src/linear/webhook.rs`, remove the `cycle_started: Arc<AtomicBool>` field and any logic that reads it (the slice 1-4 listener used it as a "first-cycle lock" so subsequent webhooks bounced; the dispatcher now owns the cycle state through the cache).

Update `WebhookState` to only carry `sender: Arc<mpsc::Sender<NormalizedTicket>>`. Adjust the request handler to always forward an accepted ticket via `sender.send(...)` (or `try_send` with a 503 on Full to back-pressure).

- [ ] **Step 5: Build + run the unit tests**

```bash
cargo build -p roki-daemon
cargo test -p roki-daemon --lib
```

Expected: build clean. Lib tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs \
        crates/roki-daemon/src/linear/webhook.rs \
        crates/roki-daemon/src/error.rs \
        crates/roki-daemon/src/daemon/real_runner.rs \
        crates/roki-daemon/src/daemon/mod.rs
git commit -m "feat(runtime): persistent daemon loop with cache + dispatcher + drain"
```

---

## Task 9: E2E — persistent across two cycles, same ticket

**Files:**
- Create: `crates/roki-daemon/tests/e2e/support/persistent.rs`
- Modify: `crates/roki-daemon/tests/e2e/support/mod.rs` (add `pub mod persistent;`)
- Create: `crates/roki-daemon/tests/e2e/persistent_two_cycles_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml` (add `[[test]]` entry)

- [ ] **Step 1: Add the e2e helper**

```rust
// crates/roki-daemon/tests/e2e/support/persistent.rs

use std::path::Path;
use std::time::Duration;

use tokio::process::Child;
use tokio::time::{Instant, sleep};

/// Wait until `<session_root>/<ticket-id>.events.jsonl` contains a line
/// whose `event` field equals `event_kind`. Returns the matching JSON
/// value. Panics on timeout.
pub async fn await_event(
    session_root: &Path,
    ticket_id: &str,
    event_kind: &str,
    timeout: Duration,
) -> serde_json::Value {
    let path = session_root.join(format!("{}.events.jsonl", ticket_id));
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(body) = tokio::fs::read_to_string(&path).await {
            for line in body.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if v["event"] == event_kind {
                        return v;
                    }
                }
            }
        }
        sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "timed out waiting for event_kind={event_kind} in {}",
        path.display()
    );
}

pub async fn await_daemon_event(
    session_root: &Path,
    event_kind: &str,
    timeout: Duration,
) -> serde_json::Value {
    await_event(session_root, "_daemon", event_kind, timeout).await
}

/// Send SIGTERM to the daemon, wait up to `timeout` for it to exit, and
/// return the exit code.
pub async fn sigterm_and_wait(child: &mut Child, timeout: Duration) -> Option<i32> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status.code(),
        _ => {
            let _ = child.kill().await;
            None
        }
    }
}
```

Wire into `tests/e2e/support/mod.rs`:

```rust
pub mod persistent;
```

- [ ] **Step 2: Write the e2e**

```rust
// crates/roki-daemon/tests/e2e/persistent_two_cycles_smoke.rs
//! Persistent daemon: webhook A → cycle 1 → webhook B mid-cycle →
//! pending_recheck → cycle 1 ends → cycle 2 dispatches against latest
//! cache snapshot. Single binary instance; SIGTERM after second
//! `cycle_completed`.

mod support {
    pub use crate::support::*;
}

use std::time::Duration;
// Mirror cleanup_cycle_smoke.rs setup verbatim except the rule fixture
// matches *both* `Todo` and `InProgress` so each webhook lands a cycle.

#[tokio::test]
async fn persistent_runs_two_cycles_for_same_ticket() {
    // 1. Bind a port; spin up wiremock Linear; build session_root + wt_root.
    //    (Copy lines 14-35 of cleanup_cycle_smoke.rs.)
    // 2. Workflow: two `[[rule]]` entries. First matches `when.status =
    //    "Todo"` with `[rule.run] cmd = "echo cycle1"` and a post that
    //    returns `directive: "end"`. Second matches `when.status =
    //    "InProgress"` similarly.
    // 3. roki.toml: paths + linear webhook + `[engine] max_iterations = 5`
    //    `shutdown_window_seconds = 5`.
    // 4. Spawn the binary.
    // 5. wait_for_listener.
    // 6. POST webhook A (status=Todo).
    // 7. await_event(session_root, "ENG-100", "cycle_completed", 10s).
    // 8. POST webhook B (status=InProgress).
    // 9. Assert a *second* `cycle_completed` line appears.
    // 10. sigterm_and_wait; assert exit code 0.

    // Implementation: copy the bind / wiremock / fixture-write blocks
    // from `cleanup_cycle_smoke.rs::cleanup_cycle_runs_then_deletes`.
    // Replace the post-webhook block with the two-webhook pattern above.
    // Use `support::persistent::await_event` and `sigterm_and_wait`.
}
```

Then write the actual body. The test harness from `cleanup_cycle_smoke.rs` is the canonical scaffold; copy its `bind`, `MockServer`, `TempDir`, `WORKFLOW.toml`, `roki.toml`, and `Command::new(binary)` blocks. The only behavioral changes are:

  - The workflow has two rule entries (one per status).
  - Two webhooks are POSTed.
  - The wait pattern uses `await_event` for `cycle_completed` twice (count event lines, not just first occurrence).
  - The daemon stays alive between webhooks; SIGTERM at the end.

- [ ] **Step 3: Add `[[test]]` entry**

In `crates/roki-daemon/Cargo.toml`:

```toml
[[test]]
name = "persistent_two_cycles_smoke"
path = "tests/e2e/persistent_two_cycles_smoke.rs"
```

- [ ] **Step 4: Run**

```bash
cargo test -p roki-daemon --test persistent_two_cycles_smoke
```

Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/tests/e2e/support/persistent.rs \
        crates/roki-daemon/tests/e2e/support/mod.rs \
        crates/roki-daemon/tests/e2e/persistent_two_cycles_smoke.rs \
        crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): persistent daemon runs two cycles for same ticket"
```

---

## Task 10: E2E — cross-ticket parallel

**Files:**
- Create: `crates/roki-daemon/tests/e2e/persistent_parallel_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Write the test**

```rust
// crates/roki-daemon/tests/e2e/persistent_parallel_smoke.rs
//! Two distinct admitted tickets cycle concurrently. The run-phase cmd
//! sleeps 500 ms; both `cycle_started` events must precede both
//! `cycle_completed` events in wall-clock order — proving the cycles
//! ran concurrently and not serially.

mod support { pub use crate::support::*; }

use std::time::Duration;

#[tokio::test]
async fn cross_ticket_cycles_overlap() {
    // Setup: same as persistent_two_cycles_smoke but the rule's run cmd
    // is `sh -c "sleep 0.5 && echo done"`.
    // POST webhook A for ENG-100 and webhook B for ENG-200 within ms.
    // Read both per-ticket events.jsonl files, parse `cycle_started`
    // and `cycle_completed` timestamps, assert:
    //   max(started_a, started_b) < min(completed_a, completed_b)
    // SIGTERM, expect exit 0.
}
```

Body details:

  - Workflow rule body uses `cmd = "sh -c \"sleep 0.5 && printf '{\\\"directive\\\":\\\"end\\\"}'\""` (or whatever the slice 1-4 fixtures use to emit a terminal directive after a small delay).
  - POST both webhooks back-to-back without awaiting between them.
  - Parse RFC3339 `ts` strings via `time::OffsetDateTime::parse(.., &Rfc3339)`.

- [ ] **Step 2: Add `[[test]]` + run + commit**

Same shape as Task 9.

```bash
git add crates/roki-daemon/tests/e2e/persistent_parallel_smoke.rs \
        crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): cross-ticket cycles run concurrently"
```

---

## Task 11: E2E — cleanup eviction + re-admit

**Files:**
- Create: `crates/roki-daemon/tests/e2e/persistent_cleanup_evict_readmit_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Write the test**

```rust
//! Cleanup cycle for ENG-100 → cache evicted → second webhook for the
//! same ENG-100 spawns a *fresh* ticket task and runs a cycle. Verified
//! via two `cycle_started` events with the cleanup `worktree_deleted`
//! between them.

mod support { pub use crate::support::*; }

#[tokio::test]
async fn cleanup_evicts_then_readmit_spawns_fresh_task() {
    // Setup mirrors cleanup_cycle_smoke + a [[rule]] entry that matches
    // `when.status = "InProgress"`.
    // 1. POST webhook A (status=done) → cleanup cycle runs; events file
    //    shows `worktree_delete_requested` and `cycle_completed`.
    // 2. POST webhook B (status=InProgress) → second cycle starts; the
    //    events file shows a SECOND `cycle_started` line strictly after
    //    the cleanup completion.
    // 3. SIGTERM.
}
```

Per-ticket events.jsonl is sibling, surviving cleanup-cycle deletion (per `events.rs` doc comment line 4).

- [ ] **Step 2-3: Add `[[test]]` + run + commit**

```bash
git add crates/roki-daemon/tests/e2e/persistent_cleanup_evict_readmit_smoke.rs \
        crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): cleanup eviction then re-admit spawns fresh ticket task"
```

---

## Task 12: E2E — SIGINT graceful drain

**Files:**
- Create: `crates/roki-daemon/tests/e2e/persistent_sigint_drain_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Write the test**

```rust
//! In-flight cycle + SIGINT → daemon drains within window → exit 0.

mod support { pub use crate::support::*; }

use std::time::Duration;
use support::persistent::*;

#[tokio::test]
async fn sigint_drains_in_flight_cycle_within_window() {
    // Setup: rule run cmd sleeps 200 ms then emits terminal directive.
    // shutdown_window_seconds = 5.
    // 1. POST webhook (status=InProgress).
    // 2. await_event for `cycle_started`.
    // 3. SIGINT immediately (cycle still running because the sleep is 200 ms).
    // 4. await_daemon_event for `daemon_shutdown_began`.
    // 5. Wait for child exit; assert exit code 0.
    // 6. Assert daemon events file contains `daemon_shutdown_completed
    //    {aborted: 0}` and NOT `shutdown_window_exceeded`.
}
```

- [ ] **Step 2-3: Add `[[test]]` + run + commit**

```bash
git add crates/roki-daemon/tests/e2e/persistent_sigint_drain_smoke.rs \
        crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): SIGINT drains in-flight cycle within window"
```

---

## Task 13: E2E — SIGINT timeout (`shutdown_window_exceeded`)

**Files:**
- Create: `crates/roki-daemon/tests/e2e/persistent_sigint_timeout_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Write the test**

```rust
//! Long-running cycle (sleep 30s) + SIGINT with shutdown_window_seconds=1
//! → window exceeded → `shutdown_window_exceeded` event → exit 1.

mod support { pub use crate::support::*; }

use std::time::Duration;
use support::persistent::*;

#[tokio::test]
async fn sigint_with_long_cycle_exceeds_window() {
    // Setup: rule run cmd is `sleep 30`; no terminal directive will be
    // observed within the test horizon.
    // shutdown_window_seconds = 1.
    // 1. POST webhook (status=InProgress).
    // 2. await_event for `cycle_started`.
    // 3. SIGINT.
    // 4. Wait for child exit (≤ 5s); assert exit code 1.
    // 5. Assert `_daemon.events.jsonl` contains
    //    `shutdown_window_exceeded` with `aborted >= 1` and at least
    //    one entry in `aborted_ticket_ids`.
}
```

- [ ] **Step 2-3: Add `[[test]]` + run + commit**

```bash
git add crates/roki-daemon/tests/e2e/persistent_sigint_timeout_smoke.rs \
        crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): SIGINT with long cycle emits shutdown_window_exceeded"
```

---

## Task 14: E2E — duplicate webhook with unchanged triple is no-op

**Files:**
- Create: `crates/roki-daemon/tests/e2e/persistent_no_diff_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Write the test**

```rust
//! Two identical webhook payloads → second is `webhook_skipped reason=no_diff`
//! and does NOT emit a second `cycle_started`.

mod support { pub use crate::support::*; }

#[tokio::test]
async fn duplicate_webhook_with_unchanged_triple_is_no_op() {
    // Setup: rule matches status=InProgress.
    // 1. POST webhook A.
    // 2. await `cycle_completed` for ENG-100.
    // 3. POST webhook A again (identical payload).
    // 4. await `webhook_skipped reason=no_diff` in `_daemon.events.jsonl`.
    // 5. Assert ENG-100 events.jsonl has exactly one `cycle_started`.
    // 6. SIGTERM, exit 0.
}
```

- [ ] **Step 2-3: Add `[[test]]` + run + commit**

```bash
git add crates/roki-daemon/tests/e2e/persistent_no_diff_smoke.rs \
        crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): duplicate webhook with unchanged triple is webhook_skipped no_diff"
```

---

## Task 15: Slice 1-4 e2e backwards compat sweep

**Files:**
- Modify: every `crates/roki-daemon/tests/e2e/*.rs` file that calls `child.wait().await` and expects exit on its own

The slice 1-4 e2e tests assume the binary exits after one cycle. Slice 5 keeps it alive. Replace the `child.wait().await` lines with `support::persistent::sigterm_and_wait(&mut child, Duration::from_secs(5))`.

- [ ] **Step 1: List affected files**

```bash
grep -lE 'child\.wait\(\)\.await' crates/roki-daemon/tests/e2e/*.rs
```

- [ ] **Step 2: For each file, replace the wait pattern**

Find lines like:

```rust
let status = child.wait().await.expect("daemon exit");
assert_eq!(status.code(), Some(0));
```

Replace with:

```rust
use support::persistent::sigterm_and_wait;
// ... after the test's terminal assertion (e.g. event line observed):
let exit = sigterm_and_wait(&mut child, Duration::from_secs(5)).await;
assert_eq!(exit, Some(0));
```

For tests that explicitly expect exit code 1 (slice 3 `failure_unhandled_smoke`, slice 4 `worktree_cleanup_fs_error_smoke`, etc.), the failure path now keeps the daemon alive in non-failure tickets. Adjust expectations: the *binary* exits 1 only when a failure cycle is unhandled AND there are no other in-flight ticket tasks AND drain completes. Slice 5 changes this contract — the failure path now lives inside the ticket task and keeps the daemon alive. For these tests, expect:

  - The `failure_unhandled` event still appears in the per-ticket events.jsonl.
  - SIGTERM still exits cleanly (`Some(0)`) because the daemon's exit code reflects shutdown drain success, not per-ticket failure.

If a test was specifically asserting `exit code 1` on an unhandled cycle failure, change it to assert the `failure_unhandled` event line and SIGTERM-clean-exit.

- [ ] **Step 3: Run the full e2e suite**

```bash
cargo test -p roki-daemon
```

Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/tests/e2e
git commit -m "test(e2e): slice 1-4 fixtures use SIGTERM exit with persistent daemon"
```

---

## Task 16: Cross-task self-review

After all tasks land, run the full validation pass.

- [ ] **Step 1: `cargo build -p roki-daemon`** — clean build, no warnings.
- [ ] **Step 2: `cargo clippy -p roki-daemon -- -D warnings`** — clean.
- [ ] **Step 3: `cargo fmt --check`** — clean.
- [ ] **Step 4: `cargo test -p roki-daemon`** — all unit + e2e pass.
- [ ] **Step 5: Validate doc graph**

```bash
kusara validate
```

Expected: clean. The new spec doc and plan doc carry no `refs:` (per `docs/superpowers/specs/` convention; this is intentional and matches slice 1-4).

- [ ] **Step 6: Push the branch**

```bash
git push -u origin slice5-persistent-daemon-spec
```

- [ ] **Step 7: Open PR** describing slice 5 with the spec link in the body. The PR template (if any) lives in `.github/pull_request_template.md`.

---

## Notes for the executor

- **`StallWindow` shape may differ:** slice 1-4 wired the executor with `StallWindow::CommandDefault(seconds)`. Verify the variant name / argument by reading `crates/roki-daemon/src/engine/phase.rs` before Task 8. If different, mirror what `runtime::run_inner` currently does on `main`.
- **`Cleanup::run` is `Option<PhaseBody>`** in slice 3; the `cleanup_to_rule` helper in `daemon::real_runner` handles only the non-shorthand path (the dispatcher returns `CleanupShorthand` for the shorthand case before reaching the runner).
- **`mpsc::Sender::send` vs `try_send`** in the listener: when slice 5 raises capacity to 64, the listener's POST handler can stay with `send().await` for back-pressure semantics, or use `try_send` and respond 503 on Full. Pick whichever matches slice 1-4's existing handler shape.
- **`tokio::signal::unix` is Unix-only.** Slice 5 is Unix-only per `fr:12 §Boundaries` ("Windows support is out of scope"). No `cfg(target_os)` gating needed; the dependency line in `Cargo.toml` already enables `tokio` features that include `signal`.
