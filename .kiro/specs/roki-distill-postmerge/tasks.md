# Implementation Plan

- [ ] 1. Foundation: WORKFLOW.md extension keys and manifest schema

- [ ] 1.1 (P) Add `distill.paths` and `distill.routes` to the WORKFLOW.md schema
  - Extend the JSON-Schema published by roki-mvp's `workflow/schema.rs` to register `distill.paths` (list of workspace-relative path patterns), `distill.routes` (list of `{ id, pattern, disposition, archive_root? }` rules), `distill.sweep_max_turns` (integer, default 4), and `distill.project_archive_roots` (list of paths). All keys live under the existing reserved extension namespace and are additive.
  - Add round-trip types in `src/distill/workflow_ext.rs` and parse them out of `WorkflowPolicy.extension` so callers consume strongly-typed values.
  - Observable completion: a unit test loads a `WORKFLOW.md` with a populated `distill.*` block and asserts that the parsed `WorkflowPolicy.extension.distill` returns the expected typed values; a malformed disposition value (e.g. `"keep"`) is rejected with the offending key path captured in the error.
  - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5_
  - _Boundary: distill/workflow_ext, workflow/schema_

- [ ] 1.2 (P) Define the manifest types and v1 JSON-Schema
  - Implement `Manifest`, `ManifestEntry`, `Disposition`, and `ManifestSummary` in `src/distill/manifest.rs` with serde deserialization.
  - Implement `ManifestSchema` in `src/distill/schema.rs` as a `BTreeMap<String, JsonSchema>` keyed by `schema_version`; populate `"v1"` matching the documented shape (`schema_version`, `generated_at`, `repo`, `issue`, `entries[]`, `summary`).
  - Observable completion: a unit test deserializes a representative v1 manifest and asserts every field is present; another test rejects a manifest missing `entries` and reports the field path; a third test rejects a manifest with `schema_version: "v2"` as `UnrecognizedSchemaVersion`.
  - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5, 9.4_
  - _Boundary: distill/manifest, distill/schema_

