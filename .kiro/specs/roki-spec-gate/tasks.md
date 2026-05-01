# Implementation Plan

- [ ] 1. Foundation: gate module scaffolding and configuration
- [ ] 1.1 Scaffold the spec-gate module tree
  - Create the `src/gates/spec/` directory and add empty `mod.rs`, `subscriber.rs`, `evaluation.rs`, `materialization.rs`, `validator.rs`, `attempts.rs`, `config.rs`, `tool.rs`, `events.rs` with module declarations only
  - Add `mod gates;` to the crate root and `mod spec;` to `src/gates/mod.rs` so the module compiles cleanly with no public surface yet
  - Observable completion: `cargo check` succeeds with the new module tree present and no warnings about unused code beyond standard "unused" hints
  - _Requirements: 1.1_
  - _Boundary: SpecGate, src/gates/spec/_

- [ ] 1.2 Implement `ConfigResolver` with documented defaults and validation
  - Define `SpecGateConfig` with `required_status: String`, `timeout_ms: u32`, `max_attempts: u32`
  - Read `extension.gates.spec.required_status`, `extension.gates.spec.timeout_ms`, `extension.gates.spec.max_attempts` from roki-mvp's `WorkflowPolicy` via `WorkflowLoader::current(repo)`
  - Apply defaults `required_status = "Todo"`, `timeout_ms = 600_000`, `max_attempts = 3` when keys are absent and log each defaulted key once per resolution
  - Return a typed misconfiguration error when `timeout_ms` or `max_attempts` resolves non-positive
  - Observable completion: a unit test asserts each default is applied, each defaulted key produces a single log event, and a non-positive override returns the misconfiguration error
  - _Requirements: 7.1, 7.2, 7.4, 7.5_
  - _Boundary: ConfigResolver_

- [ ] 1.3 Implement `events.rs` structured event helpers
  - Declare the fixed event-name set: `spec_gate.evaluation.start`, `spec_gate.materialization.start`, `spec_gate.materialization.end`, `spec_gate.timeout`, `spec_gate.validation.outcome`, `spec_gate.decision`, `spec_gate.escalation`, `spec_gate.misconfigured`, `spec_gate.internal_error`
  - Provide tracing helpers that always include `repo`, `issue`, `correlation_id`, and `attempt_index` where applicable
  - Route every emission through roki-mvp's tracing pipeline so the existing redaction layer applies
  - Observable completion: a unit test captures emitted events and asserts each helper includes the expected stable name and required context fields
  - _Requirements: 9.1, 9.2, 9.3, 9.5_
  - _Boundary: events.rs_

- [ ] 2. Core: state, validator, materialization, evaluation
- [ ] 2.1 (P) Implement `AttemptStore` with restart-tolerant in-memory state
  - Provide a per-`(RepoId, IssueId)` map storing `attempt_count`, `in_flight`, `last_correlation_id`, `last_outcome`, `last_reason`, `last_updated`
  - Expose interior-mutability methods so the `kiro_spec_status` tool can hold an `Arc` clone without locking out the coordinator
  - Provide a snapshot builder that yields `{ artifact_present, attempt_count, attempts_remaining, last_outcome, last_reason }`
  - Persist nothing to disk; assert the store starts empty after a synthetic restart in tests
  - Observable completion: unit tests prove `attempt_count` monotonicity, `correlation_id`-based dedup of duplicate transition events, and a clean post-restart state
  - _Requirements: 4.3, 6.2, 6.3, 8.2, 8.4, 8.5_
  - _Boundary: AttemptStore_

- [ ] 2.2 (P) Implement `ArtifactValidator` mechanical EARS-shape check
  - Resolve the artifact path as `<workspace>/.kiro/specs/<issue>/requirements.md`
  - Fail closed with `Missing`, `Empty`, `Unreadable`, `Oversize`, or `NotUtf8` when the corresponding precondition fails (use a documented size cap)
  - Compile a case-insensitive regex once that finds at least one occurrence of `WHEN`, `IF`, `WHILE`, `WHERE`, or `SHALL` in EARS-shaped positions
  - Return `ValidationOutcome { verdict, reason }` where `reason` matches the published enum
  - Observable completion: unit tests cover each `ValidationReason` variant against representative artifacts, including a passing example and a non-EARS file that triggers `NoEarsTrigger`
  - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5_
  - _Boundary: ArtifactValidator_

