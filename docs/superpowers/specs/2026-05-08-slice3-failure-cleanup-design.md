# Slice 3 — Failure Handling & Cleanup Design

Date: 2026-05-08
Scope: Layer `[[on_failure]]` first-match handler cycles, `[[cleanup]]` cycles (including the all-phases-omitted shorthand), `cycle.kind` propagation, `{{ failure.* }}` context vars, `FailureKind::FsPoison` for session_tempdir creation errors, and a `failure_unhandled` structured event on top of slice 2's session/stream-json engine. After this slice the binary can run a rule cycle, route any internal failure through `[[on_failure]]` (one level deep), run a cleanup cycle that triggers session_tempdir deletion plus a forward-compat `worktree_delete_requested` event, and surface unhandled failures via the structured event log instead of a bare `exit 1` log line.

## 1. Position in the Roadmap

Slice 3 lifts the failure-handling and cleanup surfaces deferred by slice 2:

- `roki-engine-on-failure` — `fr:01 §Failure handling` / `fr:06` first-match handler cycle, `{{ failure.* }}` vars, recursion bound (1 level).
- `roki-engine-cleanup` — `fr:01 §Cleanup` / `fr:07 §Cycle dispatch` cleanup-first dispatch, cleanup-shorthand sync delete, post-cycle session_tempdir deletion.
- `roki-engine-cycle-kind` — `fr:01 §Cycle kinds` `cycle.kind` value (`rule` / `cleanup` / `failure`) propagation through Liquid + env.
- `roki-engine-failure-event` — `fr:06 §Failure-handler cycle` `failure_unhandled` structured event for the no-match path.

Slice 2 (`docs/superpowers/specs/2026-05-08-slice2-session-streamjson-design.md`) provides: command + session phase shapes, stall watchdog, `run.terminal.json` extraction, `FailureKind::Stall`. The runtime currently exits 1 on any cycle failure with a bare tracing line. Slice 3 inserts the `[[on_failure]]` evaluation step between cycle failure and process exit, and inserts the `[[cleanup]]`-first dispatch step between admission and cycle launch.

Out-of-scope, deferred to later slices:

- Escalation queue (`fr:06 §Escalation queue`). Recursive failures and cleanup-time fs errors are routed through `failure_unhandled` instead, with a `marker` field. Daemon slice with HTTP/TUI surface picks the queue up.
- Worktree creation via `wt` (`fr:05`). Phases still run with `cwd = ghq base path`. Cleanup deletes `session_tempdir` only and emits `worktree_delete_requested` for the future worktree slice to consume. `FailureKind::FsPoison` covers session_tempdir creation only; worktree-side fs_poison sources land when worktree creation lands.
- Persistent daemon, queue preemption, cold-start enumeration, hot reload, per-repo overrides. Single-shot binary still: webhook → cycle → exit.
- Run-phase session shape (still rejected at config load per slice 2).

## 2. Architecture

### 2.1 Module layout

```
crates/roki-daemon/src/
├── runtime.rs                  // dispatch wiring; admission → cleanup-or-rule → cycle → on_failure
├── config/workflow.rs          // + parse [[cleanup]] (incl. shorthand) and [[on_failure]]
├── events.rs                   // NEW: structured event JSONL writer
├── capture.rs                  // session_tempdir creation errors → FsPoison
└── engine/
    ├── mod.rs                  // re-exports + CycleKind
    ├── outcome.rs              // + FailureKind::FsPoison, CycleKind, FailureMeta
    ├── cycle.rs                // + cycle.kind plumbing through PhaseContext
    ├── context.rs              // + ROKI_CYCLE_KIND + ROKI_FAILURE_* + {{ failure.* }} namespace
    ├── dispatch.rs             // NEW: evaluate(admitted, mode) → DispatchTarget
    ├── on_failure.rs           // NEW: first-match against FailureMeta + handler-cycle launch
    └── cleanup.rs              // NEW: shorthand sync path + post-cycle session_tempdir delete + worktree_delete event
```

