# Slice 7 Escalation Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an in-memory bounded escalation queue per `fr:06 §Escalation queue`. Reroute daemon-stuck failures (recursive failure cycles, cleanup-time fs errors, cold-start orphan-reconcile fs errors) from `failure_unhandled` to a new `escalation_added` event. Narrow `failure_unhandled` to `marker = none` only. Drop the spec drift "Daemon exits 1" wording from `fr:08` and `ref:log-events`.

**Architecture:** New `escalation` module owns `EscalationQueue` (a bounded `VecDeque` ring with newest-wins overflow). Queue is constructed in `runtime::run_inner`, cloned into `Dispatcher`, `RealCycleRunner`, `ColdStart`, and `engine::cleanup::*`. Push call sites move from `Event::FailureUnhandled` to `EscalationQueue::push_cycle` / `push_daemon`, which emit `Event::EscalationAdded` to the daemon-scoped event log. Eviction is wired at four sites (cleanup-cycle terminal, shorthand-cleanup, admission-revoke, orphan-reconcile delete) so cycle-bound entries clear automatically. Cycle-less entries persist until daemon restart.

**Tech Stack:** Rust 2024 (workspace edition), `tokio` async runtime, slice 1-6 deps. No new crates.

**Spec:** `docs/superpowers/specs/2026-05-09-slice7-escalation-queue-design.md`.

**Working branch:** `slice7-escalation-queue-spec` (already created; spec committed there). All implementation commits land on this branch.

---

## File Structure

### Created

| Path | Responsibility |
|---|---|
| `crates/roki-daemon/src/escalation/mod.rs` | Module root. Re-exports `EscalationQueue`, `EscalationEntry`. |
| `crates/roki-daemon/src/escalation/entry.rs` | `EscalationEntry` struct. |
| `crates/roki-daemon/src/escalation/queue.rs` | `EscalationQueue` (Mutex<Ring> + daemon writer Arc). Push, evict, snapshot. |
| `crates/roki-daemon/src/escalation/ring.rs` | `Ring<T>` bounded VecDeque with overflow drop signal. |
| `crates/roki-daemon/tests/e2e/escalation_recursion_smoke.rs` | Replaces `recursion_bound_smoke.rs`. Asserts queue + `escalation_added`. |
| `crates/roki-daemon/tests/e2e/escalation_cleanup_fs_error_smoke.rs` | Replaces `worktree_cleanup_fs_error_smoke.rs`. |
| `crates/roki-daemon/tests/e2e/escalation_orphan_reconcile_smoke.rs` | Cold-start orphan fs error → cycle-less queue entry. |
| `crates/roki-daemon/tests/e2e/escalation_evicted_on_cleanup_smoke.rs` | Cycle-bound entry cleared after cleanup-cycle terminal. |
| `crates/roki-daemon/tests/e2e/escalation_capacity_overflow_smoke.rs` | Overflow drops oldest with warn log. |

### Modified

| Path | Change |
|---|---|
| `crates/roki-daemon/src/lib.rs` | `pub mod escalation;`. |
| `crates/roki-daemon/src/events.rs` | Add `Event::EscalationAdded`. Remove `FailureMarker::RecursionBound` and `::CleanupFsError` (Task 11). |
| `crates/roki-daemon/src/config/roki.rs` | `EscalationSection { queue_size: u32 }`; validation `1..=1024`. |
| `crates/roki-daemon/src/runtime.rs` | Construct `EscalationQueue` after daemon-events writer; pass into `Dispatcher`, `RealCycleRunner`, `ColdStart`. |
| `crates/roki-daemon/src/daemon/dispatcher.rs` | Hold `Arc<EscalationQueue>`; call `evict_ticket` on admission-revoke evict path. |
| `crates/roki-daemon/src/daemon/ticket_task.rs` | Receive queue handle; call `evict_ticket` on `Cleanup`-terminal and `ShorthandDeleted`. |
| `crates/roki-daemon/src/daemon/real_runner.rs` | Recursion paths push to queue (replaces `Event::FailureUnhandled` emits at lines 189, 235, 246). |
| `crates/roki-daemon/src/daemon/cold_start.rs` | Pass queue to `orphan::reconcile`; cycle-less push for fs errors. |
| `crates/roki-daemon/src/daemon/orphan.rs` | Accept queue handle; push cycle-less entry per fs error. |
| `crates/roki-daemon/src/engine/cleanup.rs` | `delete_immediate` / `post_cycle_delete` / `emit_wt_remove_error` accept `&EscalationQueue`; push `escalation_added` instead of `failure_unhandled`. New `CycleResult::CleanupFsError` variant in `daemon::ticket_task`. |
| `crates/roki-daemon/tests/e2e/recursion_bound_smoke.rs` | Deleted (replaced). |
| `crates/roki-daemon/tests/e2e/worktree_cleanup_fs_error_smoke.rs` | Deleted (replaced). |
| `crates/roki-daemon/tests/e2e/failure_unhandled_smoke.rs` | Add assertion: zero `escalation_added` events. |
| `crates/roki-daemon/tests/e2e/on_failure_smoke.rs` | Add assertion: zero `escalation_added` events. |
| `docs/fr/08-observability-logs.md` | Replace `failure_unhandled` row per spec §6.1. |
| `docs/fr/02-configuration.md` | Add `[escalation]` section to TOML example. |
| `docs/reference/log-events.md` | Replace `failure_unhandled` and `escalation_added` rows per spec §6.1, §6.2. |
| `docs/reference/config.md` | Add `[escalation].queue_size` row. |

---

## Task 0: Confirm spec + branch already in place

**Files:**
- Read: `docs/superpowers/specs/2026-05-09-slice7-escalation-queue-design.md`

- [ ] **Step 1: Verify branch and spec**

```bash
git rev-parse --abbrev-ref HEAD
test -f docs/superpowers/specs/2026-05-09-slice7-escalation-queue-design.md && echo OK
```

Expected:
```
slice7-escalation-queue-spec
OK
```

If on `main`, run `git checkout slice7-escalation-queue-spec`. If the spec file is missing, abort and re-run `/superpowers:brainstorming`.

---

## Task 1: `escalation::ring::Ring<T>` bounded VecDeque

**Files:**
- Create: `crates/roki-daemon/src/escalation/ring.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/roki-daemon/src/escalation/ring.rs`:

```rust
//! Bounded VecDeque ring with newest-wins overflow.
//!
//! Used by `EscalationQueue`. Capacity is fixed at construction. On push
//! beyond capacity the oldest element is dropped and `PushOutcome::Overflowed`
//! is returned so the caller can emit a warn-severity log.

use std::collections::VecDeque;

#[derive(Debug)]
pub struct Ring<T> {
    buf: VecDeque<T>,
    capacity: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome<T> {
    Inserted,
    Overflowed { dropped: T },
}

impl<T> Ring<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Ring capacity must be > 0");
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, item: T) -> PushOutcome<T> {
        if self.buf.len() == self.capacity {
            let dropped = self.buf.pop_front().expect("len == capacity > 0");
            self.buf.push_back(item);
            PushOutcome::Overflowed { dropped }
        } else {
            self.buf.push_back(item);
            PushOutcome::Inserted
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.buf.iter()
    }

    pub fn retain<F: FnMut(&T) -> bool>(&mut self, mut f: F) {
        self.buf.retain(|t| f(t));
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_within_capacity_inserts() {
        let mut r = Ring::new(3);
        assert_eq!(r.push(1), PushOutcome::Inserted);
        assert_eq!(r.push(2), PushOutcome::Inserted);
        assert_eq!(r.push(3), PushOutcome::Inserted);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn push_at_capacity_drops_oldest() {
        let mut r = Ring::new(2);
        r.push(1);
        r.push(2);
        assert_eq!(r.push(3), PushOutcome::Overflowed { dropped: 1 });
        let snap: Vec<_> = r.iter().copied().collect();
        assert_eq!(snap, vec![2, 3]);
    }

    #[test]
    fn iter_yields_oldest_first() {
        let mut r = Ring::new(4);
        r.push("a");
        r.push("b");
        r.push("c");
        let snap: Vec<_> = r.iter().copied().collect();
        assert_eq!(snap, vec!["a", "b", "c"]);
    }

    #[test]
    fn retain_drops_matching_entries() {
        let mut r = Ring::new(4);
        r.push(1);
        r.push(2);
        r.push(3);
        r.retain(|n| *n != 2);
        let snap: Vec<_> = r.iter().copied().collect();
        assert_eq!(snap, vec![1, 3]);
    }

    #[test]
    #[should_panic]
    fn zero_capacity_panics() {
        let _: Ring<i32> = Ring::new(0);
    }
}
```

- [ ] **Step 2: Add module to lib.rs**

Edit `crates/roki-daemon/src/lib.rs`. Add `pub mod escalation;` near the other top-level module declarations (alphabetical with `engine`, `events`, `linear`).

