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
roki-review-gate adds a daemon-enforced pre-PR review checkpoint to the roki orchestrator. Without it, a "done" signal depends entirely on the agent's self-assessment, and finished implementations may still violate EARS acceptance criteria. The gate plugs into roki-mvp's state-machine subscription hooks at the **`Active -> Inactive`** vetoable transition (the worker's clean-exit transition): before that transition is allowed, the daemon requires a structured `review.md` artifact at `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` attesting per-criterion pass with code-level references. The review artifact is produced **inside the worker session** by the kiro skill set (per-task `kiro-review` + feature-level `kiro-validate-impl` + fresh-evidence `kiro-verify-completion`, all auto-invoked by description) before the worker clean-exits — the daemon does not launch a separate constrained review turn. When the artifact is absent, malformed, or marked as failing, the daemon Denies the transition with a structured fix-finding payload, which roki-mvp re-routes back to `Active` by re-launching the worker with the payload injected via `additional_context`, up to a configurable `max_attempts`. Daemon-side validation is purely structural (file presence, schema shape, per-criterion pass status, code references), with no LLM judgment. The gate registers a read-only `kiro_review_status` tool through roki-mvp's tool registry so the agent can self-check. New `WORKFLOW.md` keys live under the reserved `extension.gates.review.*` namespace: `required_status`, `max_attempts`. Cross-spec contract: review reads spec criteria from `.kiro/specs/<issue>/requirements.md` produced by roki-spec-gate.

## Introduction

The roki-review-gate specification defines a daemon-enforced pre-PR quality checkpoint for the roki system. It bolts onto roki-mvp's published state-machine extension points without forking the orchestrator: it registers a `TransitionSubscriber` that vetoes the `Active -> Inactive` transition unless a structurally valid review artifact exists in the per-issue workspace. The artifact is produced inside the worker subprocess by the kiro skill set (`kiro-review` per task, `kiro-validate-impl` feature-level, `kiro-verify-completion` as fresh-evidence gate, all auto-invoked by description) against the spec criteria written by roki-spec-gate and the implementation diff produced during the active phase, and is written to disk before the worker clean-exits.

The gate's daemon-side responsibility is bounded to validation: presence of `review.md`, schema shape, per-criterion pass/fail status, and the presence of code-level references on each pass entry. Substantive judgment is the kiro skill set's responsibility (per-task `kiro-review` for code-vs-task alignment, feature-level `kiro-validate-impl` for cross-task integration). On gate failure, the gate returns a `Deny+RetryWithContext(payload)` decision that roki-mvp's orchestrator turns into a re-launch of the worker subprocess with `payload` forwarded via the engine adapter's `additional_context` channel, up to `extension.gates.review.max_attempts` (default 3). After exhausting attempts, the gate returns a plain `Deny`, the orchestrator routes the issue to `Inactive(reason=review_gate_exhausted)`, and roki-mvp dispatches the linear-updater subagent to surface the escalation on Linear and the TUI escalation queue.

This spec is symphony-aligned and roki-style: no persistent storage, no LLM judgment in the daemon, agent-owned write effects (including the `review.md` produced inside the worker session via the kiro skill set), and a stable artifact path.

## Boundary Context

- **In scope**: a `TransitionSubscriber` registered against roki-mvp's `Active -> Inactive` vetoable transition; structured validation of `review.md` (presence, schema, per-criterion pass/fail, code references); a `kiro_review_status` read-only tool registered through roki-mvp's `Registry` trait; new `WORKFLOW.md` schema keys under the reserved `extension.gates.review.*` namespace (`required_status`, `max_attempts`); a fix-finding feedback loop driven through the orchestrator's `Deny+RetryWithContext(payload)` decision shape, bounded by `max_attempts` and respecting roki-mvp's overall worker `max_turns` budget; an escalation event after attempt exhaustion (delivered to the operator via roki-mvp's linear-updater dispatch and the TUI escalation queue); a stable artifact path `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` produced by the worker session.
- **Out of scope**: any LLM-style or semantic judgment of whether code actually satisfies a criterion (judgment lives entirely inside the worker session via the kiro skill set); auto-merge orchestration; auto-PR-open; Linear writes (the worker still drives Linear via the operator's installed Linear MCP); pull-request creation, branch management, or `gh` invocation (the worker owns those); spec materialization (owned by roki-spec-gate); a daemon-launched constrained review turn (removed — review artifact is produced inside the worker, not by a separate `claude` subprocess); the prompt content of any kiro skill beyond its observable inputs and outputs (owned by the kiro skill set authors); persistent storage of review history; per-attempt time bounds on a daemon-launched review turn (no such turn exists; the review artifact is produced before worker clean exit, bounded by the worker's own `max_turns`).
- **Adjacent expectations**: roki-mvp publishes the state machine, the `TransitionSubscriber` interface with `Allow` / `Deny` / `Deny+RetryWithContext(payload)` semantics, the `Tool`/`Registry` trait for read-only tool registration, the `WORKFLOW.md` schema with reserved `extension.gates.*` namespaces, the `additional_context` channel on `WorkerContext` that the orchestrator uses to forward `payload`, the linear-updater dispatch on `Inactive(reason=review_gate_exhausted)`, and the `<workspace_root>/<repo>/<issue>/` workspace path layout; roki-spec-gate writes `.kiro/specs/<issue>/requirements.md` with EARS-shaped acceptance criteria that the kiro skill set reads; the kiro skill set (`kiro-impl`, `kiro-review`, `kiro-validate-impl`, `kiro-debug`, `kiro-verify-completion`) is installed under `~/.claude/skills/kiro-*/` (or the project's `.claude/skills/`) and auto-invokes by description from inside the worker session before clean exit; the operator configures the review gate per repo through `WORKFLOW.md`.

## Requirements

### Requirement 1: State-Machine Hook on Active → Inactive Transition

**Objective:** As an operator, I want roki to refuse the worker's clean-exit `Active -> Inactive` transition until a structured review artifact exists, so that no Linear ticket can advance to a PR-ready state on the agent's self-assessment alone.

#### Acceptance Criteria
1. When the roki daemon starts, the review gate shall register a `TransitionSubscriber` against the orchestrator that subscribes to the `Active -> Inactive` transition declared as vetoable by roki-mvp.
2. When the orchestrator evaluates an `Active -> Inactive` transition, the review gate shall return one of three decisions: `Allow` (artifact valid), `Deny+RetryWithContext(payload)` (artifact invalid and retry budget remaining), or `Deny` (artifact invalid and retry budget exhausted, or internal gate error).
3. When the review gate returns `Deny+RetryWithContext(payload)`, the orchestrator shall transition the issue from `Active → Inactive` back to `Active`, re-launch a fresh worker subprocess with `payload` forwarded as `additional_context`, and increment the attempt counter for the issue. The review gate is responsible for constructing `payload` (the failing per-criterion entries, criterion id, fail reason, diagnostic text); roki-mvp is responsible for the re-launch.
4. When the review gate returns plain `Deny` (retry exhausted), the orchestrator shall route the issue to `Inactive(reason=review_gate_exhausted)` and dispatch the linear-updater subagent with a `review_gate_exhausted` directive.
5. When the review gate returns `Allow`, the orchestrator shall complete the transition to `Inactive(reason=awaiting_linear)` without further interception by this gate.
6. If the review gate raises an unhandled error while evaluating a transition, the daemon shall treat the result as plain `Deny` (fail-closed) and shall log the error with the gate's identifier; the issue shall route to `Inactive(reason=review_gate_exhausted)`.

### Requirement 2: Review Artifact Path and Structural Schema

**Objective:** As a downstream consumer (operator, future readers), I want the review artifact at a stable, documented path with a stable schema, so that I can rely on the location and shape across spec evolutions.

#### Acceptance Criteria
1. The review gate shall locate the review artifact at `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` derived from the workspace path layout published by roki-mvp.
2. The review gate shall require the review artifact to declare an overall status field whose value is one of `pass` or `fail`.
3. The review gate shall require the review artifact to enumerate, for every numeric requirement ID present in the corresponding `.kiro/specs/<issue>/requirements.md`, a per-criterion entry with a status value in `pass` or `fail`.
4. The review gate shall require every per-criterion entry whose status is `pass` to include at least one code reference, where a code reference is a path to a file inside the workspace (with optional line range) that is reachable on disk at validation time.
5. The review gate shall publish the artifact path and schema in `docs/reference/artifacts.md` so that the worker session (the producer) and any future consumer share a single source of truth.

### Requirement 3: Daemon-Side Structural Validation

**Objective:** As an operator, I want the daemon to validate the review artifact structurally without applying any LLM judgment, so that the gate's behavior is deterministic, auditable, and free of model drift.

#### Acceptance Criteria
1. When the review gate evaluates a transition, it shall validate the review artifact only by checking file presence, schema shape, per-criterion pass/fail status, and the presence of code references on pass entries.
2. If the review artifact is absent, the review gate shall record the gate result as `fail-missing` and shall not attempt any semantic interpretation of the absence.
3. If the review artifact is present but does not parse against the published schema, the review gate shall record the gate result as `fail-schema` and shall log the offending key path.
4. If the review artifact is present and parses but a code reference on a `pass` entry points to a path that does not exist inside the workspace, the review gate shall record the gate result as `fail-evidence` and shall log the offending entry.
5. The review gate shall never invoke any language-model API or any heuristic substring search to decide whether code actually satisfies a criterion; substantive judgment belongs to the kiro skill set that runs inside the worker session.

### Requirement 4: Worker-Produced Review Artifact (no daemon-launched review turn)

**Objective:** As an operator, I want the review artifact to be produced inside the worker session by the kiro skill set rather than by a separate daemon-launched turn, so that ticket processing remains a single bounded `claude` invocation per attempt and the daemon does not embed prompts of its own.

#### Acceptance Criteria
1. The review gate shall not launch any `claude` subprocess of its own; the engine adapter exposes no review-turn launch interface for this gate.
2. The review gate shall assume the worker session is responsible for producing `review.md` before clean exit; the worker's kiro skill set auto-invokes per-task `kiro-review` and feature-level `kiro-validate-impl` (auto-invoked by description, no slash command dependency).
3. If the worker clean-exits without a `review.md` at the published path, the review gate shall record `fail-missing` and proceed to `Deny+RetryWithContext(...)` per Requirement 5; on the next attempt the kiro skill receives the failure context via `additional_context` and is expected to re-attempt review production.
4. The review gate shall not embed prompt text for any kiro skill anywhere in the daemon binary; daemon ↔ skill coupling is limited to the artifact-path contract and the structured `additional_context` payload shape.
5. When the review gate evaluates the `Active → Inactive` transition, it shall validate the review artifact in a single pass per attempt and shall not re-evaluate within the same attempt.

### Requirement 5: Fix-Finding Feedback Loop via additional_context

**Objective:** As an operator, I want a failed review to re-enter the implementation phase with the findings injected as additional agent context, so that the worker's next turn can address concrete failures rather than re-doing self-assessment.

#### Acceptance Criteria
1. When the review gate records a failed gate result and the attempt counter is below `extension.gates.review.max_attempts`, the review gate shall return `Deny+RetryWithContext(payload)` to the orchestrator and shall increment the attempt counter for that issue.
2. The `payload` constructed by the review gate shall include the failing per-criterion entries (criterion id, fail reason, any diagnostic text the kiro skill set emitted) in the engine adapter's documented `additional_context` shape; roki-mvp's engine adapter is responsible for the verbatim forwarding to the worker prompt's machine-extractable section.
3. The review gate shall not consume any worker turns beyond what the engine adapter records against the per-worker `max_turns` budget published by roki-mvp; the fix-finding loop shall respect that budget.
4. If the attempt counter reaches `extension.gates.review.max_attempts` without a passing review, the review gate shall stop returning `Deny+RetryWithContext` and shall return plain `Deny`; the orchestrator routes the issue to `Inactive(reason=review_gate_exhausted)` and roki-mvp dispatches the linear-updater subagent with a `review_gate_exhausted` directive whose payload includes the issue identifier, the attempt count, and the most recent failure reason.
5. The review gate shall reset the attempt counter for an issue when the issue enters a fresh `Active` state from a non-veto path (operator-driven retry, re-admission per roki-mvp Req 3.14) so that operator-driven retries start clean.

### Requirement 6: WORKFLOW.md Schema Keys

**Objective:** As an operator, I want the review gate to be configured through `WORKFLOW.md` under the reserved `extension.gates.review.*` namespace, so that policy lives in the repo and reloads without restarting the daemon.

#### Acceptance Criteria
1. The review gate shall consume configuration only from the `extension.gates.review.*` namespace in the parsed `WorkflowPolicy` exposed by roki-mvp's `WorkflowLoader`.
2. The review gate shall require the `extension.gates.review.required_status` key to declare the artifact status that counts as a pass (default `pass`).
3. The review gate shall require the `extension.gates.review.max_attempts` key to bound the number of fix-finding re-launches per issue lifecycle (default 3).
4. When `WORKFLOW.md` hot reload changes any `extension.gates.review.*` key, the review gate shall apply the new values to subsequent attempts and shall not retroactively reset attempt counters that are already in flight.
5. The review gate shall not introduce its own per-attempt time-bound key (`timeout_ms` or similar); time bounding for the review-artifact-producing work is enforced by roki-mvp's per-worker `max_turns` and stall detection on the same worker subprocess.

### Requirement 7: Read-Only `kiro_review_status` Tool

**Objective:** As the agent, I want a read-only `kiro_review_status` tool registered in roki-mvp's tool registry so that subsequent turns can self-check the gate state without parsing daemon logs or guessing.

#### Acceptance Criteria
1. The review gate shall register a `kiro_review_status` tool against roki-mvp's `Registry` trait when the daemon starts.
2. When the agent invokes `kiro_review_status` with the current issue identifier, the tool shall return a structured response containing the artifact presence flag, the latest gate result, the current attempt counter, the configured `max_attempts`, and the most recent failure reason if any.
3. The `kiro_review_status` tool shall be read-only: it shall not mutate state, shall not produce side effects on the workspace, and shall not invoke Linear or `gh`.
4. The `kiro_review_status` tool shall report the same gate result that the daemon used to make its most recent veto or allow decision for that issue; the agent shall not be able to observe a divergent view.
5. The `kiro_review_status` tool shall apply credential redaction consistent with roki-mvp's tool-registry redaction policy and shall not echo any secret strings even if they appear inside a failure reason.

### Requirement 8: Escalation, Cross-Spec Contract, and Logging

**Objective:** As an operator, I want the review gate to escalate cleanly through the linear-updater + TUI escalation queue path, to honor the cross-spec contract with roki-spec-gate, and to log every decision, so that the gate never blocks indefinitely and the daemon-only failure surfacing channel stays coherent.

#### Acceptance Criteria
1. When the review gate reaches `extension.gates.review.max_attempts` without a passing review, the review gate shall return plain `Deny` so that roki-mvp routes the issue to `Inactive(reason=review_gate_exhausted)` and dispatches the linear-updater subagent (per roki-mvp Req 5.10 and Req 12.2(e)); the gate shall include the issue identifier, the attempt count, the last gate result code, and the last failure reason in the structured event that feeds the linear-updater directive payload and the escalation queue.
2. The review gate shall read the spec criteria for an issue exclusively from `.kiro/specs/<issue>/requirements.md` produced by roki-spec-gate; if that file is absent, the review gate shall record `fail-missing-spec`, shall not synthesize criteria of its own, and shall return plain `Deny` (skipping the retry loop) so the issue routes to escalation immediately.
3. The review gate shall keep the review artifact path stable across spec versions so that the worker session (the producer) and any future consumer share a single source of truth.
4. The review gate shall log every gate decision with the issue identifier, the correlation identifier of the worker invocation, the attempt counter, and the resulting decision so that operators can audit the gate without an external UI.
5. The review gate shall not register its own Slack notification path nor any other side push channel; daemon-only failure surfacing for review-gate exhaustion routes through the canonical roki-mvp linear-updater + escalation queue path.
