# Slice 12 Daemon Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the three remaining fr:12-daemon-lifecycle gaps: (1) refuse to start when `wt`/`ghq` are missing from `$PATH`; (2) actively SIGTERM in-flight state subprocesses on SIGINT/SIGTERM so they exit within `[engine].shutdown_window_seconds`; (3) replace the `aborted_ticket_ids` payload on `shutdown_window_exceeded` with a per-subprocess offender list (`ticket_id`, `cycle_id`, `state_id`, `visit`, `pid`).

**Architecture:** Three workstreams. (A) `daemon::deps::check()` runs after the per-daemon event writer is open and before `DaemonStarted`; failure emits `daemon_dependency_missing` and returns `SkeletonError::MissingDependency`. (B) `runtime::run` clones the existing `ShutdownToken` into a new `InflightRegistry` and into `RealCycleRunner` → `RealStateRunner`. `RealStateRunner::run_state` runs the existing stall watchdog with an additional `tokio::select!` arm on `ShutdownToken::wait()`; when it fires the runner SIGTERMs the live child (reusing `engine::stall::terminate_child_external`) and reaps. The registry tracks `(ticket, cycle, state, visit, pid)` per active subprocess. (C) At drain-deadline, runtime reads the registry, SIGKILLs each surviving pid, and emits `shutdown_window_exceeded` with `offenders`. Test fixtures are rewritten to match.

**Tech Stack:** Rust 2024 (workspace edition). New direct dep on the `which` crate. Existing deps reused: `tokio`, `nix`, `serde_json`, `serde`, `uuid`, `time`, `tempfile`.

**Spec:** `docs/superpowers/specs/2026-05-12-slice12-daemon-lifecycle-design.md` (HEAD as of plan-write: `d730c55`).

**Working branch:** `feature/slice12-daemon-lifecycle`. Branched from `feature/slice11-roki-cli` (where the spec landed) in Task 0.

---

## File Structure

### Created

| Path | Responsibility |
|---|---|
| `crates/roki-daemon/src/daemon/deps.rs` | `MissingDependency`, `check()` — probe `which::which("wt")` and `which::which("ghq")`. |
| `crates/roki-daemon/src/daemon/inflight.rs` | `Inflight`, `InflightRegistry` — process-wide map of (`ticket_id` → live subprocess descriptor) for shutdown offender collection. |
| `crates/roki-daemon/tests/e2e/daemon_dependency_missing_smoke.rs` | E2E: PATH stripped of `wt`/`ghq` → exit ≠ 0 + `daemon_dependency_missing` line + no `daemon_started`. |
| `crates/roki-daemon/tests/e2e/shutdown_clean_smoke.rs` | E2E: fast-exit state → SIGINT → `daemon_shutdown_completed { drained, aborted: 0 }` + no `shutdown_window_exceeded`. |
| `crates/roki-daemon/tests/e2e/shutdown_window_exceeded_smoke.rs` | E2E: long-sleeping state → SIGINT with 1 s window → `shutdown_window_exceeded { offenders: [{ ... }] }` + offender pid is dead by exit. |

### Modified

| Path | Change |
|---|---|
| `crates/roki-daemon/Cargo.toml` | Add `which = "..."` dep; add `[[test]]` entries for three new e2e files. |
| `crates/roki-daemon/src/daemon/mod.rs` | Declare `pub mod deps; pub mod inflight;`. |
| `crates/roki-daemon/src/error.rs` | Add `SkeletonError::MissingDependency { binaries: Vec<String> }`. |
| `crates/roki-daemon/src/events.rs` | Add `Event::DaemonDependencyMissing { ts, binary, remediation }`; add `ShutdownOffender` struct; replace `aborted_ticket_ids` field on `Event::ShutdownWindowExceeded` with `offenders: Vec<ShutdownOffender>`. Wire `kind_str`, `routing_keys`. |
| `crates/roki-daemon/src/engine/real_state_runner.rs` | Add `shutdown: ShutdownToken` + `inflight: Arc<InflightRegistry>` fields. Wrap the watchdog `run` with a `tokio::select!` arm against `shutdown.wait()` that calls `terminate_child_external` and falls through to reap. Register/clear in `InflightRegistry` around the child lifecycle. |
| `crates/roki-daemon/src/engine/stall.rs` | (Unchanged — `terminate_child_external` already exists.) |
| `crates/roki-daemon/src/daemon/real_runner.rs` | Plumb `shutdown` + `inflight` through `RealCycleRunner` into `RealStateRunner`. |
| `crates/roki-daemon/src/runtime.rs` | Insert dep check between event-writer open and `DaemonStarted`. Construct `InflightRegistry`; pass into `RealCycleRunner`. At drain-deadline, read registry → SIGKILL each surviving pid → emit revised `shutdown_window_exceeded`. |
| `crates/roki-daemon/src/cli/mod.rs` | Add the `cleanup_help_names_config_and_roki_toml` symmetric test. |
| `crates/roki-daemon/tests/e2e/persistent_sigint_timeout_smoke.rs` | Switch the offender assertion from `aborted_ticket_ids` to `offenders[].ticket_id`. |
| `docs/fr/12-daemon-lifecycle.md` | Name `daemon_dependency_missing` under "Missing dependency CLI"; cross-link `shutdown_window_exceeded`'s offender payload to ref:log-events. |
| `docs/reference/log-events.md` | Add `daemon_dependency_missing` row; revise the `shutdown_window_exceeded` "Carries" cell to "Per-offender `{ticket_id, cycle_id, state_id, visit, pid}`". |

---

## Cross-Task Conventions

- **Branch:** `feature/slice12-daemon-lifecycle` (created in Task 0). All commits land here. Push when each task closes.
- **Test command (per task):** `cargo test -p roki-daemon <test-name-substring>` — full suite (`cargo test -p roki-daemon`) at task end.
- **Build verification:** `cargo build -p roki-daemon` after each task. CI also runs `cargo clippy -p roki-daemon -- -D warnings` and `cargo fmt --check`.
- **Daemon-scoped events** continue to land in `<session_root>/_daemon.events.jsonl` via `EventWriter::open(session_root, "_daemon")`.
- **No new crates besides `which`.** `which` is the only addition. Anyone proposing more is wrong — re-read this line.
- **Module dead-code suppression:** new modules open with `#![allow(dead_code)]` until `runtime::run` consumes them, matching the slice 5 convention.
- **`ShutdownToken` semantics** (slice 5): cheap `Clone`, `fire()` is idempotent, `wait()` returns the moment the flag is set. The selector arm pattern in this slice is identical to the one used by `runtime::run`'s `signal_handle` block.
- **kill semantics:** the `terminate_child_external` helper in `engine::stall` already does SIGTERM → 5 s grace → SIGKILL → reap. Reuse it for shutdown-time termination so the SIGKILL escalation isn't re-implemented per call site.