Create `crates/roki-daemon/src/escalation/mod.rs` as a temporary stub so the crate compiles:

```rust
//! Escalation queue (fr:06 §Escalation queue). In-memory bounded ring of
//! daemon-stuck failures. See `docs/superpowers/specs/2026-05-09-slice7-
//! escalation-queue-design.md`.

pub mod ring;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p roki-daemon --lib escalation::ring`
Expected: 5 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/lib.rs \
        crates/roki-daemon/src/escalation/mod.rs \
        crates/roki-daemon/src/escalation/ring.rs
git commit -m "feat(escalation): bounded ring with newest-wins overflow"
```

---

## Task 2: `EscalationEntry` struct

**Files:**
- Create: `crates/roki-daemon/src/escalation/entry.rs`

- [ ] **Step 1: Write `EscalationEntry`**

```rust
//! In-memory escalation queue entry (fr:06 §Escalation queue).
//!
//! Cycle-bound entries (`failure-handler cycle that itself failed`,
//! `cleanup-time fs error`) carry concrete `ticket_id`, `cycle_id`, `phase`.
//! Cycle-less entries (`daemon-internal error with no cycle association`,
//! e.g. cold-start orphan reconcile fs error) leave all three as `None`.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::engine::outcome::{FailureKind, PhaseKind};

#[derive(Debug, Clone)]
pub struct EscalationEntry {
    pub ticket_id: Option<String>,
    pub cycle_id: Option<Uuid>,
    pub failure_kind: FailureKind,
    pub phase: Option<PhaseKind>,
    pub timestamp: OffsetDateTime,
    pub error_text: String,
}

impl EscalationEntry {
    pub fn cycle(
        ticket_id: String,
        cycle_id: Uuid,
        failure_kind: FailureKind,
        phase: PhaseKind,
        error_text: String,
    ) -> Self {
        Self {
            ticket_id: Some(ticket_id),
            cycle_id: Some(cycle_id),
            failure_kind,
            phase: Some(phase),
            timestamp: OffsetDateTime::now_utc(),
            error_text: sanitize(&error_text),
        }
    }

    pub fn daemon(failure_kind: FailureKind, error_text: String) -> Self {
        Self {
            ticket_id: None,
            cycle_id: None,
            failure_kind,
            phase: None,
            timestamp: OffsetDateTime::now_utc(),
            error_text: sanitize(&error_text),
        }
    }
}

/// Strip ASCII control characters except tab and newline; replace invalid
/// UTF-8 with U+FFFD (already enforced by `String`). The HTTP API and TUI
/// apply ANSI strip + HTML escape on read; sanitize here only enforces the
/// invariant that `error_text` does not break the JSONL writer.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\t' || *c == '\n' || !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_ansi_csi_and_keeps_tabs() {
        let raw = "before\x1b[31mred\x1b[0m\tafter\nline2";
        let s = sanitize(raw);
        assert!(!s.contains('\x1b'), "ANSI ESC must be stripped");
        assert!(s.contains('\t'));
        assert!(s.contains('\n'));
        assert!(s.contains("red"));
    }

    #[test]
    fn cycle_constructor_sets_all_fields() {
        let id = Uuid::new_v4();
        let e = EscalationEntry::cycle(
            "TEAM-1".to_string(),
            id,
            FailureKind::FsPoison,
            PhaseKind::Post,
            "msg".to_string(),
        );
        assert_eq!(e.ticket_id.as_deref(), Some("TEAM-1"));
        assert_eq!(e.cycle_id, Some(id));
        assert_eq!(e.phase, Some(PhaseKind::Post));
        assert_eq!(e.failure_kind, FailureKind::FsPoison);
    }

    #[test]
    fn daemon_constructor_leaves_cycle_fields_none() {
        let e = EscalationEntry::daemon(FailureKind::FsPoison, "boom".to_string());
        assert!(e.ticket_id.is_none());
        assert!(e.cycle_id.is_none());
        assert!(e.phase.is_none());
    }
}
```

- [ ] **Step 2: Re-export from `escalation::mod`**

Edit `crates/roki-daemon/src/escalation/mod.rs`:

```rust
//! Escalation queue (fr:06 §Escalation queue).

pub mod entry;
pub mod ring;

pub use entry::EscalationEntry;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p roki-daemon --lib escalation::entry`
Expected: 3 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/escalation/entry.rs \
        crates/roki-daemon/src/escalation/mod.rs
git commit -m "feat(escalation): EscalationEntry with cycle-bound and cycle-less constructors"
```

---

## Task 3: `Event::EscalationAdded` wire format

**Files:**
- Modify: `crates/roki-daemon/src/events.rs`

- [ ] **Step 1: Read existing event surface**

```bash
grep -n "FailureUnhandled\|FailureMetaSer" crates/roki-daemon/src/events.rs | head -10
```

Confirm `FailureMetaSer` lives at events.rs:58–78 and `Event::FailureUnhandled` lives at 90–96 with the `marker` field.

- [ ] **Step 2: Add the `EscalationAdded` variant**

Add inside the `pub enum Event` block in `crates/roki-daemon/src/events.rs`, immediately before the closing brace (right after `FailureUnhandled`):

```rust
    EscalationAdded {
        ts: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ticket_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cycle_id: Option<String>,
        failure: FailureMetaSer,
    },
```

- [ ] **Step 3: Add a serialization unit test**

Append inside the existing `#[cfg(test)] mod tests { ... }` block in `events.rs` (just after `failure_unhandled_serializes_marker_and_failure`):

```rust
#[test]
fn escalation_added_serializes_cycle_bound_entry() {
    let ev = Event::EscalationAdded {
        ts: "2026-05-09T12:34:56Z".to_string(),
        ticket_id: Some("TEAM-1".to_string()),
        cycle_id: Some("00000000-0000-0000-0000-000000000001".to_string()),
        failure: FailureMetaSer {
            kind: "fs_poison".to_string(),
            phase: Some("post".to_string()),
            iter: 0,
            exit_code: None,
            error_text: "cleanup remove_dir_all failed".to_string(),
        },
    };
    let s = serde_json::to_string(&ev).unwrap();
    assert!(s.contains("\"event\":\"escalation_added\""), "{s}");
    assert!(s.contains("\"ticket_id\":\"TEAM-1\""), "{s}");
    assert!(s.contains("\"cycle_id\":\"00000000-0000-0000-0000-000000000001\""), "{s}");
    assert!(s.contains("\"kind\":\"fs_poison\""), "{s}");
}

#[test]
fn escalation_added_omits_cycle_fields_for_daemon_entry() {
    let ev = Event::EscalationAdded {
        ts: "2026-05-09T12:34:56Z".to_string(),
        ticket_id: None,
        cycle_id: None,
        failure: FailureMetaSer {
            kind: "fs_poison".to_string(),
            phase: None,
            iter: 0,
            exit_code: None,
            error_text: "orphan reconcile failed".to_string(),
        },
    };
    let s = serde_json::to_string(&ev).unwrap();
    assert!(s.contains("\"event\":\"escalation_added\""), "{s}");
    assert!(!s.contains("\"ticket_id\""), "ticket_id must be elided: {s}");
    assert!(!s.contains("\"cycle_id\""), "cycle_id must be elided: {s}");
    assert!(!s.contains("\"phase\""), "phase must be elided: {s}");
}
```

`FailureMetaSer.phase` already carries `#[serde(skip_serializing_if = "Option::is_none")]` (verify at events.rs line 61–62; if missing, add it as part of this task). Ditto `exit_code` at line 63–64 — already skipped.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p roki-daemon --lib events::tests`
Expected: all existing event tests still pass plus 2 new ones.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/events.rs
git commit -m "feat(events): add escalation_added event variant"
```

---

## Task 4: `EscalationQueue` type with daemon-writer-backed emission

**Files:**
- Create: `crates/roki-daemon/src/escalation/queue.rs`
- Modify: `crates/roki-daemon/src/escalation/mod.rs`

- [ ] **Step 1: Write the queue**

Create `crates/roki-daemon/src/escalation/queue.rs`:

