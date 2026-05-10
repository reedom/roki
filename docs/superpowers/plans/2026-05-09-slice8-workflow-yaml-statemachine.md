# Slice 8 Workflow YAML + State Machine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `WORKFLOW.toml` (TOML, pre/run/post phase loop) with `WORKFLOW.yaml` (YAML, explicit state machine with linear-array sugar). Replace stdout-JSON directive parsing with a sentinel-file control channel. Remove the long-lived AI session shape — every state spawns a fresh subprocess. Migrate slice 7 `[[on_failure]]` semantics to `on_failure:` rules in the new schema. Daemon config (`roki.toml`) stays TOML.

**Architecture:** New `workflow::*` module owns YAML parser, sugar→canonical state-machine expansion, and 8-rule validator. New `engine::sentinel` provides per-state directive-file protocol via `$ROKI_DIRECTIVE_PATH`. New `engine::state_runtime` runs one state per call (spawn subprocess → wait → read sentinel → resolve edge). `engine::cycle` is rewritten around `StateMachine` consumption. Pre/run/post phase enum, session-shape phases, `iter_exhausted` failure kind, and stdout-JSON directive parsing are removed. Slice 7's escalation queue is reused unchanged: `recursion_bound` (replaces `iter_exhausted`'s loop semantics + recursive failure-cycle handling) feeds the queue.

**Tech Stack:** Rust 2024 (workspace edition), `serde_yaml_ng = "0.10"` (already in `crates/roki-daemon/Cargo.toml`; maintained successor to deprecated `serde_yaml`), `liquid` (existing), `tokio` (existing), slice 1-7 deps.

**Spec:** `docs/superpowers/specs/2026-05-09-slice8-workflow-yaml-statemachine-design.md`.

**Working branch:** `feature/workflow` (spec uncommitted at plan time; commit before starting Task 1). All implementation commits land on this branch.

---

## Session progress (2026-05-10)

**Completed:** Tasks 0-7, 9-10, 12-15 (13 of 18). All committed on `feature/workflow`. 375 binary unit tests pass; pre-existing slice 1-7 e2e suite still green (no engine wiring touched yet).

| Task | Commit | Status |
|---|---|---|
| 0 spec/plan | `65a4121`, `0c63f38`, `c6a9dec`, `5877e4a` | done |
| 1 canonical types | `5c0b4d4` | done |
| 2 parse | `6ca56e7` | done |
| 3 sugar 5-pass | `66dc5ee` | done |
| 4 validate | `2b4806f` | done |
| 5 sentinel | `cad2161` | done |
| 6 state_runtime | `09f0477` | done |
| 7 cycle_state | `4bb58a2` | done |
| 9+10 CLI validate+graph | `a63234c` | done |
| 12 ref:config | `6edcb2c` | done |
| 14 ref:cli + ref:log-events | `2ba94ec` | done |
| 15 YAML examples + delete TOML | `893c5ac` | done |
| 13 FR doc rewrite (fr:01/02/04→04-state/06/08) | `fb51aba` | done |

**Pending (engine cliff — needs dedicated session):**

| Task | Why deferred |
|---|---|
| 8 failure routing wiring | Demolish + replace `engine/cycle.rs` 44KB + `phase.rs` 34KB + `session.rs` 31KB + `directive.rs` 8KB; rewrite `engine/{outcome,on_failure,dispatch,context,cleanup,stall}.rs`, `daemon/{dispatcher,ticket_task,real_runner}.rs`, `events.rs`. Touch ~150KB. Each step breaks compile until full chain rewritten. |
| 11 roki.toml config rename | `[default.ai.command]` → `[default.ai]` rename forces engine refactor (current code references `[default.ai.session]` and `[default.ai.command]`). |
| 16 slice 1-7 e2e fixture migration | Each fixture's TOML emitter rewrites to new YAML harness; depends on Task 8 + 11 being live. |
| 17 12 new slice 8 e2e fixtures | Depends on engine using YAML + state machine. |
| 18 sweep | Final fmt + clippy + full e2e green. |

**Resumption note:** Start Task 8. Read `engine/{phase,session,directive,cycle}.rs` end-to-end first; identify shared helpers (`engine/template`, `engine/stall`, `engine/worktree`, `engine/cwd`, `engine/stream` are reusable; `engine/context` needs Liquid-globals rewrite). Build `RealStateRunner` against the existing helper stack, then swap `daemon::ticket_task::CycleRunner` to consume it. Delete legacy modules last, after dispatcher uses canonical `RuleEntry`. ref:config rows for `[default.ai.session]`/`[default.ai.command]` should be replaced with the merged `[default.ai]` row simultaneously with the code change (Task 11).

---

## File Structure

### Created

| Path | Responsibility |
|---|---|
| `crates/roki-daemon/src/workflow/mod.rs` | Module root. `WorkflowFile`, re-exports. |
| `crates/roki-daemon/src/workflow/canonical.rs` | `StateMachine`, `State`, `StateBody`, `Terminal`, `EdgeTarget`, `RuleEntry`, `WhenClause`, `Admission`, `RepoEntry` types. |
| `crates/roki-daemon/src/workflow/parse.rs` | `serde_yaml_ng` deserializer to a sugar-or-canonical IR; resolves admission + per-repo override files; path resolution. |
| `crates/roki-daemon/src/workflow/sugar.rs` | 5-pass expansion (terminals, tasks-array, directive defaults, max_visits SCC injection). Outputs canonical `WorkflowFile`. |
| `crates/roki-daemon/src/workflow/validate.rs` | 8 validation rules; multi-error accumulation. |
| `crates/roki-daemon/src/workflow/liquid.rs` | Liquid context construction for state machines (cycle, ticket, repo, state, tasks.<id>.*, failure.* namespaces). |
| `crates/roki-daemon/src/engine/sentinel.rs` | Atomic write contract; `read_sentinel(path) -> Result<Option<DirectivePayload>>`; failure mode classification (absent / unparseable / schema-drift). |
| `crates/roki-daemon/src/engine/state_runtime.rs` | Runs one state: render Liquid → spawn subprocess → wait or stall-timeout → read sentinel → resolve edge. Returns `StateOutcome { next_target, captures }`. |
| `crates/roki-daemon/src/engine/cycle_state.rs` | Replaces `engine::cycle::iter_loop`. `run_cycle(SM, ctx) -> CycleResult`. Per-state visits map, recursion-bound enforcement, terminal handling. |
| `crates/roki-daemon/src/cli/workflow_graph.rs` | `roki workflow graph` ASCII + DOT renderers. |
| `crates/roki-daemon/src/cli/workflow_validate.rs` | `roki workflow validate` subcommand: load + expand + validate; multi-error report. |
| `docs/examples/WORKFLOW.minimal.yaml` | Smallest bootable YAML. |
| `docs/examples/WORKFLOW.annotated.yaml` | Annotated reference for every key. |
| `docs/examples/repos/bar.yaml` | Per-repo override example. |
| `docs/fr/04-state-execution.md` | Renamed from `04-phase-execution.md`; updated for state model. |
| `crates/roki-daemon/tests/e2e/yaml_load_smoke.rs` | Slice 8: minimal YAML loads, daemon emits `daemon_ready`. |
| `crates/roki-daemon/tests/e2e/sugar_linear_smoke.rs` | Three-task `tasks:` chain → all run → `__success__`. |
| `crates/roki-daemon/tests/e2e/sugar_retry_smoke.rs` | `tasks: [a, b]` with `b.directives.retry: a`; verifies auto-injected `max_visits`. |
| `crates/roki-daemon/tests/e2e/canonical_branch_smoke.rs` | Explicit SM + sentinel directive `skip` → `outcome: no_action`. |
| `crates/roki-daemon/tests/e2e/sentinel_absent_smoke.rs` | Exit 0, no sentinel → `on_done` taken. |
| `crates/roki-daemon/tests/e2e/sentinel_unparseable_smoke.rs` | Invalid JSON → `unparseable` → routes to `on_failure when.kind: unparseable`. |
| `crates/roki-daemon/tests/e2e/state_on_fail_smoke.rs` | Exit 1 → state `on_fail` taken; no failure-cycle spawned. |
| `crates/roki-daemon/tests/e2e/recursion_bound_yaml_smoke.rs` | Self-loop with `max_visits: 2`; third visit → `recursion_bound` → escalation queue. |
| `crates/roki-daemon/tests/e2e/validate_orphan_target_smoke.rs` | Edge to undeclared state → daemon refuses startup; multi-error report. |
| `crates/roki-daemon/tests/e2e/cleanup_immediate_delete_yaml_smoke.rs` | Body-less cleanup → synchronous delete, no cycle. |
| `crates/roki-daemon/tests/e2e/per_repo_override_smoke.rs` | `[[admission.repos]] workflow:` → loads `repos/bar.yaml` rules. |
| `crates/roki-daemon/tests/e2e/workflow_graph_cli_smoke.rs` | `roki workflow graph` ASCII output snapshot. |
| `crates/roki-daemon/tests/e2e/workflow_validate_cli_smoke.rs` | `roki workflow validate` exits 0 on valid, non-zero with multi-error on invalid. |