---

## Task 0: Branch + verify spec commit (DONE up to plan-write)

**Status:** Spec is committed on `feature/slice11-roki-cli` at HEAD `d730c55`. Before starting Task 1, branch from there:

```bash
git fetch origin
git switch feature/slice11-roki-cli
git switch -c feature/slice12-daemon-lifecycle
git log --oneline -1   # expect d730c55 docs(slice12): design for daemon lifecycle gap-close
```

Push the branch only after Task 1 lands so reviewers see at least one implementation commit alongside the spec.

---

## Task 1: New event variants — `DaemonDependencyMissing` + `ShutdownOffender`

**Files:**
- Modify: `crates/roki-daemon/src/events.rs`

- [ ] **Step 1: Add the `DaemonDependencyMissing` variant + `ShutdownOffender` struct**

Open `crates/roki-daemon/src/events.rs`. Below the existing `pub enum FailureMarker` (around line 20), add:

```rust
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ShutdownOffender {
    pub ticket_id: String,
    pub cycle_id: String,           // UUID stringified; matches other variants
    pub state_id: String,
    pub visit: u32,
    pub pid: u32,
}
```

Then inside the `pub enum Event` (find the `ShutdownWindowExceeded { ... aborted_ticket_ids: Vec<String> }` variant, around line 169), replace it with:

```rust
ShutdownWindowExceeded {
    ts: String,
    aborted: usize,
    offenders: Vec<ShutdownOffender>,
},
```

Add the new event variant right below `ShutdownWindowExceeded`:

```rust
DaemonDependencyMissing {
    ts: String,
    binary: String,
    remediation: String,
},
```

- [ ] **Step 2: Wire `kind_str` and `routing_keys`**

In `impl Event { pub fn kind_str ... }` add the new arm:

```rust
Event::DaemonDependencyMissing { .. } => "daemon_dependency_missing",
```

In `pub fn routing_keys`, add:

```rust
Event::DaemonDependencyMissing { .. } => (None, None),
```

The `ShutdownWindowExceeded` arms keep `=> (None, None)` — no change.

- [ ] **Step 3: Update the existing serializer round-trip test**

`grep -n "shutdown_window_exceeded\|aborted_ticket_ids" crates/roki-daemon/src/events.rs` — locate the existing serialization test. Rewrite its `aborted_ticket_ids: vec!["ENG-1".into()]` line as:

```rust
offenders: vec![ShutdownOffender {
    ticket_id: "ENG-1".into(),
    cycle_id: "00000000-0000-0000-0000-000000000001".into(),
    state_id: "phase-1".into(),
    visit: 1,
    pid: 9999,
}],
```

And update the `assert!(s.contains(...))` lines to match the new payload (one assertion per scalar field, e.g. `assert!(s.contains("\"state_id\":\"phase-1\""))`).

- [ ] **Step 4: Add a new test for `daemon_dependency_missing`**

Append to the existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn daemon_dependency_missing_serializes() {
    let ev = Event::DaemonDependencyMissing {
        ts: "2026-05-12T00:00:00Z".into(),
        binary: "wt".into(),
        remediation: "install wt and put it on PATH".into(),
    };
    let s = serde_json::to_string(&ev).unwrap();
    assert!(s.contains("\"event\":\"daemon_dependency_missing\""));
    assert!(s.contains("\"binary\":\"wt\""));
    assert!(s.contains("\"remediation\":\"install wt and put it on PATH\""));
}
```

- [ ] **Step 5: Build + run the events tests**

```bash
cargo test -p roki-daemon events::
```

Expected: all events tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/events.rs
git commit -m "feat(events): add daemon_dependency_missing + ShutdownOffender payload"
```

---

## Task 2: `daemon::deps::check()` — startup PATH probe

**Files:**
- Create: `crates/roki-daemon/src/daemon/deps.rs`
- Modify: `crates/roki-daemon/src/daemon/mod.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add `which` to the daemon crate**

Open `crates/roki-daemon/Cargo.toml`, locate the `[dependencies]` section, and add (alphabetically, between `uuid` and the next entry if present):

```toml
which = "6"
```

- [ ] **Step 2: Add the module declaration**

In `crates/roki-daemon/src/daemon/mod.rs` (currently a one-line `pub mod ...;` list) add:

```rust
pub mod deps;
```

after the existing entries (alphabetical insertion is fine).

- [ ] **Step 3: Write the failing tests + implementation**

Create `crates/roki-daemon/src/daemon/deps.rs`:

```rust
#![allow(dead_code)]

//! Startup PATH probe for `wt` and `ghq` (fr:12 §Capabilities).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDependency {
    pub binary: &'static str,
    pub hint: &'static str,
}

const REQUIRED: &[(&str, &str)] = &[
    ("wt",  "install worktree manager 'wt' and put it on PATH"),
    ("ghq", "install ghq (https://github.com/x-motemen/ghq) and put it on PATH"),
];

pub fn check() -> Result<(), Vec<MissingDependency>> {
    check_with(|bin| which::which(bin).is_ok())
}

