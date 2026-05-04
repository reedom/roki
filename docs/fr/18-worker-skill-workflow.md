---
refs:
  id: fr:18-worker-skill-workflow
  kind: fr
  title: "Phase Subprocess Catalog"
  related:
    - fr:07-worker-execution
    - fr:11-agent-tool-boundary
    - fr:14-operator-notifications
    - fr:19-orchestrator-session
---

# FR 18: Phase Subprocess Catalog

> The catalog of bounded `claude -p '/<kiro-skill> <args>' --output-format stream-json` phase subprocesses the orchestrator session A nominates per ticket: which kiro skill (or daemon-internal prompt fragment) drives each phase, the per-phase exit envelope A reads, and the artifacts produced inside selected phases (`requirements.md` from `materialize_spec`, `review.md` from `finalize_review`) that A then validates structurally per [FR 19 §Artifact validation](19-orchestrator-session.md).

## Purpose

[FR 19: Orchestrator Session](19-orchestrator-session.md) describes the long-lived "thinking" component A that classifies admission, plans phases, validates produced artifacts (`requirements.md` after `materialize_spec`, `review.md` after `finalize_review`), processes daemon-only failure directives, and writes Linear via Linear MCP. A does not edit code; the actual code-changing work runs in **short-lived bounded phase subprocesses** that A nominates via `action=run_phase`. [FR 07: Worker Execution](07-worker-execution.md) describes the engine adapter's mechanical supervision of those subprocesses (launch flags, stall detection, stream-json parsing, exit translation into `phase_complete` / `phase_nonclean` events).

This FR canonicalizes the **per-phase catalog**: which slash-command-driven kiro skill (or daemon-internal prompt fragment) drives each phase, what the daemon expects in the structured exit envelope, and what A reads from the resulting `phase_complete` / `phase_nonclean` event to decide its next action. Spec presence at admission and review pass at PR-readiness are not separate daemon-side gates — A reads `requirements.md` after `materialize_spec` clean exit and `review.md` after `finalize_review` clean exit and decides next-step itself per [FR 19 §Artifact validation](19-orchestrator-session.md).

## User-visible Behavior

### Phase catalog (one bounded subprocess per phase A nominates)

A's `action=run_phase` directive carries a `phase` value drawn from the catalog below. For each phase the daemon spawns a single `claude -p '/<kiro-skill> <args>' --output-format stream-json` subprocess (or a small daemon-internal prompt fragment for `open_pr` and `finalize_review`) inside the issue's session tempdir, with its own `--max-turns` budget and the per-phase context envelope (including A's `additional_context` verbatim through the engine adapter's `additional_context` channel — see `req:roki-mvp:13`).

| `phase` | Invocation | Skill (or prompt) | Purpose |
|---|---|---|---|
| `materialize_spec` | `claude -p '/kiro-discovery <issue>' --output-format stream-json --max-turns N` | `kiro-discovery` | Synthesize the per-issue `requirements.md` (canonical path in [ref:artifacts](../reference/artifacts.md)) by merging the Linear ticket body and the project's existing EARS docs under `.kiro/specs/`. Produces `requirements.md` only — not `spec.json`, `design.md`, or `tasks.md` (see §Open issues). After clean exit A reads the artifact and validates it structurally per [FR 19 §Artifact validation](19-orchestrator-session.md); on structural failure with retry budget remaining A re-nominates `materialize_spec` with `additional_context` populated from the failure detail. |
| `implement` | `claude -p '/kiro-impl <feature>' --output-format stream-json --max-turns N` | `kiro-impl` | Drives the implementer-then-reviewer loop per task in autonomous mode (no task numbers): for each pending task, dispatch a fresh implementer subagent (TDD-first), then dispatch `kiro-review` as an independent reviewer subagent. The skill's internal loop handles reviewer rejections by remediation + re-review until APPROVED, marks the task `[x]`, and proceeds. Counters live inside `kiro-impl`. |
| `validate` | `claude -p '/kiro-validate-impl <feature>' --output-format stream-json --max-turns N` | `kiro-validate-impl` | Cross-task feature-level validation after `implement` reports done. Catches cross-task issues that per-task `kiro-review` cannot see (cross-task boundary spillover, integration seams, full-suite regressions). Output: `GO` / `NO_GO`. A decides whether to re-run `implement` with `additional_context` populated from validation findings, fall through to `ci_fix`, or `action=stop`. |
| `open_pr` | `claude -p '<daemon-internal prompt fragment>' --output-format stream-json --max-turns N` | (no skill) | `gh pr create` via Bash. The PR description embeds a brief change summary plus the validation outcome A passes through `additional_context`. |
| `ci_fix` | `claude -p '/kiro-debug <feature>' --output-format stream-json --max-turns N` | `kiro-debug` (with `kiro-verify-completion` used internally as the fresh-evidence gate) | On CI red, root-cause-first analysis + minimal fix + push. Each push attempt is gated by `kiro-verify-completion` (claim type `TEST_OR_BUILD`) before exit. A nominates this phase (typically after a `validate`-derived `phase_nonclean` or a poll observation A maintains in conversation context); the daemon does not auto-poll CI for A. |
| `finalize_review` | `claude -p '<daemon-internal synthesis prompt>' --output-format stream-json --max-turns N` | (no skill) | Synthesize the structured `review.md` artifact at the path documented in [ref:artifacts](../reference/artifacts.md), drawing on the verdicts already accumulated this session (per-task `kiro-review` APPROVED set, `kiro-validate-impl` GO, the verify-cmd outcome, any `kiro-verify-completion` VERIFIED stamps, and the artefacts in the worktree). After clean exit A reads the artifact and validates it structurally per [FR 19 §Artifact validation](19-orchestrator-session.md); on structural failure or overall `status=fail` with retry budget remaining A re-nominates `implement` with `additional_context` populated from the failing per-criterion entries. |