`runtime::run_inner` is the only orchestrator. The `engine::dispatch` and `engine::on_failure` modules are pure decision layers — they do not spawn cycles themselves; they hand a target back to `run_inner`, which calls `engine::run_cycle` exactly the same way slice 2 does.

### 2.2 Types

```rust
// engine::outcome
pub enum CycleKind { Rule, Cleanup, Failure }

impl CycleKind {
    pub fn as_str(self) -> &'static str { /* "rule" | "cleanup" | "failure" */ }
}

pub enum FailureKind {
    Unparseable,
    SchemaDrift,
    ProcessCrash,
    TemplateError,
    IterExhausted,
    Stall,
    FsPoison,            // NEW
}

pub struct FailureMeta {
    pub failed_cycle_id: Uuid,
    pub kind: FailureKind,
    pub phase: PhaseKind,
    pub iter: u32,
    pub exit_code: Option<i32>,
    pub error_text: String,
}

// engine::dispatch
pub enum DispatchMode { Default, CleanupOnly }

pub enum DispatchTarget<'a> {
    Cycle { kind: CycleKind, entry: PhaseEntryRef<'a> },
    CleanupShorthand,
    NoMatch,
}
```

`PhaseEntryRef` is a borrow over a `Rule` / `Cleanup` / `OnFailure` config struct, all of which carry the same `pre / run / post` triple. The `engine::run_cycle` entry point takes `&PhaseEntry` (a structural slice of the entry) plus a `CycleKind` and a `Option<FailureMeta>`. The `FailureMeta` is `Some` only when `kind == Failure`.

### 2.3 Call graph

```
runtime::run_inner
  ├─ load roki.toml + WORKFLOW.toml (config now parses cleanup + on_failure)
  ├─ bind webhook listener
  ├─ admission::accept(ticket, &workflow, &me_ref)
  ├─ dispatch::evaluate(&admitted, &workflow, mode)
  │    Default mode:
  │      cleanup_match = first_match(admitted, &workflow.cleanups)
  │      if let Some(c) = cleanup_match:
  │        if c.is_shorthand(): return CleanupShorthand
  │        return Cycle{Cleanup, c}
  │      rule_match = first_match(admitted, &workflow.rules)
  │      return rule_match.map(|r| Cycle{Rule, r}).unwrap_or(NoMatch)
  │    CleanupOnly mode:
  │      cleanup_match = first_match(admitted, &workflow.cleanups)
  │      ... (rule list ignored; NoMatch on no cleanup hit)
  ├─ branch on DispatchTarget:
  │    CleanupShorthand:
  │      cleanup::delete_immediate(ticket_id, session_root)
  │        ├─ events::emit(cycle_completed, kind=cleanup, iters=0, outcome=null)
  │        ├─ events::emit(worktree_delete_requested, reason=cleanup_shorthand)
  │        └─ remove_dir_all(<session_root>/<ticket-id>)  // fs error → log + exit 1
  │      → exit 0
  │    NoMatch:
  │      tracing::info!("no match; awaiting next webhook")  // existing behavior
  │      → loop back to listener
  │    Cycle{kind, entry}:
  │      outcome = engine::run_cycle(executor, &admitted, entry, kind, /* failure */ None, ...)
  │      branch on outcome:
  │        Completed:
  │          events::emit(cycle_completed, kind, iters, outcome=operator_string_or_null)
  │          if kind == Cleanup:
  │            events::emit(worktree_delete_requested, reason=cleanup_terminal)
  │            cleanup::post_cycle_delete(ticket_id, session_root)  // fs error → cleanup_fs_error path
  │          → exit 0
  │        Failed{meta}:
  │          if kind == Failure:
  │            events::emit(failure_unhandled, marker=recursion_bound, ..meta)
  │            → exit 1
  │          on_failure::route(&workflow.on_failures, &meta):
  │            Some(handler_entry):
  │              outcome2 = engine::run_cycle(.., handler_entry, Failure, Some(meta), ..)
  │              recurse into branch-on-outcome (Failed under Failure → recursion_bound exit)
  │            None:
  │              events::emit(failure_unhandled, marker=none, ..meta)
  │              → exit 1
  └─ shutdown listener
```