fn check_with<F: Fn(&str) -> bool>(found: F) -> Result<(), Vec<MissingDependency>> {
    let missing: Vec<MissingDependency> = REQUIRED
        .iter()
        .copied()
        .filter(|(bin, _)| !found(bin))
        .map(|(binary, hint)| MissingDependency { binary, hint })
        .collect();
    if missing.is_empty() { Ok(()) } else { Err(missing) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_present_returns_ok() {
        assert!(check_with(|_| true).is_ok());
    }

    #[test]
    fn only_wt_missing() {
        let res = check_with(|bin| bin != "wt");
        let missing = res.unwrap_err();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].binary, "wt");
        assert!(!missing[0].hint.is_empty());
    }

    #[test]
    fn only_ghq_missing() {
        let res = check_with(|bin| bin != "ghq");
        let missing = res.unwrap_err();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].binary, "ghq");
    }

    #[test]
    fn both_missing_lists_both_in_order() {
        let res = check_with(|_| false);
        let missing = res.unwrap_err();
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0].binary, "wt");
        assert_eq!(missing[1].binary, "ghq");
    }
}
```

- [ ] **Step 4: Run the new tests**

```bash
cargo test -p roki-daemon daemon::deps
```

Expected: 4 tests pass.

- [ ] **Step 5: Add the `SkeletonError::MissingDependency` variant**

Open `crates/roki-daemon/src/error.rs`. Find `pub enum SkeletonError` (its variants currently include `Config`, `Webhook`, `Capture`, `ShutdownWindowExceeded`, etc.). Add a new variant — placement just below `ShutdownWindowExceeded` keeps the file readable:

```rust
#[error("missing required CLI dependency: {}", binaries.join(", "))]
MissingDependency { binaries: Vec<String> },
```

- [ ] **Step 6: Build + lint**

```bash
cargo build -p roki-daemon
cargo clippy -p roki-daemon -- -D warnings
```

Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/Cargo.toml \
        crates/roki-daemon/src/daemon/mod.rs \
        crates/roki-daemon/src/daemon/deps.rs \
        crates/roki-daemon/src/error.rs
git commit -m "feat(daemon): startup PATH probe for wt/ghq"
```

---

## Task 3: Wire dep check into `runtime::run_inner`

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`

- [ ] **Step 1: Locate the insertion point**

Open `crates/roki-daemon/src/runtime.rs`. Find the block "5. Emit DaemonStarted" (around line 118). The dep check goes immediately above it — after the event writer is open (step 4) but before `DaemonStarted` lands.

- [ ] **Step 2: Insert the dep-check block**

Above the `// 5. Emit DaemonStarted.` comment, insert:

```rust
    // 4c. Dependency check (fr:12 §Capabilities). Runs after the event
    //     writer is open so the failure surfaces in `_daemon.events.jsonl`
    //     in addition to the tracing line.
    if let Err(missing) = crate::daemon::deps::check() {
        for m in &missing {
            let mut w = daemon_events.lock().await;
            let _ = w.emit(&Event::DaemonDependencyMissing {
                ts: now_rfc3339(),
                binary: m.binary.into(),
                remediation: m.hint.into(),
            });
            drop(w);
            tracing::error!(
                event_name = "daemon_dependency_missing",
                binary = m.binary,
                hint = m.hint,
                "missing required CLI dependency"
            );
        }
        return Err(SkeletonError::MissingDependency {
            binaries: missing.iter().map(|m| m.binary.to_string()).collect(),
        });
    }
```

- [ ] **Step 3: Add the matching unit test**

At the bottom of `crates/roki-daemon/src/runtime.rs`'s `mod tests`, append:

```rust
    #[tokio::test]
    async fn missing_dependency_short_circuits_before_daemon_started() {
        // Stub `PATH` so neither `wt` nor `ghq` resolves.
        let tmp_path = TempDir::new().unwrap();
        let original_path = std::env::var_os("PATH");
        // SAFETY: tests in this module are serial under tokio::test on a
        // single thread; the env mutation is reverted on drop below.
        unsafe { std::env::set_var("PATH", tmp_path.path()); }
        struct PathGuard(Option<std::ffi::OsString>);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe { std::env::set_var("PATH", v) },
                    None => unsafe { std::env::remove_var("PATH") },
                }
            }
        }
        let _guard = PathGuard(original_path);

        // Build a minimal valid roki.toml that still loads (config-load
        // happens before the dep check; the dep check is what must trip).
        let cfg_dir = TempDir::new().unwrap();
        let cfg_path = cfg_dir.path().join("roki.toml");
        std::fs::write(&cfg_path, MINIMAL_ROKI_TOML).unwrap();
        // The workflow path inside MINIMAL_ROKI_TOML must point at a real
        // WORKFLOW.yaml; reuse the fixture from the existing skeleton smoke
        // test or write a stub here.

        match run_inner(&cfg_path, DispatchMode::Default).await {
            Err(SkeletonError::MissingDependency { binaries }) => {
                assert!(binaries.iter().any(|b| b == "wt"));
                assert!(binaries.iter().any(|b| b == "ghq"));
            }
            other => panic!("expected MissingDependency, got {other:?}"),
        }
    }

    // Minimal roki.toml + workflow stub literal. Borrow the strings from
    // `tests/e2e/support/fixtures.rs` if they already exist; otherwise add
    // them inline so this test is self-contained.
    const MINIMAL_ROKI_TOML: &str = r#"
# minimal roki.toml for dep-check test
[linear]
token = "xxx"
# ...
"#;
```

If the existing test-fixtures module already exposes a `minimal_roki_toml` helper, use it instead of the inline literal. Add a `#[ignore]` only if the workflow path cannot be plumbed cleanly in a unit test — the e2e in Task 8 is the load-bearing coverage. If the inline literal proves brittle, replace the unit test with a comment pointing at the e2e.

- [ ] **Step 4: Build + run**

```bash
cargo test -p roki-daemon missing_dependency
```

Expected: pass. If the inline literal breaks because of unrelated config schema, skip it and delete the test — the e2e in Task 8 is the contractual check.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs
git commit -m "feat(daemon): refuse start when wt/ghq missing from PATH"
```

---

## Task 4: `daemon::inflight` — live-subprocess registry

**Files:**
- Create: `crates/roki-daemon/src/daemon/inflight.rs`
- Modify: `crates/roki-daemon/src/daemon/mod.rs`

- [ ] **Step 1: Add the module declaration**

In `crates/roki-daemon/src/daemon/mod.rs` add:

```rust
pub mod inflight;
```

- [ ] **Step 2: Write the failing tests + implementation**

Create `crates/roki-daemon/src/daemon/inflight.rs`:

```rust
#![allow(dead_code)]

//! Live-subprocess registry consulted at drain time.
//!
//! `RealStateRunner::run_state` registers right after `Command::spawn` and
//! deregisters right after `child.wait()` reaps. The shutdown drain reads
//! the registry at the cumulative shutdown deadline to populate
//! `Event::ShutdownWindowExceeded.offenders`.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inflight {
    pub ticket_id: String,
    pub cycle_id: Uuid,
    pub state_id: String,
    pub visit: u32,
    pub pid: u32,
}

