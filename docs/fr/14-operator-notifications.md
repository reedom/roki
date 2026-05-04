---
refs:
  id: fr:14-operator-notifications
  kind: fr
  title: "Daemon-Only Failure Surfacing"
  spec: roki-mvp
  implements:
    - req:roki-mvp:12
    - req:roki-mvp:5.10
  related:
    - fr:19-orchestrator-session
    - fr:04-state-machine-and-recovery
    - fr:07-worker-execution
    - fr:09-pre-pr-gate
    - fr:13-observability-logs
    - fr:15-http-api
    - fr:16-roki-tui
---

# FR 14: Daemon-Only Failure Surfacing

> Surface every daemon-only failure (the kind no agent has self-reported to Linear) through the long-lived orchestrator session A when A is alive, and through the in-memory escalation queue (consumed by the optional TUI / HTTP API) in every case. The daemon process itself never writes Linear; when A is dead there is no Linear-side notification — TUI + structured log is the only surface.

## Purpose

Some failures are visible only to the daemon: a phase subprocess stalled and was killed, a phase non-clean-exit retry budget was exhausted, a filesystem error poisoned a worktree, restart-recovery found an orphan, the review gate denied beyond its retry budget, A itself crashed or schema-drifted persistently or exhausted `max_phases`. The phase agent never gets a chance to write back to Linear in those cases, and the daemon does not hold a Linear write path itself.

The new architecture replaces the prior linear-updater subagent with **A processing a `daemon_directive` event** (see [fr:19-orchestrator-session > Event catalog](19-orchestrator-session.md)). When A is alive the daemon sends a structured directive on A's stdin; A writes the appropriate Linear label + comment via the operator's installed Linear MCP and returns `action=linear_update_done`. When A is dead — `orchestrator_crash`, `orchestrator_unparseable`, or `orchestrator_budget_exhausted` — the daemon does **not** fall back to a Linear write of its own; the failure surfaces via structured log + TUI escalation queue only.

The same failure events are also enqueued in the in-memory **escalation queue** so they are visible through the optional HTTP API ([15-http-api](15-http-api.md)) and the TUI ([16-roki-tui](16-roki-tui.md)) regardless of whether A's Linear write path was usable.

## User-visible Behavior

### When a `daemon_directive` is sent (A is alive)

The daemon emits a `daemon_directive` event to A whenever a daemon-only failure event is recorded for an issue whose orchestrator session is still running. The current trigger set:

| Trigger | Reason / event | Source |
|---|---|---|
| Phase subprocess stalled and was terminated | `daemon_directive` (`kind=stall`); A typically follows with `action=stop` | [07-worker-execution](07-worker-execution.md) |
| Ticket-level retry budget exhausted on phase non-clean exits | `daemon_directive` (`kind=retry_exhausted`) → A `action=stop` with `outcome=failure` | [07-worker-execution](07-worker-execution.md), `req:roki-mvp:5.10` |
| Pre-PR review gate Denied beyond its retry budget | `daemon_directive` (`kind=review_gate_exhausted`) → `Inactive(reason=review_gate_exhausted)` | [09-pre-pr-gate](09-pre-pr-gate.md) |
| Filesystem error poisoned an issue | `daemon_directive` (`kind=fs_poison`) | [04-state-machine-and-recovery](04-state-machine-and-recovery.md) |
| Restart-recovery saw orphaned residue | `daemon_directive` (`kind=orphan`) | [04-state-machine-and-recovery](04-state-machine-and-recovery.md), Req 10.3 |

The `needs_split` and `allowlist_rejected` admission rejections are **not** routed through `daemon_directive` — A returns those `admission_decision` directives and writes the matching Linear label + comment in the same turn (per [fr:19-orchestrator-session](19-orchestrator-session.md)).

