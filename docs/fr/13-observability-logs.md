---
refs:
  id: fr:13-observability-logs
  kind: fr
  title: "Observability Logs"
  spec: roki-mvp
  implements:
    - req:roki-mvp:11
  related:
    - ref:log-events
    - ref:cli
    - ref:config
    - fr:14-operator-notifications
    - fr:15-http-api
---

# FR 13: Observability Logs

> A single structured logging pipeline shared by the daemon, every gate, and the distill phase. Includes per-issue debug capture, surfacing of subprocess stderr, and secret redaction.
> See [`docs/reference/log-events.md`](../reference/log-events.md) for the full event list and the common context fields.

## Purpose

Even in stages without a dedicated UI, an operator must be able to diagnose the daemon's behavior, gate decisions, and the distill phase from structured logs alone. By making every spec route through the roki-mvp tracing pipeline rather than its own log destination, log aggregation and redaction are centralized. (Required as a base layer even when [15-http-api](15-http-api.md) / [16-roki-tui](16-roki-tui.md) exist.)

## User-visible Behavior

### Shared pipeline

- **A single tracing-crate-based pipeline**: every event passes through here.
- **Per-issue / per-worker / per-repo fields**: automatically attached based on tracing spans.
- **Correlation identifier**: one is allocated per worker invocation and attached to all related events.
- **Secret redaction**: Linear API token / webhook secret / Slack credentials / other operator-declared secrets are redacted before emit (each spec does not have its own redaction; it is shared).
- **Configurable destination**: stdout / file / both (`roki.toml`).
- **Configurable log level.**

### Where events come from and what they are

The **exact list** of events emitted by each spec lives in [`docs/reference/log-events.md`](../reference/log-events.md).
Here we only outline the conceptual categories:

- **roki-mvp**: worker lifecycle changes (including `Inactive(reason=...)` transitions), session/worktree operations, Linear poll/webhook, backoff/stall decisions, retry attempts, state-machine transitions, setup-judge completion, linear-updater dispatch / outcome / escalation queue updates, subprocess stderr lines.
- **Pre-implementation gate** ([08](08-pre-implementation-gate.md)): gate-evaluation start, spec-materialization turn start/end, per-attempt timeout, validation outcome, veto decision, escalation.
- **Pre-PR gate** ([09](09-pre-pr-gate.md)): gate decision (Allow / Deny+RetryWithContext / Deny exhausted), validation failure code, fix-finding context payload size, escalation.
- **HTTP API** ([15](15-http-api.md)): per-request event (method/path/status/duration/correlation), refresh request.

### Surfacing subprocess stderr

- The stderr of judge / worker / sweep subprocesses is surfaced as **one warn-severity structured event per line**.
- Tags: issue identifier + role (`judge` / `worker` / `sweep`) + correlation identifier.
- Empty lines are skipped.

### Per-issue debug capture (opt-in)

- Enabled by the `--debug` CLI flag (canonical reference: [01-daemon-lifecycle](01-daemon-lifecycle.md)) or a config block.
- When enabled, every line of stdout/stderr from each worker subprocess is appended to a per-issue file (under the debug-log directory; the file name is e.g. `<issue>.log`).
- Each line is tagged with an **RFC 3339 nanosecond timestamp + stream tag** (stdout/stderr).
- On write failure, log the offending path at warn severity and continue without stopping the worker.

### What is not logged

- **Request / response bodies** (HTTP API) — only metadata fields, so agent strings do not leak into logs.
- **Artifact contents** (distill phase) — only the artifact path and the manifest's structured fields.

## Capabilities

- **Single source for downstream**: each spec reuses the same pipeline + redaction instead of standing up its own destination.
- **Filter-friendly event names**: log consumers can filter by gate / distill / mvp etc.
- **Per-issue forensics**: debug capture lets you investigate a problematic ticket's subprocess after the fact.
- **Stderr never silently dropped**: as long as a subprocess emits non-empty stderr, it is logged.

## Boundaries

- **HTTP API / TUI** are out of scope ([15-http-api](15-http-api.md) / [16-roki-tui](16-roki-tui.md) own those).
- **Metrics / time-series** are out of scope (event log only).
- **Log retention / rotation** is the responsibility of external tools (logrotate, etc.).
- **Per-issue debug log analysis** is out of scope (the operator reads them).
- **Persistent gate / distill decision history** is not maintained (log retention belongs to external tooling).
- **Operator notification destinations** are a separate channel ([14-operator-notifications](14-operator-notifications.md)).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Optional HTTP API + ratatui TUI for observability" — base layer that the observability surface assumes
- **Requirements**:
  - `roki-mvp Req 11`: Observability of Daemon Internals
  - `roki-spec-gate Req 9`: Observability and Escalation
  - `roki-review-gate Req 8.5`: Decision logging
  - `roki-distill-postmerge Req 13`: Observability of the Distill Phase
- **Design**:
  - The Observability section of each spec's `design.md`
- **Related reference**: [log-events.md](../reference/log-events.md), [cli.md](../reference/cli.md) (`--debug`), [config.md](../reference/config.md) (log destination / level)
- **Related FR**: 14-operator-notifications, 15-http-api
