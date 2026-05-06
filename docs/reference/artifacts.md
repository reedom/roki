---
refs:
  id: ref:artifacts
  kind: reference
  title: "Public Artifacts"
  related:
    - fr:09-log-access-cli
    - fr:08-observability-logs
    - fr:04-phase-execution
    - fr:10-http-api
---

# Reference: Public Artifacts

Paths and required elements of public artifacts that the daemon writes. Operator-authored artifacts (anything a phase subprocess produces beyond the captures listed below) are out of scope — they live wherever the operator's cli line writes them.

## Storage layout

```
<session_root>/<ticket-id>/
  cycle-<uuid>/
    meta.json
    iter-001/
      pre.stdout
      pre.stderr
      pre.events.jsonl       (session-shape pre only)
      pre.response.json
      run.stdout
      run.stderr
      run.exit_code
      run.terminal.json      (when run cli emits stream-json `result`)
      post.stdout
      post.stderr
      post.events.jsonl      (session-shape post only)
      post.response.json
    iter-002/
      ...
```

`<session_root>` = `roki.toml [paths].session_root`. The on-disk layout is **not** a stable operator-facing contract; access goes through `roki log` / HTTP API. The daemon may switch backends in the future without breaking those CLIs.

## Artifact list

| Artifact | Path (under `<session_root>/<ticket-id>/cycle-<uuid>/`) | Writer | Reader | Purpose |
|---|---|---|---|---|
| `meta.json` | `meta.json` | daemon (cycle engine) | `roki log --meta`, `GET /api/tickets/{id}/cycles` | Per-cycle summary; durable independent of structured event log retention |
| `<phase>.stdout` / `<phase>.stderr` | `iter-<n>/<phase>.{stdout,stderr}` | daemon (subprocess capture) | `roki log --stream stdout` / `--stream stderr` | Byte-for-byte subprocess output |
| `<phase>.events.jsonl` | `iter-<n>/{pre,post}.events.jsonl` | daemon (event capture, session-shape pre/post only) | `roki log --stream events` | One parseable JSON object per line; advisory stream-json events the AI emits between turns |
| `<phase>.response.json` | `iter-<n>/{pre,post}.response.json` | daemon (terminal-directive capture) | `roki log --stream response` | The terminal JSON object the daemon parsed from this phase invocation |
| `run.terminal.json` | `iter-<n>/run.terminal.json` | daemon (when run cli speaks stream-json) | `roki log --stream terminal` | Parsed claude/codex `result` event |
| `run.exit_code` | `iter-<n>/run.exit_code` | daemon (post-`wait()`) | `roki log --stream exit_code` | Numeric run-phase exit code |

## Schema of `meta.json`

One JSON object per file. UTF-8, no trailing newline required. Written at `cycle_started` time (with `ended_at` / terminal fields null) and replaced atomically when the cycle ends. Readers tolerate intermediate states.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `cycle_id` | string (UUID v4) | yes | Cycle identifier matching the parent directory `cycle-<uuid>/` |
| `ticket_id` | string | yes | Linear issue identifier the cycle belongs to |
| `repo` | string | yes | Admission-resolved ghq path (e.g. `github.com/foo/bar`) |
| `cycle_kind` | enum | yes | `rule` / `cleanup` / `failure` |
| `cycle_trigger` | enum | yes | `runtime` (any runtime-detected diff: webhook delivery, polling fallback, or refresh nudge) / `cold_start` (daemon startup enumeration). Extensible. |
| `failed_cycle_id` | string (UUID v4) or null | when `cycle_kind == "failure"`, otherwise null | UUID of the cycle this failure handler is recovering |
| `started_at` | RFC 3339 timestamp | yes | UTC; matches the `cycle_started` event timestamp |
| `ended_at` | RFC 3339 timestamp or null | yes | Null while the cycle is in flight |
| `iter_count` | int | yes | Number of completed iterations |
| `terminal_directive` | enum or null | one of `terminal_directive` / `failure_kind` is non-null when `ended_at` is set | `run` / `end` / `pre` (the post directive that terminated the cycle), or null if the cycle ended through a failure |
| `failure_kind` | enum or null | see above | `process_crash` / `unparseable` / `schema_drift` / `repo_mismatch` / `fs_poison` / `stall` / `iter_exhausted` / `template_error`, or null if the cycle terminated cleanly |
| `failure_phase` | enum or null | non-null only when `failure_kind` is set | `pre` / `run` / `post` (always concrete for cycle-routed failures per [fr:02 §Recognized fields](../fr/02-configuration.md)) |

The Rust shape lives in the `roki-api-types` crate.

## Schema of `<phase>.response.json`

One JSON object per file. UTF-8, no trailing newline required. Written when the daemon parses a terminal directive from the phase's stdout (last JSON object).

| Field | Type | Required | Meaning |
|---|---|---|---|
| `directive` | enum | yes | Phase-specific legal set: pre returns `run` / `end`; post returns `pre` / `run` / `end` ([fr:01 §Directive schema](../fr/01-engine-model.md)) |
| `outcome` | string | no | Free-form operator string (TUI label, structured log discriminator). Daemon does not interpret |
| `repo` | string | no | (pre only) Admission-resolved repo for this ticket; daemon validates against `[[admission.repos]]` resolution |
| (any operator-defined field) | any JSON | no | Exposed to the next phase as `{{ pre.* }}` / `{{ post.* }}` Liquid variables; top-level scalars also exported as `ROKI_PRE_*` / `ROKI_POST_*` env vars per [fr:01 §Inter-phase data flow](../fr/01-engine-model.md) |

## Schema of `run.terminal.json`

When the run cli emits claude/codex stream-json, the daemon scans for the terminal `result` event mid-stream and writes it to this file. Other shapes leave the file absent.

| Field | Type | Required | Meaning |
|---|---|---|---|
| (the parsed `result` event verbatim) | JSON object | yes | The cli's terminal `result` event. Shape matches the cli's own output schema; the daemon does not impose a shape beyond extracting the event |

## Schema of `run.exit_code`

Plain integer (ASCII), no surrounding whitespace. Written after `wait()` returns regardless of cli shape.

## Schema of `<phase>.events.jsonl` (session-shape pre / post only)

One parseable JSON object per line. Carries the full advisory stream-json output (thinking blocks, tool-use messages, etc.) the AI emits between turns. The daemon does not interpret entries; they are forensic-only.

## When adding a new artifact

1. Add a row to the **Artifact list** table above.
2. Add a schema section if the artifact has structured contents.
3. Link to this reference from the FR page that uses it.

## Related reference

- [config.md](config.md): operator-facing configuration knobs (including `[paths].session_root`)
- [cli.md](cli.md): `roki log` flags for reading these artifacts
- [log-events.md](log-events.md): structured events emitted alongside artifact writes
