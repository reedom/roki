---
refs:
  id: fr:01-engine-model
  kind: fr
  title: "Rule and Cycle Engine"
  spec: roki-engine-iteration-loop
  related:
    - fr:02-configuration
    - fr:03-linear-admission
    - fr:07-recovery
    - fr:05-worktree-and-session
    - fr:04-state-execution
    - fr:08-observability-logs
    - fr:06-failure-handling
    - fr:09-log-access-cli
---

# FR 01: Rule and Cycle Engine

> The config-driven heart of the daemon. Each Linear webhook diff selects one entry from operator-authored `cleanup:` / `rules:` / `on_failure:` lists; the matched entry runs as a **cycle** that drives a state machine until landing in a terminal or hitting a daemon-detected failure. The daemon reads a per-state sentinel-file directive and resolves the next edge accordingly; state contents are operator-authored.

## Purpose

All workflow knowledge lives in the operator's `WORKFLOW.yaml` + `workflow/*.md`. The daemon is a thin event-driven engine: a runtime diff (webhook delivery, polling fallback, or refresh nudge) or cold-start enumeration → admission → first-match dispatch → state-machine cycle → terminal or failure routing.

## User-visible Behavior

### Cycle kinds

A cycle is the unit of work the daemon spawns when an operator-authored entry matches.

| Kind | Triggered by | Auto-cleanup at end? | `cycle.kind` value |
|---|---|---|---|
| Rule | `rules:` first-match | no | `rule` |
| Cleanup | `cleanup:` first-match | yes — daemon deletes worktree + session_tempdir and evicts the ticket | `cleanup` |
| Failure | A daemon-detected internal failure during another cycle, with `on_failure:` first-match | no | `failure` |

Evaluation order on each diff: cleanup before rules. Failure cycles only spawn when an in-flight cycle hits an internal failure (see §Failure handling).

A `cleanup:` entry with no body (no `tasks:` / `states:` / `terminals:`) is shorthand for "delete immediately": the daemon performs the cleanup directly without spawning a cycle. Use this for unconditional teardown rules where no Linear ceremony is needed.

### State machine cycle

Each cycle drives a state machine declared in the matched entry. The driver:

```
state_id ← state_machine.start
loop:
  if state_id ∈ terminals: emit cycle_completed(terminal_id, outcome); break
  visits[state_id] += 1; cycle.iter += 1
  if visits[state_id] > state.max_visits: emit recursion_bound failure; break
  if state.if Liquid evaluates falsy: state_id ← state.on_done; continue
  spawn subprocess for state (run: cmd or uses: workflow/*.md)
  wait for exit (stall window per state.timeout)
  read sentinel file at $ROKI_DIRECTIVE_PATH
  match (exit_code, sentinel):
    (0, absent)         → state_id ← state.on_done
    (0, present)        → look up directive name in state.directives ∪ built-in defaults;
                          unknown name → schema_drift failure; otherwise state_id ← edge target
    (≠0, _)             → state_id ← state.on_fail
    (signal, _)         → process_crash failure; break
```

Every state is command-shape: each visit spawns a fresh subprocess. There is no long-lived AI session shared across states or visits.

Built-in directive name defaults (overridable per state):

| Directive name | Default edge target |
|---|---|
| `end` | `__success__` |
| `skip` | `__no_action__` |
| `retry` | self (current state id) |
| `fail` | `__failure__` |
| `cancel` | `__cancelled__` |

Reserved terminal ids auto-injected when referenced and not declared: `__success__` (outcome=success), `__failure__` (outcome=failure), `__no_action__` (outcome=no_action), `__cancelled__` (outcome=cancelled — operator-target only; daemon never auto-fires).

### Directive schema

Each state may write a sentinel file at `$ROKI_DIRECTIVE_PATH` before exit. The daemon reads the file after the subprocess exits.

```json
{
  "directive": "<name>",
  "outcome": "<operator string, optional>",
  "<operator field>": "..."
}
```

Field semantics:

- `directive` (required): name matched against `state.directives` ∪ built-in defaults. Unknown name = `schema_drift` failure (§Failure handling). Missing field or invalid JSON = `unparseable` failure.
- `outcome` (optional): when the resolved edge targets a terminal, this string overrides that terminal's declared outcome for the cycle's terminal record. Otherwise advisory.
- Any additional operator-defined fields are exposed to subsequent states as `{{ tasks.<state_id>.directive.<key> }}`.

