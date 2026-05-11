---
refs:
  id: ref:artifacts
  kind: reference
  title: "Public Artifacts"
  related:
    - fr:09-log-access-cli
    - fr:08-observability-logs
    - fr:04-state-execution
    - fr:10-http-api
---

# Reference: Public Artifacts

Paths and required elements of public artifacts the daemon writes. Operator-authored artifacts (anything a state subprocess produces beyond the captures listed below) are out of scope — they live wherever the operator's cli line writes them.

## Storage layout

```
<session_root>/<ticket-id>/
  cycle-<uuid>/
    meta.json
    visit-001/
      <state_id>.stdout
      <state_id>.stderr
      <state_id>.events.jsonl    (when cli emits stream-json)
      <state_id>.terminal.json   (when cli emits stream-json `result`)
      <state_id>.directive.json  (when sentinel file present at exit)
      <state_id>.exit_code
    visit-002/
      ...
```

`<session_root>` = `roki.toml [paths].session_root`. `<state_id>` is the operator-declared id from `WORKFLOW.yaml`; visit numbering is cycle-wide (every state spawn increments `visit_n`). The on-disk layout is **not** a stable operator-facing contract; access goes through `roki log` / HTTP API. The daemon may switch backends in the future without breaking those CLIs.

## Artifact list

| Artifact | Path (under `<session_root>/<ticket-id>/cycle-<uuid>/`) | Writer | Reader | Purpose |
|---|---|---|---|---|
| `meta.json` | `meta.json` | daemon (cycle engine) | `roki log --meta`, `GET /api/tickets/{id}/cycles` | Per-cycle summary; durable independent of structured event log retention |
| `cycle.json` | `cycle.json` | daemon (`daemon::cycle_metadata`) | `GET /api/tickets/{id}/cycles` ([fr:10](../fr/10-http-api.md)) | Cycle metadata: `kind`, `trigger`, `started_at`, `ended_at`, `terminal_id`, `failure_kind`, `total_visits`, declared `states[]`. Atomic write at cycle start (`ended_at: null`); atomic update at cycle end |
| `<state_id>.stdout` / `<state_id>.stderr` | `visit-<n>/<state_id>.{stdout,stderr}` | daemon (subprocess capture) | `roki log --stream stdout` / `--stream stderr` | Byte-for-byte subprocess output |
| `<state_id>.events.jsonl` | `visit-<n>/<state_id>.events.jsonl` | daemon (event capture, when cli emits stream-json) | `roki log --stream events` | One parseable JSON object per line; advisory stream-json events the cli emits between turns |
| `<state_id>.directive.json` | `visit-<n>/<state_id>.directive.json` | daemon (sentinel-file copy at exit) | `roki log --stream directive` | The sentinel JSON the operator's subprocess wrote to `$ROKI_DIRECTIVE_PATH` before exit |
| `<state_id>.terminal.json` | `visit-<n>/<state_id>.terminal.json` | daemon (when cli speaks stream-json) | `roki log --stream terminal` | Parsed claude/codex `result` event |
| `<state_id>.exit_code` | `visit-<n>/<state_id>.exit_code` | daemon (post-`wait()`) | `roki log --stream exit_code` | Numeric subprocess exit code |

## Schema of `meta.json`

One JSON object per file. UTF-8, no trailing newline required. Written at `cycle_started` time (with `ended_at` / terminal fields null) and replaced atomically when the cycle ends. Readers tolerate intermediate states.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `cycle_id` | string (UUID v4) | yes | Cycle identifier matching the parent directory `cycle-<uuid>/` |
| `ticket_id` | string | yes | Linear issue identifier the cycle belongs to |
| `repo` | string | yes | Admission-resolved ghq path (e.g. `github.com/foo/bar`) |
| `cycle_kind` | enum | yes | `rule` / `cleanup` / `failure` |
| `cycle_trigger` | enum | yes | `runtime` (any runtime-detected diff: webhook delivery, polling fallback, or refresh nudge) / `cold_start` (daemon startup enumeration). Extensible |
| `failed_cycle_id` | string (UUID v4) or null | when `cycle_kind == "failure"`, otherwise null | UUID of the cycle this failure handler is recovering |
| `started_at` | RFC 3339 timestamp | yes | UTC; matches the `cycle_started` event timestamp |
| `ended_at` | RFC 3339 timestamp or null | yes | Null while the cycle is in flight |
| `iter_count` | int | yes | Total state-visits across the cycle (matches `cycle.iter` at completion) |
| `terminal_id` | string or null | one of `terminal_id` / `failure_kind` is non-null when `ended_at` is set | The terminal state id the cycle landed at (e.g. `__success__`, `__failure__`, or operator-declared) |
| `outcome` | string or null | non-null when `terminal_id` is set | The terminal's `outcome` string (terminal-declared or sentinel-overridden) |
| `failure_kind` | enum or null | see above | `process_crash` / `unparseable` / `schema_drift` / `fs_poison` / `stall` / `recursion_bound` / `template_error`, or null if the cycle terminated at a terminal |
| `failure_state_id` | string or null | non-null only when `failure_kind` is set | State id that emitted the failure |
| `failure_visit_n` | int or null | non-null only when `failure_kind` is set | Visit count of the failing state at failure time |

The Rust shape lives in the `roki-api-types` crate.

## Schema of `<state_id>.directive.json`

One JSON object per file. UTF-8, no trailing newline required. Copy of the sentinel file at `$ROKI_DIRECTIVE_PATH` after the subprocess exits.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `directive` | string | yes | Operator-supplied directive name. Resolved against the state's `directives:` map ∪ built-in defaults ([fr:01 §Directive schema](../fr/01-engine-model.md)). Unknown name → `schema_drift` failure |
| `outcome` | string | no | Operator override of the terminal's declared outcome label, applied only when the resolved edge targets a terminal |
| (any operator-defined field) | any JSON | no | Exposed to subsequent states as `{{ tasks.<state_id>.directive.<key> }}`; top-level scalars also exported as `ROKI_TASK_<ID>_DIRECTIVE_<KEY>` per [fr:01 §Inter-state data flow](../fr/01-engine-model.md) |

## Schema of `<state_id>.terminal.json`

When the cli emits claude/codex stream-json, the daemon scans for the terminal `result` event mid-stream and writes it to this file. Other shapes leave the file absent.

| Field | Type | Required | Meaning |
|---|---|---|---|
| (the parsed `result` event verbatim) | JSON object | yes | The cli's terminal `result` event. Shape matches the cli's own output schema; the daemon does not impose a shape beyond extracting the event |

## Schema of `<state_id>.exit_code`

Plain integer (ASCII), no surrounding whitespace. Written after `wait()` returns regardless of cli shape.

## Schema of `<state_id>.events.jsonl`

One parseable JSON object per line. Carries advisory stream-json output (thinking blocks, tool-use messages, etc.) the cli emits between turns. The daemon does not interpret entries; they are forensic-only.

## When adding a new artifact

1. Add a row to the **Artifact list** table above.
2. Add a schema section if the artifact has structured contents.
3. Link to this reference from the FR page that uses it.

## Related reference

- [config.md](config.md): operator-facing configuration knobs (including `[paths].session_root`)
- [cli.md](cli.md): `roki log` flags for reading these artifacts
- [log-events.md](log-events.md): structured events emitted alongside artifact writes
