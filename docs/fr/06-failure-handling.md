---
refs:
  id: fr:06-failure-handling
  kind: fr
  title: "Failure Handling"
  spec: roki-skeleton
  related:
    - fr:07-recovery
    - fr:04-state-execution
    - fr:08-observability-logs
    - fr:10-http-api
    - fr:11-roki-tui
    - fr:01-engine-model
---

# FR 06: Failure Handling

> Daemon-detected internal failures route through `on_failure:` first-match in WORKFLOW.yaml. The matched entry runs as a failure-handler cycle whose state subprocesses can write Linear (or any other channel) using whatever cli line the operator authored. When `on_failure:` does not match, the failure surfaces only through the structured event log; daemon-stuck failures additionally enter an in-memory escalation queue consumed by the TUI / HTTP API. The daemon process itself never writes Linear under any circumstance.

## Purpose

Failure surfacing is operator-authored. The daemon fires a failure-handler cycle (`cycle.kind = "failure"`) whose states can do whatever the operator wants — including a Linear MCP write — and the daemon adds an entry to the TUI escalation queue when the recovery path is exhausted.

## User-visible Behavior

### Daemon-detected failure kinds

| Kind | Trigger |
|---|---|
| `process_crash` | Subprocess killed by signal without a sentinel write |
| `unparseable` | Sentinel file present but JSON parse failed or `directive` field missing |
| `schema_drift` | Sentinel `directive` value not in `state.directives` ∪ built-in directive defaults |
| `fs_poison` | Filesystem error creating or recovering worktree / session-tempdir / sentinel-dir before a state launch ([05-worktree-and-session](05-worktree-and-session.md)). Cleanup-time fs errors do not match `on_failure:` — they enter the escalation queue. |
| `stall` | Stall window exceeded; daemon SIGTERMed the subprocess |
| `recursion_bound` | `state.visits > state.max_visits`; daemon refused the next visit |
| `template_error` | Liquid render failure when preparing `run:` cmd, `uses:` body, or `if:` condition |

Operator-facing outcomes (e.g. `needs_operator`, `needs_split`, `allowlist_rejected`) are operator-authored `outcome` strings on a normal cycle's terminal sentinel directive. Operator-driven Linear writes happen inside the states of those cycles.

### Failure-handler cycle

When the daemon detects an internal failure during an in-flight cycle:

1. The originating cycle is marked aborted; the current visit is recorded with the failure metadata (`failure.state_id`, `failure.visit_n`).
2. The daemon evaluates `on_failure:` first-match against `failure.kind` (and optionally `failure.state_id`, exposed via `when.phase` for matcher-syntax compatibility).
3. On match: spawn a new cycle with `cycle.kind = "failure"`. The cycle has its own UUID; the failed cycle's UUID is exposed as `{{ failure.failed_cycle_id }}` / `ROKI_FAILURE_FAILED_CYCLE_ID` so the failure handler can read the failed cycle's logs via `roki log --cycle <failed_cycle_id> --state <state_id> ...`.
4. On no match: the daemon emits a `failure_unhandled` event in the structured event log carrying `(ticket_id, cycle_id, failure.kind, state_id, visit_n, error_text, marker)`. The escalation queue is **not** touched. Operators that want unhandled failures surfaced grep / filter `roki events --kind failure_unhandled`. No Linear write occurs.

A failure cycle that itself fails does **not** chain into another failure cycle. Such recursive failures (and other daemon-stuck cases below) flow into the escalation queue as the **only** route to operator attention.

### `on_failure:` shape

```yaml
on_failure:
  - when:
      kind: { in: [unparseable, schema_drift] }
      phase: post                # optional; matches failure.state_id
    tasks:
      - id: postmortem
        run:
          cmd: "claude -p '/post-mortem {{ failure.failed_cycle_id }}' --output-format stream-json"
```

`when.kind` and `when.phase` use the same matcher vocabulary as `rules:` ([02-configuration §Condition vocabulary](02-configuration.md)). `when.phase` matches the state id that emitted the failure (kept as `phase` for matcher-grammar uniformity across rule lists). The three `when.kind` forms — equality (`when.kind = "..."`), set membership (`when.kind.in = [...]`), and negation (`when.kind.not = "..."`) — are mutually exclusive per entry; declaring more than one is a config-load error.

### Escalation queue

