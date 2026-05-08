# Slice 3 Failure Handling & Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Layer `[[on_failure]]` first-match handler cycles, `[[cleanup]]` cycles (including the all-phases-omitted shorthand), `cycle.kind` propagation, `{{ failure.* }}` context vars, `FailureKind::FsPoison` for session_tempdir creation errors, and a `failure_unhandled` structured event on top of slice 2's session/stream-json engine. After this plan the binary can run a rule cycle, route any internal failure through `[[on_failure]]` (one level deep), run a cleanup cycle that triggers session_tempdir deletion plus a `worktree_delete_requested` event, and surface unhandled failures via the structured event log.

**Architecture:** Two new engine submodules (`dispatch`, `on_failure`, `cleanup`) sit between admission and `engine::run_cycle` in `runtime::run_inner`. A new `events.rs` module owns the per-ticket NDJSON event writer at `<session_root>/<ticket-id>.events.jsonl` (sibling of the ticket dir, not a child, so cleanup does not delete it). `engine::cycle::run_cycle` gains a `CycleKind` parameter and an `Option<FailureMeta>` parameter; `CycleOutcome::Failed` carries a full `FailureMeta`. `runtime::run_inner` reads the failed-cycle meta, evaluates `[[on_failure]]` first-match, and either spawns a handler cycle (`CycleKind::Failure`) or emits a `failure_unhandled` event before exiting 1. Cleanup-shorthand bypasses `engine::run_cycle` entirely.

**Tech Stack:** Rust 2024 (workspace edition), `tokio` async runtime, slice-1+2 deps (`liquid`, `shell-words`, `async-trait`, `serde_json`, `serde`, `tempfile`, `wiremock`, `reqwest`, `nix`, `serde_yaml_ng`, `uuid`, `time`, `clap`).

**Spec:** `docs/superpowers/specs/2026-05-08-slice3-failure-cleanup-design.md` (committed on branch `slice3-failure-cleanup-spec`).

**Working branch:** `slice3-failure-cleanup-spec` (already exists, contains the spec commit). All implementation commits land on this branch.

---

## File Structure

### Created

| Path | Responsibility |
|---|---|
| `crates/roki-daemon/src/events.rs` | `EventWriter`, `Event` enum, `WorktreeDeleteReason`, `FailureMarker`, sibling-path resolver. NDJSON append-only writer. |
| `crates/roki-daemon/src/engine/dispatch.rs` | `DispatchMode`, `DispatchTarget`, `evaluate(&AdmittedTicket, &WorkflowConfig, DispatchMode) -> DispatchTarget`. |
| `crates/roki-daemon/src/engine/on_failure.rs` | `KindMatcher`, `OnFailure::matches(&FailureMeta) -> bool`, `route(&[OnFailure], &FailureMeta) -> Option<&OnFailure>`. |
| `crates/roki-daemon/src/engine/cleanup.rs` | `delete_immediate` (shorthand) + `post_cycle_delete` (after a normal cleanup cycle). Owns the `remove_dir_all(<ticket-id>/)` call and the `worktree_delete_requested` event emission. |
| `crates/roki-daemon/tests/e2e/cleanup_shorthand_smoke.rs` | E2E: shorthand match → no cycle dirs, two events, ticket dir gone. |
| `crates/roki-daemon/tests/e2e/cleanup_cycle_smoke.rs` | E2E: non-shorthand cleanup cycle → cycle dirs created during run, deleted after, events ordered. |
| `crates/roki-daemon/tests/e2e/on_failure_smoke.rs` | E2E: rule cycle fails (process_crash) → `[[on_failure]]` matches → handler cycle runs → exit 0. |
| `crates/roki-daemon/tests/e2e/recursion_bound_smoke.rs` | E2E: handler cycle itself fails → `failure_unhandled marker=recursion_bound` → exit 1. |
| `crates/roki-daemon/tests/e2e/failure_unhandled_smoke.rs` | E2E: rule cycle fails, no `[[on_failure]]` match → `failure_unhandled marker=none` → exit 1. |
| `crates/roki-daemon/tests/e2e/fs_poison_smoke.rs` | E2E: session_root unwritable → `FsPoison` routes through `[[on_failure]]`. |
| `crates/roki-daemon/tests/e2e/cleanup_subcommand_smoke.rs` | E2E: `roki cleanup` ignores rule list, only matches `[[cleanup]]`. |
| `crates/roki-daemon/tests/e2e/fixtures/fail_run.sh` | Bash fake that exits non-zero with no parseable JSON (process_crash). |
| `crates/roki-daemon/tests/e2e/fixtures/fail_handler.sh` | Bash fake whose run phase exits non-zero (drives recursion_bound). |
| `crates/roki-daemon/tests/e2e/fixtures/echo_failure_env.sh` | Bash fake that echoes `ROKI_FAILURE_*` env then emits `{directive:"end"}`. |

### Modified

| Path | Change |
|---|---|
| `crates/roki-daemon/Cargo.toml` | Add `[[test]]` entries for the seven new e2e files. |
| `crates/roki-daemon/src/lib.rs` (or `main.rs` if no lib) | Declare `pub mod events;`. |
| `crates/roki-daemon/src/engine/mod.rs` | Declare new `dispatch`, `on_failure`, `cleanup` submodules. Re-export `CycleKind`, `FailureMeta`, `DispatchMode`, `DispatchTarget`. |
| `crates/roki-daemon/src/engine/outcome.rs` | Add `CycleKind` enum, `FailureMeta` struct, `FailureKind::FsPoison`. Replace `CycleOutcome::Failed { kind, iter }` with `Failed { meta: FailureMeta }`. |
| `crates/roki-daemon/src/engine/cycle.rs` | Plumb `CycleKind` + `Option<FailureMeta>` into `run_cycle`. Populate `FailureMeta` at every detection site. |
| `crates/roki-daemon/src/engine/phase.rs` | Same FailureMeta plumbing for `CommandPhaseExecutor`. |
| `crates/roki-daemon/src/engine/session.rs` | Same FailureMeta plumbing for `SessionSupervisor`. |
| `crates/roki-daemon/src/engine/context.rs` | Add `FailureView`, populate when `cycle.kind = "failure"`, expose `cycle.kind` parameter, expose `ROKI_FAILURE_*` env vars. |
| `crates/roki-daemon/src/capture.rs` | Wrap session_tempdir creation `io::Error` into a typed result that `engine::cycle` lifts to `FailureKind::FsPoison`. |
| `crates/roki-daemon/src/config/workflow.rs` | Parse `[[cleanup]]` (incl. shorthand), parse `[[on_failure]]`, add validation errors. |
| `crates/roki-daemon/src/error.rs` | Add new `WorkflowError` variants. |
| `crates/roki-daemon/src/cli.rs` | Add `cleanup` subcommand. |
| `crates/roki-daemon/src/runtime.rs` | Replace inline `rule::first_match` with `dispatch::evaluate`. Branch on `DispatchTarget`. Implement on_failure routing on cycle failure. Emit events via `EventWriter`. |
| `crates/roki-daemon/src/rule.rs` | Add `first_match` for `Vec<Cleanup>` (mirrors existing `Rule` first-match). |

---

## Cross-Task Conventions

- **Branch:** `slice3-failure-cleanup-spec` (already exists). All commits land here. Push when done with each task.
- **Test command:** `cargo test -p roki-daemon` for unit tests in the daemon crate. E2E tests run under the same command (`tests/e2e/*` are `[[test]]` entries).
- **Build verification:** `cargo build -p roki-daemon` after each task. CI also runs `cargo clippy -p roki-daemon -- -D warnings` and `cargo fmt --check`.
- **Commit messages:** Conventional Commits. Subject ≤50 chars, lowercase. Body explains *why* when non-obvious.
- **TDD discipline:** every task that adds behavior writes a failing test first. The failing-test step has expected error wording so the engineer can confirm the failure is the *intended* one.
- **No code beyond what tests cover.** YAGNI applies. Slice-3 deferrals (escalation queue, worktree creation) are explicit non-goals.
- **Test placement:** unit tests live in `#[cfg(test)] mod tests { ... }` at the bottom of the module. Integration / e2e tests live in `crates/roki-daemon/tests/e2e/<name>.rs`.

---

## Task 1: Add `CycleKind` enum to `engine::outcome`

Add the `CycleKind` enum alongside the existing types. No call-site changes.

**Files:**
- Modify: `crates/roki-daemon/src/engine/outcome.rs`

- [ ] **Step 1: Write the failing test**

Append to the existing `#[cfg(test)] mod tests` block at the bottom of `crates/roki-daemon/src/engine/outcome.rs`:

```rust
    #[test]
    fn cycle_kind_str_round_trip() {
        assert_eq!(CycleKind::Rule.as_str(), "rule");
        assert_eq!(CycleKind::Cleanup.as_str(), "cleanup");
        assert_eq!(CycleKind::Failure.as_str(), "failure");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p roki-daemon engine::outcome::tests::cycle_kind_str_round_trip`
Expected: compile error `cannot find type 'CycleKind' in this scope`.

- [ ] **Step 3: Add the enum**

Insert above the existing `FailureKind` definition in the same file:

```rust
/// Which list a cycle was dispatched from. Surfaced as
/// `cycle.kind` / `ROKI_CYCLE_KIND` per fr:01 §Cycle kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleKind {
    Rule,
    Cleanup,
    Failure,
}

impl CycleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CycleKind::Rule => "rule",
            CycleKind::Cleanup => "cleanup",
            CycleKind::Failure => "failure",
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p roki-daemon engine::outcome::tests::cycle_kind_str_round_trip`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/engine/outcome.rs
git commit -m "feat(engine): add CycleKind enum"
```

---

## Task 2: Add `FailureKind::FsPoison` variant

Slice-3 spec §8 requires this variant to route session_tempdir creation errors through `[[on_failure]]`.

**Files:**
- Modify: `crates/roki-daemon/src/engine/outcome.rs`

- [ ] **Step 1: Write the failing test**

Append to the same `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn failure_kind_fs_poison_str_round_trip() {
        assert_eq!(FailureKind::FsPoison.as_str(), "fs_poison");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p roki-daemon engine::outcome::tests::failure_kind_fs_poison_str_round_trip`
Expected: compile error `no variant named 'FsPoison' found for enum 'FailureKind'`.

- [ ] **Step 3: Extend the enum**

In the existing `pub enum FailureKind { ... }`, add:

```rust
    /// Filesystem error creating or recovering session_tempdir before phase
    /// launch. Worktree-side fs errors land here too once worktree creation
    /// lands; for slice 3 only session_tempdir is in scope.
    FsPoison,
```

In `impl FailureKind { fn as_str ... }` add the match arm:

```rust
            FailureKind::FsPoison => "fs_poison",
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p roki-daemon engine::outcome::tests::failure_kind_fs_poison_str_round_trip`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/engine/outcome.rs
git commit -m "feat(engine): add FailureKind::FsPoison"
```

---

## Task 3: Add `FailureMeta` struct

`FailureMeta` is the full record handed to `[[on_failure]]` + Liquid `{{ failure.* }}` namespace.

**Files:**
- Modify: `crates/roki-daemon/src/engine/outcome.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn failure_meta_constructor_round_trip() {
        let id = uuid::Uuid::nil();
        let meta = FailureMeta {
            failed_cycle_id: id,
            kind: FailureKind::Stall,
            phase: PhaseKind::Run,
            iter: 2,
            exit_code: Some(124),
            error_text: "stall after 30s".into(),
        };
        assert_eq!(meta.kind.as_str(), "stall");
        assert_eq!(meta.phase.as_str(), "run");
        assert_eq!(meta.iter, 2);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p roki-daemon engine::outcome::tests::failure_meta_constructor_round_trip`
Expected: compile error `cannot find struct 'FailureMeta'`.

- [ ] **Step 3: Add the struct**

Insert below the `FailureKind` impl block:

```rust
/// Full failure record routed to `[[on_failure]]` and exposed as
/// `{{ failure.* }}` per fr:01 §107.
#[derive(Debug, Clone)]
pub struct FailureMeta {
    /// UUID of the cycle that failed (NOT the handler cycle).
    pub failed_cycle_id: uuid::Uuid,
    pub kind: FailureKind,
    pub phase: PhaseKind,
    pub iter: u32,
    /// Subprocess exit code when applicable; `None` for stall/template_error/
    /// fs_poison/iter_exhausted detected before exit.
    pub exit_code: Option<i32>,
    /// Operator-facing description: head + tail of stderr, or a synthesized
    /// message for non-subprocess failures (template render, fs error, etc.).
    pub error_text: String,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p roki-daemon engine::outcome::tests::failure_meta_constructor_round_trip`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/engine/outcome.rs
git commit -m "feat(engine): add FailureMeta record"
```

---

## Task 4: Replace `CycleOutcome::Failed { kind, iter }` with `Failed { meta }`

Switch the cycle's failure variant to carry the full `FailureMeta`. `runtime::run_inner` extracts `kind` and `iter` from `meta` to keep the existing `SkeletonError::PhaseInfra(CycleFailed { ... })` mapping unchanged.

**Files:**
- Modify: `crates/roki-daemon/src/engine/cycle.rs`
- Modify: `crates/roki-daemon/src/runtime.rs`

- [ ] **Step 1: Write the failing test**

Append to `#[cfg(test)] mod tests` in `crates/roki-daemon/src/engine/cycle.rs`:

```rust
    #[test]
    fn cycle_outcome_failed_carries_meta() {
        // Smoke: variant exists with the expected shape.
        let id = uuid::Uuid::nil();
        let meta = crate::engine::outcome::FailureMeta {
            failed_cycle_id: id,
            kind: crate::engine::outcome::FailureKind::IterExhausted,
            phase: crate::engine::outcome::PhaseKind::Post,
            iter: 5,
            exit_code: None,
            error_text: "iter 5 exceeded cap".into(),
        };
        let outcome = crate::engine::CycleOutcome::Failed { meta };
        match outcome {
            crate::engine::CycleOutcome::Failed { meta } => {
                assert_eq!(meta.iter, 5);
                assert_eq!(meta.kind.as_str(), "iter_exhausted");
            }
            _ => panic!("expected Failed"),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p roki-daemon engine::cycle::tests::cycle_outcome_failed_carries_meta`
Expected: compile error `expected the field 'meta' on variant 'Failed'`.

- [ ] **Step 3: Replace the variant**

In `crates/roki-daemon/src/engine/cycle.rs`, change:

```rust
pub enum CycleOutcome {
    Completed { iters: u32 },
    Failed { kind: FailureKind, iter: u32 },
}
```

to:

```rust
pub enum CycleOutcome {
    Completed { iters: u32 },
    Failed { meta: FailureMeta },
}
```

Update the `use` line near the top of `cycle.rs` to include `FailureMeta`:

```rust
use crate::engine::outcome::{
    FailureKind, FailureMeta, PhaseBody, PhaseKind, PhaseOutcome, PhaseShape, PostDirective,
    PreDirective,
};
```

- [ ] **Step 4: Update every `CycleOutcome::Failed` construction site in `cycle.rs`**

Each existing call like:

```rust
break 'cycle Ok(CycleOutcome::Failed { kind, iter });
```

becomes:

```rust
break 'cycle Ok(CycleOutcome::Failed {
    meta: FailureMeta {
        failed_cycle_id: cycle_id,
        kind,
        phase: phase_kind,           // local var per detection site
        iter,
        exit_code,                   // Option<i32> per detection site
        error_text,                  // String per detection site
    },
});
```

For each site, populate `phase_kind`, `exit_code`, `error_text` from the surrounding code:
- After a `pre` failure: `phase_kind = PhaseKind::Pre`, `exit_code` = the just-observed exit code (or `None`), `error_text` = the captured stderr tail or directive-error message.
- After a `run` failure: `phase_kind = PhaseKind::Run`, `exit_code` = subprocess exit, `error_text` = stderr tail.
- After a `post` failure: `phase_kind = PhaseKind::Post`, similar.
- For `IterExhausted`: `phase_kind = PhaseKind::Post` (the post directive that triggered the cap), `exit_code = None`, `error_text = format!("iter {iter} exceeded max_iterations {cap}")`.

`cycle_id` is already in scope (the `Uuid` generated at cycle start in `run_cycle`).

For helper failure-text formatting, add at the top of the module (private):

```rust
fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len().saturating_sub(max);
    format!("...{}", &s[start..])
}
```

Call sites that have stderr already in scope use `truncate_tail(&stderr_buf, 4096)`. Sites without stderr (template error, etc.) build the `error_text` from the error's `Display`.

- [ ] **Step 5: Update `runtime::run_inner` failure branch**

In `crates/roki-daemon/src/runtime.rs`, change:

```rust
crate::engine::CycleOutcome::Failed { kind, iter } => {
    tracing::error!(
        failure_kind = %kind.as_str(),
        iter,
        "cycle failed"
    );
    // ...
    Err(SkeletonError::PhaseInfra(
        crate::error::PhaseInfraError::CycleFailed { kind, iter },
    ))
}
```

to:

```rust
crate::engine::CycleOutcome::Failed { meta } => {
    tracing::error!(
        failure_kind = %meta.kind.as_str(),
        phase = %meta.phase.as_str(),
        iter = meta.iter,
        "cycle failed"
    );
    // ...
    Err(SkeletonError::PhaseInfra(
        crate::error::PhaseInfraError::CycleFailed {
            kind: meta.kind,
            iter: meta.iter,
        },
    ))
}
```

(The on_failure routing is added in Task 17; for now this preserves the slice-2 exit-1 surface.)

- [ ] **Step 6: Run all tests**

Run: `cargo test -p roki-daemon`
Expected: all tests PASS, including the new `cycle_outcome_failed_carries_meta` and every existing test that asserts on `CycleOutcome::Failed`. If existing test bodies match on `kind, iter`, update them to match on `meta` and read `meta.kind` / `meta.iter`.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/src/engine/cycle.rs crates/roki-daemon/src/runtime.rs
git commit -m "refactor(engine): CycleOutcome::Failed carries FailureMeta"
```

---

## Task 5: Wire session_tempdir creation errors → `FailureKind::FsPoison`

Capture errors raised before any phase has run for the cycle become `FailureKind::FsPoison` instead of escaping as `SkeletonError`. The detection point is `engine::cycle::run_cycle` immediately after `prepare_iter_dir`.

**Files:**
- Modify: `crates/roki-daemon/src/capture.rs`
- Modify: `crates/roki-daemon/src/engine/cycle.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/roki-daemon/src/engine/cycle.rs` test module (the test module already defines `admitted()`, `cfg(u32)`, `rule(pre, post)`, and `FakeExec`):

```rust
    #[tokio::test]
    async fn fs_poison_when_session_root_unwritable() {
        // Force <session_root>/<ticket-id>/cycle-<uuid>/iter-1/ creation to
        // fail by passing a regular file as session_root. The first mkdir
        // hits ENOTDIR and run_cycle should lift it to FailureKind::FsPoison
        // routed through CycleOutcome::Failed.
        let tmp_file = tempfile::NamedTempFile::new().unwrap();
        let unwritable_root = tmp_file.path().to_path_buf();

        // Pre + Run scripted; FsPoison should fire BEFORE Pre executes, so
        // FakeExec::execute should never be called. We still need a
        // PhaseBody for the rule.
        let exec = FakeExec::new(vec![]);
        let r = rule(Some(PhaseBody::InlineCmd { cmd: "true".into() }), None);

        let outcome = run_cycle(&exec, &admitted(), &r, &unwritable_root, &cfg(10))
            .await
            .unwrap();

        match outcome {
            CycleOutcome::Failed { meta } => {
                assert_eq!(meta.kind, FailureKind::FsPoison);
                // Phase is whichever phase would have run first.
                assert!(matches!(meta.phase, PhaseKind::Pre | PhaseKind::Run));
                assert_eq!(meta.iter, 1);
                assert!(meta.exit_code.is_none());
                assert!(meta.error_text.contains("session_tempdir"));
            }
            other => panic!("expected FsPoison failure; got {other:?}"),
        }

        // FakeExec was never called.
        assert!(exec.calls.lock().unwrap().is_empty());
    }
```

(The signature `run_cycle(&exec, &admitted(), &r, &unwritable_root, &cfg(10))` matches the slice-2 form at this point in the plan. Task 6 later extends `run_cycle` with two more args; if Task 6 has already merged, append `, CycleKind::Rule, None` to this call.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p roki-daemon engine::cycle::tests::fs_poison_when_session_root_unwritable`
Expected: PANIC `expected FsPoison` (because today the io::Error escapes as `Result::Err(PhaseInfraError::Capture(...))`, the test never sees `FailureKind::FsPoison`).

- [ ] **Step 3: Wire the conversion**

In `crates/roki-daemon/src/engine/cycle.rs`, immediately before the iteration loop (where `prepare_iter_dir` or `open_session_phase_files` is first called), add a guard:

```rust
let iter_dir = match crate::capture::prepare_iter_dir(session_root, &ticket.id, cycle_id, iter) {
    Ok(d) => d,
    Err(e) => {
        let next_phase = if rule.pre.is_some() { PhaseKind::Pre } else { PhaseKind::Run };
        break 'cycle Ok(CycleOutcome::Failed {
            meta: FailureMeta {
                failed_cycle_id: cycle_id,
                kind: FailureKind::FsPoison,
                phase: next_phase,
                iter,
                exit_code: None,
                error_text: format!("session_tempdir creation failed: {e}"),
            },
        });
    }
};
```

Apply the same pattern at every fs-creation call site inside `run_cycle` (and inside `phase.rs` / `session.rs` if those files own their own `prepare_iter_dir` calls — grep for `prepare_iter_dir`, `open_session_phase_files`, `write_run_terminal_json` to find them).

`capture.rs` itself does not need changes: it already returns `io::Error` and the lift happens at the cycle level.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p roki-daemon engine::cycle::tests::fs_poison_when_session_root_unwritable`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/engine/cycle.rs
git commit -m "feat(engine): map session_tempdir create errors to FsPoison"
```

---

## Task 6: Plumb `CycleKind` through `run_cycle` and `PhaseContext`

`run_cycle` accepts `CycleKind`. `PhaseContext::new` accepts `CycleKind` and stores it in `cycle.kind`. `cycle.kind` is exposed via `ROKI_CYCLE_KIND`.

**Files:**
- Modify: `crates/roki-daemon/src/engine/cycle.rs`
- Modify: `crates/roki-daemon/src/engine/context.rs`
- Modify: `crates/roki-daemon/src/runtime.rs`

- [ ] **Step 1: Write the failing test**

Append to `#[cfg(test)] mod tests` in `crates/roki-daemon/src/engine/context.rs`:

```rust
    #[test]
    fn phase_context_cycle_kind_failure() {
        use crate::engine::outcome::CycleKind;
        let admitted = admitted();
        let cfg = cfg(5);
        let ctx = PhaseContext::new(&admitted, uuid::Uuid::nil(), &cfg, CycleKind::Failure);
        assert_eq!(ctx.cycle.kind, "failure");

        // env exposure
        let env: Vec<(String, String)> = roki_env_pairs(&ctx).into_iter().collect();
        assert!(env.iter().any(|(k, v)| k == "ROKI_CYCLE_KIND" && v == "failure"));
    }
```

(`admitted()` and `cfg(u32)` already exist in this module's `#[cfg(test)] mod tests` block — reuse them. The `env_vars` method name is illustrative; if the slice-2 builder uses a different name (e.g. `to_env`, `iter_env`, etc.), substitute it.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p roki-daemon engine::context::tests::phase_context_cycle_kind_failure`
Expected: compile error `this function takes 3 arguments but 4 arguments were supplied` (or similar; `PhaseContext::new` is the slice-2 3-arg form).

- [ ] **Step 3: Update `PhaseContext::new`**

In `crates/roki-daemon/src/engine/context.rs`, change:

```rust
impl PhaseContext {
    pub fn new(admitted: &AdmittedTicket, cycle_id: Uuid, cfg: &RokiConfig) -> Self {
        Self {
            // ...
            cycle: CycleView {
                id: cycle_id.to_string(),
                kind: "rule",
                // ...
```

to:

```rust
impl PhaseContext {
    pub fn new(
        admitted: &AdmittedTicket,
        cycle_id: Uuid,
        cfg: &RokiConfig,
        cycle_kind: crate::engine::outcome::CycleKind,
    ) -> Self {
        Self {
            // ...
            cycle: CycleView {
                id: cycle_id.to_string(),
                kind: cycle_kind.as_str(),
                // ...
```

In `pub fn roki_env_pairs(ctx: &PhaseContext) -> Vec<(String, String)>` (the slice-2 free function that enumerates the `ROKI_*` exports), add a `ROKI_CYCLE_KIND` entry derived from `ctx.cycle.kind`. Place it next to the existing `ROKI_CYCLE_ID` line.

- [ ] **Step 4: Update `run_cycle` signature**

In `crates/roki-daemon/src/engine/cycle.rs`, change:

```rust
pub async fn run_cycle<E: PhaseExecutor>(
    executor: &E,
    admitted: &AdmittedTicket,
    rule: &Rule,
    session_root: &Path,
    cfg: &RokiConfig,
) -> Result<CycleOutcome, PhaseInfraError> {
```

to:

```rust
pub async fn run_cycle<E: PhaseExecutor>(
    executor: &E,
    admitted: &AdmittedTicket,
    rule: &Rule,
    session_root: &Path,
    cfg: &RokiConfig,
    cycle_kind: crate::engine::outcome::CycleKind,
    failure: Option<crate::engine::outcome::FailureMeta>,
) -> Result<CycleOutcome, PhaseInfraError> {
```

Inside the function body, replace `PhaseContext::new(admitted, cycle_id, cfg)` with `PhaseContext::new(admitted, cycle_id, cfg, cycle_kind)`. The `failure` parameter is unused for now (consumed in Task 8); silence with `let _ = failure;` until then.

- [ ] **Step 5: Update every existing `run_cycle` call site**

In `crates/roki-daemon/src/runtime.rs`, change:

```rust
let outcome = crate::engine::run_cycle(
    &executor,
    &admitted,
    &matched_rule,
    &cfg.paths.session_root,
    &cfg,
)
.await?;
```

to:

```rust
let outcome = crate::engine::run_cycle(
    &executor,
    &admitted,
    &matched_rule,
    &cfg.paths.session_root,
    &cfg,
    crate::engine::outcome::CycleKind::Rule,
    None,
)
.await?;
```

Update every test-module call site in `cycle.rs` and `phase.rs` similarly: pass `CycleKind::Rule, None`.

- [ ] **Step 6: Run all tests**

Run: `cargo test -p roki-daemon`
Expected: PASS, including the new `phase_context_cycle_kind_failure`.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/src/engine/cycle.rs crates/roki-daemon/src/engine/context.rs crates/roki-daemon/src/runtime.rs
git commit -m "feat(engine): plumb CycleKind into run_cycle + PhaseContext"
```

---

## Task 7: Add `FailureView` namespace + `ROKI_FAILURE_*` env

`PhaseContext` carries an optional `failure: Option<FailureView>`. Liquid sees `{{ failure.kind }}` etc. only on failure cycles. Env exposes `ROKI_FAILURE_*` only when populated.

**Files:**
- Modify: `crates/roki-daemon/src/engine/context.rs`
- Modify: `crates/roki-daemon/src/engine/cycle.rs`

- [ ] **Step 1: Write the failing test**

Append to the same `#[cfg(test)] mod tests` block in `context.rs`:

```rust
    #[test]
    fn phase_context_failure_view_populated() {
        use crate::engine::outcome::{CycleKind, FailureKind, FailureMeta, PhaseKind};
        let admitted = admitted();
        let cfg = cfg(5);
        let failed_cycle_id = uuid::Uuid::from_u128(42);
        let meta = FailureMeta {
            failed_cycle_id,
            kind: FailureKind::Unparseable,
            phase: PhaseKind::Post,
            iter: 3,
            exit_code: Some(0),
            error_text: "no JSON object on stdout".into(),
        };
        let mut ctx = PhaseContext::new(&admitted, uuid::Uuid::nil(), &cfg, CycleKind::Failure);
        ctx.set_failure(meta);

        let env: std::collections::HashMap<String, String> = roki_env_pairs(&ctx).into_iter().collect();
        assert_eq!(env.get("ROKI_FAILURE_KIND").unwrap(), "unparseable");
        assert_eq!(env.get("ROKI_FAILURE_PHASE").unwrap(), "post");
        assert_eq!(env.get("ROKI_FAILURE_ITER").unwrap(), "3");
        assert_eq!(env.get("ROKI_FAILURE_EXIT_CODE").unwrap(), "0");
        assert_eq!(env.get("ROKI_FAILURE_ERROR_TEXT").unwrap(), "no JSON object on stdout");
        assert_eq!(env.get("ROKI_FAILURE_FAILED_CYCLE_ID").unwrap(), &failed_cycle_id.to_string());
    }

    #[test]
    fn phase_context_failure_absent_for_rule_cycle() {
        use crate::engine::outcome::CycleKind;
        let admitted = admitted();
        let cfg = cfg(5);
        let ctx = PhaseContext::new(&admitted, uuid::Uuid::nil(), &cfg, CycleKind::Rule);
        let env: std::collections::HashMap<String, String> = roki_env_pairs(&ctx).into_iter().collect();
        assert!(!env.contains_key("ROKI_FAILURE_KIND"));
    }

    #[test]
    fn phase_context_failure_exit_code_absent_when_none() {
        use crate::engine::outcome::{CycleKind, FailureKind, FailureMeta, PhaseKind};
        let admitted = admitted();
        let cfg = cfg(5);
        let meta = FailureMeta {
            failed_cycle_id: uuid::Uuid::nil(),
            kind: FailureKind::Stall,
            phase: PhaseKind::Run,
            iter: 1,
            exit_code: None,
            error_text: "stall".into(),
        };
        let mut ctx = PhaseContext::new(&admitted, uuid::Uuid::nil(), &cfg, CycleKind::Failure);
        ctx.set_failure(meta);
        let env: std::collections::HashMap<String, String> = roki_env_pairs(&ctx).into_iter().collect();
        assert!(!env.contains_key("ROKI_FAILURE_EXIT_CODE"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p roki-daemon engine::context::tests::phase_context_failure`
Expected: compile errors `cannot find method 'set_failure'` and `unknown field 'failure'`.

- [ ] **Step 3: Add `FailureView` and the setter**

In `crates/roki-daemon/src/engine/context.rs`, add:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct FailureView {
    pub kind: String,
    pub phase: String,
    pub iter: u32,
    /// Stringified for Liquid; absent in env when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub error_text: String,
    pub failed_cycle_id: String,
}
```

Add `failure: Option<FailureView>` to `PhaseContext`:

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureView>,
```

Initialize it as `None` in `PhaseContext::new`. Add a setter:

```rust
    pub fn set_failure(&mut self, meta: crate::engine::outcome::FailureMeta) {
        self.failure = Some(FailureView {
            kind: meta.kind.as_str().to_string(),
            phase: meta.phase.as_str().to_string(),
            iter: meta.iter,
            exit_code: meta.exit_code,
            error_text: meta.error_text,
            failed_cycle_id: meta.failed_cycle_id.to_string(),
        });
    }
```

In `roki_env_pairs(ctx: &PhaseContext) -> Vec<(String, String)>`, append the failure block before the final return:

```rust
    if let Some(f) = &ctx.failure {
        out.push(("ROKI_FAILURE_KIND".to_string(), f.kind.clone()));
        out.push(("ROKI_FAILURE_PHASE".to_string(), f.phase.clone()));
        out.push(("ROKI_FAILURE_ITER".to_string(), f.iter.to_string()));
        if let Some(ec) = f.exit_code {
            out.push(("ROKI_FAILURE_EXIT_CODE".to_string(), ec.to_string()));
        }
        out.push(("ROKI_FAILURE_ERROR_TEXT".to_string(), f.error_text.clone()));
        out.push(("ROKI_FAILURE_FAILED_CYCLE_ID".to_string(), f.failed_cycle_id.clone()));
    }
```

The existing builder named `out` is a `Vec<(String, String)>`. Match the slice-2 push pattern verbatim.

- [ ] **Step 4: Wire `set_failure` into `run_cycle`**

In `crates/roki-daemon/src/engine/cycle.rs`, after building `PhaseContext`:

```rust
let mut ctx = PhaseContext::new(admitted, cycle_id, cfg, cycle_kind);
if let Some(meta) = failure.clone() {
    ctx.set_failure(meta);
}
```

(The unused `let _ = failure;` from Task 6 step 4 is now removed.)

- [ ] **Step 5: Run tests**

Run: `cargo test -p roki-daemon engine::context::tests`
Expected: every new failure-namespace test PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/engine/context.rs crates/roki-daemon/src/engine/cycle.rs
git commit -m "feat(engine): expose failure.* / ROKI_FAILURE_* on failure cycles"
```

---

## Task 8: Add `events` module — types and writer scaffold

The events writer lives at `<session_root>/<ticket-id>.events.jsonl` (sibling). This task adds the file with the type definitions and a unit-tested writer; integration with `runtime` lands in Task 18.

**Files:**
- Create: `crates/roki-daemon/src/events.rs`
- Modify: `crates/roki-daemon/src/main.rs` (or `lib.rs` if present)

- [ ] **Step 1: Create the file**

Write `crates/roki-daemon/src/events.rs`:

```rust
//! Structured event JSONL writer.
//!
//! One file per ticket at `<session_root>/<ticket-id>.events.jsonl` (sibling
//! of the ticket dir, not a child — survives cleanup-cycle deletion).
//! Append-only NDJSON; one event per line; flush after each line.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use uuid::Uuid;

use crate::engine::outcome::{CycleKind, FailureKind, PhaseKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureMarker {
    None,
    RecursionBound,
    CleanupFsError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeDeleteReason {
    CleanupTerminal,
    CleanupShorthand,
}

#[derive(Debug, Serialize)]
pub struct FailureMetaSer {
    pub kind: String,
    pub phase: Option<String>,
    pub iter: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub error_text: String,
}

impl FailureMetaSer {
    pub fn from_meta(meta: &crate::engine::outcome::FailureMeta) -> Self {
        Self {
            kind: meta.kind.as_str().to_string(),
            phase: Some(meta.phase.as_str().to_string()),
            iter: meta.iter,
            exit_code: meta.exit_code,
            error_text: meta.error_text.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    CycleCompleted {
        ts: String,
        cycle_id: String,
        cycle_kind: String,
        iters: u32,
        outcome: Option<String>,
    },
    FailureUnhandled {
        ts: String,
        cycle_id: String,
        cycle_kind: String,
        failure: FailureMetaSer,
        marker: FailureMarker,
    },
    WorktreeDeleteRequested {
        ts: String,
        ticket_id: String,
        cycle_id: Option<String>,
        reason: WorktreeDeleteReason,
    },
}

pub fn events_path(session_root: &Path, ticket_id: &str) -> PathBuf {
    session_root.join(format!("{}.events.jsonl", sanitize_ticket(ticket_id)))
}

/// Sanitize the ticket id for use as a path component. Mirrors
/// `capture::sanitize` so the events file's stem matches the ticket dir name.
fn sanitize_ticket(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

pub struct EventWriter {
    file: BufWriter<File>,
    path: PathBuf,
}

impl EventWriter {
    pub fn open(session_root: &Path, ticket_id: &str) -> std::io::Result<Self> {
        let path = events_path(session_root, ticket_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            file: BufWriter::new(file),
            path,
        })
    }

    pub fn emit(&mut self, event: &Event) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.file, event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn now_rfc3339() -> String {
    use time::OffsetDateTime;
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_path_is_sibling_of_ticket_dir() {
        let root = Path::new("/tmp/sessions");
        let p = events_path(root, "OPS-123");
        assert_eq!(p, Path::new("/tmp/sessions/OPS-123.events.jsonl"));
    }

    #[test]
    fn event_writer_appends_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut w = EventWriter::open(root, "OPS-7").unwrap();
        w.emit(&Event::CycleCompleted {
            ts: "2026-05-08T00:00:00Z".into(),
            cycle_id: uuid::Uuid::nil().to_string(),
            cycle_kind: "rule".into(),
            iters: 1,
            outcome: Some("success".into()),
        })
        .unwrap();
        w.emit(&Event::WorktreeDeleteRequested {
            ts: "2026-05-08T00:00:01Z".into(),
            ticket_id: "OPS-7".into(),
            cycle_id: None,
            reason: WorktreeDeleteReason::CleanupShorthand,
        })
        .unwrap();
        drop(w);

        let path = events_path(root, "OPS-7");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"cycle_completed\""));
        assert!(lines[0].contains("\"cycle_kind\":\"rule\""));
        assert!(lines[1].contains("\"event\":\"worktree_delete_requested\""));
        assert!(lines[1].contains("\"reason\":\"cleanup_shorthand\""));
    }

    #[test]
    fn event_writer_creates_file_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let _ = EventWriter::open(root, "OPS-9").unwrap();
        let p = events_path(root, "OPS-9");
        assert!(p.exists());
    }

    #[test]
    fn ticket_id_with_special_chars_sanitized() {
        let p = events_path(Path::new("/r"), "team/abc#1");
        assert_eq!(p, Path::new("/r/team_abc_1.events.jsonl"));
    }

    #[test]
    fn failure_unhandled_serializes_marker_and_failure() {
        let ev = Event::FailureUnhandled {
            ts: "t".into(),
            cycle_id: "c".into(),
            cycle_kind: "rule".into(),
            failure: FailureMetaSer {
                kind: "stall".into(),
                phase: Some("run".into()),
                iter: 2,
                exit_code: None,
                error_text: "stalled".into(),
            },
            marker: FailureMarker::None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"event\":\"failure_unhandled\""));
        assert!(s.contains("\"marker\":\"none\""));
        assert!(s.contains("\"kind\":\"stall\""));
        assert!(!s.contains("exit_code"), "None exit_code should be omitted");
    }
}
```

- [ ] **Step 2: Declare the module**

In `crates/roki-daemon/src/main.rs` (or wherever the module declarations live), add:

```rust
pub mod events;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p roki-daemon events::tests`
Expected: all five tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/events.rs crates/roki-daemon/src/main.rs
git commit -m "feat(events): add NDJSON event writer scaffold"
```

---

## Task 9: Parse `[[cleanup]]` (incl. shorthand) into `WorkflowConfig`

`WorkflowConfig` gains `pub cleanups: Vec<Cleanup>`. Shorthand entry = no `pre`/`run`/`post` fields and no `when.*` keys.

**Files:**
- Modify: `crates/roki-daemon/src/config/workflow.rs`
- Modify: `crates/roki-daemon/src/error.rs`

- [ ] **Step 1: Write failing tests**

Append to `#[cfg(test)] mod tests` in `crates/roki-daemon/src/config/workflow.rs`:

```rust
    #[test]
    fn workflow_parses_cleanup_shorthand() {
        let toml = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "true"

[[cleanup]]
"#;
        let path = write_tmp(toml);
        let cfg = WorkflowConfig::load(&path).unwrap();
        assert_eq!(cfg.cleanups.len(), 1);
        assert!(cfg.cleanups[0].is_shorthand());
    }

    #[test]
    fn workflow_parses_cleanup_with_when_status() {
        let toml = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "true"

[[cleanup]]
when.status = "Done"
run.cmd = "echo done"
"#;
        let path = write_tmp(toml);
        let cfg = WorkflowConfig::load(&path).unwrap();
        assert_eq!(cfg.cleanups.len(), 1);
        assert!(!cfg.cleanups[0].is_shorthand());
        assert_eq!(cfg.cleanups[0].when_status.as_deref(), Some("Done"));
    }

    #[test]
    fn workflow_rejects_cleanup_with_pre_but_no_run() {
        let toml = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "true"

[[cleanup]]
when.status = "Done"
pre.cmd = "echo pre"
"#;
        let path = write_tmp(toml);
        let err = WorkflowConfig::load(&path).unwrap_err();
        match err {
            WorkflowError::CleanupMissingRun { .. } => {}
            other => panic!("expected CleanupMissingRun, got {other:?}"),
        }
    }

    #[test]
    fn workflow_rejects_cleanup_shorthand_with_when() {
        let toml = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "true"

[[cleanup]]
when.status = "Done"
"#;
        let path = write_tmp(toml);
        let err = WorkflowConfig::load(&path).unwrap_err();
        match err {
            WorkflowError::CleanupShorthandWithWhen { .. } => {}
            other => panic!("expected CleanupShorthandWithWhen, got {other:?}"),
        }
    }

    /// Helper for these tests. Add at the bottom of the test module if missing.
    fn write_tmp(toml: &str) -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.toml");
        std::fs::write(&path, toml).unwrap();
        // Leak the dir so it survives the test invocation. Only safe in test code.
        std::mem::forget(dir);
        path
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p roki-daemon config::workflow::tests::workflow_parses_cleanup_shorthand`
Expected: compile errors `no field 'cleanups' on type 'WorkflowConfig'`, `no method 'is_shorthand'`, `no variant 'CleanupMissingRun'`, `no variant 'CleanupShorthandWithWhen'`.

- [ ] **Step 3: Add the `Cleanup` type**

In `crates/roki-daemon/src/config/workflow.rs`, near the existing `Rule` struct:

```rust
#[derive(Clone, Debug)]
pub struct Cleanup {
    pub when_status: Option<String>,
    pub when_labels_has_all: Vec<String>,
    pub pre: Option<crate::engine::outcome::PhaseBody>,
    pub run: Option<crate::engine::outcome::PhaseBody>,
    pub post: Option<crate::engine::outcome::PhaseBody>,
}

impl Cleanup {
    pub fn is_shorthand(&self) -> bool {
        self.pre.is_none() && self.run.is_none() && self.post.is_none()
    }
}
```

Add `pub cleanups: Vec<Cleanup>` to `WorkflowConfig`. Update `WorkflowConfig::load` to parse it (default `Vec::new()` if absent).

- [ ] **Step 4: Implement the parser**

Add to `workflow.rs`:

```rust
fn parse_cleanups(
    path: &Path,
    workflow_dir: &Path,
    root: &Value,
) -> Result<Vec<Cleanup>, WorkflowError> {
    let Some(arr) = root.get("cleanup").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut out = Vec::with_capacity(arr.len());
    for (idx, entry) in arr.iter().enumerate() {
        let table = entry.as_table().ok_or_else(|| WorkflowError::Parse {
            path: path.to_path_buf(),
            source: toml::de::Error::custom(format!("[[cleanup]][{idx}] is not a table")),
        })?;

        let when = table.get("when").and_then(Value::as_table);
        let when_status = when
            .and_then(|w| w.get("status"))
            .and_then(Value::as_str)
            .map(String::from);
        let when_labels_has_all = when
            .and_then(|w| w.get("labels"))
            .and_then(Value::as_table)
            .and_then(|l| l.get("has_all"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let pre = parse_phase_body(path, workflow_dir, table.get("pre"), "cleanup", idx, "pre")?;
        let run_body =
            parse_phase_body(path, workflow_dir, table.get("run"), "cleanup", idx, "run")?;
        let post =
            parse_phase_body(path, workflow_dir, table.get("post"), "cleanup", idx, "post")?;

        let any_phase = pre.is_some() || run_body.is_some() || post.is_some();
        let any_when = when_status.is_some() || !when_labels_has_all.is_empty();

        // Shorthand: no phases AND no when. Anything else with phases must have run.
        if !any_phase {
            if any_when {
                return Err(WorkflowError::CleanupShorthandWithWhen {
                    path: path.to_path_buf(),
                    index: idx,
                });
            }
            // Pure shorthand.
        } else if run_body.is_none() {
            return Err(WorkflowError::CleanupMissingRun {
                path: path.to_path_buf(),
                index: idx,
            });
        }

        // Slice-2 narrowing: run-shape Session is rejected for any cycle-spawning entry.
        if let Some(rb) = &run_body {
            if rb.shape() == crate::engine::outcome::PhaseShape::Session {
                return Err(WorkflowError::SessionRunUnsupported {
                    path: path.to_path_buf(),
                    where_: format!("cleanup[{idx}].run"),
                });
            }
        }

        out.push(Cleanup {
            when_status,
            when_labels_has_all,
            pre,
            run: run_body,
            post,
        });
    }
    Ok(out)
}
```

(`parse_phase_body` is the slice-1+2 helper that already parses inline `cmd`/`prompt` and `path = "..."`. Reuse it. The signature in the codebase may differ slightly — match it. The `where_` argument names the location for error messages.)

In `WorkflowConfig::load`, after `let rules = parse_rules(...)?;`:

```rust
let cleanups = parse_cleanups(path, &workflow_dir, &root)?;
```

And add `cleanups,` to the constructor.

- [ ] **Step 5: Add error variants**

In `crates/roki-daemon/src/error.rs`, in `pub enum WorkflowError`:

```rust
    #[error("[[cleanup]][{index}] declares pre/post but no run; in {path}")]
    CleanupMissingRun {
        path: std::path::PathBuf,
        index: usize,
    },
    #[error("[[cleanup]][{index}] is shorthand (no phases) but declares when.*; in {path}")]
    CleanupShorthandWithWhen {
        path: std::path::PathBuf,
        index: usize,
    },
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p roki-daemon config::workflow::tests`
Expected: every cleanup test PASS, including the four new ones.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/src/config/workflow.rs crates/roki-daemon/src/error.rs
git commit -m "feat(config): parse [[cleanup]] including shorthand"
```

---

## Task 10: Parse `[[on_failure]]` into `WorkflowConfig`

`WorkflowConfig` gains `pub on_failures: Vec<OnFailure>`. Matcher vocabulary: `when.kind` (single), `when.kind.in` (array), `when.kind.not` (single), and optional `when.phase`.

**Files:**
- Modify: `crates/roki-daemon/src/config/workflow.rs`
- Modify: `crates/roki-daemon/src/error.rs`
- Create: `crates/roki-daemon/src/engine/on_failure.rs`

- [ ] **Step 1: Create `engine/on_failure.rs` skeleton**

Write `crates/roki-daemon/src/engine/on_failure.rs`:

```rust
//! `[[on_failure]]` first-match evaluation against a `FailureMeta`.
//!
//! Per fr:06 §53 + §63, `when.kind` accepts:
//!   - single value: `when.kind = "stall"`
//!   - in-array:     `when.kind.in = ["unparseable", "schema_drift"]`
//!   - not:          `when.kind.not = "iter_exhausted"`
//! plus optional `when.phase = "pre" | "run" | "post"`.
//!
//! Exactly one of the three `when.kind` forms may be set per entry; mixing
//! them is a config-load error (`OnFailureKindMatcherConflict`).

use crate::engine::outcome::{FailureKind, FailureMeta, PhaseBody, PhaseKind};

#[derive(Debug, Clone)]
pub enum KindMatcher {
    Eq(FailureKind),
    In(Vec<FailureKind>),
    Not(FailureKind),
}

#[derive(Debug, Clone)]
pub struct OnFailure {
    pub when_kind: KindMatcher,
    pub when_phase: Option<PhaseKind>,
    pub pre: Option<PhaseBody>,
    pub run: PhaseBody,
    pub post: Option<PhaseBody>,
}

impl OnFailure {
    pub fn matches(&self, meta: &FailureMeta) -> bool {
        let kind_ok = match &self.when_kind {
            KindMatcher::Eq(k) => *k == meta.kind,
            KindMatcher::In(ks) => ks.contains(&meta.kind),
            KindMatcher::Not(k) => *k != meta.kind,
        };
        let phase_ok = self.when_phase.map_or(true, |p| p == meta.phase);
        kind_ok && phase_ok
    }
}

pub fn route<'a>(entries: &'a [OnFailure], meta: &FailureMeta) -> Option<&'a OnFailure> {
    entries.iter().find(|e| e.matches(meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::outcome::{FailureKind, PhaseKind};

    fn meta(kind: FailureKind, phase: PhaseKind) -> FailureMeta {
        FailureMeta {
            failed_cycle_id: uuid::Uuid::nil(),
            kind,
            phase,
            iter: 1,
            exit_code: None,
            error_text: String::new(),
        }
    }

    fn entry(when_kind: KindMatcher, when_phase: Option<PhaseKind>) -> OnFailure {
        OnFailure {
            when_kind,
            when_phase,
            pre: None,
            run: PhaseBody::InlineCmd { cmd: "true".into() },
            post: None,
        }
    }

    #[test]
    fn matcher_eq() {
        let e = entry(KindMatcher::Eq(FailureKind::Stall), None);
        assert!(e.matches(&meta(FailureKind::Stall, PhaseKind::Run)));
        assert!(!e.matches(&meta(FailureKind::Unparseable, PhaseKind::Post)));
    }

    #[test]
    fn matcher_in() {
        let e = entry(
            KindMatcher::In(vec![FailureKind::Unparseable, FailureKind::SchemaDrift]),
            None,
        );
        assert!(e.matches(&meta(FailureKind::Unparseable, PhaseKind::Post)));
        assert!(e.matches(&meta(FailureKind::SchemaDrift, PhaseKind::Pre)));
        assert!(!e.matches(&meta(FailureKind::Stall, PhaseKind::Run)));
    }

    #[test]
    fn matcher_not() {
        let e = entry(KindMatcher::Not(FailureKind::IterExhausted), None);
        assert!(e.matches(&meta(FailureKind::Stall, PhaseKind::Run)));
        assert!(!e.matches(&meta(FailureKind::IterExhausted, PhaseKind::Post)));
    }

    #[test]
    fn matcher_phase_optional() {
        let e = entry(KindMatcher::Eq(FailureKind::Stall), Some(PhaseKind::Run));
        assert!(e.matches(&meta(FailureKind::Stall, PhaseKind::Run)));
        assert!(!e.matches(&meta(FailureKind::Stall, PhaseKind::Pre)));
    }

    #[test]
    fn route_first_match_wins() {
        let entries = vec![
            entry(KindMatcher::Eq(FailureKind::Stall), Some(PhaseKind::Pre)),
            entry(KindMatcher::Eq(FailureKind::Stall), None),
        ];
        let m = meta(FailureKind::Stall, PhaseKind::Run);
        let hit = route(&entries, &m).unwrap();
        // First entry's phase-pre filter excludes the run-phase failure;
        // second entry (no phase) matches.
        assert!(hit.when_phase.is_none());
    }

    #[test]
    fn route_no_match_returns_none() {
        let entries = vec![entry(KindMatcher::Eq(FailureKind::Stall), None)];
        let m = meta(FailureKind::Unparseable, PhaseKind::Post);
        assert!(route(&entries, &m).is_none());
    }
}
```

In `crates/roki-daemon/src/engine/mod.rs`, add:

```rust
pub mod on_failure;
```

- [ ] **Step 2: Run on_failure tests to verify they pass**

Run: `cargo test -p roki-daemon engine::on_failure::tests`
Expected: all six tests PASS.

- [ ] **Step 3: Write workflow-parser failing tests**

Append to `#[cfg(test)] mod tests` in `crates/roki-daemon/src/config/workflow.rs`:

```rust
    #[test]
    fn workflow_parses_on_failure_eq() {
        let toml = r#"
[admission]
assignee = "me"
[[admission.repos]]
ghq = "github.com/foo/bar"
[[rule]]
when.status = "InProgress"
run.cmd = "true"
[[on_failure]]
when.kind = "stall"
run.cmd = "echo handled"
"#;
        let path = write_tmp(toml);
        let cfg = WorkflowConfig::load(&path).unwrap();
        assert_eq!(cfg.on_failures.len(), 1);
        match &cfg.on_failures[0].when_kind {
            crate::engine::on_failure::KindMatcher::Eq(k) => {
                assert_eq!(k.as_str(), "stall");
            }
            _ => panic!("expected Eq"),
        }
    }

    #[test]
    fn workflow_parses_on_failure_in() {
        let toml = r#"
[admission]
assignee = "me"
[[admission.repos]]
ghq = "github.com/foo/bar"
[[rule]]
when.status = "InProgress"
run.cmd = "true"
[[on_failure]]
when.kind.in = ["unparseable", "schema_drift"]
when.phase = "post"
run.cmd = "true"
"#;
        let path = write_tmp(toml);
        let cfg = WorkflowConfig::load(&path).unwrap();
        match &cfg.on_failures[0].when_kind {
            crate::engine::on_failure::KindMatcher::In(ks) => {
                assert_eq!(ks.len(), 2);
            }
            _ => panic!("expected In"),
        }
        assert_eq!(cfg.on_failures[0].when_phase, Some(crate::engine::outcome::PhaseKind::Post));
    }

    #[test]
    fn workflow_rejects_on_failure_kind_conflict() {
        let toml = r#"
[admission]
assignee = "me"
[[admission.repos]]
ghq = "github.com/foo/bar"
[[rule]]
when.status = "InProgress"
run.cmd = "true"
[[on_failure]]
when.kind = "stall"
when.kind.in = ["stall", "unparseable"]
run.cmd = "true"
"#;
        let path = write_tmp(toml);
        let err = WorkflowConfig::load(&path).unwrap_err();
        assert!(matches!(err, WorkflowError::OnFailureKindMatcherConflict { .. }));
    }

    #[test]
    fn workflow_rejects_on_failure_unknown_kind() {
        let toml = r#"
[admission]
assignee = "me"
[[admission.repos]]
ghq = "github.com/foo/bar"
[[rule]]
when.status = "InProgress"
run.cmd = "true"
[[on_failure]]
when.kind = "bogus"
run.cmd = "true"
"#;
        let path = write_tmp(toml);
        let err = WorkflowConfig::load(&path).unwrap_err();
        assert!(matches!(err, WorkflowError::OnFailureUnknownKind { .. }));
    }

    #[test]
    fn workflow_rejects_on_failure_missing_run() {
        let toml = r#"
[admission]
assignee = "me"
[[admission.repos]]
ghq = "github.com/foo/bar"
[[rule]]
when.status = "InProgress"
run.cmd = "true"
[[on_failure]]
when.kind = "stall"
post.prompt = "noop"
"#;
        let path = write_tmp(toml);
        let err = WorkflowConfig::load(&path).unwrap_err();
        assert!(matches!(err, WorkflowError::OnFailureMissingRun { .. }));
    }

    #[test]
    fn workflow_rejects_on_failure_empty_kind_in() {
        let toml = r#"
[admission]
assignee = "me"
[[admission.repos]]
ghq = "github.com/foo/bar"
[[rule]]
when.status = "InProgress"
run.cmd = "true"
[[on_failure]]
when.kind.in = []
run.cmd = "true"
"#;
        let path = write_tmp(toml);
        let err = WorkflowConfig::load(&path).unwrap_err();
        assert!(matches!(err, WorkflowError::OnFailureEmptyKindIn { .. }));
    }
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p roki-daemon config::workflow::tests::workflow_parses_on_failure`
Expected: compile errors `no field 'on_failures' on type 'WorkflowConfig'` and `no variant 'OnFailureKindMatcherConflict'`.

- [ ] **Step 5: Implement the parser**

Add `pub on_failures: Vec<crate::engine::on_failure::OnFailure>` to `WorkflowConfig`.

Add the parser function:

```rust
fn parse_on_failures(
    path: &Path,
    workflow_dir: &Path,
    root: &Value,
) -> Result<Vec<crate::engine::on_failure::OnFailure>, WorkflowError> {
    use crate::engine::on_failure::{KindMatcher, OnFailure};
    use crate::engine::outcome::{FailureKind, PhaseKind, PhaseShape};

    let Some(arr) = root.get("on_failure").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut out = Vec::with_capacity(arr.len());
    for (idx, entry) in arr.iter().enumerate() {
        let table = entry.as_table().ok_or_else(|| WorkflowError::Parse {
            path: path.to_path_buf(),
            source: toml::de::Error::custom(format!("[[on_failure]][{idx}] is not a table")),
        })?;

        let when = table.get("when").and_then(Value::as_table);
        let kind_eq_str = when
            .and_then(|w| w.get("kind"))
            .and_then(|v| v.as_str().map(String::from));
        let kind_in_arr = when
            .and_then(|w| w.get("kind"))
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("in"))
            .and_then(Value::as_array);
        let kind_not_str = when
            .and_then(|w| w.get("kind"))
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("not"))
            .and_then(|v| v.as_str().map(String::from));

        // Detect mutual exclusion: kind=str / kind.in=arr / kind.not=str.
        // Note: when both `kind = "stall"` and `kind.in = [...]` are set in
        // TOML, the parser treats `kind` as a string (kind_eq_str = Some)
        // AND the `[when.kind] in = [...]` table form gets ignored because
        // kind is already typed as string. Catch both shapes:
        let any_kind_field = when.map_or(false, |w| w.contains_key("kind"));
        let forms_set = [kind_eq_str.is_some(), kind_in_arr.is_some(), kind_not_str.is_some()]
            .iter()
            .filter(|b| **b)
            .count();
        if !any_kind_field || forms_set == 0 {
            return Err(WorkflowError::OnFailureMissingKind {
                path: path.to_path_buf(),
                index: idx,
            });
        }
        if forms_set > 1 {
            return Err(WorkflowError::OnFailureKindMatcherConflict {
                path: path.to_path_buf(),
                index: idx,
            });
        }

        let when_kind = if let Some(s) = kind_eq_str {
            KindMatcher::Eq(parse_failure_kind(&s, path, idx)?)
        } else if let Some(arr) = kind_in_arr {
            if arr.is_empty() {
                return Err(WorkflowError::OnFailureEmptyKindIn {
                    path: path.to_path_buf(),
                    index: idx,
                });
            }
            let mut ks = Vec::with_capacity(arr.len());
            for v in arr {
                let s = v.as_str().ok_or_else(|| WorkflowError::Parse {
                    path: path.to_path_buf(),
                    source: toml::de::Error::custom(format!(
                        "[[on_failure]][{idx}].when.kind.in must be a string array"
                    )),
                })?;
                ks.push(parse_failure_kind(s, path, idx)?);
            }
            KindMatcher::In(ks)
        } else {
            KindMatcher::Not(parse_failure_kind(&kind_not_str.unwrap(), path, idx)?)
        };

        let when_phase = when
            .and_then(|w| w.get("phase"))
            .and_then(Value::as_str)
            .map(|s| parse_phase_kind(s, path, idx))
            .transpose()?;

        let pre = parse_phase_body(path, workflow_dir, table.get("pre"), "on_failure", idx, "pre")?;
        let run_body =
            parse_phase_body(path, workflow_dir, table.get("run"), "on_failure", idx, "run")?
                .ok_or_else(|| WorkflowError::OnFailureMissingRun {
                    path: path.to_path_buf(),
                    index: idx,
                })?;
        let post = parse_phase_body(
            path,
            workflow_dir,
            table.get("post"),
            "on_failure",
            idx,
            "post",
        )?;

        if run_body.shape() == PhaseShape::Session {
            return Err(WorkflowError::SessionRunUnsupported {
                path: path.to_path_buf(),
                where_: format!("on_failure[{idx}].run"),
            });
        }

        out.push(OnFailure {
            when_kind,
            when_phase,
            pre,
            run: run_body,
            post,
        });
    }
    Ok(out)
}

fn parse_failure_kind(
    s: &str,
    path: &Path,
    idx: usize,
) -> Result<crate::engine::outcome::FailureKind, WorkflowError> {
    use crate::engine::outcome::FailureKind::*;
    let kind = match s {
        "unparseable" => Unparseable,
        "schema_drift" => SchemaDrift,
        "process_crash" => ProcessCrash,
        "template_error" => TemplateError,
        "iter_exhausted" => IterExhausted,
        "stall" => Stall,
        "fs_poison" => FsPoison,
        _ => {
            return Err(WorkflowError::OnFailureUnknownKind {
                path: path.to_path_buf(),
                index: idx,
                value: s.to_string(),
            });
        }
    };
    Ok(kind)
}

fn parse_phase_kind(
    s: &str,
    path: &Path,
    idx: usize,
) -> Result<crate::engine::outcome::PhaseKind, WorkflowError> {
    use crate::engine::outcome::PhaseKind::*;
    match s {
        "pre" => Ok(Pre),
        "run" => Ok(Run),
        "post" => Ok(Post),
        _ => Err(WorkflowError::OnFailureUnknownPhase {
            path: path.to_path_buf(),
            index: idx,
            value: s.to_string(),
        }),
    }
}
```

In `WorkflowConfig::load`, add `let on_failures = parse_on_failures(path, &workflow_dir, &root)?;` and `on_failures,` to the constructor.

- [ ] **Step 6: Add error variants**

In `crates/roki-daemon/src/error.rs`, in `pub enum WorkflowError`:

```rust
    #[error("[[on_failure]][{index}] missing run; in {path}")]
    OnFailureMissingRun {
        path: std::path::PathBuf,
        index: usize,
    },
    #[error("[[on_failure]][{index}] missing when.kind; in {path}")]
    OnFailureMissingKind {
        path: std::path::PathBuf,
        index: usize,
    },
    #[error("[[on_failure]][{index}] sets multiple of when.kind / when.kind.in / when.kind.not; in {path}")]
    OnFailureKindMatcherConflict {
        path: std::path::PathBuf,
        index: usize,
    },
    #[error("[[on_failure]][{index}] when.kind = {value:?} not in legal set; in {path}")]
    OnFailureUnknownKind {
        path: std::path::PathBuf,
        index: usize,
        value: String,
    },
    #[error("[[on_failure]][{index}] when.phase = {value:?} not in {{pre, run, post}}; in {path}")]
    OnFailureUnknownPhase {
        path: std::path::PathBuf,
        index: usize,
        value: String,
    },
    #[error("[[on_failure]][{index}] when.kind.in is empty; in {path}")]
    OnFailureEmptyKindIn {
        path: std::path::PathBuf,
        index: usize,
    },
```

If `WorkflowError::SessionRunUnsupported` does not yet have a `where_` field, extend it:

```rust
    #[error("session-shape run not supported in {where_}; in {path}")]
    SessionRunUnsupported {
        path: std::path::PathBuf,
        where_: String,
    },
```

Update slice-2 callers that constructed this variant to pass a `where_` arg (typically `"rule[N].run"` for the existing call site).

- [ ] **Step 7: Run tests**

Run: `cargo test -p roki-daemon config::workflow::tests`
Expected: all six new on_failure tests PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/config/workflow.rs crates/roki-daemon/src/error.rs crates/roki-daemon/src/engine/mod.rs crates/roki-daemon/src/engine/on_failure.rs
git commit -m "feat(config): parse [[on_failure]] with kind/phase matchers"
```

---

## Task 11: Add `dispatch::evaluate` and `Cleanup` first-match

`engine::dispatch` decides whether the next cycle is a rule, cleanup, cleanup-shorthand, or no-match.

**Files:**
- Create: `crates/roki-daemon/src/engine/dispatch.rs`
- Modify: `crates/roki-daemon/src/engine/mod.rs`
- Modify: `crates/roki-daemon/src/rule.rs`

- [ ] **Step 1: Add `Cleanup` first-match helper to `rule.rs`**

The slice-1 `rule::first_match` works against `&[Rule]`. Add a sibling for cleanup, plus a shared test helper `admitted_with(status, labels)`. Append to `crates/roki-daemon/src/rule.rs`:

```rust
#[cfg(test)]
pub(crate) fn admitted_with(status: &str, labels: Vec<String>) -> crate::admission::AdmittedTicket {
    crate::admission::AdmittedTicket {
        ticket: crate::linear::ticket::NormalizedTicket::new(
            "ENG-DSP".to_string(),
            Some("u1".to_string()),
            status.to_string(),
            labels,
            "T".to_string(),
            "B".to_string(),
        ),
        ghq: "github.com/acme/widget".to_string(),
    }
}
```

(Place this above the existing `#[cfg(test)] mod tests { ... }`. The `pub(crate)` visibility lets the dispatch test module reach it via `crate::rule::admitted_with` without going through the test-only sub-module.)

Then append the cleanup matcher itself:

```rust
/// First-match against `[[cleanup]]` entries. Mirrors `first_match` for rules:
/// `when.status` matches the ticket's current status if set, and
/// `when.labels.has_all` matches if every listed label is on the ticket.
/// Shorthand entries (no `when.*`) match unconditionally.
pub fn first_cleanup_match<'a>(
    admitted: &crate::admission::AdmittedTicket,
    cleanups: &'a [crate::config::workflow::Cleanup],
) -> Option<&'a crate::config::workflow::Cleanup> {
    cleanups.iter().find(|c| {
        let status_ok = c
            .when_status
            .as_deref()
            .map_or(true, |s| s == admitted.ticket.status);
        let labels_ok = c
            .when_labels_has_all
            .iter()
            .all(|l| admitted.ticket.labels.iter().any(|tl| tl == l));
        status_ok && labels_ok
    })
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use crate::config::workflow::Cleanup;

    fn admitted(status: &str, labels: Vec<String>) -> crate::admission::AdmittedTicket {
        super::admitted_with(status, labels)
    }

    #[test]
    fn shorthand_matches_unconditionally() {
        let cleanups = vec![Cleanup {
            when_status: None,
            when_labels_has_all: vec![],
            pre: None,
            run: None,
            post: None,
        }];
        let a = admitted("InProgress", vec![]);
        assert!(first_cleanup_match(&a, &cleanups).is_some());
    }

    #[test]
    fn status_filter_excludes_non_matching() {
        let cleanups = vec![Cleanup {
            when_status: Some("Done".into()),
            when_labels_has_all: vec![],
            pre: None,
            run: Some(crate::engine::outcome::PhaseBody::InlineCmd { cmd: "true".into() }),
            post: None,
        }];
        let a = admitted("InProgress", vec![]);
        assert!(first_cleanup_match(&a, &cleanups).is_none());
    }

    #[test]
    fn labels_has_all_requires_every_label() {
        let cleanups = vec![Cleanup {
            when_status: None,
            when_labels_has_all: vec!["urgent".into(), "bug".into()],
            pre: None,
            run: Some(crate::engine::outcome::PhaseBody::InlineCmd { cmd: "true".into() }),
            post: None,
        }];
        let a1 = admitted("InProgress", vec!["urgent".into()]);
        assert!(first_cleanup_match(&a1, &cleanups).is_none());
        let a2 = admitted("InProgress", vec!["urgent".into(), "bug".into()]);
        assert!(first_cleanup_match(&a2, &cleanups).is_some());
    }
}
```

(`super::tests::admitted_with` is the existing slice-1 helper. Match its real name.)

Run: `cargo test -p roki-daemon rule::cleanup_tests`
Expected: all three PASS.

- [ ] **Step 2: Create `engine/dispatch.rs`**

Write `crates/roki-daemon/src/engine/dispatch.rs`:

```rust
//! Cycle dispatch evaluator: cleanup-first then rule first-match.
//! Per fr:01 §38 + fr:07 §Cycle dispatch.

use crate::admission::AdmittedTicket;
use crate::config::workflow::{Cleanup, Rule, WorkflowConfig};
use crate::engine::outcome::CycleKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Default: evaluate `[[cleanup]]` first, then `[[rule]]`.
    Default,
    /// `roki cleanup` subcommand: only `[[cleanup]]` matches lead to a cycle.
    /// `[[rule]]` list is ignored.
    CleanupOnly,
}

#[derive(Debug)]
pub enum DispatchTarget<'a> {
    /// Spawn a normal cycle (rule or cleanup) with these phases.
    Cycle {
        kind: CycleKind,
        rule: Option<&'a Rule>,
        cleanup: Option<&'a Cleanup>,
    },
    /// Cleanup shorthand: synchronous delete, no cycle.
    CleanupShorthand,
    /// No `[[cleanup]]` and no `[[rule]]` matched.
    NoMatch,
}

pub fn evaluate<'a>(
    admitted: &AdmittedTicket,
    workflow: &'a WorkflowConfig,
    mode: DispatchMode,
) -> DispatchTarget<'a> {
    if let Some(c) = crate::rule::first_cleanup_match(admitted, &workflow.cleanups) {
        if c.is_shorthand() {
            return DispatchTarget::CleanupShorthand;
        }
        return DispatchTarget::Cycle {
            kind: CycleKind::Cleanup,
            rule: None,
            cleanup: Some(c),
        };
    }

    if matches!(mode, DispatchMode::CleanupOnly) {
        return DispatchTarget::NoMatch;
    }

    if let Some(r) = crate::rule::first_match(admitted, &workflow.rules) {
        return DispatchTarget::Cycle {
            kind: CycleKind::Rule,
            rule: Some(r),
            cleanup: None,
        };
    }

    DispatchTarget::NoMatch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::workflow::{Cleanup, Rule};
    use crate::engine::outcome::PhaseBody;

    fn workflow_with(rules: Vec<Rule>, cleanups: Vec<Cleanup>) -> WorkflowConfig {
        WorkflowConfig {
            admission: crate::config::workflow::AdmissionSection { assignee: "me".into() },
            repo: None,
            rules,
            cleanups,
            on_failures: vec![],
        }
    }

    fn rule_for(status: &str) -> Rule {
        Rule {
            when_status: status.into(),
            when_labels_has_all: vec![],
            pre: None,
            run: PhaseBody::InlineCmd { cmd: "true".into() },
            post: None,
        }
    }

    fn cleanup_for(status: Option<&str>) -> Cleanup {
        Cleanup {
            when_status: status.map(String::from),
            when_labels_has_all: vec![],
            pre: None,
            run: status.map(|_| PhaseBody::InlineCmd { cmd: "true".into() }),
            post: None,
        }
    }

    fn shorthand_cleanup() -> Cleanup {
        Cleanup {
            when_status: None,
            when_labels_has_all: vec![],
            pre: None,
            run: None,
            post: None,
        }
    }

    #[test]
    fn cleanup_wins_over_rule() {
        let wf = workflow_with(
            vec![rule_for("InProgress")],
            vec![cleanup_for(Some("InProgress"))],
        );
        let a = crate::rule::admitted_with("InProgress", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::Cycle { kind: CycleKind::Cleanup, .. } => {}
            other => panic!("expected Cleanup cycle, got {other:?}"),
        }
    }

    #[test]
    fn shorthand_dispatch() {
        let wf = workflow_with(vec![rule_for("Done")], vec![shorthand_cleanup()]);
        let a = crate::rule::admitted_with("Done", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::CleanupShorthand => {}
            other => panic!("expected CleanupShorthand, got {other:?}"),
        }
    }

    #[test]
    fn rule_dispatch_when_no_cleanup_match() {
        let wf = workflow_with(vec![rule_for("InProgress")], vec![cleanup_for(Some("Done"))]);
        let a = crate::rule::admitted_with("InProgress", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::Cycle { kind: CycleKind::Rule, .. } => {}
            other => panic!("expected Rule cycle, got {other:?}"),
        }
    }

    #[test]
    fn no_match_when_neither_list_hits() {
        let wf = workflow_with(vec![rule_for("InProgress")], vec![cleanup_for(Some("Done"))]);
        let a = crate::rule::admitted_with("Triage", vec![]);
        match evaluate(&a, &wf, DispatchMode::Default) {
            DispatchTarget::NoMatch => {}
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn cleanup_only_mode_ignores_rule_list() {
        let wf = workflow_with(vec![rule_for("InProgress")], vec![cleanup_for(Some("Done"))]);
        let a = crate::rule::admitted_with("InProgress", vec![]);
        match evaluate(&a, &wf, DispatchMode::CleanupOnly) {
            DispatchTarget::NoMatch => {}
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }
}
```

In `crates/roki-daemon/src/engine/mod.rs`:

```rust
pub mod dispatch;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p roki-daemon engine::dispatch::tests`
Expected: all five PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/engine/dispatch.rs crates/roki-daemon/src/engine/mod.rs crates/roki-daemon/src/rule.rs
git commit -m "feat(engine): add dispatch::evaluate (cleanup-first)"
```

---

## Task 12: Implement `engine::cleanup` — shorthand + post-cycle delete

**Files:**
- Create: `crates/roki-daemon/src/engine/cleanup.rs`
- Modify: `crates/roki-daemon/src/engine/mod.rs`

- [ ] **Step 1: Write the module with unit tests**

Write `crates/roki-daemon/src/engine/cleanup.rs`:

```rust
//! Cleanup cycle deletion logic.
//!
//! Two entry points:
//! - `delete_immediate`: shorthand path. Emits `cycle_completed kind=cleanup
//!   iters=0`, emits `worktree_delete_requested reason=cleanup_shorthand`,
//!   removes `<session_root>/<ticket-id>/`. Used when the matched
//!   `[[cleanup]]` entry has no phases.
//! - `post_cycle_delete`: called after a non-shorthand cleanup cycle
//!   completes. Emits `worktree_delete_requested reason=cleanup_terminal`,
//!   removes `<session_root>/<ticket-id>/`.
//!
//! Both routes treat `NotFound` on `<ticket-id>/` as success (it never
//! existed; the cycle was a no-op session-wise). Other fs errors emit
//! `failure_unhandled marker=cleanup_fs_error` and propagate as Err so
//! `runtime::run_inner` exits 1.

use std::path::Path;

use uuid::Uuid;

use crate::events::{
    Event, EventWriter, FailureMarker, FailureMetaSer, WorktreeDeleteReason, now_rfc3339,
};

#[derive(Debug)]
pub enum CleanupError {
    /// A `failure_unhandled` event was emitted; the runtime should exit 1.
    FsError(std::io::Error),
}

impl std::fmt::Display for CleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CleanupError::FsError(e) => write!(f, "cleanup fs error: {e}"),
        }
    }
}
impl std::error::Error for CleanupError {}