The handler cycle uses a fresh `cycle_id`. `meta.failed_cycle_id` is the *original* cycle's id; that value stays attached to the `FailureMeta` value passed into the handler cycle's `PhaseContext`.

### 2.4 Capture layout

Unchanged from slice 1/2. Each cycle (rule, cleanup, failure) gets its own `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{pre,run,post}.{stdout,stderr,...}`. The original failed cycle's directory is retained on disk; only cleanup-cycle terminal deletion and cleanup-shorthand deletion remove `<ticket-id>/` (which removes every cycle-uuid subdir under it).

### 2.5 Events file

`<session_root>/<ticket-id>.events.jsonl` — sibling of the ticket directory, not a child. Append-only JSONL, one event per line. The sibling layout keeps the events file alive after a cleanup cycle deletes `<session_root>/<ticket-id>/`, so `cycle_completed` and `worktree_delete_requested` events emitted in the cleanup terminal step persist on disk. Writer flushes after each line via `write_all` + `BufWriter::flush`.

Event shapes (provisional; `fr:08` log catalog formalizes when its slice lands):

```json
{"event":"cycle_completed","ts":"2026-05-08T03:12:44Z","cycle_id":"...","cycle_kind":"rule","iters":3,"outcome":"success"}
{"event":"failure_unhandled","ts":"...","cycle_id":"...","cycle_kind":"rule","failure":{"kind":"unparseable","phase":"post","iter":2,"exit_code":0,"error_text":"..."},"marker":"none"}
{"event":"worktree_delete_requested","ts":"...","ticket_id":"OPS-123","cycle_id":"...","reason":"cleanup_terminal"}
```

`marker` ∈ `{none, recursion_bound, cleanup_fs_error}`. `cycle_id` on `worktree_delete_requested` is the cleanup cycle's id, or null for shorthand.

## 3. Config Parsing

### 3.1 Workflow surface additions

`WorkflowConfig` (now):

```rust
pub struct WorkflowConfig {
    pub admission: AdmissionSection,
    pub repo: Option<AdmissionRepo>,
    pub rules: Vec<Rule>,
    pub cleanups: Vec<Cleanup>,         // NEW
    pub on_failures: Vec<OnFailure>,    // NEW
}

pub struct Cleanup {
    pub when_status: Option<String>,
    pub when_labels_has_all: Vec<String>,
    pub pre: Option<PhaseBody>,
    pub run: Option<PhaseBody>,         // None only when shorthand
    pub post: Option<PhaseBody>,
}

impl Cleanup {
    pub fn is_shorthand(&self) -> bool {
        self.pre.is_none() && self.run.is_none() && self.post.is_none()
    }
}

pub struct OnFailure {
    pub when_kind: KindMatcher,
    pub when_phase: Option<PhaseKind>,
    pub pre: Option<PhaseBody>,
    pub run: PhaseBody,                 // required (non-shorthand)
    pub post: Option<PhaseBody>,
}

pub enum KindMatcher {
    Eq(FailureKind),
    In(Vec<FailureKind>),
    Not(FailureKind),
}
```

### 3.2 Validation rules

