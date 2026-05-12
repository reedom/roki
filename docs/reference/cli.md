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

## `roki cleanup`

Identical flags to `roki run`. Dispatch mode is `CleanupOnly`: `cleanup:` entries are evaluated first-match; `rules:` is ignored.

| Flag | Argument | Overrides | Purpose |
|---|---|---|---|
| `--config <path>` | path | (none) | Path to `roki.toml`. Documented default applies when omitted. |

## `roki log`

Per-ticket subprocess capture reader. Defaults read `ROKI_TICKET_ID` / `ROKI_CYCLE_ID` / `ROKI_CONFIG_SESSION_ROOT` from the environment when invoked inside a state subprocess. External callers pass `--config <PATH>` so `paths.session_root` resolves.

```
roki log [OPTIONS]
roki log --list-visits [OPTIONS]
roki log --meta [OPTIONS]
```

| Flag | Type | Default | Notes |
|---|---|---|---|
| `--ticket <id>` | string | `$ROKI_TICKET_ID` | Required when env unset. |
| `--cycle <uuid>` | string | `$ROKI_CYCLE_ID` | Required when env unset. |
| `--state <state_id>` | string | required for stream reads | Operator-declared state id from `WORKFLOW.yaml`. |
| `--iter <n>` | i32 | latest completed visit | Absolute `>0` or relative `-N` (N visits back from latest). |
| `--stream <kind>` | enum | required for stream reads | `stdout` / `stderr` / `events` / `terminal` / `directive` / `exit_code`. |
| `--tail <n>` | usize | unset | Last N lines (line-oriented streams). Conflicts with `--bytes`. |
| `--bytes <n>` | usize | unset | Last N bytes. Conflicts with `--tail`. |
| `--list-visits` | flag | — | Emit per-visit JSON Lines `{visit_n, state_id, exit_code}`. |
| `--meta` | flag | — | Emit `cycle.json` content verbatim. |
| `--follow` | flag | — | Continue tailing `stdout` / `stderr` after EOF; polls every 200 ms (hidden `--follow-poll-ms` for tests). |
| `--config <PATH>` | path | (none) | Required when `$ROKI_CONFIG_SESSION_ROOT` is unset. |

## `roki events`

Structured event stream reader. Default mode connects to the daemon's HTTP API; `--offline --file <path>` reads a JSON Lines file directly. HTTP API URL resolves in this order: `--api <URL>` flag, `$ROKI_API_URL` env, `[api]` section of `--config <roki.toml>`.

```
roki events [--tail] [--since <S>] [--kind <K>] [--ticket <T>] [--cycle <U>]
            [--format json|human] [--api <URL>] [--config <PATH>]
roki events --offline --file <PATH> [filters...]
```

| Flag | Default | Notes |
|---|---|---|
| `--tail` | unset | Continuous polling loop until SIGINT. |
| `--since <S>` | unset | `<u64>` → server-side cursor. RFC3339 timestamp → client-side filter after server `since=0`. |
| `--kind <K>` | unset | Filter on `event` (one value; AND with the rest). |
| `--ticket <T>` | unset | Forwarded to `/api/events?ticket=`. |
| `--cycle <U>` | unset | Forwarded to `/api/events?cycle=`. |
| `--format` | `json` | `human` = one-line text reformatter. |
| `--api <URL>` | `$ROKI_API_URL` else `--config`-derived else error | HTTP base URL. |
| `--config <PATH>` | (none) | Used to synthesize `--api` from `[api]` when env unset. |
| `--offline --file <P>` | — | Read JSONL file directly. Ignores `--api` / `--config`. |
| `--cadence-ms <N>` | 1000 | `--tail` polling cadence. Hidden flag (testability). |

Filters compose with AND.

## `roki repo`

Per-ticket repo path resolver. Defaults read `ROKI_TICKET_ID` / `ROKI_REPO_GHQ` from the environment. Default returns the worktree path when one exists, else the ghq base path. Pre-run callers receive the ghq base — treat it as **read-only** unless `--worktree` confirmed worktree materialization ([fr:09-log-access-cli §`roki repo`](../fr/09-log-access-cli.md)).

```
roki repo [<ghq>] [--ticket <id>] [--worktree] [--auto-clone] [--config <PATH>]
```

| Arg / Flag | Default | Notes |
|---|---|---|
| `<ghq>` positional | `$ROKI_REPO_GHQ` | E.g. `github.com/foo/bar`. |
| `--ticket <id>` | `$ROKI_TICKET_ID` | Needed only when worktree resolution is attempted. |
| `--worktree` | flag | Require worktree (exit 1 if absent). |
| `--auto-clone` | flag | Run `ghq get <ghq>` before resolving the ghq base. |
| `--config <PATH>` | (none) | Currently optional (the ghq + worktree lookup is config-free). |

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
