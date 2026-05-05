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
    - fr:20-rule-and-cycle-engine
    - fr:21-log-access
---

# FR 13: Observability Logs

> Three independent storage / access tiers cover everything operators need to inspect: a structured event log (daemon-wide tracing-crate JSON Lines), per-ticket subprocess raw captures (under the session tempdir), and an in-memory ring buffer that backs the HTTP API live event subscription. The previous single-pipeline model and the opt-in `--debug` per-issue capture are absorbed into these tiers.
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
- **Per-cycle / per-iter / per-ticket / per-repo fields**: automatically attached based on tracing spans. Cycle id (`cycle.id`), iteration (`cycle.iter`), phase (`pre` / `run` / `post`), ticket id, repo, and trigger (`webhook` / `cold_start`) are present where relevant.
- **Secret redaction**: Linear API token, webhook secret, and any operator-declared secrets in `roki.toml` are redacted before emit. Secrets in capture files (Tier 2) are not redacted because the daemon does not parse their contents — operators that want redacted captures route their cli line's output through their own redactor.
- **Read access**: `roki events` ([21-log-access](21-log-access.md)) reads from the HTTP API ring buffer (live tail / range filter) and, with `--offline --file <path>`, from a JSON Lines file directly.

### Tier 2: per-ticket subprocess raw captures

- **Layout**: `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{pre,run,post}.{stdout,stderr}`, plus parsed-derivative files (`pre.response.json`, `run.exit_code`, `run.terminal.json`, `post.response.json`) per [21-log-access §Storage layout](21-log-access.md).
- **Capture mode**: byte-for-byte. The daemon does not strip ANSI codes, does not redact, and does not impose a per-line tag.
- **Lifetime**: deleted on cleanup-cycle completion, on admission-eviction orphan cleanup, and on cold-start orphan reconcile (matches [06-worktree-and-session](06-worktree-and-session.md)).
- **Read access**: `roki log` (scope = same ticket); HTTP API mirrors via `GET /api/tickets/{id}/cycles/{cycle_id}/iters/{n}/{phase}/{stream}`.

### Tier 3: in-memory ring buffer

- **Scope**: a daemon-internal ring of the most recent N structured events (N = `roki.toml [log].ring_size`, default `1000`).
- **Use**: backs the HTTP API live event subscription (`GET /api/events`) and the TUI live view ([16-roki-tui](16-roki-tui.md)).
- **Loss on restart**: the ring buffer is in-memory only. After daemon restart, the ring is empty; older events are recovered from the file destination if `[log].destination = "file" | "both"`.

### What the daemon does **not** capture

- **HTTP API request / response bodies**: only metadata (method, path, status, duration). Agent strings, secrets in headers, and request payloads do not enter the event log.
- **Artifact contents** (`requirements.md`, `review.md`, etc.): the daemon does not parse artifacts, so it cannot leak them. Per-iter captures may contain artifact references the operator's cli line emitted; the operator owns redaction in that path.

### Surfacing subprocess stderr in the event log

For every phase iteration, the daemon emits one structured event when the subprocess exits, with summary fields including the truncated head/tail of stderr (configurable size; default first 256 bytes + last 256 bytes). The full stderr is in the per-iter capture file (Tier 2). The previous "one warn-severity event per stderr line" mode is removed because the per-iter capture already preserves every line for forensics, and per-line events flooded the structured stream.

## Capabilities

- **Three tiers**: structured event log (cross-ticket, JSON Lines), per-ticket capture (raw bytes), live ring buffer (HTTP / TUI). Each tier has a stable read interface.
- **Filter-friendly event names**: `roki events --kind <event_kind>` filters by canonical event names (`webhook_received`, `cycle_started`, `phase_started`, etc.).
- **Per-issue forensics**: `roki log --ticket <id> --cycle <uuid> ...` plus the structured event stream covers every observable aspect of a past cycle.
- **Stderr never silently dropped**: per-iter capture preserves all of it; structured event emits a head/tail summary plus a path to the capture file.
- **Configurable destination + level**: `roki.toml [log]` covers the daemon-wide settings; `--log-level` overrides level on the CLI.

## Boundaries

- **HTTP API / TUI** are out of scope here ([15-http-api](15-http-api.md) / [16-roki-tui](16-roki-tui.md) own those).
- **Metrics / time-series** are out of scope (event log only; if metrics are wanted, an external scraper consumes the event stream).
- **Log retention / rotation** is the responsibility of external tooling for the file destination. Per-ticket captures (Tier 2) are deleted by ticket lifecycle events ([06-worktree-and-session](06-worktree-and-session.md)) — there is no time-based retention for them.
- **Per-issue debug log analysis** is out of scope (operators read the captures).
- **Persistent observability history** beyond the file destination is out of scope; once an event scrolls off the file (after operator-managed rotation), it is gone.
- **Operator notification destinations** are a separate channel ([14-operator-notifications](14-operator-notifications.md)).
- **The `--debug` CLI flag** and the per-issue debug log directory are removed; per-iter captures fulfill the same role and are always on.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Optional HTTP API + ratatui TUI for observability" — base layer that the observability surface assumes.
- **Requirements**:
  - `roki-mvp Req 11`: Structured Logging.
- **Design**:
  - `Observability Pipeline` section of `.kiro/specs/roki-mvp/design.md` (pending rewrite).
- **Related reference**: [`docs/reference/log-events.md`](../reference/log-events.md) (pending rewrite for the new event catalog), [`docs/reference/config.md`](../reference/config.md) (`[log]` section).
- **Related FR**: [14-operator-notifications](14-operator-notifications.md), [15-http-api](15-http-api.md), [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md), [21-log-access](21-log-access.md).