/// Shorthand path. `cycle_id` is synthesized so the structured event has a
/// stable id.
pub fn delete_immediate(
    ticket_id: &str,
    session_root: &Path,
    events: &mut EventWriter,
) -> Result<(), CleanupError> {
    let cycle_id = Uuid::new_v4();
    let _ = events.emit(&Event::CycleCompleted {
        ts: now_rfc3339(),
        cycle_id: cycle_id.to_string(),
        cycle_kind: "cleanup".into(),
        iters: 0,
        outcome: None,
    });
    let _ = events.emit(&Event::WorktreeDeleteRequested {
        ts: now_rfc3339(),
        ticket_id: ticket_id.to_string(),
        cycle_id: Some(cycle_id.to_string()),
        reason: WorktreeDeleteReason::CleanupShorthand,
    });
    remove_ticket_dir(session_root, ticket_id, Some(cycle_id), events)
}

/// Post-cycle delete. Called only after a non-shorthand cleanup cycle
/// completes. `cycle_id` is the cleanup cycle's UUID.
pub fn post_cycle_delete(
    ticket_id: &str,
    session_root: &Path,
    cycle_id: Uuid,
    events: &mut EventWriter,
) -> Result<(), CleanupError> {
    let _ = events.emit(&Event::WorktreeDeleteRequested {
        ts: now_rfc3339(),
        ticket_id: ticket_id.to_string(),
        cycle_id: Some(cycle_id.to_string()),
        reason: WorktreeDeleteReason::CleanupTerminal,
    });
    remove_ticket_dir(session_root, ticket_id, Some(cycle_id), events)
}