The daemon maintains an in-memory escalation queue surfacing **daemon-stuck failures** — cases where every operator-authored recovery path has been exhausted (or no path applies because the failure is daemon-internal). An entry is added when:

1. **A failure cycle itself fails.** `on_failure:` matched, the handler cycle ran, the handler cycle hit its own internal failure. Recursion is intentionally bounded to one level.
2. **Cleanup-time fs error.** Worktree / session-tempdir delete or orphan reconcile failed; no `on_failure:` evaluation applies because the cycle has already terminated.
3. **A daemon-internal error has no cycle association.** Examples: WORKFLOW.yaml hot-reload validation failure (the offending entry is rejected and the failure is queued), Liquid render failure before any subprocess is spawned, cold-start config load failure that does not refuse startup.

Normal cycle failures with no `on_failure:` match surface via the `failure_unhandled` structured event ([08-observability-logs §Event catalog](08-observability-logs.md)) only — they do **not** enter the queue. This keeps the queue scoped to "daemon needs human help".

Each entry carries `(ticket_id | null, cycle_id | null, failure.kind, state_id | null, visit_n | null, timestamp, error_text)`. `ticket_id`, `cycle_id`, `state_id`, and `visit_n` are null only for daemon-internal errors not associated with a specific cycle. Cycle-routed entries (failure-cycle-of-failure-cycle, cleanup-time fs error) always carry the originating cycle's `state_id`.

Consumers:

- The HTTP API exposes the queue at `GET /api/escalations` ([10-http-api](10-http-api.md)).
- The TUI renders the queue with a local-only acknowledgement affordance ([11-roki-tui](11-roki-tui.md)).
- Entries are cleared automatically when the ticket is evicted (cleanup-cycle completion, admission failure, orphan reconcile). Cycle-less daemon-internal entries persist until daemon restart. The queue is in-memory only and is reset on restart.

### Worktree retention

Failures that route through `on_failure:` do not delete the worktree or session tempdir on their own — the failure-handler cycle is a normal cycle and inherits the same lifecycle rules ([05-worktree-and-session](05-worktree-and-session.md)). Operators that want post-failure cleanup author either (a) a `cleanup:` entry whose match condition the failure handler creates (e.g. by writing a Linear label that the cleanup matches), or (b) a `cleanup:` entry that fires on the same `when.status` / `when.labels` change the operator's failure handler caused.

When `on_failure:` does not match, the worktree and session tempdir are retained for forensics until the operator manually cleans up.

### Configuration

- Linear write capability is whatever cli line the failure handler runs provides; the daemon holds no Linear write credential.
- Slack / Email / PagerDuty / Discord channels are not built into the daemon. Operators that want them author a failure-handler cycle that calls the appropriate webhook / CLI / MCP from inside a state.

## Capabilities

- **Linear feedback without daemon credentials**: the failure handler runs whatever cli the operator authored. The daemon never holds a Linear write credential or any other notification credential.
- **Channel separation**: Linear writes (or other operator-defined notifications) live inside the failure-handler cycle; the TUI escalation queue lives in-process and surfaces daemon-stuck failures only.
- **No-handler fallback**: when no `on_failure:` entry matches, the failure still appears in the structured event log via `failure_unhandled`. No silent failure path.
- **Worktree retention by default**: failures preserve forensics; cleanup is operator-driven.
- **Daemon never blocked by Linear / MCP**: a hung MCP call inside a failure handler is supervised by the same stall window used for any subprocess; SIGTERM applies.

## Boundaries

- **Slack / Email / PagerDuty / Discord** built into the daemon are out of scope. Operators add them inside `on_failure:` states.
- **Per-event routing rules / per-issue / per-repo channel split** are out of scope. A single `on_failure:` list is evaluated first-match.
- **Acknowledgement / read management on the Linear side** depends on Linear's own labelling / comment workflow; the daemon does not track ack state.
- **Notifications for normal state progress** are emitted by the operator's state cli lines, not by the daemon.
- **Failure-cycle inside a failure cycle** does not chain. The escalation queue is the only surface.
- **Persistent escalation queue** is out of scope; the queue is in-memory only and is reset on daemon restart.

## Related

[`docs/reference/log-events.md`](../reference/log-events.md), [07-recovery](07-recovery.md), [04-state-execution](04-state-execution.md), [08-observability-logs](08-observability-logs.md), [10-http-api](10-http-api.md), [11-roki-tui](11-roki-tui.md), [01-engine-model](01-engine-model.md).
