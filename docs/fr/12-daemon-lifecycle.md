---
refs:
  id: fr:12-daemon-lifecycle
  kind: fr
  title: "Daemon Lifecycle"
  spec: roki-mvp
  implements:
    - req:roki-mvp:1
  related:
    - ref:cli
    - fr:02-configuration
    - fr:08-observability-logs
    - fr:07-recovery
    - fr:04-phase-execution
    - fr:01-engine-model
  modules:
    - crates/roki-daemon/src/runtime.rs
    - crates/roki-daemon/src/config/mod.rs
---

# FR 12: Daemon Lifecycle

> The lifecycle of the single-binary daemon launched by `roki run`.
> See [`docs/reference/cli.md`](../reference/cli.md) for the full CLI flag list.

## Purpose

An operator runs roki by launching the single `roki` binary with `roki run` and stops it with SIGINT. No dependency on scripts, systemd, or supervisor. Startup failures must surface the cause in structured logs.

## User-visible Behavior

- **Normal startup**: `roki run --config ./roki.toml` loads `roki.toml`, loads `WORKFLOW.toml` (and any per-repo TOMLs referenced through `[[admission.repos]] workflow`), validates both, brings up the Linear adapter, the diff cache, the cycle engine, and the Linear webhook receiver bound to `[linear.webhook]`, optionally brings up the observability HTTP API bound to `[api]` (only when `[api].port` is set; see [10-http-api §Server gating and bind](10-http-api.md)), runs the cold-start enumeration ([07-recovery §Cold start](07-recovery.md)), then logs that it is ready.
- **Configuration error**: configuration file not found, schema validation failure for `roki.toml`, or schema validation failure for the initial `WORKFLOW.toml` load → non-zero exit, with the offending field name in the structured log.
- **Missing dependency CLI**: if `wt` / `ghq` are not on `PATH` → refuse to start, with the missing binary name and a remediation hint in the structured log. The cli lines configured in `roki.toml [default.ai.session]` and `[default.ai.command]` are **not** validated at startup (the daemon does not parse them); their first failure surfaces as a `process_crash` failure on the first phase that uses them.
- **Normal shutdown**: on SIGINT / SIGTERM, stop accepting new work, signal every active cycle (each in-flight pre / run / post subprocess) to terminate within the configured shutdown window, then exit cleanly. In-flight worktrees and session tempdirs are not deleted at shutdown — the next cold start reconciles them.
- **Help**: `roki --help` and the `--help` of each subcommand (`roki run`, `roki log`, `roki events`, `roki repo`) list every CLI flag and the configuration key each one corresponds to.

### Cycle integration

The cycle engine ([01-engine-model](01-engine-model.md)) decides when to spawn cycles; subprocess supervision is owned by [04-phase-execution](04-phase-execution.md). Daemon responsibilities at the lifecycle layer:

- **Launch a cycle**: the engine signals "spawn pre/run/post for ticket X under matched entry Y". The daemon prepares the per-iter capture directory, renders the cli line, and invokes the launcher. Cycle kind (`rule` / `cleanup` / `failure`) and trigger (`runtime` / `cold_start`) are propagated through environment variables.
- **Graceful termination**: when a cycle ends (terminal directive, failure routing, or admission eviction after natural end), the daemon writes the final structured event log entry for the cycle and, in the cleanup case, deletes the worktree + session tempdir.
- **Forced termination on shutdown**: SIGINT / SIGTERM signals every in-flight subprocess. The shutdown window applies uniformly to session-shape and command-shape subprocesses ([04-phase-execution §Subprocess shapes](04-phase-execution.md)).
- **Restart non-persistence**: nothing about the cycle engine or the diff cache is persisted across daemon restarts. The next cold start re-enumerates Linear and reconciles disk residue.

## Capabilities

- **CLI flags**: the canonical list lives in [`docs/reference/cli.md`](../reference/cli.md). Flags override configuration-file values, and `--help` displays every flag together with the corresponding configuration key.
- **Structured logging foundation**: per-ticket / per-cycle / per-iter fields are attached to every event through the tracing pipeline (see [08-observability-logs](08-observability-logs.md)).
- **Dependency check**: at startup, verify the existence of `wt` and `ghq`; refuse to start if either is missing. AI cli lines (`claude`, `codex`, etc.) are not pre-checked.
- **Signal handling**: graceful shutdown (stop new admission → signal each in-flight subprocess to exit → wait within the configured window → exit).

## Boundaries

- The configuration schema lives in [02-configuration](02-configuration.md).
- **daemonize / systemd integration / pid file** are out of scope for the MVP (operator's responsibility).
- **Windows support** is out of scope (macOS + Linux only).
- **Updates / migrations** are out of scope for v1.
- **AI CLI startup verification** is out of scope: the daemon does not parse `[default.ai.session].cli` or `[default.ai.command].cli`. Operators that want a fail-fast check author a one-shot rule whose run cmd verifies the cli is on PATH.

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Platform.
- **Requirements**:
  - `roki-mvp Req 1`: Daemon Lifecycle and CLI.
- **Design**:
  - `Daemon Bootstrap` section of `.kiro/specs/roki-mvp/design.md` (pending rewrite).
  - `.kiro/specs/roki-mvp/design-bootstrap.md` (pending rewrite).
- **Related FR**: [02-configuration](02-configuration.md), [07-recovery](07-recovery.md), [04-phase-execution](04-phase-execution.md), [08-observability-logs](08-observability-logs.md), [01-engine-model](01-engine-model.md).
