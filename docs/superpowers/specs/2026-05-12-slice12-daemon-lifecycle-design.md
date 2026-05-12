# Slice 12 — Daemon Lifecycle Design

Date: 2026-05-12
Scope: Close the remaining gaps between `fr:12-daemon-lifecycle` and the implementation that landed in slices 1–11. Two operator-visible behaviors are missing today: a startup PATH check for `wt` / `ghq`, and active SIGTERM propagation to in-flight state subprocesses during graceful shutdown. Slice 12 also tightens the `shutdown_window_exceeded` payload so the operator can identify which subprocess overran the window.

## 1. Position in the Roadmap

Slice 12 closes:

- `roki-cli-daemon-deps` — Startup dependency check. The daemon refuses to start when `wt` or `ghq` is missing from `$PATH`, emits a structured `daemon_dependency_missing` event (when the per-config event writer is available) plus an ERROR-severity tracing line, and exits non-zero. The AI cli (`claude`, `codex`, …) is **not** pre-checked per fr:12 §Boundaries.
- `roki-cli-daemon-shutdown` — Active subprocess termination on graceful shutdown. The shared `ShutdownToken` is threaded into `RealStateRunner`. When the token fires, the runner sends SIGTERM to the live state subprocess and joins the wait. A hard SIGKILL is sent only when the cumulative shutdown window (`[engine].shutdown_window_seconds`) expires. `shutdown_window_exceeded` carries one entry per offending subprocess (ticket_id, cycle_id, state_id, visit, pid) rather than a flat ticket-id list.
- `roki-cli-daemon-doc-fix` — Reference / FR text touch-ups so the lifecycle contract reads consistently: log-events.md gains rows for `daemon_dependency_missing` and the new `shutdown_window_exceeded` payload shape; fr:12 names the new event.

Slices 1–11 already provide: `Cli` parser with `run` / `cleanup` subcommands, `runtime::run` orchestrator, `RokiConfig::load` + `WorkflowConfig::load` validation with field-name errors, `EventWriter::open` per-daemon log, `ShutdownToken`, `Dispatcher` drain, ticket-task registry, `DaemonStarted` / `DaemonReady` / `DaemonShutdownBegan` / `DaemonShutdownCompleted` / `ShutdownWindowExceeded` events, cold-start enumeration, and stall-watchdog SIGTERM/SIGKILL inside `engine::stall`.

Out of scope, deferred:

- **Per-subprocess shutdown deadlines.** Slice 5 picked a cumulative deadline (one window covers the whole drain). fr:12 §Normal shutdown does not contradict that; we keep it.
- **Pre-check of the AI cli line.** fr:12 §Boundaries: not validated at startup; first failure surfaces as `process_crash`.
- **Daemonize / systemd integration / pid file.** fr:12 §Boundaries.
- **Windows.** fr:12 §Boundaries.
- **Restart-time state recovery.** fr:12 §Cycle integration: nothing is persisted; cold start re-enumerates.
- **`FailureMarker` enum expansion** (`recursion_bound` / `cleanup_fs` / `daemon_internal`). Owned by slice 7's escalation queue; fr:06 §Escalation queue is the authoritative source. Slice 12 does not touch it.

---

## 2. Architecture

### 2.1 Module touch list

```
crates/roki-daemon/src/
├── runtime.rs              // wire dep check; pass ShutdownToken into runner
├── daemon/
│   ├── shutdown.rs         // (unchanged) ShutdownToken already exposes wait()
│   ├── real_runner.rs      // forward shutdown into RealStateRunner
│   └── deps.rs             // NEW — `which wt` / `which ghq` probe
├── engine/
│   └── real_state_runner.rs// select! between child.wait() and shutdown.wait();
│                            // SIGTERM the child on shutdown; collect descriptor
│                            // for shutdown_window_exceeded
└── events.rs               // DaemonDependencyMissing variant + revised
                            // ShutdownWindowExceeded payload
```

No new crate is added.

### 2.2 Async model

Both new behaviors stay inside the existing `#[tokio::main]` runtime. The dep check is a synchronous `which` over `std::env::var("PATH")` performed before any task is spawned. The shutdown signal already lives on `Arc<ShutdownToken>` and is `.wait()`able from any future, so the runner only needs an additional `select!` arm.

