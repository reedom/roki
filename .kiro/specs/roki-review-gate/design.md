---
refs:
  id: design:roki-review-gate
  kind: design
  title: "roki-review-gate Design"
  spec: roki-review-gate
  implements:
    - requirements:roki-review-gate
---

# Design Document

## Overview

**Purpose**: roki-review-gate adds a daemon-enforced pre-PR review checkpoint to roki by registering a `TransitionSubscriber` against roki-mvp's `AwaitingReview -> TerminalSuccess` vetoable transition. It refuses the transition until a structurally valid review artifact (`review.md`) exists at a stable per-issue path, and on failure routes the issue back into the implementation phase with findings injected as additional agent context, bounded by `extension.gates.review.max_attempts`.

**Users**: A roki operator who has roki-mvp running, configures the review gate per repo in `WORKFLOW.md`, and depends on the daemon to enforce that no Linear ticket reaches a PR-ready terminal state on the agent's self-assessment alone.

**Impact**: Closes the symphony-vs-roki gap on review enforcement: symphony relies on prompt convention; roki turns this into a daemon-enforced contract. Establishes the second of roki's two pre-PR gates (alongside roki-spec-gate) and publishes a stable `review.md` artifact that roki-distill-postmerge consumes downstream.

### Goals
- Register a daemon-side `TransitionSubscriber` that vetoes `AwaitingReview -> TerminalSuccess` until `review.md` validates structurally.
- Validate the artifact deterministically: presence, schema, per-criterion pass/fail, and reachable code references on pass entries — no LLM judgment, no heuristics.
- Orchestrate a constrained, time-bounded review turn that invokes the kiro-review skill agent-side and produces `review.md`.
- Re-enter the implementation phase on failure with structured findings injected as additional context, up to `max_attempts`, respecting the worker's `max_turns` budget.
- Publish a `kiro_review_status` read-only tool through roki-mvp's `Registry` so the agent can self-check.
- Land new `WORKFLOW.md` keys under the reserved `extension.gates.review.*` namespace without breaking the published schema.

### Non-Goals
- LLM or heuristic judgment of whether code actually satisfies a criterion (lives in the review turn / kiro-review skill).
- Auto-merge, auto-PR-open, Linear writes, branch management, `gh` invocation — the agent owns those, full stop.
- Spec materialization (owned by roki-spec-gate) or implementation phase mechanics (owned by roki-mvp).
- Persistent storage of review history or run analytics.
- Authoring the kiro-review skill prompt content.

## Boundary Commitments

### This Spec Owns
- A `ReviewGate` component that implements roki-mvp's `TransitionSubscriber` trait, scoped to the `AwaitingReview -> TerminalSuccess` vetoable transition.
- The `review.md` schema (frontmatter shape + per-criterion entry shape) and its location: `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md`.
- A `ReviewArtifactValidator` that performs presence, schema, per-criterion status, and code-reference reachability checks and produces a typed `GateResult`.
- A `ReviewTurnOrchestrator` that requests a constrained review turn from roki-mvp's engine adapter and tracks its lifecycle.
- A `ReviewAttemptTracker` that holds in-memory per-`(repo, issue)` attempt counters and reset semantics, with no persistence.
- A `FixFindingInjector` that converts the previous attempt's failing per-criterion entries into a structured additional-context payload for the next worker invocation, routed through the engine adapter's documented additional-context channel.
- The `kiro_review_status` read-only `Tool` registered through roki-mvp's `Registry`.
- The `extension.gates.review.{required_status, timeout_ms, max_attempts}` keys consumed from the `WorkflowPolicy` exposed by roki-mvp's `WorkflowLoader` (the keys live in the reserved namespace already round-tripped by roki-mvp; this spec does not change the `WorkflowSchema` itself).
- A `SPEC.md` extension that documents the `review.md` path and schema, the gate decision codes, and the configuration keys.

### Out of Boundary
- Changes to roki-mvp's state machine, vetoable-transition list, `TransitionSubscriber` trait, `Registry`/`Tool` traits, or `WorkflowSchema` infrastructure — this spec consumes those interfaces unchanged.
- The kiro-review skill's prompt, content, or judgment heuristics.
- Any logic that decides whether code actually satisfies a criterion.
- Linear writes, PR creation, branch management, `gh` CLI invocation.
- Spec materialization (`requirements.md` is produced by roki-spec-gate; this spec only reads it).
- Auto-merge, multi-host orchestration, persistent state.