### Modified

| Path | Change |
|---|---|
| `crates/roki-daemon/Cargo.toml` | Confirm `serde_yaml_ng` dep present (already in Cargo.toml). |
| `crates/roki-daemon/src/main.rs` | Add `mod workflow;` to the top-level module list. (Daemon is a binary; no `lib.rs`.) |
| `crates/roki-daemon/src/config/mod.rs` | Drop `pub mod workflow;` and `pub mod workflow_md;` (TOML parsers superseded by top-level `workflow/` module). |
| `crates/roki-daemon/src/config/roki.rs` | `[paths] workflow` default → `./WORKFLOW.yaml`. `[default.ai.session]` block removed. `[default.ai.command]` → `[default.ai]` (single `cli` + `stall_seconds`). |
| `crates/roki-daemon/src/runtime.rs` | `load_workflow_yaml(path)` replaces `load_workflow_toml`. Per-repo override file resolution per spec §3.2.1. |
| `crates/roki-daemon/src/engine/mod.rs` | Drop `pub mod phase;`, `pub mod session;`, `pub mod directive;`, `pub mod cycle;`. Add `pub mod sentinel;`, `pub mod state_runtime;`, `pub mod cycle_state;`. |
| `crates/roki-daemon/src/engine/outcome.rs` | `FailureKind` enum: drop `IterExhausted`; add `RecursionBound`; rename `phase: PhaseKind` → `state_id: String` on failure metadata. |
| `crates/roki-daemon/src/engine/on_failure.rs` | Match on `FailureKind` + `state_id` (was `phase`). Routing logic per spec §7.3. |
| `crates/roki-daemon/src/engine/dispatch.rs` | Consume canonical `RuleEntry` + `WhenClause`. First-match against new shape. |
| `crates/roki-daemon/src/engine/context.rs` | Liquid context construction: drop `pre.*`/`post.*`/`run.*` namespaces; add `state.*` and `tasks.<id>.*`. |
| `crates/roki-daemon/src/engine/template.rs` | Reused. Liquid renderer unchanged (grammar same). Confirm `tasks.<id>` lookups resolve correctly. |
| `crates/roki-daemon/src/engine/cleanup.rs` | Cleanup-cycle pathway uses state machine; immediate-delete shorthand unchanged. |
| `crates/roki-daemon/src/engine/stall.rs` | Reused. Stall-window adapter rebound from `phase` to `(state_id, visit_n)` keying. |
| `crates/roki-daemon/src/engine/worktree.rs`, `cwd.rs`, `stream.rs` | Reused as-is. |
| `crates/roki-daemon/src/events.rs` | Cycle events carry `state_id: String` + `visit_n: u32` (replaces `phase`). `cycle_completed` adds `terminal_id`. `failure_unhandled` payload field rename. |
| `crates/roki-daemon/src/daemon/dispatcher.rs` | Match on `WhenClause`-shaped condition; dispatch consumes `RuleEntry`. |
| `crates/roki-daemon/src/daemon/ticket_task.rs` | Drives `cycle_state::run_cycle` instead of phase loop. |
| `crates/roki-daemon/src/daemon/real_runner.rs` | `run_state` replaces `run_phase`. Recursion path emits `recursion_bound` failure. |
| `crates/roki-daemon/src/engine/cleanup.rs` | Cleanup-cycle pathway uses state machine; immediate-delete shorthand unchanged. |
| `crates/roki-daemon/src/cli/mod.rs` | Register `workflow graph` + `workflow validate` subcommands. Rename `roki log --phase` → `--state`. |
| `crates/roki-daemon/src/cli/log.rs` | `--state` flag accepts state id; `--phase` removed. |
| `docs/fr/01-engine-model.md` | Phase loop, directive schema, iteration cap, failure handling, cleanup, cold-start sections rewritten per spec §11.1. |
| `docs/fr/02-configuration.md` | `WORKFLOW.toml` schema section replaced with YAML schema per spec §3. |
| `docs/fr/06-failure-handling.md` | Failure-kind table updated; `iter_exhausted` removed; `recursion_bound` added; `on_failure:` matcher discussion. |
| `docs/fr/08-observability-logs.md` | Cycle-engine event payloads: `phase` → `state_id` + `visit_n`. `cycle_completed` gains `terminal_id`. |
| `docs/reference/config.md` | `WORKFLOW.toml` schema replaced with canonical `WORKFLOW.yaml` schema. `[default.ai]` rename + `[default.ai.session]` removal. |
| `docs/reference/frontmatter.md` | Drop `session:` row from `workflow/*.md` table. Update prose: every state command-shape. |
| `docs/reference/cli.md` | Add `roki workflow graph` + `roki workflow validate` rows; rename `roki log --phase` → `--state`. |
| `docs/reference/log-events.md` | `phase` → `state_id` + `visit_n` on cycle-engine event rows. Add `recursion_bound` failure kind. Drop `iter_exhausted`. |
| `docs/examples/roki.minimal.toml` | `[paths] workflow = "./WORKFLOW.yaml"`; remove `[default.ai.session]`; merge `[default.ai.command]` into `[default.ai]`. |
| `docs/examples/roki.annotated.toml` | Same as above with annotations. |
| `docs/examples/workflow-impl.md` | Drop `session:` frontmatter key if present. Prose: pre/run/post → states. |
| `docs/examples/workflow-judge.md` | Same. |
| `docs/examples/workflow-verdict.md` | Same. |
| `crates/roki-daemon/tests/e2e/*.rs` (slices 1-7) | Test-harness YAML emitter replaces TOML emitter. `phase` assertions → `state_id`. `iter_exhausted` → `recursion_bound`. |
| `crates/roki-daemon/tests/e2e/support_workflow_toml.rs` (if present) → `support_workflow_yaml.rs` | Test-harness module rename. Pattern matches existing `support_cold_start.rs`. |
| `crates/roki-daemon/Cargo.toml` | 13 new `[[test]]` entries for slice 8 e2e fixtures. |

