# Reference: CLI Flags

The **canonical reference** for the CLI flags accepted by the `roki run` subcommand.
Flags override the corresponding values in the configuration file (`roki.toml`).

## Flags

| Flag | Argument | Overrides | Purpose | Used by | Requirements |
|---|---|---|---|---|---|
| `--config <path>` | path | (none; specifies the config file itself loaded at startup) | Path to `roki.toml`. The documented default is used when omitted | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 1.6 |
| `--bind <addr>` | bind addr | `[server].bind` | Bind host of the webhook receiver | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 1.6, Req 2.5 |
| `--port <num>` | port | `[server].port` | Bind port of the webhook receiver | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 1.6, Req 2.5 |
| `--dangerously-skip-permissions` | (boolean) | Pins the entire permission strategy to `--dangerously-skip-permissions` | Fallback for when Claude Code's allowlist cannot be trusted | [07-worker-execution](../fr/07-worker-execution.md) | roki-mvp Req 1.6, Req 9.4 |
| `--debug` | (boolean) | Enables per-issue debug capture | Records each worker subprocess's stdout/stderr to a per-issue file | [13-observability-logs](../fr/13-observability-logs.md) | roki-mvp Req 1.6, Req 11.6 |

`roki --help` and the `--help` of each subcommand document every flag in the table above together with **the configuration key it corresponds to**.

## When adding a new CLI flag

1. Add a row to the table above (Flag / Argument / Overrides / Purpose / Used by / Requirements).
2. From the FR page that uses the flag, link to this table (no duplicate explanation).
3. Update the corresponding `roki-mvp` requirements.

## Related reference

- [config.md](config.md): the schema of the configuration keys overridden by the CLI flags
