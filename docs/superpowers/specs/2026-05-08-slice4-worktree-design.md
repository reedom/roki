# Slice 4 — Worktree Lifecycle Design

Date: 2026-05-08
Scope: Layer lazy `wt`-driven worktree creation on first `pre.directive: "run"`, per-spawn cwd selection (worktree if present, else ghq base) with session-shape cwd fixed at cycle start, real `wt remove` from `[[cleanup]]` cycles, and `FailureKind::FsPoison` extension to worktree create/recover errors on top of slice 3's failure-handler / cleanup engine. After this slice the binary materializes the per-ticket worktree only when a cycle actually reaches its run phase, reuses it across cycles, deletes it as part of cleanup, and routes worktree-side fs errors through `[[on_failure]]`.

## 1. Position in the Roadmap

Slice 4 lifts the worktree surface deferred by slice 3 §14:

- `roki-engine-worktree-create` — `fr:05 §Worktree` lazy creation on first `pre.directive: "run"` via `wt switch-create`; reused across cycles; recreated if removed out-of-band.
- `roki-engine-worktree-cwd` — `fr:05 §Worktree §Working directory` cwd selection (worktree if present, else ghq base); session-shape cwd fixed at cycle start.
- `roki-engine-worktree-cleanup` — `fr:05 §Cleanup` `wt remove` invoked from `engine::cleanup` before session-tempdir removal.
- `roki-engine-fs-poison-worktree` — `fr:05 §Failure mode retention` worktree create/recover fs errors route through `[[on_failure]] when.kind = "fs_poison"`.

Slice 3 (`docs/superpowers/specs/2026-05-08-slice3-failure-cleanup-design.md`) provides: `worktree_delete_requested` event, `FailureKind::FsPoison`, `[[on_failure]]` routing, `[[cleanup]]` cycles, structured event writer.

Out-of-scope, deferred to later slices:

- Admission-filter eviction cleanup (`fr:05 §Cleanup` item 2). Needs persistent admission state from daemon slice.
- Orphan reconcile at cold start (`fr:05 §Cleanup` item 3). Needs daemon (`fr:12`).
- `roki repo` CLI (`fr:09`). Log-access slice.
- Cleanup-time fs errors → escalation queue (`fr:05 line 54`). Slice 3's `failure_unhandled marker=cleanup_fs_error` surface remains; the queue lands with the `fr:06` escalation slice.
- Branch deletion, container/VM isolation, multi-host (`fr:05 §Boundaries`). Operator concerns / explicitly out of scope.

## 2. Architecture

### 2.1 Module layout

```
crates/roki-daemon/src/
├── runtime.rs                    // unchanged shape; passes ghq through to cleanup
└── engine/
    ├── mod.rs                    // + re-exports
    ├── worktree.rs               // NEW: ensure / exists / remove via `wt`
    ├── cwd.rs                    // NEW: resolve(ghq, ticket_id) -> PathBuf
    ├── cycle.rs                  // worktree::ensure on first pre→run; cwd resolved at cycle start
    ├── phase.rs                  // execute_at uses cwd::resolve, not raw resolve_ghq_base
    ├── cleanup.rs                // calls worktree::remove before session_tempdir delete
    └── outcome.rs                // FailureKind::FsPoison reused for worktree errors
```

The `engine::worktree` module is the only caller of the `wt` external binary. `engine::cwd::resolve` is the only cwd decision site; both `engine::cycle` (session-shape spawn at cycle start) and `engine::phase::execute_at` (per-spawn for command-shape) route through it.

### 2.2 Types

