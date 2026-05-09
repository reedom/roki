# Slice 8 — Workflow YAML + State Machine Design

Date: 2026-05-09
Scope: Replace `WORKFLOW.toml` (TOML, pre/run/post phase loop) with `WORKFLOW.yaml` (YAML, explicit state machine with linear-array sugar). Replace stdout-JSON directive parsing with a sentinel-file control channel. Remove the long-lived AI session shape — every state spawns a fresh subprocess. Re-express slice 7's `[[on_failure]]` semantics in the new schema. Daemon config (`roki.toml`) stays TOML.

## 1. Position in the Roadmap

Slice 8 closes:

- `roki-workflow-file-format` — `fr:02 §Workflow file` switches to YAML. `[admission]`, `[[admission.repos]]`, `[[rule]]`, `[[cleanup]]`, `[[on_failure]]` re-expressed as YAML maps and lists.
- `roki-engine-state-machine` — `fr:01 §Cycle engine` rewritten around a state-machine cycle model. Pre/run/post phase loop and directive set `{run, end, pre}` retired.
- `roki-engine-control-channel` — `fr:04 §Control channel` switches from "last JSON object on stdout" to a per-state sentinel file written by the operator's process. Stdout becomes pure work output.
- `roki-engine-sugar-expansion` — Linear-array `tasks:` sugar compiles to canonical state machine at config-load time. Engine only sees canonical.
- `roki-engine-failure-routing` — On-process-fail and on-state-fail edges are state-local; daemon-detected internal failures still spawn `kind: failure` cycles via top-level `on_failure:` rules.
- `roki-engine-no-session-shape` — `[default.ai.session]` config block, `session:` frontmatter key on `workflow/*.md`, and the cross-phase long-lived subprocess model are removed. All states are command-shape.
- `roki-cli-workflow-graph` — `roki workflow graph <file>` renders any rule's state machine as ASCII or DOT.
- `roki-cli-workflow-validate` — `roki workflow validate <file>` runs sugar→canonical expansion + the 8 validation rules and exits 0 / non-zero with a multi-error report.
- `roki-ref-config-yaml` — `docs/reference/config.md` carries the canonical `WORKFLOW.yaml` schema (admission, rules, cleanup, on_failure, state, terminal). `WORKFLOW.toml` schema removed.

Slices 1–7 provide: cycle engine baseline, admission filter, webhook receiver, persistent dispatcher with diff cache, ticket-task registry, drain semantics, cold-start enumeration, admission-eviction, orphan reconcile, escalation queue, structured event writer.

Out of scope, deferred:

- **Hot reload of `WORKFLOW.yaml`**. Current contract: restart-required. Hot reload remains a later slice (was already deferred in slice 7).
- **Per-state `outputs:` declaration**. State-machine model exposes raw sentinel JSON via `{{ tasks.<id>.directive }}`. Named outputs left for a follow-up.
- **Sub-workflow `uses: ./common/foo.yaml` with `with:` inputs**. Per-repo override file (declared via `[[admission.repos]]`) is the only sub-workflow form in this slice.
- **Persisted resume across daemon restart** (`fr:07 §Recovery`). Cycles still abort on daemon stop; cold-start re-enumerates.
- **Built-in primitive states** (`kind: linear.comment` etc.). Every state runs an operator-authored process.
- **TOML coexistence**. Hard cut: parser only accepts YAML. Pre-release.

## 2. Architecture

### 2.1 Module layout

```
crates/roki-daemon/src/
├── workflow/
│   ├── mod.rs                   // NEW: WorkflowFile (top-level YAML doc)
│   ├── parse.rs                 // NEW: serde_yaml → SugarRule | CanonicalRule
│   ├── sugar.rs                 // NEW: tasks-array → state-machine expansion (5 passes)
│   ├── canonical.rs             // NEW: StateMachine, State, Terminal, EdgeTarget types
│   ├── validate.rs              // NEW: 8 validation rules (§4.4)
│   └── liquid.rs                // moved from engine/render.rs; unchanged grammar
├── engine/
│   ├── cycle.rs                 // rewritten around StateMachine, not phase enum
│   ├── state_runtime.rs         // NEW: runs one state (spawn → wait → read sentinel → pick edge)
│   ├── sentinel.rs              // NEW: ROKI_DIRECTIVE_PATH protocol
│   ├── outcome.rs               // FailureKind enum: drop schema_drift_unparseable narrow forms; add recursion_bound
│   └── data_flow.rs             // tasks.<id>.* exposure (env + Liquid context)
├── config/
│   └── mod.rs                   // RokiConfig path lookup: WORKFLOW.yaml not WORKFLOW.toml
├── cli/
│   └── workflow_graph.rs        // NEW: `roki workflow graph` subcommand (ASCII + DOT)
└── runtime.rs                   // boot path loads YAML; per-repo override is .yaml

docs/examples/
├── WORKFLOW.minimal.yaml        // NEW
├── WORKFLOW.annotated.yaml      // NEW
├── repos/bar.yaml               // NEW: per-repo override example
├── workflow-impl.md             // unchanged (frontmatter unchanged)
└── workflow-judge.md            // unchanged
```

