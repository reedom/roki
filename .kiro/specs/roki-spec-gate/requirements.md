# Requirements Document

## Project Description (Input)
Without an explicit spec phase, the agent jumps straight from a Linear ticket to code. Acceptance criteria written in the ticket body drift from project-level EARS specs (managed under `.kiro/specs/`), and the agent loses the chance to align them before implementation. roki-spec-gate is a daemon-enforced pre-implementation gate that plugs into roki-mvp's state-machine subscription hook for the `Queued -> Active` vetoable transition. Before allowing that transition, the gate triggers a constrained "spec materialization" agent turn whose only purpose is to read the Linear ticket body, scan project-level EARS docs under `.kiro/specs/`, and write a merged `.kiro/specs/<issue>/requirements.md` with EARS-shaped acceptance criteria. The daemon then verifies file presence plus a regex-based EARS shape check. Pass advances the transition; fail records an attempt and either retries or escalates after `max_attempts`. The gate is time-bounded via `timeout_ms` and never blocks indefinitely. Validation is daemon-side and mechanical (no LLM judgment); authoring is always agent-side. The gate also adds a read-only `kiro_spec_status` tool to roki-mvp's tool registry so subsequent agent turns can self-check, and it adds three reserved keys under `extension.gates.spec.*` in `WORKFLOW.md`: `required_status`, `timeout_ms`, `max_attempts`. Out of scope: deep semantic EARS validation, project-wide spec sync (deferred to a future spec).

## Introduction

The roki-spec-gate specification defines a daemon-enforced pre-implementation gate that plugs into roki-mvp's published state-machine subscription hook for the `Queued -> Active` vetoable transition. The gate ensures that no per-issue worker enters the `Active` state — and therefore no implementation work begins — until a Linear ticket has a corresponding `.kiro/specs/<issue>/requirements.md` artifact in the workspace whose contents pass a mechanical EARS shape check. The gate orchestrates a constrained agent invocation (a "spec materialization turn") whose sole purpose is to merge the Linear ticket body with the project's existing EARS docs under `.kiro/specs/` and emit the merged spec; validation of the resulting file is daemon-side, file-presence plus regex, with no LLM judgment.

This spec is a downstream extension of roki-mvp. It assumes the roki-mvp orchestrator state machine, the workspace path layout `<workspace_root>/<repo>/<issue>/`, the agent tool registry shape with its `Tool` and `Registry` traits, and the `WORKFLOW.md` schema with its `extension.gates.spec.*` reserved namespace. It does not redesign any of those; it consumes them. The gate is time-bounded, fail-closed when the gate's own decision is unreliable, and emits a final escalation outcome rather than blocking indefinitely.

## Boundary Context