```rust
//! In-memory escalation queue. Pushes emit `escalation_added` to the
//! daemon-scoped event log. Eviction drops cycle-bound entries by ticket id.

use std::sync::Arc;

use time::format_description::well_known::Rfc3339;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::engine::outcome::{FailureKind, PhaseKind};
use crate::escalation::entry::EscalationEntry;
use crate::escalation::ring::{PushOutcome, Ring};
use crate::events::{Event, EventWriter, FailureMetaSer};

pub struct EscalationQueue {
    inner: Mutex<Ring<EscalationEntry>>,
    daemon_writer: Arc<Mutex<EventWriter>>,
}

impl EscalationQueue {
    pub fn new(capacity: usize, daemon_writer: Arc<Mutex<EventWriter>>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Ring::new(capacity)),
            daemon_writer,
        })
    }

    pub async fn push_cycle(
        &self,
        ticket_id: String,
        cycle_id: Uuid,
        failure_kind: FailureKind,
        phase: PhaseKind,
        error_text: String,
    ) {
        let entry = EscalationEntry::cycle(
            ticket_id.clone(),
            cycle_id,
            failure_kind,
            phase,
            error_text,
        );
        self.insert_and_emit(entry).await;
    }

    pub async fn push_daemon(&self, failure_kind: FailureKind, error_text: String) {
        let entry = EscalationEntry::daemon(failure_kind, error_text);
        self.insert_and_emit(entry).await;
    }

    async fn insert_and_emit(&self, entry: EscalationEntry) {
        let snapshot = entry.clone();
        {
            let mut ring = self.inner.lock().await;
            if let PushOutcome::Overflowed { dropped } = ring.push(entry) {
                tracing::warn!(
                    dropped_kind = dropped.failure_kind.as_str(),
                    dropped_ticket_id = dropped.ticket_id.as_deref().unwrap_or("<daemon>"),
                    "escalation queue overflow; oldest entry dropped"
                );
            }
        }
        let mut w = self.daemon_writer.lock().await;
        let _ = w.emit(&Event::EscalationAdded {
            ts: snapshot
                .timestamp
                .format(&Rfc3339)
                .unwrap_or_else(|_| String::new()),
            ticket_id: snapshot.ticket_id.clone(),
            cycle_id: snapshot.cycle_id.map(|u| u.to_string()),
            failure: FailureMetaSer {
                kind: snapshot.failure_kind.as_str().to_string(),
                phase: snapshot.phase.map(|p| p.as_str().to_string()),
                iter: 0,
                exit_code: None,
                error_text: snapshot.error_text.clone(),
            },
        });
    }

    pub async fn evict_ticket(&self, ticket_id: &str) {
        let mut ring = self.inner.lock().await;
        ring.retain(|e| e.ticket_id.as_deref() != Some(ticket_id));
    }

    pub async fn snapshot(&self) -> Vec<EscalationEntry> {
        let ring = self.inner.lock().await;
        ring.iter().cloned().collect()
    }

    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn writer_for(dir: &Path) -> Arc<Mutex<EventWriter>> {
        let w = EventWriter::open(dir, "_daemon").expect("open daemon writer");
        Arc::new(Mutex::new(w))
    }

    #[tokio::test]
    async fn push_cycle_appends_entry() {
        let dir = TempDir::new().unwrap();
        let q = EscalationQueue::new(8, writer_for(dir.path()));
        q.push_cycle(
            "T-1".into(),
            Uuid::new_v4(),
            FailureKind::FsPoison,
            PhaseKind::Post,
            "boom".into(),
        )
        .await;
        assert_eq!(q.len().await, 1);
        let snap = q.snapshot().await;
        assert_eq!(snap[0].ticket_id.as_deref(), Some("T-1"));
    }

    #[tokio::test]
    async fn push_daemon_leaves_cycle_fields_none() {
        let dir = TempDir::new().unwrap();
        let q = EscalationQueue::new(4, writer_for(dir.path()));
        q.push_daemon(FailureKind::FsPoison, "no cycle".into()).await;
        let snap = q.snapshot().await;
        assert!(snap[0].ticket_id.is_none());
        assert!(snap[0].cycle_id.is_none());
        assert!(snap[0].phase.is_none());
    }

    #[tokio::test]
    async fn evict_ticket_drops_only_matching_cycle_entries() {
        let dir = TempDir::new().unwrap();
        let q = EscalationQueue::new(8, writer_for(dir.path()));
        q.push_cycle(
            "T-1".into(),
            Uuid::new_v4(),
            FailureKind::FsPoison,
            PhaseKind::Post,
            "x".into(),
        )
        .await;
        q.push_cycle(
            "T-2".into(),
            Uuid::new_v4(),
            FailureKind::FsPoison,
            PhaseKind::Post,
            "y".into(),
        )
        .await;
        q.push_daemon(FailureKind::FsPoison, "z".into()).await;
        q.evict_ticket("T-1").await;
        let snap = q.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|e| e.ticket_id.as_deref() != Some("T-1")));
        assert!(snap.iter().any(|e| e.ticket_id.is_none()));
    }

    #[tokio::test]
    async fn overflow_drops_oldest_and_writes_event() {
        let dir = TempDir::new().unwrap();
        let q = EscalationQueue::new(2, writer_for(dir.path()));
        for i in 0..3 {
            q.push_cycle(
                format!("T-{i}"),
                Uuid::new_v4(),
                FailureKind::FsPoison,
                PhaseKind::Post,
                format!("e{i}"),
            )
            .await;
        }
        let snap = q.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].ticket_id.as_deref(), Some("T-1"));
        assert_eq!(snap[1].ticket_id.as_deref(), Some("T-2"));

        let body = std::fs::read_to_string(
            crate::events::events_path(dir.path(), "_daemon"),
        )
        .unwrap();
        assert_eq!(
            body.lines().filter(|l| l.contains("\"event\":\"escalation_added\"")).count(),
            3,
            "one escalation_added per push"
        );
    }
}
```

- [ ] **Step 2: Re-export from `escalation::mod`**

Edit `crates/roki-daemon/src/escalation/mod.rs`:

```rust
//! Escalation queue (fr:06 §Escalation queue).

pub mod entry;
pub mod queue;
pub mod ring;

pub use entry::EscalationEntry;
pub use queue::EscalationQueue;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p roki-daemon --lib escalation::queue`
Expected: 4 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/escalation/queue.rs \
        crates/roki-daemon/src/escalation/mod.rs
git commit -m "feat(escalation): EscalationQueue with cycle-bound, cycle-less, evict, snapshot"
```

---

## Task 5: `[escalation].queue_size` config key

**Files:**
- Modify: `crates/roki-daemon/src/config/roki.rs`

- [ ] **Step 1: Add `EscalationSection` and validate**

In `crates/roki-daemon/src/config/roki.rs`:

(a) After the `LogSection` definition (around line 132), append:

```rust
/// `[escalation]` section. Bounds the in-memory escalation queue
/// (fr:06 §Escalation queue).
#[derive(Clone, Debug)]
pub struct EscalationSection {
    pub queue_size: u32,
}

impl Default for EscalationSection {
    fn default() -> Self {
        Self { queue_size: 64 }
    }
}
```

(b) Add `pub escalation: EscalationSection,` to the `RokiConfig` struct (after the `log` field, before `default_ai_session`). Update the `Debug` impl to include `.field("escalation", &self.escalation)` after `.field("log", ...)`.

(c) In `RawRokiConfig` add `escalation: Option<RawEscalation>,`.

(d) Add the raw shape after `RawLog`:

```rust
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawEscalation {
    queue_size: Option<u32>,
}
```

(e) In `RawRokiConfig::validate`, after the `let log = LogSection { ... };` block:

```rust
let raw_escalation = self.escalation.unwrap_or_default();
let escalation = parse_escalation(path, raw_escalation)?;
```

…and pass `escalation` into the final `Ok(RokiConfig { ... })`.

(f) Add the validator helper near `parse_engine`:

```rust
fn parse_escalation(
    path: &Path,
    raw: RawEscalation,
) -> Result<EscalationSection, RokiConfigError> {
    let queue_size = raw.queue_size.unwrap_or(64);
    if !(1..=1024).contains(&queue_size) {
        return Err(RokiConfigError::InvalidValue {
            path: path.to_path_buf(),
            key: "escalation.queue_size",
            reason: "must be between 1 and 1024".into(),
        });
    }
    Ok(EscalationSection { queue_size })
}
```

If `RokiConfigError` lacks an `InvalidValue { path, key, reason }` variant, add it next to the existing `MissingField` variant in the same file (search for `pub enum RokiConfigError` and mirror the existing variant shape including `Display`).

(g) Update `RokiConfig::test_default` to include `escalation: EscalationSection::default(),`.

- [ ] **Step 2: Add unit tests for the validator**

Append to the existing `#[cfg(test)] mod tests { ... }` block at the bottom of `roki.rs`:

```rust
#[test]
fn escalation_default_is_64() {
    let toml = minimal_toml_without_escalation();
    let cfg = parse(&toml);
    assert_eq!(cfg.escalation.queue_size, 64);
}

#[test]
fn escalation_zero_is_rejected() {
    let toml = format!(
        "{}\n[escalation]\nqueue_size = 0\n",
        minimal_toml_without_escalation()
    );
    let err = parse_err(&toml);
    assert!(matches!(
        err,
        RokiConfigError::InvalidValue { key: "escalation.queue_size", .. }
    ));
}

#[test]
fn escalation_above_1024_is_rejected() {
    let toml = format!(
        "{}\n[escalation]\nqueue_size = 2000\n",
        minimal_toml_without_escalation()
    );
    let err = parse_err(&toml);
    assert!(matches!(
        err,
        RokiConfigError::InvalidValue { key: "escalation.queue_size", .. }
    ));
}
```

