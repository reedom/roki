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

The single `roki` binary exposes one daemon subcommand, three observability subcommands, and one workflow utility subcommand. CLI flags override the corresponding values in `roki.toml` ([config.md](config.md)) where applicable. Each subcommand's `--help` lists every flag together with the configuration key it overrides.

## Subcommands

| Subcommand | Purpose | FR |
|---|---|---|
| `roki run` | Launch the daemon — default dispatch (cleanup-first then rule) | [fr:12-daemon-lifecycle](../fr/12-daemon-lifecycle.md) |
| `roki cleanup` | Launch the daemon — cleanup-only dispatch; `rules:` is ignored | [fr:12-daemon-lifecycle](../fr/12-daemon-lifecycle.md) |
| `roki log` | Read per-ticket subprocess captures | [fr:09-log-access-cli](../fr/09-log-access-cli.md) |
| `roki events` | Read the structured event stream | [fr:09-log-access-cli](../fr/09-log-access-cli.md) |
| `roki repo` | Resolve per-ticket repo path | [fr:09-log-access-cli](../fr/09-log-access-cli.md) |
| `roki workflow validate` | Sugar-expand + validate a `WORKFLOW.yaml` file | [fr:02-configuration](../fr/02-configuration.md) |
| `roki workflow graph` | Render any rule's state machine as ASCII or DOT | [fr:02-configuration](../fr/02-configuration.md) |
| `roki-tui` | Terminal UI client for the observability HTTP API | [fr:11-roki-tui](../fr/11-roki-tui.md) |

## `roki run`

| Flag | Argument | Overrides | Purpose |
|---|---|---|---|
| `--config <path>` | path | (none) | Path to `roki.toml`. Documented default applies when omitted. |
| `--log-level <level>` | one of `error` / `warn` / `info` / `debug` / `trace` | `[log].level` | Structured log level. |

## `roki cleanup`

Identical flags to `roki run`. Dispatch mode is `CleanupOnly`: `cleanup:` entries are evaluated first-match; `rules:` is ignored.

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
| `--iter <n>` | int (absolute) or `-N` (relative) | Visit selector (cycle-wide visit ordering). Negative = N visits back from current. |
| `--state <state_id>` | string | State selector. Operator-defined ids declared in `WORKFLOW.yaml`. |
| `--stream <stream>` | `stdout` / `stderr` / `directive` / `events` / `terminal` / `exit_code` | Stream selector. |
| `--tail <N>` | int | Last N lines. |
| `--bytes <N>` | int | Last N bytes. |
| `--list-visits` | (boolean) | Enumerate per-visit `(visit_n, state_id, exit_code)` tuples. |
| `--meta` | (boolean) | Cycle meta (kind, trigger, started_at, ended_at, terminal_id, total visits). |

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

## `roki workflow validate`

Pre-flight loader: parse + sugar-expand + validate. Intended for operator use ahead of daemon restart.

| Flag | Argument | Purpose |
|---|---|---|
| (positional) | path to `WORKFLOW.yaml` | File to validate. |

Exit codes:

| Code | Meaning |
|---|---|
| `0` | All checks pass. Stdout/stderr silent. |
| `1` | I/O or YAML parse error. |
| `2` | Validation error. Stderr lists every accumulated `ValidationError` ([config.md §Validation rules](config.md)). |

## `roki workflow graph`

Render any rule's state machine as ASCII or DOT. Useful for operator review of complex `rules:` / `cleanup:` / `on_failure:` lists.

| Flag | Argument | Purpose |
|---|---|---|
| (positional) | path to `WORKFLOW.yaml` | File to render. |
| `--rule <selector>` | `rules[<n>]` / `cleanup[<n>]` / `on_failure[<n>]` | Render a single rule. Omit to render every state machine in the file. |
| `--format <fmt>` | `ascii` (default) / `dot` | Output format. |
| `--out <path>` | path | Write to file. Stdout when omitted. |

Validation runs before rendering. A validation error exits non-zero without rendering.

## `roki-tui`

Standalone `ratatui` binary. Connects to the observability HTTP API and renders four views.

| Flag | Argument | Overrides | Purpose |
|---|---|---|---|
| (positional) | `API_URL` | (none) | Base URL of the API (http or https). Required. |
| `--config <path>` | path | (none) | Override `~/.config/roki-tui/config.toml`. Required to exist when supplied. |
| `--tickets-cadence <secs>` | int | `[polling].tickets_seconds` | Tickets refresh cadence (min 1). |
| `--events-cadence <secs>` | int | `[polling].events_seconds` | Events tail cadence (min 1). |
| `--escalations-cadence <secs>` | int | `[polling].escalations_seconds` | Escalations refresh cadence (min 1). |

Configuration file schema:

````toml
[polling]
tickets_seconds = 2       # default 2, min 1
events_seconds = 1        # default 1, min 1
escalations_seconds = 5   # default 5, min 1
````

## When adding a new flag

1. Add a row to the corresponding subcommand section.
2. Link to this table from the FR page that uses the flag.
3. Update the corresponding requirement IDs (post spec rebuild).

## Related reference

- [config.md](config.md): schema of the configuration keys overridden by these flags
- [log-events.md](log-events.md): canonical event names accepted by `roki events --kind`
