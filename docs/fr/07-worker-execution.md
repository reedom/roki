# FR 07: Worker Execution

> One bounded `claude --print --output-format stream-json` invocation per admitted ticket. Includes permission strategy, retry budget, max-turns, and stall detection.

## Purpose

Run the agent (whose orchestration is performed inside by a kiro skill) as a single bounded subprocess per ticket. The daemon stays a thin layer that only observes its lifecycle; it does not drive the agent loop itself. The permission strategy lets the operator pick "the safest profile that works" plus a fallback "for when Claude's allowlist cannot be trusted".

## User-visible Behavior

### Launch and observation

- **Launch**: the orchestrator promotes an issue into an active worker slot → the daemon launches `claude --print --output-format stream-json` inside the issue's session tempdir → stdout is parsed as a stream of newline-delimited JSON events.
- **Event handling**: each JSON event is parsed into a typed lifecycle event → emitted as a structured log + observability surface. **State machine transitions are driven only by subprocess exit and Linear state**, never by the contents of an event.
- **Skill discovery**: launch flags are passed so that kiro skills can auto-trigger from `~/.claude/skills/kiro-*` (we do not use `--bare`). We do not depend on slash commands.

### Permission strategy

The strategy is configured via the `[permissions].strategy` config and the `--dangerously-skip-permissions` CLI flag (canonical references: [02-configuration](02-configuration.md) / [01-daemon-lifecycle](01-daemon-lifecycle.md)).

- **Default sandbox**: `workspace-write` + elicitations rejected.
- **Override via `WORKFLOW.md`**: if an alternative sandbox / elicitation policy is declared there, it is applied to the worker.
- **`--settings` allowlist strategy**: pass the configured allowlist to the worker through Claude Code's settings interface.
- **`--dangerously-skip-permissions` strategy**: pass that flag to the worker, and log the fact that "elevated permission was used" on every worker launch.
- **No strategy configured**: refuse to start.
- **Setup judge exception**: the judge subprocess is fixed to read-only sandbox + elicitations rejected **regardless of any operator override** (see [05-setup-judge](05-setup-judge.md)).

### Termination handling

- **Stall**: if no event arrives for longer than the configured stall window, treat as stalled, terminate the subprocess → `TerminalFailure` (no retry) + Slack notification.
- **Max-turns reached**: a terminal `result` event reports turn-budget exhaustion → `TerminalFailure` (no retry) + Slack notification.
- **Clean exit (success)**: a terminal `result` event with `subtype: success` → transition from `Active` to `AwaitingCleanup`; do not relaunch. Entering `Cleaning` waits for a tracker observation (terminal Linear state / reassignment).
- **Non-clean exit (retryable)**: relaunch a fresh subprocess after exponential backoff between 10 seconds and 5 minutes, within the configured ticket-level retry budget (default 3, range 1-10). The worktree and session tempdir are preserved across retries.
- **Unknown `result.subtype`**: do not consume the retry budget; go straight to `TerminalFailure`, log the raw subtype, and notify the operator.

## Capabilities

- **One invocation per ticket**: the daemon never relaunches after a clean exit (internal orchestration is the responsibility of the agent-side kiro skill).
- **`--max-turns` passthrough**: a per-invocation turn budget is configurable. The subprocess honors the budget.
- **Retryable causes** (consume the retry budget):
  1. Non-zero exit without a terminal `result` event
  2. Termination by signal
  3. Terminal `result` event reports a subtype that is retryable in the compiled mapping (at least `error_during_execution` as of the MVP build)
- **Non-retryable causes**: stall / max-turns reached / unknown subtype go straight to `TerminalFailure` without consuming the retry budget.
- **Per-launch logging of the permission strategy**: each `--dangerously-skip-permissions` elevation decision is recorded on every worker launch.

## Boundaries

- **Driving the agent loop** is owned by the agent-side kiro skill, not the daemon.
- **Per-turn control / interruption / resumption** is out of scope (1 invocation = 1 lifecycle).
- **Semantic interpretation of subprocess output** is not done (only logging and stall detection).
- **Per-tool-granularity permission policy** is out of scope (only what Claude Code's interface allows).
- **Container / VM isolation** is out of scope (we depend on the `workspace-write` sandbox plus path safety).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Single bounded `claude ...` invocation per admitted issue"; Constraints > Permissions
- **Requirements**:
  - `roki-mvp Req 5`: Bounded Claude Code Subprocess Adapter
  - `roki-mvp Req 9`: Permission Strategy and Default Sandbox
- **Design**:
  - `Engine Adapter` / `Permission Strategy` sections of `.kiro/specs/roki-mvp/design.md`
  - `.kiro/specs/roki-mvp/design-retry-policy.md`
- **Related FR**: 05-setup-judge, 06-worktree-and-session, 11-agent-tool-boundary