`WORKFLOW.minimal.toml` / `WORKFLOW.annotated.toml` are deleted in this slice.

### 2.2 Types

```rust
// workflow::canonical

#[derive(Debug, Clone)]
pub struct WorkflowFile {
    pub admission: Admission,                 // top-level only; absent in per-repo override
    pub rules: Vec<RuleEntry>,
    pub cleanup: Vec<RuleEntry>,
    pub on_failure: Vec<RuleEntry>,
}

#[derive(Debug, Clone)]
pub struct Admission {
    pub assignee: AssigneeMatcher,
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Clone)]
pub struct RuleEntry {
    pub when: Option<WhenClause>,             // None = catch-all (allowed only in repos / on_failure)
    pub state_machine: StateMachine,          // result of sugar expansion
}

#[derive(Debug, Clone)]
pub struct StateMachine {
    pub start: StateId,
    pub states: BTreeMap<StateId, State>,     // BTree for deterministic iteration
    pub terminals: BTreeMap<StateId, Terminal>,
}

#[derive(Debug, Clone)]
pub struct State {
    pub id: StateId,
    pub body: StateBody,                      // Run | Uses
    pub if_cond: Option<LiquidExpr>,          // skip if false; advance to on_done
    pub on_done: EdgeTarget,
    pub on_fail: EdgeTarget,
    pub directives: BTreeMap<DirectiveName, EdgeTarget>,
    pub max_visits: u32,                      // 1 if not declared and not on cycle
    pub timeout: Option<Duration>,            // overrides default stall window
}

#[derive(Debug, Clone)]
pub enum StateBody {
    Run { cmd: LiquidString },                // inline shell command
    Uses { path: PathBuf },                   // workflow/*.md path-form
}

#[derive(Debug, Clone)]
pub struct Terminal {
    pub id: StateId,
    pub outcome: String,                      // free-form operator label
}

#[derive(Debug, Clone)]
pub enum EdgeTarget {
    State(StateId),
    Terminal(StateId),                        // resolved at validation time
}

pub type StateId = String;
pub type DirectiveName = String;
```

### 2.3 Sentinel control channel

```rust
// engine::sentinel

pub struct DirectivePayload {
    pub directive: String,                    // operator-chosen name; matched against state.directives
    pub outcome: Option<String>,              // optional terminal-label override
    pub extra: serde_json::Map<String, Value>,// remaining JSON fields exposed via tasks.<id>.directive.*
}

pub fn read_sentinel(path: &Path) -> Result<Option<DirectivePayload>, SentinelError>;
```

The daemon allocates a unique directive-file path per state invocation under `<session_tempdir>/directives/<state_id>.<visit_n>.json` and exports `ROKI_DIRECTIVE_PATH` to the subprocess. The subprocess writes its directive to that path before exit (atomic write: write to `.tmp`, rename). Absence of the file at exit-time = no directive, daemon takes the `on_done` (exit==0) or `on_fail` (exit!=0) edge.

### 2.4 Cycle runtime loop

```
cycle start with state = SM.start
loop {
    if state ∈ SM.terminals → emit cycle_completed(outcome = terminal.outcome); break
    visits[state] += 1
    if visits[state] > state.max_visits → recursion_bound failure (escalation queue per slice 7)
    if state.if_cond is Some(expr) and Liquid(expr) → false → state = state.on_done; continue
    render state.body via Liquid (template_error on render fail)
    spawn subprocess, set ROKI_DIRECTIVE_PATH and ROKI_* env
    wait for exit (stall window per state.timeout or default)
    sentinel = read_sentinel(path)
    next = match (exit_code, sentinel) {
        (0, None)               => state.on_done,
        (0, Some(p))            => state.directives.get(&p.directive)
                                     .ok_or(schema_drift)?,
        (≠0, _)                 => state.on_fail,
    }
    capture sentinel.extra into tasks.<state.id>.directive.*
    state = next
}
```

