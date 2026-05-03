---
refs:
  id: requirements:roki-review-gate
  kind: requirements
  title: "roki-review-gate Requirements"
  spec: roki-review-gate
  implements:
    - roadmap
  provides:
    - req:roki-review-gate:1
    - req:roki-review-gate:2
    - req:roki-review-gate:3
    - req:roki-review-gate:4
    - req:roki-review-gate:5
    - req:roki-review-gate:6
    - req:roki-review-gate:7
    - req:roki-review-gate:8
---

# Requirements Document

## Project Description (Input)
roki-review-gate adds a daemon-enforced pre-PR review checkpoint to the roki orchestrator. Without it, a "done" signal depends entirely on the agent's self-assessment, and finished implementations may still violate EARS acceptance criteria. The gate plugs into roki-mvp's state-machine subscription hooks at the `AwaitingReview -> TerminalSuccess` vetoable transition: before that transition is allowed, the daemon requires a structured `review.md` artifact at `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` attesting per-criterion pass with code-level references. When the artifact is absent, malformed, or marked as failing, the daemon vetoes the transition, re-enters the implementation phase with the findings injected as additional context, and retries up to a configurable `max_attempts`. The gate orchestrates a constrained "review turn" that invokes the kiro-review skill agent-side; daemon-side validation is purely structural (file presence, schema shape, per-criterion pass status, code references), with no LLM judgment. The gate registers a read-only `kiro_review_status` tool through roki-mvp's tool registry so the agent can self-check. New `WORKFLOW.md` keys live under the reserved `extension.gates.review.*` namespace: `required_status`, `timeout_ms`, `max_attempts`. Cross-spec contract: review reads spec criteria from `.kiro/specs/<issue>/requirements.md` produced by roki-spec-gate; both gates publish stable artifact paths so roki-distill-postmerge can consume them.

## Introduction

The roki-review-gate specification defines a daemon-enforced pre-PR quality checkpoint for the roki system. It bolts onto roki-mvp's published state-machine extension points without forking the orchestrator: it registers a `TransitionSubscriber` that vetoes the `AwaitingReview -> TerminalSuccess` transition unless a structurally valid review artifact exists in the per-issue workspace. The artifact is produced by an agent-side review turn that invokes the kiro-review skill against the spec criteria written by roki-spec-gate and the implementation diff produced during the active phase.

The gate's daemon-side responsibility is bounded to validation: presence of `review.md`, schema shape, per-criterion pass/fail status, and the presence of code-level references on each pass entry. Substantive judgment is the review turn's responsibility. On gate failure, the gate re-enters the implementation phase by routing the issue back to `Active` with the structured findings injected as additional context for the next worker invocation, up to `extension.gates.review.max_attempts` (default 3). After exhausting attempts, the gate emits an escalation event and routes to `TerminalFailure`. The gate is time-bounded: a review turn that does not complete within `extension.gates.review.timeout_ms` is treated as a failed attempt and counts toward `max_attempts`.

This spec is symphony-aligned and roki-style: no persistent storage, no LLM judgment in the daemon, agent-owned write effects, and a stable artifact path that downstream consumers (roki-distill-postmerge) can rely on.

## Boundary Context

- **In scope**: a `TransitionSubscriber` registered against roki-mvp's `AwaitingReview -> TerminalSuccess` vetoable transition; structured validation of `review.md` (presence, schema, per-criterion pass/fail, code references); orchestration of an agent-side review turn with bounded purpose (produce `review.md`); a `kiro_review_status` read-only tool registered through roki-mvp's `Registry` trait; new `WORKFLOW.md` schema keys under the reserved `extension.gates.review.*` namespace (`required_status`, `timeout_ms`, `max_attempts`); a fix-finding feedback loop that re-routes the issue back to the implementation phase with findings injected as additional agent context, bounded by `max_attempts` and respecting roki-mvp's overall worker `max_turns` budget; an escalation transition event after attempt exhaustion or timeout; a stable artifact path `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` consumable by roki-distill-postmerge.
- **Out of scope**: any LLM-style or semantic judgment of whether code actually satisfies a criterion (judgment lives entirely in the review turn); auto-merge orchestration; auto-PR-open; Linear writes (the agent still drives Linear via `linear_graphql`); pull-request creation, branch management, or `gh` invocation (the agent owns those); spec materialization (owned by roki-spec-gate); implementation phase mechanics (owned by roki-mvp); the prompt content of the review turn beyond its observable inputs and outputs (owned by the kiro-review skill); persistent storage of review history.
- **Adjacent expectations**: roki-mvp publishes the state machine, the `TransitionSubscriber` interface with `veto()` semantics, the `Tool`/`Registry` trait for read-only tool registration, the `WORKFLOW.md` schema with reserved `extension.gates.*` namespaces, and the `<workspace_root>/<repo>/<issue>/` workspace path layout; roki-spec-gate writes `.kiro/specs/<issue>/requirements.md` with EARS-shaped acceptance criteria that the review turn reads; the kiro-review skill is installed as a personal skill under `~/.claude/skills/kiro-review/` and is auto-invoked by description; roki-distill-postmerge will consume `review.md` at its stable path as one input for archive/distill decisions; the operator configures the review gate per repo through `WORKFLOW.md`.