The atomic-write contract is the operator's: write to `<path>.tmp`, rename to `<path>`. Stdout and stderr are pure work output (logs, AI text); the daemon does not parse them for control flow.

### Inter-state data flow

The daemon retains every completed state's summary within a cycle and exposes them to subsequent states as Liquid template variables and environment variables.

| Variable | Env | Scope |
|---|---|---|
| `{{ ticket.id }}` | `ROKI_TICKET_ID` | Linear identifier |
| `{{ ticket.title }}`, `{{ ticket.body }}`, `{{ ticket.labels }}`, `{{ ticket.assignee }}`, `{{ ticket.status }}` | (inline only) | Current Linear state |
| `{{ repo.ghq }}` | `ROKI_REPO` | Admission-resolved repo |
| `{{ cycle.id }}` | `ROKI_CYCLE_ID` | UUID |
| `{{ cycle.kind }}` | `ROKI_CYCLE_KIND` | `rule` / `cleanup` / `failure` |
| `{{ cycle.trigger }}` | `ROKI_CYCLE_TRIGGER` | `runtime` (any runtime-detected diff: webhook delivery, polling fallback, or refresh nudge) / `cold_start` (daemon startup enumeration) |
| `{{ cycle.iter }}` | `ROKI_CYCLE_ITER` | int — total state-visit count across the cycle |
| `{{ config.max_iterations }}` | `ROKI_CONFIG_MAX_ITERATIONS` | int — engine default cap from `roki.toml [engine].max_iterations` |
| `{{ state.id }}` | `ROKI_STATE_ID` | id of the state about to run |
| `{{ state.visits }}` | `ROKI_STATE_VISITS` | visits to this state so far including current |
| `{{ tasks.<id>.exit_code }}` | `ROKI_TASK_<ID>_EXIT_CODE` | last completion of state `<id>` |
| `{{ tasks.<id>.duration_seconds }}` | `ROKI_TASK_<ID>_DURATION_SECONDS` | last completion |
| `{{ tasks.<id>.directive }}` | (inline only) | full sentinel JSON from last completion |
| `{{ tasks.<id>.directive.<key> }}` | `ROKI_TASK_<ID>_DIRECTIVE_<KEY>` (top-level scalars) | individual sentinel fields |
| `{{ tasks.<id>.terminal }}` | (inline only) | parsed claude/codex stream-json `result` event when applicable |
| `{{ failure.kind }}`, `{{ failure.failed_cycle_id }}`, `{{ failure.state_id }}`, `{{ failure.visit_n }}`, `{{ failure.exit_code }}`, `{{ failure.error_text }}` | `ROKI_FAILURE_*` | Failure cycles only |

The daemon exposes these variables to every subprocess on three fixed channels (see [04-state-execution §Input channels](04-state-execution.md)):

- **argv** — the cli line is itself a Liquid template; operators reference any field with `{{ ... }}`.
- **environment variables** — scalar-only `ROKI_*` entries per the table above. Complex objects are never flattened into env.
- **stdin** — the rendered state body (`uses:` body or inline `run:` for cli-line invocations that pipe stdin in). Inline `run:` shell commands receive nothing on stdin by default.

States that need a complex earlier-state object not present in the table read it through `roki log --cycle <id> --state <state_id> --stream stdout` ([09-log-access-cli](09-log-access-cli.md)).

**Env-var naming rule** for `ROKI_TASK_<ID>_*`: state ids must match `[A-Za-z][A-Za-z0-9_]*` (validated at config load), uppercased verbatim into the env-var prefix. For `ROKI_TASK_<ID>_DIRECTIVE_<KEY>`, only top-level scalar fields (string, number, bool) are exported; each operator-defined key `<key>` becomes `<KEY>` uppercased verbatim, with non-`[A-Z0-9_]` characters causing the entry to be skipped with an info-level log naming the offending key.

### Recursion bound

`max_visits` caps each state's visit count. Pass 5 of sugar expansion auto-injects `roki.toml [engine].max_iterations` on SCC entry nodes that declare none.

When `state.visits > state.max_visits`, the daemon emits a `recursion_bound` failure and the cycle aborts before the next visit. The failure routes through `on_failure:` first-match. A failure cycle that itself triggers `recursion_bound` enters the escalation queue ([06-failure-handling](06-failure-handling.md)).