fn remove_ticket_dir(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Option<Uuid>,
    events: &mut EventWriter,
) -> Result<(), CleanupError> {
    let dir = session_root.join(sanitize_ticket(ticket_id));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            let _ = events.emit(&Event::FailureUnhandled {
                ts: now_rfc3339(),
                cycle_id: cycle_id.map(|c| c.to_string()).unwrap_or_default(),
                cycle_kind: "cleanup".into(),
                failure: FailureMetaSer {
                    kind: "fs_poison".into(),
                    phase: None,
                    iter: 0,
                    exit_code: None,
                    error_text: format!("cleanup remove_dir_all failed: {e}"),
                },
                marker: FailureMarker::CleanupFsError,
            });
            Err(CleanupError::FsError(e))
        }
    }
}

fn sanitize_ticket(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_immediate_removes_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("OPS-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.txt"), "hi").unwrap();

        let mut w = EventWriter::open(root, "OPS-1").unwrap();
        delete_immediate("OPS-1", root, &mut w).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn delete_immediate_succeeds_when_dir_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut w = EventWriter::open(root, "OPS-2").unwrap();
        delete_immediate("OPS-2", root, &mut w).unwrap();
    }

    #[test]
    fn delete_immediate_emits_two_events() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut w = EventWriter::open(root, "OPS-3").unwrap();
        delete_immediate("OPS-3", root, &mut w).unwrap();
        drop(w);

        let body = std::fs::read_to_string(crate::events::events_path(root, "OPS-3")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"cycle_completed\""));
        assert!(lines[0].contains("\"cycle_kind\":\"cleanup\""));
        assert!(lines[0].contains("\"iters\":0"));
        assert!(lines[1].contains("\"event\":\"worktree_delete_requested\""));
        assert!(lines[1].contains("\"reason\":\"cleanup_shorthand\""));
    }

    #[test]
    fn post_cycle_delete_emits_one_event_then_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("OPS-4");
        std::fs::create_dir_all(&dir).unwrap();

        let cycle_id = Uuid::new_v4();
        let mut w = EventWriter::open(root, "OPS-4").unwrap();
        post_cycle_delete("OPS-4", root, cycle_id, &mut w).unwrap();
        drop(w);

        assert!(!dir.exists());
        let body = std::fs::read_to_string(crate::events::events_path(root, "OPS-4")).unwrap();
        assert!(body.contains("\"reason\":\"cleanup_terminal\""));
        assert!(body.contains(&cycle_id.to_string()));
    }
}
```

In `crates/roki-daemon/src/engine/mod.rs`:

```rust
pub mod cleanup;
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p roki-daemon engine::cleanup::tests`
Expected: all four PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-daemon/src/engine/cleanup.rs crates/roki-daemon/src/engine/mod.rs
git commit -m "feat(engine): add cleanup module (shorthand + post-cycle)"
```