## 3. Workflow file format

### 3.1 Top-level shape

```yaml
admission:
  assignee: <string>                         # required; "me" resolves token holder
  repos:                                     # required; ≥1 entry
    - ghq: <string>
      when: <WhenClause>                     # optional
      workflow: <path>                       # optional; per-repo override file

rules:                                       # 0..N; first-match
  - when: <WhenClause>
    <SugarOrCanonical>

cleanup:                                     # 0..N; first-match; evaluated before rules
  - when: <WhenClause>
    <SugarOrCanonical>                       # OR omitted entirely (immediate-delete shorthand)

on_failure:                                  # 0..N; first-match
  - when: <WhenClause>
    <SugarOrCanonical>
```

Per-repo override file (referenced via `[[admission.repos]] workflow:` key) carries `rules:` / `cleanup:` / `on_failure:` only — no `admission:` block. Schema otherwise identical.

### 3.2 `WhenClause` grammar

```yaml
when:
  status: <scalar>                           # equality
  status: { not: <scalar> }                  # negation
  status: { in: [<scalar>, ...] }            # set membership

  labels: { has_all:  [<string>, ...] }
  labels: { has_any:  [<string>, ...] }
  labels: { has_none: [<string>, ...] }

  assignee: <scalar>                         # rule-level refinement (admission gates coarsely)
  repo: <ghq path>                           # admission-resolved repo (rule-level only)
  kind: <scalar>                             # on_failure only: failure kind
  phase: <scalar>                            # on_failure only: state id that emitted the failure

  title: { regex: <string> }                 # admission.repos only
  title: { starts_with: <string> }           # admission.repos only
  title: { contains: <string> }              # admission.repos only
  body:  { contains: <string> }              # admission.repos only
```

All `when.*` keys AND together. OR by writing more list entries.

### 3.2.1 Path resolution

| Path field | Resolved relative to |
|---|---|
| `roki.toml [paths] workflow` | `roki.toml` directory (or absolute) |
| `[[admission.repos]] workflow:` (per-repo override) | top-level `WORKFLOW.yaml` directory |
| State `uses:` inside top-level `WORKFLOW.yaml` | top-level `WORKFLOW.yaml` directory |
| State `uses:` inside per-repo override file | the override file's directory |

All path keys accept absolute or `~`-prefixed paths (tilde expanded at load time). Symlink escape outside the resolution root is a `fs_poison` failure when the daemon goes to spawn the state.

### 3.3 `SugarOrCanonical` body

A rule body is one of three forms.

**Linear sugar**:

```yaml
tasks:                                       # ordered; chained by default on_done edges
  - id: <state_id>
    run: <inline cmd>                        # OR uses: <path>
    if: <Liquid expr>                        # optional
    timeout: <duration>                      # optional
    on_fail: <state_id>                      # optional; default = nearest declared in rule, else __failure__
    directives:                              # optional
      <directive_name>: <state_id>           # short form
      <directive_name>:                      # long form
        target: <state_id>
        max_visits: <int>
    max_visits: <int>                        # optional
on_fail: <state_id>                          # rule-level default for any task without on_fail
```

**Canonical state machine**:

```yaml
start: <state_id>
states:
  <state_id>:
    run: <inline cmd>                        # OR uses: <path>
    if: <Liquid expr>
    timeout: <duration>
    on_done: <state_id>
    on_fail: <state_id>
    directives: { <name>: <state_id>, ... }
    max_visits: <int>
terminals:
  <state_id>: { outcome: <string> }
```

**Cleanup immediate-delete shorthand**:

```yaml
- when: <WhenClause>                         # required for shorthand
  # no tasks:, no states:, no terminals:
```

Top-level immediate-delete shorthand is recognized only inside `cleanup:`. Inside `rules:` or `on_failure:` an entry without a body is a schema error.

### 3.4 State body fields

Exactly one of `run:` / `uses:` per state. Every state is command-shape: a fresh subprocess per visit. There is no long-lived AI session shared across states or across visits of the same state.