### Deleted

| Path | Reason |
|---|---|
| `docs/examples/WORKFLOW.minimal.toml` | Replaced by `.yaml` variant. |
| `docs/examples/WORKFLOW.annotated.toml` | Replaced by `.yaml` variant. |
| `crates/roki-daemon/src/config/workflow.rs` (~50KB) | TOML workflow parser; superseded by `src/workflow/parse.rs` + `sugar.rs` + `validate.rs`. Audit before delete: any non-parser logic (e.g. shared types) migrated to the new module first. |
| `crates/roki-daemon/src/config/workflow_md.rs` (~6.7KB) | `workflow/*.md` TOML-frontmatter parser; superseded by YAML-frontmatter equivalent reused via the new `workflow/` module. |
| `crates/roki-daemon/src/engine/cycle.rs` (~44KB) | Superseded by `cycle_state.rs`. Audit for shared helpers before delete. |
| `crates/roki-daemon/src/engine/phase.rs` (~34KB) | Pre/run/post phase loop; deleted entirely. |
| `crates/roki-daemon/src/engine/session.rs` (~31KB) | Long-lived AI session subprocess; deleted entirely (no session shape). |
| `crates/roki-daemon/src/engine/directive.rs` (~8KB) | Stdout-JSON directive parser; superseded by `engine/sentinel.rs`. |

---

## Task 0: Confirm spec + branch + commit spec

**Goal:** Spec is committed on the working branch before any code lands.

**Steps:**

- [ ] Verify current branch: `git rev-parse --abbrev-ref HEAD` returns `feature/workflow`.
- [ ] Verify spec exists and is the rebased version: `ls docs/superpowers/specs/2026-05-09-slice8-workflow-yaml-statemachine-design.md`.
- [ ] Stage + commit the spec: `git add docs/superpowers/specs/2026-05-09-slice8-workflow-yaml-statemachine-design.md && git commit -m "docs(slice8): workflow yaml + state machine design spec"`.
- [ ] Verify `git log --oneline -1` shows the spec commit.

**Acceptance:** Spec file tracked. `git status` clean. Working tree at known commit on `feature/workflow`.

---

## Task 1: `workflow::canonical` types

**Goal:** Pure-data types for the canonical (post-sugar-expansion) workflow file.

**Spec ref:** §2.2.

**Files:** `crates/roki-daemon/src/workflow/canonical.rs` (new), `crates/roki-daemon/src/workflow/mod.rs` (new), `crates/roki-daemon/src/main.rs` (modified — add `mod workflow;`).

**Steps:**

- [ ] Add `mod workflow;` to `crates/roki-daemon/src/main.rs` top-level module list (sorted alphabetically).
- [ ] Create `workflow/mod.rs` with `pub mod canonical; pub mod parse; pub mod sugar; pub mod validate; pub mod liquid;` declarations (other modules empty placeholders for later tasks).
- [ ] In `canonical.rs`, define types per spec §2.2: `WorkflowFile`, `Admission`, `AssigneeMatcher`, `RepoEntry`, `RuleEntry`, `WhenClause`, `StateMachine`, `State`, `StateBody`, `Terminal`, `EdgeTarget`, `StateId`, `DirectiveName`.
- [ ] Use `BTreeMap` for `states` and `terminals` (deterministic iteration, deterministic SCC entry-pick).
- [ ] Implement `Debug`, `Clone` on every public type.
- [ ] Add `#[cfg(test)]` builder helpers (`state_machine_builder()`, `state()`, `terminal()`) for downstream tests.

**Acceptance:** `cargo build -p roki-daemon` succeeds. `cargo test -p roki-daemon workflow::canonical::` builds (no test bodies yet).

---

## Task 2: `workflow::parse` (serde_yaml_ng round-trip)

**Goal:** Deserialize `WORKFLOW.yaml` into a sugar-or-canonical IR. Resolve per-repo override files. Apply path resolution rules.

**Spec ref:** §3.1, §3.2, §3.2.1, §3.3 (sugar form acceptance).

**Files:** `Cargo.toml` (serde_yaml_ng already present), `workflow/parse.rs` (new).

**Steps:**

- [ ] Confirm `serde_yaml_ng = "0.10"` is in `crates/roki-daemon/Cargo.toml` `[dependencies]` (already present). `serde = { version = "1", features = ["derive"] }` already present.
- [ ] Define IR enums in `parse.rs`:
  ```rust
  #[derive(Deserialize)]
  pub enum SugarOrCanonical {
      Sugar { tasks: Vec<TaskEntry>, on_fail: Option<StateId>, states: Option<BTreeMap<StateId, StateRaw>>, terminals: Option<BTreeMap<StateId, Terminal>> },
      Canonical { start: StateId, states: BTreeMap<StateId, StateRaw>, terminals: BTreeMap<StateId, Terminal>, on_fail: Option<StateId> },
      Empty {}, // immediate-delete shorthand
  }
  ```
- [ ] Use serde untagged enum + custom field-set discrimination (`tasks:` present → Sugar; `start:` present → Canonical; both absent → Empty).
- [ ] Implement `parse_workflow_file(path: &Path) -> Result<RawWorkflow, ParseError>` that reads + deserializes the top-level file.
- [ ] Implement `resolve_per_repo_overrides(top: &RawWorkflow, top_path: &Path) -> Result<RawWorkflow, ParseError>` that walks `admission.repos[].workflow` and substitutes the override file's `rules` / `cleanup` / `on_failure` lists for that repo's effective dispatch (admission stays in top file).
- [ ] Implement path resolution helper `resolve_path(reference_file: &Path, declared: &Path) -> PathBuf` per spec §3.2.1: declared is taken relative to `reference_file.parent()`. Tilde-expand. Reject absolute symlinks that escape.
- [ ] Reject the `Empty` variant outside `cleanup:` lists (schema error).
- [ ] Round-trip test: load each sugar+canonical example fixture from `tests/fixtures/workflow/` (create three: minimal sugar, full canonical, immediate-delete cleanup). Assert no panic + structurally-correct IR.

**Acceptance:** `cargo test -p roki-daemon workflow::parse::tests` green. `serde_yaml_ng::from_str` accepts both sugar and canonical forms with realistic fixtures.

---

## Task 3: Sugar → canonical expansion (5 passes)

**Goal:** `sugar::expand(raw: RawWorkflow) -> Result<WorkflowFile, ExpandError>` produces canonical types from spec §4.1-§4.5.

**Spec ref:** §4.1 - §4.5.

**Files:** `workflow/sugar.rs` (new).

**Steps:**

- [ ] **Pass 1 (implicit terminals):** for each rule, scan all `EdgeTarget` references; if any name in `["__success__", "__failure__", "__no_action__", "__cancelled__"]` is referenced and not declared in `terminals:`, inject default `Terminal { outcome: "<name minus underscores>" }`.
- [ ] **Pass 2 (`tasks:` array):** for each `Sugar` rule, walk `tasks` in order:
  - First task id → `start:`.
  - `task[i].on_done` defaults to `task[i+1].id` for `i < N-1`, else `__success__`.
  - `task[i].on_fail` defaults to rule-level `on_fail`, else `__failure__`.
  - `task[i].directives` defaults to empty map.
  - String entry `"foo"` → reference to `states.foo` (already declared); error if missing.
  - Map entry → register as state under given id.
  - Apply task-level `if`, `timeout`, `max_visits` to the state.
