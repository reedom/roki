---
refs:
  id: fr:07-worker-execution
  kind: fr
  title: "Phase Subprocess Execution"
  spec: roki-mvp
  implements:
    - req:roki-mvp:5
    - req:roki-mvp:5.10
    - req:roki-mvp:9
  related:
    - fr:01-daemon-lifecycle
    - fr:04-state-machine-and-recovery
    - fr:09-pre-pr-gate
    - fr:11-agent-tool-boundary
    - fr:14-operator-notifications
    - fr:18-worker-skill-workflow
    - fr:19-orchestrator-session
---

# FR 07: Phase Subprocess Execution

> Per-phase bounded `claude -p '/<kiro-skill> <args>' --output-format stream-json --max-turns N` subprocess lifecycle: launch flags, stall detection, stream-json parsing, exit translation into `phase_complete` / `phase_nonclean` events delivered to A. Includes the permission strategy for phase subprocesses, the per-phase `--max-turns` budget, and the rules for translating exits into events A acts on. The orchestrator session A's lifecycle is owned by [FR 19](19-orchestrator-session.md); this FR is the daemon-side phase-subprocess contract only.

## Purpose

Run the agent (whose orchestration is performed inside by a slash-command-driven kiro skill, or by a small daemon-internal prompt fragment for `open_pr` and `finalize_review`) as a **single bounded subprocess per phase A nominates**. The daemon stays a thin layer that only observes its lifecycle and forwards a structured exit envelope back to A; it does not drive the agent loop itself, and it does not select which phase runs next — A does (per [FR 19 §Event catalog](19-orchestrator-session.md), [FR 18: Phase Subprocess Catalog](18-worker-skill-workflow.md)).

The orchestrator session A is launched and supervised separately ([FR 19](19-orchestrator-session.md)); this FR does not restate A's launch flags or stall-window contract. The same engine adapter supervises both shapes (orchestrator and phase subprocess) using a uniform stream-json parser and stall detector ([FR 19 §Lifecycle](19-orchestrator-session.md)).

The permission strategy lets the operator pick "the safest profile that works" for phase subprocesses, plus a fallback "for when Claude's allowlist cannot be trusted". The orchestrator session A always runs with a read-only filesystem sandbox and has its own narrow tool surface, regardless of operator overrides ([FR 19 §Tool surface](19-orchestrator-session.md)).

## User-visible Behavior

### Launch and observation (phase subprocess)

- **Trigger**: A emits `action=run_phase` with `phase ∈ {implement, validate, open_pr, ci_fix, finalize_review}` and an optional bounded `additional_context` string.
- **Launch**: the daemon spawns one `claude -p '/<kiro-skill> <args>' --output-format stream-json --max-turns N` subprocess inside the issue's session tempdir (for `implement` / `validate` / `ci_fix`), or a `claude -p '<daemon-internal prompt>' --output-format stream-json --max-turns N` subprocess for `open_pr` and `finalize_review` (no skill). The daemon renders the per-phase context envelope, including A's `additional_context` verbatim through the engine adapter's `additional_context` channel (`req:roki-mvp:13.4`).
- **Slash-command headless**: slash commands are supported as the initial prompt argument in `-p` mode, including for skills whose manifest sets `disable-model-invocation: true` (e.g. `kiro-impl`).
- **Event handling**: stdout is parsed as a stream of newline-delimited JSON events → each event is parsed into a typed lifecycle event → emitted as a structured log + observability surface. **State machine transitions are driven only by subprocess exit and Linear state**, never by the contents of an event.
- **One phase at a time per ticket**: at most one phase subprocess is in flight per Linear issue identifier at any instant; the daemon does not deliver events to A while a phase subprocess is running ([FR 19 §Event catalog](19-orchestrator-session.md)).

### Permission strategy (phase subprocess)

The strategy is configured via the `[permissions].strategy` config and the `--dangerously-skip-permissions` CLI flag (canonical references: [02-configuration](02-configuration.md) / [01-daemon-lifecycle](01-daemon-lifecycle.md)).