- **In scope**: a `SpecGate` subscriber registered against roki-mvp's transition hook for the vetoable `Queued -> Active` transition; orchestration of a spec-materialization turn whose only purpose is to produce `.kiro/specs/<issue>/requirements.md` for the active `(repo, issue)`; mechanical validation of the produced file (existence, non-empty, presence of EARS trigger keywords `WHEN`, `IF`, `WHILE`, `WHERE`, or `SHALL` in acceptance-criteria positions, encoding sanity); a per-attempt time bound that never lets a single attempt exceed `extension.gates.spec.timeout_ms`; bounded retry up to `extension.gates.spec.max_attempts`; mapping of the gate's outcome onto a `VetoDecision` returned to the orchestrator; emission of structured gate-decision log events (allow / deny / timeout / escalate / pass) with `(repo, issue, correlation_id)` context; a read-only `kiro_spec_status` tool registered through roki-mvp's `Registry` so agent turns can self-check the gate's view of the spec artifact; consumption of the reserved `extension.gates.spec.required_status`, `extension.gates.spec.timeout_ms`, and `extension.gates.spec.max_attempts` keys exposed by the roki-mvp `WORKFLOW.md` loader; reliance on the kiro-discovery skill auto-invocation mechanism inside the materialization turn for the merge of ticket EARS and project EARS.
- **Out of scope**: changes to the orchestrator state set, the documented vetoable-transition list, or the transition event payload (those are owned by roki-mvp); changes to the `Tool` or `Registry` trait shape (this spec only registers a new tool); changes to the `WORKFLOW.md` schema other than consuming the reserved `extension.gates.spec.*` namespace already published by roki-mvp; any logic that mutates Linear, opens or comments on PRs, or edits source files (those remain the agent's responsibility); deep semantic validation of EARS bullets — only file presence plus regex shape is in scope; project-wide spec synchronization across multiple specs (deferred to a future `roki-spec-sync` spec); the review gate (deferred to roki-review-gate); post-merge distill of flow-type docs (deferred to roki-distill-postmerge); container or VM isolation; Windows support.
- **Adjacent expectations**: the operator runs roki-mvp with the kiro skills available as personal skills under `~/.claude/skills/kiro-*` (so the kiro-discovery skill auto-invokes inside the materialization turn); the operator's `WORKFLOW.md` either declares values for `extension.gates.spec.required_status`, `extension.gates.spec.timeout_ms`, and `extension.gates.spec.max_attempts`, or accepts the gate's documented defaults; the workspace tree at `<workspace_root>/<repo>/<issue>/` is provisioned by roki-mvp's workspace manager before this gate runs; the agent's session within the spec-materialization turn has filesystem access to the workspace root and to `.kiro/specs/` underneath it; downstream specs (roki-review-gate, roki-distill-postmerge) may read the same `.kiro/specs/<issue>/requirements.md` artifact this gate enforces.

## Requirements

### Requirement 1: Gate Subscription Against the Vetoable Pre-Implementation Transition

**Objective:** As an operator, I want the spec gate to subscribe to roki-mvp's vetoable `Queued -> Active` transition hook, so that no worker can begin implementation work for a `(repo, issue)` until the gate has explicitly allowed it.

#### Acceptance Criteria
1. When the roki daemon starts and the spec-gate component is enabled, the spec gate shall register a subscriber on roki-mvp's state-machine subscription hook for the vetoable `Queued -> Active` transition before any worker is promoted to `Active`.
2. When roki-mvp publishes a vetoable `Queued -> Active` transition event for a `(repo, issue)`, the spec gate shall return an allow decision only after its own gate evaluation produces a pass outcome for that `(repo, issue)`.
3. While the spec gate is evaluating a `Queued -> Active` transition for a `(repo, issue)`, the spec gate shall not process a second concurrent evaluation for the same `(repo, issue)`.
4. If the spec gate raises an unexpected error while evaluating a transition, the spec gate shall return a deny decision for that transition and shall log the error without aborting the orchestrator's transition processing.
5. The spec gate shall not subscribe to or veto any transition other than `Queued -> Active`.

### Requirement 2: Spec-Materialization Turn Invocation

**Objective:** As the operator, I want the gate to drive a constrained agent turn whose only purpose is to produce a merged `.kiro/specs/<issue>/requirements.md` from the Linear ticket and project-level EARS docs, so that the spec artifact exists before implementation begins without requiring a separate human authoring step.

#### Acceptance Criteria
1. When the spec gate begins evaluating a `Queued -> Active` transition for a `(repo, issue)`, the spec gate shall invoke a spec-materialization turn against the agent session for that `(repo, issue)` whose only stated purpose is to produce `.kiro/specs/<issue>/requirements.md` by merging the Linear ticket body with the project's existing EARS docs.
2. When the spec-materialization turn runs, the spec gate shall provide the agent with the Linear ticket identifier, the workspace path for the `(repo, issue)`, and the location of the project's `.kiro/specs/` tree, so that the turn can rely on the kiro-discovery skill auto-invocation to perform the EARS merge.
3. The spec gate shall constrain the spec-materialization turn so that its only authored output for this gate is `.kiro/specs/<issue>/requirements.md`; outputs outside that path shall not satisfy the gate.
4. While the spec-materialization turn is running, the spec gate shall not interpret intermediate agent messages as a pass or fail decision; only the post-turn artifact and validation shall produce the gate outcome.
5. If the spec-materialization turn exits without producing the expected `requirements.md` path, the spec gate shall record the attempt as failed and proceed to retry or escalation per the configured policy.

### Requirement 3: Daemon-Side Validation by File Presence and EARS-Shape Regex

**Objective:** As an operator, I want the gate's validation to be mechanical and LLM-free, so that gate decisions are deterministic, auditable, and resistant to drift in agent output.

#### Acceptance Criteria
1. When the spec-materialization turn completes for a `(repo, issue)`, the spec gate shall validate the produced `.kiro/specs/<issue>/requirements.md` only by file existence, non-empty content, encoding sanity, and the presence of EARS trigger constructs.
2. The spec gate shall recognize a file as EARS-shaped when its acceptance-criteria text contains at least one occurrence of the EARS trigger keywords drawn from the set `WHEN`, `IF`, `WHILE`, `WHERE`, or `SHALL` in a position consistent with EARS acceptance criteria.
3. If the produced file is missing, empty, unreadable, or contains no EARS trigger occurrences, the spec gate shall classify the attempt as failed validation.
4. The spec gate shall not invoke any LLM, semantic analyzer, or external service to make the validation decision.
5. The spec gate shall log every validation outcome as a structured event including the `(repo, issue)`, the attempt index, the validation verdict, and the validation reason in machine-readable form.

### Requirement 4: Time Bounding and Retry Policy

**Objective:** As an operator, I want the gate to be time-bounded and retry-bounded, so that a stuck materialization turn never blocks the orchestrator indefinitely and a known-bad ticket eventually escalates rather than looping forever.

#### Acceptance Criteria
1. The spec gate shall enforce a per-attempt timeout drawn from `extension.gates.spec.timeout_ms` in `WORKFLOW.md`, terminating the spec-materialization turn for that attempt when the timeout elapses.
2. While a spec-materialization turn is running, the spec gate shall measure elapsed time from the start of that attempt only; previous attempts shall not consume the current attempt's budget.
3. The spec gate shall enforce an attempt cap drawn from `extension.gates.spec.max_attempts` in `WORKFLOW.md`, refusing to start an additional attempt for the same `(repo, issue)` once the cap is reached.
4. When a timeout terminates a spec-materialization attempt before validation, the spec gate shall record the attempt as failed and shall not advance the gate to a pass outcome on that attempt.
5. If the configured attempt cap is exhausted without a passing attempt, the spec gate shall return a deny decision to the orchestrator for the `Queued -> Active` transition and shall emit an escalation event for that `(repo, issue)`.
6. The spec gate shall never block the orchestrator's transition decision longer than the sum of `extension.gates.spec.timeout_ms` multiplied by `extension.gates.spec.max_attempts` plus a documented small overhead window.

### Requirement 5: Pass and Fail Outcome Mapping to the Orchestrator

**Objective:** As a roki-mvp orchestrator, I want a clear, mechanical mapping from the spec gate's evaluation outcome to a vetoable-transition decision, so that allowed transitions advance to `Active` and denied transitions remain in the previous state without mid-flight ambiguity.

#### Acceptance Criteria
1. When the spec gate's evaluation produces a passing validation result for a `(repo, issue)`, the spec gate shall return an allow decision to the orchestrator for the `Queued -> Active` transition.
2. When the spec gate's evaluation produces a failing validation result and additional attempts remain under `extension.gates.spec.max_attempts`, the spec gate shall return a deny decision for the current transition event and shall remain ready to evaluate a subsequent retransition attempt for the same `(repo, issue)`.
3. When the spec gate's attempt cap is exhausted without a pass, the spec gate shall return a deny decision and shall emit an escalation event identifying the `(repo, issue)` and the final failure reason.
4. The spec gate shall never return a pass outcome based on partial information from inside the materialization turn; the outcome shall be derived only from post-turn validation of the artifact on disk.
5. If the gate's own evaluation cannot complete because of an internal error, the spec gate shall fail closed by returning a deny decision and logging the internal error.

### Requirement 6: `kiro_spec_status` Read-Only Agent Tool

**Objective:** As the agent driving subsequent turns, I want a read-only `kiro_spec_status` tool registered in roki-mvp's tool registry, so that I can query the daemon's current view of the spec artifact and the gate's recorded attempts without parsing logs or guessing.

#### Acceptance Criteria
1. The spec gate shall register a tool named `kiro_spec_status` through roki-mvp's `Registry` so that the agent can invoke it from any worker session for which the tool registry is exposed.
2. When the agent invokes `kiro_spec_status` with a `(repo, issue)` reference, the spec gate shall return the current spec artifact path, an artifact-present flag, the most recent validation outcome, the attempt count, and the remaining attempts.
3. The `kiro_spec_status` tool shall be read-only and shall not change the gate's recorded attempts, validation state, or any on-disk content.
4. If the agent invokes `kiro_spec_status` with a `(repo, issue)` reference that the orchestrator does not currently track, the spec gate shall return a structured not-found response without raising an error to the orchestrator.
5. The spec gate shall never include the Linear API token, daemon-internal credentials, or workspace paths outside the queried `(repo, issue)` in any `kiro_spec_status` response.

### Requirement 7: `WORKFLOW.md` Configuration Surface

**Objective:** As an operator, I want gate behavior to be configured through the reserved `extension.gates.spec.*` keys in `WORKFLOW.md`, so that I can tune the gate per repository without restarting the daemon or recompiling roki.

#### Acceptance Criteria
1. The spec gate shall consume only the reserved keys `extension.gates.spec.required_status`, `extension.gates.spec.timeout_ms`, and `extension.gates.spec.max_attempts` from the `WORKFLOW.md` policy struct exposed by roki-mvp's loader.
2. The spec gate shall apply documented defaults when a key under `extension.gates.spec.*` is absent from a repository's `WORKFLOW.md`, and shall log the defaulted value for each key that fell back.
3. The spec gate shall enforce `extension.gates.spec.required_status` as the Linear state in which the gate evaluates; transitions arising from any other status shall not trigger gate evaluation.
4. When a repository's `WORKFLOW.md` is hot-reloaded by roki-mvp, the spec gate shall pick up the new `extension.gates.spec.*` values for any subsequent gate evaluation in that repository without restart.
5. If `extension.gates.spec.timeout_ms` or `extension.gates.spec.max_attempts` resolves to a non-positive value after defaults and overrides, the spec gate shall refuse to evaluate transitions for that repository and shall log the misconfiguration.

### Requirement 8: Concurrency, Idempotency, and Multi-Repo Independence

**Objective:** As an operator running roki across multiple repositories, I want the gate to work correctly when several `(repo, issue)` pairs progress through `Queued -> Active` concurrently, so that one slow or failing gate evaluation cannot starve or contaminate evaluations for other tickets.

#### Acceptance Criteria
1. The spec gate shall evaluate each `(repo, issue)` independently and shall isolate failures so that a deny or timeout for one `(repo, issue)` does not affect concurrent evaluations for other `(repo, issue)` pairs.
2. While the orchestrator may publish duplicate `Queued -> Active` transition events for the same `(repo, issue)` (for example, due to webhook redelivery), the spec gate shall return the same decision for the same logical attempt without double-counting against `extension.gates.spec.max_attempts`.
3. The spec gate shall key all per-evaluation state by the same `(repo, issue)` tuple roki-mvp uses, so that attempt counts, timers, and validation outcomes never leak across repositories or issues.
4. If roki-mvp is shut down while a spec gate evaluation is in flight, the spec gate shall treat the in-flight attempt as failed on next restart and shall start a fresh attempt subject to the remaining budget recovered from in-memory reconciliation.
5. The spec gate shall never write per-evaluation state to a persistent database; any in-memory state shall be reconstructable from the orchestrator's recovery scan plus the on-disk artifact.

### Requirement 9: Observability and Escalation

**Objective:** As an operator debugging gate behavior, I want every gate decision and timing event observable through the same tracing pipeline roki-mvp uses, so that I can diagnose gate failures and escalations without an external UI.

#### Acceptance Criteria
1. The spec gate shall emit a structured log event for every gate-evaluation start, every spec-materialization turn start and end, every per-attempt timeout, every validation outcome, every veto decision returned to the orchestrator, and every escalation.
2. Every spec gate log event shall include the `(repo, issue)` key and the orchestrator-supplied correlation identifier so that operators can correlate gate activity with worker activity.
3. The spec gate shall route all log events through roki-mvp's existing tracing pipeline and shall apply roki-mvp's existing redaction layer; the spec gate shall not introduce its own log destination.
4. When the spec gate emits an escalation, the escalation event shall identify the `(repo, issue)`, the final attempt index, the final validation reason, and the configured `extension.gates.spec.required_status`, so that downstream consumers (operator, future observability spec) can decide what to do.
5. The spec gate shall never emit log content that includes Linear API tokens, raw `WORKFLOW.md` secret fields, or workspace paths outside the affected `(repo, issue)`.