| Field | Type | Notes |
|---|---|---|
| `run` | string (Liquid) | Inline shell command. Renders Liquid; spawned via `sh -c` (POSIX) / `cmd /C` (Windows). |
| `uses` | path | Path to `workflow/*.md`. Frontmatter `cli:` and `stall_seconds:` are honored; `session:` key is removed. |

### 3.5 Reserved state ids

The following identifiers are reserved as auto-injected terminals; operators may override by declaring them in `terminals:`.

| Id | Default `outcome` | Auto-targeted by |
|---|---|---|
| `__success__` | `success` | `directives.end`, last-task `on_done`, terminal-typed last task |
| `__failure__` | `failure` | `directives.fail`, default `on_fail` |
| `__no_action__` | `no_action` | `directives.skip` |
| `__cancelled__` | `cancelled` | Operator-targeted via `directives: { cancel: __cancelled__ }`. Daemon never auto-targets this terminal — admission revocation lets the in-flight cycle run to natural end per fr:01 §Queue-mode preemption. |

Identifiers beginning with `__` are otherwise reserved and rejected at validate time.

## 4. Sugar → canonical expansion

### 4.1 Pass 1 — implicit terminals

For each of `__success__`, `__failure__`, `__no_action__`, `__cancelled__`: if referenced anywhere in the rule and not declared in `terminals:`, inject default entry. Operator-declared id of the same name takes precedence.

### 4.2 Pass 2 — `tasks:` array → states + edges

Given `tasks: [t1, t2, ..., tN]`:

| Sugar input | Canonical output |
|---|---|
| `start:` (absent) | `start: t1` |
| `tasks[i].on_done` (absent) | `t[i+1]` if `i < N-1`; else `__success__` |
| `tasks[i].on_fail` (absent) | rule-level `on_fail`; else `__failure__` |
| `tasks[i].directives` (absent) | `{}` |
| `tasks[i]` is string `"foo"` | reference to `states.foo`; sugar-expansion error if absent |
| `tasks[i]` is map | register as state with given id |

### 4.3 Pass 3 — directive name defaults

Built-in directive names resolve to default targets when absent from a state's `directives:` map. Unregistered directive names received at runtime are `schema_drift` failures regardless.

| Directive name | Default edge target |
|---|---|
| `end` | `__success__` |
| `skip` | `__no_action__` |
| `retry` | self (current state id) |
| `fail` | `__failure__` |
| `cancel` | `__cancelled__` |

A state declaring `directives: {end: foo}` overrides only the `end` default; other built-ins still apply unless explicitly listed.

### 4.4 Pass 4 — validation

After expansion, the load fails (no swap, daemon refuses startup or hot-reload error) on any of:

1. Edge target id not in `states` ∪ `terminals`.
2. Two states share an id.
3. State has both `run:` and `uses:` (mutually exclusive).
4. State has neither `run:` nor `uses:` and is not in `terminals` (orphan body).
5. Reserved-prefix (`__*`) id in `states:` (only `terminals:` may declare them).
6. Cycle in the state graph where no node on the cycle has an explicit `max_visits` and no node has had one auto-injected by Pass 5.
7. Terminal `outcome` is empty string.
8. `start:` references a non-existent state or a terminal.

Validation accumulates errors (does not short-circuit) and reports them all at once.

### 4.5 Pass 5 — auto-`max_visits` injection

Run Tarjan SCC on the state graph (states only; terminals are sinks). For each non-trivial SCC where no member declares `max_visits`:

- Pick lexicographically-smallest state id in the SCC as the loop entry.
- Inject `max_visits = config.max_iterations` (from `roki.toml [engine].max_iterations`).

Trivial SCCs (single node with no self-edge) keep `max_visits = 1`.

## 5. Sentinel directive protocol

### 5.1 Path allocation

For each state invocation:

```
$ROKI_DIRECTIVE_PATH = <session_tempdir>/directives/<state_id>.<visit_n>.json
```

The daemon creates `<session_tempdir>/directives/` at cycle start and exposes the path to each subprocess via the env var. Subprocesses are responsible for atomic write: write to `<path>.tmp`, rename to `<path>`.

### 5.2 Payload schema

```json
{
  "directive": "<name>",                     // required
  "outcome": "<string>",                     // optional; only used by terminal-bound edges
  "<operator key>": "<value>"
}
```

