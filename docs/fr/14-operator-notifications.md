---
refs:
  id: fr:14-operator-notifications
  kind: fr
  title: "Failure Surfacing"
  spec: roki-mvp
  implements:
    - req:roki-mvp:12
    - req:roki-mvp:5.10
  related:
    - fr:04-state-machine-and-recovery
    - fr:07-worker-execution
    - fr:13-observability-logs
    - fr:15-http-api
    - fr:16-roki-tui
    - fr:20-rule-and-cycle-engine
---

# FR 14: Failure Surfacing

> Daemon-detected internal failures route through `[[on_failure]]` first-match in WORKFLOW.toml. The matched entry runs as a failure-handler cycle whose run / post phases can write Linear (or any other channel) using whatever cli line the operator authored. When `[[on_failure]]` does not match, the failure surfaces only through the structured event log and an in-memory escalation queue consumed by the TUI / HTTP API. The daemon process itself never writes Linear under any circumstance.

## Purpose

The previous design routed daemon-only failures through a `daemon_directive` event into a long-lived orchestrator session, which would then write Linear via the operator's Linear MCP. With the cycle engine ([20-rule-and-cycle-engine](20-rule-and-cycle-engine.md)) the orchestrator session concept is gone: there is no long-lived AI to receive a `daemon_directive` between cycles. Failure surfacing is now operator-authored: the daemon fires a failure-handler cycle (`cycle.kind = "failure"`) whose pre / run / post can do whatever the operator wants — including a Linear MCP write — and the daemon adds an entry to the TUI escalation queue regardless of whether the failure cycle ran.

## User-visible Behavior

### Daemon-detected failure kinds

The daemon classifies internal failures into the kinds listed in [20-rule-and-cycle-engine §Failure handling](20-rule-and-cycle-engine.md):

| Kind | Trigger |
|---|---|
| `process_crash` | Subprocess SIGSEGV or non-zero exit without a parseable terminal response (covers former `fs_poison` during launch) |
| `unparseable` | Last JSON object on stdout failed to parse, or `directive` missing |
| `schema_drift` | `directive` value outside the legal set for the current phase (covers former `orchestrator_unparseable`) |
| `stall` | Stall window exceeded; daemon SIGTERMed the subprocess (covers former phase-stall and orchestrator-stall) |
| `iter_exhausted` | `max_iterations` exceeded with no cooperative termination (covers former `orchestrator_budget_exhausted` and `retry_exhausted`) |
| `template_error` | Liquid render failure when preparing a phase prompt or command |

The previous twelve `Inactive.reason` variants (`awaiting_linear`, `needs_operator`, `spec_incomplete`, `needs_split`, `allowlist_rejected`, `orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted`, `stall`, `retry_exhausted`, `fs_poison`, `orphan`) are gone. Operator-facing outcomes (`needs_operator`, `spec_incomplete`, `needs_split`, `allowlist_rejected`) are now operator-authored `outcome` strings on a normal cycle's terminal post directive. Operator-driven Linear writes happen inside the run / post of those cycles.

### Failure-handler cycle

When the daemon detects an internal failure during an in-flight cycle:

1. The originating cycle is marked aborted; its current iteration is captured with the failure metadata.
2. The daemon evaluates `[[on_failure]]` first-match against `failure.kind` (and optionally `failure.phase`).
3. On match: spawn a new cycle with `cycle.kind = "failure"`. The cycle has its own UUID; the failed cycle's UUID is exposed as `{{ failure.failed_cycle_id }}` / `ROKI_FAILURE_FAILED_CYCLE_ID` so the failure handler can read the failed cycle's logs via `roki log --cycle <failed_cycle_id> ...`.
4. On no match: the daemon does not write Linear. The failure surfaces only through the structured event log and the escalation queue.

A failure cycle that itself fails does **not** chain into another failure cycle. The default behavior (silent log + escalation entry) applies, bounding the recovery loop to one extra cycle per original failure.

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

The daemon maintains an in-memory escalation queue keyed by `(ticket_id, cycle_id)` with the most recent failure category, structured fields, timestamp, and a short error text (typically the truncated last line of stderr or the parser's error message). The queue is populated **at the moment of failure detection**, before `[[on_failure]]` evaluation, so the TUI / HTTP API surface is unaffected by whether or not a failure handler ran.

Consumers:

- The HTTP API exposes the queue at `GET /api/escalations` ([15-http-api](15-http-api.md)).
- The TUI renders the queue with a local-only acknowledgement affordance ([16-roki-tui](16-roki-tui.md)). It is the **primary surface** when no `[[on_failure]]` matches and the **secondary surface** otherwise.
- Entries are cleared automatically when the ticket is evicted (cleanup-cycle completion, admission failure, orphan reconcile). They are also lost on daemon restart (the queue is in-memory only).

### Worktree retention

Failures that route through `[[on_failure]]` do not delete the worktree or session tempdir on their own — the failure-handler cycle is a normal cycle and inherits the same lifecycle rules ([06-worktree-and-session](06-worktree-and-session.md)). Operators that want post-failure cleanup author either (a) a `[[cleanup]]` entry whose match condition the failure handler creates (e.g. by writing a Linear label that the cleanup matches), or (b) a `[[cleanup]]` entry that fires on the same `when.status` / `when.labels` change the operator's failure handler caused.

When `[[on_failure]]` does not match, the worktree and session tempdir are retained for forensics until the operator manually cleans up.

### Configuration

- There is no separate `extension.linear_updater.*` namespace and no `extension.orchestrator.*` namespace. Linear write capability is whatever the cli line the failure handler runs provides.
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
  - `req:roki-mvp:12`: Daemon-Only Failure Surfacing — replaced by `[[on_failure]]` + escalation queue.
  - `req:roki-mvp:5.10`: Retry-budget exhaustion — replaced by `[[on_failure]] when.kind = "iter_exhausted"`.
- **Reference**: [`docs/reference/log-events.md`](../reference/log-events.md) (pending rewrite for the new failure event catalog).
- **Related FR**: [04-state-machine-and-recovery](04-state-machine-and-recovery.md), [07-worker-execution](07-worker-execution.md), [13-observability-logs](13-observability-logs.md), [15-http-api](15-http-api.md), [16-roki-tui](16-roki-tui.md), [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md).
