---
refs:
  id: fr:08-observability-logs
  kind: fr
  title: "Observability Logs"
  spec: roki-skeleton
  related:
    - ref:log-events
    - ref:cli
    - ref:config
    - fr:06-failure-handling
    - fr:10-http-api
    - fr:01-engine-model
    - fr:09-log-access-cli
---

# FR 08: Observability Logs

> Three independent storage / access tiers cover everything operators need to inspect: a structured event log (daemon-wide tracing-crate JSON Lines), per-ticket subprocess raw captures (under the session tempdir), and an in-memory ring buffer that backs the HTTP API live event subscription.
> See [`docs/reference/log-events.md`](../reference/log-events.md) for the full event list and the common context fields.

## Purpose

Operators must diagnose daemon behavior, admission decisions, cycle outcomes, and per-phase subprocess output from inspecting these three tiers alone. There is no separate debug mode and no separate "per-issue debug log directory": per-iter raw captures are always on disk under the session tempdir; structured events always flow through tracing.

## User-visible Behavior

### Tier 1: structured event log

- **Pipeline**: a single tracing-crate-based pipeline. Every daemon-internal event passes through here.
- **Format**: JSON Lines. Each line is one structured event with timestamp, span/correlation context, and event-kind-specific fields.
- **Destination**: configurable in `roki.toml [log]`:
  - `destination = "stdout"` (default) → daemon writes to stdout; operator pipes through systemd / launchd / external log-rotation tooling.
  - `destination = "file"` → daemon appends to `[log].file_path`. The daemon does not rotate; rotation is operator-managed (logrotate, journald, etc.).
  - `destination = "both"` → both stdout and the file.
- **Level**: configurable via `[log].level` and the `--log-level` CLI override.
- **Per-cycle / per-iter / per-ticket / per-repo fields**: automatically attached based on tracing spans. Cycle id (`cycle.id`), iteration (`cycle.iter`), phase (`pre` / `run` / `post`), ticket id, repo, and trigger (`runtime` / `cold_start`) are present where relevant.
- **Secret redaction**: Linear API token, webhook secret, and any operator-declared secrets in `roki.toml` are redacted before emit. Secrets in capture files (Tier 2) are not redacted because the daemon does not parse their contents — operators that want redacted captures route their cli line's output through their own redactor.
- **Read access**: `roki events` ([09-log-access-cli](09-log-access-cli.md)) reads from the HTTP API ring buffer (live tail / range filter) and, with `--offline --file <path>`, from a JSON Lines file directly.

### Tier 2: per-ticket subprocess raw captures

- **Layout**: `<session_root>/<ticket-id>/cycle-<uuid>/visit-<n>/<state_id>.{stdout,stderr}`, plus parsed-derivative files (`<state_id>.exit_code`, `<state_id>.terminal.json`, `<state_id>.directive.json`) and the per-line stream-json event files when the cli line emits them (`<state_id>.events.jsonl`) per [09-log-access-cli §Storage layout](09-log-access-cli.md).
- **Capture mode**: byte-for-byte. The daemon does not strip ANSI codes, does not redact, and does not impose a per-line tag.
- **Lifetime**: deleted on cleanup-cycle completion and on cold-start orphan reconcile (matches [05-worktree-and-session](05-worktree-and-session.md)). Admission-revoke does not delete the captures — the directory is retained until cleanup-cycle reclaim or orphan reconcile.
- **Read access**: `roki log` (scope = same ticket); HTTP API mirrors via `GET /api/tickets/{id}/cycles/{cycle_id}/visits/{n}/{state_id}/{stream}`.

### Tier 3: in-memory ring buffer

- **Scope**: a daemon-internal ring of the most recent N structured events (N = `roki.toml [log].ring_size`; canonical default in [`docs/reference/config.md`](../reference/config.md)).
- **Use**: backs the HTTP API live event subscription (`GET /api/events`) and the TUI live view ([11-roki-tui](11-roki-tui.md)).
- **Loss on restart**: the ring buffer is in-memory only. After daemon restart, the ring is empty; older events are recovered from the file destination if `[log].destination = "file" | "both"`.

### Event catalog (MVP)

Canonical event names emitted on the structured pipeline. `roki events --kind <name>` filters on these. The full field schema lives in [`docs/reference/log-events.md`](../reference/log-events.md).