```rust
// engine::worktree

pub enum WorktreeError {
    WtNotFound,                                                    // `wt` binary missing on PATH
    SwitchCreateFailed { stderr: String, exit_code: Option<i32> },
    ListFailed         { stderr: String, exit_code: Option<i32> },
    RemoveFailed       { stderr: String, exit_code: Option<i32> },
    PathEscape         { resolved: PathBuf, root: PathBuf },
    Conflict           { ticket_id: String, existing_path: PathBuf },
    Io(std::io::Error),
}

pub async fn ensure(ghq: &str, ticket_id: &str) -> Result<PathBuf, WorktreeError>;
pub async fn exists(ghq: &str, ticket_id: &str) -> Result<Option<PathBuf>, WorktreeError>;
pub async fn remove(ghq: &str, ticket_id: &str) -> Result<bool, WorktreeError>;
//   Ok(true)  → worktree removed
//   Ok(false) → worktree was already absent (idempotent)
```

```rust
// engine::cwd

pub async fn resolve(ghq: &str, ticket_id: &str) -> Result<PathBuf, PhaseInfraError>;
//   worktree::exists Some -> that path
//   worktree::exists None  -> phase::resolve_ghq_base
//   Any WorktreeError      -> wrap as PhaseInfraError so the cycle driver can convert
//                             to FailureMeta{kind: FsPoison, ...} at the call site.
```

`FailureKind` and `FailureMeta` from slice 3 are unchanged. `WorktreeError` is converted to `FailureMeta { kind: FsPoison, phase, iter, exit_code, error_text }` at the cycle / cleanup boundary.

### 2.3 Test seam

Mirror the existing `ROKI_GHQ_BASE_OVERRIDE` pattern (`phase.rs:357`). Add:

- `ROKI_WT_BIN_OVERRIDE` — alternate `wt` binary path; tests point at a fixture script that simulates `wt switch-create / wt list / wt remove`.
- `ROKI_WT_ROOT_OVERRIDE` — base dir for fixture worktrees; `engine::worktree` resolves `<root>/<ticket-id>/` directly without invoking `wt` when set.

When the override env is present, the shell-out is bypassed and `engine::worktree` performs the equivalent local fs operation: `mkdir <root>/<ticket-id>` (idempotent for `ensure`), `Path::exists` (for `exists`), `remove_dir_all` (for `remove`). This matches the slice 1/2/3 testing pattern (in-process integration via binary-as-subprocess + env-driven seams).

## 3. Lazy creation flow

`engine::cycle::run_cycle` is updated:

1. **At cycle start**, before the iter loop, resolve `cwd_at_cycle_start = cwd::resolve(ghq, ticket_id).await?`. This replaces the direct `phase::resolve_ghq_base` call at `cycle.rs:101`. Used as the cwd for the session-shape supervisor spawn (per `fr:05 line 34` / `fr:04 line 46`: session-shape cwd is fixed at cycle start; the supervisor process is reused across all pre/post turns of the cycle).
2. **Pre phase** (per iter):
   - Session-shape: writes its turn to the supervisor's stdin. cwd is the cycle-start cwd, regardless of any worktree creation later in the same cycle (`fr:04 line 46`).
   - Command-shape: `engine::phase::execute_at` resolves cwd per invocation via `cwd::resolve`. In cycle 1 first iter the worktree does not exist, so cwd = ghq base. In cycle 1 iter ≥ 2 (after iter 1 created the worktree) and in any cycle N ≥ 2 for the same ticket, cwd = worktree.
3. **On `PreDirective::Run`**, before the run-phase spawn, call `worktree::ensure(ghq, ticket_id).await`:
   - `Ok(path)` → run-phase command spawn uses `path`.
   - `Err(WorktreeError)` → `break 'cycle Ok(CycleOutcome::Failed { meta: FailureMeta { kind: FsPoison, phase: Run, iter, exit_code, error_text } })`. Routes through `[[on_failure]]` per slice 3.
4. **Run phase** (always command-shape per slice 2) spawns at the path returned by `ensure`. cwd resolution still goes through `cwd::resolve` for symmetry — the result is the worktree path because `ensure` just confirmed it.
5. **Post phase**:
   - Session-shape: uses `cwd_at_cycle_start` (supervisor was already spawned with that cwd).
   - Command-shape: per-spawn cwd via `cwd::resolve`. After a successful run in the same iter the worktree exists, so cwd = worktree.