| Rule | Error variant |
|---|---|
| `[[cleanup]]` entry has any one of `pre`/`run`/`post` declared but not `run` | `WorkflowError::CleanupMissingRun` (mirrors `UnsupportedRunForm`) |
| `[[on_failure]]` entry missing `run` | `WorkflowError::OnFailureMissingRun` |
| `when.kind` value not in legal set | `WorkflowError::OnFailureUnknownKind { value }` |
| `when.phase` value not in `{pre, run, post}` | `WorkflowError::OnFailureUnknownPhase { value }` |
| `when.kind.in` is empty array | `WorkflowError::OnFailureEmptyKindIn` |
| `when.kind` + `when.kind.in` + `when.kind.not` more than one set | `WorkflowError::OnFailureKindMatcherConflict` |
| `[[cleanup]]` shorthand entry has any `when.*` field set | `WorkflowError::CleanupShorthandWithWhen` (shorthand is unconditional teardown per `fr:01 §40`) |
| `[[cleanup]]` or `[[on_failure]]` `run` resolves to `PhaseShape::Session` | `WorkflowError::SessionRunUnsupported` (slice 2 narrowing applies to every cycle-spawning entry) |

The legal `when.kind` set is the lowercase `as_str()` form of every `FailureKind` variant: `unparseable`, `schema_drift`, `process_crash`, `template_error`, `iter_exhausted`, `stall`, `fs_poison`.

### 3.3 Matcher vocabulary

`when.status` and `when.labels.has_all` on `[[cleanup]]` use the same matcher as `[[rule]]` (slice 1). `when.kind` / `when.phase` on `[[on_failure]]` are new and live entirely on `FailureMeta`, not on Linear ticket state. Both lists are first-match, top-to-bottom.

### 3.4 TOML examples

```toml
# Cleanup shorthand: unconditional teardown for any ticket already evaluated as
# cleanup-eligible by an earlier entry's filter. Useful as the last [[cleanup]]
# fallback. Per fr:01 §40 the shorthand has no [when] block.
[[cleanup]]

# Cleanup with a Linear-status guard.
[[cleanup]]
when.status = "Done"
run.cmd = "echo cleanup for {{ ticket.id }}"

# Failure handler with kind matcher.
[[on_failure]]
when.kind.in = ["unparseable", "schema_drift"]
when.phase = "post"
run.cmd = "claude -p '/post-mortem {{ failure.failed_cycle_id }}' --output-format stream-json"
post.prompt = "Output {directive: 'end'}"
```

## 4. Cycle Dispatch

### 4.1 Default mode

Per `fr:01 §38` and `fr:07 §Cycle dispatch`: cleanup before rule.

```
fn evaluate(admitted, workflow, DispatchMode::Default) -> DispatchTarget {
    if let Some(c) = first_match(admitted, &workflow.cleanups) {
        return if c.is_shorthand() { CleanupShorthand } else { Cycle{Cleanup, c} };
    }
    if let Some(r) = first_match(admitted, &workflow.rules) {
        return Cycle{Rule, r};
    }
    NoMatch
}
```

### 4.2 CleanupOnly mode

Same as Default but the `[[rule]]` step is skipped. `NoMatch` becomes a no-op exit 0 with an info log naming the ticket.

### 4.3 Cleanup-shorthand match semantics

A shorthand entry has no `when.*` fields per §3.2. It is therefore an unconditional last-resort match. Operators that want a guarded shorthand author a non-shorthand entry with all phases set to no-op cmds (e.g. `run.cmd = "true"`); the shorthand form is reserved for unconditional teardown.

## 5. Cleanup Cycle Lifecycle

### 5.1 Shorthand path

```rust
pub fn delete_immediate(ticket_id: &str, session_root: &Path, events: &EventWriter) -> Result<()> {
    let cycle_id = Uuid::new_v4();
    events.emit(Event::CycleCompleted {
        cycle_id, cycle_kind: CycleKind::Cleanup, iters: 0, outcome: None,
    })?;
    events.emit(Event::WorktreeDeleteRequested {
        ticket_id, cycle_id: Some(cycle_id), reason: "cleanup_shorthand",
    })?;
    let dir = session_root.join(sanitize(ticket_id));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) | Err(e) if e.kind() == NotFound => Ok(()),
        Err(e) => {
            events.emit(Event::FailureUnhandled { /* marker: cleanup_fs_error, kind: FsPoison, .. */ })?;
            Err(e.into())  // exit 1
        }
    }
}
```

