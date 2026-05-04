---
refs:
  id: ref:log-events
  kind: reference
  title: "Structured Log Events"
  related:
    - fr:13-observability-logs
---

# Reference: Structured Log Events

Structured log events that roki emits. All events flow through roki-mvp's single tracing pipeline + redaction layer ([13-observability-logs](../fr/13-observability-logs.md)).

## Common context fields

Attached to every event via spans.

| Field | Type | Attached when |
|---|---|---|
| issue identifier | string | per-issue scoped event |
| repository identifier | string | repo-scoped event (e.g. worktree create / cleanup) |
| subprocess invocation correlation identifier | string | per-subprocess event |
| subprocess role | `orchestrator` / `phase` / `sweep` | event from a subprocess |
| phase | `materialize_spec` / `implement` / `review` / `validate` / `open_pr` / `ci_fix` / `finalize_review` | event from a phase subprocess (when role = `phase`) |

## Events emitted by roki-mvp

| Event | Summary | Used by | Requirements |
|---|---|---|---|
| Orchestrator session start | Per ticket on `Discovered → Pending`; logs the rendered `extension.orchestrator.{model, effort, max_phases, allowed_tools}` snapshot used at launch | [19-orchestrator-session](../fr/19-orchestrator-session.md), [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 5.1, Req 11.1 |
| Orchestrator session stop | Graceful exit on terminal `Inactive(reason=*)` (after any orchestrator-driven Linear writes for that terminal state have completed) | [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 5.11, Req 11.1, Req 11.8 |
| Orchestrator turn | Per orchestrator response: `(issue, turn_index, action, phase or null, judge or null, outcome or null, reason)`; the `reason` field is bounded and redacted when long | [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 5.2, Req 11.1 |
| Orchestrator schema drift | The orchestrator's response failed JSON schema validation; counter-incremented; on a second drift after one daemon-side reprompt the issue lands `Inactive(reason=orchestrator_unparseable)` | [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 5.4, Req 12.3 |
| Daemon directive sent | Every `daemon_directive` event the daemon writes on the orchestrator's stdin (kind, structured fields, payload size) — the directive itself is never logged with secrets per `Req 12.5` | [14-operator-notifications](../fr/14-operator-notifications.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 12.2, Req 12.5 |
| Linear write applied | Per Linear write the orchestrator reports back via the `linear_writes` field on its `admission_decision` (rejection variant) or `linear_update_done` response; partial writes are logged distinctly | [14-operator-notifications](../fr/14-operator-notifications.md) | roki-mvp Req 12.7, Req 11.1 |
| Phase subprocess lifecycle change | Each phase subprocess lifecycle change (launch / clean exit / non-clean exit / signal / `--max-turns` exhaustion). The raw exit envelope is captured here; the orchestrator's resulting decision is captured by the next `Orchestrator turn` event | [07-worker-execution](../fr/07-worker-execution.md), [18-worker-skill-workflow](../fr/18-worker-skill-workflow.md) | roki-mvp Req 5.6, Req 5.8, Req 11.1 |
| Phase subprocess unknown subtype | The terminal `result.subtype` is not in the daemon's compiled mapping; raw subtype captured and forwarded to the orchestrator in the matching `phase_nonclean` event (the daemon does not unilaterally route to `Inactive`) | [07-worker-execution](../fr/07-worker-execution.md) | roki-mvp Req 5.9, Req 11.1 |
| Session tempdir create / delete | Session tempdir operations | [06-worktree-and-session](../fr/06-worktree-and-session.md) | roki-mvp Req 11.1 |
| Worktree create / remove | Worktree operations | [06-worktree-and-session](../fr/06-worktree-and-session.md) | roki-mvp Req 11.1, Req 11.2 |
| Linear poll | Tracker polling | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 11.1 |
| Webhook receipt | Tracker webhook received | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 11.1 |
| Pre-admission skipped | Issue rejected by the silent-skip judge before any state entry; carries `reason` ∈ `assignee_mismatch` / `state_not_admitted` / `missing_roki_ready` / `roki_impl_without_roki_ready` | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 3.7, Req 3.8, Req 3.9, Req 11.1 |
| Backoff decision | 429 backoff applied | [03-linear-integration](../fr/03-linear-integration.md) | roki-mvp Req 3.4, Req 11.1 |
| Stall decision | Stall detected (per-phase or orchestrator) | [07-worker-execution](../fr/07-worker-execution.md), [19-orchestrator-session](../fr/19-orchestrator-session.md) | roki-mvp Req 5.3, Req 5.7, Req 11.1 |
| Retry attempt | Retry with attempt counter (ticket-level, between `phase_nonclean → run_phase` cycles the orchestrator drives) | [07-worker-execution](../fr/07-worker-execution.md) | roki-mvp Req 5.10, Req 11.1 |
| State-machine transition | Per-issue state transition (prev / next / trigger source / `Inactive.reason` when transitioning to `Inactive`); `reason` may be any of the discriminator values including the three orchestrator-dead values `orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted` | [04-state-machine-and-recovery](../fr/04-state-machine-and-recovery.md) | roki-mvp Req 8.1, Req 8.2, Req 11.1, Req 12.3 |
| Subprocess stderr line | One stderr line of orchestrator / phase / sweep subprocess = one warn event tagged with the subprocess role and (for phases) the phase name | [13-observability-logs](../fr/13-observability-logs.md) | roki-mvp Req 11.5 |

## Events emitted by orchestrator-driven artifact validation

The orchestrator's structural validation of `requirements.md` and `review.md` runs inside the orchestrator's own session ([19-orchestrator-session](../fr/19-orchestrator-session.md) §Artifact validation). Each validation outcome surfaces through the existing `Orchestrator turn` event (the `action` / `phase` / `additional_context` / `reason` fields) plus the `Phase subprocess lifecycle change` event for the producing phase. There are no dedicated gate events because there is no daemon-side gate; the prior `roki-spec-gate` and `roki-review-gate` event tables are removed.

## Events emitted by the roki-mvp escalation queue

The prior linear-updater subagent's `dispatch` / `outcome` events are removed alongside the subagent itself. Linear-write side effects are now logged on the orchestrator-session side: the daemon emits `Daemon directive sent` when it writes a `daemon_directive` to the orchestrator's stdin, and `Linear write applied` when the orchestrator reports back via its `linear_writes` field (see "Events emitted by roki-mvp" above).

| Event | Summary | Used by | Requirements |
|---|---|---|---|
| Escalation queue update | issue id + failure category + structured fields + (set / cleared); populated for both orchestrator-alive (`daemon_directive` sent) and orchestrator-dead (no Linear write) paths | [14-operator-notifications](../fr/14-operator-notifications.md) | roki-mvp Req 12.1, Req 12.3 |

## Events emitted by the roki-observability HTTP server

| Event | Summary | Used by | Requirements |
|---|---|---|---|
| HTTP request | method / path / response status / request duration / client address / per-request correlation identifier | [15-http-api](../fr/15-http-api.md) | roki-observability Req 14.1 |
| Refresh request | Client address + coalescing decision | [15-http-api](../fr/15-http-api.md) | roki-observability Req 4.5 |

## Per-issue debug capture (opt-in)

Enabled by the `--debug` CLI flag or a config block ([cli.md](cli.md)).

- Append **every line** of each subprocess's (orchestrator session and every phase subprocess) stdout/stderr to a per-issue file (under the debug-log directory).
- Tag each line with an **RFC 3339 nanosecond timestamp + stream tag** (stdout/stderr).
- On write failure, log the offending path at warn severity and continue without stopping the subprocess.
- Used by: [13-observability-logs](../fr/13-observability-logs.md)
- Requirements: roki-mvp Req 11.6, Req 11.7

## What is not logged

- **Request / response bodies** (HTTP API) — only metadata fields, so agent strings do not leak into logs
- **`daemon_directive` directive prose** — the directive carries the issue id + `kind` + structured fields; the Linear label name(s) and comment text the orchestrator composes (driven by `prompt_template_orchestrator`) are not reflected back into the daemon log beyond the `linear_writes` summary the orchestrator returns
- **The orchestrator's reasoning / extended-thinking text** — only the parsed JSON action (and bounded `reason` field) is logged per turn
- **Secret strings** — Linear API token / webhook secret and similar values are redacted before emit

## When adding a new event

1. Add a row to the relevant section's table.
2. Document any fields beyond the common context fields here.
3. Link to this reference from the FR pages that use it.
4. Update the corresponding requirements.

## Related reference

- [cli.md](cli.md): flags that change logging behavior (e.g. `--debug`)
- [config.md](config.md): log destination / level configuration
- [artifacts.md](artifacts.md): artifact contents that are not logged