### Allowed Dependencies
- Rust 2024 + tokio (consumed via roki-mvp's runtime, not new).
- roki-mvp's `Orchestrator::subscribe`, `TransitionSubscriber` (with `veto`), `TransitionEvent`, `WorkerState`.
- roki-mvp's `Registry::register`, `Tool` trait.
- roki-mvp's `WorkflowLoader::current` and the `extension` scope on `WorkflowPolicy`.
- roki-mvp's `Engine` trait for launching workers and an additional-context channel on `WorkerContext` (or an equivalent injection seam — see Cross-Spec Contract below).
- serde + serde_yaml for parsing `review.md` frontmatter.
- pulldown-cmark or hand-rolled scanner for cross-checking referenced code paths inside the workspace (path reachability only; no semantic parsing).
- tracing for structured log events with `(repo, issue, correlation_id)` context.
- thiserror for typed adapter errors.

### Revalidation Triggers
Changes that should force dependent specs (roki-distill-postmerge, future ports) to re-check integration:
- Any change to the `review.md` path or to the structural schema (status field, per-criterion entry shape, code-reference shape).
- Any change to the gate decision-code taxonomy (`fail-missing`, `fail-schema`, `fail-evidence`, `fail-timeout`, `fail-missing-spec`, `pass`).
- Any change to the `extension.gates.review.*` key set or default values.
- Any change to the additional-context injection contract that the engine adapter exposes.
- Any change to escalation-event payload shape (the `TerminalFailure` route on attempt exhaustion).

### Cross-Spec Contract Note (Engine Adapter Additional-Context Channel)

This spec depends on the engine adapter exposing a way to pass structured additional context to the next worker invocation when re-entering the implementation phase from a failed review. roki-mvp's published `WorkerContext` does not currently include this field by name, but its design states "the orchestrator depends only on those traits" and the engine adapter is permitted to evolve. The cleanest contract surface is to extend `WorkerContext` (or a sibling envelope) with an optional structured additional-context payload that the engine adapter forwards to the agent at launch (e.g., as a documented stream-json prelude or a workspace-side scratch file the agent reads). The exact mechanism is implementation-internal to roki-mvp's engine adapter; this spec consumes whichever stable seam roki-mvp provides. If roki-mvp has not yet exposed such a seam, the first task in this spec adds it as a small additive change to `WorkerContext` plus engine-adapter forwarding semantics, declared explicitly as an integration task that crosses into roki-mvp's owned files. (See Tasks: integration task 3.x.)

## Architecture

### Architecture Pattern & Boundary Map

```mermaid
graph TB
    Operator[Operator]
    Linear[Linear API]
    Workspace[(Workspace dir per repo issue)]
    SpecMd[.kiro/specs/issue/requirements.md]
    ReviewMd[.kiro/specs/issue/review.md]

    subgraph RokiMvp[roki-mvp daemon core]
        Orch[Orchestrator]
        Eng[Engine adapter]
        WfLoader[WORKFLOW.md loader]
        ToolReg[Tool registry]
        EventBus[Transition event bus]
    end

    subgraph ReviewGate[roki-review-gate]
        Gate[ReviewGate subscriber]
        Validator[ReviewArtifactValidator]
        TurnOrch[ReviewTurnOrchestrator]
        Tracker[ReviewAttemptTracker]
        Injector[FixFindingInjector]
        StatusTool[kiro_review_status tool]
    end

    Operator --> WfLoader
    Orch --> EventBus
    EventBus --> Gate
    Gate --> Validator
    Validator --> ReviewMd
    Validator --> SpecMd
    Validator --> Workspace
    Gate --> TurnOrch
    TurnOrch --> Eng
    Eng --> ReviewMd
    Gate --> Tracker
    Gate --> Injector
    Injector --> Eng
    Gate --> WfLoader
    StatusTool --> ToolReg
    StatusTool --> Tracker
    StatusTool --> Validator
```

**Architecture Integration**:
- **Selected pattern**: Subscriber-of-orchestrator with a small set of focused collaborators. The gate is a single `TransitionSubscriber` that delegates to a validator, a turn orchestrator, an attempt tracker, an injector, and a read-only status tool. No new bus, no new state machine.
- **Domain boundaries**: Gate (decision) vs Validator (structural check) vs Turn Orchestrator (engine-adapter request) vs Attempt Tracker (in-memory counter) vs Injector (engine-adapter prelude payload) vs Status Tool (read-only projection).
- **Existing patterns preserved**: roki-mvp's hexagonal core, vetoable-transition contract, `Tool`/`Registry`, `extension.gates.*` reserved namespace, no DB, agent owns writes.
- **New components rationale**: each collaborator has exactly one responsibility; combining them would either pull LLM judgment into the daemon (forbidden) or couple the gate to engine-adapter internals (forbidden).
- **Steering compliance**: Rust 2024 + tokio, no SQLite, kiro skills as personal skills, macOS + Linux, agent owns Linear/PR writes.

### Technology Stack

| Layer | Choice / Version | Role in Feature | Notes |
|-------|------------------|-----------------|-------|
| Runtime | Rust 2024 + tokio 1.x (inherited from roki-mvp) | Async subscriber dispatch, engine-adapter request | No new runtime dependency |
| Subscription | roki-mvp `TransitionSubscriber` | Single subscriber registered against the vetoable transition | Hard fail closed on subscriber error per roki-mvp |
| Front matter | serde + serde_yaml | Parse `review.md` frontmatter | Loose Markdown body permitted; only frontmatter is schema-validated |
| Markdown body scan | hand-rolled string scanner | Extract code-reference path tokens from per-criterion sections when not in frontmatter | Reachability check only; no semantic parsing |
| Validation | thiserror typed errors | Typed `GateResult` with one variant per decision code | One canonical decision-code enum |
| Logging | tracing (inherited) | Per-decision structured events with `(repo, issue, attempt, correlation_id)` | Same context fields roki-mvp already standardizes |
| Configuration | roki-mvp `WorkflowPolicy::extension` | Read `extension.gates.review.{required_status, timeout_ms, max_attempts}` | Loader is unchanged; namespace was reserved by roki-mvp |
| Engine integration | roki-mvp `Engine::launch` plus an additional-context channel on `WorkerContext` | Launch the constrained review turn; inject fix-finding payload on re-entry | Additive `WorkerContext` field; documented in this spec's integration task |

## File Structure Plan

### Directory Structure

```
src/
├── gates/
│   └── review/
│       ├── mod.rs                  # ReviewGate subscriber, registration entry point
│       ├── artifact.rs             # ReviewArtifact type, frontmatter parser, body code-ref scanner
│       ├── schema.rs               # Per-criterion entry shape, status enum, decision-code enum
│       ├── validator.rs            # ReviewArtifactValidator: presence, schema, code-ref reachability
│       ├── turn.rs                 # ReviewTurnOrchestrator: launch and time-bound the review turn
│       ├── attempts.rs             # ReviewAttemptTracker: in-memory counters, reset semantics
│       ├── inject.rs               # FixFindingInjector: build the additional-context payload
│       ├── status_tool.rs          # kiro_review_status Tool implementation
│       └── config.rs               # Read extension.gates.review.* from WorkflowPolicy
tests/
├── integration_review_gate.rs      # End-to-end gate veto + fix-finding loop with stub engine
└── integration_review_status_tool.rs
```

### Modified Files
- `src/orchestrator/mod.rs` (or wherever roki-mvp constructs subscribers) — register `ReviewGate` at daemon start.
- `src/tools/mod.rs` — register `kiro_review_status` through `Registry`.
- `src/engine/mod.rs` (or `src/engine/claude.rs`) — extend `WorkerContext` with an optional structured additional-context payload; forward it at worker launch as a documented prelude. (Integration task; ownership remains roki-mvp's.)
- `SPEC.md` — add the Review Gate section: `review.md` path, schema, decision-code taxonomy, `extension.gates.review.*` keys, additional-context channel contract.

> Each new file owns one responsibility. The integration into roki-mvp is intentionally narrow and additive: one new subscriber registration, one new tool registration, one new optional `WorkerContext` field.

## System Flows

### Gate decision lifecycle on `AwaitingReview -> TerminalSuccess`

```mermaid
sequenceDiagram
    participant Orch as Orchestrator
    participant Gate as ReviewGate
    participant Val as Validator
    participant Turn as TurnOrchestrator
    participant Eng as Engine adapter
    participant Inj as Injector
    participant Track as AttemptTracker

    Orch->>Gate: veto(AwaitingReview -> TerminalSuccess)
    Gate->>Val: validate(repo, issue)
    alt artifact present and passes
        Val-->>Gate: GateResult::Pass
        Gate-->>Orch: VetoDecision::Allow
    else artifact missing or fails
        Val-->>Gate: GateResult::Fail(code, reasons)
        Gate->>Track: read attempt
        alt attempt < max_attempts
            Gate->>Turn: launch review turn (timeout_ms)
            Turn->>Eng: launch constrained review turn
            Eng-->>Turn: review turn outcome (CleanExit | Stalled | Timeout)
            Turn-->>Gate: outcome
            Gate->>Val: re-validate(repo, issue)
            alt re-validate Pass
                Gate-->>Orch: VetoDecision::Allow
            else still Fail
                Gate->>Track: increment attempt
                Gate->>Inj: build additional context from reasons
                Gate-->>Orch: VetoDecision::Deny + request route to Active
            end
        else attempt >= max_attempts
            Gate->>Track: mark exhausted
            Gate-->>Orch: VetoDecision::Deny + request route to TerminalFailure (escalation)
        end
    end
```

> The gate uses `VetoDecision::Deny` for the immediate transition refusal; the route-back to `Active` (fix-finding) and the route to `TerminalFailure` (escalation) are requested through the same `TransitionSubscriber` channels roki-mvp already documents. The orchestrator owns committing the actual transition; the gate only requests it via the subscriber's documented escalation/re-route mechanism.

### Fix-finding injection into the next worker invocation

```mermaid
sequenceDiagram
    participant Gate as ReviewGate
    participant Inj as FixFindingInjector
    participant Eng as Engine adapter
    participant Worker as claude subprocess

    Gate->>Inj: build_payload(failing_entries)
    Inj-->>Gate: AdditionalContext payload
    Gate->>Eng: launch(worker_ctx with additional_context)
    Eng->>Worker: spawn with prelude prompt referencing payload
    Worker-->>Eng: stream-json events
```

## Requirements Traceability

| Requirement | Summary | Components | Interfaces | Flows |
|-------------|---------|------------|------------|-------|
| 1.1, 1.2, 1.3, 1.4, 1.5 | State-machine hook + Deny/Allow + fail-closed | ReviewGate | `TransitionSubscriber::veto` | Gate decision lifecycle |
| 2.1, 2.2, 2.3, 2.4, 2.5 | Artifact path + schema + traceability to requirements.md + SPEC.md publication | ReviewArtifact, ReviewSchema, SpecRoot | Frontmatter shape, per-criterion entry shape | n/a |
| 3.1, 3.2, 3.3, 3.4, 3.5 | Structural validation only, no LLM/heuristic judgment | ReviewArtifactValidator | `validate(repo, issue) -> GateResult` | Gate decision lifecycle |
| 4.1, 4.2, 4.3, 4.4, 4.5 | Constrained review turn, kiro-review auto-invocation, no parallel impl, single re-validate per attempt | ReviewTurnOrchestrator | `Engine::launch` with constrained `WorkerContext` | Gate decision lifecycle |
| 5.1, 5.2, 5.3, 5.4, 5.5 | Fix-finding loop, additional-context injection, max_turns respect, escalation on exhaustion, counter reset | ReviewAttemptTracker, FixFindingInjector | `WorkerContext.additional_context` channel | Fix-finding injection |
| 6.1, 6.2, 6.3, 6.4, 6.5 | `extension.gates.review.*` config consumption + hot reload semantics | ReviewGateConfig | `WorkflowLoader::current` | n/a |
| 7.1, 7.2, 7.3, 7.4, 7.5 | `kiro_review_status` read-only tool with redaction | KiroReviewStatusTool | `Tool` + `Registry::register` | n/a |
| 8.1, 8.2, 8.3, 8.4, 8.5 | Time-boundedness, escalation event, missing spec handling, stable artifact path, audit logs | ReviewGate, ReviewTurnOrchestrator | Decision-code enum + escalation transition request | Gate decision lifecycle |

## Components and Interfaces

| Component | Domain/Layer | Intent | Req Coverage | Key Dependencies (P0/P1) | Contracts |
|-----------|--------------|--------|--------------|--------------------------|-----------|
| ReviewGate | Gate orchestration | Single `TransitionSubscriber` against the vetoable `AwaitingReview -> TerminalSuccess` transition; coordinates collaborators and returns `VetoDecision` | 1.1, 1.2, 1.3, 1.4, 1.5, 5.1, 5.4, 8.1, 8.2, 8.5 | Orchestrator (P0), Validator (P0), TurnOrchestrator (P0), Tracker (P0), Injector (P0), Config (P0) | Service, Event |
| ReviewArtifact | Artifact model | In-memory representation of `review.md` (overall status + per-criterion entries with code references) | 2.2, 2.3, 2.4 | serde_yaml (P0) | State |
| ReviewSchema | Artifact schema | Static shape for the frontmatter and per-criterion entries; defines the decision-code enum | 2.2, 2.3, 2.4, 2.5 | n/a | State |
| ReviewArtifactValidator | Validation | Presence, schema, per-criterion status, code-reference reachability; never invokes LLMs or substring-matches code content | 2.1, 2.2, 2.3, 2.4, 3.1, 3.2, 3.3, 3.4, 3.5, 8.3 | Workspace filesystem (P0), spec criteria from `.kiro/specs/<issue>/requirements.md` (P0) | Service |
| ReviewTurnOrchestrator | Turn orchestration | Launch a constrained review turn through the engine adapter, enforce `timeout_ms`, surface outcome | 4.1, 4.2, 4.3, 4.4, 4.5, 8.1 | Engine adapter (P0), Config (P0) | Service |
| ReviewAttemptTracker | Counters | In-memory per-`(repo, issue)` attempt counter with reset semantics on fresh `AwaitingReview` entries | 5.1, 5.4, 5.5 | n/a | State |
| FixFindingInjector | Re-implementation feedback | Build the structured additional-context payload from failing per-criterion entries | 5.2, 5.3 | Engine adapter additional-context channel (P0) | Service |
| KiroReviewStatusTool | Read-only tool | Project the gate's last decision and counters to the agent through roki-mvp's `Registry` | 7.1, 7.2, 7.3, 7.4, 7.5 | Registry (P0), Tracker (P0), Validator (P1) | API |
| ReviewGateConfig | Configuration | Read `extension.gates.review.*` from `WorkflowPolicy`; surface defaults and hot-reload-aware accessors | 6.1, 6.2, 6.3, 6.4, 6.5 | WorkflowLoader (P0) | State |
| SpecRootExtension | Documentation | Add the Review Gate section to `SPEC.md` (artifact path, schema, decision codes, config keys) | 2.5, 8.4 | n/a | n/a |

### Gate orchestration

#### ReviewGate

| Field | Detail |
|-------|--------|
| Intent | Single `TransitionSubscriber` that decides Allow/Deny on `AwaitingReview -> TerminalSuccess` |
| Requirements | 1.1, 1.2, 1.3, 1.4, 1.5, 5.1, 5.4, 8.1, 8.2, 8.5 |

**Responsibilities & Constraints**
- Subscribe to roki-mvp's `Orchestrator::subscribe`; declare itself for the vetoable `AwaitingReview -> TerminalSuccess` transition only.
- On every `veto(event)`, invoke the validator first; if Pass, return `Allow`. Otherwise consult the attempt tracker and decide whether to launch a new review turn or escalate.
- Never invoke any LLM API; never run shell commands; never write Linear or invoke `gh`.
- Fail closed: any unhandled error during evaluation translates to `Deny` and is logged with the gate's identifier.
- Honor `extension.gates.review.timeout_ms` end-to-end across a single attempt, including the validator + turn + re-validate sequence.
- Log a single structured decision event per `veto` invocation with `(repo, issue, attempt, decision_code, correlation_id)`.

**Dependencies**
- Inbound: roki-mvp `Orchestrator` invokes `veto(event)` and `on_transition(event)` (P0).
- Outbound: ReviewArtifactValidator (P0), ReviewTurnOrchestrator (P0), ReviewAttemptTracker (P0), FixFindingInjector (P0), ReviewGateConfig (P0).

**Contracts**: Service [x] / API [ ] / Event [x] / Batch [ ] / State [ ]

##### Service Interface

```rust
pub struct ReviewGate {
    validator: Arc<ReviewArtifactValidator>,
    turn: Arc<ReviewTurnOrchestrator>,
    attempts: Arc<ReviewAttemptTracker>,
    injector: Arc<FixFindingInjector>,
    config: Arc<ReviewGateConfig>,
    workflow: Arc<dyn WorkflowLoader>, // from roki-mvp
}

#[async_trait]
impl TransitionSubscriber for ReviewGate {
    async fn on_transition(&self, event: &TransitionEvent) -> Result<(), SubscriberError> {
        // Reset attempt counter on a fresh AwaitingReview entry from a non-veto path.
        if event.next == WorkerState::AwaitingReview
            && !matches!(event.trigger, TransitionTrigger::SubscriberVeto)
        {
            self.attempts.reset(&event.repo, &event.issue).await;
        }
        Ok(())
    }

    async fn veto(&self, event: &TransitionEvent) -> Result<VetoDecision, SubscriberError> {
        // Only acts on the documented vetoable transition.
        if !(event.previous == WorkerState::AwaitingReview
            && event.next == WorkerState::TerminalSuccess)
        {
            return Ok(VetoDecision::Allow);
        }
        let policy = self.workflow.current(&event.repo).map_err(SubscriberError::from)?;
        let cfg = self.config.read(&policy).ok_or_else(|| SubscriberError::config_missing("extension.gates.review"))?;
        match self.evaluate(event, &cfg).await {
            Ok(GateOutcome::Pass) => Ok(VetoDecision::Allow),
            Ok(GateOutcome::Deny { reason }) => Ok(VetoDecision::Deny { reason }),
            Err(e) => {
                tracing::error!(repo = %event.repo, issue = %event.issue, error = %e, "review-gate evaluation failed; failing closed");
                Ok(VetoDecision::Deny { reason: format!("review-gate error: {e}") })
            }
        }
    }
}

pub enum GateOutcome {
    Pass,
    Deny { reason: String },
}

pub enum DecisionCode {
    Pass,
    FailMissing,
    FailSchema { offending_path: String },
    FailEvidence { criterion_id: String, missing_path: String },
    FailTimeout,
    FailMissingSpec,
    FailExhausted { last_reason: String, attempts: u32 },
}
```

- Preconditions: `Validator`, `TurnOrchestrator`, `AttemptTracker`, `Injector`, `Config`, and `WorkflowLoader` are constructed and injected before registration.
- Postconditions: every `veto` returns within the configured `timeout_ms` per attempt; every decision is logged; no external state is mutated except attempt counters.
- Invariants: no LLM call, no `gh`, no Linear write; `Deny` is the default on any internal error.

**Implementation Notes**
- Integration: register exactly one instance per daemon; rely on roki-mvp's per-subscriber error isolation.
- Validation: declared scope is exactly the vetoable transition; non-vetoable observations short-circuit to `Allow`.
- Risks: subscriber back-pressure if the review turn is slow. Mitigation: `timeout_ms` is enforced inside `evaluate`, and non-vetoable observations never trigger a turn.

### Validation

#### ReviewArtifactValidator

| Field | Detail |
|-------|--------|
| Intent | Structurally validate the review artifact and check code-reference reachability — no LLM, no heuristics |
| Requirements | 2.1, 2.2, 2.3, 2.4, 3.1, 3.2, 3.3, 3.4, 3.5, 8.3 |

**Responsibilities & Constraints**
- Locate `review.md` at `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md`.
- Parse the frontmatter as YAML; reject any document that does not parse against `ReviewSchema`.
- Cross-reference per-criterion entries against the numeric requirement IDs found in `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/requirements.md`. If `requirements.md` is missing, return `FailMissingSpec`.
- For every per-criterion entry whose status is `pass`, ensure each declared code reference resolves to an existing path inside the workspace root (canonicalize and assert descendant of workspace root).
- Never read code content for semantic comparison; reachability check is the entire substantive check.

**Dependencies**
- Inbound: ReviewGate (P0).
- Outbound: filesystem (P0), serde_yaml (P0).

**Contracts**: Service [x] / API [ ] / Event [ ] / Batch [ ] / State [ ]

##### Service Interface

```rust
pub trait ReviewArtifactValidator: Send + Sync {
    async fn validate(&self, repo: &RepoId, issue: &IssueId) -> Result<ValidationOutcome, ValidatorError>;
}

pub enum ValidationOutcome {
    Pass,
    FailMissing,
    FailMissingSpec,
    FailSchema { offending_path: String },
    FailEvidence { entries: Vec<EvidenceFailure> },
}

pub struct EvidenceFailure {
    pub criterion_id: String,
    pub missing_path: String,
}

pub struct ReviewArtifact {
    pub overall_status: ArtifactStatus,
    pub per_criterion: Vec<CriterionEntry>,
}

pub enum ArtifactStatus { Pass, Fail }

pub struct CriterionEntry {
    pub criterion_id: String,           // numeric requirement ID, e.g. "1.2"
    pub status: ArtifactStatus,
    pub code_refs: Vec<CodeRef>,
    pub note: Option<String>,
}

pub struct CodeRef {
    pub path: PathBuf,                  // relative to workspace root
    pub lines: Option<(u32, u32)>,
}
```

- Preconditions: workspace root and `(repo, issue)` are valid; canonicalization rules of roki-mvp's `WorkspaceManager` apply.
- Postconditions: returns exactly one `ValidationOutcome`; never panics on malformed YAML; never opens code files for content reads.
- Invariants: every `Pass` outcome implies the artifact parses, the requirement-ID set covers every numeric ID present in `requirements.md`, every `pass` entry has at least one reachable code reference, and the overall status equals the configured `required_status`.

**Implementation Notes**
- Integration: the validator reads `requirements.md` only for the numeric ID set; it does not interpret EARS prose.
- Validation: the canonicalize-and-assert-descendant check uses roki-mvp's documented workspace path-safety invariant — reject any reference that escapes the workspace root.
- Risks: agents may produce code references with platform-specific separators. Mitigation: normalize to forward slashes before canonicalization; reject absolute paths outright.

### Turn orchestration

#### ReviewTurnOrchestrator

| Field | Detail |
|-------|--------|
| Intent | Launch a constrained review turn through the engine adapter and enforce `timeout_ms` |
| Requirements | 4.1, 4.2, 4.3, 4.4, 4.5, 8.1 |

**Responsibilities & Constraints**
- Build a `WorkerContext` whose declared purpose is "produce review.md for (repo, issue) using kiro-review skill", referencing `requirements.md` and the implementation diff.
- Call `Engine::launch` with that context and the subscriber-supplied `ShutdownSignal`.
- Apply a tokio timer of `timeout_ms`; on elapse, request engine cancellation and record `FailTimeout`.
- Refuse to launch a parallel implementation phase for the same `(repo, issue)` while the review turn is in flight (coordinated through the orchestrator's per-`(repo, issue)` task model — only one engine launch is in flight at a time per key).

**Contracts**: Service [x] / API [ ] / Event [x] / Batch [ ] / State [ ]

##### Service Interface

```rust
pub trait ReviewTurnOrchestrator: Send + Sync {
    async fn run(
        &self,
        repo: &RepoId,
        issue: &IssueId,
        timeout: Duration,
    ) -> Result<TurnOutcome, TurnError>;
}

pub enum TurnOutcome {
    Completed,
    Timeout,
    EngineFailure { reason: String },
}
```

- Preconditions: Engine adapter constructed; the `(repo, issue)` is currently in `AwaitingReview`.
- Postconditions: returns within `timeout` plus a small grace window; on `Timeout`, the engine has been requested to terminate the subprocess.
- Invariants: review turn purpose declares it should not run implementation work; the daemon expresses this via the constrained `WorkerContext` (a documented `purpose` tag plus the additional-context payload that flags the turn as a review).

### Counters

#### ReviewAttemptTracker

| Field | Detail |
|-------|--------|
| Intent | In-memory per-`(repo, issue)` attempt counters with reset semantics |
| Requirements | 5.1, 5.4, 5.5 |

**Responsibilities & Constraints**
- Hold a `HashMap<(RepoId, IssueId), AttemptState>` behind a `tokio::sync::Mutex`.
- `increment` on a recorded fail; `read` on every gate evaluation; `reset` on a fresh `AwaitingReview` entry from a non-veto path.
- Never persist; the tracker is recreated on daemon restart, which is acceptable because Linear state and `review.md` presence carry the meaningful state.

**Contracts**: Service [x] / API [ ] / Event [ ] / Batch [ ] / State [x]

##### Service Interface

```rust
pub struct AttemptState {
    pub attempts: u32,
    pub last_decision: Option<DecisionCode>,
    pub last_failure_reason: Option<String>,
}

pub trait ReviewAttemptTracker: Send + Sync {
    async fn read(&self, repo: &RepoId, issue: &IssueId) -> AttemptState;
    async fn increment(&self, repo: &RepoId, issue: &IssueId, decision: DecisionCode, reason: Option<String>);
    async fn reset(&self, repo: &RepoId, issue: &IssueId);
}
```

### Injection

#### FixFindingInjector

| Field | Detail |
|-------|--------|
| Intent | Build a structured additional-context payload from the previous attempt's failing per-criterion entries |
| Requirements | 5.2, 5.3 |

**Responsibilities & Constraints**
- Map each failing per-criterion entry into a stable `FixFindingFinding` shape (criterion id, fail reason, optional review-turn diagnostic excerpt, optional referenced paths).
- Emit a single `AdditionalContext` payload that the engine adapter forwards to the next worker invocation as a documented prelude (e.g., a JSON line in stream-json or a `.kiro/specs/<issue>/.review-findings.json` workspace file the engine adapter places before launch).
- Cap payload size to a small fixed byte budget; if the failing entries exceed the cap, truncate per-entry diagnostic text first and log a structured truncation event.

**Contracts**: Service [x] / API [ ] / Event [ ] / Batch [ ] / State [ ]

##### Service Interface

```rust
pub struct FixFindingFinding {
    pub criterion_id: String,
    pub reason: String,                      // fail-schema | fail-evidence | turn-diagnostic
    pub diagnostic_excerpt: Option<String>,
    pub referenced_paths: Vec<PathBuf>,
}

pub struct AdditionalContext {
    pub kind: &'static str,                  // "review-fix-finding"
    pub findings: Vec<FixFindingFinding>,
}

pub trait FixFindingInjector: Send + Sync {
    fn build(&self, validation: &ValidationOutcome, last_turn: Option<&TurnOutcome>) -> AdditionalContext;
}
```

### Status tool

#### KiroReviewStatusTool

| Field | Detail |
|-------|--------|
| Intent | Read-only `Tool` that projects the gate's most recent decision and counters to the agent |
| Requirements | 7.1, 7.2, 7.3, 7.4, 7.5 |

**Responsibilities & Constraints**
- Register through roki-mvp's `Registry::register` at daemon start.
- On call, read the attempt tracker plus (optionally) re-run the validator without launching a turn; return a JSON envelope with `artifact_present`, `last_decision`, `attempts`, `max_attempts`, `last_failure_reason`.
- Apply the same redaction layer roki-mvp's tool registry uses; never echo secrets.

**Contracts**: Service [ ] / API [x] / Event [ ] / Batch [ ] / State [ ]

##### API Contract (`kiro_review_status`)

| Method | Endpoint | Request | Response | Errors |
|--------|----------|---------|----------|--------|
| Tool call | `kiro_review_status` | `{ repo: string, issue: string }` | `{ artifact_present: bool, last_decision: string, attempts: number, max_attempts: number, last_failure_reason: string \| null }` | `INVALID_INPUT`, `NOT_TRACKED` |

### Configuration

#### ReviewGateConfig

| Field | Detail |
|-------|--------|
| Intent | Read `extension.gates.review.*` from `WorkflowPolicy` and surface defaults |
| Requirements | 6.1, 6.2, 6.3, 6.4, 6.5 |

**Responsibilities & Constraints**
- Defaults: `required_status = "pass"`, `timeout_ms = 600000` (10 min), `max_attempts = 3`.
- Read on every gate evaluation so hot reload is honored on the next attempt; do not retroactively reset attempt counters that are already in flight.
- Surface a typed `ReviewGateSettings { required_status, timeout, max_attempts }` value.

```rust
pub struct ReviewGateSettings {
    pub required_status: ArtifactStatus,
    pub timeout: Duration,
    pub max_attempts: u32,
}
```

### Documentation

#### SpecRootExtension

Implementation note: extend `SPEC.md` with a Review Gate section that documents the `review.md` path and schema (overall status, per-criterion entry shape, code-reference shape), the decision-code taxonomy, and the `extension.gates.review.*` keys with defaults. The Rust implementation is a conformant implementation among possibly many.

## Data Models

### Domain Model

The review gate has no persistent domain model. Runtime in-memory aggregates:
- **AttemptState** keyed by `(RepoId, IssueId)` with `attempts`, `last_decision`, `last_failure_reason` — owned by `ReviewAttemptTracker`, recreated on daemon restart.
- **ReviewArtifact** transient, parsed per `validate` call from disk and discarded.

### Data Contracts & Integration

- `review.md` frontmatter (YAML) — published in `SPEC.md`:

```yaml
---
status: pass | fail
spec: <issue>                     # the issue identifier this artifact reviews
generated_at: <ISO 8601>
criteria:
  - id: "1.1"                     # numeric requirement ID from .kiro/specs/<issue>/requirements.md
    status: pass | fail
    code_refs:                    # required when status == pass; at least one entry
      - path: "src/foo/bar.rs"    # relative to workspace root
        lines: "42-67"            # optional
    note: "..."                    # optional
  - id: "1.2"
    status: fail
    note: "criterion not met because..."
---
# Body (free-form Markdown; daemon ignores the body except as a fallback when code_refs is omitted in YAML for a single criterion that explicitly opts in via inline tags — body parsing is not required for v1)
```

- `kiro_review_status` request: `{ repo, issue }`. Response: `{ artifact_present, last_decision, attempts, max_attempts, last_failure_reason | null }`.
- `AdditionalContext` envelope (engine-adapter input on re-entry): `{ kind: "review-fix-finding", findings: [...] }`.
- `WORKFLOW.md` extension keys (under `extension.gates.review.*`): `required_status: string`, `timeout_ms: integer`, `max_attempts: integer`.

## Error Handling

### Error Strategy

The review gate uses one canonical `DecisionCode` enum surfaced in logs and the status tool. Adapter errors (filesystem, serde) convert into the appropriate decision code rather than bubbling up to the orchestrator. Internal panics are caught and translated to `Deny` with a `review-gate error` reason — fail closed.

### Error Categories and Responses

- **Configuration errors** (missing `extension.gates.review` namespace): `Deny` with reason "config missing"; logged with the offending key path. Operator must fix `WORKFLOW.md`.
- **Filesystem errors** during validation (workspace not readable, canonicalization failure): `FailEvidence` for that entry; if global, treat as `FailMissing`. Always log the offending path.
- **Schema errors** in `review.md`: `FailSchema` with the offending key path. Logged.
- **Engine-adapter errors** during the review turn: `TurnOutcome::EngineFailure` mapped to `FailTimeout`-equivalent gate outcome unless the engine reports a clean exit; counts as one attempt.
- **Spec missing** (`requirements.md` not present): `FailMissingSpec`; routed to escalation immediately without counting against `max_attempts` (escalation event names the missing spec). This avoids burning attempts when the upstream gate has not produced a spec.
- **Tool registry errors** during `kiro_review_status` registration: refuse to start the daemon (consistent with roki-mvp's startup error policy).

### Monitoring

Every gate decision logs through tracing with fields: `repo`, `issue`, `attempt`, `max_attempts`, `decision_code`, `correlation_id` (when a review turn ran), and `last_failure_reason` for failure cases. The escalation event uses a distinct `event = "review_gate.escalation"` field for downstream filtering.

## Testing Strategy

### Unit Tests
- `ReviewArtifactValidator` returns `FailMissing` when `review.md` is absent and `FailMissingSpec` when `requirements.md` is absent.
- `ReviewArtifactValidator` returns `FailSchema` with the offending key path for malformed frontmatter and missing per-criterion entries.
- `ReviewArtifactValidator` returns `FailEvidence` when a `pass` entry references a path that does not exist, escapes the workspace root, or is absolute.
- `ReviewArtifactValidator` returns `Pass` only when the overall status equals the configured `required_status`, every numeric requirement ID from `requirements.md` has a per-criterion entry, and every `pass` entry has at least one reachable in-workspace code reference.
- `FixFindingInjector` produces an `AdditionalContext` whose `findings` cover every failing entry, truncates per-entry diagnostic text when over budget, and never includes secrets.
- `ReviewAttemptTracker` increments only on recorded fails, resets on a fresh `AwaitingReview` entry from a non-veto trigger, and reads concurrently without races (use `tokio::test` with a few hundred concurrent operations).
- `KiroReviewStatusTool` returns the same decision code that the gate last logged for the same `(repo, issue)` and applies the redaction layer to every field.

### Integration Tests
- Full gate lifecycle with a stub engine adapter: starts in `AwaitingReview`, validator returns `FailMissing`, gate launches a stub review turn that writes a passing `review.md`, gate re-validates, returns `Allow`. Assert exactly one transition reaches `TerminalSuccess` and the audit log records one decision per attempt.
- Fix-finding loop with a stub engine: validator returns `FailEvidence` twice, then `Pass`. Assert the `AdditionalContext` payload reaches the engine adapter on each re-entry, the attempt counter increments to 2, and the final decision is `Allow`.
- Exhaustion path: validator never passes; assert the gate routes to `TerminalFailure` after `max_attempts`, an escalation event is emitted with the attempt count and last reason, and no further review turns are launched.
- Timeout path: stub review turn never completes within `timeout_ms`; assert `FailTimeout` decision and that the engine cancellation request was sent.
- Hot reload: change `extension.gates.review.max_attempts` between attempts; assert the new value applies to subsequent attempts but does not retroactively reset the in-flight counter.
- `kiro_review_status` reflects the most recent gate decision after each attempt and matches the gate's own decision log.

### E2E Tests
- Full daemon harness with fake Linear, fake `claude`, and roki-mvp's other adapters: drive `Discovered -> Queued -> Active -> AwaitingReview -> AwaitingReview (fail) -> Active (fix-finding) -> AwaitingReview -> TerminalSuccess` and assert the resulting transition log plus the on-disk presence of `review.md` with status `pass`.
- Same harness but the fake `claude` cannot satisfy the criteria and the gate exhausts attempts; assert `TerminalFailure` and the workspace is retained for inspection.

### Performance / Load (informational)
- Validator latency budget: under 50 ms per call on a small workspace; verified with a single integration benchmark feeding a 50-entry `review.md`.
- Timeout enforcement: a stub turn that hangs is killed within `timeout_ms + 500ms` grace.

## Optional Sections

### Security Considerations

- The status tool inherits the redaction layer from roki-mvp's tool registry; failure-reason text passes through redaction before being returned.
- Code-reference reachability uses canonicalize-and-descendant-of-workspace-root checks (the same invariant roki-mvp's `WorkspaceManager` enforces) so a crafted `review.md` cannot point outside the workspace.
- The fix-finding payload never includes secrets; the injector explicitly redacts the same secret strings the daemon-wide tracing layer redacts.
- The gate never invokes the network; the only outbound effect is launching the engine adapter, which is the same path roki-mvp already audits.

### Performance & Scalability

- The gate's hot path is a single filesystem read of `review.md` plus a small set of `Path::canonicalize` calls. Negligible compared to a Claude Code subprocess turn.
- The attempt tracker is bounded by the number of active issues (low-tens per host per roki-mvp's stated target). Memory footprint is trivial.
- Hot-reload semantics are intentionally non-disruptive: in-flight attempts complete under their original settings; new attempts pick up the new settings.
