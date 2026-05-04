---
refs:
  id: ref:cli
  kind: reference
  title: "CLI Flags"
  implements:
    - req:roki-mvp:1.6
    - req:roki-mvp:2.5
    - req:roki-mvp:9.4
    - req:roki-mvp:11.6
  related:
    - ref:config
    - fr:01-daemon-lifecycle
    - fr:07-worker-execution
    - fr:13-observability-logs
---

# Reference: CLI Flags

CLI flags accepted by `roki run`. Flags override the corresponding values in `roki.toml`.

## Flags

| Flag | Argument | Overrides | Purpose | Used by | Requirements |
|---|---|---|---|---|---|
| `--config <path>` | path | (none; specifies the config file itself loaded at startup) | Path to `roki.toml`. The documented default is used when omitted | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 1.6 |
| `--bind <addr>` | bind addr | `[server].bind` | Bind host of the webhook receiver | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 1.6, Req 2.5 |
| `--port <num>` | port | `[server].port` | Bind port of the webhook receiver | [01-daemon-lifecycle](../fr/01-daemon-lifecycle.md) | roki-mvp Req 1.6, Req 2.5 |
| `--dangerously-skip-permissions` | (boolean) | Pins the entire permission strategy to `--dangerously-skip-permissions` | Fallback for when Claude Code's allowlist cannot be trusted | [07-worker-execution](../fr/07-worker-execution.md) | roki-mvp Req 1.6, Req 9.4 |
| `--debug` | (boolean) | Enables per-issue debug capture | Records each worker subprocess's stdout/stderr to a per-issue file | [13-observability-logs](../fr/13-observability-logs.md) | roki-mvp Req 1.6, Req 11.6 |

`roki --help` and each subcommand's `--help` document every flag together with **the configuration key it corresponds to**.

## When adding a new CLI flag

1. Add a row to the table above.
2. Link to this table from the FR page that uses the flag.
3. Update the corresponding `roki-mvp` requirements.

## Related reference

- [config.md](config.md): schema of the configuration keys overridden by these flags