### 2.3 Failure-mode budget

- Startup dep missing: log + emit + non-zero exit; cycle engine never starts.
- Subprocess fails to exit on SIGTERM within the window: SIGKILL + `shutdown_window_exceeded` with that subprocess as one entry. The daemon still exits.
- Shutdown signal during cold start: existing behavior — webhooks already see `503 cold_start_in_progress`; the cold-start GraphQL enumerate honors `ShutdownToken` (slice 6). No change in slice 12.

---

## 3. Startup dependency check

### 3.1 What is checked

`wt` and `ghq` only. These are the two binaries the daemon spawns directly (`engine::worktree`, `engine::cwd`). The AI cli is not parsed by the daemon — fr:12 §Boundaries.

### 3.2 Where it sits in the boot order

```
1. RokiConfig::load
2. WorkflowConfig::load
3. Open daemon event writer    ← (already step 4 today)
4. Dep check  ← NEW step 4a
5. DaemonStarted
6. Linear client / cold start / ...
```

The dep check runs **after** the event writer is open so the failure path can emit a structured `daemon_dependency_missing` event into `<session_root>/_daemon/events.jsonl`. If the check is moved earlier than the writer the failure would only show up via tracing — losing the JSONL row that `roki events --offline --file ...` could read. The trade is one writer-open per refusal, which is acceptable on a fatal path.

### 3.3 New module `daemon::deps`

```rust
pub struct MissingDependency {
    pub binary: &'static str,
    pub hint: &'static str,
}

pub fn check() -> Result<(), Vec<MissingDependency>> {
    let mut missing = Vec::new();
    for (bin, hint) in [
        ("wt",  "install worktree manager 'wt' and put it on PATH"),
        ("ghq", "install ghq (https://github.com/x-motemen/ghq) and put it on PATH"),
    ] {
        if which::which(bin).is_err() {
            missing.push(MissingDependency { binary: bin, hint });
        }
    }
    if missing.is_empty() { Ok(()) } else { Err(missing) }
}
```

The `which` crate is added as a direct dependency of `roki-daemon`. It is not in the lockfile today. The probe path itself is a few lines; the crate handles platform differences (`PATHEXT`, executable bit, symlink resolution) we would otherwise re-implement.

### 3.4 Runtime integration

