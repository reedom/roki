---
refs:
  id: fr:12-daemon-lifecycle
  kind: fr
  title: "Daemon Lifecycle"
  spec: roki-cli-daemon
  related:
    - ref:cli
    - fr:02-configuration
    - fr:08-observability-logs
    - fr:07-recovery
    - fr:04-state-execution
    - fr:01-engine-model
  modules:
    - crates/roki-daemon/src/runtime.rs
    - crates/roki-daemon/src/config/mod.rs
---

# FR 12: Daemon Lifecycle

> The lifecycle of the single-binary daemon launched by `roki run` or `roki cleanup`.
> See [`docs/reference/cli.md`](../reference/cli.md) for the full CLI flag list.

## Purpose

An operator runs roki by launching the single `roki` binary with `roki run` (or `roki cleanup` for cleanup-only dispatch) and stops it with SIGINT. No dependency on scripts, systemd, or supervisor. Startup failures must surface the cause in structured logs.

## Dispatch modes

| Subcommand | Dispatch mode | Effect |
|---|---|---|
| `roki run` | Default | Long-running daemon. On every diff (webhook, polling fallback, refresh nudge, cold-start enumeration) evaluates `cleanup:` first-match, then `rules:` first-match. |
| `roki cleanup` | CleanupOnly | Long-running daemon. Same dispatch loop but `rules:` is ignored; only `cleanup:` first-match runs. |

Both subcommands run the same long-running daemon. The process exits only on signal or fatal startup error; cycles do not terminate the daemon.

## User-visible Behavior

- **Normal startup**: `roki run --config ./roki.toml` (or `roki cleanup --config ./roki.toml`) loads `roki.toml`, loads `WORKFLOW.yaml` (and any per-repo YAML files referenced through `[[admission.repos]] workflow`), validates both, brings up the Linear adapter, the diff cache, the cycle engine, and the Linear webhook receiver bound to `[linear.webhook]`, optionally brings up the observability HTTP API bound to `[api]` (only when `[api].port` is set; see [10-http-api §Server gating and bind](10-http-api.md)), runs the cold-start enumeration ([07-recovery §Cold start](07-recovery.md)), then logs that it is ready.
- **Configuration error**: configuration file not found, schema validation failure for `roki.toml`, or schema validation failure for the initial `WORKFLOW.yaml` load → non-zero exit, with the offending field name in the structured log.
- **Missing dependency CLI**: if `wt` / `ghq` are not on `PATH` → refuse to start, with the missing binary name and a remediation hint in the structured log. The refusal emits `daemon_dependency_missing` ([ref:log-events](../reference/log-events.md)) for each missing binary before the daemon exits. The cli line configured in `roki.toml [default.ai]` is **not** validated at startup (the daemon does not parse it); its first failure surfaces as a `process_crash` failure on the first state that uses it.
- **Normal shutdown**: on SIGINT / SIGTERM, stop accepting new work, signal every active cycle's in-flight state subprocess to terminate within the configured shutdown window (`roki.toml [engine].shutdown_window_seconds`), then exit cleanly. In-flight worktrees and session tempdirs are not deleted at shutdown — the next cold start reconciles them. (See [§Normal shutdown](#normal-shutdown) below for the full sequence.)
- **Help**: `roki --help` and the `--help` of each subcommand (`roki run`, `roki cleanup`, `roki log`, `roki events`, `roki repo`) list every CLI flag and the configuration key each one corresponds to.

### Normal shutdown

On SIGINT / SIGTERM the daemon:

1. Stops accepting webhooks and stops launching new cycles.
2. Sends SIGTERM to every in-flight state subprocess. The shutdown grace window from `roki.toml [engine].shutdown_window_seconds` applies uniformly to every state regardless of cli line, since every state is command-shape ([04-state-execution §Subprocess shape](04-state-execution.md)).
3. Waits up to that window for each subprocess to exit. Subprocesses still alive at the end of the window are SIGKILLed and the daemon emits `shutdown_window_exceeded` ([ref:log-events](../reference/log-events.md)) naming each offending subprocess (`offenders[].{ticket_id, cycle_id, state_id, visit, pid}`).
4. Drops the in-memory diff cache (nothing is persisted).
5. Exits with code 0. Worktrees and session tempdirs are **not** deleted at shutdown — the next cold start reconciles them.

### Cycle integration

The cycle engine ([01-engine-model](01-engine-model.md)) decides when to spawn cycles; subprocess supervision is owned by [04-state-execution](04-state-execution.md). Daemon responsibilities at the lifecycle layer:

- **Launch a cycle**: the engine signals "drive state machine for ticket X under matched entry Y". The daemon prepares the per-cycle capture root, the engine drives state visits, and each visit invokes the launcher. Cycle kind (`rule` / `cleanup` / `failure`) and trigger (`runtime` / `cold_start`) are propagated through environment variables.
- **Cycle completion**: when a cycle reaches a terminal, the daemon writes the final structured event (`cycle_completed` with `terminal_id` + `outcome`) and, in the cleanup case, deletes the worktree + session tempdir. On cycle failure, the runtime evaluates `on_failure:` first-match: a match spawns a `cycle.kind = "failure"` handler cycle (recursion bounded to one level); no match emits `failure_unhandled` with `marker = none`; a handler cycle that itself fails is added to the escalation queue with `marker = recursion_bound`. Cleanup-time fs errors enter the queue with `marker = cleanup_fs`; daemon-internal errors with no cycle association use `marker = daemon_internal` ([06-failure-handling §Escalation queue](06-failure-handling.md)). The daemon stays running across all of these outcomes.
- **Forced termination on shutdown**: SIGINT / SIGTERM signals every in-flight state subprocess. The shutdown window applies uniformly to every state regardless of cli line, since every state is command-shape ([04-state-execution §Subprocess shape](04-state-execution.md)).
- **Restart non-persistence**: nothing about the cycle engine or the diff cache is persisted across daemon restarts. The next cold start re-enumerates Linear and reconciles disk residue.

## Capabilities

- **CLI flags**: the canonical list lives in [`docs/reference/cli.md`](../reference/cli.md). Flags override configuration-file values, and `--help` displays every flag together with the corresponding configuration key.
- **Structured logging foundation**: per-ticket / per-cycle / per-state / per-visit fields are attached to every event through the tracing pipeline (see [08-observability-logs](08-observability-logs.md)).
- **Dependency check**: at startup, verify the existence of `wt` and `ghq`; refuse to start if either is missing. AI cli lines (`claude`, `codex`, etc.) are not pre-checked.
- **Signal handling**: graceful shutdown (stop new admission → signal each in-flight state subprocess to exit → wait within the configured window → exit).

## Boundaries

- The configuration schema lives in [02-configuration](02-configuration.md).
- **daemonize / systemd integration / pid file** are out of scope for the MVP (operator's responsibility).
- **Windows support** is out of scope (macOS + Linux only).
- **Updates / migrations** are out of scope for v1.
- **AI CLI startup verification** is out of scope: the daemon does not parse `[default.ai].cli`. Operators that want a fail-fast check author a one-shot rule whose first state runs `which <cli>` and writes a sentinel directive accordingly.

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Platform.
- **Requirements**:
  - `roki-mvp Req 1`: Daemon Lifecycle and CLI.
- **Related FR**: [02-configuration](02-configuration.md), [07-recovery](07-recovery.md), [04-state-execution](04-state-execution.md), [08-observability-logs](08-observability-logs.md), [01-engine-model](01-engine-model.md).
