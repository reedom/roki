# FR 01: Daemon Lifecycle

> The lifecycle of the single-binary daemon launched by `roki run`.
> See [`docs/reference/cli.md`](../reference/cli.md) for the full CLI flag list.

## Purpose

Guarantee that an operator can run roki by simply "launching the single `roki` binary with `roki run` and stopping it with SIGINT". The daemon must not depend on a particular combination of scripts, systemd, or supervisor. When startup fails, the operator must immediately learn what went wrong from the structured logs.

## User-visible Behavior

- **Normal startup**: `roki run --config ./roki.toml` brings up the orchestrator, Linear adapter, workflow loader, and webhook server, then logs that it is ready.
- **Configuration error**: configuration file not found / validation failure → non-zero exit, with the offending field name in the structured log.
- **Missing dependency CLI**: if `wt` / `ghq` / `claude` are not on `PATH` → refuse to start, with the missing binary name and a remediation hint in the structured log.
- **Normal shutdown**: on SIGINT / SIGTERM, stop accepting new work, give running worker subprocesses a bounded shutdown window, then exit cleanly.
- **Help**: `roki --help` and the `--help` of each subcommand list every CLI flag and the configuration key each one corresponds to.

## Capabilities

- **CLI flags**: the canonical list of flags lives in [`docs/reference/cli.md`](../reference/cli.md). Flags override configuration-file values, and `--help` displays every flag together with the corresponding configuration key.
- **Structured logging foundation**: per-issue / per-worker / per-repo fields are attached to every event through the tracing pipeline (see [13-observability-logs](13-observability-logs.md) for details).
- **Dependency check**: at startup, verify the existence of `wt` / `ghq` / the configured `claude` binary; refuse to start if any is missing.
- **Signal handling**: graceful shutdown (stop new admission → signal workers to exit → wait within the configured window → exit).

## Boundaries

- The configuration schema lives in [02-configuration](02-configuration.md).
- **daemonize / systemd integration / pid file** are out of scope for the MVP (operator's responsibility).
- **Windows support** is out of scope (macOS + Linux only).
- **Updates / migrations** are out of scope for v1.

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Platform
- **Requirements**:
  - `roki-mvp Req 1`: Daemon Lifecycle and CLI
- **Design**:
  - `Daemon Bootstrap` section of `.kiro/specs/roki-mvp/design.md`
  - `.kiro/specs/roki-mvp/design-bootstrap.md`
- **Related FR**: 02-configuration, 13-observability-logs
