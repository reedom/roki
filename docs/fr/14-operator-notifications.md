---
refs:
  id: fr:14-operator-notifications
  kind: fr
  title: "Daemon-Only Failure Surfacing"
  spec: roki-mvp
  implements:
    - req:roki-mvp:12
    - req:roki-mvp:5.10
---

# FR 14: Daemon-Only Failure Surfacing

> Surface every daemon-only failure (the kind the agent inside the worker cannot self-report to Linear) through two complementary channels: a Linear-side label + comment posted by the **linear-updater subagent**, and an in-memory **escalation queue** consumed by the optional TUI / HTTP API. The daemon process itself never writes Linear.

## Purpose

Some failures are visible only to the daemon: the worker stalled and was killed, max-turns ran out before the agent could finish, the setup judge couldn't be parsed, a filesystem error poisoned a worktree, recovery found an orphan, or the judge classified the ticket as touching multiple repos. The agent never gets a chance to write back to Linear in those cases.

Rather than holding a Linear write path inside the daemon (which would force the daemon to carry write-capable Linear credentials and own a write code path it otherwise doesn't need), roki dispatches a **linear-updater subagent**: a setup-judge-shaped one-shot `claude --print --output-format stream-json` invocation whose only job is to translate a structured directive payload into Linear MCP `addLabel` / `createComment` calls. The label name and comment body are operator-authored in `prompt_template_linear_updater` ([02-configuration](02-configuration.md)).

The same failure events are also enqueued in the in-memory **escalation queue** so they're visible through the optional HTTP API ([15-http-api](15-http-api.md)) and the TUI ([16-roki-tui](16-roki-tui.md)) without depending on Linear-side propagation.

## User-visible Behavior

### When a linear-updater dispatch fires

A linear-updater invocation is dispatched whenever a daemon-only failure event is recorded — i.e. whenever the orchestrator transitions an issue into a `failure`-flavored `Inactive(reason=...)`, or rejects a setup-judge classification. The current trigger set:

| Trigger | Reason / event | Source |
|---|---|---|
| Worker stalled and was terminated | `Inactive(reason=stall)` | [07-worker-execution](07-worker-execution.md) |
| Worker hit `--max-turns` before clean exit | `Inactive(reason=max_turns_exhausted)` | [07-worker-execution](07-worker-execution.md) |
| Worker terminal `result` event reported an uncompiled `subtype` | `Inactive(reason=unknown_subtype)` | [07-worker-execution](07-worker-execution.md) |
| Non-clean exit retry budget exhausted | `Inactive(reason=retry_exhausted)` | [07-worker-execution](07-worker-execution.md) |
| Pre-PR review gate Denied beyond its retry budget | `Inactive(reason=review_gate_exhausted)` | [09-pre-pr-gate](09-pre-pr-gate.md) |
| Setup judge unparseable after retry | `Inactive(reason=judge_unparseable)` | [05-setup-judge](05-setup-judge.md) |
| Filesystem error poisoned an issue | `Inactive(reason=fs_poison)` | [06-worktree-and-session](06-worktree-and-session.md) |
| Recovery saw orphaned residue | `Inactive(reason=orphan)` | [04-state-machine-and-recovery](04-state-machine-and-recovery.md) |
| Setup judge classified as multi-repo | `Inactive(reason=needs_split)` | [05-setup-judge](05-setup-judge.md) |
| Setup judge named a repo outside the allowlist | `Inactive(reason=allowlist_rejected)` | [05-setup-judge](05-setup-judge.md) |

Events that the worker is expected to self-report through Linear (normal completions, agent-recoverable errors, the worker's own re-launches via the review gate's fix-finding loop) **do not** trigger a linear-updater dispatch.

### Directive payload (daemon → linear-updater)

The daemon does not embed any prose copy. It hands the linear-updater a structured payload:

```
issue_id:    "ENG-1234"
kind:        "stall" | "max_turns_exhausted" | "unknown_subtype" |
             "retry_exhausted" | "review_gate_exhausted" |
             "judge_unparseable" | "fs_poison" | "orphan" |
             "needs_split" | "allowlist_rejected"
fields:      { ...kind-specific structured fields, e.g.
               correlation_id, repos[], worktree_path, last_subtype,
               attempts, window_ms, errno, classified_repos[], ... }
timestamp:   "2026-05-04T12:34:56Z"
```

The fields each `kind` carries are documented in [`docs/reference/log-events.md`](../reference/log-events.md) alongside the daemon's own structured-log event for the same trigger.

### Linear-updater prompt template responsibility

The operator-authored `prompt_template_linear_updater` block in `WORKFLOW.md` decides:

- which Linear label name to add for each `kind` (e.g. `roki-needs-split`, `roki-stalled`, `roki-escalated`);
- the comment text and Linear formatting per `kind` (typically a Liquid `if/elsif` chain over `directive.kind`);
- whether to remove a previously-added label on subsequent dispatches.

The daemon never sees the prose. This keeps user-facing copy where existing `prompt_template_*` blocks already live.

### Escalation queue

The daemon maintains an in-memory escalation queue keyed by Linear issue identifier; each entry holds the most recent failure category, structured fields, timestamp, correlation identifier, and repo identifier (when applicable). The queue is populated **at the same moment** the linear-updater dispatch fires (not after) so it is unaffected by linear-updater outcome.

Consumers:

- The optional HTTP API exposes the queue through `GET /api/v1/state` ([15-http-api](15-http-api.md)).
- The TUI renders the queue with a local-only acknowledgement affordance ([16-roki-tui](16-roki-tui.md)).
- A queue entry is automatically cleared when the corresponding issue moves out of `Inactive` (e.g. via re-admission or cleanup).

### Failure handling (linear-updater itself failed)

- The linear-updater subprocess is supervised with the same lifecycle as the worker: stall detection, stream-json parsing, bounded retry (at most one immediate retry on non-clean exit, per [07-worker-execution](07-worker-execution.md) §linear-updater).
- On persistent failure (Linear MCP unavailable, network error, configuration drift, subprocess non-clean exit, MCP authentication failure):
  - Log the failure at warn severity with the directive `kind`, the issue identifier, and the underlying error.
  - **Do not** retry the directive after the immediate retry; do not crash; do not block; do not alter the per-issue state machine.
  - The escalation queue entry remains in place, so the TUI / HTTP API still surfaces the failure.

### Configuration

- The `extension.linear_updater.*` namespace under `WORKFLOW.md` configures: `timeout_ms` (per-invocation timeout), `model` (the Claude model identifier; defaults to the same small model used by the setup judge), and any per-kind allowed-tool restrictions.
- Slack and other push notification channels are **not** configured here. Daemon-only failure surfacing routes through linear-updater + escalation queue only.

## Capabilities

- **Linear-side feedback without daemon write credentials**: linear-updater is an agent invocation that uses the operator's installed Linear MCP. The daemon never holds a write-capable Linear path.
- **Channel separation**: Linear ticket carries the agent-visible feedback (label + comment); the TUI escalation queue carries the live operator-facing surface. Both populate from the same in-daemon event.
- **Secrets-free directive payload**: the directive carries identifiers and structured fields only; secrets (Linear API token, webhook secret) are never propagated.
- **Operator-controlled copy**: label names and comment prose live in `prompt_template_linear_updater`, not in the daemon binary.
- **Daemon never blocked by Linear**: a Linear / MCP outage logs and continues; the per-issue state machine is unaffected.

## Boundaries

- **Slack / Email / PagerDuty / Discord** are out of scope for v1 (linear-updater + TUI escalation queue replaces them; can be re-introduced as a separate channel if Linear-routed feedback proves insufficient).
- **Per-event routing rules / per-issue or per-repo channel split** are out of scope (a single linear-updater path handles every trigger).
- **Acknowledgement / read management on the Linear side** depends on Linear's own labelling / comment workflow; the daemon does not track ack state.
- **Notifications to Linear tickets for normal worker progress** (PR opened, status updates) are performed by the worker's own kiro skill via Linear MCP, not by the linear-updater.
- **The linear-updater is not a worker substitute**: it does not implement, review, or write to the worktree. Its sole responsibility is translating a directive payload into Linear-side label / comment writes.
- **Failures of the linear-updater itself** are not sent through linear-updater (no infinite recursion); they are logged structurally and surfaced via the escalation queue.

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Operator notifications; Boundary Strategy > "Subprocess invocation taxonomy"
- **Requirements**:
  - `roki-mvp Req 12`: Daemon-Only Failure Surfacing via linear-updater
  - `roki-mvp Req 5.10`: linear-updater invocation contract
  - `roki-mvp Req 2`: `extension.linear_updater.*` namespace reservation
- **Design**:
  - `Linear-Updater Subagent` / `Engine Adapter` / `Escalation Queue` sections of `.kiro/specs/roki-mvp/design.md`
- **Related FR**: 02-configuration, 04-state-machine-and-recovery, 05-setup-judge, 06-worktree-and-session, 07-worker-execution, 09-pre-pr-gate, 13-observability-logs, 15-http-api, 16-roki-tui