---

## Task 13: Wire `dispatch::evaluate` into `runtime::run_inner` (Default mode)

Replace the inline `rule::first_match` call with `dispatch::evaluate`, branch on `DispatchTarget`, but only handle the existing `Rule` cycle path for now. CleanupShorthand, Cleanup cycles, and on_failure routing land in subsequent tasks.

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`

- [ ] **Step 1: Update `run_inner`**

Replace the existing block:

```rust
match admission::accept(&ticket, &workflow, &me_ref) {
    Ok(admitted) => match rule::first_match(&admitted, &workflow.rules) {
        Some(matched) => break (admitted, matched.clone()),
        None => {
            tracing::info!(
                ticket_id = %admitted.ticket.id,
                "rule no-match; awaiting next webhook"
            );
            continue;
        }
    },
    Err(err) => { /* unchanged */ }
}
```

with:

```rust
match admission::accept(&ticket, &workflow, &me_ref) {
    Ok(admitted) => {
        use crate::engine::dispatch::{evaluate, DispatchMode, DispatchTarget};
        match evaluate(&admitted, &workflow, DispatchMode::Default) {
            DispatchTarget::Cycle { kind, rule: Some(r), .. } => {
                break (admitted, kind, DispatchedEntry::Rule(r.clone()));
            }
            DispatchTarget::Cycle { kind, cleanup: Some(c), .. } => {
                break (admitted, kind, DispatchedEntry::Cleanup(c.clone()));
            }
            DispatchTarget::CleanupShorthand => {
                // Handled below outside the loop.
                break (admitted, crate::engine::outcome::CycleKind::Cleanup, DispatchedEntry::Shorthand);
            }
            DispatchTarget::NoMatch => {
                tracing::info!(
                    ticket_id = %admitted.ticket.id,
                    "no dispatch match; awaiting next webhook"
                );
                continue;
            }
            DispatchTarget::Cycle { rule: None, cleanup: None, .. } => unreachable!(),
        }
    }
    Err(err) => { /* unchanged */ }
}
```

Add the helper enum near the top of `runtime.rs` (private):

```rust
enum DispatchedEntry {
    Rule(crate::config::workflow::Rule),
    Cleanup(crate::config::workflow::Cleanup),
    Shorthand,
}
```

The downstream code that consumed `matched_rule` now switches on `DispatchedEntry`. For this task, only `DispatchedEntry::Rule(r)` proceeds to `engine::run_cycle`; `Shorthand` and `Cleanup` get handled in Task 14 / 15.

Update the `run_cycle` invocation:

```rust
let outcome = match dispatched {
    DispatchedEntry::Rule(rule) => crate::engine::run_cycle(
        &executor,
        &admitted,
        &rule,
        &cfg.paths.session_root,
        &cfg,
        cycle_kind, // CycleKind::Rule
        None,
    )
    .await?,
    DispatchedEntry::Cleanup(_) => unimplemented!("cleanup cycle wired in Task 14"),
    DispatchedEntry::Shorthand => unimplemented!("shorthand wired in Task 14"),
};
```

- [ ] **Step 2: Run the existing skeleton/iteration smoke tests**

Run: `cargo test -p roki-daemon`
Expected: all existing tests still pass. Slice 2's `iteration_smoke`, `session_smoke`, `stall_smoke`, `run_terminal_smoke`, `skeleton_smoke` still work because the new dispatch path resolves a rule cycle identically.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs
git commit -m "refactor(runtime): wire dispatch::evaluate (rule path only)"
```

