---
refs:
  id: fr:07-worker-execution
  kind: fr
  title: "Worker Execution"
  spec: roki-mvp
  implements:
    - req:roki-mvp:5
    - req:roki-mvp:5.10
    - req:roki-mvp:9
---

# FR 07: Worker Execution

> One bounded `claude --print --output-format stream-json` invocation per admitted ticket for the main worker, plus two short-lived bounded one-shot invocations of the same engine: the setup judge and the linear-updater subagent. Includes permission strategy, retry budget, max-turns, stall detection, and the rules for review-gate-driven intentional re-launches.

## Purpose

Run the agent (whose orchestration is performed inside by a kiro skill) as a single bounded subprocess per ticket. The daemon stays a thin layer that only observes its lifecycle; it does not drive the agent loop itself. The same engine adapter additionally supervises two daemon-driven bounded one-shot invocations: the setup judge ([05-setup-judge](05-setup-judge.md)) before the worker, and the linear-updater on daemon-only failures (translating those events into Linear label additions and comments via the operator's installed Linear MCP).

The permission strategy lets the operator pick "the safest profile that works" for the main worker, plus a fallback "for when Claude's allowlist cannot be trusted". The setup judge and the linear-updater always run with a read-only filesystem sandbox regardless of operator overrides.

## User-visible Behavior

### Launch and observation (main worker)

- **Launch**: the orchestrator promotes an issue into an active worker slot → the daemon launches `claude --print --output-format stream-json` inside the issue's session tempdir → stdout is parsed as a stream of newline-delimited JSON events.
- **Event handling**: each JSON event is parsed into a typed lifecycle event → emitted as a structured log + observability surface. **State machine transitions are driven only by subprocess exit and Linear state**, never by the contents of an event.
- **Skill discovery**: launch flags are passed so that kiro skills can auto-trigger from `~/.claude/skills/kiro-*` (we do not use `--bare`). We do not depend on slash commands.

### Permission strategy (main worker)

The strategy is configured via the `[permissions].strategy` config and the `--dangerously-skip-permissions` CLI flag (canonical references: [02-configuration](02-configuration.md) / [01-daemon-lifecycle](01-daemon-lifecycle.md)).

- **Default sandbox**: `workspace-write` + elicitations rejected.
- **Override via `WORKFLOW.md`**: if an alternative sandbox / elicitation policy is declared there, it is applied to the worker.
- **`--settings` allowlist strategy**: pass the configured allowlist to the worker through Claude Code's settings interface.
- **`--dangerously-skip-permissions` strategy**: pass that flag to the worker, and log the fact that "elevated permission was used" on every worker launch.
- **No strategy configured**: refuse to start.
- **Setup judge and linear-updater exception**: both subprocesses are launched with **read-only filesystem sandbox + elicitations rejected** regardless of any operator override. Whether they can call write-capable Linear MCP tools is governed by the operator's Claude Code MCP allowlist, not by the daemon.

### Termination handling (main worker)

- **Stall**: if no event arrives for longer than the configured stall window, treat as stalled, terminate the subprocess → `Inactive(reason=stall)` (no retry) + dispatch linear-updater with a `stall` directive.
- **Max-turns reached**: a terminal `result` event reports turn-budget exhaustion → `Inactive(reason=max_turns_exhausted)` (no retry) + dispatch linear-updater with a `max_turns_exhausted` directive.
- **Clean exit (success)**: a terminal `result` event with `subtype: success` → publish the `Active → Inactive` transition for the review gate ([09-pre-pr-gate](09-pre-pr-gate.md)) to inspect:
  - `Allow` (or no veto) → `Inactive(reason=awaiting_linear)`. Subsequent transition into `Cleaning` is driven only by tracker observation of a terminal Linear state or assignment loss.
  - `Deny+RetryWithContext(payload)` with retry budget remaining → re-launch a fresh worker subprocess with `additional_context = payload` ([12-extension-surface](12-extension-surface.md)). Worktree and session tempdir are preserved.
  - `Deny` with retry exhausted → `Inactive(reason=review_gate_exhausted)` + dispatch linear-updater with a `review_gate_exhausted` directive.
- **Non-clean exit (retryable)**: enter `Backoff`. After exponential backoff between 10 seconds and 5 minutes (within the configured ticket-level retry budget, default 3, range 1-10), re-launch a fresh subprocess. The worktree and session tempdir are preserved across retries. On retry exhaustion → `Inactive(reason=retry_exhausted)` + dispatch linear-updater with a `retry_exhausted` directive.
- **Unknown `result.subtype`**: do not consume the retry budget; go straight to `Inactive(reason=unknown_subtype)`, log the raw subtype, and dispatch linear-updater with an `unknown_subtype` directive.

### linear-updater invocation lifecycle

- **Trigger**: any of the directive cases above (stall / max-turns / unknown subtype / retry-exhausted / review-gate-exhausted), plus `judge_unparseable` ([05-setup-judge](05-setup-judge.md)), `fs_poison` ([06-worktree-and-session](06-worktree-and-session.md)), `orphan` ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)), and `needs_split` / `allowlist_rejected` ([05-setup-judge](05-setup-judge.md)).
- **Launch**: a one-shot `claude --print --output-format stream-json` subprocess that renders `prompt_template_linear_updater` ([02-configuration](02-configuration.md)) against the directive payload (issue id + directive `kind` + structured fields) and performs Linear writes via the operator's installed Linear MCP. The daemon never issues a Linear write itself.
- **Supervision**: same lifecycle, stall detection, and stream-json parsing as the worker.
- **Bounded retry**: at most one immediate retry on non-clean exit; on persistent failure, log the failure (directive `kind`, issue id, underlying error) and continue. The daemon does not crash, block, or alter the per-issue state machine on linear-updater failure.
- **Sandbox**: filesystem read-only + elicitations rejected (per the strategy section above).

