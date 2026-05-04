---
refs:
  id: fr:19-orchestrator-session
  kind: fr
  title: "Orchestrator Session"
  spec: roki-mvp
  related:
    - fr:01-daemon-lifecycle
    - fr:04-state-machine-and-recovery
    - fr:07-worker-execution
    - fr:11-agent-tool-boundary
    - fr:12-extension-surface
    - fr:14-operator-notifications
    - fr:18-worker-skill-workflow
---

# FR 19: Orchestrator Session

> The long-lived per-ticket `claude --input-format stream-json --output-format stream-json` "thinking" component (A) that absorbs admission decisions, phase planning, artifact validation (`requirements.md` after `materialize_spec`, `review.md` after `finalize_review`), daemon-directive interpretation, and Linear writes — driving zero or more short-lived phase subprocesses for the actual code-changing work.

## Purpose

The previous architecture launched a single bounded `claude --print` worker per admitted issue plus two auxiliary one-shot subprocesses (a setup judge for admission classification and a linear-updater subagent for daemon-only failure surfacing) and two daemon-side mechanical artifact-validation gates (kiro-spec gate at `Judging → Active`, kiro-review gate at `Active → Inactive`). That model gave the daemon no structured handle for per-phase budgets, no clean way to cap thinking effort, forced two extra prompt templates and lifecycles to surface admission and Linear writes, and split artifact validation across an LLM substantive layer (kiro-discovery / per-task `kiro-review` / `kiro-validate-impl` / `finalize_review` synthesis) plus a daemon-side mechanical structural layer with its own config namespace, retry counters, and `Inactive.reason` discriminator values.

This FR canonicalizes the replacement: a long-lived **orchestrator session** (A), launched per ticket as `claude --input-format stream-json --output-format stream-json`, that absorbs all three roles — auxiliary admission classification, daemon-only failure surfacing via Linear writes, and structural artifact validation — and emits phase requests as strict JSON action directives. The actual code-changing work runs in short-lived phase subprocesses ([18-worker-skill-workflow](18-worker-skill-workflow.md)) that A nominates via `run_phase`. A is the only "thinking" component the daemon launches per ticket; it is where admission classifies, phases get planned, artifacts get validated, daemon-only failure directives get translated into Linear writes, and the ticket's terminal `stop` action gets emitted. A does not edit code (no Edit, no Write) but it can run shell and read files (Bash + Read inside a read-only filesystem sandbox) so it can `stat` / `test -f` / `grep` artifacts itself. The daemon itself never writes Linear directly and never decides whether an artifact passes — both are A's responsibility.

Rejected alternatives: keeping the prior worker shape and adding a separate per-phase planner (would have created a third "thinking" surface with no clear seam to the existing two); driving phase choice from the daemon ("what's next") in Rust (conflicts with the skill-first pivot — the daemon is mechanical observation only); keeping the prior daemon-side mechanical kiro-spec / kiro-review gates alongside A (creates an LLM-free witness at the cost of a second config namespace, two `kiro_*_status` agent tools, dedicated `Inactive.reason` values, and split-brain artifact validation — A already has Read + Bash and can do the structural check itself); deferring A's artifact-validation responsibility to a Rust-native read-only check tool (out of MVP scope — built when A's Bash usage stabilizes into a fixed pattern, at which point Bash can be dropped from A's allowlist).

## User-visible Behavior

### Lifecycle