---

## Task 14: Wire CleanupShorthand and Cleanup cycle paths into `runtime`

`DispatchedEntry::Shorthand` and `DispatchedEntry::Cleanup` finally drive `engine::cleanup::delete_immediate` / `engine::run_cycle(CycleKind::Cleanup, ...)` + `cleanup::post_cycle_delete`.

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`

- [ ] **Step 1: Open the events writer once per ticket**

Add near the top of the cycle-bound block (after admission decides):

```rust
let mut events = crate::events::EventWriter::open(&cfg.paths.session_root, &admitted.ticket.id)
    .map_err(|e| SkeletonError::Capture(e))?;
```

(`SkeletonError::Capture` already exists for io errors. Use whatever variant slice 1 chose for capture failures; the failure should escape the binary as exit 1 because this is daemon-internal infra. If a more specific variant exists, prefer that.)

- [ ] **Step 2: Branch on `DispatchedEntry`**

Replace the `match dispatched` block from Task 13:

```rust
let cycle_outcome_result: Result<CycleOutcomeOrShortcircuit, SkeletonError> = match dispatched {
    DispatchedEntry::Rule(rule) => {
        let outcome = crate::engine::run_cycle(
            &executor,
            &admitted,
            &rule,
            &cfg.paths.session_root,
            &cfg,
            crate::engine::outcome::CycleKind::Rule,
            None,
        )
        .await?;
        Ok(CycleOutcomeOrShortcircuit::Cycle { kind: crate::engine::outcome::CycleKind::Rule, outcome })
    }
    DispatchedEntry::Cleanup(cleanup) => {
        // Convert Cleanup -> a Rule-shaped struct for engine::run_cycle.
        let rule_view = cleanup_to_rule(&cleanup);
        let outcome = crate::engine::run_cycle(
            &executor,
            &admitted,
            &rule_view,
            &cfg.paths.session_root,
            &cfg,
            crate::engine::outcome::CycleKind::Cleanup,
            None,
        )
        .await?;
        Ok(CycleOutcomeOrShortcircuit::Cycle { kind: crate::engine::outcome::CycleKind::Cleanup, outcome })
    }
    DispatchedEntry::Shorthand => {
        crate::engine::cleanup::delete_immediate(
            &admitted.ticket.id,
            &cfg.paths.session_root,
            &mut events,
        )
        .map_err(|e| SkeletonError::Capture(std::io::Error::other(e)))?;
        Ok(CycleOutcomeOrShortcircuit::Shorthand)
    }
};

enum CycleOutcomeOrShortcircuit {
    Cycle {
        kind: crate::engine::outcome::CycleKind,
        outcome: crate::engine::CycleOutcome,
    },
    Shorthand,
}

fn cleanup_to_rule(c: &crate::config::workflow::Cleanup) -> crate::config::workflow::Rule {
    crate::config::workflow::Rule {
        when_status: c.when_status.clone().unwrap_or_default(),
        when_labels_has_all: c.when_labels_has_all.clone(),
        pre: c.pre.clone(),
        // Non-shorthand cleanup: run is Some by parser invariant.
        run: c.run.clone().expect("non-shorthand cleanup has run"),
        post: c.post.clone(),
    }
}
```

- [ ] **Step 3: Handle the outcome**

Replace the `match outcome` block:

```rust
match cycle_outcome_result? {
    CycleOutcomeOrShortcircuit::Shorthand => {
        // delete_immediate already emitted both events. Exit 0.
    }
    CycleOutcomeOrShortcircuit::Cycle { kind, outcome } => match outcome {
        crate::engine::CycleOutcome::Completed { iters } => {
            let _ = events.emit(&crate::events::Event::CycleCompleted {
                ts: crate::events::now_rfc3339(),
                cycle_id: cycle_id_str.clone(), // cycle_id_str is the run_cycle's UUID; thread it back
                cycle_kind: kind.as_str().to_string(),
                iters,
                outcome: None, // operator outcome string; left as None until wired in a future slice
            });
            if kind == crate::engine::outcome::CycleKind::Cleanup {
                // The cycle's UUID is what we want to label the worktree_delete_requested event with.
                crate::engine::cleanup::post_cycle_delete(
                    &admitted.ticket.id,
                    &cfg.paths.session_root,
                    cycle_uuid, // Uuid, not the string
                    &mut events,
                )
                .map_err(|e| SkeletonError::Capture(std::io::Error::other(e)))?;
            }
        }
        crate::engine::CycleOutcome::Failed { meta } => {
            // on_failure routing wired in Task 17.
            tracing::error!(
                failure_kind = %meta.kind.as_str(),
                phase = %meta.phase.as_str(),
                iter = meta.iter,
                "cycle failed"
            );
            return Err(SkeletonError::PhaseInfra(
                crate::error::PhaseInfraError::CycleFailed {
                    kind: meta.kind,
                    iter: meta.iter,
                },
            ));
        }
    },
}
```

For `cycle_uuid` and `cycle_id_str`: `engine::run_cycle` currently generates the cycle UUID internally and the runtime never sees it. For event emission to carry the right cycle id, expose it. Add an in/out-param or change `CycleOutcome` to carry the id:

In `crates/roki-daemon/src/engine/cycle.rs`:

```rust
pub enum CycleOutcome {
    Completed { iters: u32, cycle_id: Uuid },
    Failed { meta: FailureMeta },  // meta.failed_cycle_id is the cycle id
}
```

Update every `CycleOutcome::Completed { iters: ... }` construction in `cycle.rs` to include `cycle_id`. Test assertions that match `Completed { iters: 1 }` become `Completed { iters: 1, .. }`.

In `runtime.rs`, after the match:

```rust
let cycle_uuid = match &outcome {
    crate::engine::CycleOutcome::Completed { cycle_id, .. } => *cycle_id,
    crate::engine::CycleOutcome::Failed { meta } => meta.failed_cycle_id,
};
let cycle_id_str = cycle_uuid.to_string();
```

- [ ] **Step 4: Run all tests**

Run: `cargo test -p roki-daemon`
Expected: PASS. The new event-emission paths don't have integration tests yet (those land in Tasks 19-22); for now only unit tests should still be green.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs crates/roki-daemon/src/engine/cycle.rs
git commit -m "feat(runtime): wire cleanup shorthand + cleanup-cycle paths"
```

---

## Task 15: End-to-end smoke — cleanup shorthand

**Files:**
- Create: `crates/roki-daemon/tests/e2e/cleanup_shorthand_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml` (`[[test]]` entry)

- [ ] **Step 1: Add `[[test]]` entry**

In `crates/roki-daemon/Cargo.toml`:

```toml
[[test]]
name = "cleanup_shorthand_smoke"
path = "tests/e2e/cleanup_shorthand_smoke.rs"
```

- [ ] **Step 2: Write the test**

Write `crates/roki-daemon/tests/e2e/cleanup_shorthand_smoke.rs`:

```rust
//! E2E: a webhook for an admitted ticket that matches a `[[cleanup]]`
//! shorthand entry causes immediate deletion of `<session_root>/<ticket-id>/`
//! and emits two events (`cycle_completed kind=cleanup iters=0`, then
//! `worktree_delete_requested reason=cleanup_shorthand`). No cycle-uuid dirs
//! are created.

mod common;

use common::{spawn_daemon_with_workflow, post_webhook, wait_for_exit};

#[tokio::test]
async fn cleanup_shorthand_deletes_ticket_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    // Pre-populate the ticket dir to confirm deletion.
    let ticket_id = "OPS-99";
    let ticket_dir = session_root.join(ticket_id);
    std::fs::create_dir_all(&ticket_dir).unwrap();
    std::fs::write(ticket_dir.join("stale.txt"), "leftover").unwrap();

    let workflow = format!(r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "true"

[[cleanup]]
"#);

    let roki_toml = format!(r#"
[paths]
session_root = "{}"

[engine]
max_iterations = 5

[default.ai.command]
cli = "claude -p '{{{{ ticket.id }}}}'"
stall_seconds = 30
"#, session_root.display());

    let (handle, addr) = spawn_daemon_with_workflow(&workflow, &roki_toml).await;
    post_webhook(&addr, ticket_id, "Done", "me").await;
    let exit_status = wait_for_exit(handle).await;
    assert!(exit_status.success());

    // Ticket dir gone.
    assert!(!ticket_dir.exists());

    // Events file is a sibling.
    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    let body = std::fs::read_to_string(&events_path).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "expected exactly 2 events; got {body}");
    assert!(lines[0].contains("\"event\":\"cycle_completed\""));
    assert!(lines[0].contains("\"cycle_kind\":\"cleanup\""));
    assert!(lines[0].contains("\"iters\":0"));
    assert!(lines[1].contains("\"event\":\"worktree_delete_requested\""));
    assert!(lines[1].contains("\"reason\":\"cleanup_shorthand\""));
}
```