```rust
// In runtime::run_inner, between event-writer open and DaemonStarted:
if let Err(missing) = crate::daemon::deps::check() {
    for m in &missing {
        let mut w = daemon_events.lock().await;
        let _ = w.emit(&Event::DaemonDependencyMissing {
            ts: now_rfc3339(),
            binary: m.binary.into(),
            remediation: m.hint.into(),
        });
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

`SkeletonError::MissingDependency` is a new variant that maps to `ExitCode::FAILURE` through the existing `run() -> ExitCode` conversion at runtime.rs:55. No new exit code is introduced.

### 3.5 Event shape

```rust
Event::DaemonDependencyMissing {
    ts: String,             // RFC3339
    binary: String,         // "wt" | "ghq"
    remediation: String,    // human-readable hint
}
```

Snake-case event name: `daemon_dependency_missing`. Carries no ticket / cycle context (startup-bound).

---

## 4. Active SIGTERM on graceful shutdown

### 4.1 Today

`runtime.rs` traps SIGINT / SIGTERM, fires `ShutdownToken`, sends `DispatchMsg::Shutdown` to every ticket task, and waits on each `JoinHandle` within `shutdown_window_seconds`. A ticket task drops out of its `inbox.recv()` loop on `Shutdown`, but the in-flight `RealStateRunner::run` is **not signalled** — the awaited `child.wait()` runs to natural completion. A long-running `claude` invocation therefore blocks the drain until either the cli exits or `tokio::time::timeout` cancels the join. Cancellation does **not** kill the child — `tokio::process::Child` requires `.kill()` to terminate the OS process. Today the child is left to outlive the daemon.

### 4.2 New shape

The shutdown token is plumbed one level deeper, into `RealStateRunner::run`, which gates the child's `wait()` against the token:

```rust
// engine/real_state_runner.rs (sketch — actual struct names follow current code)
let pid = child.id().unwrap_or(0);
let descriptor = SubprocessDescriptor {
    ticket_id: self.ticket_id.clone(),
    cycle_id: self.cycle_id,
    state_id: self.state_id.clone(),
    visit: self.visit,
    pid,
};
let outcome = tokio::select! {
    biased;
    res = child.wait_with_capture(&mut stdout, &mut stderr) => res?,
    () = self.shutdown.wait() => {
        // best-effort SIGTERM; do not block on send
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        // Hand off to the shutdown-aware wait below.
        return Err(StateError::ShutdownInProgress { descriptor });
    }
};
```

`ShutdownInProgress` is a new internal variant. The ticket task observes it, records the subprocess descriptor onto a shared `Arc<Mutex<Vec<SubprocessDescriptor>>>`, drops its inbox, and returns. The runtime's drain loop already runs after the dispatcher exits; the new vector is consulted when the cumulative window elapses to decide whether `shutdown_window_exceeded` should fire and which entries to list.

### 4.3 SIGKILL on window expiry

The existing drain at runtime.rs:399 uses `tokio::time::timeout(remaining, join)`. When that fires, the descriptor in the shared vec already points at the still-live subprocess. The drain loop SIGKILLs the pid (`nix::sys::signal::Signal::SIGKILL`) before emitting `shutdown_window_exceeded`. The kill is best-effort: if the process has already exited (race), the syscall returns `ESRCH` and is ignored.

### 4.4 `shutdown_window_exceeded` payload change

Existing variant:

```rust
ShutdownWindowExceeded {
    ts: String,
    aborted: usize,
    aborted_ticket_ids: Vec<String>,
}
```

New variant:

```rust
ShutdownWindowExceeded {
    ts: String,
    aborted: usize,
    offenders: Vec<ShutdownOffender>,
}

