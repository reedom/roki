---
refs:
  id: fr:01-engine-model
  kind: fr
  title: "Rule and Cycle Engine"
  spec: roki-mvp
  related:
    - fr:02-configuration
    - fr:03-linear-admission
    - fr:07-recovery
    - fr:05-worktree-and-session
    - fr:04-phase-execution
    - fr:08-observability-logs
    - fr:06-failure-handling
    - fr:09-log-access-cli
---

# FR 01: Rule and Cycle Engine

> The config-driven heart of the daemon. Each Linear webhook diff selects one entry from operator-authored `[[cleanup]]` / `[[rule]]` / `[[on_failure]]` lists; the matched entry runs as a **cycle** of three phases (pre / run / post) that can loop until a terminal directive or a daemon-enforced cap. The daemon parses a structured directive from each phase's stdout and routes accordingly; phase contents are operator-authored.

## Purpose

All workflow knowledge lives in the operator's WORKFLOW.toml + workflow/*.md. The daemon is a thin event-driven engine: webhook → admission → diff → first-match dispatch → cycle of subprocess phases → directive-driven loop → termination.

## User-visible Behavior

### Cycle kinds

A cycle is the unit of work the daemon spawns when an operator-authored entry matches.

| Kind | Triggered by | Auto-cleanup at end? | `cycle.kind` value |
|---|---|---|---|
| Rule | `[[rule]]` first-match | no | `rule` |
| Cleanup | `[[cleanup]]` first-match | yes — daemon deletes worktree + session_tempdir and evicts the ticket | `cleanup` |
| Failure | A daemon-detected internal failure during another cycle, with `[[on_failure]]` first-match | no | `failure` |

Evaluation order on each diff: cleanup before rule. Failure cycles only spawn when an in-flight cycle hits an internal failure (see §Failure handling).

A `[[cleanup]]` entry with all three phases (pre / run / post) omitted is shorthand for "delete immediately": the daemon performs the cleanup directly without spawning a cycle. Use this for unconditional teardown rules where no Linear ceremony is needed.

### Phase loop

Each cycle runs through a phase loop:

```
cycle start
  ↓
[iteration N]
  pre → response.directive ∈ {run, end}
    end → cycle terminates (run / post are skipped)
    run → run → post → response.directive ∈ {pre, run, end}
      pre  → goto [iteration N+1] pre
      run  → goto [iteration N+1] run (skips pre)
      end  → cycle terminates
```

Phase optionality:

- **run is required** for every `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` entry that spawns a cycle. Schema validation rejects an entry that has any phase declared but lacks `run`.
- **pre is optional** — when omitted the daemon synthesizes `directive: "run"` and proceeds.
- **post is optional** — when omitted the daemon synthesizes `directive: "end"` and the cycle terminates after the run phase.
- **`[[cleanup]]` shorthand**: an entry with **all three** phases omitted means "delete immediately, no cycle starts" (see §Cleanup). This is the only case in which `run` may be absent.