Operators preempt cooperatively by inspecting `{{ state.visits }}` / `{{ state.max_visits }}` / `{{ cycle.iter }}` / `{{ config.max_iterations }}` in their state body:

```liquid
{% if state.visits >= state.max_visits | minus: 1 %}
This is your final visit. Output a final summary then write `{"directive":"end"}`
to $ROKI_DIRECTIVE_PATH.
{% endif %}
```

There is no daemon-side retry budget for failed exits. Operators encode retry policy by inspecting `{{ tasks.<id>.exit_code }}` / `{{ tasks.<id>.terminal.* }}` and writing `{"directive":"retry"}` from a downstream state. Backoff is the operator's responsibility.

### Queue-mode preemption

A new webhook arriving while the same ticket has an in-flight cycle:

1. Updates the in-memory diff cache to the new state immediately.
2. Defers rule re-evaluation until the in-flight cycle terminates.
3. After the cycle terminates, the daemon evaluates lists against the latest cached state. The retained webhooks are not replayed individually; only the final state matters.

The single exception is admission-filter failure mid-cycle (assignee revoked, repo allowlist match lost): the in-flight cycle still runs to its natural end. After it terminates, the daemon evicts the cache entry but **retains** worktree + session_tempdir for re-admission reuse; reclamation is by `[[cleanup]]` cycle on re-admission or by cold-start orphan reconcile when the ticket is no longer enumerable. Operators that want forced-termination behavior on a Linear status change author a `[[cleanup]]` entry whose run phase issues a SIGTERM-equivalent action against whatever subprocess they care about (or simply omits all phases for immediate delete).

### Stall detection

Each subprocess has a stall window. Default lives in `roki.toml [default.ai].stall_seconds`. Per-file override in `workflow/*.md` frontmatter. Per-state override via `state.timeout`.

Canonical defaults and validation rules live in [`docs/reference/config.md`](../reference/config.md).

If stdout is silent for the configured window, the daemon SIGTERMs the subprocess and routes the cycle through `on_failure: when.kind: stall`. The discarded stdout/stderr remain in the per-visit capture for forensics.

### Failure handling

Daemon-detected internal failures during a cycle:

| Kind | Trigger |
|---|---|
| `process_crash` | Subprocess killed by signal without a sentinel write |
| `unparseable` | Sentinel file present but JSON parse failed or `directive` field missing |
| `schema_drift` | Sentinel `directive` value not in `state.directives` ∪ built-in defaults |
| `fs_poison` | Filesystem error creating or recovering worktree / session-tempdir / sentinel-dir before state launch (permission denied, disk full, symlink escape, missing parent path, etc.). Cleanup-time fs errors are not routed here — they go to the escalation queue ([06-failure-handling §Escalation queue](06-failure-handling.md)) |
| `stall` | Stall window exceeded; daemon SIGTERMed the subprocess |
| `recursion_bound` | State visited more than `state.max_visits` times |
| `template_error` | Liquid render failure when preparing `run:` cmd, `uses:` body, or `if:` condition |

Sequence:

1. The originating cycle is marked aborted; its current visit is recorded with the failure metadata (`failure.state_id`, `failure.visit_n`).
2. The daemon evaluates `on_failure:` first-match against `failure.kind` (and optionally `failure.phase`, which matches the state id that emitted the failure).
3. On match: spawn a new cycle with `cycle.kind = "failure"`; populate `{{ failure.* }}` and the `ROKI_FAILURE_*` env vars. The handler can read the failed cycle's logs via `roki log --cycle <failed_cycle_id> --state <state_id> ...`.
4. On no match: emit a `failure_unhandled` structured event ([06-failure-handling §Failure-handler cycle](06-failure-handling.md)) carrying the failure metadata. The escalation queue is **not** touched. Worktree is retained for forensics.

A failure cycle that itself fails does **not** chain into another failure cycle. Such recursive failures land in the escalation queue ([06-failure-handling §Escalation queue](06-failure-handling.md)) instead, which bounds the recovery loop to one extra cycle per original failure.

### Cleanup

Cleanup priority is enforced before rule evaluation: the daemon walks `cleanup:` first-match before `rules:`. A matched cleanup entry runs as a `kind: cleanup` cycle (full state-machine semantics), then the daemon deletes the ticket's worktree + session_tempdir and evicts it from the cache.

