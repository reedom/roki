# Implementation Plan

- [ ] 1. Foundation: schema, configuration, and engine-adapter integration seam

- [ ] 1.1 Define the review artifact schema and decision-code taxonomy
  - Implement `ReviewArtifact`, `CriterionEntry`, `CodeRef`, and `ArtifactStatus` types in `src/gates/review/schema.rs` plus `src/gates/review/artifact.rs` with serde + serde_yaml deserialization for the documented frontmatter shape.
  - Implement the canonical `DecisionCode` enum and a `ValidationOutcome` enum that maps every documented failure case (`FailMissing`, `FailMissingSpec`, `FailSchema`, `FailEvidence`).
  - Reject malformed YAML, missing fields, and non-numeric criterion IDs at parse time with a typed error that names the offending key path.
  - Observable completion: a unit test parses a representative passing `review.md` plus three malformed variants (missing `status`, missing `criteria`, malformed code-ref shape) and asserts each returns the expected typed error or `ValidationOutcome` variant including the offending key path.
  - _Requirements: 2.2, 2.3, 2.4, 3.3_
  - _Boundary: gates/review/schema, gates/review/artifact_

- [ ] 1.2 Implement the `ReviewGateConfig` over `WorkflowPolicy`
  - Read `extension.gates.review.required_status`, `extension.gates.review.timeout_ms`, and `extension.gates.review.max_attempts` from the `WorkflowPolicy::extension` scope exposed by roki-mvp's `WorkflowLoader`.
  - Apply documented defaults (`required_status = "pass"`, `timeout_ms = 600_000`, `max_attempts = 3`) and surface a typed `ReviewGateSettings { required_status, timeout, max_attempts }`.
  - Read settings on every gate evaluation so hot reload is honored on the next attempt.
  - Observable completion: a unit test loads three `WORKFLOW.md` policy fixtures (defaults only, all three keys overridden, one key invalid type) and asserts the resulting `ReviewGateSettings` plus a typed config error for the invalid case.
  - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5_
  - _Boundary: gates/review/config_