- [ ] 2.3 (P) Implement `MaterializationDriver` constrained-turn dispatch
  - Build a constrained prompt frame whose only stated purpose is producing `.kiro/specs/<issue>/requirements.md`, including the Linear ticket identifier, the absolute workspace path, the relative `.kiro/specs/` location, and a directive to rely on kiro-discovery skill auto-invocation
  - Dispatch the prompt through roki-mvp's existing engine adapter for the `(repo, issue)` worker session
  - Surface a `TurnOutcome` enum (`Completed`, `EngineError`, `EngineStalled`) without interpreting intermediate agent messages
  - Observable completion: a unit test against a fake engine adapter verifies the prompt includes the constrained-purpose directive and the kiro-discovery cue, and that `TurnOutcome` faithfully reflects the fake's reported state
  - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.5_
  - _Boundary: MaterializationDriver_
  - _Depends: 1.1_

- [ ] 2.4 Implement `EvaluationCoordinator` per-attempt loop and decision mapping
  - Acquire a per-`(repo, issue)` lock so concurrent vetoable events for the same `(repo, issue)` collapse to one in-flight attempt
  - Short-circuit to `Allow` when the trigger context indicates the issue is not in `extension.gates.spec.required_status`; evaluate conservatively if status metadata is unavailable
  - Resolve `SpecGateConfig` per evaluation; on misconfiguration return `Deny { reason: "spec_gate_misconfigured" }`
  - Wrap materialization-and-validation in `tokio::time::timeout(timeout_ms, ...)`; treat elapsed timeouts as `Fail { reason: timeout }` without back-filling a pass
  - Increment `attempt_count` once per logical attempt using `correlation_id` dedup; map outcomes to `GateOutcome::Pass` or `GateOutcome::Deny { reason, escalate }` and emit `spec_gate.decision` and `spec_gate.escalation` events
  - Observable completion: integration-style tests prove pass, fail-with-attempts-remaining, attempts-exhausted, timeout, and misconfiguration paths each yield the documented `GateOutcome` and event sequence
  - _Requirements: 2.4, 4.1, 4.2, 4.3, 4.4, 4.5, 4.6, 5.1, 5.2, 5.3, 5.4, 5.5, 7.3, 7.5, 8.1, 8.2, 8.3_
  - _Boundary: EvaluationCoordinator_
  - _Depends: 1.2, 1.3, 2.1, 2.2, 2.3_

- [ ] 3. Agent tool surface
- [ ] 3.1 Implement `SpecStatusTool` for the `kiro_spec_status` tool registration
  - Implement roki-mvp's `Tool` trait with `name() = "kiro_spec_status"`
  - Declare an input schema requiring `repo: string`, `issue: string` and an output schema covering `artifact_path`, `artifact_present`, `attempt_count`, `attempts_remaining`, `last_outcome`, `last_reason` per the design
  - Read `AttemptStore` snapshots without mutation and never include credentials or paths outside the queried `(repo, issue)`
  - Return a not-found-shaped response (not an error) when the orchestrator does not currently track the requested `(repo, issue)`
  - Observable completion: unit tests dispatch the tool through a fake `Registry`, assert the response matches the published JSON-Schema for both tracked and untracked `(repo, issue)`, and confirm `AttemptStore` state is unchanged before and after the call
  - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5_
  - _Boundary: SpecStatusTool_
  - _Depends: 2.1_

- [ ] 4. Integration: subscriber wiring and boot installation
- [ ] 4.1 Implement `SubscriberAdapter` against roki-mvp's `TransitionSubscriber`
  - Implement `on_transition` as a no-op and `veto` only for `Queued -> Active`; return `VetoDecision::Allow` immediately for any other transition
  - Convert internal `GateOutcome::Pass` to `VetoDecision::Allow` and `GateOutcome::Deny { reason, escalate }` to `VetoDecision::Deny { reason }`, emitting `spec_gate.escalation` when `escalate` is true
  - Catch unexpected internal errors and map them to `VetoDecision::Deny { reason: "spec_gate_internal_error" }` while logging the original error through the redacted tracing pipeline
  - Observable completion: unit tests prove allow, deny-with-reason, escalation event emission, fail-closed on internal error, and Allow short-circuit for non-`Queued -> Active` events
  - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 5.1, 5.2, 5.3, 5.5, 9.4_
  - _Boundary: SubscriberAdapter_
  - _Depends: 2.4_