A cleanup entry with no body (no `tasks:` / `states:` / `terminals:`) is shorthand for "delete immediately, no cycle starts". The daemon performs the cleanup synchronously and emits a single structured `cycle_completed` event with `cycle.kind = cleanup`, `terminal_id = __success__`, and zero iterations. Operators that want a guarded teardown author a non-shorthand cleanup entry whose state body performs the desired action.

### Cold start

On daemon process start, the engine runs the same evaluation flow but with `cycle.trigger = "cold_start"`. See [07-recovery §Cold start](07-recovery.md) for the full enumeration / reconcile flow. Operators that need to suppress duplicate Linear comments on cold-start re-runs check `{% if cycle.trigger == "cold_start" %}` in their state body.

## Capabilities

- **Generic dispatch**: `cleanup:` / `rules:` / `on_failure:` are the only three lists the daemon evaluates. Each is first-match. Operators express any workflow within them.
- **State machine cycle**: each entry declares a state machine. Sugar `tasks:` form (linear chain) or canonical `start:` / `states:` / `terminals:`. The daemon drives state visits to a terminal or a daemon-detected failure.
- **Structured directive contract**: per-state sentinel file at `$ROKI_DIRECTIVE_PATH`. The daemon parses only the JSON in that file; stdout / stderr are pure work output.
- **Inter-state data flow**: `{{ tasks.<id>.* }}` exposes every completed state's exit_code, duration, sentinel directive (incl. operator-defined fields), and parsed terminal event.
- **Recursion bound**: `state.max_visits` (operator-declared or auto-injected on SCC entry nodes) is a hard daemon-enforced boundary. Operators can preempt cooperatively by inspecting `{{ state.visits }}` / `{{ state.max_visits }}` in the state body. When the cap trips, the daemon refuses the next visit and routes through `on_failure: when.kind: recursion_bound`.
- **Operator-driven retry/backoff**: there is no daemon retry budget. Operators emit `{"directive":"retry"}` (or any operator-defined directive bound to a self-loop) to re-enter, with whatever delay the operator implements inside the next state body.
- **Queue-mode webhook serialization**: at most one cycle per ticket at a time. New webhooks update the diff cache and re-evaluate after the in-flight cycle ends.
- **Failure handler cycle as first-class**: `on_failure:` runs as a cycle, with `{{ failure.* }}` and forensics access via `roki log --cycle <failed_id>`. Failures inside a failure cycle fall back to the default escalation behavior.

## Boundaries

- **No daemon-managed retry budget**: the daemon does not count state non-clean exits and does not enforce backoff.
- **No daemon-side Linear writes**: the daemon never writes Linear directly. Linear feedback (labels, comments) is entirely operator-driven from inside state subprocesses.
- **No long-lived AI session**: every state visit spawns a fresh subprocess. No cross-state subprocess sharing, no cross-visit subprocess sharing within a state. Operators relying on Claude / Codex conversational continuity drive it inside a single state's process (e.g. one stream-json invocation that holds the conversation).
- **No state-id catalog**: operators name their states however they like; the daemon does not reserve any id beyond the `__*` reserved-prefix terminals.
- **No daemon-side artifact validation**: operators encode whatever artifact checks they want inside their state body.
- **No operator-installed Linear MCP requirement**: the daemon does not assume operators have any specific MCP installed. If a workflow needs to write Linear, the operator includes that capability in the cli line of the relevant state.
- **No tracker-terminal preemption event**: the daemon does not synthesize a special preempt event when Linear state moves to `done` / `canceled`. Operators express the desired behavior with a `cleanup:` entry.

## Traceability

- **Roadmap**: `roadmap.md` > Boundary Strategy.
- **Requirements**: pending — the spec rebuild will introduce IDs covering rule-list evaluation order, the directive schema, the iteration cap, and failure handling. Until then this FR carries the contract directly.
- **Design**: pending — the new spec set's design.md files will reference back to this FR.
- **Related FR**: [02-configuration](02-configuration.md), [03-linear-admission](03-linear-admission.md), [07-recovery](07-recovery.md), [05-worktree-and-session](05-worktree-and-session.md), [04-phase-execution](04-state-execution.md), [08-observability-logs](08-observability-logs.md), [06-failure-handling](06-failure-handling.md), [09-log-access-cli](09-log-access-cli.md).