| Field | Required | Use |
|---|---|---|
| `directive` | yes | Matched against `state.directives` map (with built-in defaults from §4.3). Unknown name = `schema_drift`. |
| `outcome` | no | If the resolved edge target is a terminal, override that terminal's default `outcome` for this cycle's terminal record. |
| any other key | no | Exposed downstream as `{{ tasks.<state_id>.directive.<key> }}`. |

### 5.3 Failure modes

| Sentinel state at exit | Daemon behavior |
|---|---|
| Path absent | exit==0 → `on_done`; exit!=0 → `on_fail` |
| Path present, valid JSON, `directive` missing | `unparseable` failure |
| Path present, invalid JSON | `unparseable` failure |
| Path present, `directive` not in state.directives ∪ built-ins | `schema_drift` failure |
| Path present, two atomic writes race (rare) | last-write wins; daemon reads after exit so race is operator-internal |

### 5.4 Stdout / stderr

Stdout and stderr remain pure work output (logs, AI text). The daemon never parses them for control flow. The per-iter capture file (`fr:09`) keeps both streams intact for forensics.

## 6. Inter-state data flow

The daemon retains all completed states' summaries within a cycle and exposes them as Liquid template variables and environment variables.

| Variable | Env | Scope |
|---|---|---|
| `{{ ticket.* }}` | unchanged | (unchanged from `fr:01 §Inter-phase data flow`) |
| `{{ repo.ghq }}` | `ROKI_REPO` | unchanged |
| `{{ cycle.id }}`, `{{ cycle.kind }}`, `{{ cycle.trigger }}` | unchanged | unchanged |
| `{{ cycle.iter }}` | `ROKI_CYCLE_ITER` | total state-visit count across the cycle |
| `{{ config.max_iterations }}` | `ROKI_CONFIG_MAX_ITERATIONS` | unchanged |
| `{{ state.id }}` | `ROKI_STATE_ID` | id of the state about to run |
| `{{ state.visits }}` | `ROKI_STATE_VISITS` | visits to this state so far including current |
| `{{ tasks.<id>.exit_code }}` | `ROKI_TASK_<ID>_EXIT_CODE` for top-level scalars | last completion of state `<id>` |
| `{{ tasks.<id>.duration_seconds }}` | `ROKI_TASK_<ID>_DURATION_SECONDS` | last completion |
| `{{ tasks.<id>.directive }}` | (Liquid only) | full sentinel JSON object from last completion (or null) |
| `{{ tasks.<id>.directive.<key> }}` | `ROKI_TASK_<ID>_DIRECTIVE_<KEY>` for top-level scalars | individual sentinel fields |
| `{{ tasks.<id>.terminal }}` | (Liquid only) | parsed claude/codex stream-json `result` event when applicable |
| `{{ failure.* }}` | `ROKI_FAILURE_*` | unchanged; failure cycles only |

`<ID>` env-name encoding: state id uppercased verbatim, `[A-Z0-9_]` only. State ids containing other characters are rejected at validate time.

`{{ pre.* }}` / `{{ post.* }}` / `{{ run.* }}` namespaces are removed.

## 7. Failure handling

### 7.1 State-local edges (operator-controlled)

| Trigger | Edge taken |
|---|---|
| Process exit 0, no sentinel | `on_done` |
| Process exit 0, sentinel directive in state.directives ∪ built-ins | resolved edge |
| Process exit ≠ 0 | `on_fail` |

State-local edges stay inside the same cycle. They are not "failures" from the daemon's perspective; they are operator-defined control flow.

### 7.2 Daemon-detected failure kinds

| Kind | Trigger |
|---|---|
| `process_crash` | Subprocess killed by signal without sentinel write |
| `unparseable` | Sentinel file present but JSON parse failed or `directive` missing |
| `schema_drift` | Sentinel `directive` value not in `state.directives` ∪ built-in defaults |
| `fs_poison` | Worktree / session-tempdir / sentinel-dir setup error before state launch |
| `stall` | State stall window exceeded; daemon SIGTERMed the subprocess |
| `recursion_bound` | `state.visits > state.max_visits` |
| `template_error` | Liquid render failure for `run:` cmd, `uses:` body, or `if:` condition |

`iter_exhausted` is removed (subsumed by `recursion_bound`).

`schema_drift` covers the previous "directive value outside legal set" case. The legal set is now per-state (`state.directives` ∪ built-in defaults), not phase-fixed.

### 7.3 `on_failure:` rules