6. **Cycle N+1 for the same ticket**: `cwd::resolve` at cycle start finds the worktree left by cycle N → session-supervisor cwd = worktree, all command-shape phases also resolve to worktree from the first invocation. `worktree::ensure` on the next pre→run is a fast-path: `wt list` confirms presence, returns the path, no second `wt switch-create`.
7. **Out-of-band removal between iters**: every iter that reaches pre→run calls `worktree::ensure`; if the worktree disappeared since the last iter, `wt list` reports absent and `wt switch-create` is re-invoked. fr:05 line 33 contract.

Note: pre/post phases that run at the ghq base path (no worktree yet) must treat cwd as read-only (`fr:04 line 48`); only the run-phase / post-worktree-creation phases write into the worktree. This is an operator-side contract — the daemon does not enforce read-only via the filesystem; it merely picks the cwd per the rule above.

## 4. Cwd resolution rule

`engine::cwd::resolve(ghq, ticket_id)`:

```
if let Some(p) = worktree::exists(ghq, ticket_id).await? {
    return Ok(p);
}
phase::resolve_ghq_base(ghq).await
```

Used by:

- `engine::cycle` session-supervisor spawn (replaces `resolve_ghq_base` at `cycle.rs:101`).
- `engine::phase::execute_at` per phase invocation (replaces `resolve_ghq_base` at `phase.rs:75`). The executor signature is unchanged; only the internal resolution call differs.

Cwd is decided **per spawn** for command-shape and **once per cycle** for session-shape. Slice 4 does not introduce a cycle-wide cached cwd for command-shape — the cost of `wt list` per phase is acceptable, and the rule "always reflect current worktree state" is simpler to reason about than a TTL.

## 5. Path safety

`worktree::ensure` and `worktree::exists` apply the following checks on every returned path:

- **Canonicalize** via `std::fs::canonicalize` (resolves symlinks).
- **Confine to the ghq tree**: the canonicalized path must be under the canonicalized parent of `ghq list -p <ghq>`. Otherwise → `WorktreeError::PathEscape`.
- **Conflict detection**: if `wt list` reports a different ticket-id branch already pointing at the resolved path (i.e. same path but different branch name), return `WorktreeError::Conflict`. Branch-name collisions across tickets cannot happen because branch name = ticket id verbatim (`fr:05 line 36`); the conflict check defends against operator-side branch reuse.

Both `PathEscape` and `Conflict` surface as `FailureKind::FsPoison` at the cycle / cleanup boundary.

Ticket-id sanitization for the branch arg: ticket id is passed verbatim (`fr:05 line 36`). `wt switch-create` rejects ticket ids that are invalid git refs; that rejection arrives as `WorktreeError::SwitchCreateFailed` and routes through `[[on_failure]]`.

## 6. Cleanup integration

`engine::cleanup::post_cycle_delete` and `delete_immediate` updated:

```
emit worktree_delete_requested  (audit event, retained from slice 3)
worktree::remove(ghq, ticket_id).await
remove_dir_all <session_root>/<ticket_id>/
```

Order: worktree first, session-tempdir second (`fr:05 §Cleanup line 44`).