| Event | When |
|---|---|
| `webhook_received` | Webhook arrives |
| `webhook_skipped` | Admission failed or the diff produced no rule match |
| `cycle_started` | Cycle begins (`cycle.kind` ∈ `rule` / `cleanup` / `failure`) |
| `phase_started` | Phase subprocess spawned |
| `phase_completed` | Phase clean exit; carries head/tail stderr summary |
| `phase_failed` | Phase failure (`failure.kind` per [01-engine-model](01-engine-model.md) §Failure kinds) |
| `failure_unhandled` | A cycle failure with no ``on_failure:` entries` match (`marker = none`). Carries `(ticket_id, cycle_id, cycle_kind, failure.kind, phase, error_text, marker)`. Daemon stays alive; the ticket task drops the cycle and waits for the next admission ([06-failure-handling §Failure-handler cycle](06-failure-handling.md)). Recursive failure-cycle failures and cleanup-time fs errors enter the escalation queue instead — see `escalation_added`. |
| `cycle_completed` | Cycle ends with terminal directive |
| `cycle_aborted` | Cycle aborted (failure or admission lost mid-cycle) |
| `escalation_added` | Escalation queue entry added. Daemon-stuck failure: failure-handler cycle that itself failed, cleanup-time fs error, or daemon-internal error with no cycle association. Carries `(ticket_id?, cycle_id?, failure.kind, phase?, error_text)`. Cycle-less entries omit `ticket_id`, `cycle_id`, `phase` ([06-failure-handling §Escalation queue](06-failure-handling.md)) |
| `worktree_created` / `worktree_deleted` | Worktree lifecycle |
| `cold_start_began` / `cold_start_completed` | Daemon startup reconciliation |

Subprocess advisory output (claude `stream-json` thinking turns, etc.) is not parsed by the daemon. It is captured as raw stdout / stderr in Tier 2 and accessible via `roki log`.

### What the daemon does **not** capture

- **HTTP API request / response bodies**: only metadata (method, path, status, duration). Agent strings, secrets in headers, and request payloads do not enter the event log.
- **Artifact contents** (`requirements.md`, `review.md`, etc.): the daemon does not parse artifacts, so it cannot leak them. Per-iter captures may contain artifact references the operator's cli line emitted; the operator owns redaction in that path.

### Surfacing subprocess stderr in the event log

For every phase iteration, the daemon emits one structured event when the subprocess exits, with summary fields including a truncated head + tail slice of stderr (size configurable in `roki.toml [log]`; canonical key names and default sizes in [`docs/reference/config.md`](../reference/config.md)). The full stderr is in the per-iter capture file (Tier 2).

## Capabilities

- **Three tiers**: structured event log (cross-ticket, JSON Lines), per-ticket capture (raw bytes), live ring buffer (HTTP / TUI). Each tier has a stable read interface.
- **Filter-friendly event names**: `roki events --kind <event_kind>` filters by canonical event names (`webhook_received`, `cycle_started`, `phase_started`, etc.).
- **Per-issue forensics**: `roki log --ticket <id> --cycle <uuid> ...` plus the structured event stream covers every observable aspect of a past cycle.
- **Stderr never silently dropped**: per-iter capture preserves all of it; structured event emits a head/tail summary plus a path to the capture file.
- **Configurable destination + level**: `roki.toml [log]` covers the daemon-wide settings; `--log-level` overrides level on the CLI.

## Boundaries

- **HTTP API / TUI** are out of scope here ([10-http-api](10-http-api.md) / [11-roki-tui](11-roki-tui.md) own those).
- **Metrics / time-series** are out of scope (event log only; if metrics are wanted, an external scraper consumes the event stream).
- **Log retention / rotation** is the responsibility of external tooling for the file destination. Per-ticket captures (Tier 2) are deleted by ticket lifecycle events ([05-worktree-and-session](05-worktree-and-session.md)) — there is no time-based retention for them.
- **Per-issue debug log analysis** is out of scope (operators read the captures).
- **Persistent observability history** beyond the file destination is out of scope; once an event scrolls off the file (after operator-managed rotation), it is gone.
- **Operator notification destinations** are a separate channel ([06-failure-handling](06-failure-handling.md)).
- **A `--debug` CLI flag and a per-issue debug log directory** are out of scope; per-iter captures cover the same need and are always on.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Optional HTTP API + ratatui TUI for observability" — base layer that the observability surface assumes.
- **Requirements**:
  - `roki-mvp Req 11`: Structured Logging.
- **Related reference**: [`docs/reference/log-events.md`](../reference/log-events.md) (pending rewrite for the new event catalog), [`docs/reference/config.md`](../reference/config.md) (`[log]` section).
- **Related FR**: [06-failure-handling](06-failure-handling.md), [10-http-api](10-http-api.md), [01-engine-model](01-engine-model.md), [09-log-access-cli](09-log-access-cli.md).