Daemon-detected failure kinds route through top-level `on_failure:` first-match. The matched entry runs as a new cycle with `cycle.kind = "failure"` and `{{ failure.* }}` populated. Recursive failures (failure cycle itself fails) route to the escalation queue per slice 7.

`when.phase` matches the `state_id` that emitted the failure (renamed from "phase name" — same field, broader semantics).

### 7.4 Cleanup cycle

`cleanup` evaluation order remains: cleanup before rules. A matched `cleanup` entry runs as a `kind: cleanup` cycle, then the daemon deletes worktree + session_tempdir and evicts the ticket.

Immediate-delete shorthand: cleanup entry with no body (no `tasks:`, `states:`, or `terminals:`) deletes synchronously without a cycle. Same semantics as the previous schema.

## 8. Configuration

### 8.1 `roki.toml` changes

| Change | Detail |
|---|---|
| `[paths] workflow` default | `./WORKFLOW.toml` → `./WORKFLOW.yaml`. |
| `[default.ai.session]` block | Removed. Session-shape phases no longer exist. |
| `[default.ai.command].stall_seconds` | Renamed to `[default.ai].stall_seconds`. Single stall window applies to every state. |
| `[default.ai.command].cli` | Renamed to `[default.ai].cli`. Used as the default `cli` when a `workflow/*.md` frontmatter omits it. |
| `[engine].max_iterations`, `[engine].shutdown_window_seconds`, `[linear.*]` | Unchanged. |

No new daemon-config keys in this slice. `roki.toml` stays TOML.

### 8.2 Liquid templating

Liquid grammar unchanged. Inputs that accept Liquid:

- State `run:` strings.
- State `if:` strings (rendered, then truthy-tested per Liquid `if` rules).
- `workflow/*.md` body content (path-form), unchanged.

YAML block scalars (`|`, `>`) carry Liquid bodies without escape. Single-quoted strings are recommended for inline regexes (single backslash, not double).

## 9. CLI

### 9.1 New: `roki workflow graph`

```
roki workflow graph <FILE> [--rule <selector>] [--format ascii|dot] [--out <path>]
```

| Flag | Default | Behavior |
|---|---|---|
| `--rule` | (all rules) | Selector form `rules[<idx>]`, `cleanup[<idx>]`, `on_failure[<idx>]`. Omit to render every state machine in the file. |
| `--format` | `ascii` | `ascii` for terminal; `dot` for Graphviz. |
| `--out` | stdout | Write to file. |

Behavior: parses the file, runs the full sugar→canonical expansion + validation, then renders. A validation failure prints all errors and exits non-zero without rendering.

### 9.2 New: `roki workflow validate`

```
roki workflow validate <FILE>
```

Loads the file, runs sugar→canonical expansion + the 8 validation rules. Exits 0 on success (prints nothing), non-zero on failure (prints all accumulated errors with file:line markers). Intended for operator pre-flight before triggering daemon restart.

### 9.3 Existing CLI surface

`roki log`, `roki status`, etc. unchanged. The `--phase` flag on `roki log` is renamed to `--state` and accepts state ids; `--phase pre|run|post` no longer applies.

## 10. Hot reload

Restart-required. Hot reload of `WORKFLOW.yaml` is deferred to the same later slice that hot-reloads `WORKFLOW.toml` was deferred to in slice 7. The slice 7 `escalation_added` trigger 2 (workflow file hot-reload validation failure) remains unreachable in this slice, as before.

## 11. Doc updates

### 11.1 `docs/fr/01-engine-model.md`

Rewrite §Phase loop, §Directive schema, §Inter-phase data flow, §Iteration cap, §Failure handling, §Cleanup, §Cold start sections to reflect:

- Cycle is a state machine; no fixed pre/run/post phases.
- `cycle.iter` redefined as state-visit count (was: per-cycle iteration of the pre/run/post triple).
- Inter-state data flow table per §6.
- Failure-kind table per §7.2.
- Session-shape phase shape removed; the "session-shape phase shares one long-lived subprocess across all pre / post invocations" sentence and surrounding paragraph are deleted.
- Removed: phase-loop diagram (lines 46-56), pre/run/post phase optionality table (lines 58-65), pre/post directive set table (line 81-84), iter_exhausted row.

### 11.2 `docs/fr/02-configuration.md`

Replace the `WORKFLOW.toml` schema section with the YAML schema per §3. Update phase-specification subsection to "state body specification" per §3.4.