#[derive(Default, Clone)]
pub struct InflightRegistry {
    inner: Arc<Mutex<HashMap<String, Inflight>>>,
}

impl InflightRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, info: Inflight) {
        let mut g = self.inner.lock().await;
        g.insert(info.ticket_id.clone(), info);
    }

    pub async fn clear(&self, ticket_id: &str) {
        let mut g = self.inner.lock().await;
        g.remove(ticket_id);
    }

    pub async fn snapshot(&self) -> Vec<Inflight> {
        let g = self.inner.lock().await;
        g.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ticket: &str, pid: u32) -> Inflight {
        Inflight {
            ticket_id: ticket.into(),
            cycle_id: Uuid::nil(),
            state_id: "phase-1".into(),
            visit: 1,
            pid,
        }
    }

    #[tokio::test]
    async fn register_then_snapshot_includes_entry() {
        let reg = InflightRegistry::new();
        reg.register(sample("ENG-1", 1234)).await;
        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].pid, 1234);
    }

    #[tokio::test]
    async fn clear_removes_entry_keyed_by_ticket() {
        let reg = InflightRegistry::new();
        reg.register(sample("ENG-1", 1234)).await;
        reg.clear("ENG-1").await;
        assert!(reg.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn second_register_for_same_ticket_replaces_first() {
        let reg = InflightRegistry::new();
        reg.register(sample("ENG-1", 1)).await;
        reg.register(sample("ENG-1", 2)).await;
        let snap = reg.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].pid, 2);
    }
}
```

- [ ] **Step 3: Run the new tests**

```bash
cargo test -p roki-daemon daemon::inflight
```

Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/daemon/mod.rs \
        crates/roki-daemon/src/daemon/inflight.rs
git commit -m "feat(daemon): InflightRegistry for shutdown offender collection"
```

---

## Task 5: Thread `ShutdownToken` + `InflightRegistry` into `RealStateRunner`

**Files:**
- Modify: `crates/roki-daemon/src/engine/real_state_runner.rs`
- Modify: `crates/roki-daemon/src/daemon/real_runner.rs`

- [ ] **Step 1: Add the new fields to `RealStateRunner`**

Open `crates/roki-daemon/src/engine/real_state_runner.rs`. Edit the struct at line 36:

```rust
use std::sync::Arc;
use crate::daemon::inflight::{Inflight, InflightRegistry};
use crate::daemon::shutdown::ShutdownToken;
```

(Add these `use` lines near the top of the existing imports.)

Then extend the struct:

```rust
pub struct RealStateRunner {
    pub default_cli: String,
    pub default_stall_seconds: u32,
    pub ticket_id: String,
    pub ghq: String,
    pub session_root: PathBuf,
    pub session_tempdir: PathBuf,
    pub cycle_id: Uuid,
    /// Fires on SIGINT / SIGTERM. The runner SIGTERMs the live child when
    /// this becomes ready and reaps normally afterward.
    pub shutdown: ShutdownToken,
    /// Process-wide live-subprocess registry. The runner registers right
    /// after spawn and clears right after reap.
    pub inflight: Arc<InflightRegistry>,
}
```

- [ ] **Step 2: Locate the spawn + watchdog block**

Find the spawn site (currently lines 184–253):

```rust
let mut child = match cmd.spawn() {
    ...
};
// 12. Write stdin once if needed.
...
// 13. Drain stdout (tee to file + scan for terminal) and stderr.
let stdout_pipe = child.stdout.take().expect("piped");
...
// 14. Watchdog runs to completion (Healthy or StalledThenTerminated).
let stall_outcome = watchdog.run(&mut child).await;
```

The new register/clear and shutdown-aware watchdog wrap go around this block.

- [ ] **Step 3: Register the inflight entry right after spawn**

Immediately after the `let mut child = match cmd.spawn() { ... };` block (currently line 213) insert:

```rust
        let pid = child.id().unwrap_or(0);
        self.inflight
            .register(Inflight {
                ticket_id: self.ticket_id.clone(),
                cycle_id: self.cycle_id,
                state_id: state.id.clone(),
                visit: visit_n,
                pid,
            })
            .await;
```

- [ ] **Step 4: Shutdown-aware watchdog wrap**

Replace the existing `let stall_outcome = watchdog.run(&mut child).await;` line with:

```rust
        // Watchdog runs in parallel with a shutdown observer. On shutdown
        // fire we SIGTERM the live child (reusing `engine::stall::
        // terminate_child_external`, which already does TERM → 5 s grace
        // → KILL → reap). The watchdog's `Healthy` short-circuit covers
        // the race where the child exits cleanly before SIGTERM lands.
        let stall_outcome = {
            let shutdown = self.shutdown.clone();
            tokio::select! {
                biased;
                outcome = watchdog.run(&mut child) => outcome,
                _ = shutdown.wait() => {
                    crate::engine::stall::terminate_child_external(&mut child).await;
                    // Treat as "Healthy" for the wait-and-reap path below;
                    // the resulting exit_status is a signal kill, which
                    // step 17 already classifies as `ProcessCrash`.
                    StallOutcome::Healthy
                }
            }
        };
```

- [ ] **Step 5: Clear the inflight entry on reap**

After the existing `let exit_status = match child.wait().await { ... };` block (currently around line 266) insert:

```rust
        self.inflight.clear(&self.ticket_id).await;