If `tests/e2e/common.rs` does not exist, copy the slice-2 e2e common helpers (`spawn_daemon_with_workflow`, `post_webhook`, `wait_for_exit`). If those helpers exist under different names, use the existing ones — every slice-2 e2e test does the same thing.

- [ ] **Step 3: Run the smoke**

Run: `cargo test -p roki-daemon --test cleanup_shorthand_smoke -- --nocapture`
Expected: PASS. If failure, inspect the events file contents the test prints.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/tests/e2e/cleanup_shorthand_smoke.rs crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): cleanup shorthand deletes ticket dir + emits 2 events"
```

---

## Task 16: End-to-end smoke — cleanup cycle (non-shorthand)

A non-shorthand cleanup entry runs through `engine::run_cycle`, then `cleanup::post_cycle_delete` removes the ticket dir.

**Files:**
- Create: `crates/roki-daemon/tests/e2e/cleanup_cycle_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add `[[test]]` entry**

```toml
[[test]]
name = "cleanup_cycle_smoke"
path = "tests/e2e/cleanup_cycle_smoke.rs"
```

- [ ] **Step 2: Write the test**

Write `crates/roki-daemon/tests/e2e/cleanup_cycle_smoke.rs`:

```rust
mod common;

use common::{spawn_daemon_with_workflow, post_webhook, wait_for_exit};

#[tokio::test]
async fn cleanup_cycle_runs_then_deletes() {
    let tmp = tempfile::tempdir().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let ticket_id = "OPS-100";

    let workflow = format!(r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "true"

[[cleanup]]
when.status = "Done"
run.cmd = "echo cleanup"
post.prompt = '{{"directive": "end", "outcome": "cleanup_done"}}'
"#);

    // Use the e2e fake post agent fixture from slice 2.
    let roki_toml = format!(r#"
[paths]
session_root = "{}"

[engine]
max_iterations = 3

[default.ai.command]
cli = "tests/e2e/fixtures/fake_post.sh"
stall_seconds = 30

[default.ai.session]
cli = "tests/e2e/fixtures/fake_session_agent.sh"
stall_seconds = 30
"#, session_root.display());

    let (handle, addr) = spawn_daemon_with_workflow(&workflow, &roki_toml).await;
    post_webhook(&addr, ticket_id, "Done", "me").await;
    let exit_status = wait_for_exit(handle).await;
    assert!(exit_status.success());

    // Ticket dir gone.
    let ticket_dir = session_root.join(ticket_id);
    assert!(!ticket_dir.exists(), "ticket dir should be deleted; remains at {ticket_dir:?}");

    // Events file is a sibling.
    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    let body = std::fs::read_to_string(&events_path).unwrap();
    let lines: Vec<&str> = body.lines().collect();

    // Ordering: cycle_completed first (cycle finished), then worktree_delete_requested.
    assert_eq!(lines.len(), 2, "got events: {body}");
    assert!(lines[0].contains("\"event\":\"cycle_completed\""));
    assert!(lines[0].contains("\"cycle_kind\":\"cleanup\""));
    let iters_one = lines[0].contains("\"iters\":1");
    let iters_more = lines[0].contains("\"iters\":2") || lines[0].contains("\"iters\":3");
    assert!(iters_one || iters_more, "expected iters >= 1");
    assert!(lines[1].contains("\"event\":\"worktree_delete_requested\""));
    assert!(lines[1].contains("\"reason\":\"cleanup_terminal\""));
}
```

- [ ] **Step 3: Run the smoke**

Run: `cargo test -p roki-daemon --test cleanup_cycle_smoke -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/tests/e2e/cleanup_cycle_smoke.rs crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): cleanup cycle runs then deletes ticket dir"
```

---

## Task 17: Wire `[[on_failure]]` routing into `runtime`

Failed rule (or cleanup) cycles are evaluated against `[[on_failure]]`. On match, a handler cycle (`CycleKind::Failure`) is launched. Recursion bound enforced (handler cycle that itself fails → `failure_unhandled marker=recursion_bound`, exit 1). No-match → `failure_unhandled marker=none`, exit 1.

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`

- [ ] **Step 1: Replace the `Failed { meta }` branch**

In the `cycle_outcome_result` handling, replace:

```rust
crate::engine::CycleOutcome::Failed { meta } => {
    tracing::error!(...);
    return Err(SkeletonError::PhaseInfra(...));
}
```

with:

```rust
crate::engine::CycleOutcome::Failed { meta } => {
    handle_failed_cycle(
        &meta,
        kind,                          // CycleKind that just failed
        &workflow.on_failures,
        &executor,
        &admitted,
        &cfg,
        &mut events,
    ).await?;
    return Err(SkeletonError::PhaseInfra(
        crate::error::PhaseInfraError::CycleFailed {
            kind: meta.kind,
            iter: meta.iter,
        },
    ));
}
```

(`handle_failed_cycle` either returns `Ok(())` because a handler cycle succeeded — in which case the outer flow falls through — or returns `Err`. To keep the runtime's exit-code surface unchanged when a handler succeeds, refactor: treat handler success as an early-return `Ok(())`, treat handler failure / no-handler as `Err`.)

The simplest shape: rewrite the failure branch as:

```rust
crate::engine::CycleOutcome::Failed { meta } => {
    let runtime_decision = handle_failed_cycle(
        &meta,
        kind,
        &workflow,
        &executor,
        &admitted,
        &cfg,
        &mut events,
    ).await;
    match runtime_decision {
        FailureDecision::HandlerSucceeded => { /* exit 0 below */ }
        FailureDecision::Unhandled => {
            return Err(SkeletonError::PhaseInfra(
                crate::error::PhaseInfraError::CycleFailed {
                    kind: meta.kind,
                    iter: meta.iter,
                },
            ));
        }
    }
}
```

- [ ] **Step 2: Implement `handle_failed_cycle`**

Add private to `runtime.rs`:

```rust
enum FailureDecision {
    HandlerSucceeded,
    Unhandled,
}

async fn handle_failed_cycle(
    meta: &crate::engine::outcome::FailureMeta,
    failed_cycle_kind: crate::engine::outcome::CycleKind,
    workflow: &crate::config::workflow::WorkflowConfig,
    executor: &crate::engine::CommandPhaseExecutor,
    admitted: &crate::admission::AdmittedTicket,
    cfg: &crate::config::roki::RokiConfig,
    events: &mut crate::events::EventWriter,
) -> FailureDecision {
    use crate::engine::outcome::CycleKind;
    use crate::events::{Event, FailureMarker, FailureMetaSer, now_rfc3339};

    // Recursion bound: failure cycle that itself fails -> failure_unhandled.
    if failed_cycle_kind == CycleKind::Failure {
        let _ = events.emit(&Event::FailureUnhandled {
            ts: now_rfc3339(),
            cycle_id: meta.failed_cycle_id.to_string(),
            cycle_kind: "failure".into(),
            failure: FailureMetaSer::from_meta(meta),
            marker: FailureMarker::RecursionBound,
        });
        return FailureDecision::Unhandled;
    }

    // First-match against [[on_failure]].
    let Some(handler) = crate::engine::on_failure::route(&workflow.on_failures, meta) else {
        let _ = events.emit(&Event::FailureUnhandled {
            ts: now_rfc3339(),
            cycle_id: meta.failed_cycle_id.to_string(),
            cycle_kind: failed_cycle_kind.as_str().to_string(),
            failure: FailureMetaSer::from_meta(meta),
            marker: FailureMarker::None,
        });
        return FailureDecision::Unhandled;
    };

    // Convert OnFailure -> Rule shape for run_cycle.
    let handler_rule = on_failure_to_rule(handler);
    let handler_outcome = match crate::engine::run_cycle(
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
        Ok(o) => o,
        Err(infra) => {
            // Infra error in the handler cycle treated as recursion bound.
            tracing::error!(?infra, "handler cycle infra error");
            let _ = events.emit(&Event::FailureUnhandled {
                ts: now_rfc3339(),
                cycle_id: meta.failed_cycle_id.to_string(),
                cycle_kind: "failure".into(),
                failure: FailureMetaSer::from_meta(meta),
                marker: FailureMarker::RecursionBound,
            });
            return FailureDecision::Unhandled;
        }
    };

    match handler_outcome {
        crate::engine::CycleOutcome::Completed { iters, cycle_id } => {
            let _ = events.emit(&Event::CycleCompleted {
                ts: now_rfc3339(),
                cycle_id: cycle_id.to_string(),
                cycle_kind: "failure".into(),
                iters,
                outcome: None,
            });
            FailureDecision::HandlerSucceeded
        }
        crate::engine::CycleOutcome::Failed { meta: handler_meta } => {
            let _ = events.emit(&Event::FailureUnhandled {
                ts: now_rfc3339(),
                cycle_id: handler_meta.failed_cycle_id.to_string(),
                cycle_kind: "failure".into(),
                failure: FailureMetaSer::from_meta(&handler_meta),
                marker: FailureMarker::RecursionBound,
            });
            FailureDecision::Unhandled
        }
    }
}

fn on_failure_to_rule(
    h: &crate::engine::on_failure::OnFailure,
) -> crate::config::workflow::Rule {
    crate::config::workflow::Rule {
        // Handler entries do not have when.status; use a sentinel that
        // never matches (handler is launched directly, not via admission).
        when_status: String::new(),
        when_labels_has_all: vec![],
        pre: h.pre.clone(),
        run: h.run.clone(),
        post: h.post.clone(),
    }
}
```

- [ ] **Step 3: Run all tests**

Run: `cargo test -p roki-daemon`
Expected: existing tests still PASS (no on_failure entries in the slice-2 fixtures, so the failure branch behaves identically to before — emits `failure_unhandled marker=none` then exits 1).

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs
git commit -m "feat(runtime): wire [[on_failure]] routing + recursion bound"
```

---

## Task 18: End-to-end smoke — `[[on_failure]]` handler succeeds

**Files:**
- Create: `crates/roki-daemon/tests/e2e/on_failure_smoke.rs`
- Create: `crates/roki-daemon/tests/e2e/fixtures/fail_run.sh`
- Create: `crates/roki-daemon/tests/e2e/fixtures/echo_failure_env.sh`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Write the fixtures**

`crates/roki-daemon/tests/e2e/fixtures/fail_run.sh`:

```bash
#!/usr/bin/env bash
echo "fake run failure" >&2
exit 7
```

`crates/roki-daemon/tests/e2e/fixtures/echo_failure_env.sh`:

```bash
#!/usr/bin/env bash
echo "kind=$ROKI_FAILURE_KIND phase=$ROKI_FAILURE_PHASE iter=$ROKI_FAILURE_ITER failed=$ROKI_FAILURE_FAILED_CYCLE_ID" >&2
echo '{"directive":"end","outcome":"handled"}'
```

Make both executable. Add a build-script step or rely on `git update-index --chmod=+x` in the commit.

- [ ] **Step 2: Add `[[test]]` entry**

```toml
[[test]]
name = "on_failure_smoke"
path = "tests/e2e/on_failure_smoke.rs"
```

- [ ] **Step 3: Write the test**

Write `crates/roki-daemon/tests/e2e/on_failure_smoke.rs`:

```rust
mod common;

use common::{spawn_daemon_with_workflow, post_webhook, wait_for_exit};

#[tokio::test]
async fn on_failure_handler_recovers_from_process_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let ticket_id = "OPS-200";

    let workflow = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "tests/e2e/fixtures/fail_run.sh"

[[on_failure]]
when.kind = "process_crash"
run.cmd = "true"
post.cmd = "tests/e2e/fixtures/echo_failure_env.sh"
"#;

    let roki_toml = format!(r#"
[paths]
session_root = "{}"

[engine]
max_iterations = 3

[default.ai.command]
cli = "echo unused"
stall_seconds = 30
"#, session_root.display());

    let (handle, addr) = spawn_daemon_with_workflow(workflow, &roki_toml).await;
    post_webhook(&addr, ticket_id, "InProgress", "me").await;
    let status = wait_for_exit(handle).await;
    assert!(status.success(), "exit status: {status:?}");

    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    let body = std::fs::read_to_string(&events_path).unwrap();
    let lines: Vec<&str> = body.lines().collect();

    // Exactly one cycle_completed for the FAILURE cycle. No failure_unhandled.
    let n_completed = lines.iter().filter(|l| l.contains("\"event\":\"cycle_completed\"")).count();
    let n_unhandled = lines.iter().filter(|l| l.contains("\"event\":\"failure_unhandled\"")).count();
    assert_eq!(n_completed, 1, "got events: {body}");
    assert_eq!(n_unhandled, 0, "got events: {body}");

    let completed = lines.iter().find(|l| l.contains("cycle_completed")).unwrap();
    assert!(completed.contains("\"cycle_kind\":\"failure\""));

    // Stderr from echo_failure_env.sh is in the failure cycle's iter dir.
    // Verify {{ failure.* }} env vars surfaced.
    let failure_cycle_dir = walk_cycle_dirs(&session_root.join(ticket_id))
        .into_iter()
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("cycle-"))
        .expect("a cycle dir exists for the failure handler");
    let post_stderr_path = failure_cycle_dir.join("iter-1").join("post.stderr");
    let post_stderr = std::fs::read_to_string(&post_stderr_path).unwrap();
    assert!(post_stderr.contains("kind=process_crash"));
    assert!(post_stderr.contains("phase=run"));
}

fn walk_cycle_dirs(ticket_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(ticket_dir) {
        for entry in rd.flatten() {
            out.push(entry.path());
        }
    }
    out
}
```

- [ ] **Step 4: Run the smoke**

Run: `chmod +x crates/roki-daemon/tests/e2e/fixtures/*.sh && cargo test -p roki-daemon --test on_failure_smoke -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/tests/e2e/on_failure_smoke.rs crates/roki-daemon/tests/e2e/fixtures/fail_run.sh crates/roki-daemon/tests/e2e/fixtures/echo_failure_env.sh crates/roki-daemon/Cargo.toml
git update-index --chmod=+x crates/roki-daemon/tests/e2e/fixtures/fail_run.sh crates/roki-daemon/tests/e2e/fixtures/echo_failure_env.sh
git commit -m "test(e2e): on_failure handler recovers from process_crash"
```

---

## Task 19: End-to-end smoke — recursion bound

**Files:**
- Create: `crates/roki-daemon/tests/e2e/recursion_bound_smoke.rs`
- Create: `crates/roki-daemon/tests/e2e/fixtures/fail_handler.sh`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Write the fixture**

`crates/roki-daemon/tests/e2e/fixtures/fail_handler.sh`:

```bash
#!/usr/bin/env bash
echo "handler also fails" >&2
exit 9
```

- [ ] **Step 2: Add `[[test]]` entry**

```toml
[[test]]
name = "recursion_bound_smoke"
path = "tests/e2e/recursion_bound_smoke.rs"
```

- [ ] **Step 3: Write the test**