### 11.3 `docs/fr/04-phase-execution.md`

Rename to `docs/fr/04-state-execution.md`. Redirect from old path. Update §Input channels: stdin and argv unchanged; remove "stdout last-JSON parse" wording; add §Sentinel channel per §5.

### 11.4 `docs/fr/06-failure-handling.md`

Update failure-kind table per §7.2. Remove `iter_exhausted` row. Add `recursion_bound` row. Update `[[on_failure]]` matcher discussion to `on_failure:` and the `when.phase` semantics per §7.3. Slice 7 § Escalation queue text remains accurate.

### 11.5 `docs/fr/08-observability-logs.md`

Replace `phase` field on cycle-engine event payloads with `state_id` (string) and `visit_n` (int). `cycle_completed` event gains `terminal_id` and keeps `outcome`.

### 11.6 `docs/reference/config.md`

Replace the `WORKFLOW.toml` schema section with the canonical `WORKFLOW.yaml` schema covering: `admission` (assignee + repos), `WhenClause`, `RuleEntry`, sugar `tasks:` form, canonical `states:` + `terminals:` form, state body fields (`run` / `uses` / `if` / `timeout` / `on_done` / `on_fail` / `directives` / `max_visits`), reserved terminal ids, built-in directive defaults, path resolution, and validation rules. Update `roki.toml` schema for `[default.ai.command]` → `[default.ai]` rename and `[default.ai.session]` removal per §8.1. Authoritative — spec §3 narrates, ref:config defines.

### 11.7 `docs/reference/log-events.md`

Update `phase` → `state_id` + `visit_n` columns on cycle-engine event rows. Add `recursion_bound` row to failure-kind enum. Drop `iter_exhausted` row.

### 11.8 `docs/reference/cli.md`

Add `roki workflow graph` and `roki workflow validate` rows. Rename `roki log --phase` to `roki log --state`.

### 11.9 `docs/reference/frontmatter.md`

Drop the `session:` key row from the `workflow/*.md` frontmatter table. `cli:` and `stall_seconds:` rows remain. Update prose noting every state is command-shape.

### 11.10 `docs/examples/`

- Delete `WORKFLOW.minimal.toml`, `WORKFLOW.annotated.toml`, `roki.minimal.toml` reference to `WORKFLOW.toml`, `roki.annotated.toml` ditto.
- Add `WORKFLOW.minimal.yaml`, `WORKFLOW.annotated.yaml`, `repos/bar.yaml`.
- `roki.minimal.toml` / `roki.annotated.toml` set `[paths] workflow = "./WORKFLOW.yaml"`.
- `workflow-impl.md`, `workflow-judge.md`, `workflow-verdict.md`: drop `session:` frontmatter key from any file that carries it; references to "pre/run/post" in prose update to "states".

## 12. Tests

### 12.1 New e2e fixtures

- `slice8-yaml-load` — minimal YAML loads, daemon emits `daemon_ready`, no validation errors.
- `slice8-sugar-linear` — three-task `tasks:` sugar expands to chain; cycle runs all three then `__success__`.
- `slice8-sugar-retry` — `tasks: [a, b]` with `b.directives: { retry: a }`. `b` emits `retry` twice, then `end`. Verifies `max_visits` auto-injection on `a`.
- `slice8-canonical-branch` — explicit state machine with `directives: { skip: __no_action__ }`. Sentinel `{"directive":"skip"}` → cycle terminates `outcome: no_action`.
- `slice8-sentinel-absent` — state exits 0 without writing sentinel → `on_done` taken.
- `slice8-sentinel-unparseable` — state writes invalid JSON → `unparseable` failure → routed via `on_failure: when.kind: unparseable`.
- `slice8-state-on-fail` — state exits 1 → state's `on_fail` edge taken; cycle does not failure-route.
- `slice8-recursion-bound` — explicit `max_visits: 2` on a self-loop state; third visit emits `recursion_bound`; routes to escalation queue (per slice 7).
- `slice8-validate-orphan-target` — invalid YAML (edge to undeclared state) → daemon refuses startup with all errors listed.
- `slice8-cleanup-immediate-delete` — body-less cleanup entry deletes synchronously, no cycle.
- `slice8-per-repo-override` — `[[admission.repos]] workflow:` points to `repos/bar.yaml`; that file's `rules:` execute.
- `slice8-workflow-graph-cli` — `roki workflow graph` renders ASCII for the loaded fixtures.