- **Launch**: A is launched once per ticket on the `Discovered → Pending` transition, so it is already running when the orchestrator publishes the `Pending → Judging` transition that fires the first `admission_request` event. Launch happens inside the issue's session tempdir; tool surface is restricted via `--settings` (see §Tool surface). The `prompt_template_orchestrator` block from `WORKFLOW.md` ([12-extension-surface](12-extension-surface.md)) is rendered as the system prompt.
- **Steady state**: the daemon writes JSON events to A's stdin; A returns one strict JSON action object per turn on stdout (after any extended-thinking block). The daemon does not interpret A's reasoning text — only the JSON action field.
- **Graceful termination**: A is gracefully terminated when the issue lands in any `Inactive(reason=*)` state and any A-driven Linear writes for that terminal state have completed. The daemon sends a final `stop`-acknowledgement signal then closes A's stdin and waits for clean exit within the configured shutdown window ([01-daemon-lifecycle](01-daemon-lifecycle.md)).
- **Forced termination**: `Cleaning` (entered on tracker-observed terminal Linear state or assignment loss, per [04-state-machine-and-recovery](04-state-machine-and-recovery.md)) may force-terminate A regardless of in-flight turns — cleanup of worktree / session tempdir takes priority.
- **Restart non-persistence**: A is not persisted across daemon restarts. A new A is launched fresh when the issue re-enters `Pending` via restart-recovery (per `roki-mvp Req 3.14` / `Req 10`). In-flight turns and any A-internal scratch state are discarded; the next A starts from the rendered system prompt.

### Event catalog (daemon → A stdin)

The daemon translates state-machine and subprocess-lifecycle observations into JSON events on A's stdin. Each event is a single JSON object on its own line. The current event catalog:

| event | trigger | A action expected |
|---|---|---|
| `admission_request` | `Pending → Judging` published by the orchestrator | return `action=admission_decision` with `judge` ∈ `act` / `noop` / `needs_split` / `allowlist_rejected`; for the `needs_split` and `allowlist_rejected` variants A also writes the matching Linear label + comment via Linear MCP in the same turn |
| `phase_complete` | a phase subprocess clean-exited with `subtype: success` | for `materialize_spec` / `finalize_review` A first reads the produced artifact (see §Artifact validation) and decides; otherwise A returns `action=run_phase` (next phase) or `action=stop` (`outcome=success` or `outcome=failure`) |
| `phase_nonclean` | phase non-zero exit, stall, or per-phase `--max-turns` exhaustion | judgment call by A — re-run the same phase, fall through to a `ci_fix` phase, or `action=stop` |
| `daemon_directive` | a daemon-only failure that A is alive to surface (stall on a sibling phase, retry exhaustion, filesystem poison, etc.) | A writes the appropriate Linear label + comment via Linear MCP and returns `action=linear_update_done` |
| `tracker_terminal` | Linear state moved to `done` / `canceled` or assignment was lost ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)) | return `action=stop` with `outcome=cancelled`; cleanup follows |

The daemon does not deliver events to A while a phase subprocess is running — it waits for the phase to terminate, observes the exit, then sends the matching `phase_complete` / `phase_nonclean`.

### Response schema (strict JSON)

After any extended-thinking block, A emits exactly one JSON object per turn on stdout. The daemon parses the **last** JSON object on A's stdout per turn; earlier emissions are advisory progress and do not influence the state machine.

```json
{
  "action": "admission_decision" | "run_phase" | "linear_update_done" | "stop",
  "phase": "implement" | "validate" | "open_pr" | "ci_fix" | "finalize_review" | null,
  "judge": "act" | "noop" | "needs_split" | "allowlist_rejected" | null,
  "repo": "<ghq-id>" | null,
  "rejected_repos": ["<ghq>", ...] | null,
  "additional_context": "<bounded string>" | null,
  "outcome": "success" | "failure" | "cancelled" | null,
  "linear_writes": ["label:<name>", "comment_posted:<id>", ...] | null,
  "reason": "<= 200 chars"
}
```

Field semantics:

- `action` is always required; the other fields are populated only when meaningful for the chosen action.
- `phase` is required when `action=run_phase`; the catalog matches the phase set in [18-worker-skill-workflow](18-worker-skill-workflow.md).
- `judge` and `repo` (and `rejected_repos` for the rejection variants) are required when `action=admission_decision`.
- `additional_context` is the verbatim payload forwarded to the next phase subprocess via the engine adapter's `additional_context` channel ([12-extension-surface](12-extension-surface.md)).
- `outcome` is required when `action=stop` and is the input that drives the issue's terminal `Inactive.reason` selection on the daemon side ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)).
- `linear_writes` is required when `action=linear_update_done` (and on the rejection variants of `admission_decision`); it lists what A wrote in this turn so the daemon can log the side effect and detect partial writes.
- `reason` is bounded human-readable rationale for the structured log; it is **not** a state-machine input.