## Requirements

### Requirement 1: State-Machine Hook on AwaitingReview Transition

**Objective:** As an operator, I want roki to refuse the `AwaitingReview -> TerminalSuccess` transition until a structured review artifact exists, so that no Linear ticket can advance to a PR-ready state on the agent's self-assessment alone.

#### Acceptance Criteria
1. When the roki daemon starts, the review gate shall register a `TransitionSubscriber` against the orchestrator that subscribes to the `AwaitingReview -> TerminalSuccess` transition declared as vetoable by roki-mvp.
2. When the orchestrator evaluates an `AwaitingReview -> TerminalSuccess` transition, the review gate shall return a `Deny` decision unless the configured pass criteria for the corresponding `(repo, issue)` are met.
3. When the review gate returns a `Deny` decision, the roki daemon shall keep the issue in `AwaitingReview` and shall publish a structured veto log event that names the failing pass criterion.
4. When the review gate returns an `Allow` decision, the roki daemon shall let the orchestrator commit the transition to `TerminalSuccess` without further interception by this gate.
5. If the review gate raises an unhandled error while evaluating a transition, the roki daemon shall treat the result as `Deny` so that the gate fails closed and shall log the error with the gate's identifier.

### Requirement 2: Review Artifact Path and Structural Schema

**Objective:** As a downstream consumer (operator, roki-distill-postmerge), I want the review artifact at a stable, documented path with a stable schema, so that I can rely on the location and shape across spec evolutions.

#### Acceptance Criteria
1. The review gate shall locate the review artifact at `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` derived from the workspace path layout published by roki-mvp.
2. The review gate shall require the review artifact to declare an overall status field whose value is one of `pass` or `fail`.
3. The review gate shall require the review artifact to enumerate, for every numeric requirement ID present in the corresponding `.kiro/specs/<issue>/requirements.md`, a per-criterion entry with a status value in `pass` or `fail`.
4. The review gate shall require every per-criterion entry whose status is `pass` to include at least one code reference, where a code reference is a path to a file inside the workspace (with optional line range) that is reachable on disk at validation time.
5. The review gate shall publish the artifact path and schema in `SPEC.md` so future ports and roki-distill-postmerge can rely on it without reading Rust source.

### Requirement 3: Daemon-Side Structural Validation

**Objective:** As an operator, I want the daemon to validate the review artifact structurally without applying any LLM judgment, so that the gate's behavior is deterministic, auditable, and free of model drift.

#### Acceptance Criteria
1. When the review gate evaluates a transition, it shall validate the review artifact only by checking file presence, schema shape, per-criterion pass/fail status, and the presence of code references on pass entries.
2. If the review artifact is absent, the review gate shall record the gate result as `fail-missing` and shall not attempt any semantic interpretation of the absence.
3. If the review artifact is present but does not parse against the published schema, the review gate shall record the gate result as `fail-schema` and shall log the offending key path.
4. If the review artifact is present and parses but a code reference on a `pass` entry points to a path that does not exist inside the workspace, the review gate shall record the gate result as `fail-evidence` and shall log the offending entry.
5. The review gate shall never invoke any language-model API or any heuristic substring search to decide whether code actually satisfies a criterion; substantive judgment belongs to the review turn.

### Requirement 4: Constrained Agent-Side Review Turn

**Objective:** As an operator, I want the review gate to invoke a single bounded "review turn" that produces the review artifact, so that the gate's evaluation is preceded by an agent-side judgment that the daemon then validates.

#### Acceptance Criteria
1. When the review gate has no usable review artifact for the current attempt, the review gate shall request the engine adapter to launch a constrained review turn for the issue's `(repo, issue)`.
2. The review turn shall be constrained so that its declared purpose is to produce `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` by reading the spec criteria from `.kiro/specs/<issue>/requirements.md` and the implementation diff produced by the active phase.
3. The review turn shall invoke the kiro-review skill (auto-invoked by description, no slash command dependency); the daemon shall not embed prompt text for the skill.
4. While the review turn is running, the review gate shall not allow a parallel implementation phase to run for the same `(repo, issue)`.
5. When the review turn completes, the review gate shall re-validate the review artifact in the same evaluation cycle and shall record the resulting gate result without launching another review turn within the same attempt.

### Requirement 5: Fix-Finding Feedback Loop