## Capabilities

- **One main worker invocation per ticket per attempt**: the daemon never relaunches the worker after a clean exit on its own initiative. The only re-launch paths are (a) the review gate's intentional `Deny+RetryWithContext` decision, and (b) `Backoff → Active` after a non-clean exit. Internal phase orchestration is the responsibility of the agent-side kiro skill.
- **`--max-turns` passthrough**: a per-invocation turn budget is configurable. The subprocess honors the budget.
- **Retryable causes** (consume the retry budget):
  1. Non-zero exit without a terminal `result` event
  2. Termination by signal
  3. Terminal `result` event reports a subtype that is retryable in the compiled mapping (at least `error_during_execution` as of the MVP build)
- **Non-retryable causes**: stall / max-turns reached / unknown subtype go straight to `Inactive(reason=...)` without consuming the retry budget.
- **Per-launch logging of the permission strategy**: each `--dangerously-skip-permissions` elevation decision is recorded on every worker launch.
- **linear-updater dispatch on every daemon-only failure**: every transition into a `failure`-flavored `Inactive(reason=...)` plus the `needs_split` / `allowlist_rejected` rejections triggers a linear-updater invocation for Linear-side surfacing.

## Boundaries

- **Driving the agent loop** is owned by the agent-side kiro skill, not the daemon.
- **Per-turn control / interruption / resumption** is out of scope (one main worker invocation = one lifecycle, except for the two re-launch paths above).
- **Semantic interpretation of subprocess output** is not done (only logging and stall detection).
- **Per-tool-granularity permission policy** is out of scope (only what Claude Code's interface allows).
- **Container / VM isolation** is out of scope (we depend on the `workspace-write` sandbox plus path safety).
- **The linear-updater is not a worker substitute**: it does not implement, review, open PRs, or write to the worktree. Its sole responsibility is translating directive payloads into Linear label/comment writes via the operator's Linear MCP. Worker-originated Linear writes (PR linkage, status comments produced by the kiro skill) go through the worker's own MCP path, not through the linear-updater.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Single bounded `claude ...` invocation per admitted issue"; Constraints > Permissions; Boundary Strategy > "Subprocess invocation taxonomy"
- **Requirements**:
  - `roki-mvp Req 5`: Bounded Subprocess Adapters (Worker, Judge, linear-updater)
  - `roki-mvp Req 5.10`: linear-updater invocation contract
  - `roki-mvp Req 9`: Permission Strategy and Default Sandbox
- **Design**:
  - `Engine Adapter` / `Permission Strategy` sections of `.kiro/specs/roki-mvp/design.md`
  - `.kiro/specs/roki-mvp/design-retry-policy.md`
- **Related FR**: 04-state-machine-and-recovery, 05-setup-judge, 06-worktree-and-session, 09-pre-pr-gate, 11-agent-tool-boundary, 14-operator-notifications