- [ ] 4.2 Implement `SpecGate::install` boot wiring
  - Construct `ConfigResolver`, `AttemptStore`, `ArtifactValidator`, `MaterializationDriver`, `EvaluationCoordinator`, `SubscriberAdapter`, `SpecStatusTool`
  - Subscribe `SubscriberAdapter` through roki-mvp's `Orchestrator::subscribe` and register `SpecStatusTool` through roki-mvp's `Registry::register`
  - Hold the returned `SubscriptionHandle` inside a `SpecGateHandle` so shutdown is clean
  - Fail boot loudly with a structured error if either the subscription or the tool registration fails
  - Add the single call site in roki-mvp's boot wiring that invokes `SpecGate::install`
  - Observable completion: a smoke test boots the daemon harness with the gate installed, verifies the subscriber appears in the orchestrator's registered set, and verifies `kiro_spec_status` is callable through the registry
  - _Requirements: 1.1, 6.1_
  - _Boundary: SpecGate, src/main.rs_
  - _Depends: 3.1, 4.1_

- [ ] 4.3 Update `SPEC.md` with the spec-gate extension contract
  - Add a subsection under the existing extension-points discussion in `SPEC.md` covering the reserved `extension.gates.spec.*` keys, their documented defaults, the documented total time bound `timeout_ms * max_attempts` plus overhead, and the `kiro_spec_status` tool name with its read-only contract
  - Observable completion: `SPEC.md` contains the new subsection and a future-port author can implement a conformant spec gate without reading the Rust source
  - _Requirements: 7.1, 7.2, 4.6, 6.1_
  - _Boundary: SPEC.md_
  - _Depends: 4.2_

- [ ] 5. Validation: integration coverage of the gate end-to-end
- [ ] 5.1 (P) End-to-end pass and retry coverage in `tests/integration_spec_gate.rs`
  - Drive a fake orchestrator and fake engine adapter through a single-attempt pass path that materializes a valid EARS file and yields `VetoDecision::Allow` plus a `spec_gate.decision` event with verdict `pass`
  - Drive a fail-then-pass path where the first attempt produces no artifact and the second attempt produces a valid one; assert two `attempt_count` increments, a `Deny` followed by an `Allow`, and no escalation event
  - Drive an attempts-exhausted path where every attempt produces no artifact; assert `max_attempts` `Deny` decisions in sequence, a single `spec_gate.escalation` event on the final attempt, and refusal to start additional attempts for the same `(repo, issue)`
  - Observable completion: the test file passes under `cargo test --test integration_spec_gate` and the assertions on event names and ordering match the design's structured event set
  - _Requirements: 1.2, 2.1, 3.1, 4.5, 5.1, 5.2, 5.3, 8.2, 9.1, 9.4_
  - _Boundary: integration_spec_gate.rs_
  - _Depends: 4.2_

- [ ] 5.2 (P) Tool round-trip coverage in `tests/integration_spec_gate_tool.rs`
  - Dispatch `kiro_spec_status` through a fake `Registry` from a fake worker session for both tracked and untracked `(repo, issue)` pairs
  - Assert response shape matches the published JSON-Schema, that `AttemptStore` state does not change across the call, and that no field exposes credentials or paths outside the queried scope
  - Observable completion: the test file passes under `cargo test --test integration_spec_gate_tool` and JSON-Schema validation runs as part of the test
  - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5_
  - _Boundary: integration_spec_gate_tool.rs_
  - _Depends: 4.2_

- [ ] 5.3 (P) Configuration coverage in `tests/integration_spec_gate_config.rs`
  - Verify each `extension.gates.spec.*` default is applied when the key is absent from a fake `WorkflowPolicy` and that each defaulted key emits exactly one log event per resolution
  - Hot-reload `extension.gates.spec.max_attempts` through the fake `WorkflowLoader` mid-test and assert the next evaluation uses the new value without daemon restart
  - Set `extension.gates.spec.timeout_ms` to a non-positive value and assert evaluations for that repo return `VetoDecision::Deny { reason: "spec_gate_misconfigured" }` with a `spec_gate.misconfigured` event
  - Observable completion: the test file passes under `cargo test --test integration_spec_gate_config` and the misconfiguration path leaves AttemptStore unchanged
  - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5, 9.1_
  - _Boundary: integration_spec_gate_config.rs_
  - _Depends: 4.2_

- [ ] 5.4 Concurrency and idempotency burst test
  - Issue 50 concurrent vetoable events across distinct `(repo, issue)` pairs and assert each evaluation runs independently with no cross-contamination of `AttemptStore` entries or logs
  - Issue a duplicate-event burst against a single `(repo, issue)` carrying the same `correlation_id` and assert `attempt_count` increments at most once per logical attempt
  - Observable completion: the test passes deterministically under `cargo test`, and the assertions on counter monotonicity and decision determinism hold under repeated runs
  - _Requirements: 8.1, 8.2, 8.3_
  - _Boundary: integration_spec_gate.rs_
  - _Depends: 5.1_