If `parse`, `parse_err`, and `minimal_toml_without_escalation` helpers do not already exist in the test module, look for the closest existing fixture (e.g. `minimal_toml`) and write the new helper as:

```rust
fn minimal_toml_without_escalation() -> String {
    // Use the fixture the existing tests already use; add nothing under [escalation].
    minimal_toml().to_string()
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p roki-daemon --lib config::roki`
Expected: all existing config tests still pass + 3 new ones.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/config/roki.rs
git commit -m "feat(config): [escalation].queue_size with default 64, validation 1..=1024"
```

---

## Task 6: Wire `EscalationQueue` through runtime → dispatcher → ticket_task → real_runner (no behavior change)

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`
- Modify: `crates/roki-daemon/src/daemon/dispatcher.rs`
- Modify: `crates/roki-daemon/src/daemon/ticket_task.rs`
- Modify: `crates/roki-daemon/src/daemon/real_runner.rs`
- Modify: `crates/roki-daemon/src/daemon/cold_start.rs`

- [ ] **Step 1: Construct queue in `runtime::run_inner`**

Edit `crates/roki-daemon/src/runtime.rs`. After step 4 ("Open the daemon-scoped event log") and before step 5 (`DaemonStarted` emit), insert:

```rust
    // 4b. Build escalation queue (fr:06 §Escalation queue) — wired before
    //     DaemonStarted so any startup-bound failure has a receiver.
    let escalation = crate::escalation::EscalationQueue::new(
        cfg.escalation.queue_size as usize,
        daemon_events.clone(),
    );
```

- [ ] **Step 2: Thread queue into `Dispatcher`**

In `crates/roki-daemon/src/daemon/dispatcher.rs`:

(a) Add to imports:
```rust
use crate::escalation::EscalationQueue;
```

(b) Add `escalation: Arc<EscalationQueue>` field to the `Dispatcher` struct.

(c) Update `Dispatcher::new` signature: append `escalation: Arc<EscalationQueue>,` to the parameter list and assign in `Self { ... }`.

(d) In `runtime.rs` step 9, pass `escalation.clone()` when calling `Dispatcher::new`.

- [ ] **Step 3: Thread queue into `RealCycleRunner` and `CycleRunner` trait**

In `crates/roki-daemon/src/daemon/real_runner.rs`:

(a) Add field `pub escalation: Arc<crate::escalation::EscalationQueue>,` to `RealCycleRunner`.

(b) `runtime.rs` step 8 — set the field on construction:

```rust
    let runner = Arc::new(RealCycleRunner {
        workflow: workflow.clone(),
        cfg: cfg.clone(),
        executor,
        escalation: escalation.clone(),
    });
```

(c) The `CycleRunner` trait (in `daemon::ticket_task`) already drives `RealCycleRunner` through dynamic dispatch; the trait itself does not need a queue arg because the runner owns its own `Arc`. Skip trait edits.

- [ ] **Step 4: Thread queue into `ticket_task` for eviction (no push sites yet)**

In `crates/roki-daemon/src/daemon/ticket_task.rs`:

(a) Find the per-ticket actor entry (`pub async fn run` or similar). Add a parameter `escalation: Arc<EscalationQueue>` (use `crate::escalation::EscalationQueue`).

(b) Plumb the parameter through to wherever the dispatcher spawns the task. Search for `tokio::spawn(...)` in `daemon::dispatcher::*` that constructs a per-ticket actor — pass `self.escalation.clone()` into it.

(c) No callsites of `escalation` yet — Task 9 wires the eviction calls. This step only adds the field.

- [ ] **Step 5: Thread queue into `ColdStart`**

In `crates/roki-daemon/src/daemon/cold_start.rs`:

(a) Add `pub escalation: Arc<crate::escalation::EscalationQueue>,` to the `ColdStart` struct.

(b) `runtime.rs` step 10 — set the field on construction:

```rust
    let cold_start = crate::daemon::cold_start::ColdStart {
        cfg: cfg.clone(),
        workflow: workflow.clone(),
        me: me.clone(),
        cache: cache.clone(),
        dispatcher: dispatcher.clone(),
        graphql,
        mode,
        escalation: escalation.clone(),
    };
```

- [ ] **Step 6: Verify the crate compiles**

Run: `cargo build -p roki-daemon`
Expected: clean build. No new warnings (the `escalation` field is read in later tasks; if the compiler warns about an unused field, suppress by leaving the field public-and-untouched — Rust does not warn on public unused fields).

- [ ] **Step 7: Run all unit tests**

Run: `cargo test -p roki-daemon --lib`
Expected: all green.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs \
        crates/roki-daemon/src/daemon/dispatcher.rs \
        crates/roki-daemon/src/daemon/ticket_task.rs \
        crates/roki-daemon/src/daemon/real_runner.rs \
        crates/roki-daemon/src/daemon/cold_start.rs