### Tool surface

A's tool surface is enforced by the daemon via `--settings` at launch and is independent of phase subprocesses' tool surface ([11-agent-tool-boundary](11-agent-tool-boundary.md)):

- **Linear MCP (write)** — the operator's installed Linear MCP, used by A for label + comment writes (admission rejections, daemon-directive surfacing, and artifact-validation retry-budget exhaustion).
- **Read** (workspace, read-only) — A reads ticket-related files inside the worktree, including the artifacts it validates (`requirements.md` after `materialize_spec`, `review.md` after `finalize_review`).
- **Bash** (read-only filesystem sandbox) — A runs read-only shell commands for artifact validation: `stat`, `test -f`, `grep -E` for EARS keywords in `requirements.md`, schema spot-checks on `review.md` per-criterion entries, and `test -f` for each `code_references` entry's reachability. Bash invocations execute inside the read-only filesystem sandbox so they cannot mutate the worktree or session tempdir even if A or its prompt accidentally tries.
- **No** Edit, **no** Write, **no** Agent dispatch, **no** other MCPs.

The session runs with a read-only filesystem sandbox regardless of operator overrides (per `roki-mvp` Constraints > Permissions). Phase subprocesses inherit the operator's broader tool surface separately; A's narrow surface does not constrain them.

A future Rust-native read-only artifact-validation tool (`roki-check requirements <path>`, `roki-check review <path>`) is a deferred MVP-out item: when A's Bash usage stabilizes into a fixed pattern, that tool can replace the Bash usage and Bash can be removed from `extension.orchestrator.allowed_tools`. Until then Bash is the simplest path; the read-only filesystem sandbox bounds the blast radius.

### Artifact validation

A is the structural witness for two artifacts produced inside phase subprocesses:

- **`requirements.md`** — produced by the `materialize_spec` phase ([18-worker-skill-workflow](18-worker-skill-workflow.md)). After the `phase_complete(materialize_spec)` event A reads the artifact (canonical path in [ref:artifacts](../reference/artifacts.md)) and checks: file presence, non-empty, encoding-sane, at least one EARS trigger keyword (`WHEN` / `IF` / `WHILE` / `WHERE` / `SHALL`) at an acceptance-criteria position. On pass, A nominates the next phase (typically `implement`). On structural failure with retry budget remaining, A re-nominates `materialize_spec` with `additional_context` set to the failure detail (e.g. "missing EARS trigger keyword in section 3"). On retry-budget exhaustion, A writes the matching Linear label + comment via Linear MCP and emits `action=stop` with `outcome=failure`.
- **`review.md`** — produced by the `finalize_review` phase ([18-worker-skill-workflow](18-worker-skill-workflow.md)). After the `phase_complete(finalize_review)` event A reads the artifact (canonical path in [ref:artifacts](../reference/artifacts.md)) and checks: file presence, schema (overall `status` of `pass | fail`, per-criterion entries indexed by the numeric requirement IDs in `requirements.md`, `code_references` on each `pass` entry), and reachability of each `code_references` path via `test -f`. On pass with overall `status=pass`, A emits `action=stop` with `outcome=success`. On structural failure (missing artifact, schema invalid, unreachable code reference) or overall `status=fail` with retry budget remaining, A re-nominates the `implement` phase with `additional_context` populated from the failing per-criterion entries (criterion id, fail reason, diagnostic text). On retry-budget exhaustion, A writes the matching Linear label + comment via Linear MCP and emits `action=stop` with `outcome=failure`.