- [ ] **Pass 3 (directive name defaults):** for each state, for each built-in directive in `["end", "skip", "retry", "fail", "cancel"]`: if not explicitly listed in `state.directives`, the engine resolves at runtime via the default-target table (spec §4.3). Implement as a runtime lookup function `resolve_directive(name: &str, state: &State) -> Option<EdgeTarget>` consulted by state runtime; do NOT statically inject. Validation refers to the same table.
- [ ] **Pass 4 (validation):** call `validate::run(file)` (Task 4); short-circuit return on error.
- [ ] **Pass 5 (auto-`max_visits`):** Tarjan SCC over the state graph (states only; terminals are sinks). For each non-trivial SCC (≥2 nodes, OR 1 node with self-edge):
  - If any member declares `max_visits` (explicit, not the default `1`), leave alone.
  - Else inject `max_visits = config.max_iterations` on the lexicographically smallest state id in the SCC.
- [ ] Trivial single-node SCCs without self-edge get `max_visits = 1` (no auto-injection needed; default is already 1).
- [ ] Snapshot tests: minimal sugar, retry sugar, branch canonical → expanded canonical YAML. Use `insta` or hand-written assertions on `Debug` output.

**Acceptance:** `cargo test -p roki-daemon workflow::sugar::tests` green. Snapshot tests cover the three worked examples in spec §3 and §4.

---

## Task 4: `workflow::validate` (8 rules + multi-error)

**Goal:** Validation pass over canonical `WorkflowFile`. Accumulate all errors; return them all at once.

**Spec ref:** §4.4.

**Files:** `workflow/validate.rs` (new).

**Steps:**

- [ ] Define `ValidationError` enum with one variant per rule (UnknownEdgeTarget, DuplicateStateId, BothRunAndUses, OrphanBody, ReservedPrefixState, UnboundedCycle, EmptyTerminalOutcome, InvalidStartReference) plus a `StateIdNotEnvSafe(String)` for the spec §6 env-name rejection.
- [ ] Implement `validate(file: &WorkflowFile) -> Result<(), Vec<ValidationError>>`:
  - Walk every `EdgeTarget` (in `start`, `state.on_done`, `state.on_fail`, `state.directives.values()`); collect `UnknownEdgeTarget` for any id not in `states` ∪ `terminals`.
  - Detect duplicate ids by accumulating during sugar Pass 2 OR by comparing cardinalities post-expansion.
  - For each state, assert exactly one of `run` / `uses` is `Some` (excluding terminals).
  - Reject states with reserved `__*` prefix.
  - Tarjan SCC: for each non-trivial SCC with no `max_visits` declared anywhere on the cycle → `UnboundedCycle`.
  - For each terminal: `outcome.is_empty()` → `EmptyTerminalOutcome`.
  - `start` must be in `states` and not in `terminals` → `InvalidStartReference` otherwise.
  - State id must match `[A-Za-z][A-Za-z0-9_]*` and uppercase form must be valid env-var fragment → `StateIdNotEnvSafe`.
- [ ] Multi-error: do NOT short-circuit. Accumulate all errors; sort by deterministic key (kind name + offending id); return in `Err(Vec<...>)`.
- [ ] Unit tests: each error variant has a fixture that triggers it; one composite fixture triggers ≥3 errors at once.

**Acceptance:** `cargo test -p roki-daemon workflow::validate::tests` green. Multi-error fixture asserts `.len() >= 3` and contains expected variants.

---

## Task 5: `engine::sentinel` module

**Goal:** Per-state directive-file protocol via `$ROKI_DIRECTIVE_PATH`.

**Spec ref:** §2.3, §5.1, §5.2, §5.3.

**Files:** `engine/sentinel.rs` (new), `engine/mod.rs` (add `pub mod sentinel;`).

**Steps:**

- [ ] Define types:
  ```rust
  pub struct DirectivePayload {
      pub directive: String,
      pub outcome: Option<String>,
      pub extra: serde_json::Map<String, Value>,
  }
  pub enum SentinelError {
      Unparseable(String),       // JSON parse failed or `directive` missing
      ReadFailed(io::Error),
  }
  pub fn read_sentinel(path: &Path) -> Result<Option<DirectivePayload>, SentinelError>;
  ```
- [ ] `read_sentinel`: returns `Ok(None)` if path does not exist (file absent). Returns `Ok(Some(payload))` on valid JSON with `directive` field. Returns `Err(Unparseable)` on missing `directive` or invalid JSON. `extra` is the JSON object minus `directive` and `outcome`.
- [ ] Path allocator: `pub fn allocate_path(session_tempdir: &Path, state_id: &str, visit_n: u32) -> PathBuf` returns `<session_tempdir>/directives/<state_id>.<visit_n>.json`. Caller (state_runtime) creates the parent dir with `fs::create_dir_all` before spawning subprocess.
- [ ] Atomic-write contract is the SUBPROCESS's responsibility (operator-authored). Daemon does not write the file. Document this in module-level doc comment.
- [ ] Unit tests: path absent → `Ok(None)`. Valid JSON → `Ok(Some)`. Missing `directive` → `Err(Unparseable)`. Malformed JSON → `Err(Unparseable)`. Extra fields preserved in `extra` map.

**Acceptance:** `cargo test -p roki-daemon engine::sentinel::tests` green.

---

## Task 6: `engine::state_runtime` (one-state runner)

**Goal:** Run one state to completion: render Liquid → spawn subprocess → wait or stall-timeout → read sentinel → resolve next edge.

**Spec ref:** §2.4.

**Files:** `engine/state_runtime.rs` (new), `engine/outcome.rs` (modified — `FailureKind` updates).

**Steps:**

- [ ] Update `engine::outcome::FailureKind`:
  - Remove `IterExhausted` variant.
  - Add `RecursionBound`.
  - Failure metadata struct: rename `phase: PhaseKind` field to `state_id: String`.
  - Add `visit_n: u32` to the metadata struct.
- [ ] Define `StateOutcome`:
  ```rust
  pub enum StateOutcome {
      Edge { next: EdgeTarget, captures: TaskCaptures },
      Failure { kind: FailureKind, error_text: String },
  }
  pub struct TaskCaptures {
      pub exit_code: i32,
      pub duration_seconds: u64,
      pub directive: Option<DirectivePayload>,
      pub terminal: Option<serde_json::Value>,  // parsed claude/codex stream-json `result`
  }
  ```
- [ ] Define `pub trait StateRunner` with `async fn run_state(&self, state: &State, ctx: &CycleContext) -> StateOutcome`. Production impl `RealStateRunner` does:
  1. Render `state.run` (or read `state.uses` body) via Liquid with `ctx.liquid_globals`. Render error → `StateOutcome::Failure { kind: TemplateError, ... }`.
  2. If `state.if` is `Some`, render and truthy-test; falsy → `StateOutcome::Edge { next: state.on_done, captures: <skip> }`.
  3. Allocate sentinel path; create parent dir. Setup error → `Failure { kind: FsPoison }`.
  4. Spawn subprocess (`tokio::process::Command::spawn`). Set `ROKI_DIRECTIVE_PATH` and all `ROKI_*` env per spec §6.
  5. Stall window: `state.timeout` or `roki.toml [default.ai].stall_seconds`. Track stdout silence via existing slice 4 stall-detector adapted to single-state granularity.
  6. On exit: read sentinel.
  7. Match `(exit_code, sentinel)`:
     - `(0, None)` → `Edge { next: state.on_done }`.
     - `(0, Some(p))` → look up `p.directive` in `state.directives` ∪ built-in defaults table; missing → `Failure { kind: SchemaDrift }`. Hit → `Edge { next: target }`. If target is terminal and `p.outcome` is `Some`, override the terminal's outcome label for this cycle.
     - `(≠0, _)` → `Edge { next: state.on_fail }`.
     - `(killed_by_signal, _)` → `Failure { kind: ProcessCrash }`.
     - Stall fired → `Failure { kind: Stall }`.
