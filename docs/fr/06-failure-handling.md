---
refs:
  id: fr:06-failure-handling
  kind: fr
  title: "Failure Handling"
  spec: roki-mvp
  implements:
    - req:roki-mvp:12
    - req:roki-mvp:5.10
  related:
    - fr:07-recovery
    - fr:04-phase-execution
    - fr:08-observability-logs
    - fr:10-http-api
    - fr:11-roki-tui
    - fr:01-engine-model
---

# FR 06: Failure Handling

> Daemon-detected internal failures route through `[[on_failure]]` first-match in WORKFLOW.toml. The matched entry runs as a failure-handler cycle whose run / post phases can write Linear (or any other channel) using whatever cli line the operator authored. When `[[on_failure]]` does not match, the failure surfaces only through the structured event log and an in-memory escalation queue consumed by the TUI / HTTP API. The daemon process itself never writes Linear under any circumstance.

## Purpose

Failure surfacing is operator-authored. The daemon fires a failure-handler cycle (`cycle.kind = "failure"`) whose pre / run / post can do whatever the operator wants — including a Linear MCP write — and the daemon adds an entry to the TUI escalation queue regardless of whether the failure cycle ran.

## User-visible Behavior

### Daemon-detected failure kinds

The daemon classifies internal failures into the kinds listed in [01-engine-model §Failure handling](01-engine-model.md):

| Kind | Trigger |
|---|---|
| `process_crash` | Subprocess SIGSEGV or non-zero exit without a parseable terminal response |
| `unparseable` | Last JSON object on stdout failed to parse, or `directive` missing |
| `schema_drift` | `directive` value outside the legal set for the current phase |
| `repo_mismatch` | Pre's `repo` field does not match the admission-resolved repo ([05-worktree-and-session](05-worktree-and-session.md)) |
| `stall` | Stall window exceeded; daemon SIGTERMed the subprocess |
| `iter_exhausted` | Post directive requested another iteration while `cycle.iter == max_iterations`; daemon refused to start the next iteration |
| `template_error` | Liquid render failure when preparing a phase prompt or command |

Operator-facing outcomes (e.g. `needs_operator`, `needs_split`, `allowlist_rejected`) are operator-authored `outcome` strings on a normal cycle's terminal post directive. Operator-driven Linear writes happen inside the run / post of those cycles.

### Failure-handler cycle

When the daemon detects an internal failure during an in-flight cycle:

1. The originating cycle is marked aborted; its current iteration is captured with the failure metadata.
2. The daemon evaluates `[[on_failure]]` first-match against `failure.kind` (and optionally `failure.phase`).
3. On match: spawn a new cycle with `cycle.kind = "failure"`. The cycle has its own UUID; the failed cycle's UUID is exposed as `{{ failure.failed_cycle_id }}` / `ROKI_FAILURE_FAILED_CYCLE_ID` so the failure handler can read the failed cycle's logs via `roki log --cycle <failed_cycle_id> ...`. A `phase_failed` event for the original failure is emitted as usual.
4. On no match: the daemon emits a `failure_unhandled` event in the structured event log carrying the same `(ticket_id, cycle_id, failure.kind, phase, error_text)` payload as the escalation entry would carry. The escalation queue is **not** touched. Operators that want unhandled failures surfaced in the TUI grep / filter `roki events --kind failure_unhandled`. No Linear write occurs.

A failure cycle that itself fails does **not** chain into another failure cycle. Such recursive failures (and other daemon-stuck cases described below) flow into the escalation queue as the **only** route to operator attention.

### `[[on_failure]]` shape

```toml
[[on_failure]]
when.kind.in = ["unparseable", "schema_drift"]
when.phase = "post"   # optional
run.cmd = "claude -p '/post-mortem {{ failure.failed_cycle_id }}' --output-format stream-json"
post.prompt = "Output {directive: 'end'}"
```

`when.kind` and `when.phase` use the same matcher vocabulary as `[[rule]]` ([02-configuration §Condition vocabulary](02-configuration.md)). `when.kind.not = "..."` and `when.kind.in = [...]` are also allowed.

### Escalation queue

The daemon maintains an in-memory escalation queue surfacing **daemon-stuck failures** — the cases where every operator-authored recovery path has already been exhausted (or no path applies because the failure is daemon-internal). Specifically, an entry is added when:

1. **A failure cycle itself fails.** `[[on_failure]]` matched, the handler cycle ran, the handler cycle hit its own internal failure. The recursion is intentionally bounded to one level, so this lands in the queue rather than spawning another failure cycle.
2. **A daemon-internal error has no cycle association.** Examples: WORKFLOW.toml hot-reload validation failure (the offending entry is rejected and the failure is queued), filesystem error during cleanup, Liquid render failure before any subprocess is spawned, cold-start config load failure that does not refuse startup.

Normal cycle failures with no `[[on_failure]]` match are surfaced via the `failure_unhandled` structured event ([08-observability-logs §Event catalog](08-observability-logs.md)) only — they do **not** enter the queue. This keeps the queue tightly scoped to "daemon needs human help" cases instead of ambient unhandled failures.

Each entry carries `(ticket_id | null, cycle_id | null, failure.kind, phase | null, timestamp, error_text)`. `ticket_id` and `cycle_id` are null for daemon-internal errors not associated with a specific cycle.

Consumers:

- The HTTP API exposes the queue at `GET /api/escalations` ([10-http-api](10-http-api.md)).
- The TUI renders the queue with a local-only acknowledgement affordance ([11-roki-tui](11-roki-tui.md)).
- Entries are cleared automatically when the ticket is evicted (cleanup-cycle completion, admission failure, orphan reconcile). Cycle-less daemon-internal entries persist until daemon restart. The queue is in-memory only and is reset on restart.

### Worktree retention

Failures that route through `[[on_failure]]` do not delete the worktree or session tempdir on their own — the failure-handler cycle is a normal cycle and inherits the same lifecycle rules ([05-worktree-and-session](05-worktree-and-session.md)). Operators that want post-failure cleanup author either (a) a `[[cleanup]]` entry whose match condition the failure handler creates (e.g. by writing a Linear label that the cleanup matches), or (b) a `[[cleanup]]` entry that fires on the same `when.status` / `when.labels` change the operator's failure handler caused.

When `[[on_failure]]` does not match, the worktree and session tempdir are retained for forensics until the operator manually cleans up.

### Configuration

- Linear write capability is whatever cli line the failure handler runs provides; the daemon holds no Linear write credential.
- Slack / Email / PagerDuty / Discord channels are not built into the daemon. Operators that want them author a failure-handler cycle that calls the appropriate webhook / CLI / MCP from inside the run phase.

## Capabilities

- **Linear feedback without daemon credentials**: the failure handler runs whatever cli the operator authored. The daemon never holds a Linear write credential or any other notification credential.
- **Channel separation**: a Linear write (or any other operator-defined notification) lives inside the failure-handler cycle; the TUI escalation queue lives in-process and surfaces every detected failure.
- **No-handler fallback**: when no `[[on_failure]]` entry matches, the failure still appears in the structured event log and the escalation queue. There is no silent failure path.
- **Worktree retention by default**: failures preserve forensics; cleanup is operator-driven.
- **Daemon never blocked by Linear / MCP**: a hung MCP call inside a failure handler is supervised by the same stall window used for any subprocess; SIGTERM applies.

## Boundaries

- **Slack / Email / PagerDuty / Discord** built into the daemon are out of scope. Operators add them inside `[[on_failure]]` run phases.
- **Per-event routing rules / per-issue / per-repo channel split** are out of scope. A single `[[on_failure]]` list is evaluated first-match.
- **Acknowledgement / read management on the Linear side** depends on Linear's own labelling / comment workflow; the daemon does not track ack state.
- **Notifications for normal phase progress** are emitted by the operator's pre / run / post cli lines, not by the daemon.
- **Failure-cycle inside a failure cycle** does not chain. The default behavior (escalation entry only) bounds the recovery depth.
- **Persistent escalation queue** is out of scope; the queue is in-memory only and is reset on daemon restart.

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Operator notifications.
- **Requirements**:
  - `req:roki-mvp:12`: Daemon-Only Failure Surfacing — covered by `[[on_failure]]` + escalation queue.
  - `req:roki-mvp:5.10`: Retry-budget exhaustion — covered by `[[on_failure]] when.kind = "iter_exhausted"`.
- **Reference**: [`docs/reference/log-events.md`](../reference/log-events.md) (pending rewrite for the new failure event catalog).
- **Related FR**: [07-recovery](07-recovery.md), [04-phase-execution](04-phase-execution.md), [08-observability-logs](08-observability-logs.md), [10-http-api](10-http-api.md), [11-roki-tui](11-roki-tui.md), [01-engine-model](01-engine-model.md).