git commit -m "feat(escalation): wire queue through runtime/dispatcher/ticket_task/cold_start"
```

---

## Task 7: Migrate `engine::cleanup` from `failure_unhandled` to `escalation_added`

**Files:**
- Modify: `crates/roki-daemon/src/engine/cleanup.rs`
- Modify: `crates/roki-daemon/src/daemon/real_runner.rs`
- Modify: `crates/roki-daemon/src/daemon/ticket_task.rs`

- [ ] **Step 1: Update `engine::cleanup` push sites**

In `crates/roki-daemon/src/engine/cleanup.rs`:

(a) Module doc comment: replace the line "emit `failure_unhandled marker=cleanup_fs_error` and propagate as Err so" with "push to the escalation queue (fr:06 §Escalation queue) and propagate as Err so".

(b) Update `delete_immediate` signature — add `escalation: &EscalationQueue,` parameter:

```rust
pub async fn delete_immediate(
    ticket_id: &str,
    ghq: &str,
    session_root: &Path,
    cycle_id: Uuid,
    events: &mut EventWriter,
    escalation: &EscalationQueue,
) -> Result<(), CleanupError> {
```

Note: a stable `cycle_id` is now passed in by the caller rather than synthesized inside, so the escalation entry carries an id the caller can correlate with `cycle_completed`. Update the body to use the passed-in id; remove `let cycle_id = Uuid::new_v4();`.

(c) Update `post_cycle_delete` signature — same `escalation` parameter:

```rust
pub async fn post_cycle_delete(
    ticket_id: &str,
    ghq: &str,
    session_root: &Path,
    cycle_id: Uuid,
    events: &mut EventWriter,
    escalation: &EscalationQueue,
) -> Result<(), CleanupError> {
```

(d) Replace the `failure_unhandled` emit inside `remove_ticket_dir` (events.rs:101–113):

```rust
        Err(e) => {
            let err_text = format!("cleanup remove_dir_all failed: {e}");
            if let Some(cid) = cycle_id {
                escalation
                    .push_cycle(
                        ticket_id.to_string(),
                        cid,
                        FailureKind::FsPoison,
                        PhaseKind::Post,
                        err_text,
                    )
                    .await;
            } else {
                escalation
                    .push_daemon(FailureKind::FsPoison, err_text)
                    .await;
            }
            Err(CleanupError::FsError(e))
        }
```

Update `remove_ticket_dir` signature to accept `escalation: &EscalationQueue`.

(e) Replace `emit_wt_remove_error` body. Make it `async` and accept `escalation`:

```rust
async fn emit_wt_remove_error(
    escalation: &EscalationQueue,
    ticket_id: &str,
    cycle_id: Uuid,
    err: &crate::engine::worktree::WorktreeError,
) -> CleanupError {
    let err_text = format!("cleanup wt remove failed: {err}");
    escalation
        .push_cycle(
            ticket_id.to_string(),
            cycle_id,
            FailureKind::FsPoison,
            PhaseKind::Post,
            err_text,
        )
        .await;
    CleanupError::FsError(std::io::Error::other(err.to_string()))
}
```

Adjust call sites inside `delete_immediate` and `post_cycle_delete` to `await` it and pass `ticket_id` / `cycle_id`. Drop the `events` parameter on this helper.

(f) Adjust imports at the top of `cleanup.rs`:

```rust
use crate::engine::outcome::{FailureKind, PhaseKind};
use crate::escalation::EscalationQueue;
use crate::events::{
    Event, EventWriter, WorktreeDeleteReason, now_rfc3339,
};
```

Drop `FailureMarker` and `FailureMetaSer` from the import if they are no longer referenced.

- [ ] **Step 2: Update `real_runner` to pass the queue and synthesize a cycle id for shorthand**

In `crates/roki-daemon/src/daemon/real_runner.rs`:

(a) `DispatchTarget::CleanupShorthand` branch (line ~57): synthesize a cycle id locally and pass it in.

```rust
            DispatchTarget::CleanupShorthand => {
                let cycle_id = uuid::Uuid::new_v4();
                if crate::engine::cleanup::delete_immediate(
                    &admitted.ticket.id,
                    &admitted.ghq,
                    &self.cfg.paths.session_root,
                    cycle_id,
                    &mut events,
                    &self.escalation,
                )
                .await
                .is_err()
                {
                    return CycleResult::CleanupFsError {
                        ticket_id: admitted.ticket.id.clone(),
                    };
                }
                return CycleResult::ShorthandDeleted;
            }
```

(b) `post_cycle_delete` callsite (line ~115):

```rust
                    let _ = crate::engine::cleanup::post_cycle_delete(
                        &admitted.ticket.id,
                        &admitted.ghq,
                        &self.cfg.paths.session_root,
                        cycle_id,
                        &mut events,
                        &self.escalation,
                    )
                    .await;
```

(c) The shorthand-failed path previously returned `CycleResult::Failed { meta: boot_path_failure(), kind: Cleanup }` — which routed through `handle_failed_cycle` and could match `[[on_failure]]` for `kind = fs_poison`, contradicting `fr:06 §Daemon-detected failure kinds` for `fs_poison`. Returning the new `CycleResult::CleanupFsError` skips that path.

- [ ] **Step 3: Add `CycleResult::CleanupFsError` variant**

In `crates/roki-daemon/src/daemon/ticket_task.rs`, find the `pub enum CycleResult` definition (around line 73). Add:

```rust
    CleanupFsError {
        ticket_id: String,
    },
```

In the actor's match arm that consumes `CycleResult`, add:

```rust
        CycleResult::CleanupFsError { ticket_id } => {
            // fr:06: cleanup-time fs errors land in the escalation queue,
            // not [[on_failure]]. The push has already happened inside
            // engine::cleanup. This arm just tears the cycle down without
            // routing through handle_failed_cycle.
            cache.evict(&ticket_id).await;
            return StepOutcome::Dispatched { kind: CycleKind::Cleanup, evicted: true };
        }
```

(Mirror the eviction the existing `ShorthandDeleted` arm already does — search for `evict` calls in the file. If the existing arms use a slightly different return shape, mirror that shape; the key invariant is: do NOT call `handle_failed_cycle`.)

- [ ] **Step 4: Replace the existing `worktree_cleanup_fs_error_smoke` test with `escalation_cleanup_fs_error_smoke`**

```bash
git rm crates/roki-daemon/tests/e2e/worktree_cleanup_fs_error_smoke.rs
```

Create `crates/roki-daemon/tests/e2e/escalation_cleanup_fs_error_smoke.rs` by copying the deleted file's structure (commands and fixtures) and changing the assertion block:

Old assertion (deleted file): asserted `failure_unhandled` event with `marker=cleanup_fs_error`.

New assertion:

```rust
    // fr:06: cleanup-time fs errors push to the escalation queue and emit
    // escalation_added on the daemon-scoped event log. failure_unhandled
    // is NOT emitted; [[on_failure]] is NOT consulted.
    let body = std::fs::read_to_string(daemon_events_path).expect("read _daemon events");

    assert!(
        !body.contains("\"event\":\"failure_unhandled\""),
        "no failure_unhandled expected:\n{body}"
    );
    assert!(
        !body.contains("\"cycle_kind\":\"failure\""),
        "no failure-handler cycle expected (cleanup_fs_error must skip [[on_failure]]):\n{body}"
    );
    let escalations: Vec<_> = body
        .lines()
        .filter(|l| l.contains("\"event\":\"escalation_added\""))
        .collect();
    assert_eq!(escalations.len(), 1, "expected exactly one escalation_added:\n{body}");
    let line = escalations[0];
    assert!(line.contains("\"kind\":\"fs_poison\""), "{line}");
    assert!(line.contains("\"phase\":\"post\""), "{line}");
    assert!(line.contains("\"ticket_id\":"), "{line}");
```

If the deleted file's setup pinned `_daemon` events path differently, mirror it exactly. The path discovery helper (`session_root.join("_daemon.events.jsonl")` in slice 4 fixtures) is unchanged.

Add the `[[test]]` entry to `crates/roki-daemon/Cargo.toml` mirroring the prior `worktree_cleanup_fs_error_smoke` entry — replace the name only.

- [ ] **Step 5: Run unit tests + the new e2e**

```
cargo test -p roki-daemon --lib
cargo test -p roki-daemon --test escalation_cleanup_fs_error_smoke
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/engine/cleanup.rs \
        crates/roki-daemon/src/daemon/real_runner.rs \
        crates/roki-daemon/src/daemon/ticket_task.rs \
        crates/roki-daemon/tests/e2e/escalation_cleanup_fs_error_smoke.rs \
        crates/roki-daemon/Cargo.toml
git rm crates/roki-daemon/tests/e2e/worktree_cleanup_fs_error_smoke.rs 2>/dev/null || true
git commit -m "feat(escalation): route cleanup_fs_error to queue, skip [[on_failure]]"
```

---

## Task 8: Migrate recursion paths from `failure_unhandled` to `escalation_added`

**Files:**
- Modify: `crates/roki-daemon/src/daemon/real_runner.rs`
- Delete: `crates/roki-daemon/tests/e2e/recursion_bound_smoke.rs`
- Create: `crates/roki-daemon/tests/e2e/escalation_recursion_smoke.rs`

- [ ] **Step 1: Replace the three `FailureUnhandled` emits in `handle_failed_cycle`**

In `crates/roki-daemon/src/daemon/real_runner.rs`:

(a) Add imports at top:

```rust
use crate::engine::outcome::{CycleKind, FailureKind, FailureMeta, PhaseKind};
use crate::escalation::EscalationQueue;
```

(`FailureMarker` and `FailureMetaSer` imports stay — `FailureMarker::None` is still needed for the no-match `failure_unhandled` emit at line 200–209.)

(b) Pass `&self.escalation` into `handle_failed_cycle` and pass through the ticket id. Update the function signature:

```rust
#[allow(clippy::too_many_arguments)]
async fn handle_failed_cycle(
    meta: &FailureMeta,
    failed_kind: CycleKind,
    workflow: &WorkflowConfig,
    executor: &CommandPhaseExecutor,
    admitted: &AdmittedTicket,
    cfg: &RokiConfig,
    events: &mut EventWriter,
    cycle_trigger: CycleTrigger,
    escalation: &EscalationQueue,
) -> HandlerDecision {
```

Update the call site in `RealCycleRunner::run_cycle` (line ~127–137) to pass `&self.escalation`.

(c) Replace the three `Event::FailureUnhandled` emits at lines 189, 235, 246. The expected new bodies:

**Site 1 — recursion bound on the failure cycle's own failure (line ~189):**

```rust
    // Recursion bound: a failure cycle that itself fails must not recurse.
    // fr:06 trigger 1: push to escalation queue instead of emitting
    // failure_unhandled.
    if failed_kind == CycleKind::Failure {
        escalation
            .push_cycle(
                admitted.ticket.id.clone(),
                meta.failed_cycle_id,
                meta.kind,
                meta.phase,
                meta.error_text.clone(),
            )
            .await;
        return HandlerDecision::Unhandled;
    }
```

**Site 2 — handler cycle returned `Failed` (line ~234–243):**

```rust
        Ok(crate::engine::CycleOutcome::Failed { meta: handler_meta }) => {
            // fr:06 trigger 1: handler cycle failed. Push to queue with the
            // handler cycle's own id so operators can correlate logs.
            escalation
                .push_cycle(
                    admitted.ticket.id.clone(),
                    handler_meta.failed_cycle_id,
                    handler_meta.kind,
                    handler_meta.phase,
                    handler_meta.error_text.clone(),
                )
                .await;
            HandlerDecision::Unhandled
        }
```

**Site 3 — handler cycle infra error (line ~244–252):**

```rust
        Err(infra) => {
            tracing::error!(?infra, "handler cycle infra error");
            // fr:06 trigger 1: handler cycle hit an infra error. Synthesize
            // FsPoison for the failure_kind because infra errors do not carry
            // a phase-level FailureKind. Tag with the original failed cycle's
            // id and phase to keep the operator-visible scope identical to
            // the user's [[on_failure]] match.
            escalation
                .push_cycle(
                    admitted.ticket.id.clone(),
                    meta.failed_cycle_id,
                    FailureKind::FsPoison,
                    meta.phase,
                    format!("handler cycle infra error: {infra}"),
                )
                .await;
            HandlerDecision::Unhandled
        }
```

(d) Leave the no-match emit (line 201–209) unchanged — `failure_unhandled marker=none` is still the contract for that path per spec §6.1.

- [ ] **Step 2: Delete the old e2e and create the new one**

```bash
git rm crates/roki-daemon/tests/e2e/recursion_bound_smoke.rs
```

Create `crates/roki-daemon/tests/e2e/escalation_recursion_smoke.rs`. Use `recursion_bound_smoke.rs` from git history as the structural baseline (`git show HEAD:crates/roki-daemon/tests/e2e/recursion_bound_smoke.rs`) and change only the assertion block:

```rust
    // fr:06: handler-cycle-fails route through the escalation queue, not
    // failure_unhandled. The persistent daemon stays alive.
    let body = std::fs::read_to_string(&daemon_events_path).expect("read _daemon events");

    let escalations: Vec<_> = body
        .lines()
        .filter(|l| l.contains("\"event\":\"escalation_added\""))
        .collect();
    assert_eq!(escalations.len(), 1, "expected exactly one escalation_added:\n{body}");

    let unhandled: Vec<_> = body
        .lines()
        .filter(|l| l.contains("\"event\":\"failure_unhandled\""))
        .collect();
    assert!(unhandled.is_empty(), "no failure_unhandled expected:\n{body}");

    let entry = escalations[0];
    assert!(entry.contains("\"ticket_id\":"), "{entry}");
    assert!(entry.contains("\"cycle_id\":"), "{entry}");
    assert!(entry.contains("\"kind\":"), "{entry}");
    assert!(entry.contains("\"phase\":"), "{entry}");
```

Update `Cargo.toml`'s `[[test]]` entry name from `recursion_bound_smoke` to `escalation_recursion_smoke`.

- [ ] **Step 3: Update `failure_unhandled_smoke.rs` to assert no escalation**

In `crates/roki-daemon/tests/e2e/failure_unhandled_smoke.rs`, after the existing `failure_unhandled` count assertion, append:

```rust
    assert!(
        !body.contains("\"event\":\"escalation_added\""),
        "no escalation_added expected for marker=none path:\n{body}"
    );
```

- [ ] **Step 4: Update `on_failure_smoke.rs` to assert no escalation**

In `crates/roki-daemon/tests/e2e/on_failure_smoke.rs`, after the existing `failure_unhandled` zero-count assertion, append:

```rust
    assert!(
        !body.contains("\"event\":\"escalation_added\""),
        "no escalation_added expected when [[on_failure]] succeeds:\n{body}"
    );
```

- [ ] **Step 5: Run the affected e2e**

```
cargo test -p roki-daemon --test escalation_recursion_smoke
cargo test -p roki-daemon --test failure_unhandled_smoke
cargo test -p roki-daemon --test on_failure_smoke
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/daemon/real_runner.rs \
        crates/roki-daemon/tests/e2e/escalation_recursion_smoke.rs \
        crates/roki-daemon/tests/e2e/failure_unhandled_smoke.rs \
        crates/roki-daemon/tests/e2e/on_failure_smoke.rs \
        crates/roki-daemon/Cargo.toml
git rm crates/roki-daemon/tests/e2e/recursion_bound_smoke.rs 2>/dev/null || true
git commit -m "feat(escalation): route recursion paths to queue, narrow failure_unhandled to marker=none"
```

---

## Task 9: Cold-start orphan-reconcile fs error → cycle-less queue entry

**Files:**
- Modify: `crates/roki-daemon/src/daemon/orphan.rs`
- Modify: `crates/roki-daemon/src/daemon/cold_start.rs`
- Create: `crates/roki-daemon/tests/e2e/escalation_orphan_reconcile_smoke.rs`

- [ ] **Step 1: Surface fs errors from `orphan::reconcile` (already done in slice 6)**

```bash
grep -n "fs_errors" crates/roki-daemon/src/daemon/orphan.rs
```

Confirm `OrphanReport::fs_errors: Vec<(String, std::io::Error)>` already exists. If not, add it and populate it where `remove_dir_all` errors are observed.

- [ ] **Step 2: Push cycle-less entries from `cold_start` after reconcile**

In `crates/roki-daemon/src/daemon/cold_start.rs`, after the line `let orphan_report = orphan::reconcile(scan, writer.clone()).await;` (line ~175):

```rust
    for (ticket_id, err) in &orphan_report.fs_errors {
        self.escalation
            .push_daemon(
                crate::engine::outcome::FailureKind::FsPoison,
                format!("orphan reconcile {ticket_id}: {err}"),
            )
            .await;
    }
```

- [ ] **Step 3: Write the e2e**

Create `crates/roki-daemon/tests/e2e/escalation_orphan_reconcile_smoke.rs`. Mirror the structure of `cold_start_orphan_reconcile_smoke.rs` (which exercises the success path) and force a permission-denied delete to trigger `fs_errors`.

```rust
//! Cold-start orphan reconcile fs error → cycle-less escalation entry.
//!
//! Setup: pre-populate `<session_root>/<ticket-id>/` then `chmod 0` the
//! parent so `remove_dir_all` fails with PermissionDenied. Linear returns
//! zero matching tickets so the directory is treated as orphan and the
//! reconcile attempt errors.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

mod support;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_reconcile_fs_error_pushes_cycle_less_escalation() {
    // Use the shared cold-start fixture from support/cold_start.rs.
    let fx = support::cold_start::ColdStartFixture::new()
        .with_no_admitted_tickets()
        .build()
        .await;

    // Pre-create an orphan dir.
    let orphan_dir: PathBuf = fx.session_root.join("ORPHAN-1");
    std::fs::create_dir_all(&orphan_dir).expect("create orphan dir");

    // chmod the parent to read-only to force PermissionDenied on remove.
    let parent = fx.session_root.clone();
    let mut perms = std::fs::metadata(&parent).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&parent, perms).unwrap();

    fx.run_cold_start_to_ready().await;

    // Restore writable so test cleanup works.
    let mut restored = std::fs::metadata(&parent).unwrap().permissions();
    restored.set_mode(0o755);
    std::fs::set_permissions(&parent, restored).unwrap();

    let body = std::fs::read_to_string(&fx.daemon_events_path).expect("read _daemon events");

    let escalations: Vec<_> = body
        .lines()
        .filter(|l| l.contains("\"event\":\"escalation_added\""))
        .collect();
    assert_eq!(escalations.len(), 1, "expected one escalation_added:\n{body}");
    let line = escalations[0];
    assert!(!line.contains("\"ticket_id\""), "cycle-less entry must omit ticket_id: {line}");
    assert!(!line.contains("\"cycle_id\""), "cycle-less entry must omit cycle_id: {line}");
    assert!(line.contains("\"kind\":\"fs_poison\""), "{line}");
    assert!(
        body.contains("\"event\":\"cold_start_completed\""),
        "cold_start_completed should still fire:\n{body}"
    );
}
```

If `support::cold_start::ColdStartFixture` does not have a `with_no_admitted_tickets()` builder, mirror the smallest existing cold-start fixture's plumbing and stub the GraphQL response with an empty `nodes` array. The full helper API is documented in `crates/roki-daemon/tests/e2e/support/cold_start.rs` (slice 6 created it).

Add the `[[test]]` entry to `Cargo.toml`.

- [ ] **Step 4: Run the e2e**

Run: `cargo test -p roki-daemon --test escalation_orphan_reconcile_smoke`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/daemon/cold_start.rs \
        crates/roki-daemon/tests/e2e/escalation_orphan_reconcile_smoke.rs \
        crates/roki-daemon/Cargo.toml
git commit -m "feat(escalation): cold-start orphan reconcile fs errors push cycle-less entries"
```

---

## Task 10: Eviction wiring + `escalation_evicted_on_cleanup_smoke`

**Files:**
- Modify: `crates/roki-daemon/src/daemon/dispatcher.rs`
- Modify: `crates/roki-daemon/src/daemon/ticket_task.rs`
- Modify: `crates/roki-daemon/src/daemon/cold_start.rs`
- Create: `crates/roki-daemon/tests/e2e/escalation_evicted_on_cleanup_smoke.rs`

- [ ] **Step 1: Evict on admission revoke (dispatcher)**

In `crates/roki-daemon/src/daemon/dispatcher.rs`, find the eviction branch around line 109–120 ("cache-only eviction. Worktree + session_tempdir are retained"). After `self.cache.evict(&ticket_id).await;` (line 120) and after `self.cache.set_pending_evict(&ticket_id).await;` (line 114), add:

```rust
                self.escalation.evict_ticket(&ticket_id).await;
```

(Place a single call at each of the two branches — the immediate-evict path and the pending-evict path. The pending-evict path's call clears whatever escalation entries already exist; the ticket task may push more after this call, which is the documented race in spec §3.4.)

- [ ] **Step 2: Evict on cleanup-cycle terminal and shorthand (ticket_task)**

In `crates/roki-daemon/src/daemon/ticket_task.rs`, the actor's match arm for `CycleResult::Completed { kind: Cleanup, .. }` and `CycleResult::ShorthandDeleted` already evicts the cache (search for `cache.evict` near line 188–199). Immediately after each `cache.evict(ticket_id).await` add:

```rust
                escalation.evict_ticket(ticket_id).await;
```

`escalation` is the field threaded in Task 6 Step 4. The `CycleResult::CleanupFsError` arm added in Task 7 Step 3 also evicts cache; mirror the call there.

- [ ] **Step 3: Evict on cold-start orphan delete (cold_start)**

In `crates/roki-daemon/src/daemon/cold_start.rs`, after the orphan-fs-error loop added in Task 9 Step 2, append:

```rust
    for ticket_id in &orphan_report.deleted {
        self.escalation.evict_ticket(ticket_id).await;
    }
```

- [ ] **Step 4: Write the e2e**

Create `crates/roki-daemon/tests/e2e/escalation_evicted_on_cleanup_smoke.rs`:

```rust
//! Cycle-bound escalation entries clear when the ticket reaches a terminal
//! state via cleanup-cycle.
//!
//! Setup:
//! 1. Configure `[[on_failure]]` so a rule failure triggers a handler that
//!    itself fails (a `false` cli line). This adds an escalation entry for
//!    the ticket.
//! 2. The same ticket then transitions to a status that matches
//!    `[[cleanup]]`, dispatching a cleanup cycle.
//! 3. Assert: after `cycle_completed kind=cleanup`, the snapshot of the
//!    queue (read via the test-only inspector wired below) contains no
//!    entry for that ticket.
//!
//! Because the HTTP API is deferred to a later slice, this test reaches
//! into the daemon-events JSONL to count escalation entries: it asserts the
//! exact sequence `escalation_added → cycle_completed kind=cleanup`, then
//! reads the daemon's process state via the `roki cleanup` invocation that
//! re-attaches to the same `<session_root>` and pushes a no-op rule. After
//! the cleanup cycle the queue is empty in-process, so a follow-up
//! `escalation_added` from a fresh failure on a different ticket appears
//! adjacent to no remaining T-1 entry. (The simpler form below uses the
//! eviction observability hook added in Task 10 Step 5.)

mod support;

use support::common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cycle_bound_entry_cleared_after_cleanup_terminal() {
    let fx = build_recursion_then_cleanup_fixture().await;
    fx.run_until_cleanup_terminal().await;

    let body = std::fs::read_to_string(&fx.daemon_events_path).unwrap();

    // Order: escalation_added → cycle_completed (kind=cleanup) for the
    // same ticket.
    let escalation_idx = body
        .lines()
        .position(|l| l.contains("\"event\":\"escalation_added\""))
        .expect("escalation_added must precede cleanup");
    let cleanup_idx = body
        .lines()
        .position(|l| {
            l.contains("\"event\":\"cycle_completed\"") && l.contains("\"cycle_kind\":\"cleanup\"")
        })
        .expect("cleanup must complete");
    assert!(escalation_idx < cleanup_idx, "escalation must precede cleanup_completed");

    // After eviction, a snapshot taken via the queue's test inspector is
    // empty (see runtime helper).
    let remaining = fx.escalation_snapshot().await;
    assert!(
        remaining.iter().all(|e| e.ticket_id.as_deref() != Some(&fx.ticket_id)),
        "no entry must remain for the evicted ticket: {remaining:?}"
    );
}
```

- [ ] **Step 5: Add the test-only `escalation_snapshot` hook**

The fixture needs a way to read the queue snapshot. The test fixture already constructs the daemon in-process for slice-6 e2e tests; mirror that approach.

In `crates/roki-daemon/tests/e2e/support/common.rs` (or wherever the `Fixture` lives in the e2e support tree — slice 6 likely placed it under `support/cold_start.rs`), add a method:

```rust
pub async fn escalation_snapshot(&self) -> Vec<roki_daemon::escalation::EscalationEntry> {
    self.escalation.snapshot().await
}
```

…where `Fixture::escalation` is an `Arc<EscalationQueue>` cloned from the runtime construction. If the fixture currently calls `runtime::run` (which boxes everything internally), expose a new test-only variant `runtime::run_with_handles` that returns an `Arc<EscalationQueue>` alongside the join handle. Implement it as a thin wrapper around `run_inner`.

A new helper signature:

```rust
// In runtime.rs
#[cfg(any(test, feature = "test-support"))]
pub async fn run_with_handles(
    config_path: &Path,
    mode: DispatchMode,
) -> Result<RuntimeHandles, SkeletonError> {
    // Same body as run_inner up to the point queue + dispatcher exist.
    // Spawn the same shutdown-block in a background task and return
    // the handles synchronously.
}

#[cfg(any(test, feature = "test-support"))]
pub struct RuntimeHandles {
    pub escalation: Arc<crate::escalation::EscalationQueue>,
    pub shutdown: ShutdownToken,
    pub join: tokio::task::JoinHandle<()>,
}
```

Add `test-support` to `Cargo.toml [features]` (no-op feature). The test fixture flips it on. If the existing slice-6 fixture already uses a similar pattern, mirror it; do not introduce a parallel mechanism.

- [ ] **Step 6: Add the `[[test]]` entry**

In `crates/roki-daemon/Cargo.toml`:

```toml
[[test]]
name = "escalation_evicted_on_cleanup_smoke"
path = "tests/e2e/escalation_evicted_on_cleanup_smoke.rs"
```

- [ ] **Step 7: Run the e2e**

Run: `cargo test -p roki-daemon --test escalation_evicted_on_cleanup_smoke`
Expected: 1 passed.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/daemon/dispatcher.rs \
        crates/roki-daemon/src/daemon/ticket_task.rs \
        crates/roki-daemon/src/daemon/cold_start.rs \
        crates/roki-daemon/src/runtime.rs \
        crates/roki-daemon/Cargo.toml \
        crates/roki-daemon/tests/e2e/support/ \
        crates/roki-daemon/tests/e2e/escalation_evicted_on_cleanup_smoke.rs
git commit -m "feat(escalation): wire eviction at admission-revoke, cleanup-terminal, orphan-delete"
```

---

## Task 11: Capacity overflow e2e

**Files:**
- Create: `crates/roki-daemon/tests/e2e/escalation_capacity_overflow_smoke.rs`

- [ ] **Step 1: Write the e2e**

Create `crates/roki-daemon/tests/e2e/escalation_capacity_overflow_smoke.rs`:

```rust
//! Capacity overflow drops the oldest entry. Configures
//! `[escalation].queue_size = 2`, then drives three handler-failure events
//! across three distinct tickets. The queue snapshot retains only the two
//! most recent entries. The first push's `escalation_added` is on disk
//! (the file is append-only) but the in-memory snapshot does not contain
//! the corresponding ticket.

mod support;

use support::common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overflow_drops_oldest_entry() {
    let fx = Fixture::builder()
        .with_escalation_queue_size(2)
        .with_recursion_handler() // [[on_failure]] that itself fails
        .build()
        .await;

    for n in 1..=3 {
        fx.send_recursion_failure(&format!("T-{n}")).await;
    }

    fx.wait_for_n_escalations(3).await;

    let snap = fx.escalation_snapshot().await;
    assert_eq!(snap.len(), 2, "queue cap=2: {snap:?}");
    let ids: Vec<_> = snap
        .iter()
        .filter_map(|e| e.ticket_id.as_deref())
        .collect();
    assert_eq!(ids, vec!["T-2", "T-3"], "oldest dropped");

    let body = std::fs::read_to_string(&fx.daemon_events_path).unwrap();
    let count = body
        .lines()
        .filter(|l| l.contains("\"event\":\"escalation_added\""))
        .count();
    assert_eq!(count, 3, "all three pushes still on disk");
}
```

The `Fixture::builder()` API is the slice-1-onward pattern; if your slice-6 fixture spells the builder differently (e.g. `support::cold_start::ColdStartFixture`), follow that name. The two new builder methods needed:
- `with_escalation_queue_size(u32)` — writes `[escalation]\nqueue_size = N` into the generated `roki.toml`.
- `with_recursion_handler()` — writes a `[[rule]]` whose run is `false` plus an `[[on_failure]]` entry whose handler `cmd` is also `false`, so the handler cycle itself fails.
- `send_recursion_failure(ticket_id)` / `wait_for_n_escalations(n)` — drive the daemon's webhook with a synthetic Linear payload assigning a new ticket id, then poll until the daemon-events file shows `n` escalation lines.

Search the slice-3 `recursion_bound_smoke.rs` (now deleted but recoverable from git) for the corresponding helper invocations and copy the patterns.

Add the `[[test]]` entry to `Cargo.toml`.

- [ ] **Step 2: Run the e2e**

Run: `cargo test -p roki-daemon --test escalation_capacity_overflow_smoke`
Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-daemon/tests/e2e/escalation_capacity_overflow_smoke.rs \
        crates/roki-daemon/Cargo.toml
git commit -m "test(escalation): capacity overflow drops oldest"
```

---

## Task 12: Remove unused `FailureMarker` variants

**Files:**
- Modify: `crates/roki-daemon/src/events.rs`

- [ ] **Step 1: Confirm no remaining references**

```bash
grep -rn "FailureMarker::RecursionBound\|FailureMarker::CleanupFsError" crates/roki-daemon/
```

Expected: no matches in `src/`. Tests in deleted e2e files no longer reference them.

- [ ] **Step 2: Trim the enum**

Edit `crates/roki-daemon/src/events.rs` lines 13–19:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureMarker {
    None,
}
```

If existing unit tests reference `FailureMarker::RecursionBound` or `FailureMarker::CleanupFsError`, delete those test cases — they covered behavior that no longer exists.

- [ ] **Step 3: Run all unit tests**

Run: `cargo test -p roki-daemon --lib`
Expected: all green.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/events.rs
git commit -m "refactor(events): drop unused FailureMarker variants"
```

---

## Task 13: Doc updates (`fr:08`, `fr:02`, `ref:log-events`, `ref:config`)

**Files:**
- Modify: `docs/fr/08-observability-logs.md`
- Modify: `docs/fr/02-configuration.md`
- Modify: `docs/reference/log-events.md`
- Modify: `docs/reference/config.md`

- [ ] **Step 1: Update `fr:08` line 66**

Find the `failure_unhandled` row in `docs/fr/08-observability-logs.md` (line 66). Replace exactly:

| Old | New |
|---|---|
| `` `failure_unhandled` `` | `` `A cycle failure was not recovered: no `[[on_failure]]` match (`marker = none`), handler cycle itself failed (`marker = recursion_bound`), or handler cycle hit an infra error (`marker = recursion_bound`). Carries `(ticket_id, cycle_id, cycle_kind, failure.kind, phase, error_text, marker)`. Daemon exits 1. No escalation queue entry ([06-failure-handling §Failure-handler cycle](06-failure-handling.md)).` `` | `` `A cycle failure with no `[[on_failure]]` match (`marker = none`). Carries `(ticket_id, cycle_id, cycle_kind, failure.kind, phase, error_text, marker)`. Daemon stays alive; the ticket task drops the cycle and waits for the next admission ([06-failure-handling §Failure-handler cycle](06-failure-handling.md)). Recursive failure-cycle failures and cleanup-time fs errors enter the escalation queue instead — see `escalation_added`.` ``

Use `Edit` with the full old paragraph as `old_string`.

- [ ] **Step 2: Update `fr:08` line 69 (`escalation_added` row)**

Replace the `escalation_added` row in `docs/fr/08-observability-logs.md` (line 69) with:

| `escalation_added` | Escalation queue entry added. Daemon-stuck failure: failure-handler cycle that itself failed, cleanup-time fs error, or daemon-internal error with no cycle association. Carries `(ticket_id?, cycle_id?, failure.kind, phase?, error_text)`. Cycle-less entries omit `ticket_id`, `cycle_id`, `phase` ([06-failure-handling §Escalation queue](06-failure-handling.md)) |

- [ ] **Step 3: Update `ref:log-events` line 43 (`failure_unhandled`)**

Same edit as Step 1 applied verbatim to `docs/reference/log-events.md` line 43.

- [ ] **Step 4: Update `ref:log-events` line 46 (`escalation_added`)**

Replace the existing one-liner with:

| `escalation_added` | Escalation queue entry added | Daemon-stuck failure: failure-handler cycle that itself failed, cleanup-time fs error, or daemon-internal error with no cycle association. Carries `(ticket_id?, cycle_id?, failure.kind, phase?, error_text)`. Cycle-less entries omit `ticket_id`, `cycle_id`, `phase` ([fr:06 §Escalation queue](../fr/06-failure-handling.md)) |

- [ ] **Step 5: Update `fr:02` example block**

In `docs/fr/02-configuration.md`, find the example `roki.toml` block that contains `[log]\nring_size = 1000`. Append:

```toml

[escalation]
queue_size = 64
```

- [ ] **Step 6: Update `ref:config` table**

In `docs/reference/config.md`, find the table row for `[log].ring_size` (line 49). Add a new row directly below it:

| `[escalation].queue_size` | no | int | `64` | `1..=1024` | [fr:06 §Escalation queue](../fr/06-failure-handling.md) |

- [ ] **Step 7: Verify cross-reference graph**

The post-edit hook auto-runs `kusara validate`. If it complains about a dangling reference, fix the offending citation.

```bash
git status
```

Confirm no unintended changes.

- [ ] **Step 8: Commit**

```bash
git add docs/fr/08-observability-logs.md \
        docs/fr/02-configuration.md \
        docs/reference/log-events.md \
        docs/reference/config.md
git commit -m "docs(fr,ref): drop 'Daemon exits 1' wording, narrow failure_unhandled to marker=none, add [escalation].queue_size"
```

---

## Task 14: Whole-suite green + lint

**Files:** none

- [ ] **Step 1: Run the full daemon test suite**

```bash
cargo test -p roki-daemon
```

Expected: 100% green, including every existing slice-1-through-slice-6 e2e.

- [ ] **Step 2: Run clippy**

```bash
cargo clippy -p roki-daemon --all-targets -- -D warnings
```

Expected: zero warnings.

- [ ] **Step 3: Run rustfmt**

```bash
cargo fmt --all
```

Expected: no diff. If there is a diff, commit it as a separate `chore(slice7): fmt` commit.

- [ ] **Step 4: Verify the kusara graph**

```bash
kusara validate
```

Expected: no dangling references. (The hook would have already caught this on each doc edit, but a final manual sweep guards against an edit that bypassed the hook.)

- [ ] **Step 5: Final commit, if any**

```bash
git status
git log --oneline slice7-escalation-queue-spec ^main
```

Expected log (in order, oldest-first):
```
docs(slice7): escalation queue design spec
feat(escalation): bounded ring with newest-wins overflow
feat(escalation): EscalationEntry with cycle-bound and cycle-less constructors
feat(events): add escalation_added event variant
feat(escalation): EscalationQueue with cycle-bound, cycle-less, evict, snapshot
feat(config): [escalation].queue_size with default 64, validation 1..=1024
feat(escalation): wire queue through runtime/dispatcher/ticket_task/cold_start
feat(escalation): route cleanup_fs_error to queue, skip [[on_failure]]
feat(escalation): route recursion paths to queue, narrow failure_unhandled to marker=none
feat(escalation): cold-start orphan reconcile fs errors push cycle-less entries
feat(escalation): wire eviction at admission-revoke, cleanup-terminal, orphan-delete
test(escalation): capacity overflow drops oldest
refactor(events): drop unused FailureMarker variants
docs(fr,ref): drop 'Daemon exits 1' wording, narrow failure_unhandled to marker=none, add [escalation].queue_size
```

If anything is missing, run the corresponding task's commit step.

---

## Spec coverage check

| Spec section | Task |
|---|---|
| §2.1 module layout — `escalation/{ring,entry,queue}.rs` | 1, 2, 4 |
| §2.1 — `events.rs` extension (`Event::EscalationAdded`) | 3 |
| §2.1 — `runtime.rs` wiring | 6 |
| §2.1 — `daemon/dispatcher.rs` queue handle + eviction | 6, 10 |
| §2.1 — `daemon/ticket_task.rs` queue handle + eviction | 6, 7, 10 |
| §2.1 — `daemon/real_runner.rs` recursion-path migration | 8 |
| §2.1 — `daemon/cold_start.rs` cycle-less push + eviction | 6, 9, 10 |
| §2.1 — `daemon/orphan.rs` surface fs errors | 9 |
| §2.1 — `engine/cleanup.rs` queue migration + skip-`[[on_failure]]` | 7 |
| §2.2 — `EscalationEntry` shape | 2 |
| §2.2 — `EscalationQueue::{new, push_cycle, push_daemon, evict_ticket, snapshot}` | 4 |
| §2.2 — `Ring<T>` newest-wins | 1 |
| §2.2 — `Event::EscalationAdded` JSON shape | 3 |
| §2.2 — `FailureMarker::None` only | 12 |
| §2.3 — boot-time wiring order | 6 |
| §2.4 — single-Mutex concurrency | 4 |
| §3.1 — recursion paths to queue | 8 |
| §3.2 — `cleanup_fs_error` to queue, skip `[[on_failure]]` | 7 |
| §3.3 — orphan reconcile cycle-less push | 9 |
| §3.4 — eviction at four sites | 10 |
| §3.5 — overflow drops oldest with warn log | 1, 4, 11 |
| §4 — `[escalation].queue_size` config | 5 |
| §5.1 — `escalation_added` JSON | 3 |
| §5.2 — `failure_unhandled` narrowing | 8, 12 |
| §6.1 — `fr:08` line 66 update | 13 |
| §6.2 — `ref:log-events` `escalation_added` row | 13 |
| §6.5 — `fr:02` + `ref:config` updates | 13 |
| §8 e2e — recursion | 8 |
| §8 e2e — cleanup_fs_error | 7 |
| §8 e2e — orphan reconcile | 9 |
| §8 e2e — eviction-on-cleanup | 10 |
| §8 e2e — capacity overflow | 11 |
| §8 e2e — `failure_unhandled_smoke`, `on_failure_smoke` updates | 8 |
| §9 implementation order | tasks 1→13 follow spec order |