Substantive judgment of "does the code satisfy the criterion" is owned by the kiro skill set inside the prior phase subprocesses (`kiro-discovery` for `requirements.md`; per-task `kiro-review` plus `kiro-validate-impl` plus `finalize_review` synthesis for `review.md`). A's role is structural-only: file presence, non-emptiness, schema shape, EARS-keyword presence, code-reference reachability. A does not re-judge whether a criterion is correct.

The retry budget per artifact is A-internal (drawn from `prompt_template_orchestrator`; default suggestion: 2 retries for `materialize_spec`, 3 retries for `review.md`) and is bounded overall by `max_phases`. There is no per-artifact `max_attempts` config knob; operators tune the limit by editing `prompt_template_orchestrator` or `max_phases`.

When the artifact is structurally invalid for non-recoverable reasons (e.g. operator deleted `requirements.md` between phases — caught by A's pre-`finalize_review` check), A treats it as the `requirements.md` retry path: re-nominate `materialize_spec` with `additional_context` describing the corruption.

### Configuration

Configuration lives under the reserved `extension.orchestrator.*` namespace in `WORKFLOW.md` ([12-extension-surface](12-extension-surface.md), per `roki-mvp Req 6.5`). Canonical defaults:

| key | default | meaning |
|---|---|---|
| `model` | `"claude-opus-4-7"` | Claude model identifier for A |
| `effort` | `"middle"` | Extended-thinking budget; range `low` / `middle` / `high` |
| `max_phases` | `20` | Total phase subprocesses A may nominate before the budget is exhausted |
| `allowed_tools` | Linear MCP (write) + `Read` + `Bash` | Allowlist passed via `--settings`. `Bash` is included so A can run read-only artifact-validation commands (`stat`, `test -f`, `grep -E`); the read-only filesystem sandbox prevents mutation regardless |

A single named template block — `prompt_template_orchestrator` — drives the system prompt. It replaces the previous three template blocks (`prompt_template_worker`, `prompt_template_judge`, `prompt_template_linear_updater`); the latter two are removed alongside their corresponding subprocess shapes. Hot-reload of the namespace applies from the next ticket admission; an in-flight A keeps its rendered prompt for the lifetime of the session.

### Failure modes

When A is alive, A is responsible for surfacing daemon-only failures to Linear via the `daemon_directive` event path; this replaces the previous one-shot linear-updater subprocess. When A is dead — process crash, persistent JSON schema drift, or `max_phases` exhausted — the daemon **does not** fall back to a Linear write of its own. Instead it routes the issue to one of three new `Inactive.reason` values, logs the event structurally, and populates the TUI escalation queue ([14-operator-notifications](14-operator-notifications.md)). Operators notice these three cases via the TUI; there is no Linear-side notification.

| failure | action | Inactive.reason |
|---|---|---|
| A crash / SIGSEGV / non-zero exit without a `stop` action | log + escalation queue entry; no Linear write | `orchestrator_crash` |
| A schema drift on two consecutive turns (after one daemon-side reprompt) | log + escalation queue entry; no Linear write | `orchestrator_unparseable` |
| `max_phases` exhausted (A would nominate another phase but the budget is gone) | log + escalation queue entry; no Linear write | `orchestrator_budget_exhausted` |
| A stall (no event emitted in N seconds, configurable) | daemon SIGTERMs A → routes through the crash path | `orchestrator_crash` |
| Linear MCP write failure inside an A turn | A retries up to once internally, then surfaces a partial-write entry in `linear_update_done.linear_writes`; daemon does **not** retry on A's behalf | n/a (turn continues) |

The first three reasons are in addition to the existing `Inactive.reason` discriminator set ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)). They are **not** auto-cleanup eligible: the worktree / session tempdir is preserved until the operator manually closes the Linear ticket, after which `Cleaning` proceeds.

## Capabilities