- [ ] Define `MockStateRunner` for tests that maps `(state_id, visit_n)` → canned outcome.
- [ ] Unit tests using `MockStateRunner`: each `(exit_code, sentinel)` combination produces the expected `StateOutcome`.

**Acceptance:** `cargo test -p roki-daemon engine::state_runtime::tests` green. `cargo build` clean.

---

## Task 7: `engine::cycle_state` (state machine cycle driver)

**Goal:** Replace the pre/run/post phase loop with a state-machine driver that consumes `StateMachine` and runs to a terminal.

**Spec ref:** §2.4, §6 (data-flow), §7.

**Files:** `engine/cycle_state.rs` (new), `engine/cycle.rs` (deleted).

**Steps:**

- [ ] Delete `engine::cycle` (the old phase-loop module).
- [ ] In `cycle_state.rs`, define:
  ```rust
  pub struct CycleResult {
      pub terminal_id: StateId,
      pub outcome: String,
      pub iterations: u32,                 // total state visits
  }
  pub async fn run_cycle<R: StateRunner>(
      sm: &StateMachine,
      runner: &R,
      ctx: &mut CycleContext,
      escalation: &EscalationQueue,
  ) -> Result<CycleResult, FailureMetadata>;
  ```
- [ ] Loop body:
  ```
  state_id = sm.start
  visits: BTreeMap<StateId, u32> = {}
  task_captures: BTreeMap<StateId, TaskCaptures> = {}
  loop {
      if state_id ∈ sm.terminals → return CycleResult { terminal_id, outcome, iterations }
      visits[state_id] += 1
      ctx.cycle_iter += 1
      if visits[state_id] > sm.states[state_id].max_visits → emit RecursionBound failure
      ctx.set_liquid_globals(state_id, visits[state_id], &task_captures)
      outcome = runner.run_state(&sm.states[state_id], ctx).await
      match outcome {
          Edge { next, captures } => {
              task_captures.insert(state_id, captures)
              state_id = resolve_target(next, sm)
          }
          Failure { kind, error_text } => return Err(FailureMetadata { kind, state_id, visit_n: visits[state_id], error_text })
      }
  }
  ```
- [ ] `resolve_target` handles `EdgeTarget::State(id)` and `EdgeTarget::Terminal(id)` returning the next loop state_id (terminals are still tracked via `sm.terminals` lookup in the loop check).
- [ ] On `Err(FailureMetadata)`, the caller (`ticket_task`) routes to `on_failure:` rules or escalation queue per Task 8.
- [ ] `RecursionBound` failure: push to escalation queue immediately and return `Err`.
- [ ] Tests: integration with `MockStateRunner`. Linear chain → success terminal. Self-loop with `max_visits: 2` → `RecursionBound` after third visit. Branch via directive → correct terminal. `if:` skip → `on_done` taken without runner invocation.

**Acceptance:** `cargo test -p roki-daemon engine::cycle_state::tests` green. Mock-runner integration covers the worked examples in spec §3.

---

## Task 8: Failure routing wiring

**Goal:** Route daemon-detected failures (`process_crash`, `unparseable`, `schema_drift`, `fs_poison`, `stall`, `recursion_bound`, `template_error`) to `on_failure:` first-match. Recursive failure cycles → escalation queue.

**Spec ref:** §7.2, §7.3.

**Files:** `daemon/dispatcher.rs`, `daemon/ticket_task.rs`, `daemon/real_runner.rs`.

**Steps:**

- [ ] In `dispatcher.rs`, change `match_first` to consume `RuleEntry` (canonical) instead of `[[rule]]` (TOML). When-clause matching uses `WhenClause` from canonical types.
- [ ] In `ticket_task.rs`, after `cycle_state::run_cycle` returns:
  - `Ok(result)` → emit `cycle_completed` with `terminal_id` + `outcome`. For cleanup cycles, proceed to delete worktree + session_tempdir.
  - `Err(meta)` → if `meta.kind == RecursionBound` → push to escalation queue (already done in Task 7); emit `escalation_added` (slice 7 wire). Else evaluate `on_failure` rules: first-match by `(meta.kind, meta.state_id)` → spawn `kind: failure` cycle with `failure.*` Liquid context populated. No match → emit `failure_unhandled` event with `marker = none`.
- [ ] Failure-cycle that itself fails: `Err(meta')` from the failure cycle is NOT recursively routed; push to escalation queue with `recursion_bound`-flavored metadata (slice 7 trigger 1).
- [ ] In `real_runner.rs`, `run_state` emits `state_id` field on every event payload (replaces `phase`).
- [ ] Update `events::Event::CycleCompleted` payload struct: add `terminal_id: String`, `outcome: String`. Remove any `phase` field. Update structured-log writer.
- [ ] Update `events::Event::FailureUnhandled` and `events::Event::EscalationAdded` payloads: include `state_id` field; drop `phase`.
- [ ] Unit test in `ticket_task` (mock-runner): `(RecursionBound)` → escalation push. `(SchemaDrift, on_failure-match)` → failure cycle spawned. `(SchemaDrift, no-match)` → `failure_unhandled` emitted.

**Acceptance:** `cargo test -p roki-daemon daemon::ticket_task::tests` green. `cargo test -p roki-daemon engine::cycle_state::tests` green. No `phase` field in any structured-log emit (grep `\"phase\"` returns nothing in src/).

---

## Task 9: `roki workflow validate` CLI

**Goal:** Operator pre-flight: load + expand + validate a YAML file. Exit 0 on success, non-zero with multi-error report on failure.

**Spec ref:** §9.2.

**Files:** `cli/workflow_validate.rs` (new), `cli/mod.rs` (modified).

**Steps:**

- [ ] In `cli/mod.rs`, register `workflow` parent subcommand with `validate` and `graph` (Task 10) children using `clap` derive.
- [ ] In `workflow_validate.rs`:
  - Accept `<FILE>` arg.
  - Call `workflow::parse::parse_workflow_file` + `workflow::sugar::expand`.
  - On `Ok(_)`: exit 0, print nothing.
  - On parse error: print error with `file:line` if `serde_yaml_ng` provides location; exit 1.
  - On validation error: print every accumulated error one per line as `<file>:<rule_idx>: <ValidationError variant>: <details>`; exit 2.
- [ ] Unit test: valid fixture → exit 0. Invalid fixture (orphan target) → exit 2; stderr contains all expected error variants.

**Acceptance:** `cargo run --bin roki -- workflow validate docs/examples/WORKFLOW.minimal.yaml` exits 0. `cargo test -p roki-daemon cli::workflow_validate::tests` green.

---

## Task 10: `roki workflow graph` CLI