```

This call is unconditional. If the child was already reaped by `terminate_child_external` above, `child.wait()` returns the cached status — the clear still runs.

- [ ] **Step 6: Plumb the new fields through `RealCycleRunner`**

Open `crates/roki-daemon/src/daemon/real_runner.rs`. Locate the `RealCycleRunner` struct + its `run_cycle` body. Add `shutdown: ShutdownToken` and `inflight: Arc<InflightRegistry>` fields:

```rust
pub struct RealCycleRunner {
    pub workflow: Arc<WorkflowConfig>,
    pub cfg: Arc<RokiConfig>,
    pub escalation: Arc<crate::escalation::EscalationQueue>,
    pub shutdown: crate::daemon::shutdown::ShutdownToken,
    pub inflight: Arc<crate::daemon::inflight::InflightRegistry>,
}
```

Inside the body where `RealStateRunner` is constructed (search for `RealStateRunner {`), add:

```rust
            shutdown: self.shutdown.clone(),
            inflight: self.inflight.clone(),
```

to the struct literal.

- [ ] **Step 7: Build**

```bash
cargo build -p roki-daemon
```

Fix any unused-import / unused-variable warnings caused by `Arc` / `ShutdownToken` imports landing in files that didn't have them.

- [ ] **Step 8: Verify existing engine tests still pass**

```bash
cargo test -p roki-daemon engine::real_state_runner
```

Existing tests construct `RealStateRunner` directly. Add the two new fields to those test-site struct literals: `shutdown: ShutdownToken::new(), inflight: Arc::new(InflightRegistry::new())`. Tests should pass without behavioral change because the watchdog `select!` arm prefers the watchdog outcome when shutdown is unset.

- [ ] **Step 9: Commit**

```bash
git add crates/roki-daemon/src/engine/real_state_runner.rs \
        crates/roki-daemon/src/daemon/real_runner.rs
git commit -m "feat(engine): SIGTERM in-flight state subprocess on daemon shutdown"
```

---

## Task 6: Add the `RealStateRunner` shutdown unit test

**Files:**
- Modify: `crates/roki-daemon/src/engine/real_state_runner.rs` (tests at the bottom)

- [ ] **Step 1: Write the failing test**

In `crates/roki-daemon/src/engine/real_state_runner.rs`, find the existing `#[cfg(test)] mod tests` block (around line 656+). Append:

```rust
    #[tokio::test]
    async fn shutdown_terminates_running_state_subprocess() {
        // Build a RealStateRunner that runs `sleep 30` via a `run:` state.
        // Fire ShutdownToken from a side task; assert the runner returns a
        // ProcessCrash outcome within ~2 s (well under the 30 s sleep) and
        // that the inflight registry has been cleared by reap.
        use crate::daemon::inflight::InflightRegistry;
        use crate::daemon::shutdown::ShutdownToken;

        let tmp = tempfile::TempDir::new().unwrap();
        let session_root = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_root).unwrap();
        let session_tempdir = tmp.path().join("tempdir-ENG-1");
        std::fs::create_dir_all(session_tempdir.join("directives")).unwrap();

        let shutdown = ShutdownToken::new();
        let inflight = std::sync::Arc::new(InflightRegistry::new());

        let runner = RealStateRunner {
            default_cli: "sh -lc".into(),
            default_stall_seconds: 60,
            ticket_id: "ENG-1".into(),
            ghq: "github.com/test/repo".into(),
            session_root: session_root.clone(),
            session_tempdir: session_tempdir.clone(),
            cycle_id: uuid::Uuid::new_v4(),
            shutdown: shutdown.clone(),
            inflight: inflight.clone(),
        };

        let state = run_state_with_cmd("sleep 30");
        let ctx = test_ctx_for(&state);

        let shutdown_fire = shutdown.clone();
        let fire = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            shutdown_fire.fire();
        });

        let start = std::time::Instant::now();
        let outcome = runner.run_state(&state, &ctx).await;
        fire.await.unwrap();

        assert!(
            start.elapsed() < std::time::Duration::from_secs(15),
            "shutdown did not terminate the child quickly: {:?}",
            start.elapsed()
        );
        match outcome {
            StateOutcome::Failure { kind, .. } => {
                assert!(matches!(kind, crate::engine::outcome::FailureKind::ProcessCrash));
            }
            other => panic!("expected Failure(ProcessCrash), got {other:?}"),
        }
        assert!(inflight.snapshot().await.is_empty(), "registry not cleared");
    }
```

`test_ctx_for` and `run_state_with_cmd` exist in this module already (search the file). If `test_ctx_for` does not exist, reuse the pattern from the closest neighboring test in the file; if no equivalent exists, replace this test with the slightly larger e2e in Task 8 and delete the unit test.

- [ ] **Step 2: Run the test**

```bash
cargo test -p roki-daemon shutdown_terminates_running_state_subprocess
```

Expected: pass within a few seconds (`sleep 30` is killed by SIGTERM ~200 ms after start).

- [ ] **Step 3: Commit**

```bash
git add crates/roki-daemon/src/engine/real_state_runner.rs
git commit -m "test(engine): RealStateRunner SIGTERMs child on shutdown"
```

---

## Task 7: Runtime drain — registry → offenders → SIGKILL survivors

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`

- [ ] **Step 1: Construct the `InflightRegistry` and pass it into the runner**

Open `crates/roki-daemon/src/runtime.rs`. Above the existing `let runner = Arc::new(RealCycleRunner { ... });` block (around line 167), insert:

```rust
    let inflight = Arc::new(crate::daemon::inflight::InflightRegistry::new());
```

Then add the two new fields to the `RealCycleRunner { ... }` literal:

```rust
        shutdown: shutdown.clone(),
        inflight: inflight.clone(),
```

- [ ] **Step 2: Replace the offender-collection logic in the drain loop**

Locate the drain block beginning `// 16. Drain ticket tasks within the configured window.` (around line 384). The current implementation collects `aborted_ticket_ids: Vec<String>` by tracking which ticket tasks timed out. Rewrite:

Replace this block (currently lines 396–429):

```rust
    let mut drained: usize = 0;
    let mut aborted_ticket_ids: Vec<String> = Vec::new();

    for (ticket_id, handle) in entries {
        let crate::daemon::dispatcher::TicketHandle { inbox, join } = handle;

        let _ = inbox.send(DispatchMsg::Shutdown).await;
        drop(inbox);

        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, join).await {
            Ok(Ok(())) => {
                drained += 1;
            }
            Ok(Err(join_err)) => {
                tracing::error!(
                    ticket_id = %ticket_id,
                    error = %join_err,
                    "ticket task join error during shutdown"
                );
                aborted_ticket_ids.push(ticket_id);
            }
            Err(_) => {
                aborted_ticket_ids.push(ticket_id);
            }
        }
    }

    let aborted = aborted_ticket_ids.len();
```

with:

```rust
    let mut drained: usize = 0;

    for (ticket_id, handle) in entries {
        let crate::daemon::dispatcher::TicketHandle { inbox, join } = handle;

        let _ = inbox.send(DispatchMsg::Shutdown).await;
        drop(inbox);

        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, join).await {
            Ok(Ok(())) => {
                drained += 1;
            }
            Ok(Err(join_err)) => {
                // Task panicked. Don't classify here; the registry check
                // below decides whether the child is still alive.
                tracing::error!(
                    ticket_id = %ticket_id,
                    error = %join_err,
                    "ticket task join error during shutdown"
                );
            }
            Err(_) => {
                // Timed out — drain windowexpired before the task joined.
            }
        }
    }

    // Read the live-subprocess registry. Anything still here at deadline
    // is an offender that did not honour SIGTERM within the window.
    let mut offenders_raw = inflight.snapshot().await;
    // Sort by ticket_id for a stable event payload.
    offenders_raw.sort_by(|a, b| a.ticket_id.cmp(&b.ticket_id));

    // SIGKILL each surviving pid. Ignore ESRCH (already-exited race).
    for off in &offenders_raw {
        if off.pid != 0 {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(off.pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }

    let offenders: Vec<crate::events::ShutdownOffender> = offenders_raw
        .into_iter()
        .map(|off| crate::events::ShutdownOffender {
            ticket_id: off.ticket_id,
            cycle_id: off.cycle_id.to_string(),
            state_id: off.state_id,
            visit: off.visit,
            pid: off.pid,
        })
        .collect();

    let aborted = offenders.len();
```

- [ ] **Step 3: Update the `Event::DaemonShutdownCompleted` + `Event::ShutdownWindowExceeded` emit sites**

Inside the same drain block, the `DaemonShutdownCompleted` emit already uses `drained` and `aborted` — no change needed.

Find the `Event::ShutdownWindowExceeded { ... aborted_ticket_ids }` emit (currently around line 458) and replace `aborted_ticket_ids` with `offenders`:

```rust
        let _ = w.emit(&Event::ShutdownWindowExceeded {
            ts: now_rfc3339(),
            aborted,
            offenders,
        });
```

- [ ] **Step 4: Build**

```bash
cargo build -p roki-daemon
```

Expected: compiles. Address any drift from removed `aborted_ticket_ids` references.

- [ ] **Step 5: Rewrite the existing e2e assertion**

Open `crates/roki-daemon/tests/e2e/persistent_sigint_timeout_smoke.rs`. Find the lines that parse `aborted_ticket_ids` (around lines 192–202) and replace with the offender-list traversal:

```rust
    let offenders = exceeded_event["offenders"]
        .as_array()
        .unwrap_or_else(|| panic!("missing offenders in shutdown_window_exceeded: {exceeded_event}"));
    let ticket_ids: Vec<&str> = offenders
        .iter()
        .filter_map(|o| o["ticket_id"].as_str())
        .collect();
    assert!(
        ticket_ids.iter().any(|id| *id == "ENG-100"),
        "offenders[].ticket_id must contain \"ENG-100\"; got: {exceeded_event}"
    );
```

Also update the file-level doc comment at the top (line 14) so it reads `offenders[].ticket_id` instead of `aborted_ticket_ids`.

- [ ] **Step 6: Build + run only this e2e**

```bash
cargo test -p roki-daemon --test persistent_sigint_timeout_smoke
```

Expected: pass.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs \
        crates/roki-daemon/tests/e2e/persistent_sigint_timeout_smoke.rs
git commit -m "feat(runtime): emit shutdown_window_exceeded.offenders from InflightRegistry"
```

---

## Task 8: New e2e — `daemon_dependency_missing_smoke`

**Files:**
- Create: `crates/roki-daemon/tests/e2e/daemon_dependency_missing_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml` (add the `[[test]]` entry)

- [ ] **Step 1: Add the test entry to `Cargo.toml`**

Open `crates/roki-daemon/Cargo.toml` and append (in the `[[test]]` block region):

```toml
[[test]]
name = "daemon_dependency_missing_smoke"
path = "tests/e2e/daemon_dependency_missing_smoke.rs"
```

- [ ] **Step 2: Create the e2e fixture**

Locate an existing e2e file that already sets up a minimal `roki.toml` (e.g. `tests/e2e/persistent_sigint_drain_smoke.rs` or a sibling). Mirror its fixture helpers. Skeleton:

```rust
//! fr:12 §"Missing dependency CLI": daemon refuses to start when `wt` or
//! `ghq` is absent from PATH. Confirms the structured event and the
//! non-zero exit.

use std::process::Command;
use tempfile::TempDir;

mod support;
use support::fixtures;

#[test]
fn missing_wt_and_ghq_aborts_before_daemon_started() {
    let tmp = TempDir::new().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    // Stub PATH with a directory that contains neither `wt` nor `ghq`.
    let empty_bin = tmp.path().join("empty-bin");
    std::fs::create_dir_all(&empty_bin).unwrap();

    let cfg_path = fixtures::write_minimal_roki_toml(tmp.path(), &session_root);
    let bin = env!("CARGO_BIN_EXE_roki");

    let out = Command::new(bin)
        .arg("run")
        .args(["--config", cfg_path.to_str().unwrap()])
        .env("PATH", &empty_bin)
        .output()
        .expect("spawn roki");

    assert!(!out.status.success(), "expected non-zero exit, got {}", out.status);

    // The daemon writer lives at <session_root>/_daemon.events.jsonl.
    let log = session_root.join("_daemon.events.jsonl");
    let body = std::fs::read_to_string(&log).expect("daemon event log exists");
    let lines: Vec<&str> = body.lines().collect();

    let dep_lines: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("\"event\":\"daemon_dependency_missing\""))
        .collect();
    assert!(
        dep_lines.iter().any(|l| l.contains("\"binary\":\"wt\"")),
        "missing wt dep line: {body}"
    );
    assert!(
        dep_lines.iter().any(|l| l.contains("\"binary\":\"ghq\"")),
        "missing ghq dep line: {body}"
    );

    assert!(
        !lines.iter().any(|l| l.contains("\"event\":\"daemon_started\"")),
        "daemon_started must not appear when deps are missing: {body}"
    );
}
```

If `fixtures::write_minimal_roki_toml` does not exist, add it in `tests/e2e/support/fixtures.rs` writing a valid `roki.toml` whose only requirement is that `RokiConfig::load` + `WorkflowConfig::load` succeed (point `workflow` at a stub written into `tmp`). Reuse whatever helper the slice 5 / 6 e2e suite already uses for "boot the binary".

- [ ] **Step 3: Run**

```bash
cargo test -p roki-daemon --test daemon_dependency_missing_smoke
```

Expected: pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/Cargo.toml \
        crates/roki-daemon/tests/e2e/daemon_dependency_missing_smoke.rs \
        crates/roki-daemon/tests/e2e/support/fixtures.rs  # if you added a helper
git commit -m "test(e2e): daemon refuses start when wt/ghq missing"
```