- **Single thinking component per ticket**: A absorbs admission classification, phase planning, artifact validation, daemon-directive surfacing, and Linear writes into one long-lived session. The previous setup-judge subprocess, linear-updater subagent, and daemon-side mechanical kiro-spec / kiro-review gates are removed.
- **Strict JSON action contract**: A's stdout is a small action enum with a bounded response schema; the daemon parses the last JSON object per turn and ignores reasoning text.
- **Bounded thinking budget**: `extension.orchestrator.{model, effort, max_phases, allowed_tools}` lets the operator cap A's thinking effort (`effort`) and total phase nominations (`max_phases`) without per-process `--max-turns`.
- **Read + Bash + Linear-MCP-write only**: enforced by `--settings`; A cannot edit code, dispatch agents, or mutate the filesystem (Bash runs inside a read-only sandbox). Code-changing work is the phase subprocess's role.
- **A-driven artifact validation**: A reads `requirements.md` after `materialize_spec` and `review.md` after `finalize_review`, structural-only, with retry-with-context re-nomination of the producing phase on failure. No daemon-side gate hook subscribes to state transitions to do this.
- **Single template block**: `prompt_template_orchestrator` replaces the prior three blocks; one prompt covers admission, phase planning, artifact validation, and daemon-directive surfacing.
- **Distinct dead-A failure path**: three new `Inactive.reason` values plus the TUI escalation queue cover the cases where A cannot surface to Linear itself, without forcing the daemon to hold a Linear write path.

## Boundaries

- **A does not edit code, invoke `gh`, or push to git** — that is the phase subprocesses' role ([18-worker-skill-workflow](18-worker-skill-workflow.md)). A can run shell (Bash) but only read-only inside the sandbox; mutation requires a phase subprocess.
- **The daemon does not interpret A's reasoning text** — only the JSON action field; thinking blocks are ignored.
- **`max_turns` is a per-phase budget, not an A budget** — A is bounded by `max_phases` instead. There is no per-turn budget on A.
- **A is not persisted across daemon restarts** — restart-recovery starts a fresh A when the issue re-enters `Pending` (per `roki-mvp Req 3.14` / `Req 10`).
- **Phase subprocess catalog is owned by [18-worker-skill-workflow](18-worker-skill-workflow.md)** — this FR enumerates the action enum's `phase` values for completeness but does not restate per-phase contracts.
- **A's context compaction across long-running tickets** is out of MVP scope (deferred — Phase 2 feature). For MVP the `max_phases` budget bounds session length and operators retry from scratch when it is hit.
- **Multi-engine portability of the orchestrator role** is out of scope (A is written against Claude Code stream-json; cross-engine adapters are deferred).
- **Daemon-side recovery of a partially-completed Linear write after A crash** is out of scope. There is no fallback channel; the partial write is surfaced via the TUI escalation queue and the operator reconciles Linear manually.
- **Daemon-driven Linear writes** are not introduced here; the daemon never writes Linear directly. When A is dead, the failure surfaces via TUI only.

## Traceability

- **Roadmap**: `roadmap.md` > Overview ("a long-lived orchestrator session"); Scope > In ("Long-lived **orchestrator session** per admitted issue"); Constraints > Engine ("Orchestrator session (long-lived)"); Boundary Strategy > "Orchestrator-vs-phase boundary".
- **Requirements**: TBD — concrete `req:roki-mvp:*` IDs are added by the requirements rewrite that succeeds the Stage A roadmap rewrite. Until then this FR documents the orchestrator-session contract that complements the daemon-side requirements in `roki-mvp` Req 5 / 7 / 8 / 9 / 13 and replaces the obsolete `extension.linear_updater.*` namespace from prior `Req 2`.
- **Design**: TBD — `.kiro/specs/roki-mvp/design.md` will gain an `Orchestrator Session` section in a later stage; this FR is the placeholder of record until then.
- **Related FR**: [01-daemon-lifecycle](01-daemon-lifecycle.md), [04-state-machine-and-recovery](04-state-machine-and-recovery.md), [07-worker-execution](07-worker-execution.md), [11-agent-tool-boundary](11-agent-tool-boundary.md), [12-extension-surface](12-extension-surface.md), [14-operator-notifications](14-operator-notifications.md), [18-worker-skill-workflow](18-worker-skill-workflow.md).