- **Default sandbox**: `workspace-write` + elicitations rejected.
- **Override via `WORKFLOW.md`**: if an alternative sandbox / elicitation policy is declared there, it is applied to every phase subprocess.
- **`--settings` allowlist strategy**: pass the configured allowlist to each phase subprocess through Claude Code's settings interface.
- **`--dangerously-skip-permissions` strategy**: pass that flag to each phase subprocess, and log the elevated-permission decision per phase launch.
- **No strategy configured**: refuse to start.
- **Orchestrator session A is governed separately**: A always runs with a read-only filesystem sandbox; the `--dangerously-skip-permissions` fallback does **not** apply to A. A's tool surface is governed exclusively by `extension.orchestrator.allowed_tools` via `--settings` ([FR 19 §Tool surface](19-orchestrator-session.md), `req:roki-mvp:9.4`).

### Termination handling (phase subprocess → daemon → A)

The daemon translates each phase subprocess exit into a single event delivered on A's stdin. A makes the next-step decision; the daemon does not auto-retry on its own initiative.

- **Stall**: if no event arrives from the phase subprocess for longer than the configured per-phase stall window, the daemon SIGTERMs the subprocess and sends `phase_nonclean` (kind=`stall`) to A. A may re-nominate the same phase, fall through to a `ci_fix` phase, or `action=stop`. If A is no longer alive when the stall is detected, the daemon routes the issue to `Inactive(reason=stall)` and surfaces the failure via structured log + TUI escalation queue only ([FR 14: Operator Notifications](14-operator-notifications.md)).
- **`--max-turns` exhausted**: a terminal `result` event reports turn-budget exhaustion → `phase_nonclean` (kind=`max_turns_exhausted`, raw subtype forwarded). A decides whether to retry the phase, fall through to `ci_fix`, or `action=stop`.
- **Clean exit (success)**: a terminal `result` event with `subtype: success` → `phase_complete` to A with the parsed `result` envelope, `pr_url` (when `open_pr`), `review_artifact_path` (when `finalize_review`), and any phase-specific summary fields the skill emitted. A returns `action=run_phase` (next phase) or `action=stop`.
- **Non-clean exit (no terminal `result` event, signal-terminated, or terminal `result` with a non-`success` subtype)** → `phase_nonclean` to A with the failure classification. A's response drives whatever recovery happens; the daemon does not retry on its own.
- **Unknown `result.subtype`**: the daemon forwards the raw `subtype` value verbatim in the `phase_nonclean` payload (and captures it in the structured log) — A is alive to make the recovery judgment, so the daemon does not unilaterally route to `Inactive` for an unknown subtype (per `req:roki-mvp:5.9`).

### Ticket-level retry budget (A drives, daemon enforces the cap)

The ticket-level retry budget for phase non-clean exits (default 3 attempts, range 1–10) is **enforced by the daemon as a counter** but the retry decision itself belongs to A: each `phase_nonclean → run_phase` (re-nomination of the same phase) counts as one attempt against the budget. While remaining attempts exist, the daemon transitions the issue from `Active` to `Backoff`, applies exponential backoff between attempts bounded between ten seconds and five minutes, retains the session tempdir and worktree, and on timer expiry transitions back to `Active` for A's next phase nomination. When the budget is exhausted, the daemon sends a `daemon_directive` event with kind `retry_exhausted` to A so that A surfaces the failure to Linear via Linear MCP and emits its own `action=stop` with `outcome=failure` (per `req:roki-mvp:5.10`).

The daemon does not auto-retry a phase: A must return `action=run_phase` for the same `phase` for the daemon to spend a retry slot. A may also choose to fall through to a `ci_fix` phase, change `additional_context`, or `action=stop` — those choices are A's, and only same-phase re-nominations count against the retry budget.

The review-gate intentional re-launch is a separate path: `Deny+RetryWithContext(payload)` from the review gate translates into a `gate_deny` event on A's stdin, after which A returns `action=run_phase` with `phase=implement` and populated `additional_context` (per [FR 19 §Event catalog](19-orchestrator-session.md), [FR 09: Pre-PR Gate](09-pre-pr-gate.md)). The review-gate retry budget is owned by [FR 09](09-pre-pr-gate.md) and is independent of the phase-non-clean retry budget above.

### Daemon-only failure surfacing (no linear-updater)

Daemon-only failures (phase stall after the daemon killed the subprocess; phase non-clean retry-budget exhaustion; review-gate retry exhaustion; filesystem poison; restart-recovery orphan) are surfaced through `daemon_directive` events on A's stdin per [FR 14: Operator Notifications](14-operator-notifications.md). When A is alive, A writes the appropriate Linear label + comment via Linear MCP and returns `action=linear_update_done`. When A is dead — `orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted` — there is no Linear-side notification; the daemon logs structurally and populates the TUI escalation queue. The previously specified linear-updater subagent is removed; the `daemon_directive → A → Linear MCP` path is its full replacement.