---

## Task 9: New e2e — `shutdown_clean_smoke`

**Files:**
- Create: `crates/roki-daemon/tests/e2e/shutdown_clean_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add the test entry**

```toml
[[test]]
name = "shutdown_clean_smoke"
path = "tests/e2e/shutdown_clean_smoke.rs"
```

- [ ] **Step 2: Write the fixture**

```rust
//! fr:12 §Normal shutdown happy path: a fast-completing state drains
//! cleanly within the shutdown window; `shutdown_window_exceeded` must
//! NOT appear and `daemon_shutdown_completed.aborted` must be 0.

mod support;
use support::persistent;

#[tokio::test]
async fn fast_state_drains_within_window() {
    // Reuse the slice 5 helper that wires a daemon with a one-state
    // workflow whose `run:` is `true` (returns 0 immediately), posts a
    // webhook, waits for `state_started`, then sends SIGTERM and reads
    // the daemon event log.
    let outcome = persistent::run_and_signal_after_state_started(
        /* workflow body */ "run: true",
        /* shutdown_window_seconds */ 30,
    )
    .await;

    assert!(
        outcome.shutdown_completed_aborted == 0,
        "aborted should be 0; got {}, log: {}",
        outcome.shutdown_completed_aborted,
        outcome.log
    );
    assert!(
        !outcome.log.contains("\"event\":\"shutdown_window_exceeded\""),
        "shutdown_window_exceeded must not appear: {}",
        outcome.log
    );
}
```

`persistent::run_and_signal_after_state_started` and `outcome.shutdown_completed_aborted` are conventions from `tests/e2e/support/persistent.rs`. If they do not already expose that shape, extend the helper module rather than inlining the spawn / signal logic.

- [ ] **Step 3: Run**

```bash
cargo test -p roki-daemon --test shutdown_clean_smoke
```

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/Cargo.toml \
        crates/roki-daemon/tests/e2e/shutdown_clean_smoke.rs \
        crates/roki-daemon/tests/e2e/support/persistent.rs  # if extended
git commit -m "test(e2e): clean shutdown emits aborted=0 + no window_exceeded"
```

