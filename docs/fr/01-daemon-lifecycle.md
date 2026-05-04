---
refs:
  id: fr:01-daemon-lifecycle
  kind: fr
  title: "Daemon Lifecycle"
  spec: roki-mvp
  implements:
    - req:roki-mvp:1
  depends_on:
    - fr:02-configuration
    - fr:13-observability-logs
  related:
    - ref:cli
    - design:roki-mvp:bootstrap
    - fr:19-orchestrator-session
    - fr:04-state-machine-and-recovery
  modules:
    - crates/roki-daemon/src/runtime.rs
    - crates/roki-daemon/src/config/mod.rs
---

# FR 01: Daemon Lifecycle

> The lifecycle of the single-binary daemon launched by `roki run`.
> See [`docs/reference/cli.md`](../reference/cli.md) for the full CLI flag list.

## Purpose

Guarantee that an operator can run roki by simply "launching the single `roki` binary with `roki run` and stopping it with SIGINT". The daemon must not depend on a particular combination of scripts, systemd, or supervisor. When startup fails, the operator must immediately learn what went wrong from the structured logs.

## User-visible Behavior

- **Normal startup**: `roki run --config ./roki.toml` brings up the orchestrator, Linear adapter, workflow loader, and webhook server, then logs that it is ready.
- **Configuration error**: configuration file not found / validation failure → non-zero exit, with the offending field name in the structured log.
- **Missing dependency CLI**: if `wt` / `ghq` / `claude` are not on `PATH` → refuse to start, with the missing binary name and a remediation hint in the structured log.
- **Normal shutdown**: on SIGINT / SIGTERM, stop accepting new work, give every active orchestrator session and every active phase subprocess a bounded shutdown window, then exit cleanly.
- **Help**: `roki --help` and the `--help` of each subcommand list every CLI flag and the configuration key each one corresponds to.

### Orchestrator session A lifecycle integration

Per [FR 19: Orchestrator Session](19-orchestrator-session.md) the daemon launches one long-lived `claude --input-format stream-json --output-format stream-json` orchestrator session A per ticket. The daemon's lifecycle responsibility for A is mechanical:

- **Launch**: A is started on the `Discovered → Pending` transition for an admitted issue, so it is already running when the orchestrator publishes the `Pending → Judging` transition that fires the first `admission_request` event ([FR 19 §Lifecycle](19-orchestrator-session.md)).
- **Graceful termination**: A is gracefully terminated when the issue lands in any `Inactive(reason=*)` and any A-driven Linear writes for that terminal state have completed; the daemon sends a final `stop`-acknowledgement signal then closes A's stdin and waits for clean exit within the configured shutdown window.
- **Forced termination**: `Cleaning` (entered on tracker-observed terminal Linear state or assignment loss, per [04-state-machine-and-recovery](04-state-machine-and-recovery.md)) may force-terminate A regardless of in-flight turns — cleanup of worktree / session tempdir takes priority.
- **Restart non-persistence**: A is not persisted across daemon restarts. On restart-recovery, a fresh A is launched per re-admitted ticket when the issue re-enters `Pending`; in-flight turns and any A-internal scratch state are discarded.

The contract for A's tool surface, response schema, event catalog, configuration namespace, and failure modes lives in [FR 19: Orchestrator Session](19-orchestrator-session.md); this FR does not restate it.

## Capabilities

- **CLI flags**: the canonical list of flags lives in [`docs/reference/cli.md`](../reference/cli.md). Flags override configuration-file values, and `--help` displays every flag together with the corresponding configuration key.
- **Structured logging foundation**: per-issue / per-worker / per-repo fields are attached to every event through the tracing pipeline (see [13-observability-logs](13-observability-logs.md) for details).
- **Dependency check**: at startup, verify the existence of `wt` / `ghq` / the configured `claude` binary; refuse to start if any is missing.
- **Signal handling**: graceful shutdown (stop new admission → signal each active orchestrator session and each active phase subprocess to exit → wait within the configured window → exit). A's contract is owned by [FR 19: Orchestrator Session](19-orchestrator-session.md); phase subprocesses are owned by [07-worker-execution](07-worker-execution.md).

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
- **Related FR**: 02-configuration, 04-state-machine-and-recovery, 07-worker-execution, 13-observability-logs, 19-orchestrator-session