## Capabilities

- **One phase subprocess per A-nominated phase**: the daemon never re-launches a phase on its own initiative. The only re-launch paths are (a) A returns `action=run_phase` for the same phase after a `phase_nonclean` (consumes one retry slot), and (b) A returns `action=run_phase` with `phase=implement` after a `gate_deny` event (review-gate intentional re-launch, owned by [FR 09](09-pre-pr-gate.md)).
- **`--max-turns` passthrough**: a per-phase turn budget is configurable. Each phase subprocess honors its own budget; the orchestrator session A is bounded by `max_phases` instead ([FR 19](19-orchestrator-session.md)).
- **Retryable phase-nonclean classifications**: the daemon classifies the `phase_nonclean` payload but does not retry on its own; A's `run_phase` decision is what consumes a retry slot. The compiled subtype mapping (e.g. `error_during_execution` as of the MVP build) flows verbatim into the `phase_nonclean` payload so A can decide.
- **Per-launch logging of the permission strategy**: each `--dangerously-skip-permissions` elevation decision is recorded on every phase launch.
- **`daemon_directive → A` for daemon-only failures**: every transition into a non-auto-cleanup `Inactive(reason=...)` (other than the three orchestrator-dead reasons) routes through `daemon_directive` for Linear-side surfacing while A is alive. The daemon never writes Linear directly.

## Boundaries

- **Driving the agent loop** is owned by the agent-side kiro skill (or the daemon-internal prompt fragment for `open_pr` / `finalize_review`), not the daemon.
- **Selecting which phase runs next** is owned by A's `action=run_phase` directive, not the daemon. The daemon never picks a phase on its own.
- **Per-turn control / interruption / resumption** is out of scope (one phase invocation = one lifecycle, except for the two re-launch paths above).
- **Semantic interpretation of subprocess output** is not done (only structured-event logging, stall detection, and exit translation).
- **Per-tool-granularity permission policy** is out of scope (only what Claude Code's interface allows).
- **Container / VM isolation** is out of scope (we depend on the `workspace-write` sandbox plus path safety for phase subprocesses; A runs read-only).
- **The orchestrator session A's lifecycle, tool surface, response schema, and failure modes** are owned by [FR 19](19-orchestrator-session.md); this FR does not restate them.
- **Linear writes from inside a phase subprocess** (PR linkage, status comments produced by the kiro skill via Linear MCP) go through the operator's Claude Code tool surface unchanged — not through any daemon-side write path. Daemon-only failure Linear writes are exclusively A's job, not the phase subprocess's.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Phase subprocesses for code-changing work"; Constraints > Engine ("Phase subprocess (short-lived, one per phase)"); Constraints > Permissions; Boundary Strategy > "Orchestrator-vs-phase boundary".
- **Requirements**:
  - `req:roki-mvp:5`: Bounded Subprocess Adapters (Orchestrator Session A and Phase Subprocesses)
  - `req:roki-mvp:5.6`: Phase subprocess spawn contract on A's `action=run_phase`
  - `req:roki-mvp:5.7`: Per-phase stall detection → `phase_nonclean` to A
  - `req:roki-mvp:5.8`: Phase clean / non-clean exit translation
  - `req:roki-mvp:5.9`: Unknown `subtype` forwarded raw to A
  - `req:roki-mvp:5.10`: Retry budget exhaustion → `daemon_directive (kind=retry_exhausted)`
  - `req:roki-mvp:9`: Permission Strategy and Default Sandbox (phase subprocesses)
- **Design**:
  - `Engine Adapter` / `Permission Strategy` sections of `.kiro/specs/roki-mvp/design.md`
  - `.kiro/specs/roki-mvp/design-retry-policy.md`
- **Related FR**: [01-daemon-lifecycle](01-daemon-lifecycle.md), [04-state-machine-and-recovery](04-state-machine-and-recovery.md), [09-pre-pr-gate](09-pre-pr-gate.md), [11-agent-tool-boundary](11-agent-tool-boundary.md), [14-operator-notifications](14-operator-notifications.md), [18-worker-skill-workflow](18-worker-skill-workflow.md), [19-orchestrator-session](19-orchestrator-session.md)
