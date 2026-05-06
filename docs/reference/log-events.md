---
refs:
  id: ref:log-events
  kind: reference
  title: "Structured Log Events"
  related:
    - fr:08-observability-logs
    - fr:01-engine-model
    - fr:03-linear-admission
    - fr:06-failure-handling
    - fr:10-http-api
    - fr:12-daemon-lifecycle
---

# Reference: Structured Log Events

Canonical event names emitted on the daemon's tracing pipeline. The pipeline + redaction layer + ring buffer are described in [fr:08](../fr/08-observability-logs.md). All events flow through the same JSON Lines stream; consumers filter via `roki events --kind <event_kind>`.

## Common context fields

Attached to every event via tracing spans (when in scope).

| Field | Type | Attached when |
|---|---|---|
| `ticket.id` | string | per-ticket scoped event |
| `repo` | string (ghq path) | repo-scoped event |
| `cycle.id` | string (UUID v4) | per-cycle event |
| `cycle.kind` | enum (`rule` / `cleanup` / `failure`) | per-cycle event |
| `cycle.trigger` | enum (`runtime` / `cold_start`) | per-cycle event |
| `cycle.iter` | int (1-indexed) | per-iteration event |
| `phase` | enum (`pre` / `run` / `post`) | per-phase event |

`cycle.trigger = runtime` covers webhook delivery, polling fallback, and refresh nudge driven cycles; sub-source detail surfaces via separate event kinds (`webhook_received`, `polling_started`, `refresh_received`).

## Cycle engine events

| Event | When | Carries |
|---|---|---|
| `cycle_started` | Cycle begins | `cycle.kind`, `cycle.trigger`, matched entry index |
| `phase_started` | Phase subprocess spawned | `phase`, cli line (Liquid-rendered, secrets-redacted), env var keys, working directory |
| `phase_completed` | Phase clean exit | `phase`, exit code, duration, terminal directive (when applicable), head/tail summary of stderr |
| `phase_failed` | Phase failure | `phase`, `failure.kind` per [fr:01 §Failure handling](../fr/01-engine-model.md), `error_text`, head/tail summary of stderr |
| `failure_unhandled` | A `phase_failed` failure had no `[[on_failure]]` first-match | `(ticket.id, cycle.id, failure.kind, phase, error_text)`. The escalation queue is **not** touched ([fr:06 §Failure-handler cycle](../fr/06-failure-handling.md)) |
| `cycle_completed` | Cycle ends with terminal directive | `cycle.kind`, terminal directive, iter count, duration |
| `cycle_aborted` | Cycle aborted (failure or admission lost mid-cycle) | `cycle.kind`, `failure.kind` (if applicable), iter count |
| `escalation_added` | Escalation queue entry added | Daemon-stuck failures only: failure-handler cycle that itself failed, or daemon-internal error with no cycle association ([fr:06 §Escalation queue](../fr/06-failure-handling.md)) |

## Worktree / session lifecycle

| Event | When | Carries |
|---|---|---|
| `worktree_created` | Worktree materialized on first `pre.directive: "run"` | `repo`, branch name, worktree path |
| `worktree_deleted` | Worktree removed (cleanup-cycle completion / admission eviction / orphan reconcile) | `repo`, branch name, `reason` ∈ `cleanup` / `eviction` / `orphan` |
| `session_tempdir_created` | Session tempdir created at admission | `ticket.id`, path |
| `session_tempdir_deleted` | Session tempdir removed | `ticket.id`, path, `reason` |

## Cold start

| Event | When | Carries |
|---|---|---|
| `cold_start_began` | Daemon process start, after config validation | `roki.toml` path, WORKFLOW.toml path |
| `cold_start_completed` | Cold-start enumeration + reconciliation finished | Total tickets enumerated, cycles spawned, orphans deleted |

## Linear admission

| Event | When | Carries |
|---|---|---|
| `webhook_received` | Linear webhook arrives | Verified flag, payload kind |
| `webhook_skipped` | Admission failed or no diff | `reason` ∈ `signature_invalid` / `assignee_mismatch` / `repo_unresolvable` / `no_diff` |
| `polling_started` | Outage-driven polling cycle started | `reason` ∈ `webhook_outage` (only outage-driven; nudge-driven uses `refresh_received`) |
| `polling_completed` | Polling pass finished | Tickets fetched, diffs detected |
| `refresh_received` | `POST /api/refresh` arrived | Client address, coalescing decision (`fired` / `coalesced` / `dropped_during_backoff`) |
| `linear_backoff_applied` | HTTP 429 received from Linear | Backoff window seconds |

## HTTP API

| Event | When | Carries |
|---|---|---|
| `api_disabled` | `[api].port` unset at startup | Info severity; no socket bound |
| `api_listening` | API server bound | Bind addr, port |
| `api_bind_failure` | API server failed to bind | Port, OS error. Daemon continues without API |
| `api_non_loopback_warn` | `[api].bind` resolves outside `127.0.0.0/8` and `::1/128` | Bind host (warns about absent authentication) |
| `api_request` | Per-request metadata | Method, path, response status, duration, client address, correlation id (no body) |

## Daemon lifecycle

| Event | When | Carries |
|---|---|---|
| `daemon_started` | After config load + validation, before cold start | Config path, schema version |
| `daemon_ready` | All subsystems up + cold start complete | Webhook receiver bind, API bind (if enabled) |
| `daemon_shutdown_began` | SIGINT / SIGTERM received | Active cycle count |
| `daemon_shutdown_completed` | Graceful exit | Cycles drained count |
| `shutdown_window_exceeded` | Warn-severity event when one or more in-flight subprocesses failed to drain inside the shutdown window | Offending subprocess descriptors |

## Per-iteration capture (Tier 2)

Every phase subprocess writes byte-for-byte stdout / stderr to `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{phase}.{stdout,stderr}` plus parsed-derivative files. **Not part of the structured event log** ([fr:08 §Tier 2](../fr/08-observability-logs.md)) — the structured event log emits a head/tail summary on `phase_completed` / `phase_failed`. The full bytes are accessible via `roki log` ([fr:09](../fr/09-log-access-cli.md)).

## What the daemon does **not** log

- **HTTP API request / response bodies**: only metadata fields per `api_request`.
- **Subprocess advisory output** (claude stream-json thinking turns, tool-use messages, etc.): captured to `<phase>.events.jsonl` (Tier 2) only; never parsed by the daemon.
- **Operator-defined post-directive payload contents** (beyond the `directive` value): captured to `<phase>.response.json` (Tier 2). The structured event records the directive value but not arbitrary operator fields.
- **Secrets**: Linear API token, webhook secret, and any `roki.toml` secret values are redacted before emit.

## When adding a new event

1. Add a row to the corresponding category table above.
2. Document any fields beyond the common context fields here.
3. Link to this reference from the FR page that uses it.

## Related reference

- [cli.md](cli.md): flags that change logging behavior (`--log-level`)
- [config.md](config.md): `[log]` block — destination / level / ring size
- [artifacts.md](artifacts.md): per-iter capture artifacts that are not part of the structured log