The synthetic `cycle_id` is generated solely so the structured event has a stable id field. No iter dirs are written.

### 5.2 Normal cleanup cycle path

A non-shorthand cleanup entry runs through `engine::run_cycle` like any other cycle. On `CycleOutcome::Completed`, `runtime::run_inner` calls `cleanup::post_cycle_delete`:

```rust
pub fn post_cycle_delete(ticket_id, session_root, cycle_id, events) -> Result<()> {
    events.emit(Event::WorktreeDeleteRequested {
        ticket_id, cycle_id: Some(cycle_id), reason: "cleanup_terminal",
    })?;
    let dir = session_root.join(sanitize(ticket_id));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) | Err(e) if e.kind() == NotFound => Ok(()),
        Err(e) => {
            events.emit(Event::FailureUnhandled { /* marker: cleanup_fs_error */ })?;
            Err(e.into())
        }
    }
}
```

The deletion runs after the cycle's iter dirs are already on disk; it removes the entire `<ticket-id>/` subtree. The events file lives at `<session_root>/<ticket-id>.events.jsonl` (sibling, see §2.5), so the just-emitted `cycle_completed` and `worktree_delete_requested` events survive the deletion.

### 5.3 Cleanup-time fs error → failure_unhandled

Per `fr:06 §35`, cleanup-time fs errors land in the escalation queue. Slice 3 has no queue. Substitute: emit `failure_unhandled` with `marker = cleanup_fs_error`, `failure.kind = fs_poison`, `failure.phase = null` (cycle is over), `failed_cycle_id = cleanup cycle id`. Process exits 1.

### 5.4 What is NOT deleted

Worktree creation does not exist yet, so cleanup deletes `session_tempdir` only. The `worktree_delete_requested` event is the forward-compat handoff: when worktree slice lands, that slice's consumer reads the event (or wires the call directly) and deletes the worktree path. Slice 3 does not import or call any worktree library.

## 6. Failure Routing

### 6.1 FailureMeta capture

`engine::cycle::run_cycle` already detects every failure variant slice 1+2 emit. Slice 3 changes the return shape: instead of `CycleOutcome::Failed { kind: FailureKind, iter: u32 }` (slice 1), the variant now carries the full `FailureMeta`. `phase`, `iter`, `exit_code`, and `error_text` are populated at the point of detection inside `engine::cycle` / `engine::phase` / `engine::session`. The `failed_cycle_id` is the cycle currently running.

### 6.2 on_failure first-match

```rust
pub fn route<'a>(
    on_failures: &'a [OnFailure],
    meta: &FailureMeta,
) -> Option<&'a OnFailure> {
    on_failures.iter().find(|e| e.matches(meta))
}

impl OnFailure {
    pub fn matches(&self, meta: &FailureMeta) -> bool {
        match &self.when_kind {
            KindMatcher::Eq(k)  => *k == meta.kind,
            KindMatcher::In(ks) => ks.contains(&meta.kind),
            KindMatcher::Not(k) => *k != meta.kind,
        } && self.when_phase.map_or(true, |p| p == meta.phase)
    }
}
```

### 6.3 Handler cycle launch

`engine::run_cycle(executor, admitted, handler_entry, CycleKind::Failure, Some(meta), session_root, cfg)` — same call surface as a rule cycle. The handler cycle gets its own UUID, its own iter dirs under the same `<ticket-id>/`, the shape (command vs session) declared by the entry, and the slice 1 `[engine].max_iterations` cap.

The handler's `PhaseContext` differs from a rule cycle's only in that:

- `cycle.kind = "failure"`, `ROKI_CYCLE_KIND = "failure"`.
- `failure.*` namespace is populated (see §7).
- `pre.*` / `post.*` / `run.*` from the failed cycle are NOT exposed; the handler reads those via `roki log --cycle <failed_cycle_id>` per `fr:06 §47`. (The slice 1 inter-phase data flow exposes only the *current* cycle's last completed iteration.)

### 6.4 Recursion bound

If `cycle.kind == Failure` and the cycle returns `Failed{meta}`, the engine refuses to evaluate `[[on_failure]]` again. `runtime::run_inner` emits `failure_unhandled` with `marker = recursion_bound` and exits 1. Per `fr:06 §51` the canonical destination is the escalation queue; with no queue in slice 3 the structured event is the only surface.

### 6.5 No-match (rule cycle failure with empty / non-matching `[[on_failure]]`)

Emit `failure_unhandled` with `marker = none`, populate every `failure.*` field. Exit 1.

## 7. Liquid + Env Additions

### 7.1 cycle.kind

Slice 1+2 already carry `cycle.id`, `cycle.iter`, `cycle.trigger`. Slice 3 adds:

| Liquid | Env | Value |
|---|---|---|
| `{{ cycle.kind }}` | `ROKI_CYCLE_KIND` | `rule` / `cleanup` / `failure` |

`cycle.trigger` stays at `runtime` (slice 3 has no cold-start path). The value is set in `PhaseContext` at cycle start and is identical across all phases of the cycle.

### 7.2 failure.* namespace

Populated only when `cycle.kind == failure`. Per `fr:01 §107`:

| Liquid | Env | Value |
|---|---|---|
| `{{ failure.kind }}` | `ROKI_FAILURE_KIND` | `unparseable` / `schema_drift` / `process_crash` / `template_error` / `iter_exhausted` / `stall` / `fs_poison` |
| `{{ failure.failed_cycle_id }}` | `ROKI_FAILURE_FAILED_CYCLE_ID` | UUID string |
| `{{ failure.phase }}` | `ROKI_FAILURE_PHASE` | `pre` / `run` / `post` |
| `{{ failure.iter }}` | `ROKI_FAILURE_ITER` | int |
| `{{ failure.exit_code }}` | `ROKI_FAILURE_EXIT_CODE` | int (only when applicable; absent variable when `None`) |
| `{{ failure.error_text }}` | `ROKI_FAILURE_ERROR_TEXT` | string |

When `meta.exit_code` is `None`, both the Liquid value (`{{ failure.exit_code }}`) and the env var (`ROKI_FAILURE_EXIT_CODE`) are absent — Liquid renders empty string and the env var is not set. Templates that need a default use `{{ failure.exit_code | default: -1 }}`.

For non-failure cycles `failure.*` is the empty object `{}` and no `ROKI_FAILURE_*` env vars are set.

## 8. FsPoison Wiring

`engine::outcome::FailureKind::FsPoison` is added with `as_str() = "fs_poison"`.

`crates/roki-daemon/src/capture.rs::open_session_phase_files` (and the slice 1 `prepare_iter_dir` it sits behind) currently surface `std::io::Error` upward as `SkeletonError::Capture`. Slice 3 changes the surface: capture errors raised before any phase has run for the cycle are wrapped as `FailureMeta { kind: FsPoison, phase: <next-phase>, iter: <next-iter>, exit_code: None, error_text: io::Error display }` and routed through `[[on_failure]]`. The `phase` field is the phase that *would have* run; the `iter` is the iteration the engine was preparing.

This applies to:

- Initial cycle dir creation (`<session_root>/<ticket-id>/cycle-<uuid>/`).
- Per-iter dir creation (`iter-<n>/`).
- Per-phase capture file open.

Worktree-side fs_poison sources do not exist yet (no worktree creation).

## 9. Events Writer

```rust
// events.rs
pub struct EventWriter {
    file: BufWriter<File>,
    path: PathBuf,
}

impl EventWriter {
    pub fn open(session_root: &Path, ticket_id: &str) -> io::Result<Self>;
    pub fn emit(&mut self, event: &Event) -> io::Result<()> {
        // serde_json::to_writer + write '\n' + flush
    }
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    CycleCompleted { ts: String, cycle_id: Uuid, cycle_kind: CycleKind, iters: u32, outcome: Option<String> },
    FailureUnhandled { ts: String, cycle_id: Uuid, cycle_kind: CycleKind, failure: FailureMetaSer, marker: FailureMarker },
    WorktreeDeleteRequested { ts: String, ticket_id: String, cycle_id: Option<Uuid>, reason: WorktreeDeleteReason },
}
```

`ts` is RFC3339 in UTC. Writer is created lazily when the first event for a ticket is emitted; if creation fails, the daemon falls back to `tracing::error!` with the same fields (no second-order failure).

## 10. Implementation Order

Tasks are listed so each one compiles and the test suite is green before the next starts. Each task is small enough to PR independently.

1. **`CycleKind` + `FailureMeta` types.** Add to `engine::outcome` as new types only (no `CycleOutcome` variant change yet). `FailureMeta` is unused in this step; tests cover only constructor + `as_str` round-trips.
2. **`engine::cycle` returns `FailureMeta`.** Replace `CycleOutcome::Failed { kind, iter }` with `CycleOutcome::Failed { meta: FailureMeta }`; populate `phase` / `iter` / `exit_code` / `error_text` at every detection site. `runtime::run_inner` translates the new variant to its old-shape `SkeletonError::PhaseInfra(CycleFailed { kind, iter })` (extracted from `meta`) so the existing exit-1 surface is unchanged.
3. **`cycle.kind` plumbing.** Thread `CycleKind` into `engine::run_cycle` and `PhaseContext`. Liquid + env exposure. All call sites pass `CycleKind::Rule`.
4. **Events writer.** `events.rs` + the three event types. Emit `cycle_completed` from `runtime::run_inner` on cycle success; remove the corresponding tracing line.
5. **Config parsing for `[[cleanup]]` (incl. shorthand).** `WorkflowConfig::cleanups` populated; matcher reuses slice 1's `Rule` matcher. Tests cover shorthand, when-status, when-labels, validation errors.
6. **Config parsing for `[[on_failure]]`.** `WorkflowConfig::on_failures` populated; `KindMatcher` + `when.phase`. Tests cover all three matcher forms + every error variant.
7. **`engine::dispatch::evaluate`.** Replace the inline `rule::first_match` call in `runtime::run_inner` with a `dispatch::evaluate` call. Default mode only at this step; rule list still wins when cleanup is empty.
8. **Cleanup-shorthand sync path.** `engine::cleanup::delete_immediate` + `worktree_delete_requested` event. End-to-end test: ticket with shorthand match → no cycle dirs created, events emitted, `<ticket-id>/` is gone (or never existed).
9. **Normal cleanup cycle.** `cleanup::post_cycle_delete` after `CycleOutcome::Completed` when `cycle_kind == Cleanup`. Test: cleanup cycle with run + post → cycle iter dirs created, then deleted, events emitted in order.
10. **`FailureKind::FsPoison`.** Wire capture errors through `FailureMeta`. Test: deliberately unwritable session_root → `Failed { meta.kind = FsPoison }`.
11. **`engine::on_failure::route` + handler-cycle launch.** Recursion bound enforcement. `failure_unhandled` event for both no-match and recursion-bound cases. Tests: each `FailureKind` × each matcher form, plus recursion-bound trigger.
12. **CLI `cleanup` subcommand.** `DispatchMode::CleanupOnly` plumbing. End-to-end test: ticket admitted, `[[cleanup]]` matches → cleanup cycle runs; `[[rule]]` matches but no cleanup → exit 0 with no-match log.
13. **Slice-3 backwards compat sweep.** Confirm slice 1/2 fixtures load and run unchanged (no `[[cleanup]]`, no `[[on_failure]]`). The `cleanups` and `on_failures` vectors default to empty.

## 11. Testing Strategy

Slice 1+2 already carry the in-process integration test scaffolding (binary-as-subprocess, fixture WORKFLOW.toml, fake Linear webhook). Slice 3 adds:

- **Config parser unit tests** for every new validation error variant (§3.2).
- **Dispatch unit tests** for cleanup-before-rule order and CleanupOnly mode.
- **Matcher unit tests** for `KindMatcher::Eq` / `In` / `Not` × phase optional/present.
- **Cleanup-shorthand integration test** — fixture with `[[cleanup]]` shorthand; webhook for an admitted ticket; assert no cycle dirs exist, events.jsonl has exactly two lines (`cycle_completed` + `worktree_delete_requested`), `<ticket-id>/` is removed.
- **Cleanup cycle integration test** — fixture with non-shorthand cleanup that runs `true` for run + `{"directive":"end"}` for post; assert cycle dirs are created during execution and deleted after, events.jsonl ordering correct.
- **Failure handler integration tests** — one per `FailureKind` × handler-shape (command, session). Use a fixture rule whose run phase exits non-zero / emits a malformed directive / busy-loops past the stall window / etc., paired with `[[on_failure]]` that runs `echo handled` and posts `directive: 'end'`. Assert: original cycle dir retained, handler cycle dir created with `{{ failure.* }}` available, events.jsonl carries `cycle_completed kind=failure`.
- **Recursion-bound integration test** — handler entry whose run phase itself fails. Assert: events.jsonl carries `failure_unhandled marker=recursion_bound`, exit code 1.
- **No-match integration test** — rule fails, `[[on_failure]]` empty or non-matching. Assert: `failure_unhandled marker=none`, exit code 1.
- **FsPoison integration test** — point `session_root` at a path the binary cannot create (parent dir is a regular file). Assert: `FailureKind::FsPoison` routes through `[[on_failure]]` when present.
- **Cleanup-fs-error integration test** — make the ticket dir undeletable (e.g. file owned by another uid in CI; on Darwin, a fixture that opens a file inside the dir and locks it via `fcntl`). Assert: `failure_unhandled marker=cleanup_fs_error`.
- **CLI `cleanup` subcommand smoke test** — same fixture as cleanup integration, invoked via `roki cleanup` instead of `roki`. Assert rule list is ignored.

## 12. Backwards Compatibility

A slice 1/2 `WORKFLOW.toml` with no `[[cleanup]]` and no `[[on_failure]]` loads and runs unchanged: `cleanups` and `on_failures` default to empty, `dispatch::evaluate` falls straight through to the rule first-match, and the no-failure-handler path emits `failure_unhandled` instead of the slice-2 bare `tracing::error!` line. The structured event is additive; existing log-based observability still sees the tracing line for the cycle-failed case (kept as a stderr mirror so CI logs do not regress).

## 13. Dependency Additions

- `time` crate already present (slice 2). Reused for RFC3339 timestamps in events.
- `serde_json` already present. Used for events serialization.
- `uuid` already present. Used for synthetic shorthand cycle ids.
- No new crates.

## 14. Open Questions Deferred to Slice 4+

- Escalation queue (in-memory) + HTTP `GET /api/escalations` + TUI rendering — needs daemon, HTTP server, TUI.
- Worktree creation via `wt`. Once landed, `[[cleanup]]` deletes the worktree alongside session_tempdir; `worktree_delete_requested` event becomes a direct call. `FailureKind::FsPoison` extends to worktree-side fs errors.
- Persistent daemon lifecycle, queue preemption, cold-start enumeration.
- Hot reload of `WORKFLOW.toml` and `workflow/*.md`.
- Per-repo `WORKFLOW.toml` overrides.
- Run-phase session shape (still rejected at config load).
- `roki log --cycle <failed_id>` CLI is invoked by handler templates per `fr:06 §47`; the log-access slice ships the `roki log` command. Slice 3 leaves `error_text` content abbreviated (head + tail of stderr) so handlers without `roki log` still get usable failure context.

These are intentionally out of scope; the slice surface is sized so a single binary run can route a failed rule cycle into a handler cycle and a cleanup cycle into a delete, with no daemon, no queue, no worktree.
