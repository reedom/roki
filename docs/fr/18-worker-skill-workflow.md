---
refs:
  id: fr:18-worker-skill-workflow
  kind: fr
  title: "Phase Subprocess Catalog"
  related:
    - fr:02-configuration
    - fr:07-worker-execution
    - fr:11-agent-tool-boundary
    - fr:12-extension-surface
    - fr:14-operator-notifications
    - fr:19-orchestrator-session
---

# FR 18: Phase Subprocess Catalog

> The catalog of bounded phase subprocesses the orchestrator session A nominates per ticket. Each phase ships a daemon-built **default invocation** (a slash-command-driven skill or a daemon-internal prompt fragment); operators MAY override per phase via `WORKFLOW.md` using either an `extension.phase.<name>.command` key (slash-command swap) or a named `prompt_template_<name>` block (Liquid template rendered onto the subprocess's stdin). Documents the per-phase exit envelope A reads, and the artifacts produced inside selected phases (`requirements.md` from `materialize_spec`, `review.md` from `finalize_review`) that A then validates structurally per [FR 19 §Artifact validation](19-orchestrator-session.md).

## Purpose

[FR 19: Orchestrator Session](19-orchestrator-session.md) describes the long-lived "thinking" component A that classifies admission, plans phases, validates produced artifacts (`requirements.md` after `materialize_spec`, `review.md` after `finalize_review`), processes daemon-only failure directives, and writes Linear via Linear MCP. A does not edit code; the actual code-changing work runs in **short-lived bounded phase subprocesses** that A nominates via `action=run_phase`. [FR 07: Worker Execution](07-worker-execution.md) describes the engine adapter's mechanical supervision of those subprocesses (launch flags, stall detection, stream-json parsing, exit translation into `phase_complete` / `phase_nonclean` events).

This FR canonicalizes the **per-phase catalog**: which default skill (or daemon-internal prompt fragment) drives each phase, what override surface operators expose to swap or replace that default, what the daemon expects in the structured exit envelope, and what A reads from the resulting `phase_complete` / `phase_nonclean` event to decide its next action. Spec presence at admission and review pass at PR-readiness are not separate daemon-side gates — A reads `requirements.md` after `materialize_spec` clean exit and `review.md` after `finalize_review` clean exit and decides next-step itself per [FR 19 §Artifact validation](19-orchestrator-session.md).

## User-visible Behavior

### Phase catalog (one bounded subprocess per phase A nominates)

A's `action=run_phase` directive carries a `phase` value drawn from the catalog below. For each phase the daemon spawns a single subprocess inside the issue's session tempdir, with its own `--max-turns` budget and the per-phase context envelope (including A's `additional_context` verbatim through the engine adapter's `additional_context` channel — see `req:roki-mvp:13`). The invocation form follows the phase override surface (see §Phase override): the catalog default for each phase is either a slash-command-driven skill (`claude -p '/<skill> <args>' --output-format stream-json --max-turns N`) or a daemon-internal templated prompt rendered onto stdin (`claude --input-format stream-json --output-format stream-json --max-turns N`).

| `phase` | Default invocation | Default skill / prompt | Purpose |
|---|---|---|---|
| `materialize_spec` | `claude -p '/kiro-discovery <issue>'` | `kiro-discovery` | Synthesize the per-issue `requirements.md` (canonical path in [ref:artifacts](../reference/artifacts.md)) by merging the Linear ticket body and the project's existing EARS docs under `.kiro/specs/`. Produces `requirements.md` only — not `spec.json`, `design.md`, or `tasks.md` (see §Open issues). After clean exit A reads the artifact and validates it structurally per [FR 19 §Artifact validation](19-orchestrator-session.md); on structural failure with retry budget remaining A re-nominates `materialize_spec` with `additional_context` populated from the failure detail. |
| `implement` | `claude -p '/kiro-impl <feature>'` | `kiro-impl` | Drives the implementer-then-reviewer loop per task in autonomous mode (no task numbers): for each pending task, dispatch a fresh implementer subagent (TDD-first), then dispatch `kiro-review` as an independent per-task reviewer subagent. The skill's internal loop handles reviewer rejections by remediation + re-review until APPROVED, marks the task `[x]`, and proceeds. Counters live inside `kiro-impl`. The per-task `kiro-review` invocations here are distinct from the feature-level `review` phase below. |
| `review` | `claude -p '/kiro-review <feature>'` | `kiro-review` | Feature-level adversarial code review run as its own phase after `implement` reports done. Output: `APPROVED` / `REJECTED` with structured findings. On `APPROVED` A nominates `validate`. On `REJECTED` A re-nominates `implement` with `additional_context` populated from the reviewer findings (criteria id, severity, file references). The phase covers cross-task design / smell / readability concerns the per-task `kiro-review` invocations inside `kiro-impl` cannot observe holistically. The retry loop budget is bounded by the ticket-level retry counter ([FR 07](07-worker-execution.md)) and `max_phases` ([FR 19](19-orchestrator-session.md)). |
| `validate` | `claude -p '/kiro-validate-impl <feature>'` | `kiro-validate-impl` | Two-stage feature-level validation. Stage 1 (mechanical): run the workspace's fmt / lint / test commands; on failure exit with `verdict=NO_GO` and `category=build` (fail-fast — stage 2 skipped). Stage 2 (spec acceptance): LLM-driven check of the implementation against the EARS acceptance criteria in `requirements.md`; on failure exit with `verdict=NO_GO` and `category=spec`. On both stages clean: `verdict=GO`. A reads the verdict + category and either nominates `open_pr` (GO) or re-nominates `implement` with `additional_context` populated from the failing category and findings. The mechanical fmt/lint/test commands are the workspace's local subset; the full CI superset (audit, coverage, doc, e2e, multi-OS) is the responsibility of the remote CI and the `ci_fix` phase. |
| `open_pr` | `claude --input-format stream-json` with daemon-internal prompt | (no skill — daemon-internal prompt fragment, overridable via `prompt_template_open_pr`) | `gh pr create` via Bash. The PR description embeds a brief change summary plus the validation outcome A passes through `additional_context`. |
| `ci_fix` | `claude -p '/roki-ci-fix <feature>'` | `roki-ci-fix` | Remote CI-failure triage and patch loop. Steps inside the skill: (1) fetch failing job logs via `gh run view --log-failed`; (2) categorize each job failure into one of `build` / `test` / `lint` / `audit` / `coverage` / `doc` / `e2e` / `other`; (3) delegate root-cause analysis + minimal fix to `kiro-debug` per category; (4) gate the resulting commits via `kiro-verify-completion` (claim type `TEST_OR_BUILD`) before push; (5) push to the PR branch. The terminal exit envelope reports the categorized failure set and the pushed commit shas. A nominates `ci_fix` after observing CI red (typically after a poll observation A maintains in conversation context, or after a manual hint from the operator); the daemon does not auto-poll CI for A. On persistent failure A may re-nominate `ci_fix` (subject to `max_phases`) or `action=stop` with `outcome=failure`. |
| `finalize_review` | `claude -p '/roki-finalize-review <feature>'` | `roki-finalize-review` | Synthesize the structured `review.md` artifact at the path documented in [ref:artifacts](../reference/artifacts.md), drawing on the verdicts already accumulated this session (per-task `kiro-review` APPROVED set inside `implement`, the feature-level `review` phase APPROVED, `kiro-validate-impl` GO, the verify-cmd outcome, any `kiro-verify-completion` VERIFIED stamps, the `ci_fix` outcome when applicable, and the artefacts in the worktree). After clean exit A reads the artifact and validates it structurally per [FR 19 §Artifact validation](19-orchestrator-session.md); on structural failure or overall `status=fail` with retry budget remaining A re-nominates `implement` with `additional_context` populated from the failing per-criterion entries. |

Slash commands work as the initial prompt argument in `-p` mode (the prompt string is parsed before headless takes over), so a SKILL.md `disable-model-invocation: true` flag (e.g. on `kiro-impl` or `roki-ci-fix`) does not prevent the daemon from launching that skill via `/<skill> <args>`. The same property lets the daemon launch `kiro-discovery` for the `materialize_spec` phase even though `kiro-discovery` is otherwise an operator-side authoring skill — the slash-command entry path is unaffected by the model-invocation flag. Other authoring-time skills (`kiro-spec-init`, `kiro-spec-requirements`, `kiro-spec-design`, `kiro-spec-tasks`, `kiro-spec-batch`, `kiro-spec-quick`, `kiro-spec-status`, `kiro-validate-design`, `kiro-validate-gap`, `kiro-steering`, `kiro-steering-custom`) are operator-side only and are not invoked by the daemon.

A Type B (with-human-planning) ticket is handled inside an `implement` phase: the phase agent uses the operator-installed Linear MCP (per [FR 11: Agent Tool Boundary](11-agent-tool-boundary.md)) to post questions as ticket comments and poll for replies. There is no dedicated `kiro-plan-with-human` skill today — this is normal Claude Code session work using Linear MCP and Bash, scoped to the same `implement` phase budget.

### Phase override (operator)

Each phase ships a daemon-built default invocation (catalog above). Operators MAY override per phase via the workspace-level `WORKFLOW.md` ([02-configuration](02-configuration.md)) using either of two mutually exclusive forms:

1. **Slash-command override** — `extension.phase.<name>.command = "/<custom-skill> <args...>"` swaps in a different slash-command-driven skill while keeping the daemon's invocation pattern (`claude -p '<command>' --output-format stream-json --max-turns N`). Variable substitution against the same per-phase context envelope (issue id, feature name, repo, etc.) is applied to the command string before launch.
2. **Template override** — a `prompt_template_<name>` named template block (Liquid + Markdown) replaces the default prompt entirely; the daemon renders the block against the same per-phase variables, writes the rendered text to the subprocess's stdin, and launches `claude --input-format stream-json --output-format stream-json --max-turns N`.

The two forms are mutually exclusive per phase: declaring both for the same phase is a configuration error and is rejected at startup or retained as the previous policy at hot reload (per `roki-mvp Req 6.7`). When neither form is declared the daemon uses the catalog default.

Override applies per ticket admission of that phase: an in-flight phase subprocess always finishes with the configuration that was in effect when the daemon spawned it; subsequent nominations of the same phase pick up the new policy.

The phase subprocess's tool surface, sandbox profile, and `--max-turns` budget are governed by [07-worker-execution](07-worker-execution.md) and the operator's permission strategy regardless of which override form is used. An override changes the prompt or the slash command — it does not change the daemon-side supervision contract.

The `kiro-*` skills referenced as catalog defaults provide reusable spec-driven dev protocol (admission-time materialization, implementation, code-quality review, acceptance validation, debug). The `roki-*` skills provide roki-specific lifecycle protocol (CI-failure triage, `review.md` synthesis) for which kiro has no equivalent. Operators can swap either with their own slash-command-driven skill or templated prompt by declaring the override.

### Skill set (verified manifests)

The runtime skills below exist at `.claude/skills/<skill>/SKILL.md` (project) or `~/.claude/skills/<skill>/SKILL.md` (operator). `kiro-*` skills provide reusable spec-driven dev protocol; `roki-*` skills provide roki-specific lifecycle protocol that has no kiro equivalent. Operators may swap any default via the phase override surface (above).

| Skill | Default for phase | Purpose | Tool scope (per skill manifest) |
|---|---|---|---|
| `kiro-discovery` | `materialize_spec` | Synthesize the per-issue `requirements.md` by merging the Linear ticket body and existing project EARS docs under `.kiro/specs/`. Authoring-time skill repurposed by the daemon via slash-command headless invocation. | per skill manifest |
| `kiro-impl` | `implement` | Drives the implementer-then-reviewer loop per task; owns TDD discipline, validation-command discovery, and per-task remediation. Dispatches `kiro-review` internally per task as an independent reviewer subagent. Manifest carries `disable-model-invocation: true` (slash-command entry only). | Read, Write, Edit, MultiEdit, Bash, Glob, Grep, Agent, WebSearch, WebFetch |
| `kiro-review` | `review` (feature-level phase) and `implement` (per-task internal dispatch) | Adversarial code review against approved spec + boundary; APPROVED / REJECTED with structured findings. Same skill, two invocation contexts: as a phase (`/kiro-review <feature>` over the whole feature) and as an internal subagent inside `kiro-impl` (per task). | Read, Bash, Grep, Glob |
| `kiro-validate-impl` | `validate` | Two-stage feature-level validation: (1) mechanical fmt / lint / test (fail-fast); (2) spec acceptance criteria check. Output: `verdict ∈ {GO, NO_GO}` + `category ∈ {build, spec}` on NO_GO. | Read, Bash, Grep, Glob, Agent |
| `kiro-debug` | `ci_fix` (internal, dispatched by `roki-ci-fix`) | Root-cause-first failure analysis; used on CI red to propose a minimal fix. | per skill manifest |
| `kiro-verify-completion` | `ci_fix` (internal, used by `roki-ci-fix` as the fresh-evidence gate before push) | Refuses success claims that lack fresh evidence; VERIFIED / NOT_VERIFIED / MANUAL_VERIFY_REQUIRED. | Read, Bash, Grep, Glob |
| `roki-ci-fix` | `ci_fix` | Remote CI-failure triage: fetch failing job logs via `gh run view --log-failed`; categorize each job failure (`build` / `test` / `lint` / `audit` / `coverage` / `doc` / `e2e` / `other`); delegate root-cause + minimal fix to `kiro-debug` per category; gate the resulting commits via `kiro-verify-completion`; push to the PR branch. Manifest carries `disable-model-invocation: true` (slash-command entry only). | Read, Bash (incl. `gh`, `git push`), Grep, Glob, Agent |
| `roki-finalize-review` | `finalize_review` | Synthesize the structured `review.md` artifact from the session's accumulated verdicts and worktree artefacts to the schema in [ref:artifacts](../reference/artifacts.md). Manifest carries `disable-model-invocation: true` (slash-command entry only). | Read, Bash, Grep, Glob |

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

- **Per-phase isolation**: each phase = its own bounded subprocess with its own `--max-turns` budget, its own driving prompt (default skill, default daemon-internal fragment, or operator override), and its own structured exit envelope. The daemon supervises lifecycle uniformly per [FR 07](07-worker-execution.md).
- **A nominates, daemon spawns**: A is the only "thinking" component that decides which phase runs next; the daemon never selects a phase on its own. The retry decision on a `phase_nonclean` and the artifact-validation retry decision after a clean `materialize_spec` / `finalize_review` exit are both A's, not the daemon's.
- **Defaults + per-phase override**: each phase ships a daemon-built default; operators replace any phase's invocation via `extension.phase.<name>.command` (slash-command swap) or `prompt_template_<name>` block (templated stdin). The override surface is uniform across the catalog.
- **Composes kiro skills + roki-specific skills**: kiro skills cover reusable spec-driven dev protocol (`kiro-discovery` / `kiro-impl` / `kiro-review` / `kiro-validate-impl` / `kiro-debug` / `kiro-verify-completion`). roki-specific skills cover roki lifecycle that kiro does not address (`roki-ci-fix` / `roki-finalize-review`).
- **Slash-command headless invocation**: `/<skill> <args>` works as the initial prompt argument in `-p` mode, even for skills whose manifest sets `disable-model-invocation: true` (the slash-command entry path is unaffected by the model-invocation flag).
- **Operator-installed tools, no daemon proxy**: every Bash / `gh` / Linear-MCP call from inside a phase goes through the operator's Claude Code tool surface unchanged ([FR 11: Agent Tool Boundary](11-agent-tool-boundary.md)).
- **Artifacts produced inside the catalog, validated by A**: `materialize_spec` writes `requirements.md` and `finalize_review` writes `review.md`; A reads each after the producing phase clean-exits and validates it structurally per [FR 19 §Artifact validation](19-orchestrator-session.md). No daemon-side gate hook subscribes.

## Boundaries

- **The daemon does not drive these phases** — it spawns the phase subprocess A nominated, observes its lifecycle, and forwards the structured exit to A. State-machine transitions come from subprocess exit and Linear state, not from any phase-internal event ([FR 07: Worker Execution](07-worker-execution.md)).
- **The daemon does not enforce the phase order** — A's `action=run_phase` directives are the only sequencing signal. A's `max_phases` budget is the daemon-side cap; per-phase order is A's choice.
- **Loop budgets are skill-internal** — `kiro-impl`'s per-task review loop, `roki-ci-fix`'s per-category fix loop, and `kiro-validate-impl`'s mechanical / spec staging all live in skill state, not in any daemon-side persistent column. Cross-phase budgets (ticket-level retry, `max_phases`) live on A and on the daemon, not in the skill.
- **Skill internals are skill-owned** — the per-task review loop, validation-command discovery, CI log categorization, and remediation strategies live inside `kiro-impl` / `kiro-validate-impl` / `kiro-review` / `kiro-debug` / `roki-ci-fix` / `roki-finalize-review` and are not re-specified here. This FR only specifies how A composes them through the phase catalog.
- **Most authoring-time kiro skills are out of scope** — `kiro-spec-*`, `kiro-validate-design`, `kiro-validate-gap`, and the steering skills are operator-side; phase subprocesses do not invoke them. `kiro-discovery` is the one exception: it is reused inside the `materialize_spec` phase via slash-command headless invocation. It produces `requirements.md` only (not `spec.json` / `design.md` / `tasks.md`).
- **No standalone `ROKI_RESULT:` line** — superseded by the per-phase stream-json `result` event mapped into `phase_complete` / `phase_nonclean` events.
- **No daemon-driven retry of the inner phases** — retries inside a phase are skill-internal; the daemon-level retry budget ([FR 07: Worker Execution](07-worker-execution.md)) only frames how many `phase_nonclean → run_phase` cycles A may drive before `daemon_directive (kind=retry_exhausted)`. Artifact-validation retries after `materialize_spec` / `finalize_review` clean exit are A-driven (see [FR 19 §Artifact validation](19-orchestrator-session.md)) and re-use the same `action=run_phase` channel; they do not consume a phase-`nonclean` retry slot because the producing phase exited cleanly. Likewise, `review` REJECTED → `implement` re-nomination and `validate` NO_GO → `implement` re-nomination are A-driven and counted against `max_phases` (and the ticket-level retry budget if the producing phase exited non-clean).
- **Phase override does not change the daemon contract** — `extension.phase.<name>.command` and `prompt_template_<name>` swap the prompt or skill body but not the lifecycle observation, exit-envelope translation, or per-phase tool surface; an override that needs more tools must declare them on the operator's Claude Code allowlist independently.
- **Auto-merge is out of scope here** — `open_pr` ends at "PR opened". Whether the daemon then auto-merges on CI green is governed elsewhere (currently out of MVP).
- **Multi-engine portability is out of scope** — the contract assumes Claude Code stream-json. If non-Claude engines are added later, the skill set abstraction (kiro + roki) needs re-expression in those engines (deferred).

## Open issues (flagged, not invented)

- **`kiro-impl` decomposition input**: the `materialize_spec` phase produces `requirements.md` only at the canonical artifact path ([ref:artifacts](../reference/artifacts.md)); it does not produce `spec.json`, `design.md`, or `tasks.md`. Whether `kiro-impl` requires a `tasks.md` decomposition layer to drive its per-task loop in the per-issue path — or whether it adapts to a single-`requirements.md` input — is an open question for `kiro-impl` and `kiro-discovery` to resolve. This FR flags the gap; it does not invent a phase to fill it.

## Traceability

- **Roadmap**: `roadmap.md` > Constraints > Engine ("Phase subprocess (short-lived, one per phase)"); Boundary Strategy > "Orchestrator-vs-phase boundary" and "Subprocess invocation taxonomy".
- **Requirements**: complemented by `req:roki-mvp:5.6` (phase nomination spawn), `req:roki-mvp:5.8` (phase clean / non-clean exit translation), `req:roki-mvp:13` (`additional_context` channel). A future spec covering the bundled kiro skill set should backfill explicit requirement IDs and add `implements:` here.
- **Reference**: [ref:artifacts](../reference/artifacts.md) (canonical `review.md` schema and path).
- **Skill manifests**: `.claude/skills/kiro-discovery/SKILL.md`, `.claude/skills/kiro-impl/SKILL.md`, `.claude/skills/kiro-review/SKILL.md`, `.claude/skills/kiro-validate-impl/SKILL.md`, `.claude/skills/kiro-debug/SKILL.md`, `.claude/skills/kiro-verify-completion/SKILL.md`, `.claude/skills/roki-ci-fix/SKILL.md`, `.claude/skills/roki-finalize-review/SKILL.md`.
- **Related FR**: [02-configuration](02-configuration.md), [07-worker-execution](07-worker-execution.md), [11-agent-tool-boundary](11-agent-tool-boundary.md), [12-extension-surface](12-extension-surface.md), [14-operator-notifications](14-operator-notifications.md), [19-orchestrator-session](19-orchestrator-session.md).