**Goal:** Render any rule's state machine as ASCII (default) or DOT.

**Spec ref:** §9.1.

**Files:** `cli/workflow_graph.rs` (new), `cli/mod.rs` (register subcommand).

**Steps:**

- [ ] Subcommand args: `<FILE>`, `--rule <selector>`, `--format <ascii|dot>`, `--out <path>`.
- [ ] Selector parsing: `rules[0]`, `cleanup[1]`, `on_failure[2]`. Empty → render all.
- [ ] Run parse + expand + validate. On validation failure → print errors and exit 2 (no rendering).
- [ ] ASCII renderer: `start: <id>`, then `<id> --on_done--> <next>`, `<id> --on_fail--> <next>`, `<id> --[directive]--> <next>` lines per state. Group by state. Terminals listed at end with `[terminal] <id> outcome=<x>`.
- [ ] DOT renderer: `digraph { ... }` with edges labeled, terminals as double-circles.
- [ ] Output to stdout or `--out` file.
- [ ] Snapshot tests for ASCII output on minimal sugar, retry sugar, canonical branch fixtures.

**Acceptance:** `cargo run --bin roki -- workflow graph docs/examples/WORKFLOW.minimal.yaml` prints ASCII. `cargo run --bin roki -- workflow graph docs/examples/WORKFLOW.annotated.yaml --format dot --out /tmp/wf.dot` writes DOT. Snapshot tests green.

---

## Task 11: `roki.toml` config schema rewrite

**Goal:** `[paths] workflow` defaults to `./WORKFLOW.yaml`. `[default.ai.session]` removed. `[default.ai.command]` → `[default.ai]` (single `cli` + `stall_seconds`).

**Spec ref:** §8.1.

**Files:** `crates/roki-daemon/src/config/roki.rs`, `crates/roki-daemon/src/config/defaults.rs` (if separate).

**Steps:**

- [ ] In `RokiConfig` struct: rename field `default_ai_command: AiCommandSection` → `default_ai: AiSection`. Drop `default_ai_session: AiSessionSection` field entirely.
- [ ] `AiSection { cli: String, stall_seconds: u64 }`.
- [ ] Update serde rename attrs / TOML key paths so `[default.ai]` parses.
- [ ] Update `default_workflow_path()` returning `PathBuf::from("./WORKFLOW.yaml")`.
- [ ] Update validation: `stall_seconds > 0`, `cli` non-empty.
- [ ] Update `test_default` to assert new shape.
- [ ] `grep -r 'default\.ai\.session' crates/` returns zero hits in src/.
- [ ] `grep -r 'default\.ai\.command' crates/` returns zero hits in src/.

**Acceptance:** `cargo test -p roki-daemon config::roki::tests` green. `cargo build` clean. Existing slice 1-7 tests fail (expected — fixed in Task 16).

---

## Task 12: Reference doc rewrite (`ref:config`, `ref:frontmatter`)

**Goal:** Authoritative canonical schema lives in `docs/reference/config.md` and `docs/reference/frontmatter.md`.

**Spec ref:** §11.6, §11.9.

**Files:** `docs/reference/config.md`, `docs/reference/frontmatter.md`.

**Steps:**

- [ ] In `docs/reference/config.md`:
  - Delete the `WORKFLOW.toml` schema section.
  - Add canonical `WORKFLOW.yaml` schema section with subsections: `admission`, `WhenClause`, `RuleEntry`, sugar `tasks:` form, canonical `states:` + `terminals:` form, state body fields, reserved terminal ids, built-in directive defaults, path resolution, validation rules.
  - Update `roki.toml` schema: `[default.ai]` (rename + merge), drop `[default.ai.session]`, `[paths] workflow` default → `./WORKFLOW.yaml`.
- [ ] In `docs/reference/frontmatter.md`:
  - Drop `session:` row from `workflow/*.md` table.
  - Update prose: "every state is command-shape; each invocation spawns a fresh subprocess".
  - `cli:` and `stall_seconds:` rows remain.
- [ ] Run `kusara validate` after each edit; fix any dangling cross-refs.

**Acceptance:** `kusara validate` returns OK. Manual read confirms canonical schema covers every type/field in spec §3 + §4.4.

---

## Task 13: FR doc rewrite

**Goal:** Update `fr:01`, `fr:02`, `fr:04→04-state-execution`, `fr:06`, `fr:08` to match the new model.

**Spec ref:** §11.1 - §11.5.

**Files:** `docs/fr/01-engine-model.md`, `docs/fr/02-configuration.md`, `docs/fr/04-phase-execution.md` → `docs/fr/04-state-execution.md`, `docs/fr/06-failure-handling.md`, `docs/fr/08-observability-logs.md`.

**Steps:**

- [ ] **`fr:01`**: rewrite §Phase loop, §Directive schema, §Inter-phase data flow, §Iteration cap, §Failure handling, §Cleanup, §Cold start sections. Cycle is a state machine; `cycle.iter` = state-visit count; data-flow table per spec §6; failure-kind table per spec §7.2; session-shape paragraph deleted; phase-loop diagram, pre/run/post phase optionality table, pre/post directive set table, `iter_exhausted` row removed.
- [ ] **`fr:02`**: replace `WORKFLOW.toml` schema section with YAML schema per spec §3. "Phase specification" subsection → "State body specification" per spec §3.4.
- [ ] **`fr:04`**: rename file to `04-state-execution.md`. Update `refs.id` in frontmatter to `fr:04-state-execution`. Update §Input channels: stdin and argv unchanged; remove "stdout last-JSON parse" wording; add §Sentinel channel per spec §5.
- [ ] Update every cross-reference to `fr:04-phase-execution` across the repo: `grep -rl 'fr:04-phase-execution' docs/ crates/` → fix each. `kusara validate` catches dangling.
- [ ] **`fr:06`**: failure-kind table per spec §7.2; remove `iter_exhausted` row; add `recursion_bound` row; update `[[on_failure]]` → `on_failure:` matcher discussion; `when.phase` semantics per spec §7.3.
- [ ] **`fr:08`**: cycle-engine event payloads gain `state_id` (string) + `visit_n` (int); `cycle_completed` gains `terminal_id`; `phase` field removed.
- [ ] Run `kusara validate` after each FR edit.

**Acceptance:** `kusara validate` returns OK. `grep -r 'iter_exhausted' docs/fr/` returns zero hits. `grep -r '\\bphase\\b' docs/fr/` returns only allowed contexts (e.g. cleanup-phase narrative, not control-flow).

---

## Task 14: `ref:cli` + `ref:log-events` updates

**Goal:** Reference docs for CLI and log events catch up.

**Spec ref:** §11.7, §11.8.

**Files:** `docs/reference/cli.md`, `docs/reference/log-events.md`.

**Steps:**

- [ ] **`ref:cli`**: add `roki workflow graph` row; add `roki workflow validate` row; rename `roki log --phase` → `--state` (flag accepts state id).
- [ ] **`ref:log-events`**: cycle-engine event rows: `phase` column → `state_id` + `visit_n`; add `recursion_bound` row to failure-kind enum; drop `iter_exhausted` row; `cycle_completed` payload includes `terminal_id`.
- [ ] Run `kusara validate`.

**Acceptance:** `kusara validate` returns OK. Manual diff matches spec §11.7 + §11.8.

---

## Task 15: YAML examples + delete TOML examples