- [ ] 1.3 (P) Implement archive path scheme helpers
  - In `src/distill/archive.rs`, expose `archive_root(workspace, issue) -> PathBuf` returning `<workspace>/.kiro/archive/<sanitized_issue>/` (sanitization reuses roki-mvp's workspace identifier rules) and `archive_destination(archive_root, original) -> Result<PathBuf, ArchiveError>` that asserts `original` is workspace-relative and returns the mirrored path.
  - Refuse absolute originals and parent-segment traversal at the helper boundary.
  - Observable completion: a unit test produces the expected mirrored path for a workspace-relative original; a second test rejects an absolute original path and a path containing `..` segments.
  - _Requirements: 10.1, 10.2, 10.4_
  - _Boundary: distill/archive_

- [ ] 1.4 Append the distill phase contract to SPEC.md
  - Document the manifest v1 JSON-Schema (fields, types, the `entries[]` shape, the `summary{}` totals, the disposition enum), the stable archive path scheme, and the `distill.*` WORKFLOW.md keys.
  - Document the post-terminal phase activation rule: triggered on `TerminalSuccess`, gates workspace deletion until validation succeeds, never blocks the merge or `Done` transition itself, never invokes any LLM daemon-side.
  - Observable completion: `SPEC.md` reads as self-contained for an alternative implementer; a reviewer can build the manifest schema and the archive path scheme from `SPEC.md` alone.
  - _Requirements: 7.2, 7.3, 7.4, 10.1, 11.2_

- [ ] 2. Core: phase coordinator, dispatcher, and validator

- [ ] 2.1 (P) Define `DistillPhaseStatus` and the `DistillPhase` coordinator skeleton
  - In `src/distill/phase.rs`, implement the `DistillPhaseStatus` enum (`Pending`, `Running`, `Complete`, `Failed { reason: DistillFailure }`) and the `DistillPhase` struct with an internal `Mutex<HashMap<(RepoId, IssueId), DistillPhaseStatus>>`.
  - Provide `enqueue(repo, issue, correlation_id)` and `status(repo, issue)` methods. `enqueue` is a no-op when status is `Complete` and refuses to re-dispatch when status is `Failed`.
  - Define the `DistillFailure` enum covering every failure variant in the design (`SweepTurnFailed`, `ManifestMissing`, `SchemaInvalid`, `PathUnsafe`, `ArchiveSchemeViolated`, `UnrecognizedSchemaVersion`).
  - Observable completion: unit tests exercise the status state machine: `enqueue` while `Complete` returns immediately; `enqueue` while `Failed` is rejected; `status` reflects every prior `enqueue` outcome.
  - _Requirements: 1.2, 9.3, 12.1, 12.3_
  - _Boundary: distill/phase_

- [ ] 2.2 (P) Implement the manifest validator
  - In `src/distill/validator.rs`, implement `ManifestValidator::validate(input) -> Result<ValidatedManifest, DistillFailure>`. Steps: read the manifest file from `.kiro/specs/<issue>/distill-manifest.json`; reject with `ManifestMissing` if absent; deserialize and validate `schema_version` against the schema registry; run jsonschema validation; for each entry, canonicalize `original_path` and `destination_path` and assert containment under the workspace root or any allowed `project_archive_roots`; for `archive`-disposition entries, additionally assert the destination is under the resolved archive root and mirrors the original relative path.
  - Reuse roki-mvp's `WorkspacePathSafety` trait for canonicalization and containment; the validator must not perform LLM calls, network I/O, or artifact-content reads.
  - Observable completion: unit tests cover (a) a valid manifest returns `Ok(ValidatedManifest)` with the correct summary; (b) missing `schema_version` returns `SchemaInvalid` with the field path; (c) a `destination_path` outside the workspace returns `PathUnsafe` with the offending path; (d) an `archive`-disposition entry whose destination is under `.kiro/archive/<other-issue>/` returns `ArchiveSchemeViolated`; (e) an `UnrecognizedSchemaVersion` is returned for `schema_version: "v99"`.
  - _Requirements: 8.1, 8.2, 8.3, 8.4, 8.5, 10.5, 11.3, 11.4, 11.5_
  - _Boundary: distill/validator_

- [ ] 2.3 Implement the sweep dispatcher
  - In `src/distill/dispatch.rs`, implement `SweepDispatcher::dispatch(ctx)` which constructs a `WorkerContext` with `max_turns = workflow.distill.sweep_max_turns` (default 4), reuses roki-mvp's stall window, and sends a single sweep continuation prompt that names the manifest path, the `distill.paths` snapshot, and the `distill.routes` snapshot.
  - Map `WorkerOutcome` values: `CleanExit` proceeds to validation; any other variant maps to `DistillFailure::SweepTurnFailed { outcome }`.
  - Observable completion: an integration test with a fake `claude` session asserts that exactly one continuation prompt is sent, that `sweep_max_turns` is honored, and that a non-clean exit produces `SweepTurnFailed`.
  - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5_
  - _Boundary: distill/dispatch_
  - _Depends: 1.1_

- [ ] 2.4 Wire the phase coordinator end-to-end
  - In `DistillPhase::enqueue`, before dispatching, read the manifest file once: if present and the `schema_version` is recognized, run validation directly and (on success) set status `Complete` without dispatching a sweep turn (idempotency per Req 9.1, 9.3); if the version is unrecognized, set `Failed { UnrecognizedSchemaVersion }` and do not overwrite the manifest (Req 9.4); otherwise call `SweepDispatcher::dispatch` and then `ManifestValidator::validate`.
  - Implement a per-`(repo, issue)` cancellation token consulted between `enqueue` and dispatch, used for the `TerminalSuccess`-revert race in Req 1.4.
  - Observable completion: unit tests cover (a) idempotent acceptance of a pre-existing valid manifest; (b) `UnrecognizedSchemaVersion` does not modify any file on disk; (c) cancellation before dispatch suppresses the dispatcher call.
  - _Requirements: 1.4, 9.1, 9.2, 9.3, 9.4, 9.5_
  - _Boundary: distill/phase, distill/dispatch, distill/validator_
  - _Depends: 2.1, 2.2, 2.3_

- [ ] 3. Integration: subscriber, orchestrator gating, logging

- [ ] 3.1 Implement `DistillSubscriber` and register it on the orchestrator
  - In `src/distill/subscriber.rs`, implement `TransitionSubscriber` for `DistillSubscriber`. On `event.next == TerminalSuccess`, call `phase.enqueue(repo, issue, correlation_id)`; on transitions out of `TerminalSuccess` while a sweep is pending, signal the cancellation token established in 2.4.
  - In `src/orchestrator/mod.rs`, construct one shared `Arc<DistillPhase>`, build the subscriber from it, and register the subscriber via roki-mvp's hook API at startup.
  - Observable completion: an integration test fires a synthetic `TerminalSuccess` transition through the bus and asserts the subscriber is invoked exactly once and that `DistillPhase::status` for the key reaches `Running` (or `Complete` in the idempotent case).
  - _Requirements: 1.1, 1.4, 1.5_
  - _Boundary: distill/subscriber, orchestrator_
  - _Depends: 2.1_

- [ ] 3.2 Gate workspace deletion on distill phase status
  - Modify the orchestrator's pre-cleanup path in `src/orchestrator/mod.rs` so that, before invoking `WorkspaceManager::remove`, it consults `DistillPhase::status(repo, issue)` and proceeds only when the status is `Complete`. When the status is `Failed`, the workspace is retained and a structured log event names the failure variant; when the status is `Running` or `Pending`, cleanup is held until the phase finishes.
  - Confirm there is no daemon-side `gh` CLI invocation, no GitHub API call, and no Linear write introduced by this gate (Req 1.3, 2.1).
  - Observable completion: an integration test drives `TerminalSuccess` with a fake successful sweep and asserts `WorkspaceManager::remove` is invoked; a second test drives `TerminalSuccess` with a sweep that fails validation and asserts the workspace is retained on disk.
  - _Requirements: 1.1, 1.2, 1.3, 12.1, 12.2_
  - _Boundary: orchestrator, distill/phase_
  - _Depends: 2.4, 3.1_

- [ ] 3.3 Add structured logging for every distill phase decision
  - Emit tracing events (with consistent `event_name` field) for: distill sweep activation, sweep turn start, sweep turn completion, manifest validation start, manifest validation outcome (including `schema_version`, per-disposition counts, and any path-safety failure detail), terminal cleanup gating decision, and cancellation due to a `TerminalSuccess` revert.
  - Reuse roki-mvp's redaction layer; never log artifact contents, only paths and structured fields.
  - Observable completion: an integration test runs a successful sweep and a path-unsafe sweep and asserts both produce the documented event sequence with the `(repo, issue, correlation_id)` context fields populated; a unit test confirms a configured secret never appears in any captured log event.
  - _Requirements: 13.1, 13.2, 13.3, 13.4, 13.5_
  - _Boundary: distill (cross-cutting)_
  - _Depends: 3.1, 3.2_

- [ ] 3.4 Expose `WORKFLOW.example.md` documentation for `distill.*`
  - Add a worked example block to `WORKFLOW.example.md` covering `distill.paths`, `distill.routes` (with at least one entry per disposition), `distill.sweep_max_turns`, and an optional `project_archive_roots` entry.
  - Observable completion: a smoke test loads `WORKFLOW.example.md` through the existing loader and asserts it parses without validation errors and that the parsed `distill` structure round-trips through the typed accessors.
  - _Requirements: 4.1, 4.2, 4.3_
  - _Boundary: workflow, distill/workflow_ext_
  - _Depends: 1.1_

- [ ] 4. Failure handling and operator recovery

- [ ] 4.1 (P) Implement failure-mode tests for sweep turn failures
  - Write integration tests that drive each `WorkerOutcome` failure variant (`NonCleanExit`, `Stalled`, `TurnBudgetExhausted`) through the dispatcher and assert each maps to `DistillFailure::SweepTurnFailed { outcome }` with the workspace retained.
  - Observable completion: each failure variant produces the expected status, the workspace directory is still present after the test, and a structured log event names the variant.
  - _Requirements: 3.4, 12.1, 12.2, 12.3_
  - _Boundary: distill/dispatch, distill/phase_
  - _Depends: 2.3, 3.2_

- [ ] 4.2 (P) Implement failure-mode tests for manifest schema and path safety
  - Write integration tests that pre-write malformed manifests into a workspace and synthesize a `TerminalSuccess` transition that exercises only the validation path (no dispatch needed when the manifest is already present): (a) missing `schema_version`; (b) extra unknown disposition; (c) `destination_path` outside the workspace; (d) an `archive` entry whose destination violates the archive scheme; (e) `schema_version: "v99"`.
  - Observable completion: each test asserts the corresponding `DistillFailure` variant and that the workspace remains on disk.
  - _Requirements: 8.2, 8.4, 9.4, 10.5, 11.4, 12.1_
  - _Boundary: distill/validator, distill/phase_
  - _Depends: 2.2, 2.4_

- [ ] 4.3 Document the operator-recovery flow
  - Add a short `## Distill phase failures` section to `SPEC.md` (and link from `WORKFLOW.example.md` if useful) describing how an operator clears a failed manifest, when a fresh sweep is permitted, and the idempotency guarantee that unchanged completed manifests remain valid across re-runs (Req 12.4, 9.1).
  - Observable completion: the documented procedure matches the implemented behavior in `DistillPhase::enqueue` exactly: clearing the manifest and re-firing `TerminalSuccess` produces a fresh sweep; leaving a valid manifest in place is honored on re-fire without dispatching.
  - _Requirements: 12.3, 12.4, 12.5_
  - _Depends: 2.4, 3.1_

- [ ] 5. End-to-end coverage

- [ ] 5.1 Happy-path E2E test
  - Extend the roki-mvp fake-Linear + fake-`claude` harness so the fake `claude` writes a valid v1 manifest with one entry per disposition during the sweep continuation. Drive `Discovered -> Active -> AwaitingReview -> TerminalSuccess`; assert the sweep dispatch happens, validation passes, the workspace is removed, and the documented log event sequence is observed in order.
  - Observable completion: the test runs deterministically and asserts every distill phase log event appears with the expected `(repo, issue, correlation_id)` fields.
  - _Requirements: 1.1, 1.2, 1.5, 3.1, 3.5, 8.1, 13.1_
  - _Boundary: integration_distill_phase test_
  - _Depends: 3.2, 3.3_

- [ ] 5.2 Idempotent re-run E2E test
  - Pre-seed a workspace with a valid v1 manifest, fire `TerminalSuccess`, and assert that no sweep continuation prompt is sent, validation passes against the existing manifest, and the workspace is removed.
  - Observable completion: the fake `claude` records zero sweep prompts received; the workspace deletion happens; the log event sequence reflects the idempotent path.
  - _Requirements: 9.1, 9.2, 9.3_
  - _Boundary: integration_distill_idempotency test_
  - _Depends: 2.4, 3.2_

- [ ] 5.3 Failure-path E2E test
  - Extend the harness with a fake `claude` that exits non-cleanly during the sweep turn. Drive `TerminalSuccess` and assert `DistillPhaseStatus::Failed { SweepTurnFailed }`, the workspace remains on disk, and the orchestrator does not retry automatically.
  - Run a second variant where the fake `claude` writes a manifest with a path-traversal `destination_path`, and assert `DistillPhaseStatus::Failed { PathUnsafe }`, workspace retained, no out-of-bounds file written.
  - Observable completion: both variants leave the workspace on disk, produce the documented `Failed` variant, and emit a structured log naming the offending path or the `WorkerOutcome`.
  - _Requirements: 11.1, 11.2, 11.4, 12.1, 12.3_
  - _Boundary: integration_distill_failure test_
  - _Depends: 4.1, 4.2_