Pre and post are subprocesses, just like run. They may be long-lived AI sessions (when declared `session = "session"`) reused across pre/post invocations within the same cycle, or one-shot commands. The choice is per-phase via the workflow/*.md frontmatter or the inline `pre.cmd` / `pre.prompt` / `post.cmd` / `post.prompt` form. See [02-configuration §Phase specification](02-configuration.md).

### Directive schema

Each pre and post invocation emits exactly one terminal JSON object on its stdout. The daemon parses the **last** JSON object on the phase's stdout per invocation; earlier JSON output is treated as advisory and stored only in the per-iter capture file ([09-log-access-cli](09-log-access-cli.md)).

```json
{
  "directive": "run" | "end" | "pre",
  "outcome": "<operator string, optional>",
  "repo": "<github.com/foo/bar, optional>",
  "<operator field>": "..."
}
```

Field semantics:

- `directive` (required): the next phase to run, or `end` to terminate the cycle. Legal sets are phase-specific:
  - pre: `run` | `end`.
  - post: `pre` | `run` | `end`.
  - An out-of-set value is a `schema_drift` failure (§Failure handling).
- `outcome` (optional): a free-form operator string used as a structured-log discriminator and TUI label. Conventional values include `success`, `failure`, `needs_operator`, `needs_split`, `cancelled`, `no_action`. The daemon does not interpret it.
- `repo` (optional, pre only): the resolved repo for this ticket, used by the daemon to create the worktree on the first `directive: "run"`. The daemon ignores it on subsequent iterations once the worktree exists.
- Any additional operator-defined fields are exposed to the next phase as `{{ pre.* }}` (after a pre invocation) and `{{ post.* }}` (after a post invocation).

### Inter-phase data flow

The daemon retains the **last completed iteration's** payloads and exposes them to subsequent phases as Liquid template variables and environment variables. Earlier iterations are not retained in scratch state; their captures still live on disk and are accessible via [09-log-access-cli](09-log-access-cli.md).

| Variable | Env | Scope |
|---|---|---|
| `{{ ticket.id }}` | `ROKI_TICKET_ID` | Linear identifier |
| `{{ ticket.title }}`, `{{ ticket.body }}`, `{{ ticket.labels }}`, `{{ ticket.assignee }}`, `{{ ticket.status }}` | (inline only) | Current Linear state |
| `{{ repo.ghq }}` | `ROKI_REPO` | Admission-resolved repo |
| `{{ cycle.id }}` | `ROKI_CYCLE_ID` | UUID |
| `{{ cycle.kind }}` | `ROKI_CYCLE_KIND` | `rule` / `cleanup` / `failure` |
| `{{ cycle.trigger }}` | `ROKI_CYCLE_TRIGGER` | `webhook` / `cold_start` (extensible) |
| `{{ cycle.iter }}` | `ROKI_CYCLE_ITER` | int (1-indexed) |
| `{{ config.max_iterations }}` | `ROKI_CONFIG_MAX_ITERATIONS` | int — engine cap from `roki.toml [engine].max_iterations` |
| `{{ pre.* }}` | `ROKI_PRE_<FIELD>` for scalars | Most recent pre response payload |
| `{{ post.* }}` | `ROKI_POST_<FIELD>` for scalars | Most recent post response payload (visible from iteration N+1 onward) |
| `{{ run.exit_code }}` | `ROKI_RUN_EXIT_CODE` | int |
| `{{ run.duration_seconds }}` | `ROKI_RUN_DURATION_SECONDS` | int |
| `{{ run.terminal.* }}` | (inline only) | Parsed claude/codex stream-json `result` event when applicable; null for shell commands |
| `{{ failure.kind }}`, `{{ failure.failed_cycle_id }}`, `{{ failure.phase }}`, `{{ failure.iter }}`, `{{ failure.exit_code }}`, `{{ failure.error_text }}` | `ROKI_FAILURE_*` | Failure cycles only |

The daemon exposes these variables to every subprocess on three fixed channels (see [04-phase-execution §Input channels](04-phase-execution.md)):

- **argv** — the cli line is itself a Liquid template; operators reference any field with `{{ ... }}`.
- **environment variables** — scalar-only `ROKI_*` entries per the table above. Complex objects are never flattened into env.
- **stdin** — the rendered phase body (`path` body or inline `prompt` string). Inline `cmd` phases write nothing to stdin.

Phases that need a complex prev-iter object not present in the table (e.g. an older iteration's parsed response) read it through `roki log --stream response --iter -N` ([09-log-access-cli](09-log-access-cli.md)).

### Iteration cap and cooperative termination

`roki.toml [engine].max_iterations` (default 10) caps a cycle's iteration count. The cap is enforced as a hard boundary on starting a new iteration:

1. When a post directive `pre` or `run` arrives and `cycle.iter == max_iterations`, the daemon does **not** start iteration N+1. For session-shape phases the daemon closes stdin and waits for the AI subprocess to exit; the session stall window applies. For command-shape phases the cycle is already at process exit, so no further action is needed.
2. The cycle is marked `iter_exhausted` failure and routes through `[[on_failure]] when.kind = "iter_exhausted"`.
3. If the session subprocess does not exit within the stall window after stdin close, the daemon SIGTERMs it; the failure kind stays `iter_exhausted`.

Operators encode cooperative wrap-up entirely in their templates. `{{ cycle.iter }}` and `{{ config.max_iterations }}` are exposed (see §Inter-phase data flow) so a pre / post body can preempt the cap:

```liquid
{% if cycle.iter >= config.max_iterations | minus: 1 %}
This is your final iteration. Output a final summary then `{"directive":"end"}`.
{% endif %}
```

Daemon never writes a magic-bytes signal to stdin; stdin is reserved for rendered phase bodies (see [04-phase-execution §Input channels](04-phase-execution.md)).

There is no daemon-side retry budget for failed runs. Operators encode retry policy by inspecting `{{ run.exit_code }}` / `{{ run.terminal.* }}` in their post template and returning `directive: "run"` (re-run the same phase) or `directive: "pre"` (restart from pre with a different payload). Backoff is the operator's responsibility — a post template can sleep before returning, or the operator can sleep inside the next pre.

### Queue-mode preemption

A new webhook arriving while the same ticket has an in-flight cycle:

1. Updates the in-memory diff cache to the new state immediately.
2. Defers rule re-evaluation until the in-flight cycle terminates.
3. After the cycle terminates, the daemon evaluates lists against the latest cached state. The retained webhooks are not replayed individually; only the final state matters.

The single exception is admission-filter failure mid-cycle (assignee revoked, repo allowlist match lost): the in-flight cycle still runs to its natural end. After it terminates, the daemon evicts the ticket and deletes worktree + session_tempdir as orphan cleanup. Operators that want forced-termination behavior on a Linear status change author a `[[cleanup]]` entry whose run phase issues a SIGTERM-equivalent action against whatever subprocess they care about (or simply omits all phases for immediate delete).

### Stall detection

Each subprocess has a stall window:

- `roki.toml [default.ai.session].stall_seconds` (default `600`) for session-mode phases.
- `roki.toml [default.ai.command].stall_seconds` (default `300`) for command-mode phases.
- The workflow/*.md frontmatter may override on a per-file basis.

If stdout is silent for the configured window, the daemon SIGTERMs the subprocess and routes the cycle through `[[on_failure]] when.kind = "stall"`. The discarded stdout/stderr remain in the iter capture for forensics.

### Failure handling

Daemon-detected internal failures during a cycle:

| Kind | Trigger |
|---|---|
| `process_crash` | Subprocess exited via signal or non-zero exit code without a parseable terminal response |
| `unparseable` | Last JSON object on stdout failed to parse, or the `directive` field is missing |
| `schema_drift` | `directive` value is outside the legal set for the current phase |
| `repo_mismatch` | A pre response's `repo` field does not match the admission-resolved repo for the ticket ([05-worktree-and-session](05-worktree-and-session.md)) |
| `stall` | Stall window exceeded; daemon SIGTERMed the subprocess |
| `iter_exhausted` | Post directive requested another iteration while `cycle.iter == max_iterations`; daemon refused to start the next iteration |
| `template_error` | Liquid render failure when preparing the phase prompt or command |

Sequence:

1. The originating cycle is marked aborted; its current iteration is recorded with the failure metadata.
2. The daemon evaluates `[[on_failure]]` first-match against `failure.kind` (and optionally `failure.phase`).
3. On match: spawn a new cycle with `cycle.kind = "failure"`; populate `{{ failure.* }}` and the `ROKI_FAILURE_*` env vars. The handler can read the failed cycle's logs via `roki log --cycle <failed_cycle_id> ...`.
4. On no match: emit a `failure_unhandled` structured event ([06-failure-handling §Failure-handler cycle](06-failure-handling.md)) carrying the failure metadata. The escalation queue is **not** touched. Worktree is retained for forensics.

A failure cycle that itself fails does **not** chain into another failure cycle. Such recursive failures land in the escalation queue ([06-failure-handling §Escalation queue](06-failure-handling.md)) instead, which bounds the recovery loop to one extra cycle per original failure.

### Cleanup

Cleanup priority is enforced before rule evaluation: the daemon walks `[[cleanup]]` first-match before `[[rule]]`. A matched cleanup entry runs as a normal cycle (subject to all phase-loop semantics), then the daemon deletes the ticket's worktree + session_tempdir and evicts it from the cache.

A cleanup entry with all three phases omitted is shorthand for "delete immediately, no cycle starts". The daemon performs the cleanup synchronously and emits a single structured `cycle_completed` event with `cycle.kind = cleanup` and zero iterations.

### Cold start

On daemon process start, the engine runs the same evaluation flow but with `cycle.trigger = "cold_start"`. See [07-recovery §Cold start](07-recovery.md) for the full enumeration / reconcile flow. Operators that need to suppress duplicate Linear comments on cold-start re-runs check `{% if cycle.trigger == "cold_start" %}` in their pre/post templates.

## Capabilities

- **Generic dispatch**: `[[cleanup]]` / `[[rule]]` / `[[on_failure]]` are the only three lists the daemon evaluates. Each is first-match. Operators express any workflow within them.
- **Three phases per cycle**: pre / run / post. `run` is required for any cycle-spawning entry; pre and post are optional with synthesized defaults. The cleanup-shorthand (all three omitted) is the only case where run may be absent. Each phase independently picks long-lived AI session or one-shot command. The daemon does not enforce a phase catalog.
- **Structured directive contract**: pre returns `run` / `end`; post returns `pre` / `run` / `end`. The daemon parses only the last JSON object on stdout per invocation; reasoning text is never interpreted.
- **Last-iteration data flow**: `{{ pre.* }}` / `{{ post.* }}` / `{{ run.* }}` expose the most recent iteration to subsequent phases. Older iterations live on disk only.
- **Iteration cap**: max_iterations is a hard daemon-enforced boundary on starting a new iteration. Operators can preempt cooperatively by inspecting `{{ cycle.iter }}` and `{{ config.max_iterations }}` in their pre / post body. When a post extends the cycle past the cap, daemon closes stdin and routes through `[[on_failure]] when.kind = "iter_exhausted"`.
- **Operator-driven retry/backoff**: there is no daemon retry budget. Post returns `run` to retry, with whatever delay the operator implements inside the template.
- **Queue-mode webhook serialization**: at most one cycle per ticket at a time. New webhooks update the diff cache and re-evaluate after the in-flight cycle ends.
- **Failure handler cycle as first-class**: `[[on_failure]]` runs as a cycle, with `{{ failure.* }}` and forensics access via `roki log --cycle <failed_id>`. Failures inside a failure cycle fall back to the default escalation behavior.

## Boundaries

- **No daemon-managed retry budget**: the daemon does not count phase non-clean exits and does not enforce backoff.
- **No daemon-side Linear writes**: the daemon never writes Linear directly. Linear feedback (labels, comments) is entirely operator-driven from inside pre / run / post invocations.
- **No long-lived per-ticket AI session across cycles**: the long-lived AI exists only within one cycle's pre/post chain. Cycle end terminates the session; the next cycle launches a fresh one. Cross-cycle scratch state goes through `roki log` if needed, not through a persisted session.
- **No phase catalog**: operators name their phases however they like inside workflow/*.md; the daemon does not reserve any phase name beyond `pre` / `run` / `post`.
- **No daemon-side artifact validation**: operators encode whatever artifact checks they want inside post.
- **No operator-installed Linear MCP requirement**: the daemon does not assume operators have any specific MCP installed. If a workflow needs to write Linear, the operator includes that capability in the cli line of the relevant phase.
- **No tracker-terminal preemption event**: the daemon does not synthesize a special preempt event when Linear state moves to `done` / `canceled`. Operators express the desired behavior with a `[[cleanup]]` entry.

## Traceability

- **Roadmap**: `roadmap.md` > Boundary Strategy.
- **Requirements**: pending — the spec rebuild will introduce IDs covering rule-list evaluation order, the directive schema, the iteration cap, and failure handling. Until then this FR carries the contract directly.
- **Design**: pending — the new spec set's design.md files will reference back to this FR.
- **Related FR**: [02-configuration](02-configuration.md), [03-linear-admission](03-linear-admission.md), [07-recovery](07-recovery.md), [05-worktree-and-session](05-worktree-and-session.md), [04-phase-execution](04-phase-execution.md), [08-observability-logs](08-observability-logs.md), [06-failure-handling](06-failure-handling.md), [09-log-access-cli](09-log-access-cli.md).