- [ ] 1.3 Extend `WorkerContext` with the additional-context channel and forward it at engine launch
  - Add an optional `additional_context: Option<AdditionalContext>` field to roki-mvp's `WorkerContext` and a stable `AdditionalContext { kind: &'static str, findings: Vec<FixFindingFinding> }` envelope shape, documented in this spec.
  - Update the engine adapter to forward the payload to the next worker invocation through a documented prelude mechanism (e.g., write `.kiro/specs/<issue>/.review-findings.json` into the workspace before subprocess launch and reference it from the launch prompt).
  - Confirm no other roki-mvp call site is broken: existing launches pass `None` and observe identical behavior to today.
  - Observable completion: an integration test launches a worker with `Some(additional_context)` and asserts the prelude file exists in the workspace before the fake `claude` subprocess starts and contains the documented JSON envelope; another test launches with `None` and asserts the file is absent.
  - _Requirements: 5.2, 5.3_
  - _Depends: 1.1_
  - _Boundary: engine (cross-spec integration into roki-mvp)_

- [ ] 2. Core: validator, turn orchestrator, attempt tracker, injector, status tool

- [ ] 2.1 (P) Implement `ReviewArtifactValidator`
  - Locate `review.md` at `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md` and `requirements.md` at the sibling path.
  - Parse the artifact via the schema from 1.1; cross-reference per-criterion entries against the numeric requirement IDs found in `requirements.md` (extract IDs by scanning headings of the form `Requirement N`).
  - For every `pass` entry, canonicalize each code-reference path against the workspace root and reject any that escape, are absolute, or do not exist.
  - Return exactly one `ValidationOutcome` variant; never invoke any LLM API and never read code-file contents for semantic comparison.
  - Observable completion: unit tests exercise each `ValidationOutcome` variant: missing review, missing spec, malformed frontmatter (with offending key path in the error), evidence failure (escape attempt + non-existent path), and a clean pass; an additional test verifies that a passing artifact whose overall status differs from the configured `required_status` is treated as a fail.
  - _Requirements: 2.1, 2.2, 2.3, 2.4, 3.1, 3.2, 3.3, 3.4, 3.5, 8.3_
  - _Depends: 1.1, 1.2_
  - _Boundary: gates/review/validator_

- [ ] 2.2 (P) Implement `ReviewAttemptTracker`
  - In-memory `HashMap<(RepoId, IssueId), AttemptState>` behind a `tokio::sync::Mutex`; expose `read`, `increment`, `reset`.
  - Reset semantics: zero the counter and clear `last_failure_reason` only when a fresh `AwaitingReview` entry arrives from a non-veto trigger.
  - Increment semantics: only on recorded gate fails; record `last_decision` and `last_failure_reason`.
  - Observable completion: a unit test runs a few hundred concurrent `read` / `increment` / `reset` operations across two keys and asserts the per-key counters match the deterministic increment count, with no panics.
  - _Requirements: 5.1, 5.4, 5.5_
  - _Boundary: gates/review/attempts_

- [ ] 2.3 (P) Implement `FixFindingInjector`
  - Map every failing `ValidationOutcome` entry plus any `TurnOutcome` diagnostic into a `FixFindingFinding { criterion_id, reason, diagnostic_excerpt, referenced_paths }` and assemble an `AdditionalContext { kind: "review-fix-finding", findings }`.
  - Cap total payload size at a small fixed byte budget; truncate per-entry `diagnostic_excerpt` first and emit a structured truncation log event when truncation occurs.
  - Apply roki-mvp's tracing redaction layer to every text field before assembling the payload.
  - Observable completion: a unit test feeds a `ValidationOutcome::FailEvidence` with three failing entries, asserts the resulting `AdditionalContext` includes all three with correct mapping, and a second test exceeds the byte budget to assert truncation plus the truncation log event.
  - _Requirements: 5.2, 5.3_
  - _Depends: 1.1, 2.1_
  - _Boundary: gates/review/inject_

- [ ] 2.4 (P) Implement `ReviewTurnOrchestrator`
  - Build a `WorkerContext` whose declared purpose is the review turn (a `purpose: "review"` tag plus a baseline `AdditionalContext` referencing `requirements.md` and the implementation diff); call `Engine::launch` and apply a tokio timer of `timeout_ms`.
  - On timer elapse, request engine cancellation and return `TurnOutcome::Timeout`; on engine error, return `TurnOutcome::EngineFailure`; on clean engine completion, return `TurnOutcome::Completed`.
  - Refuse to launch a parallel implementation phase for the same `(repo, issue)` while the review turn is in flight (rely on roki-mvp's per-`(repo, issue)` single-task model and document the assumption).
  - Observable completion: an integration test using a stub engine drives three scenarios (clean completion within timeout, timeout-triggered cancellation, simulated engine failure) and asserts the resulting `TurnOutcome` plus that exactly one engine launch occurred per attempt.
  - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5, 8.1_
  - _Depends: 1.2, 1.3_
  - _Boundary: gates/review/turn_

- [ ] 2.5 (P) Implement the `KiroReviewStatusTool` and register it through `Registry`
  - Implement the `Tool` trait: input schema `{ repo: string, issue: string }`, output schema `{ artifact_present: boolean, last_decision: string, attempts: integer, max_attempts: integer, last_failure_reason: string | null }`.
  - On call, read the `ReviewAttemptTracker` and (cheaply) re-check artifact presence via the validator without launching a turn; return the projection. Apply the redaction layer for every text field.
  - Register the tool through roki-mvp's `Registry::register` at daemon start, refusing to start the daemon on registration failure.
  - Observable completion: a unit test invokes the tool with a known `(repo, issue)` whose tracker state is populated and asserts the response payload matches the tracker exactly; a second test asserts an injected secret string in `last_failure_reason` does not appear in the response after redaction.
  - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5_
  - _Depends: 2.1, 2.2_
  - _Boundary: gates/review/status_tool_

- [ ] 3. Integration: gate subscriber, escalation, and SPEC.md

- [ ] 3.1 Implement `ReviewGate` and register it as a `TransitionSubscriber`
  - Implement `TransitionSubscriber::on_transition` for counter reset on a fresh `AwaitingReview` entry from a non-veto trigger, and `TransitionSubscriber::veto` for the `AwaitingReview -> TerminalSuccess` decision logic.
  - Compose validator + tracker + turn orchestrator + injector + config to produce a single `VetoDecision` per call; fail closed on any unhandled internal error and log with the gate identifier.
  - Register exactly one instance via roki-mvp's `Orchestrator::subscribe` at daemon start; declare the subscription scope (vetoable transition only) explicitly.
  - Observable completion: an integration test brings up the daemon with stubs for tracker and engine, drives an `AwaitingReview -> TerminalSuccess` evaluation through a passing artifact and asserts `VetoDecision::Allow`; a second drives the same evaluation with a missing artifact and asserts `VetoDecision::Deny` plus a structured veto log event naming the failing criterion.
  - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 8.5_
  - _Depends: 2.1, 2.2, 2.3, 2.4, 1.2_
  - _Boundary: gates/review_

- [ ] 3.2 Implement the fix-finding feedback loop and exhaustion escalation
  - On a fail decision with `attempt < max_attempts`: increment the tracker, build the `AdditionalContext` via the injector, and request the orchestrator route the issue from `AwaitingReview` back to `Active` so the next worker invocation receives the additional context.
  - On a fail decision with `attempt >= max_attempts`: stop re-routing, request a transition to `TerminalFailure`, and publish an escalation event with `(repo, issue, attempt_count, last_decision_code, last_failure_reason)`.
  - Confirm the loop respects roki-mvp's per-worker `max_turns` budget: the gate never extends the budget, never injects extra turns beyond the engine adapter's accounting.
  - Observable completion: an integration test runs three failing review attempts followed by exhaustion; asserts the issue routes to `Active` twice (with `AdditionalContext` reaching the stub engine on each re-entry) and to `TerminalFailure` on the third with the documented escalation event payload.
  - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5, 8.2_
  - _Depends: 3.1_
  - _Boundary: gates/review_

- [ ] 3.3 Handle missing-spec and timeout edge cases end-to-end
  - When `requirements.md` is absent for the `(repo, issue)`, route directly to escalation (`TerminalFailure` + escalation event) without consuming an attempt; the escalation event names `fail-missing-spec`.
  - When the review turn does not complete within `timeout_ms`, treat the attempt as a fail with decision code `FailTimeout`, increment the tracker, and feed into the standard fix-finding-or-escalation branch (not a separate path).
  - Observable completion: two integration tests — one starts in `AwaitingReview` with no `requirements.md` and asserts immediate escalation with `fail-missing-spec`; the other configures a stub turn that hangs and asserts `FailTimeout` is recorded and the engine cancellation request was sent within `timeout_ms + 500ms`.
  - _Requirements: 8.1, 8.3_
  - _Depends: 3.1, 3.2_
  - _Boundary: gates/review_

- [ ] 3.4 Extend `SPEC.md` with the Review Gate section
  - Add a Review Gate section to `SPEC.md` documenting the `review.md` path (`<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md`), the frontmatter schema, the per-criterion entry shape, the `DecisionCode` taxonomy, the `extension.gates.review.{required_status, timeout_ms, max_attempts}` keys with defaults, and the `AdditionalContext` envelope used by the additional-context channel.
  - Cross-reference the contract that roki-distill-postmerge consumes the `review.md` artifact at this stable path.
  - Observable completion: `SPEC.md` contains a "Review Gate" heading whose body addresses each of: artifact path, schema, decision codes, config keys, and the additional-context envelope; a manual cross-check confirms the language is implementation-neutral.
  - _Requirements: 2.5, 8.4_
  - _Depends: 1.1, 1.2, 1.3_
  - _Boundary: SPEC.md_

- [ ] 4. Validation: end-to-end paths and audit logging

- [ ] 4.1 End-to-end happy-path: pass on first attempt
  - Use the daemon harness with fake Linear and fake `claude`. The fake `claude` writes a passing `review.md` (overall `pass`, every criterion `pass` with at least one reachable code reference) when the review turn launches.
  - Drive `AwaitingReview -> TerminalSuccess`; assert exactly one review turn launched, attempt counter is 1, decision is `Pass`, and the orchestrator commits `TerminalSuccess`.
  - Observable completion: the test passes deterministically and the audit log records exactly one decision event with `(repo, issue, attempt = 1, decision = "pass")`.
  - _Requirements: 1.4, 8.5_
  - _Depends: 3.1_

- [ ] 4.2 End-to-end fix-finding loop: pass on second attempt
  - The fake `claude` first writes a `review.md` with one `fail-evidence` entry, then on re-entry to `Active` reads the prelude `AdditionalContext` and writes a passing `review.md` on the next review turn.
  - Assert the additional-context prelude file is present in the workspace before the second worker invocation, the attempt counter increments to 2, and the final decision is `Allow`.
  - Observable completion: the test passes deterministically; the audit log shows two decision events and one `route_back_to_active` event between them.
  - _Requirements: 5.1, 5.2, 5.3_
  - _Depends: 3.2, 4.1_

- [ ] 4.3 End-to-end exhaustion: fail on every attempt
  - The fake `claude` writes a failing `review.md` on every attempt; `max_attempts = 3`.
  - Assert three attempts are recorded, the orchestrator routes to `TerminalFailure`, the workspace is retained, and the escalation event payload matches the documented shape.
  - Observable completion: the test passes deterministically and the post-run filesystem layout still contains the workspace plus the last failing `review.md`.
  - _Requirements: 5.4, 8.2_
  - _Depends: 3.2, 3.3, 4.2_

- [ ] 4.4 End-to-end status-tool projection
  - Across the same harness, the agent invokes `kiro_review_status` between attempts; assert the response payload at each call matches the gate's logged decision and the tracker's counters.
  - Observable completion: the test passes deterministically and the response always includes `attempts`, `max_attempts`, `last_decision`, and `last_failure_reason` consistent with the gate's audit log.
  - _Requirements: 7.2, 7.4_
  - _Depends: 2.5, 4.1, 4.2_

- [ ] 4.5 End-to-end hot-reload of `extension.gates.review.*`
  - Mid-run, mutate `WORKFLOW.md` to lower `max_attempts` from 3 to 2; assert in-flight attempt counters are not retroactively reset and the new value applies to the next attempt.
  - Observable completion: the test passes deterministically; the audit log shows the new setting applied at the next decision and not before.
  - _Requirements: 6.5_
  - _Depends: 3.1, 3.2_

- [ ] 4.6* Optional: redaction regression coverage for the status tool and injector
  - Inject known secret strings into failure-reason fields and additional-context diagnostic excerpts; assert neither escapes through `kiro_review_status` nor through the prelude file written into the workspace.
  - Observable completion: the test asserts the redaction layer applies consistently across both surfaces.
  - _Requirements: 7.5, 5.2_
  - _Depends: 2.3, 2.5_