---

## Task 10: New e2e — `shutdown_window_exceeded_smoke`

**Files:**
- Create: `crates/roki-daemon/tests/e2e/shutdown_window_exceeded_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add the test entry**

```toml
[[test]]
name = "shutdown_window_exceeded_smoke"
path = "tests/e2e/shutdown_window_exceeded_smoke.rs"
```

- [ ] **Step 2: Write the fixture**

```rust
//! fr:12 §Normal shutdown bullet 3: a state that refuses to honour
//! SIGTERM within `[engine].shutdown_window_seconds` produces a
//! `shutdown_window_exceeded` event with one offender entry, and the
//! offender pid is dead by the time the daemon exits.

mod support;
use support::persistent;

#[tokio::test]
async fn slow_state_offender_listed_and_killed() {
    // A state that traps SIGTERM and keeps sleeping. The shutdown loop
    // SIGKILLs it after the 1 s window. We assert: the event payload
    // names the state, and the recorded pid is no longer alive when the
    // daemon exits.
    let trap = r#"run: |
  trap '' TERM
  sleep 30
"#;
    let outcome = persistent::run_and_signal_after_state_started(
        trap,
        /* shutdown_window_seconds */ 1,
    )
    .await;

    let line = outcome
        .log
        .lines()
        .find(|l| l.contains("\"event\":\"shutdown_window_exceeded\""))
        .unwrap_or_else(|| panic!("no shutdown_window_exceeded: {}", outcome.log));
    let ev: serde_json::Value = serde_json::from_str(line).unwrap();
    let offenders = ev["offenders"].as_array().expect("offenders array");
    assert_eq!(offenders.len(), 1, "exactly one offender expected: {ev}");
    let pid = offenders[0]["pid"].as_u64().unwrap() as i32;
    assert!(pid > 0, "pid must be set: {ev}");

    // `kill -0` succeeds when the process exists; ESRCH otherwise. We
    // expect ESRCH — the daemon's SIGKILL fired.
    let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
    assert!(!alive, "offender pid {pid} should be dead at daemon exit");
}
```

The `trap '' TERM` line makes the state immune to SIGTERM, forcing the SIGKILL path on the daemon side. `sleep 30` keeps the process alive until then.

- [ ] **Step 3: Run**

```bash
cargo test -p roki-daemon --test shutdown_window_exceeded_smoke
```

Expected: pass within ~2 s (1 s window + drain + assert).

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/Cargo.toml \
        crates/roki-daemon/tests/e2e/shutdown_window_exceeded_smoke.rs
git commit -m "test(e2e): shutdown_window_exceeded names offender and SIGKILLs pid"
```

---

## Task 11: `cleanup --help` symmetric parity test

**Files:**
- Modify: `crates/roki-daemon/src/cli/mod.rs`

- [ ] **Step 1: Add the symmetric test**

Inside the existing `#[cfg(test)] mod tests` block in `crates/roki-daemon/src/cli/mod.rs`, append:

```rust
    #[test]
    fn cleanup_help_names_config_and_roki_toml() {
        let cli = Cli::command();
        let cleanup = cli
            .find_subcommand("cleanup")
            .expect("cleanup subcommand exists");
        let help = cleanup.clone().render_help().to_string();
        assert!(
            help.contains("--config"),
            "cleanup help missing --config: {help}"
        );
        assert!(
            help.contains("roki.toml"),
            "cleanup help should mention roki.toml: {help}"
        );
    }
```

- [ ] **Step 2: Run**

```bash
cargo test -p roki-daemon cli::tests::cleanup_help_names_config_and_roki_toml
```