`worktree::remove` returns `Ok(false)` when the worktree was already absent (idempotent, mirrors slice 3's `remove_dir_all NotFound` handling). `wt remove` failure on a present worktree → `failure_unhandled marker=cleanup_fs_error` (same surface as slice 3 cleanup-time session-tempdir fs errors) and propagate `Err` so `runtime::run_inner` exits 1.

`delete_immediate` (shorthand path) currently takes only `ticket_id`. Slice 4 extends the signature with `ghq: &str`. The shorthand caller in `runtime::run_inner` already has the admitted ticket's ghq; the wiring is one parameter forward.

The `worktree_delete_requested` event remains an audit line. Slice 3 wrote it as a forward-compat marker for "the future worktree slice"; slice 4 *is* that slice and the event is retained because operators / log readers benefit from a single timestamped audit line per cleanup boundary, regardless of whether a worktree existed.

**Single-ghq scope vs. fr:05 line 44 allowlist enumeration.** fr:05 line 44 says cleanup "enumerates worktrees in the allowlist whose branch name matches the issue identifier and runs `wt remove`". Slice 4 only invokes cleanup as the post-cycle step of a cleanup cycle (or via the cleanup-shorthand dispatch); both paths originate from an admitted ticket whose ghq is known. A single-ghq `worktree::remove(ghq, ticket_id)` call is correct for slice 4. The allowlist-walk is required for the deferred admission-eviction and orphan-reconcile paths (`fr:05 §Cleanup` items 2 and 3) where the daemon may need to clean up a ticket without an active admission record; slice 4 does not implement those paths and §14 names the gap.

## 7. FsPoison extension

Slice 3 routes `FailureKind::FsPoison` for session_tempdir create errors. Slice 4 adds:

- `worktree::ensure` failure (any `WorktreeError` variant) at the run-phase boundary → `FailureMeta { kind: FsPoison, phase: Run, iter, exit_code: WorktreeError::SwitchCreateFailed.exit_code, error_text: stderr }`.
- `cwd::resolve` failure (e.g. `wt list` errors during session-supervisor spawn) at any phase boundary → same shape with `phase: <next-phase>`.

Routes through `[[on_failure]]` first-match (slice 3 surface unchanged). `error_text` is populated from `wt`'s stderr when available, otherwise from the `WorktreeError` Display. Truncation reuses slice 3's tail-truncate (4096 bytes).

Cleanup-time `wt remove` failures do **not** route through `[[on_failure]]` — they emit `failure_unhandled marker=cleanup_fs_error` exactly as slice 3 surfaces session_tempdir cleanup failures. fr:05 line 54 names this surface (modulo the deferred escalation queue).

## 8. Config additions

None.

`wt` and `ghq` discovery via PATH; ticket id resolved by admission. `roki.toml` and `WORKFLOW.toml` schemas unchanged. No new keys, no new defaults.

## 9. Events

No new event types. `worktree_delete_requested` retained verbatim from slice 3 (`reason: cleanup_terminal | cleanup_shorthand`). `failure_unhandled` reused for both worktree create errors (via `[[on_failure]]` no-match) and cleanup-time `wt remove` errors (`marker: cleanup_fs_error`).

## 10. Implementation Order

Tasks are listed so each one compiles and the test suite is green before the next starts.

1. **`engine::worktree` skeleton + override seam.** Module compiles; `ensure / exists / remove` shell out to `wt` with `ROKI_WT_BIN_OVERRIDE` + `ROKI_WT_ROOT_OVERRIDE` honored. Unit tests via fixture script + override.
2. **`engine::cwd::resolve`.** Wraps `worktree::exists` + `phase::resolve_ghq_base`. Unit tests cover both branches.
3. **Session supervisor cwd switch.** `engine::cycle` cycle-start cwd uses `cwd::resolve` instead of `resolve_ghq_base`. Slice 1/2/3 tests still pass (no worktree → ghq base).
4. **Phase executor cwd switch.** `engine::phase::execute_at` resolves cwd per call via `cwd::resolve`. Tests still pass.
5. **Lazy ensure on pre→run.** `engine::cycle` calls `worktree::ensure` after `PreDirective::Run` and before run-phase spawn. FsPoison routing on `WorktreeError`. Test: rule with pre→run + worktree creation observed.
6. **`worktree::remove` in cleanup module.** `delete_immediate` and `post_cycle_delete` updated; `delete_immediate` signature gains `ghq`. Cleanup-time `wt remove` failure → `failure_unhandled marker=cleanup_fs_error`. Tests: cleanup cycle deletes worktree + session_tempdir; cleanup with absent worktree no-ops.
7. **Path safety enforcement.** Canonicalize + escape check + conflict detect in `worktree::ensure / exists`. Unit tests: symlink escape, sibling-ticket conflict.
8. **Worktree reuse + recreate integration test.** End-to-end: cycle 1 creates, cycle 2 reuses (verify no second `wt switch-create` invocation), operator removes mid-test, cycle 3 recreates.
9. **FsPoison handler integration test.** Force `wt switch-create` failure (override script exits non-zero); assert `[[on_failure]] when.kind = "fs_poison"` matches; handler cycle gets `{{ failure.* }}`.
10. **Slice-3 backwards compat sweep.** Confirm slice 1/2/3 fixtures load and run unchanged. `worktree_delete_requested` event line still emitted from cleanup paths.

## 11. Testing Strategy

Slice 1/2/3 carry the in-process integration test scaffolding (binary-as-subprocess, fixture WORKFLOW.toml, fake Linear webhook, override env). Slice 4 adds:

**Unit tests (in-crate):**

- `engine::worktree::ensure / exists / remove` against a fixture `wt` script, exercised via override env.
- `engine::cwd::resolve` both branches (worktree present / absent).
- Path safety: symlink-escape, sibling-ticket conflict, both surfacing as `WorktreeError::PathEscape` / `Conflict`.

**Integration tests (binary-as-subprocess):**

- **Lazy creation**: rule fixture with pre→run; assert worktree dir exists at run time, deleted after cleanup cycle.
- **Cycle reuse**: two webhooks for same ticket; second cycle's session-supervisor cwd = worktree (verify via a `pwd`-emitting fixture cli line). Assert no second `wt switch-create` invocation in the override-script log.
- **Out-of-band removal**: between iters, the fixture removes the worktree; assert recreate on next pre→run.
- **Cleanup ordering**: cleanup cycle deletes worktree first, session_tempdir second (event-log order proves it).
- **FsPoison via worktree failure**: `wt switch-create` failure → `[[on_failure]] when.kind = "fs_poison"` matches; handler cycle dir created with `{{ failure.* }}` available.
- **Cleanup-time `wt remove` failure**: override script exits non-zero on remove → `failure_unhandled marker=cleanup_fs_error`, exit code 1.

## 12. Backwards Compatibility

A slice 1/2/3 `WORKFLOW.toml` with no rule that returns `directive: "run"` from pre never invokes `wt`; `cwd::resolve` falls through to ghq base; behavior unchanged. Rules that *do* reach run start materializing a worktree on first run — the cwd as observed by the run-phase subprocess shifts from the ghq base path to the worktree path. This is the contract change the slice ships.

`worktree_delete_requested` events keep emitting from `engine::cleanup`, now reflecting actual `wt remove` side effects. Slice 3 tests that asserted only the event line still pass; slice 4 adds tests that assert the actual side effect.

## 13. Dependency Additions

None. `wt` is an external binary like `ghq` (discovered via PATH). No new crates.

## 14. Open Questions Deferred to Slice 5+

- Admission-filter eviction cleanup (`fr:05 §Cleanup` item 2). Daemon admission state required.
- Orphan reconcile at cold start (`fr:05 §Cleanup` item 3). Daemon required (`fr:12`).
- `roki repo` CLI (`fr:09`). Log-access slice.
- Cleanup-time fs errors → escalation queue (`fr:05 line 54`). `failure_unhandled marker=cleanup_fs_error` remains until escalation slice.
- Branch deletion, container/VM isolation, multi-host. Operator concerns / out of scope per `fr:05 §Boundaries`.
- Hot reload of `WORKFLOW.toml` and `workflow/*.md`, persistent daemon, queue preemption.

These are intentionally out of scope; the slice surface is sized so a single binary run can materialize a worktree on first run, reuse it across cycles, delete it on cleanup, and route worktree-side fs errors through `[[on_failure]]`, with no daemon, no admission state, no escalation queue.