**Objective:** As an operator, I want a failed review to re-enter the implementation phase with the findings injected as additional agent context, so that the agent can address concrete failures rather than re-doing self-assessment.

#### Acceptance Criteria
1. When the review gate records a failed gate result and the attempt counter is below `extension.gates.review.max_attempts`, the review gate shall route the issue from `AwaitingReview` back to `Active` and shall increment the attempt counter for that `(repo, issue)`.
2. When the issue re-enters `Active` after a failed review, the review gate shall provide the failing per-criterion entries (criterion id, fail reason, any review-turn diagnostic text) as additional context to the next worker invocation through a documented engine-adapter channel.
3. The review gate shall not consume any worker turns beyond what the engine adapter records against the per-worker `max_turns` budget published by roki-mvp; the fix-finding loop shall respect that budget.
4. If the attempt counter reaches `extension.gates.review.max_attempts` without a passing review, the review gate shall stop re-routing the issue, shall request a transition to `TerminalFailure`, and shall publish an escalation event that names the issue, the attempt count, and the most recent failure reason.
5. The review gate shall reset the attempt counter for an `(repo, issue)` when the issue enters a fresh `AwaitingReview` state from a non-veto path so that operator-driven retries start clean.

### Requirement 6: WORKFLOW.md Schema Keys

**Objective:** As an operator, I want the review gate to be configured per repository through `WORKFLOW.md` under the reserved `extension.gates.review.*` namespace, so that policy lives in the repo and reloads without restarting the daemon.

#### Acceptance Criteria
1. The review gate shall consume configuration only from the `extension.gates.review.*` namespace in the parsed `WorkflowPolicy` exposed by roki-mvp's `WorkflowLoader`.
2. The review gate shall require the `extension.gates.review.required_status` key to declare the artifact status that counts as a pass (default `pass`).
3. The review gate shall require the `extension.gates.review.timeout_ms` key to bound the duration of a single review turn (default value documented in `SPEC.md`); a review turn that does not complete within `timeout_ms` shall be treated as a failed attempt.
4. The review gate shall require the `extension.gates.review.max_attempts` key to bound the number of review attempts per `(repo, issue)` lifecycle in `AwaitingReview` (default 3).
5. When `WORKFLOW.md` hot reload changes any `extension.gates.review.*` key, the review gate shall apply the new values to subsequent attempts and shall not retroactively reset attempt counters that are already in flight.

### Requirement 7: Read-Only `kiro_review_status` Tool

**Objective:** As the agent, I want a read-only `kiro_review_status` tool registered in roki-mvp's tool registry so that subsequent turns can self-check the gate state without parsing daemon logs or guessing.

#### Acceptance Criteria
1. The review gate shall register a `kiro_review_status` tool against roki-mvp's `Registry` trait when the daemon starts.
2. When the agent invokes `kiro_review_status` with the current `(repo, issue)`, the tool shall return a structured response containing the artifact presence flag, the latest gate result, the current attempt counter, the configured `max_attempts`, and the most recent failure reason if any.
3. The `kiro_review_status` tool shall be read-only: it shall not mutate state, shall not produce side effects on the workspace, and shall not invoke Linear or `gh`.
4. The `kiro_review_status` tool shall report the same gate result that the daemon used to make its most recent veto or allow decision for that `(repo, issue)`; the agent shall not be able to observe a divergent view.
5. The `kiro_review_status` tool shall apply credential redaction consistent with roki-mvp's tool-registry redaction policy and shall not echo any secret strings even if they appear inside a failure reason.

### Requirement 8: Time Boundedness, Escalation, and Cross-Spec Contract

**Objective:** As an operator, I want the review gate to be time-bounded, to escalate cleanly, and to honor the cross-spec contract with roki-spec-gate and roki-distill-postmerge, so that the gate never blocks indefinitely and downstream consumers stay coherent.

#### Acceptance Criteria
1. While a review turn is in flight, the review gate shall enforce `extension.gates.review.timeout_ms` and shall record a `fail-timeout` gate result for any review turn that does not complete in time.
2. When the review gate reaches `extension.gates.review.max_attempts` without a passing review, the review gate shall publish an escalation transition event whose payload includes the `(repo, issue)` key, the attempt count, the last gate result code, and the last failure reason.
3. The review gate shall read the spec criteria for an issue exclusively from `.kiro/specs/<issue>/requirements.md` produced by roki-spec-gate; if that file is absent, the review gate shall record `fail-missing-spec`, shall not synthesize criteria of its own, and shall route to escalation.
4. The review gate shall keep the review artifact path stable across spec versions so that roki-distill-postmerge can consume it without coupling to roki-review-gate's internal types.
5. The review gate shall log every gate decision with the `(repo, issue)` key, the correlation identifier of the review turn (if any), the attempt counter, and the resulting decision so that operators can audit the gate without an external UI.