### 12.2 Updated existing e2e

All slices 1-7 fixtures convert from `WORKFLOW.toml` to `WORKFLOW.yaml`. Test-harness helpers gain a YAML emitter; the TOML emitter is deleted. Existing assertions (event ordering, exit codes, structured-log shape) unchanged except:

- `phase` field in event payloads → `state_id`.
- `iter_exhausted` failure-kind assertions → `recursion_bound`.
- Pre/run/post-specific assertions (e.g. "pre runs first") → state-id-specific.

### 12.3 Unit

- `workflow::sugar`: each of passes 1-5 with handcrafted inputs; deterministic SCC entry-pick across YAML round-trips.
- `workflow::validate`: each of the 8 validation rules triggers, multiple errors accumulate.
- `engine::sentinel`: atomic-write protocol, absent file, malformed JSON, missing `directive` field.
- `engine::data_flow`: env-var encoding rules for state ids; ROKI_TASK_<ID>_*.

## 13. Implementation sequence

1. **Types + parser**: `workflow::canonical`, `workflow::parse`. Round-trip serde test on canonical YAML.
2. **Sugar expansion**: `workflow::sugar` passes 1-5. Snapshot tests on small fixtures.
3. **Validator**: `workflow::validate` with the 8 rules. Multi-error accumulation.
4. **Sentinel module**: `engine::sentinel` with atomic write + read. Unit tests.
5. **State runtime**: `engine::state_runtime` runs one state. Mock-runner tests for each (exit_code, sentinel) combination.
6. **Cycle rewrite**: `engine::cycle` consumes `StateMachine`. Integration tests with mock runner across full sample machines.
7. **Failure routing**: connect failure kinds to top-level `on_failure:` and to escalation queue.
8. **CLI**: `roki workflow graph` (ASCII + DOT) and `roki workflow validate` subcommands.
9. **Reference rewrite**: `docs/reference/config.md` (§11.6) and `docs/reference/frontmatter.md` (§11.9). Authoritative schema lands here.
10. **FR doc rewrite**: §11.1 - §11.5, §11.7, §11.8. Run `kusara validate` after each.
11. **Examples**: write new YAML examples; delete old TOML examples.
12. **Migrate test fixtures**: slice 1-7 e2e to YAML.
13. **New e2e**: slice 8 fixtures (§12.1).

Each step is a separate commit. Steps 1-5 can land as a "config + sentinel" sub-feature ahead of the engine cutover (steps 6-8). Existing slice 1-7 tests stay green only after step 12.

## 14. Boundaries / non-goals

- **No TOML compatibility shim.** Daemon refuses to load `WORKFLOW.toml`.
- **No Liquid grammar change.** No `${{ }}` introduction.
- **No new template variables beyond §6.** No declared `outputs:` block.
- **No DAG / parallel states.** State machine is a single-token-of-control machine; only one state runs at a time.
- **No long-lived AI session.** Every state spawns a fresh subprocess. No cross-state subprocess sharing, no cross-visit subprocess sharing within a state. Operators relying on Claude / Codex conversational continuity across phases must drive it inside a single state's process (e.g. one stream-json invocation that holds the conversation).
- **No cross-cycle state.** Inter-state data flow is per-cycle. Cross-cycle scratch goes through `roki log`.
- **No engine-managed retry budget per state.** `max_visits` caps loops; operators control everything else.
- **No persisted resume.** Daemon stop aborts in-flight cycles; cold-start re-enumerates per slice 6.
- **No HTTP API surface change.** Slice 7's escalation queue endpoints remain deferred.
- **No new event kinds.** Existing `cycle_started` / `cycle_completed` / `escalation_added` / `failure_unhandled` carry the new payload shape via field rename, not new event names.
- **No daemon-fired pre-emption.** Admission revocation / ticket eviction during an in-flight cycle does not force termination; cycle runs to natural end per fr:01 §Queue-mode preemption. `__cancelled__` is operator-target only.

## 15. Documented divergence

State-id encoding for env vars (`ROKI_TASK_<ID>_*`) tightens the previous `ROKI_PRE_<KEY>` / `ROKI_POST_<KEY>` rule from "skip non-`[A-Z0-9_]` keys with info log" to "reject non-`[A-Z0-9_]` state ids at validate time". Operators control state ids; payload field keys retain the skip-with-log behavior under `tasks.<id>.directive.<key>` env exposure.