pub struct ShutdownOffender {
    pub ticket_id: String,
    pub cycle_id: Uuid,
    pub state_id: String,
    pub visit: u32,
    pub pid: u32,
}
```

`aborted_ticket_ids` is removed. The on-disk JSON Lines schema bumps to `schema_version: 2` (the field `schema_version` already lives on `DaemonStarted`; the rest of the events do not version individually because the file header on `daemon_started` records the writer schema for the run). One backward-compat note: the API projection at `crates/roki-daemon/src/api/projection/events.rs` re-emits the event payload as-is, so HTTP consumers and `roki events` see the new shape unchanged.

### 4.5 Stall-watchdog interaction

`engine::stall` already SIGTERMs (and SIGKILLs after 5 s) a stalled subprocess inside an active cycle. The shutdown path and the stall path can race: if a stall fires inside the shutdown window, the child may already be gone before SIGTERM is delivered from the shutdown path. The kill syscall is best-effort and idempotent — the second SIGTERM (or SIGKILL) returns `ESRCH` harmlessly. The descriptor still ends up in the offenders list only if the cumulative window expired before the wait returned; if the stall already drove `child.wait()` to completion, the runner returns the normal stall outcome instead of `ShutdownInProgress`.

---

## 5. Help-text / config-key parity

fr:12 §Capabilities and ref:cli already document the existing flag↔config-key mapping. `roki run` / `roki cleanup` expose only `--config <PATH>` (no override), and the help text already names `roki.toml`. Slice 12 does not change the parser; we only add a unit-test assertion that the rendered `--help` for both subcommands continues to mention `roki.toml` (slice 11 added one for `run`; slice 12 adds the symmetric one for `cleanup`, and a third test for `roki --help` confirming both subcommands appear).

---

## 6. Configuration touch points

- `[engine].shutdown_window_seconds` (already documented) is consumed by the existing drain loop. No schema change.
- No new TOML keys.

---

## 7. Logging from the daemon

Two structured-event changes, both routed through the existing `EventWriter` and `EventRing`:

| Event | When | Carries |
|---|---|---|
| `daemon_dependency_missing` | Startup; `wt` or `ghq` not on PATH | `binary`, `remediation` |
| `shutdown_window_exceeded` (modified) | Drain timed out | `aborted: usize`, `offenders: [{ticket_id, cycle_id, state_id, visit, pid}]` |

`docs/reference/log-events.md` gains the `daemon_dependency_missing` row and updates the `shutdown_window_exceeded` "Carries" cell.

---

## 8. Tests

### 8.1 Unit

- `daemon::deps::check`: PATH override → both binaries present, only `wt` missing, only `ghq` missing, both missing; assert returned `MissingDependency` set per case (use a temp `$PATH` that contains a `wt` / `ghq` stub script per case).
- `events.rs`: `daemon_dependency_missing` serializes with the expected snake-case field names; `shutdown_window_exceeded` round-trips the offender list.
- `runtime::run_inner` failure path: simulate dep missing by emptying `$PATH`; assert `Err(SkeletonError::MissingDependency)` and that the `_daemon/events.jsonl` file contains a `daemon_dependency_missing` line per missing binary, **before** any `daemon_started` line.
- `real_state_runner` shutdown path: spawn a long-sleeping child (`sleep 30`), fire `ShutdownToken`, assert child exits via SIGTERM (`exit_code` reflects signal kill) within a bounded wall-clock window in the test (≤2 s), and that the runner returns `ShutdownInProgress` carrying the right descriptor.
- `cli::run_help_names_config_and_roki_toml` symmetric test for `cleanup`.

### 8.2 Integration (`crates/roki-daemon/tests/e2e/`)

- `daemon_dependency_missing_smoke.rs` — spawn the binary with `PATH` set to a directory missing both `wt` and `ghq`; assert exit ≠ 0, assert the emitted `_daemon/events.jsonl` first line is `daemon_dependency_missing`, and assert no `daemon_started` line.
- `shutdown_window_exceeded_smoke.rs` — minimal workflow with a state that runs `sleep 30`; spawn the daemon, wait for `daemon_ready`, deliver a webhook fixture, observe `state_started`, then send SIGINT with `[engine].shutdown_window_seconds = 1`. Assert: exit code = `ExitCode::FAILURE`, an emitted `shutdown_window_exceeded` line names the offending subprocess (state_id from the workflow), and the OS pid recorded is no longer alive when the daemon exits.
- `shutdown_clean_smoke.rs` — same as above but the state is `true` (returns immediately); assert clean drain with `aborted = 0` and no `shutdown_window_exceeded`.
- `persistent_sigint_timeout_smoke.rs` (existing) — rewritten to read `offenders[].ticket_id` instead of `aborted_ticket_ids`. The old field name is gone; keeping the legacy assertion would be testing a payload the daemon no longer emits.

---

## 9. Spec impact

- `docs/fr/12-daemon-lifecycle.md`: §Normal shutdown bullet 3 keeps its wording; new sentence names the event variant and points to the offender payload in ref:log-events. §User-visible Behavior — "Missing dependency CLI" gains a sentence pointing at `daemon_dependency_missing`.
- `docs/reference/log-events.md`: add `daemon_dependency_missing` row under "Daemon lifecycle"; revise the `shutdown_window_exceeded` "Carries" cell to "Per-offender `{ticket_id, cycle_id, state_id, visit, pid}`".
- `docs/reference/cli.md`: no change.
- `docs/reference/config.md`: no change.

---

## 10. Risks / open verification

- **`which` PATH semantics.** Posix `which` honors `$PATH` order and rejects non-executable entries. We rely on the `which` crate's matching behavior (executable bit on Unix). Operators running the daemon under a service manager that resets `$PATH` see the failure deterministically, which is the desired fr:12 §"Missing dependency CLI" outcome.
- **Race between SIGTERM-on-shutdown and the stall watchdog.** Mitigated by §4.5: kill is idempotent and the offender vector is only populated when `child.wait()` is still pending at window expiry.
- **Subprocess refusing SIGTERM.** Possible for misbehaving cli lines. SIGKILL on window expiry guarantees process death; the daemon does not wait past the window to observe the kill take effect. If the OS leaves the process as a zombie, `nix::wait` reaping is owned by the existing `Child::wait` future which the runner is no longer awaiting — `tokio::process::Child` drops its inner pid handle on drop, which reaps via `SIGCHLD` already. No leak beyond what the platform allows.
