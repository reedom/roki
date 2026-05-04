---
refs:
  id: ref:log-events
  kind: reference
  title: "Structured Log Events"
  related:
    - fr:13-observability-logs
---

# Reference: Structured Log Events

The **canonical reference** for the structured log events that roki emits.
Every event flows through roki-mvp's single tracing pipeline + redaction layer ([13-observability-logs](../fr/13-observability-logs.md)).

## Common context fields

Fields automatically attached to every event via spans.

| Field | Type | Attached when |
|---|---|---|
| issue identifier | string | per-issue scoped event |
| repository identifier | string | repo-scoped event (e.g. worktree create / cleanup, setup-judge findings) |
| worker invocation correlation identifier | string | per-worker event |
| subprocess role | `judge` / `worker` / `sweep` | event from a subprocess |

## Events emitted by roki-mvp

| Event | Summary | Used by | Requirements |
|---|---|---|---|
| Worker lifecycle change | Each worker lifecycle change | [07-worker-execution](../fr/07-worker-execution.md) | roki-mvp Req 11.1 |
| Session tempdir create / delete | Session tempdir operations | [06-worktree-and-session](../fr/06-worktree-and-session.md) | roki-mvp Req 11.1 |
| Worktree create / remove | Worktree operations | [06-worktree-and-session](../fr/06-worktree-and-session.md) | roki-mvp Req 11.1, Req 11.2 |
| Linear poll | Tracker polling | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 11.1 |
| Webhook receipt | Tracker webhook received | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 11.1 |
| Backoff decision | 429 backoff applied | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 3.4, Req 11.1 |
| Stall decision | Stall detected | [07-worker-execution](../fr/07-worker-execution.md) | roki-mvp Req 5.3, Req 11.1 |
| Retry attempt | Retry with attempt counter | [07-worker-execution](../fr/07-worker-execution.md) | roki-mvp Req 5.6, Req 11.1 |
| State-machine transition | Per-issue state transition (prev / next / trigger source) | [04-state-machine-and-recovery](../fr/04-state-machine-and-recovery.md) | roki-mvp Req 8.2, Req 11.1 |
| Setup judge completion | Success / retry / final failure (duration, parsed action, validated repos or rejection reason) | [05-setup-judge](../fr/05-setup-judge.md) | roki-mvp Req 11.8 |
| Subprocess stderr line | One stderr line of judge / worker / sweep = one warn event | [13-observability-logs](../fr/13-observability-logs.md) | roki-mvp Req 11.5 |

## Events emitted by roki-spec-gate

| Event | Summary | Used by | Requirements |
|---|---|---|---|
| Gate-evaluation start | Gate evaluation begins | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-spec-gate Req 9.1 |
| Spec-materialization turn start / end | Boundaries of the constrained turn | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-spec-gate Req 9.1 |
| Per-attempt timeout | Timeout detected | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-spec-gate Req 4.1, Req 9.1 |
| Validation outcome | Verdict + machine-readable reason | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-spec-gate Req 3.5, Req 9.1 |
| Veto decision | The allow/deny returned to the orchestrator | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-spec-gate Req 9.1 |
| Escalation | On cap exhaustion (`(repo, issue)` / final attempt index / final reason / applied `required_status`) | [08-pre-implementation-gate](../fr/08-pre-implementation-gate.md) | roki-spec-gate Req 4.5, Req 9.4 |

## Events emitted by roki-review-gate

| Event | Summary | Used by | Requirements |
|---|---|---|---|
| Gate decision | `(repo, issue)` + correlation identifier (review turn) + attempt counter + decision | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-review-gate Req 8.5 |
| Veto / escalation | Failing reason + on escalation (cap exhausted / `fail-missing-spec`) | [09-pre-pr-gate](../fr/09-pre-pr-gate.md) | roki-review-gate Req 8.2, Req 8.5 |

## Events emitted by the roki-mvp linear-updater subagent

| Event | Summary | Used by | Requirements |
|---|---|---|---|
| linear-updater dispatch | issue id + directive `kind` + structured fields + correlation id | [14-operator-notifications](../fr/14-operator-notifications.md) | roki-mvp Req 5.10, Req 11.1 |
| linear-updater outcome | success / non-clean exit / Linear API error / MCP unavailable + retry decision | [14-operator-notifications](../fr/14-operator-notifications.md) | roki-mvp Req 5.10, Req 11.8 |
| Escalation queue update | issue id + failure category + structured fields + (set / cleared) | [14-operator-notifications](../fr/14-operator-notifications.md) | roki-mvp Req 12.1 |

## Events emitted by the roki-observability HTTP server

| Event | Summary | Used by | Requirements |
|---|---|---|---|
| HTTP request | method / path / response status / request duration / client address / per-request correlation identifier | [15-http-api](../fr/15-http-api.md) | roki-observability Req 14.1 |
| Refresh request | Client address + coalescing decision | [15-http-api](../fr/15-http-api.md) | roki-observability Req 4.5 |

## Per-issue debug capture (opt-in)

Enabled by the `--debug` CLI flag or a config block ([cli.md](cli.md)).

- Append **every line** of each worker subprocess's stdout/stderr to a per-issue file (under the debug-log directory).
- Tag each line with an **RFC 3339 nanosecond timestamp + stream tag** (stdout/stderr).
- On write failure, log the offending path at warn severity and continue without stopping the worker.
- Used by: [13-observability-logs](../fr/13-observability-logs.md)
- Requirements: roki-mvp Req 11.6, Req 11.7

## What is not logged

- **Request / response bodies** (HTTP API) — only metadata fields, so agent strings do not leak into logs
- **Linear-updater directive prose** — the directive carries the issue id + `kind` + structured fields; comment text composed by the linear-updater agent is not reflected back into the daemon log
- **Secret strings** — Linear API token / webhook secret and similar values are redacted before emit

## When adding a new event

1. Add a row to the relevant section's table.
2. If there are dedicated fields beyond the common context fields, document them here as well.
3. Link to this reference from the FR pages that use it.
4. Update the corresponding requirements.

## Related reference

- [cli.md](cli.md): flags that change logging behavior, such as `--debug`
- [config.md](config.md): log destination / level configuration
- [artifacts.md](artifacts.md): artifact contents that are not logged