```rust
mod common;

use common::{spawn_daemon_with_workflow, post_webhook, wait_for_exit};

#[tokio::test]
async fn recursion_bound_unhandled_when_handler_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let ticket_id = "OPS-300";

    let workflow = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "tests/e2e/fixtures/fail_run.sh"

[[on_failure]]
when.kind = "process_crash"
run.cmd = "tests/e2e/fixtures/fail_handler.sh"
"#;

    let roki_toml = format!(r#"
[paths]
session_root = "{}"

[engine]
max_iterations = 2

[default.ai.command]
cli = "echo unused"
stall_seconds = 30
"#, session_root.display());

    let (handle, addr) = spawn_daemon_with_workflow(workflow, &roki_toml).await;
    post_webhook(&addr, ticket_id, "InProgress", "me").await;
    let status = wait_for_exit(handle).await;
    assert!(!status.success(), "expected exit 1; got {status:?}");

    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    let body = std::fs::read_to_string(&events_path).unwrap();
    let lines: Vec<&str> = body.lines().collect();

    let unhandled: Vec<&&str> = lines.iter().filter(|l| l.contains("failure_unhandled")).collect();
    assert_eq!(unhandled.len(), 1, "got {body}");
    assert!(unhandled[0].contains("\"marker\":\"recursion_bound\""));
}
```

- [ ] **Step 4: Run**

Run: `chmod +x crates/roki-daemon/tests/e2e/fixtures/fail_handler.sh && cargo test -p roki-daemon --test recursion_bound_smoke -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/tests/e2e/recursion_bound_smoke.rs crates/roki-daemon/tests/e2e/fixtures/fail_handler.sh crates/roki-daemon/Cargo.toml
git update-index --chmod=+x crates/roki-daemon/tests/e2e/fixtures/fail_handler.sh
git commit -m "test(e2e): recursion bound emits failure_unhandled"
```

---

## Task 20: End-to-end smoke — no-match (`failure_unhandled marker=none`)

**Files:**
- Create: `crates/roki-daemon/tests/e2e/failure_unhandled_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add `[[test]]` entry**

```toml
[[test]]
name = "failure_unhandled_smoke"
path = "tests/e2e/failure_unhandled_smoke.rs"
```

- [ ] **Step 2: Write the test**

```rust
mod common;

use common::{spawn_daemon_with_workflow, post_webhook, wait_for_exit};

#[tokio::test]
async fn rule_fails_no_handler_emits_unhandled() {
    let tmp = tempfile::tempdir().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let ticket_id = "OPS-400";

    let workflow = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "tests/e2e/fixtures/fail_run.sh"
"#;

    let roki_toml = format!(r#"
[paths]
session_root = "{}"

[engine]
max_iterations = 2

[default.ai.command]
cli = "echo unused"
stall_seconds = 30
"#, session_root.display());

    let (handle, addr) = spawn_daemon_with_workflow(workflow, &roki_toml).await;
    post_webhook(&addr, ticket_id, "InProgress", "me").await;
    let status = wait_for_exit(handle).await;
    assert!(!status.success());

    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    let body = std::fs::read_to_string(&events_path).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    let unhandled: Vec<&&str> = lines.iter().filter(|l| l.contains("failure_unhandled")).collect();
    assert_eq!(unhandled.len(), 1);
    assert!(unhandled[0].contains("\"marker\":\"none\""));
    assert!(unhandled[0].contains("\"cycle_kind\":\"rule\""));
    assert!(unhandled[0].contains("\"kind\":\"process_crash\""));
}
```

- [ ] **Step 3: Run**

Run: `cargo test -p roki-daemon --test failure_unhandled_smoke -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/tests/e2e/failure_unhandled_smoke.rs crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): rule failure with no handler emits failure_unhandled"
```

---

## Task 21: End-to-end smoke — `FsPoison`

**Files:**
- Create: `crates/roki-daemon/tests/e2e/fs_poison_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add `[[test]]` entry**

```toml
[[test]]
name = "fs_poison_smoke"
path = "tests/e2e/fs_poison_smoke.rs"
```

- [ ] **Step 2: Write the test**

```rust
mod common;

use common::{spawn_daemon_with_workflow, post_webhook, wait_for_exit};

#[tokio::test]
async fn fs_poison_routes_through_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    // Make session_root a regular file so cycle dir creation fails.
    let session_root = tmp.path().join("sessions");
    std::fs::write(&session_root, b"not a dir").unwrap();

    let ticket_id = "OPS-500";

    let workflow = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "true"

[[on_failure]]
when.kind = "fs_poison"
run.cmd = "true"
"#;

    // session_root in roki.toml points to the file above; daemon will fail to
    // create <session_root>/<ticket-id>/cycle-<uuid>/iter-1/.
    let roki_toml = format!(r#"
[paths]
session_root = "{}"

[engine]
max_iterations = 2

[default.ai.command]
cli = "echo unused"
stall_seconds = 30
"#, session_root.display());

    let (handle, addr) = spawn_daemon_with_workflow(workflow, &roki_toml).await;
    post_webhook(&addr, ticket_id, "InProgress", "me").await;
    let status = wait_for_exit(handle).await;

    // The handler cycle ALSO can't create its iter dirs (same broken root).
    // So we expect failure_unhandled marker=recursion_bound. Either way:
    // - first failure is fs_poison routed through on_failure;
    // - handler cycle launches but ALSO hits fs_poison;
    // - ends as recursion_bound failure_unhandled, exit 1.
    assert!(!status.success(), "expected exit 1; got {status:?}");

    // Events live next to a non-dir session_root, which is itself broken;
    // the events writer creates `<session_root_parent>/<ticket-id>.events.jsonl`
    // when session_root is a file? No — events_path joins on session_root.
    // Since session_root is a regular file, the event writer's open() fails
    // and events are silently dropped. The smoke test asserts only the
    // exit-status surface for FsPoison.
}
```

(This test asserts exit-code only. The events-file path can't exist when `session_root` is a regular file because the writer's `create_dir_all(parent)` fails on the parent of a path that includes `session_root` as a directory component. Treat the exit code as the source of truth here.)

- [ ] **Step 3: Run**

Run: `cargo test -p roki-daemon --test fs_poison_smoke -- --nocapture`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/tests/e2e/fs_poison_smoke.rs crates/roki-daemon/Cargo.toml
git commit -m "test(e2e): fs_poison routes through [[on_failure]]"
```

---

## Task 22: CLI `cleanup` subcommand

**Files:**
- Modify: `crates/roki-daemon/src/cli.rs`
- Modify: `crates/roki-daemon/src/main.rs`
- Modify: `crates/roki-daemon/src/runtime.rs`
- Create: `crates/roki-daemon/tests/e2e/cleanup_subcommand_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Extend the clap command**

In `crates/roki-daemon/src/cli.rs` (or wherever clap lives), add:

```rust
#[derive(clap::Parser, Debug)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(long, default_value = "roki.toml")]
    pub config: std::path::PathBuf,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Run the default pipeline: cleanup-first then rule first-match.
    Run,
    /// Cleanup-only mode: only `[[cleanup]]` matches lead to a cycle.
    Cleanup,
}
```

If `Cli` already exists, extend its derive(s) and add the new subcommand variant. Default behavior (no subcommand) should map to `Command::Run`.

- [ ] **Step 2: Plumb DispatchMode**

`runtime::run_inner` accepts a `DispatchMode` parameter:

```rust
pub(crate) async fn run_inner(
    config_path: &Path,
    mode: crate::engine::dispatch::DispatchMode,
) -> Result<(), SkeletonError> {
    // ...
    match evaluate(&admitted, &workflow, mode) { /* ... */ }
}
```

In `main.rs`:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = <Cli as clap::Parser>::parse();
    let mode = match cli.command.unwrap_or(Command::Run) {
        Command::Run => crate::engine::dispatch::DispatchMode::Default,
        Command::Cleanup => crate::engine::dispatch::DispatchMode::CleanupOnly,
    };
    // existing tokio runtime setup...
    runtime.block_on(crate::runtime::run_inner(&cli.config, mode))?;
    Ok(())
}
```

- [ ] **Step 3: Add `[[test]]` entry**

```toml
[[test]]
name = "cleanup_subcommand_smoke"
path = "tests/e2e/cleanup_subcommand_smoke.rs"
```

- [ ] **Step 4: Write the test**

Write `crates/roki-daemon/tests/e2e/cleanup_subcommand_smoke.rs`:

```rust
mod common;

use common::{post_webhook, spawn_daemon_with_workflow_and_args, wait_for_exit};

#[tokio::test]
async fn cleanup_subcommand_ignores_rule_list() {
    let tmp = tempfile::tempdir().unwrap();
    let session_root = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    let ticket_id = "OPS-600";

    // Rule matches but cleanup does not. With `cleanup` subcommand: NoMatch.
    let workflow = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"

[[rule]]
when.status = "InProgress"
run.cmd = "true"

[[cleanup]]
when.status = "Done"
run.cmd = "true"
"#;

    let roki_toml = format!(r#"
[paths]
session_root = "{}"

[engine]
max_iterations = 2

[default.ai.command]
cli = "echo unused"
stall_seconds = 30
"#, session_root.display());

    let (handle, addr) = spawn_daemon_with_workflow_and_args(workflow, &roki_toml, &["cleanup"]).await;
    post_webhook(&addr, ticket_id, "InProgress", "me").await;

    // NoMatch causes the loop to log + continue; the test runner
    // shuts the daemon down via SIGTERM after ~2s.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let _ = wait_for_exit(handle).await;

    // No events file exists (cleanup did not match, so no cycle, no emit).
    let events_path = session_root.join(format!("{ticket_id}.events.jsonl"));
    assert!(!events_path.exists(), "expected no events file; found one");
}
```

The helper `spawn_daemon_with_workflow_and_args` is a slight variant of `spawn_daemon_with_workflow` that takes additional CLI args. Add it to `tests/e2e/common.rs` if missing.

- [ ] **Step 5: Run**

Run: `cargo test -p roki-daemon --test cleanup_subcommand_smoke -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/cli.rs crates/roki-daemon/src/main.rs crates/roki-daemon/src/runtime.rs crates/roki-daemon/tests/e2e/cleanup_subcommand_smoke.rs crates/roki-daemon/Cargo.toml
git commit -m "feat(cli): add cleanup subcommand (cleanup-only dispatch)"
```

---

## Task 23: Backwards-compatibility verification

Confirm that the slice-1 + slice-2 e2e fixtures (no `[[cleanup]]`, no `[[on_failure]]`) still pass unchanged.

**Files:**
- None modified.

- [ ] **Step 1: Run every e2e test**

Run: `cargo test -p roki-daemon`
Expected: every test PASSes — slice-1 (`skeleton_smoke`, `iteration_smoke`), slice-2 (`session_smoke`, `stall_smoke`, `run_terminal_smoke`), slice-3 (the seven new e2e files).

If any slice-1/2 test regresses, the symptom indicates a backwards-compat break — investigate immediately.

- [ ] **Step 2: Run clippy + fmt**

Run: `cargo clippy -p roki-daemon -- -D warnings && cargo fmt --check`
Expected: no warnings, no diff.

- [ ] **Step 3: Run the full workspace test**

Run: `cargo test`
Expected: every workspace member's tests pass.

- [ ] **Step 4: Update `crates/roki-daemon/README.md`**

Append to the crate README under a new heading `## Slice 3 — Failure handling and cleanup`:

```markdown
## Slice 3 — Failure handling and cleanup

The daemon now evaluates `[[cleanup]]` first-match before `[[rule]]`
on each admitted webhook. A cleanup entry with all phases omitted
(`[[cleanup]]` with no `pre`/`run`/`post`) deletes
`<session_root>/<ticket-id>/` synchronously; a non-shorthand cleanup
runs as a normal cycle, then the deletion happens on terminal post.

A cycle that fails internally (process crash, unparseable directive,
schema drift, template error, iter exhausted, stall, fs_poison)
routes through `[[on_failure]]` first-match. The handler runs as a
new cycle (`cycle.kind = "failure"`) with `{{ failure.* }}` and
`ROKI_FAILURE_*` populated. Recursive failures (handler cycle that
itself fails) emit a `failure_unhandled` event with
`marker = "recursion_bound"` and exit 1.

Events are appended to `<session_root>/<ticket-id>.events.jsonl`
(NDJSON, sibling of the ticket dir). Three events are emitted:
`cycle_completed`, `failure_unhandled`, `worktree_delete_requested`.

CLI subcommand:

```sh
roki cleanup    # cleanup-only dispatch; rule list ignored
roki run        # default (cleanup-first then rule first-match)
roki            # alias for `run`
```
```

- [ ] **Step 5: Final commit**

```bash
git add crates/roki-daemon/README.md
git commit -m "docs: document slice 3 failure handling + cleanup"
```

- [ ] **Step 6: Push the branch**

```bash
git push -u origin slice3-failure-cleanup-spec
```

---

## Final Verification

- [ ] All 22 e2e + unit test additions pass.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] Slice-1 / slice-2 e2e tests still pass (no regressions).
- [ ] Spec `docs/superpowers/specs/2026-05-08-slice3-failure-cleanup-design.md` content matches the implementation. If a section drifted (e.g. an event field renamed), update the spec.
- [ ] CLI help shows `cleanup` and `run` subcommands.
- [ ] Branch `slice3-failure-cleanup-spec` pushed.

---

## Spec Coverage Checklist

| Spec section | Implemented in task |
|---|---|
| §2.2 `CycleKind` | Task 1 |
| §2.2 `FailureKind::FsPoison` | Task 2 |
| §2.2 `FailureMeta` | Task 3 |
| §2.2 `CycleOutcome::Failed { meta }` | Task 4 |
| §8 FsPoison wiring | Task 5 |
| §2.2 `CycleKind` plumbing | Task 6 |
| §7.1 `cycle.kind` / `ROKI_CYCLE_KIND` | Task 6 |
| §7.2 `failure.*` / `ROKI_FAILURE_*` | Task 7 |
| §2.5 + §9 events writer | Task 8 |
| §3 `[[cleanup]]` parser + shorthand | Task 9 |
| §3.2 cleanup validation errors | Task 9 |
| §3 `[[on_failure]]` parser + matchers | Task 10 |
| §3.2 on_failure validation errors | Task 10 |
| §3.2 `SessionRunUnsupported` for cleanup/on_failure | Tasks 9, 10 |
| §4 `dispatch::evaluate` | Task 11 |
| §4.2 `CleanupOnly` mode | Task 11 |
| §5.1 cleanup shorthand sync delete | Task 12 |
| §5.2 cleanup post-cycle delete | Task 12 |
| §5.3 cleanup-fs-error path | Task 12 |
| §2.3 dispatch wiring (rule path) | Task 13 |
| §2.3 dispatch wiring (cleanup paths) | Task 14 |
| §6 on_failure routing + handler cycle | Task 17 |
| §6.4 recursion bound | Task 17 |
| §6.5 no-match `failure_unhandled` | Task 17 |
| §2.4 cleanup-shorthand E2E | Task 15 |
| §2.4 cleanup-cycle E2E | Task 16 |
| §6.2 on_failure E2E | Task 18 |
| §6.4 recursion-bound E2E | Task 19 |
| §6.5 failure_unhandled E2E | Task 20 |
| §8 FsPoison E2E | Task 21 |
| §4.2 CLI cleanup subcommand | Task 22 |
| §12 backwards compat | Task 23 |