**Goal:** `docs/examples/` carries minimal + annotated YAML samples; per-repo override sample; TOML samples deleted; `roki.minimal.toml` and `roki.annotated.toml` reference YAML workflow.

**Spec ref:** §11.10.

**Files:**

- New: `docs/examples/WORKFLOW.minimal.yaml`, `docs/examples/WORKFLOW.annotated.yaml`, `docs/examples/repos/bar.yaml`.
- Modified: `docs/examples/roki.minimal.toml`, `docs/examples/roki.annotated.toml`, `docs/examples/workflow-impl.md`, `docs/examples/workflow-judge.md`, `docs/examples/workflow-verdict.md`.
- Deleted: `docs/examples/WORKFLOW.minimal.toml`, `docs/examples/WORKFLOW.annotated.toml`.

**Steps:**

- [ ] Write `WORKFLOW.minimal.yaml`: single-task `tasks: [impl]` with `run.cmd` claude invocation. Mirrors slice 8 spec §3 minimal worked example.
- [ ] Write `WORKFLOW.annotated.yaml`: every key with comments, drawing from spec §3, §4, §5, §6.
- [ ] Write `repos/bar.yaml`: per-repo override with `rules:` only (no `admission:` block).
- [ ] Update `roki.minimal.toml` / `roki.annotated.toml`: `[paths] workflow = "./WORKFLOW.yaml"`; remove `[default.ai.session]` block; rename `[default.ai.command]` → `[default.ai]`.
- [ ] Update `workflow-impl.md`, `workflow-judge.md`, `workflow-verdict.md`: drop `session:` frontmatter key if present; prose "pre/run/post" → "states".
- [ ] Delete `WORKFLOW.minimal.toml`, `WORKFLOW.annotated.toml`.
- [ ] Run `kusara validate`.

**Acceptance:** `kusara validate` returns OK. `cargo run --bin roki -- workflow validate docs/examples/WORKFLOW.minimal.yaml` exits 0. Same for annotated.

---

## Task 16: Migrate slice 1-7 e2e fixtures to YAML

**Goal:** Existing e2e suite goes green against the new schema.

**Spec ref:** §12.2.

**Files:** `crates/roki-daemon/tests/e2e/support_workflow_yaml.rs` (new); existing TOML harness inlined per-fixture (no central `support_workflow_toml.rs` module — confirmed by `ls tests/e2e/`); all `crates/roki-daemon/tests/e2e/*.rs`.

**Steps:**

- [ ] Write `support_workflow_yaml.rs` (in `tests/e2e/`, matching the `support_cold_start.rs` pattern): builder functions `WorkflowYaml::admission(...).rule(...).cleanup(...).on_failure(...).render() -> String`. Output is canonical YAML (no sugar) for test determinism.
- [ ] Search current TOML emitter usage: `grep -rn 'WORKFLOW.toml\|workflow.toml\|render_workflow_toml\|to_string.*\.toml' crates/roki-daemon/tests/`. Each call site replaced with the YAML emitter.
- [ ] For each slice 1-7 e2e fixture under `crates/roki-daemon/tests/e2e/`:
  - Replace `workflow_toml::WorkflowToml::...` calls with `workflow_yaml::WorkflowYaml::...`.
  - Replace `[[rule]]`-flavored test scaffolding with YAML equivalents.
  - Replace `phase` field assertions with `state_id` (and `visit_n` if relevant).
  - Replace `iter_exhausted` failure-kind assertions with `recursion_bound`.
  - Replace `pre`/`run`/`post`-named state assertions with state-id-based assertions.
- [ ] After each fixture passes, commit (one fixture per commit ideal; group small fixtures if risk is low).
- [ ] After all slice 1-7 fixtures green, run `grep -rn 'WORKFLOW.toml\|workflow.toml' crates/roki-daemon/tests/` to confirm zero residual TOML emitter calls.

**Acceptance:** `cargo test -p roki-daemon --test '*'` green for every slice 1-7 fixture. `grep -r 'workflow_toml' crates/roki-daemon/tests/` returns zero hits.

---

## Task 17: New slice 8 e2e fixtures

**Goal:** 12 new e2e tests cover the new behavior surface.

**Spec ref:** §12.1.

**Files:** `crates/roki-daemon/tests/e2e/yaml_load_smoke.rs`, `sugar_linear_smoke.rs`, `sugar_retry_smoke.rs`, `canonical_branch_smoke.rs`, `sentinel_absent_smoke.rs`, `sentinel_unparseable_smoke.rs`, `state_on_fail_smoke.rs`, `recursion_bound_yaml_smoke.rs`, `validate_orphan_target_smoke.rs`, `cleanup_immediate_delete_yaml_smoke.rs`, `per_repo_override_smoke.rs`, `workflow_graph_cli_smoke.rs`, `workflow_validate_cli_smoke.rs`.

**Steps:**

- [ ] **`yaml_load_smoke`**: minimal YAML loads, daemon emits `daemon_ready`, no validation errors, no cycles spawned.
- [ ] **`sugar_linear_smoke`**: three-task `tasks:` chain. Each task is a stub script that exits 0 and writes no sentinel. Cycle visits all three then `__success__`. Assert `cycle_completed` with `outcome: success` and `iterations: 3`.
- [ ] **`sugar_retry_smoke`**: `tasks: [a, b]` with `b.directives: { retry: a }`. Stub `b` writes `{"directive":"retry"}` twice then `{"directive":"end"}`. Verify `max_visits` auto-injection on `a` (config.max_iterations from `roki.toml`). Assert third visit to `a` does NOT trigger `recursion_bound` (visit count below cap).
- [ ] **`canonical_branch_smoke`**: explicit SM with `directives: { skip: __no_action__ }`. Stub writes `{"directive":"skip"}`. Cycle terminates with `outcome: no_action`.
- [ ] **`sentinel_absent_smoke`**: stub exits 0, writes nothing. `on_done` taken; cycle completes successfully.
- [ ] **`sentinel_unparseable_smoke`**: stub writes invalid JSON `{garbled`. `unparseable` failure routes via `on_failure: when.kind: unparseable` to a handler stub.
- [ ] **`state_on_fail_smoke`**: stub exits 1. State's `on_fail` edge taken; cycle does NOT failure-route (no `on_failure:` rule fires).
- [ ] **`recursion_bound_yaml_smoke`**: explicit `max_visits: 2` on a self-loop state. Stub always writes `{"directive":"retry"}`. Third visit emits `recursion_bound` failure → escalation queue (slice 7 `escalation_added` event).
- [ ] **`validate_orphan_target_smoke`**: YAML with edge to undeclared state. Daemon refuses startup; stderr contains UnknownEdgeTarget error.
- [ ] **`cleanup_immediate_delete_yaml_smoke`**: body-less cleanup entry. Synchronous delete; one `cycle_completed` event with `cycle.kind: cleanup` and `iterations: 0`.
- [ ] **`per_repo_override_smoke`**: top-level YAML has `[[admission.repos]] workflow: repos/bar.yaml`. `repos/bar.yaml` declares its own `rules:`. Cycle spawned uses bar.yaml's rule, not the top-level rule list.
- [ ] **`workflow_graph_cli_smoke`**: invoke `roki workflow graph` against a fixture; assert ASCII output contains expected `start:`, edge, and terminal lines.
- [ ] **`workflow_validate_cli_smoke`**: valid fixture → exit 0. Invalid fixture → exit 2; stderr contains all expected ValidationError variants.
- [ ] Register every new fixture in `crates/roki-daemon/Cargo.toml` as a `[[test]]` entry (current count is 33; slice 8 adds 13 → 46). Format mirrors existing entries:
  ```toml
  [[test]]
  name = "yaml_load_smoke"
  path = "tests/e2e/yaml_load_smoke.rs"
  ```

