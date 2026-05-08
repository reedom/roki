---
refs:
  id: ref:cli
  kind: reference
  title: "CLI Flags"
  related:
    - ref:config
    - fr:12-daemon-lifecycle
    - fr:09-log-access-cli
    - fr:08-observability-logs
---

# Reference: CLI Flags

The single `roki` binary exposes one daemon subcommand and three observability subcommands. CLI flags override the corresponding values in `roki.toml` ([config.md](config.md)) where applicable. Each subcommand's `--help` lists every flag together with the configuration key it overrides.

## Subcommands

| Subcommand | Purpose | FR |
|---|---|---|
| `roki run` | Launch the daemon — default dispatch (cleanup-first then rule) | [fr:12-daemon-lifecycle](../fr/12-daemon-lifecycle.md) |
| `roki cleanup` | Launch the daemon — cleanup-only dispatch; `[[rule]]` is ignored | [fr:12-daemon-lifecycle](../fr/12-daemon-lifecycle.md) |
| `roki log` | Read per-ticket subprocess captures | [fr:09-log-access-cli](../fr/09-log-access-cli.md) |
| `roki events` | Read the structured event stream | [fr:09-log-access-cli](../fr/09-log-access-cli.md) |
| `roki repo` | Resolve per-ticket repo path | [fr:09-log-access-cli](../fr/09-log-access-cli.md) |

## `roki run`

| Flag | Argument | Overrides | Purpose |
|---|---|---|---|
| `--config <path>` | path | (none) | Path to `roki.toml`. Documented default applies when omitted. |
| `--log-level <level>` | one of `error` / `warn` / `info` / `debug` / `trace` | `[log].level` | Structured log level. |

## `roki cleanup`

Identical flags to `roki run`. Dispatch mode is `CleanupOnly`: `[[cleanup]]` entries are evaluated first-match; `[[rule]]` is ignored.

| Flag | Argument | Overrides | Purpose |
|---|---|---|---|
| `--config <path>` | path | (none) | Path to `roki.toml`. Documented default applies when omitted. |
| `--log-level <level>` | one of `error` / `warn` / `info` / `debug` / `trace` | `[log].level` | Structured log level. |

## `roki log`

Per-ticket subprocess capture reader. Defaults read `ROKI_TICKET_ID` / `ROKI_CYCLE_ID` / `ROKI_CYCLE_ITER` from the environment.

| Flag | Argument | Purpose |
|---|---|---|
| `--ticket <id>` | Linear issue id | Override default ticket. Required when invoked without env (and required alongside `--cycle` for cross-ticket reads). |
| `--cycle <uuid>` | cycle UUID | Cross-cycle access within the same ticket. |
| `--iter <n>` | int (absolute) or `-N` (relative) | Iteration index. Negative = N back from current. |
| `--phase <phase>` | `pre` / `run` / `post` | Phase selector. |
| `--stream <stream>` | `stdout` / `stderr` / `response` / `events` / `terminal` / `exit_code` | Stream selector. |
| `--tail <N>` | int | Last N lines. |
| `--bytes <N>` | int | Last N bytes. |
| `--list-iters` | (boolean) | Enumerate iter ids and per-phase completion status. |
| `--meta` | (boolean) | Cycle meta (kind, trigger, started_at, ended_at). |

## `roki events`

Structured event stream reader. Default mode connects to the daemon's HTTP API; `--offline --file <path>` reads a JSON Lines file directly.

| Flag | Argument | Purpose |
|---|---|---|
| `--tail` | (boolean) | Live tail (HTTP polling). |
| `--since <timestamp>` | RFC 3339 | Range start. |
| `--kind <event_kind>` | event name | Filter by canonical event kind ([log-events.md](log-events.md)). |
| `--ticket <id>` | Linear issue id | Filter by ticket. |
| `--cycle <uuid>` | cycle UUID | Filter by cycle. |
| `--format <format>` | `json` (default) / `human` | Output format. |
| `--offline` | (boolean) | Read from file instead of HTTP API. Requires `--file`. |
| `--file <path>` | path | JSON Lines event file (with `--offline`). |

Filters compose with AND.

## `roki repo`

Per-ticket repo path resolver. Defaults read `ROKI_TICKET_ID` / `ROKI_REPO` from the environment.

| Flag | Argument | Purpose |
|---|---|---|
| (positional) | repo identifier (`github.com/foo/bar`) | Explicit repo. Optional. |
| `--ticket <id>` | Linear issue id | Override default ticket. |
| `--auto-clone` | (boolean) | Run `ghq get` if the ghq base path does not exist. The daemon never auto-clones implicitly. |
| `--worktree` | (boolean) | Require worktree. Exits 1 if not yet created. |

Default returns the worktree path when one exists, else the ghq base path. Pre-run callers receive the ghq base — treat it as **read-only** unless `--worktree` confirmed worktree materialization ([fr:09-log-access-cli §`roki repo`](../fr/09-log-access-cli.md)).

## When adding a new flag

1. Add a row to the corresponding subcommand section.
2. Link to this table from the FR page that uses the flag.
3. Update the corresponding requirement IDs (post spec rebuild).

## Related reference

- [config.md](config.md): schema of the configuration keys overridden by these flags
- [log-events.md](log-events.md): canonical event names accepted by `roki events --kind`