Events that an agent (A or a phase subprocess) is expected to self-report through Linear (normal phase completions, agent-recoverable errors the phase agent surfaces in the ticket itself, the review-gate fix-finding loop's intentional re-launches via `gate_deny`) **do not** trigger a `daemon_directive`.

### When A is dead (no Linear-side notification)

When the failure event itself is the death of A, the daemon routes the issue to one of three terminal `Inactive(reason=*)` values (see [fr:19-orchestrator-session > Failure modes](19-orchestrator-session.md) and [04-state-machine-and-recovery](04-state-machine-and-recovery.md)) and surfaces the issue exclusively via the TUI escalation queue:

| Trigger | Inactive.reason | Auto-cleanup eligible? |
|---|---|---|
| A process crash, SIGSEGV, non-zero exit without a `stop` action, or A stall | `orchestrator_crash` | no |
| A schema drift on two consecutive turns after one daemon-side reprompt | `orchestrator_unparseable` | no |
| `max_phases` exhausted (A would nominate another phase but the budget is gone) | `orchestrator_budget_exhausted` | no |

These three reasons are **not** auto-cleanup eligible: the worktree and session tempdir are preserved until the operator manually closes the Linear ticket, after which `Cleaning` proceeds. There is no Linear-side fallback because A is the only Linear write path — once it is dead, the daemon will not impersonate it.

If a daemon-only failure (e.g. `fs_poison`, `orphan`, phase stall) is detected while A is no longer alive — for example because the issue had already terminated and A has been gracefully torn down — the failure surfaces via structured log + escalation queue only, on the same path as the three orchestrator-dead reasons above.

### `daemon_directive` event payload (daemon → A)

Each `daemon_directive` event passes at minimum:

```
issue_id:    "ENG-1234"
kind:        "stall" | "retry_exhausted" | "review_gate_exhausted" |
             "fs_poison" | "orphan" | "<future kind>"
fields:      { ...kind-specific structured fields, e.g.
               correlation_id, repos[], worktree_path, last_subtype,
               attempts, window_ms, errno, ... }
timestamp:   "2026-05-04T12:34:56Z"
```

The fields each `kind` carries are documented in [`docs/reference/log-events.md`](../reference/log-events.md) alongside the daemon's own structured-log event for the same trigger. The directive shall not include the Linear API token, the Linear webhook secret, or any other operator-declared secret (Req 12.5).

The actual Linear label name(s) and comment text are determined by A's `prompt_template_orchestrator` system prompt and any operator instructions therein; the daemon contributes only the directive `kind` and the structured fields and does not template the Linear write itself (Req 12.6).

### Escalation queue

The daemon maintains an in-memory escalation queue keyed by Linear issue identifier; each entry holds the most recent failure category, structured fields, timestamp, correlation identifier, and repo identifier (when applicable). The queue is populated **at the same moment** the failure event is detected — for both A-alive (`daemon_directive` sent) and A-dead (no Linear write) paths — so the TUI / HTTP API surface is unaffected by A's Linear write outcome.

Consumers:

- The optional HTTP API exposes the queue through `GET /api/v1/state` ([15-http-api](15-http-api.md)).
- The TUI renders the queue with a local-only acknowledgement affordance ([16-roki-tui](16-roki-tui.md)). The TUI escalation queue is the **secondary surface for all daemon-only failures**, not just orchestrator-dead ones, and is the **primary surface for the three orchestrator-dead reasons**.
- A queue entry is automatically cleared when the corresponding issue moves out of `Inactive` (e.g. via re-admission or cleanup).

### Failure handling (A response to a `daemon_directive`)

- If A's `action=linear_update_done` indicates a partial Linear write (the `linear_writes` field shows a subset of the expected writes), the daemon logs the partial-write entry, retains the escalation queue entry, and shall not retry on A's behalf (Req 12.7).
- If A's turn ends with an error, or A crashes mid-turn while processing a `daemon_directive`, the daemon logs the failure and routes the issue to `Inactive(reason=orchestrator_crash)` (Req 12.7); the escalation queue entry stays so the TUI continues to surface it.
- The daemon does not retry the `daemon_directive` itself; if A's first attempt fails, the escalation queue is the operator's authoritative surface.

### Configuration

- The orchestrator namespace `extension.orchestrator.{model, effort, max_phases, allowed_tools}` ([fr:19-orchestrator-session > Configuration](19-orchestrator-session.md)) governs A's behaviour, including its tool surface (Linear MCP write + `Read`). There is no separate `extension.linear_updater.*` namespace; the loader rejects that legacy key per `req:roki-mvp:2.13`.
- Slack and other push notification channels are **not** configured here. Daemon-only failure surfacing routes through A → Linear MCP (when A is alive) and the TUI escalation queue (always).

## Capabilities

- **Linear-side feedback without daemon write credentials**: A is the only Linear write path; the daemon never holds a write-capable Linear path.
- **Channel separation**: when A is alive, the Linear ticket carries the agent-visible feedback (label + comment) and the TUI escalation queue carries the live operator-facing surface. When A is dead, the TUI escalation queue is the only surface.
- **Secrets-free directive payload**: the directive carries identifiers and structured fields only; secrets (Linear API token, webhook secret) are never propagated.
- **Operator-controlled copy**: label names and comment prose live in `prompt_template_orchestrator`, not in the daemon binary.
- **Daemon never blocked by Linear**: a Linear / MCP outage during a `daemon_directive` turn logs and continues; the per-issue state machine is unaffected (except for the orchestrator-dead routing path documented above).

## Boundaries

- **Slack / Email / PagerDuty / Discord** are out of scope for v1 (the A-driven Linear path + TUI escalation queue replaces them; can be re-introduced as a separate channel if Linear-routed feedback proves insufficient).
- **Per-event routing rules / per-issue or per-repo channel split** are out of scope (a single `daemon_directive` path handles every alive-A trigger).
- **Acknowledgement / read management on the Linear side** depends on Linear's own labelling / comment workflow; the daemon does not track ack state.
- **Notifications to Linear tickets for normal phase progress** (PR opened, status updates) are performed by the phase agent's own Linear MCP path, not by `daemon_directive`.
- **A is not a substitute for a phase subprocess**: A does not implement, review, or write to the worktree; A's role is admission classification, phase planning, daemon-directive interpretation, and Linear writes only ([fr:19-orchestrator-session](19-orchestrator-session.md)).
- **A failure mid-`daemon_directive`** does not trigger another `daemon_directive` (no infinite recursion); the daemon routes A's death to `Inactive(reason=orchestrator_crash)` and the TUI surfaces it.
- **No Linear fallback when A is dead**: the three orchestrator-dead reasons surface via structured log + TUI escalation queue only.

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Operator notifications; Boundary Strategy > "Orchestrator-vs-phase boundary"
- **Requirements**:
  - `req:roki-mvp:12`: Daemon-Only Failure Surfacing via `daemon_directive` (and TUI-only for orchestrator-dead reasons)
  - `req:roki-mvp:5.10`: retry-exhausted `daemon_directive` contract
  - `req:roki-mvp:2`: orchestrator namespace replaces the removed `extension.linear_updater.*`
- **Reference**: [`docs/reference/log-events.md`](../reference/log-events.md) (canonical structured-log event catalog including `daemon_directive` payload schema)
- **Related FR**: [04-state-machine-and-recovery](04-state-machine-and-recovery.md), [07-worker-execution](07-worker-execution.md), [09-pre-pr-gate](09-pre-pr-gate.md), [13-observability-logs](13-observability-logs.md), [15-http-api](15-http-api.md), [16-roki-tui](16-roki-tui.md), [19-orchestrator-session](19-orchestrator-session.md)