Expected: pass.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-daemon/src/cli/mod.rs
git commit -m "test(cli): cleanup --help names --config and roki.toml"
```

---

## Task 12: Reference + FR doc updates

**Files:**
- Modify: `docs/reference/log-events.md`
- Modify: `docs/fr/12-daemon-lifecycle.md`

- [ ] **Step 1: Add the `daemon_dependency_missing` row to log-events.md**

Open `docs/reference/log-events.md`. Locate the "Daemon lifecycle" table (around line 98–106). Add this row directly above `shutdown_window_exceeded`:

```markdown
| `daemon_dependency_missing` | Startup; `wt` or `ghq` absent from `$PATH` | `binary`, `remediation` |
```

Then revise the `shutdown_window_exceeded` row's "Carries" cell:

```markdown
| `shutdown_window_exceeded` | Warn-severity event when one or more in-flight subprocesses failed to drain inside the shutdown window | `aborted: usize`, `offenders: [{ticket_id, cycle_id, state_id, visit, pid}]` |
```

- [ ] **Step 2: Update fr:12 to name the new event**

Open `docs/fr/12-daemon-lifecycle.md`. Locate the bullet beginning "**Missing dependency CLI**" (around line 41). Append one sentence:

```markdown
The refusal emits `daemon_dependency_missing` ([ref:log-events](../reference/log-events.md)) for each missing binary before the daemon exits.
```

Locate the `### Normal shutdown` step 3 (around line 51) and update the in-line reference so it points readers at the offender payload in ref:log-events. The existing wording already names "shutdown_window_exceeded" — adjust the parenthetical to read:

```markdown
3. Waits up to that window for each subprocess to exit. Subprocesses still alive at the end of the window are SIGKILLed and the daemon emits `shutdown_window_exceeded` ([ref:log-events](../reference/log-events.md)) naming each offending subprocess (`offenders[].{ticket_id, cycle_id, state_id, visit, pid}`).
```

- [ ] **Step 3: Run the kusara validator**

The post-edit hook auto-runs `kusara validate` after every `.md` save. If it doesn't run in your environment:

```bash
kusara validate
```

Expected: clean. (No `refs:` field changes are needed; both files already carry the right frontmatter.)

- [ ] **Step 4: Commit**

```bash
git add docs/reference/log-events.md docs/fr/12-daemon-lifecycle.md
git commit -m "docs(slice12): document daemon_dependency_missing + offender payload"
```

---

## Task 13: Full-suite green run + slice-1..11 regression sweep

**Files:** none

- [ ] **Step 1: Run the full daemon test suite**

```bash
cargo test -p roki-daemon
```

Expected: all unit + e2e tests pass. If a slice-1..11 e2e file fails due to a payload that referenced `aborted_ticket_ids`, only `persistent_sigint_timeout_smoke.rs` should be affected — Task 7 already rewrote that one. Any other failure points at a `grep`-able stale assertion; fix it inline.

- [ ] **Step 2: Lint + format**

```bash
cargo clippy -p roki-daemon -- -D warnings
cargo fmt --check
```

- [ ] **Step 3: Smoke the binary by hand**

```bash
PATH=/nonexistent ./target/debug/roki run --config /tmp/does-not-exist.toml || echo "exit $?"
```

Expected behavior:
- Exit ≠ 0.
- Tracing line `missing required CLI dependency` for each of `wt`, `ghq` (if the config-load step doesn't trip first; if it does, that's the unrelated config-not-found path and is fine — the dep-check e2e in Task 8 has the contractual coverage).

- [ ] **Step 4: Commit any cleanup that landed**

```bash
git status --short
# If any unrelated drift surfaced, commit it as a separate "chore" commit.
```

- [ ] **Step 5: Push the branch**

```bash
git push -u origin feature/slice12-daemon-lifecycle
```

---

## Task 14: Cross-task self-review

**Files:** none (review only — fix inline if anything is off)

- [ ] **Step 1: Spec coverage check**

For each spec section, point at the implementing task:
- §3 Startup dependency check → Tasks 1, 2, 3, 8.
- §4 Active SIGTERM on graceful shutdown → Tasks 4, 5, 6, 7, 9, 10.
- §4.4 ShutdownWindowExceeded payload change → Task 1 (variant) + Task 7 (emit) + Task 10 (assertion).
- §5 Help-text parity → Task 11.
- §7 Logging → Tasks 1, 12.
- §8 Tests → Tasks 6, 8, 9, 10, 11.
- §9 Spec impact → Task 12.

- [ ] **Step 2: Stale-reference scan**

```bash
grep -rn "aborted_ticket_ids" crates/ docs/
```

Expected: zero hits (or only in this plan file).

- [ ] **Step 3: Confirm `daemon_dependency_missing` is grep-clean**

```bash
grep -rn "daemon_dependency_missing" crates/ docs/
```

Expected hits: `events.rs` (variant + `kind_str`), `runtime.rs` (emit), `daemon_dependency_missing_smoke.rs` (assert), `docs/reference/log-events.md`, `docs/fr/12-daemon-lifecycle.md`.

- [ ] **Step 4: Confirm registry / shutdown plumbing has no leaks**

```bash
grep -rn "InflightRegistry\|ShutdownToken" crates/roki-daemon/src/
```

Expected: `daemon/inflight.rs`, `daemon/shutdown.rs`, `daemon/real_runner.rs`, `engine/real_state_runner.rs`, `runtime.rs`. Any other hit is unintended.

- [ ] **Step 5: Mark plan complete**

No commit. The plan is reviewed; the implementation is on the branch.

---

## Notes for the executor

- **The slice-7 `FailureMarker` expansion is out of scope.** FR12 §Cycle integration mentions `recursion_bound` / `cleanup_fs` / `daemon_internal` markers; those belong to slice 7 / fr:06 and are not touched here.
- **`which` crate version 6.x** is the current stable line; `7.x` if released by execution time is equivalent for our purposes. If the lockfile resolves a transitive dep that conflicts, pin a version that builds — no behavioral change is expected.
- **Test isolation under `$PATH` mutation.** Two tests (Task 3 unit, Task 8 e2e) overwrite `$PATH`. Cargo runs tests in parallel by default. The unit test in Task 3 uses an explicit guard; the e2e in Task 8 sets `PATH` on the child process only, so it doesn't contaminate the test runner. If parallelism causes flakes, fall back to `#[serial_test::serial]` on the unit test only — do not serialize the whole suite.
- **`tokio::select!` biased.** The `biased;` directive in Task 5 step 4 makes the watchdog future the first-poll arm, so a child that exits cleanly between SIGINT trap and select poll is reported as `Healthy` instead of being terminated unnecessarily.