**Acceptance:** Each fixture green individually. `cargo test -p roki-daemon --test '*' yaml_load sugar canonical sentinel state_on_fail recursion_bound_yaml validate_orphan cleanup_immediate per_repo workflow_graph_cli workflow_validate_cli` runs all 12 + 1 in one go. `grep -c '\[\[test\]\]' crates/roki-daemon/Cargo.toml` returns 46.

---

## Task 18: Sweep — fmt + clippy + full e2e

**Goal:** Whole-suite green and clean before merge.

**Files:** all (touched).

**Steps:**

- [ ] `cargo fmt --all -- --check` clean.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean. Allowed lints: `dead_code` and `unused_imports` only on stub modules (matching slice 7 chore commit `8f781ab`).
- [ ] `cargo test --workspace` green.
- [ ] `cargo test -p roki-daemon --test '*'` green (all e2e).
- [ ] `kusara validate` returns OK.
- [ ] `grep -r 'iter_exhausted' crates/ docs/` returns zero hits.
- [ ] `grep -r '\bWORKFLOW\.toml\b' crates/ docs/` returns zero hits except in changelog or migration prose if any.
- [ ] `grep -r 'pre.cmd\|post.cmd\|run.cmd' crates/` returns zero hits in src/ (test fixture YAML may legitimately use `run.cmd` style state ids — those are operator-named, not framework keys).
- [ ] Final commit: `chore(slice8): rustfmt + clippy clean`.

**Acceptance:** All checks above pass.

---

## Spec coverage check

| Spec section | Task(s) |
|---|---|
| §1 deliverables list | 0 (spec commit), all subsequent |
| §2.1 module layout | 1, 5, 6, 7, 8, 9, 10 |
| §2.2 types | 1 |
| §2.3 sentinel control channel | 5 |
| §2.4 cycle runtime loop | 6, 7 |
| §3.1 top-level shape | 2, 12 |
| §3.2 WhenClause grammar | 2, 12 |
| §3.2.1 path resolution | 2, 12 |
| §3.3 SugarOrCanonical body | 2, 3, 12 |
| §3.4 state body fields | 1, 6, 12 |
| §3.5 reserved state ids | 3, 12 |
| §4.1 - §4.5 sugar passes | 3 |
| §4.4 validation rules | 4 |
| §5.1 - §5.4 sentinel protocol | 5, 6 |
| §6 inter-state data flow | 6, 7, 13 (fr:01) |
| §7.1 state-local edges | 6, 8 |
| §7.2 failure kinds | 6, 7, 8, 13 (fr:06), 14 |
| §7.3 on_failure rules | 8, 13 (fr:06) |
| §7.4 cleanup cycle | 7, 8 |
| §8.1 roki.toml changes | 11, 15 |
| §8.2 Liquid templating | 6 (Liquid context) |
| §9.1 workflow graph CLI | 10 |
| §9.2 workflow validate CLI | 9 |
| §9.3 existing CLI rename | 12 (cli.md), task 8 update of `--phase` flag |
| §10 hot reload | (deferred — spec acknowledges) |
| §11.1 fr:01 update | 13 |
| §11.2 fr:02 update | 13 |
| §11.3 fr:04 rename | 13 |
| §11.4 fr:06 update | 13 |
| §11.5 fr:08 update | 13 |
| §11.6 ref:config rewrite | 12 |
| §11.7 ref:log-events update | 14 |
| §11.8 ref:cli update | 14 |
| §11.9 ref:frontmatter update | 12 |
| §11.10 examples | 15 |
| §12.1 new e2e | 17 |
| §12.2 updated e2e | 16 |
| §12.3 unit tests | 1-10 (each task includes its unit tests) |
| §13 implementation sequence | this plan |
| §14 boundaries | spec; reflected in deletions of session-shape code |
| §15 documented divergence | 4 (validation error for state id env-name) |

All 15 spec sections covered.

---

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| `serde_yaml_ng` deserializes `tasks:` and `states:` ambiguously when both present in error fixtures | Custom `Deserialize` impl on `SugarOrCanonical` that requires exactly one of the discriminating keys; reject "both present" with a parse error in Task 2. |
| Stall window per-state: existing slice 4 stall-detector is per-phase; reusing for state may regress slice 4 fixtures | In Task 6 keep the same `tokio::time::timeout` pattern but key on `state_id` + `visit_n` instead of `phase`. Slice 4 e2e migration in Task 16 verifies. |
| Pre/run/post terminology leaks into operator-facing docs as "states" prose feels less natural in some places | Doc rewrites in Task 13 use "state machine" + "state" consistently. Verify with `grep -ri 'pre.phase\|post.phase\|pre/run/post' docs/` after Task 13 — only allowed in historical / migration context (none expected since we don't write migration prose per docs-concise rule). |
| Test-harness YAML emitter (Task 16) might not cover every TOML quirk used by slice 1-7 fixtures | Build emitter incrementally; migrate one fixture at a time; emitter API expands as fixtures hit gaps. |
| `roki.toml` rename `[default.ai.command]` → `[default.ai]` breaks any pre-release operator config | Pre-release per spec; no migrator. Emit a clear error at boot if `[default.ai.command]` or `[default.ai.session]` is encountered: "key removed in slice 8; use `[default.ai]`". |
| Liquid template `cycle.iter` semantics shift from per-iter (pre/run/post triple) to total state visits — operator wrap-up templates that compare to `max_iterations - 1` may behave differently | Doc explicit in Task 13 (`fr:01` rewrite). E2e `sugar_retry_smoke` (Task 17) exercises the new semantics. |
| Sub-agent task ordering: Task 8 (failure routing) depends on Tasks 6+7+12; missing one mid-flight blocks Task 8 | Plan ordering enforces 1→2→3→4→5→6→7→8. Each task has explicit `cargo build` + `cargo test` acceptance gates. Sub-agent must complete each before next. |
| Existing 50KB+ files (`config/workflow.rs`, `engine/cycle.rs`, `engine/phase.rs`, `engine/session.rs`) carry shared types or helpers consumed by code outside the workflow loop | Before deletion: `cargo build` after stubbing each module's exports to `pub use {}` and observe linker errors → relocate truly-shared helpers to a new `engine/shared.rs` (or analogous) before final delete. Audit pass added implicitly to Tasks 1, 6, 7. |
| Daemon crate has no `lib.rs` — module declarations live in `main.rs` | Plan accounts for this in Task 1 + File Structure. |
| Each `[[test]]` entry in `Cargo.toml` requires explicit registration | Task 17 acceptance criterion verifies count = 46 (33 + 13). |

---

## Out of scope (deferred per spec §1 + §14)

- Hot reload of `WORKFLOW.yaml`.
- `outputs:` declaration block on states.
- Sub-workflow `uses: ./common/foo.yaml` with `with:` inputs.
- Persisted resume across daemon restart.
- Built-in primitive state kinds (`kind: linear.comment`, etc.).
- TOML coexistence (hard cut).
- DAG / parallel states.
- New event kinds (existing names carry new payload shape).
- HTTP API surface change (slice 7 endpoints stay deferred).
- Migration tooling for pre-release operators.