Slash commands work as the initial prompt argument in `-p` mode (the prompt string is parsed before headless takes over), so a SKILL.md `disable-model-invocation: true` flag (e.g. on `kiro-impl`) does not prevent the daemon from launching that skill via `/<skill> <args>`. The same property lets the daemon launch `kiro-discovery` for the `materialize_spec` phase even though `kiro-discovery` is otherwise an operator-side authoring skill — the slash-command entry path is unaffected by the model-invocation flag. Other authoring-time skills (`kiro-spec-init`, `kiro-spec-requirements`, `kiro-spec-design`, `kiro-spec-tasks`, `kiro-spec-batch`, `kiro-spec-quick`, `kiro-spec-status`, `kiro-validate-design`, `kiro-validate-gap`, `kiro-steering`, `kiro-steering-custom`) are operator-side only and are not invoked by the daemon.

A Type B (with-human-planning) ticket is handled inside an `implement` phase: the phase agent uses the operator-installed Linear MCP (per [FR 11: Agent Tool Boundary](11-agent-tool-boundary.md)) to post questions as ticket comments and poll for replies. There is no dedicated `kiro-plan-with-human` skill today — this is normal Claude Code session work using Linear MCP and Bash, scoped to the same `implement` phase budget.

### Skill set (verified manifests)

The runtime skills below all exist at `.claude/skills/kiro-*/SKILL.md` (project) or `~/.claude/skills/kiro-*/SKILL.md` (operator). The daemon does not introduce roki-specific subagents beyond what these skills already compose internally.

| Skill | Used in phase | Purpose | Tool scope (per skill manifest) |
|---|---|---|---|
| `kiro-discovery` | `materialize_spec` | Synthesize the per-issue `requirements.md` by merging the Linear ticket body and existing project EARS docs under `.kiro/specs/`. Authoring-time skill repurposed by the daemon via slash-command headless invocation. | per skill manifest |
| `kiro-impl` | `implement` | Drives the implementer-then-reviewer loop per task; owns TDD discipline, validation-command discovery, and per-task remediation. Manifest carries `disable-model-invocation: true` (slash-command entry only). | Read, Write, Edit, MultiEdit, Bash, Glob, Grep, Agent, WebSearch, WebFetch |
| `kiro-review` | `implement` (dispatched by `kiro-impl` as an independent reviewer subagent) | Adversarial per-task review against approved spec + boundary; APPROVED / REJECTED. | Read, Bash, Grep, Glob |
| `kiro-validate-impl` | `validate` | Cross-task feature-level validation after all tasks complete; GO / NO_GO. | Read, Bash, Grep, Glob, Agent |
| `kiro-debug` | `ci_fix` | Root-cause-first failure analysis; used on CI red to propose a minimal fix. | per skill manifest |
| `kiro-verify-completion` | `ci_fix` (internal) | Refuses success claims that lack fresh evidence; VERIFIED / NOT_VERIFIED / MANUAL_VERIFY_REQUIRED. | Read, Bash, Grep, Glob |

### Per-phase exit envelope (phase subprocess → daemon → A)

The phase subprocess emits a terminal stream-json `result` event on clean exit. The daemon does not interpret reasoning text (per [FR 07: Worker Execution](07-worker-execution.md) §Boundaries); it observes the structured `result` and translates the outcome into one of two events delivered to A's stdin:

- **`phase_complete`** — clean exit with `result.subtype = success`. Payload includes `phase` (which one just ran), the parsed `result` envelope, the `pr_url` when the phase produced one (currently `open_pr`), the `review_artifact_path` when the phase wrote one (`finalize_review`), and any phase-specific summary fields the skill emitted in its `result` payload. A reads this and returns either `action=run_phase` (next phase) or `action=stop` (`outcome=success` or `outcome=failure`).
- **`phase_nonclean`** — non-zero exit, signal termination, stall-detected termination, exhausted `--max-turns`, or terminal `result` with a non-`success` subtype (including a subtype the daemon's compiled mapping does not recognize, in which case the raw subtype is forwarded to A — see [FR 07](07-worker-execution.md) §Termination handling). Payload includes the failure classification, the raw subtype when applicable, and the verbatim phase-`additional_context` A had passed in. A reads this and decides whether to re-run the same phase, fall through to a `ci_fix` phase, or `action=stop`.

The standalone `ROKI_RESULT:` final-line contract from the prior single-worker model is **removed**. Each phase emits its own structured exit through the stream-json `result` event, and the daemon maps the `result` into the `phase_complete` / `phase_nonclean` events A consumes. A's strict JSON action contract ([FR 19 §Response schema](19-orchestrator-session.md)) is the only "thinking" surface the daemon parses; intra-phase advisory progress markers stay in the structured log but are not state-machine inputs.

### Acceptance criteria convention (EARS)

Every Linear ticket eligible for roki dispatch MUST include an `## Acceptance Criteria` section in its body, written in EARS style — preferably the Ubiquitous (`The X shall Y.`) and Event-driven (`When <trigger>, the X shall Y.`) patterns. Without explicit, written criteria, there is no objective basis for `review.md` to populate per-criterion entries; A is expected to refuse such tickets at admission via `judge=noop` (per [FR 19 §Event catalog](19-orchestrator-session.md)).

### Produced artifacts

Two phases write artifacts at canonical paths under `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/` ([ref:artifacts](../reference/artifacts.md)). Neither is written under the session tempdir.

- `materialize_spec` writes `requirements.md`. After clean exit A reads it and validates structurally per [FR 19 §Artifact validation](19-orchestrator-session.md).
- `finalize_review` writes `review.md`. After clean exit A reads it and validates structurally per [FR 19 §Artifact validation](19-orchestrator-session.md).

The schemas are the canonical ones defined in [ref:artifacts](../reference/artifacts.md). For `review.md`: an overall `status` of `pass | fail`, per-criterion entries indexed by the numeric requirement ID drawn from `requirements.md`, and `code_references` (an array of workspace-relative file paths with optional line range) on each entry whose status is `pass`. This FR does not introduce a new schema; it only specifies which phase produces which artifact.

### Cancellation / timeout

If the daemon SIGTERMs a phase subprocess (per [FR 07: Worker Execution](07-worker-execution.md) stall handling), the active skill SHOULD treat it as cancellation: stop dispatching new subagents, allow the current subagent to wind down briefly, and exit. The worktree and session tempdir are preserved so a human can inspect intermediate state. The daemon then sends `phase_nonclean` to A with the stall classification; A decides whether to retry, fall through to `ci_fix`, or `action=stop`.

## Capabilities

- **Per-phase isolation**: each phase = its own bounded `claude -p` subprocess with its own `--max-turns` budget, its own slash-command-driven kiro skill (or daemon-internal prompt fragment), and its own structured exit envelope. The daemon supervises lifecycle uniformly per [FR 07](07-worker-execution.md).
- **A nominates, daemon spawns**: A is the only "thinking" component that decides which phase runs next; the daemon never selects a phase on its own. The retry decision on a `phase_nonclean` and the artifact-validation retry decision after a clean `materialize_spec` / `finalize_review` exit are both A's, not the daemon's.
- **Composes existing skills, not new ones**: phases map onto `kiro-discovery` / `kiro-impl` / `kiro-review` (dispatched internally by `kiro-impl`) / `kiro-validate-impl` / `kiro-debug` / `kiro-verify-completion`. No roki-specific subagent is introduced.
- **Slash-command headless invocation**: `/kiro-* <args>` works as the initial prompt argument in `-p` mode, even for skills whose manifest sets `disable-model-invocation: true` (the slash-command entry path is unaffected by the model-invocation flag).
- **Operator-installed tools, no daemon proxy**: every Bash / `gh` / Linear-MCP call from inside a phase goes through the operator's Claude Code tool surface unchanged ([FR 11: Agent Tool Boundary](11-agent-tool-boundary.md)).
- **Artifacts produced inside the catalog, validated by A**: `materialize_spec` writes `requirements.md` and `finalize_review` writes `review.md`; A reads each after the producing phase clean-exits and validates it structurally per [FR 19 §Artifact validation](19-orchestrator-session.md). No daemon-side gate hook subscribes.

## Boundaries

- **The daemon does not drive these phases** — it spawns the phase subprocess A nominated, observes its lifecycle, and forwards the structured exit to A. State-machine transitions come from subprocess exit and Linear state, not from any phase-internal event ([FR 07: Worker Execution](07-worker-execution.md)).
- **The daemon does not enforce the phase order** — A's `action=run_phase` directives are the only sequencing signal. A's `max_phases` budget is the daemon-side cap; per-phase order is A's choice.
- **Loop budgets are skill-internal** — `kiro-impl`'s per-task review loop, `kiro-validate-impl`'s remediation, and the CI-fix budget all live in skill state, not in any daemon-side persistent column. Cross-phase budgets (ticket-level retry, `max_phases`) live on A and on the daemon, not in the skill.
- **Skill internals are skill-owned** — the per-task review loop, validation-command discovery, and remediation strategies live inside `kiro-impl` / `kiro-validate-impl` / `kiro-review` / `kiro-debug` and are not re-specified here. This FR only specifies how A composes them through the phase catalog.
- **Most authoring-time skills are out of scope** — `kiro-spec-*`, `kiro-validate-design`, `kiro-validate-gap`, and the steering skills are operator-side; phase subprocesses do not invoke them. `kiro-discovery` is the one exception: it is reused inside the `materialize_spec` phase via slash-command headless invocation. It produces `requirements.md` only (not `spec.json` / `design.md` / `tasks.md`).
- **No standalone `ROKI_RESULT:` line** — superseded by the per-phase stream-json `result` event mapped into `phase_complete` / `phase_nonclean` events.
- **No daemon-driven retry of the inner phases** — retries inside a phase are skill-internal; the daemon-level retry budget ([FR 07: Worker Execution](07-worker-execution.md)) only frames how many `phase_nonclean → run_phase` cycles A may drive before `daemon_directive (kind=retry_exhausted)`. Artifact-validation retries after `materialize_spec` / `finalize_review` clean exit are A-driven (see [FR 19 §Artifact validation](19-orchestrator-session.md)) and re-use the same `action=run_phase` channel; they do not consume a phase-`nonclean` retry slot because the producing phase exited cleanly.
- **Auto-merge is out of scope here** — `open_pr` ends at "PR opened". Whether the daemon then auto-merges on CI green is governed elsewhere (currently out of MVP).
- **Multi-engine portability is out of scope** — the contract assumes Claude Code stream-json. If non-Claude engines are added later, the kiro skill set abstraction needs re-expression in those engines (deferred).

## Open issues (flagged, not invented)

- **`kiro-impl` decomposition input**: the `materialize_spec` phase produces `requirements.md` only at the canonical artifact path ([ref:artifacts](../reference/artifacts.md)); it does not produce `spec.json`, `design.md`, or `tasks.md`. Whether `kiro-impl` requires a `tasks.md` decomposition layer to drive its per-task loop in the per-issue path — or whether it adapts to a single-`requirements.md` input — is an open question for `kiro-impl` and `kiro-discovery` to resolve. This FR flags the gap; it does not invent a phase to fill it.

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Engine ("Phase subprocess (short-lived, one per phase)"); Boundary Strategy > "Orchestrator-vs-phase boundary" and "Subprocess invocation taxonomy".
- **Requirements**: complemented by `req:roki-mvp:5.6` (phase nomination spawn), `req:roki-mvp:5.8` (phase clean / non-clean exit translation), `req:roki-mvp:13` (`additional_context` channel). A future spec covering the bundled kiro skill set should backfill explicit requirement IDs and add `implements:` here.
- **Reference**: [ref:artifacts](../reference/artifacts.md) (canonical `review.md` schema and path).
- **Skill manifests**: `.claude/skills/kiro-discovery/SKILL.md`, `.claude/skills/kiro-impl/SKILL.md`, `.claude/skills/kiro-review/SKILL.md`, `.claude/skills/kiro-validate-impl/SKILL.md`, `.claude/skills/kiro-debug/SKILL.md`, `.claude/skills/kiro-verify-completion/SKILL.md`.
- **Related FR**: [07-worker-execution](07-worker-execution.md), [11-agent-tool-boundary](11-agent-tool-boundary.md), [14-operator-notifications](14-operator-notifications.md), [19-orchestrator-session](19-orchestrator-session.md).
