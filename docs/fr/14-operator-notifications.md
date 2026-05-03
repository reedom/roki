---
refs:
  id: fr:14-operator-notifications
  kind: fr
  title: "Operator Notifications"
  spec: roki-mvp
  implements:
    - req:roki-mvp:12
---

# FR 14: Operator Notifications

> Send daemon-only failures (the kind that the agent cannot write back to Linear) to Slack.

## Purpose

Deliver to the operator the kinds of failures that **the agent inside the worker cannot self-report to Linear**. Keep agent-originated messages on the Linear ticket and route daemon-originated "stuck/killed" events to a separate channel (Slack), so that Linear stays low-noise while stuck tickets are not missed.

## User-visible Behavior

### Configuration

- **`[notifications.slack]` block** ([02-configuration](02-configuration.md)) declares a webhook URL or bot token + target channel.
- **Block absent**: emit a startup warning and skip every Slack post (structured logs are still emitted).
- **Block present + destination cannot be resolved**: refuse startup.

### Events that produce a Slack notification (= cases where the worker cannot write to Linear)

- **`TerminalFailure` from stall detection** ([07-worker-execution](07-worker-execution.md)) — the daemon killed it, so the agent cannot self-report.
- **`TerminalFailure` from `--max-turns` exhaustion** ([07-worker-execution](07-worker-execution.md)) — Claude terminated the agent.
- **`TerminalFailure` from unknown `result.subtype`** ([07-worker-execution](07-worker-execution.md)).
- **`TerminalFailure` from ticket-level retry budget exhaustion** ([07-worker-execution](07-worker-execution.md)).
- **The setup judge did not produce parseable findings even after retry** ([05-setup-judge](05-setup-judge.md)) — the judge cannot write to Linear.
- **Filesystem error poisoned the session/worktree** ([06-worktree-and-session](06-worktree-and-session.md)).
- **Orphan found during startup recovery** ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)).

### Events that *do not* produce a Slack notification

- Failures the agent itself can self-report to Linear (normal completion, agent-recoverable error, gate fail that enters the fix loop / retry, etc.).

### Notification payload

- Linear issue identifier (with a Linear-resolvable link)
- Daemon-internal failure category
- Worker invocation correlation identifier (when applicable)
- Repository identifier (when applicable)
- Event timestamp
- **Never include any secret** (Linear API token / webhook secret / Slack credentials)

### Failure handling

- On Slack post failure: retry once immediately. Even on persistent failure, the daemon does not crash, block, or change state; the destination identifier and the underlying error are written to structured logs.
- A single workspace-level channel only (per-repo / per-issue routing is out of scope).
- Never blocks: a Slack outage does not affect the daemon itself or the per-issue state machine.

## Capabilities

- **Channel separation**: intentionally split the Linear ticket and Slack.
- **Secrets-free payload**: every field is designed to be secret-free.
- **Daemon never blocked by Slack**: a Slack failure has no side effects on per-issue state.

## Boundaries

- **Destinations other than Slack** (Email / PagerDuty / Discord, etc.) are out of scope.
- **Per-event routing rules** are out of scope (the events that get routed are fixed by the requirements).
- **Acknowledgement / read management** is out of scope (depends on Slack-side features).
- **Notifications to Linear tickets** are performed by the agent (the daemon does not touch them).
- **Failures of gates / distill phase** are notified here only when the agent cannot put them on Linear / the fix loop in the next turn (according to each spec's behavior).

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Operator notifications
- **Requirements**:
  - `roki-mvp Req 12`: Operator Notification Channel
  - `roki-mvp Req 2.12`: Slack configuration
- **Design**:
  - `Operator Notifications` section of `.kiro/specs/roki-mvp/design.md`
- **Related FR**: 02-configuration, 04-state-machine-and-recovery, 05-setup-judge, 06-worktree-and-session, 07-worker-execution, 13-observability-logs
