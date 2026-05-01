# Implementation Plan

- [ ] 1. Foundation: project scaffolding, configuration, and logging

- [x] 1.1 Initialize the Cargo workspace and the roki-daemon crate
  - Create the root `Cargo.toml` as a Cargo workspace with `[workspace]` and `members = ["crates/roki-daemon"]`. Reserve the workspace layout so downstream specs can append `crates/roki-tui` and `crates/roki-api-types` as additive members without restructuring.
  - Create the `crates/roki-daemon/` member crate with `edition = "2024"`, binary name `roki`, and core runtime dependencies (tokio, clap, tracing, tracing-subscriber, serde, serde_json, thiserror, anyhow).
  - Create `crates/roki-daemon/src/main.rs` that parses CLI arguments and bootstraps a tokio multi-threaded runtime.
  - Add a placeholder `roki run` subcommand that initializes tracing and exits cleanly.
  - Observable completion: `cargo run --bin roki -- --help` prints the documented subcommands; `cargo run --bin roki -- run` initializes the runtime, emits a startup log line, and exits without error; `cargo metadata` confirms a single workspace member at `crates/roki-daemon`.
  - _Requirements: 1.1, 1.7_
  - _Boundary: workspace root and crates/roki-daemon_

- [x] 1.2 Build the layered configuration loader with secret handling
  - Define the configuration struct hierarchy (root config plus per-repo entries), including workspace root, Linear token source, polling cadence cap, max concurrent workers, and permission strategy selection.
  - Implement loading from a config file plus environment overrides, with explicit refusal when the Linear token is absent.
  - Validate configuration at startup and return a structured error that names the offending field on failure.
  - Observable completion: a unit test loads a valid example config and a malformed one; the malformed case returns an error whose message identifies the failing field.
  - _Requirements: 1.2, 2.1, 2.3, 9.5_

- [x] 1.3 Implement structured tracing and secret-redaction layer
  - Initialize `tracing-subscriber` with a configurable log level and destination (stdout, file, or both).
  - Add a redaction layer that scrubs the Linear API token and any operator-declared secret strings from every emitted event.
  - Standardize `(repo, issue, correlation_id)` context fields on every event that has them.
  - Observable completion: a unit test asserts the configured token never appears in captured log output even when intentionally placed in a field value.
  - _Requirements: 1.5, 12.1, 12.2, 12.3, 12.4_

- [x] 1.4 Implement bounded shutdown handling
  - Wire `SIGINT` and `SIGTERM` handling to a single `ShutdownSignal` propagated through the orchestrator and adapters.
  - Stop accepting new work on shutdown, signal active workers, and wait per worker up to a bounded shutdown window before forcing exit.
  - Observable completion: an integration test starts the daemon with a fake long-running worker, sends a shutdown signal, and asserts the daemon exits cleanly within the documented window.
  - _Requirements: 1.4_

- [x] 1.5 Implement the multi-repo router and unhealthy-repo handling
  - Build the deterministic precedence rule for routing a Linear issue to exactly one configured repository when scopes overlap, and log every routing decision.
  - On startup, verify each repository path is a Git working tree; mark missing or non-Git paths as unhealthy and refuse to schedule work for them while continuing to serve the remaining repositories.
  - Observable completion: a unit test routes the same issue against two overlapping configured scopes and asserts a single `(repo, issue)` key is produced, plus a log event names the precedence decision.
  - _Requirements: 2.6 (NOTE: this task implemented the pre-7.1 deterministic precedence rule and per-repo health classifier — both are removed by task 7.1; the original requirement IDs `2.2 (overlapping precedence)` and `2.3 (unhealthy repo)` no longer exist in the synced requirements.md)_

- [ ] 2. Core: domain types, traits, and per-component implementations

- [x] 2.1 (P) Define orchestrator state, transitions, and events
  - Implement the `WorkerState` enum, including the `Cleaning` interim state, and the transition table (legal transitions, vetoable subset).
  - Define `TransitionEvent`, `TransitionTrigger`, and `VetoDecision` types.
  - Encode the vetoable subset: `Queued -> Active`, `AwaitingReview -> TerminalSuccess`, `TerminalSuccess -> Cleaning`. Workspace removal happens only on `Cleaning -> [*]`.
  - Implement deterministic state-transition functions and unit-test the legal transition matrix.
  - Observable completion: a unit test exercises every documented transition and asserts the resulting `TransitionEvent` shape, including correct `previous`/`next` and the vetoable flag for all three vetoable transitions.
  - _Requirements: 8.1, 8.2, 13.2_
  - _Boundary: orchestrator/state_

- [x] 2.1a (P) Publish the `OrchestratorRead` and `PreCleanupHook` extension traits
  - Define the `OrchestratorRead` trait (`snapshot()` returning a `SnapshotResponse`, `issue(repo, issue)` returning `Option<IssueState>`) and ensure it grants no state-mutation rights.
  - Define the `PreCleanupHook` trait and the orchestrator's `register_pre_cleanup_hook` registration API; pre-cleanup hooks are dispatched as the vetoable observers of `TerminalSuccess -> Cleaning`.
  - Document both traits in the design surface so downstream specs (roki-observability, roki-distill-postmerge) can depend on them without forking the core.
  - Observable completion: a unit test asserts that `OrchestratorRead::snapshot` returns the expected projection for a seeded set of `(repo, issue)` keys and that the trait API exposes no setter; a second unit test registers a pre-cleanup hook that returns `Deny` and asserts the orchestrator records the veto decision without invoking the workspace removal path.
  - _Depends: 2.1_
  - _Requirements: 13.1, 13.2_
  - _Boundary: orchestrator/read, orchestrator/hooks_

- [x] 2.2 (P) Implement the workspace manager and path-safety invariants
  - Implement identifier sanitization for repo and issue components, derive the workspace path under the configured workspace root, and refuse paths that escape the root.
  - Provide `ensure`, `remove`, and `list_existing` operations with idempotent semantics; surface filesystem errors with the offending path.
  - Observable completion: a unit test shows that crafted issue identifiers (path traversal, absolute paths, identifiers colliding after sanitization) are rejected, and that valid identifiers produce a path that canonicalizes inside the workspace root.
  - _Requirements: 4.1, 4.3, 4.7_
  - _Boundary: workspace_

- [x] 2.3 (P) Implement the WORKFLOW.md loader, schema, and hot reload
  - Parse YAML front matter, render the Liquid body, and validate the resulting structure against the published JSON-Schema.
  - Type `WorkflowPolicy.extension` as `serde_json::Value` so downstream specs can `serde_json::from_value` their reserved sub-slice into their own typed struct.
  - Reserve and round-trip (without interpretation) all four canonical sub-namespaces: `extension.gates.spec.*`, `extension.gates.review.*`, `extension.server.*`, `extension.distill.*`.
  - Implement filesystem watching with debounce and a last-known-good fallback that preserves the prior valid policy on failed reload.
  - Observable completion: an integration test feeds a valid `WORKFLOW.md` containing keys under all four reserved namespaces and asserts they round-trip through `WorkflowPolicy.extension` byte-for-byte; mutating the file to be invalid causes the loader to retain the prior valid policy in memory and emit a structured validation-failure log event identifying the bad key path.
  - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5, 13.5_
  - _Boundary: workflow_

- [x] 2.4 (P) Implement the agent tool registry and the `linear_graphql` proxy
  - Define the `Tool` and `Registry` traits, their stable name and JSON-Schema input/output convention, and the catalog format passed to the engine adapter at worker launch.
  - Implement the `linear_graphql` proxy: accept exactly one GraphQL operation per call, forward to Linear with the daemon-owned token, share rate-limit state with the tracker client, and apply credential redaction to errors.
  - Observable completion: a unit test sends a multi-operation GraphQL document and receives a `MULTIPLE_OPERATIONS` error; another test injects the API token into a failure path and asserts no error field returned to the caller contains it.
  - _Requirements: 7.1, 7.2, 7.3, 7.8, 7.9_
  - _Boundary: tools_

- [x] 2.5 (P) Implement the Linear tracker adapter (polling)
  - Implement the GraphQL client (reqwest) for the documented active-issue queries, the polling loop with the configurable cadence cap (<= 5 min per scope), and 429 exponential backoff with logging.
  - Normalize responses into the `NormalizedIssue` shape.
  - Observable completion: an integration test against a stub Linear server records that no scope is polled more than once per five minutes under steady load and that a 429 response defers the next request to the same endpoint.
  - _Requirements: 3.3, 3.4, 3.5, 3.6_
  - _Boundary: tracker/linear_

- [x] 2.5a (P) Publish the `TrackerRefresh` nudge trait
  - Define the `TrackerRefresh` trait with a single `nudge()` method returning a `RefreshAccepted` shape and grant no read or mutation surface beyond requesting that the next poll be scheduled sooner.
  - Implement the trait on the tracker adapter so that an out-of-cycle nudge advances only the next-poll deadline, never bypassing the documented cadence cap or the 429 backoff state.
  - Observable completion: a unit test asserts that a nudge during an active 429 backoff window does not shorten the backoff and that a nudge during a normal idle window advances the next-poll deadline; the response shape names the window within which polling will occur.
  - _Depends: 2.5_
  - _Requirements: 13.3_
  - _Boundary: tracker/linear_

- [x] 2.6 (P) Implement the Linear webhook receiver
  - Stand up the axum endpoint at the configured path, verify the Linear signature header before any normalization, and decode the payload into the same `NormalizedIssue` shape used by polling.
  - Reject unsigned, mismatched, or malformed payloads with the documented status codes and avoid echoing payload content.
  - Observable completion: an integration test posts a correctly signed webhook payload and observes a normalized issue event reach the tracker sink; an incorrectly signed payload is rejected with 401 and no normalization occurs.
  - _Requirements: 3.1, 3.2, 3.5_
  - _Boundary: tracker/webhook_

- [x] 2.7 (P) Implement the Claude Code stream-JSON parser
  - Build a tolerant newline-delimited JSON parser that maps the documented stream-json shapes to the typed `EngineLifecycleEvent` taxonomy and skips a single bad line without aborting the worker stream.
  - Treat unknown event types as `AgentMessage` so the supervisor loop continues to record progress timestamps.
  - Observable completion: a unit test feeds a recorded stream containing one bad JSON line plus a representative event sequence and asserts the parser emits all valid events in order while logging a single parse-error event for the bad line.
  - _Requirements: 5.2_
  - _Boundary: engine/stream_

- [x] 2.8 (P) Implement the engine policy controller (turn budget, stall, backoff, retry)
  - Implement the configurable per-worker turn budget so that no further continuation prompt is sent once exhausted.
  - Implement event-inactivity stall detection over a configurable window with a typed `Stalled` outcome.
  - Implement exponential backoff between worker invocations bounded between 10s and 5min, and the one-second continuation retry on clean exit.
  - Observable completion: a unit test simulates each of clean exit, non-clean exit, turn-budget exhaustion, and stall, and asserts the resulting `WorkerOutcome` plus the next-launch delay falls in the documented bounds.
  - _Requirements: 5.3, 5.4, 5.5, 5.6_
  - _Boundary: engine/policy_

- [x] 2.9 (P) Implement the permission strategy resolver
  - Resolve the effective permission strategy per worker by combining operator selection (`--settings` allowlist or `--dangerously-skip-permissions`) with any per-repo override declared in `WORKFLOW.md`.
  - Apply `workspace-write` and reject-elicitations as defaults, refuse to launch a worker when neither strategy is configured, and emit a per-launch log entry when the dangerous fallback is used.
  - Observable completion: a unit test exercises the four matrix cells (allowlist on or off; dangerous on or off; per-repo override present or absent) and asserts the resolved strategy plus the launch-time log shape for the dangerous case.
  - _Requirements: 9.1, 9.2, 9.3, 9.4, 9.5_
  - _Boundary: permissions_

- [x] 2.10 Implement the engine adapter subprocess supervisor
  - Spawn `claude --print --output-format stream-json` with the issue workspace as cwd, kiro-skill-discovery flags applied (no `--bare`), the resolved permission strategy passed through, and `kill_on_drop` set on the process handle.
  - Wire the stream-JSON parser, the engine policy controller, and the tool catalog into a single supervised lifecycle that emits one terminal `Exited` event for every launch.
  - Add the `additional_context: Option<serde_json::Value>` field to `WorkerContext`. When `Some(value)`, the adapter shall forward the value verbatim into the agent's session through a documented prelude envelope (a stable JSON block prepended to the session prompt under a stable key). The MVP shall not interpret the contents.
  - Observable completion: an integration test using a fake `claude` binary drives clean-exit, non-clean-exit, and stall scenarios and asserts the orchestrator receives the corresponding lifecycle and outcome events; an additional unit test passes a non-`None` `additional_context` and asserts the value appears verbatim in the prelude envelope captured by the fake binary.
  - _Requirements: 5.1, 5.2, 5.7, 13.4_
  - _Depends: 2.7, 2.8, 2.9, 2.4, 2.3_
  - _Boundary: engine/claude_

- [ ] 3. Integration: orchestrator wiring, recovery, and event bus

- [x] 3.1 Implement the transition event bus and subscription hooks
  - Implement a bounded broadcast channel for non-vetoable transitions plus an explicit per-subscriber await path for vetoable transitions where a `Deny` decision blocks the transition.
  - Isolate subscriber failures so a panicking or erroring subscriber cannot stall others; log the per-subscriber error counter and any drop-counter increments.
  - Observable completion: an integration test registers two subscribers, one of which errors on every event; transitions still reach the healthy subscriber and the failure is logged with the subscriber identifier.
  - _Depends: 2.1_
  - _Requirements: 8.2, 8.3, 8.4_

- [x] 3.2 Implement the orchestrator core and per-issue worker actor
  - Build the orchestrator runtime that owns one tokio task per `(repo, issue)`, drives transitions only from the declared sources (tracker, engine, recovery, shutdown), and routes lifecycle events between adapters.
  - Apply vetoable-transition checks against subscribers before committing `Queued -> Active`, `AwaitingReview -> TerminalSuccess`, and `TerminalSuccess -> Cleaning`; treat subscriber or pre-cleanup-hook error on a vetoable transition as `Deny` (fail closed).
  - Implement the `OrchestratorRead` trait against the live state map so additive consumers can read state without mutation rights.
  - Observable completion: an integration test brings up the full daemon with stubs for tracker and engine, drives an issue from `Discovered` to `Cleaning`, and asserts the published transition sequence (including `TerminalSuccess -> Cleaning`) and subscriber dispatch order; a second test reads `OrchestratorRead::snapshot` mid-run and asserts the projection matches the actual state.
  - _Depends: 1.5, 2.1, 2.1a, 2.2, 2.3, 2.10, 3.1_
  - _Requirements: 1.1, 8.1, 8.2, 8.3, 13.1_

- [x] 3.3 Implement restart recovery via Linear plus filesystem reconciliation
  - On daemon start, list the workspace root, match each existing workspace to a `(repo, issue)` key, and re-fetch the corresponding Linear state before resuming work.
  - Apply the documented per-case reconciliation: workspace plus active issue resumes Active; workspace without active issue is marked orphaned and logged without deletion; active issue without workspace produces a fresh workspace and enters Queued; absent on both sides is a no-op.
  - Confirm the daemon writes no per-issue runtime state to disk except workspace contents the agent itself produces and the structured logs the daemon emits.
  - Observable completion: an integration test pre-seeds two workspaces and a Linear stub with mixed states, starts the daemon, and asserts each `(repo, issue)` lands in the documented post-recovery state.
  - _Depends: 2.2, 2.5, 3.2_
  - _Requirements: 8.5, 10.1, 10.2, 10.3, 10.4, 10.5_

- [x] 3.4 Wire the tool registry into the engine adapter at worker launch
  - Compose the tool catalog (including the built-in `linear_graphql` proxy) and pass it to each spawned worker subprocess at launch.
  - Forward tool calls from the agent through the registry, applying redaction on errors before they leave the daemon.
  - Observable completion: an integration test with a fake `claude` binary issues a `linear_graphql` call against a stub Linear server and asserts the response is returned to the worker without the API token appearing in any tool input, output, or error.
  - _Depends: 2.4, 2.10_
  - _Requirements: 7.1, 7.2, 7.8_

- [x] 3.5 Wire the workspace lifecycle into orchestrator transitions
  - Create the workspace on the first transition into `Active`, and set the worker subprocess cwd to that workspace.
  - On transition into `TerminalSuccess`, dispatch the registered pre-cleanup hooks against the vetoable `TerminalSuccess -> Cleaning` transition; on `Allow`, advance the state machine to `Cleaning` and remove the workspace after the worker exits; on `Deny`, block workspace removal and log the veto decision (the workspace is retained pending operator intervention, treated like `TerminalFailure` for retention purposes).
  - Retain the workspace on `TerminalFailure` for inspection; on workspace creation or deletion errors, mark the worker failed, log the offending path, and refuse to start additional work for that `(repo, issue)` until the operator intervenes.
  - Observable completion: an integration test asserts that a happy-path issue with no pre-cleanup hooks registered produces a workspace that is created on activation, transitions through `TerminalSuccess -> Cleaning`, and is deleted; a second test registers a pre-cleanup hook that returns `Deny` and asserts the workspace is retained and the veto event is logged; a third test forces a workspace error and asserts the issue lands in `TerminalFailure` with the workspace retained.
  - _Depends: 2.1a, 2.2, 3.2_
  - _Requirements: 4.1, 4.5, 4.6, 4.7, 13.2_

- [x] 3.6 Connect the tracker adapter to the orchestrator
  - Bridge `NormalizedIssue` events from both polling and webhook paths into the orchestrator's tracker-event sink, ensuring duplicates are idempotent on `(repo, issue, target_state)`.
  - Refuse any code path that performs Linear writes from inside the daemon process; all writes must originate from the agent through `linear_graphql`.
  - Observable completion: an integration test delivers the same logical issue update via both webhook and polling within a single tick and asserts only one observable transition is produced.
  - _Depends: 2.5, 2.6, 3.2_
  - _Requirements: 3.1, 3.6_

- [x] 3.7 Implement the retry-budget Backoff loop in the worker actor
  - _Boundary:_ `crates/roki-daemon/src/engine/policy.rs`, `crates/roki-daemon/src/orchestrator/core.rs`, `crates/roki-daemon/src/workflow/schema.rs` (and the matching JSON-Schema asset), `SPEC.md` §3.2 + §9.5, `design.md` retry-budget paragraph (≈line 761). Tests under `crates/roki-daemon/src/engine/policy.rs` (`#[cfg(test)]`) and `crates/roki-daemon/tests/orchestrator_core.rs`.
  - Add `EnginePolicy.max_attempts: u32` (default 3, JSON-Schema range 1..=10; `1` means one shot / no retry) and `EnginePolicy.backoff_floor: Duration` (default = the existing `BACKOFF_FLOOR` constant). Update `EnginePolicy::compute_backoff` to read the field rather than the constant; the constant becomes the documented default.
  - Add an additive `engine.max_attempts` key to the `WORKFLOW.md` front-matter schema. Wire it through to `EnginePolicy` at policy resolution.
  - Extend `ActorRecord` with `consecutive_failures: u32`. In `WorkerActor::try_promote_to_active`, replace the current "all failures → TerminalFailure" arm with: `CleanExit -> Active -> AwaitingReview` (unchanged); `NonCleanExit & consecutive_failures + 1 < max_attempts -> Active -> Backoff -> sleep(EnginePolicy::next_launch_delay) -> Backoff -> Active` (re-launch via the existing engine path; increment the counter); `NonCleanExit & consecutive_failures + 1 >= max_attempts -> Active -> TerminalFailure`; `TurnBudgetExhausted | Stalled -> Active -> TerminalFailure` (no retry — agent-authored failures repeat under the same prompt).
  - Workspace is retained across the Backoff loop (no delete/recreate). Prelude / `additional_context` is re-emitted unchanged on each launch — failure-history accumulation is a downstream-spec concern, out of scope here.
  - All retry-arc transitions (`Active → Backoff`, `Backoff → Active`, retry-exhausted `Active → TerminalFailure`) are non-vetoable, matching the existing vetoable subset.
  - Per arc, emit one `transition` `tracing` event with `attempt`, `delay_ms`, `outcome_reason`. On retry-exhausted `Active → TerminalFailure` log `final_attempt` and `last_outcome_reason`.
  - Update `SPEC.md` §3.2 schema table (add the `max_attempts` row) and §9.5 retry semantics paragraph (state explicitly that only `NonCleanExit` retries) in the same change set, per §16 contract-change rule. Update `design.md` line ≈761 to match.
  - Observable completion: (a) unit test in `engine::policy` rejects `max_attempts = 0` and accepts `1..=10`; (b) integration test in `orchestrator_core.rs` with a stub `EngineLauncher` producing a configurable failure sequence asserts the exact `Active → Backoff → Active → … → TerminalFailure` transition trace for `NonCleanExit` and the immediate `Active → TerminalFailure` for `Stalled` / `TurnBudgetExhausted`, completes deterministically in well under one second using a sub-second `backoff_floor`, and confirms the workspace path on disk is retained throughout.
  - _Depends: 3.2, 3.5_
  - _Requirements: 4.6, 5.6, 8.1_
  - _Design: `.kiro/specs/roki-mvp/design-retry-policy.md`_

- [ ] 4. Validation: end-to-end paths, language-agnostic SPEC.md

- [x] 4.1 Author the language-agnostic `SPEC.md` at the repo root
  - Document the daemon contract, the `WORKFLOW.md` schema with all four reserved extension namespaces (`extension.gates.spec.*`, `extension.gates.review.*`, `extension.server.*`, `extension.distill.*`), the per-issue state machine and full vetoable-transition list (including `TerminalSuccess -> Cleaning` as the pre-cleanup hook), the agent tool registry contract and `linear_graphql` semantics, the workspace path layout and sanitization rules, and the lifecycle event taxonomy.
  - Document the published cross-spec extension surface: `OrchestratorRead`, `TrackerRefresh`, the pre-cleanup hook, `WorkerContext.additional_context` and the prelude-forwarding contract, and the reserved `WorkflowPolicy.extension` sub-namespaces.
  - Disambiguate the two `required_status` fields used by gate extensions: `extension.gates.spec.required_status` is a Linear issue state name to gate against (consumed by roki-spec-gate); `extension.gates.review.required_status` is a `review.md` frontmatter status value to gate against (consumed by roki-review-gate). These share a name but are different semantic fields and SPEC.md must call out the distinction.
  - State the rule that a contract change here must accompany the corresponding Rust change in the same change set, and enumerate the extension points downstream specs depend on.
  - Observable completion: `SPEC.md` exists at the repository root and a manual cross-check confirms each Requirement 11 and Requirement 13 acceptance criterion has a named section addressing it; a search for both `extension.gates.spec.required_status` and `extension.gates.review.required_status` in `SPEC.md` returns the disambiguation paragraph.
  - _Requirements: 11.1, 11.2, 11.3, 11.4, 13.1, 13.2, 13.3, 13.4, 13.5_

- [x] 4.2 End-to-end happy-path test with fake Linear and fake `claude`
  - Stand up a fake Linear server and a fake `claude` binary that emits a scripted stream-json sequence covering `Discovered -> Queued -> Active -> AwaitingReview -> TerminalSuccess -> Cleaning` for one `(repo, issue)`.
  - Assert workspace creation on activation, transition events emitted in the correct order with correlation ids set (including the `TerminalSuccess -> Cleaning` transition), and workspace deletion after `Cleaning -> [*]`.
  - Observable completion: the test passes deterministically and produces the expected transition log sequence with no duplicate transitions for the same key.
  - _Depends: 3.2, 3.3, 3.5, 3.6_
  - _Requirements: 1.1, 4.5, 8.2, 10.4, 13.2_

- [x] 4.3 End-to-end failure-path test for retry budget exhaustion
  - Drive the same harness so that the fake `claude` binary repeatedly exits non-cleanly until the configured retry budget is exhausted; assert the worker lands in `TerminalFailure` with the workspace retained and the failure logged.
  - Observable completion: the test passes deterministically and the post-run filesystem layout still contains the workspace directory while the orchestrator state for that key is `TerminalFailure`.
  - _Depends: 3.7, 4.2_
  - _Requirements: 4.6, 5.6, 8.1_

- [x] 4.4 End-to-end multi-repo and routing test
  - Configure two repositories with overlapping Linear scopes, emit the same logical issue into both scopes, and assert the deterministic precedence rule selects exactly one repository while the other ignores the issue, with both logged.
  - Observable completion: the test passes deterministically and the logs show one `routed` event per issue with the precedence decision named.
  - _Depends: 1.5, 3.6_
  - _Requirements: 2.6 (NOTE: this test exercises the pre-7.1 deterministic precedence rule from `routing::route_issue`; both that function and the underlying overlapping-scope behavior are removed by task 7.1. The original requirement IDs `2.2 (overlapping precedence)` and `2.4 ((repo, issue) keying)` either no longer exist or have been collapsed to per-issue keying in the synced requirements.md.)_

- [x] 4.5 End-to-end vetoable-transition test
  - Register a stub subscriber that denies `Queued -> Active` for a specific issue identifier; assert that issue stays `Queued` and the daemon emits the documented veto log event while a second issue progresses to `Active` normally.
  - Observable completion: the test passes deterministically and the metrics for vetoed transitions are visible in logs alongside the normal progression for the unaffected issue.
  - _Depends: 3.1, 3.2_
  - _Requirements: 8.3, 8.4_

- [ ] 4.6* Optional: stream-JSON parser regression coverage
  - Capture additional recorded stream-json fixtures from real Claude Code sessions and lock the parser's mapping to the lifecycle event taxonomy.
  - Observable completion: the parser keeps the same outcome on the captured fixture set across changes; new fixtures can be added with a single helper.
  - _Requirements: 5.2_

- [ ] 7. Agent-driven repo selection: collapse multi-repo routing into the agent

_Task 7.1 was split into 7.1a–7.1f after the first implementer dispatch BLOCKED on size (~16K production lines + ~4K tests + SPEC/design rewrites in one reviewer-gated unit). The 11 locked decisions in `design-agent-driven-repo-selection.md` carry through unchanged; only the envelope is split. Sub-tasks dispatched sequentially with normal subagent-per-task discipline._

- [x] 7.1a Drop `LinearScope` and the `routing.rs` module; shrink `RepoConfig` config schema
  - _Boundary:_ `crates/roki-daemon/src/config/{mod.rs,repos.rs}`, `crates/roki-daemon/src/routing.rs` (DELETED), `crates/roki-daemon/src/lib.rs` (drop `mod routing`), and any other production source whose ONLY change is removing references to deleted/renamed types. Tests: mechanical updates in any `tests/*.rs` that constructs `RepoConfig` literals or imports `routing::*`. Per §16: SPEC.md §2.2 update + .kiro/specs/roki-mvp/design.md update for the schema delta.
  - **Schema delta (breaking, additive where possible)**:
    - REMOVE `RepoConfig.id`, `RepoConfig.scope`, `RepoConfig.webhook_secret_env`, `RepoConfig.webhook_secret`, `RepoConfig.workflow_path`. After this sub-task `RepoConfig` is `{ repo: String }` only.
    - REMOVE the `LinearScope` enum and the `routing.rs` module entirely. `routing.rs` has zero production callers post-6.1; deletion is mechanical.
    - ADD `[linear]` config block: `token_env: Option<String>` (defaults to `"LINEAR_API_TOKEN"`), `webhook_secret_env: String` (required, single workspace-level secret), `endpoint: Option<String>` (test-only override; production omits).
    - ADD `[workflow]` config block: `path: PathBuf` (required, single workspace-level policy file).
    - REJECT duplicate `[[repos]]` entries with the same `repo` value at config load (hard refusal naming the offending entry).
  - **Compile cascade tolerance**: removing `RepoConfig.scope` and the `LinearScope` enum will break direct consumers (the per-repo `LinearTracker`, the per-repo webhook routes, the `route_issue` call site, etc.). 7.1a's job is to land the schema and delete `routing.rs`; immediate consumers in `tracker/linear.rs`, `tracker/webhook.rs`, `runtime.rs` will be updated mechanically here ONLY to the extent of removing the deleted/renamed references — no architectural rework. Anything that genuinely needs to be rewritten (single tracker, single webhook route, agent tool, etc.) lands in 7.1b–f. The build must still compile at the end of 7.1a; failing tests are acceptable if they're pinned to behavior that the later sub-tasks own.
  - _Note for the implementer_: this is the "land config + drop routing + minimal compile-fixes" sub-task. Resist the urge to also reshape the tracker or webhook here — that's 7.1b/c.
  - Observable completion: (a) `cargo build --workspace` clean; (b) `cargo fmt --all -- --check` clean; (c) `cargo clippy --workspace -- -D warnings` clean; (d) `crates/roki-daemon/src/routing.rs` is gone from the file tree; (e) `RepoConfig` shrinks to `{ repo: String }`; (f) `[linear]` and `[workflow]` config blocks parse from a fixture TOML; (g) duplicate `[[repos]]` entries error at load with the offending entry named; (h) SPEC.md §2.2 and .kiro/specs/roki-mvp/design.md reflect the schema delta. Tests pinned to behaviors owned by 7.1b–f may temporarily fail; document which in the status report.
  - _Depends: 6.1_
  - _Requirements: 2.1, 2.4_
  - _Design: `.kiro/specs/roki-mvp/design-agent-driven-repo-selection.md` (decisions 6, 7 set the [linear] / [workflow] block shape)_

- [x] 7.1b Collapse the state-machine key from `(repo, issue)` to `(issue,)`
  - _Boundary:_ `crates/roki-daemon/src/orchestrator/{state.rs,core.rs,events.rs,read.rs,hooks.rs,tracker_bridge.rs}` and the tests under `tests/orchestrator_*.rs` + `tests/e2e_vetoable_transition.rs` + `tests/e2e_multi_repo_routing.rs`. The biggest sub-task in 7.1 (~1500 LoC).
  - **State-machine impact**:
    - `(repo, issue)` collapses to `(issue,)`. `RepoId` stays as a type for `WorktreeRegistry` keying (added in 7.1d) but is no longer in the state-machine key.
    - Update `ActorRecord` keying, `TrackerBridge` dedup keys (`(repo, issue, target_state)` → `(issue, target_state)`), `TransitionEvent.repo` becomes `Option<RepoId>` populated post-tool-call (or removed entirely if the field has no observable consumers — implementer's call, document in the SPEC.md update that lands here or in 7.1f).
    - `Queued → Active` no longer pre-creates a worktree. The actor's "ensure workspace" call is replaced with a NoOp shim until 7.1d wires `SessionManager`. Document the shim in code with a `// TODO(7.1d):` comment naming the sub-task.
    - `Cleaning → [*]` is similarly stubbed: the existing `wt.remove`-via-`WorkspaceManager` call is replaced with a NoOp shim until 7.1d wires `WorktreeRegistry`.
  - **`tests/e2e_multi_repo_routing.rs`**: implementer's choice — delete entirely (it pins `route_issue` semantics that are gone) OR repurpose as a placeholder for 7.1d's cross-repo test (mark `#[ignore]` until 7.1d wires the new agent tool).
  - Observable completion: (a) `cargo build --workspace` clean; (b) `cargo test --workspace` clean for everything that doesn't depend on the workspace shim; (c) the orchestrator integration tests reflect the `IssueId`-only key; (d) `cargo clippy` + `cargo fmt` clean.
  - _Depends: 7.1a_
  - _Requirements: 2.1, 8.2, 10.1_
  - _Design: same as 7.1_

- [x] 7.1c Single `LinearTracker` + single webhook route + single HMAC secret
  - _Boundary:_ `crates/roki-daemon/src/tracker/{linear.rs,webhook.rs,model.rs}` and their tests (`tests/tracker_linear.rs`, `tests/tracker_webhook.rs`, `tests/tracker_bridge.rs`). Bootstrap glue lands in 7.1f.
  - Collapse per-repo trackers to one. The single tracker polls the entire Linear workspace using the API token; no `scope` filter. Honor the existing global `polling_cadence` and 5-min cap.
  - Single webhook route: `POST /linear/webhook` (no per-repo path segment). HMAC-verify against `[linear].webhook_secret_env` (single secret).
  - Webhook handler decodes → `NormalizedIssue` (no repo association) → forward to orchestrator's `tracker_inbox` keyed by `IssueId`.
  - Tests update for single-route dispatch; assert per-issue dedup at the bridge.
  - Observable completion: tracker tests pass; new test asserts the single webhook secret rejects mismatched HMACs and accepts correct ones; new test asserts polling produces one event per Linear issue regardless of how many `[[repos]]` entries are configured.
  - _Depends: 7.1b_
  - _Requirements: 3.1, 3.2_
  - _Design: same as 7.1_

- [x] 7.1d `SessionManager` + `WorktreeRegistry` + `roki_open_worktree` agent tool
  - _Boundary:_ rewrite `crates/roki-daemon/src/workspace/` as `session/` + `worktrees/` modules (drop the `Workspace` trait); add `crates/roki-daemon/src/tools/roki_open_worktree.rs` and update `tools/mod.rs` re-exports; wire the new modules into `orchestrator/core.rs` (replacing the 7.1b NoOp shims). New tests: `tests/agent_tool_open_worktree.rs` (allowlist rejection, idempotency, error taxonomy), `tests/orchestrator_session.rs` (session-tempdir lifecycle), and a new cross-repo e2e test where one worker opens worktrees in two configured repos.
  - **Session tempdir**: `~/Library/Caches/roki/sessions/<issue>` on macOS, `~/.cache/roki/sessions/<issue>` on Linux. Add the `dirs` crate to `Cargo.toml` if not already present. `SessionManager::create_session(issue)` is idempotent (calling twice for the same issue returns the same path).
  - **WorktreeRegistry**: `Arc<Mutex<HashMap<IssueId, Vec<(RepoId, BranchName, PathBuf)>>>>` (or equivalent shape). Tracks every worktree the agent opened per worker. The orchestrator's `WorkerActor` carries a registry handle; the agent tool resolves it via shared state.
  - **Agent tool `roki_open_worktree`**:
    - Description (verbatim, render in the agent's tool surface): "Open a git worktree for the current Linear issue in one of the configured repos. The daemon resolves the repo via ghq, creates a worktree branch named after the issue id via wt, and returns the absolute path. Idempotent — calling twice with the same repo returns the same path. Use this once per repo you intend to modify; cross-repo tickets call this multiple times."
    - Input: `{ repo: string }` only. Strict allowlist (must match a configured `[[repos]]` entry; reject otherwise).
    - Output: `{ path: string, repo: string, branch: string }` where `branch == issue.id`.
    - Errors (typed): `RepoNotInAllowlist { repo, allowed: [string] }`, `GhqResolutionFailed { repo, reason }`, `WorktreeCreationFailed { repo, branch, reason }`.
    - Handler flow: validate allowlist → check `WorktreeRegistry` for `(worker_id, repo)` (return existing path if present) → `ghq.ensure_cloned(repo)` → `wt.switch_create(repo_path, issue.as_str())` → register `(worker_id, repo, branch, worktree_path)` → return path.
  - **Orchestrator wiring**:
    - `Queued → Active` calls `SessionManager::create_session(issue)` and uses the resulting tempdir as the worker's CWD. (Replaces the 7.1b NoOp shim.)
    - `Cleaning → [*]` walks `WorktreeRegistry` for the worker, calls `wt.remove` on each worktree (one-by-one, log per-arc, subject to existing pre-cleanup hooks), then removes the session tempdir. (Replaces the 7.1b NoOp shim.)
    - `TerminalFailure` retains all worktrees AND the session tempdir.
  - Observable completion: (a) all tests across `cargo test --workspace`, `cargo clippy`, `cargo fmt` clean; (b) new cross-repo test passes; (c) new allowlist-rejection test passes; (d) idempotency test passes; (e) Cleaning correctly removes every registered worktree subject to pre-cleanup hooks.
  - _Depends: 7.1b, 7.1c_
  - _Requirements: 4.1, 4.2, 4.5, 7.1, 7.2_
  - _Design: same as 7.1_

- [ ] 7.1e Restart recovery rewrite (folds task 5.2)
  - _Boundary:_ `crates/roki-daemon/src/orchestrator/recovery.rs` rewrite; new production `RecoveryLinearReader` impl backed by the (now single) `LinearTracker`; `crates/roki-daemon/src/runtime.rs` swap of `Orchestrator::new` for `Orchestrator::with_recovery`. New integration test `tests/orchestrator_restart_recovery.rs` seeds both session tempdirs and pre-existing worktrees per configured repo.
  - **Five-cell decision matrix** (expanded from the existing four-cell):
    - `ResumeActive` — issue active in Linear, session tempdir + worktree(s) on disk → resume the worker
    - `OrphanedSession` — session tempdir but no Linear active state and no worktree → schedule cleanup
    - `OrphanedWorktree` — worktree exists but no session tempdir → schedule cleanup (worktree retained for inspection per design decision #6 if Linear state is `failed`)
    - `FreshQueued` — Linear issue active, nothing on disk → spawn fresh worker
    - `NoOp` — Linear issue terminal, nothing on disk → ignore
  - **Walk algorithm**:
    - List session tempdirs under `~/Library/Caches/roki/sessions/` (or platform equivalent via `dirs::cache_dir()`).
    - For each configured `[[repos]]` entry, run `git worktree list --porcelain` and filter to branches matching the operator-configurable regex (default `^[A-Z]+-\d+$`; configurable via a new optional `[recovery].issue_branch_pattern` config key — additive).
    - Reconcile every distinct issue id discovered (from either source) against Linear via the production `RecoveryLinearReader`.
  - **Production `RecoveryLinearReader`**: implementation backed by `LinearTracker` (or a thin client wrapping the same Linear GraphQL surface). Folds the task-5.2 stub into the production codebase.
  - Observable completion: (a) integration test exercises all 5 matrix cells with both session tempdirs and worktrees pre-seeded; (b) the `5.2` follow-up note in the task list is closed; (c) tests pass deterministically across 3 sequential reps.
  - _Depends: 7.1d_
  - _Requirements: 10.1, 10.2_
  - _Design: same as 7.1_
  - _Supersedes: 5.2_

- [ ] 7.1f Bootstrap finalization + e2e refactor + SPEC/design same-change-set rewrites
  - _Boundary:_ `crates/roki-daemon/src/runtime.rs` final wiring (single tracker, single webhook, single workflow loader); refactor `tests/e2e_{happy_path,failure_retry,bootstrap}.rs` to the new agent-driven flow (each refactored test must pass deterministically across 3 sequential reps); SPEC.md §2.2/§2.3/§6/§7/§10 rewrites; `.kiro/specs/roki-mvp/design.md` architecture-prose update.
  - Bootstrap composition: load config → init redacted logging (with `[linear].webhook_secret_env`-resolved value in the redaction list) → install signal handlers → load single `WORKFLOW.md` from `[workflow].path` → build `SessionManager`, `WorktreeRegistry`, `PermissionResolver`, `ClaudeEngineAdapter`, `RealWt`, `RealGhq` → build `Orchestrator::with_recovery` → start single `LinearTracker` → mount single `POST /linear/webhook` route → axum::serve → run until shutdown.
  - **Doc updates (same change set per §16)**:
    - `SPEC.md` §2.2 — describe `[[repos]]` as the agent allowlist; `[linear]` and `[workflow]` block descriptions.
    - `SPEC.md` §2.3 — replace the deterministic-precedence-rule section with "agent-driven repo selection via `roki_open_worktree`."
    - `SPEC.md` §6 — replace the worktree-path section with "session tempdir layout + `WorktreeRegistry` semantics + lifecycle invariants (open via tool, remove on Cleaning, retain on TerminalFailure)."
    - `SPEC.md` §7 — add `roki_open_worktree` to the registry table with input/output/error shape.
    - `SPEC.md` §10 — rewrite the recovery section to walk both session tempdirs and worktrees per the new five-cell matrix.
    - `.kiro/specs/roki-mvp/design.md` — fold the agent-driven model into the architecture prose; show the new component breakdown (`SessionManager`, `WorktreeRegistry`, `RokiOpenWorktreeTool`).
  - **Determinism gate**: every refactored e2e must pass deterministically across 3 sequential reps with `-- --test-threads=1`.
  - Observable completion: (a) all tests across `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` clean; (b) every refactored e2e test passes deterministically across 3 sequential reps; (c) new cross-repo e2e (from 7.1d) and allowlist-rejection test (from 7.1d) still pass; (d) SPEC.md §2.2/§2.3/§6/§7/§10 reflect the new model; (e) restart recovery test from 7.1e exercises all five matrix cells.
  - _Depends: 7.1e_
  - _Requirements: 1.1, 1.2, 2.1, 2.5, 3.1, 3.2, 4.1, 8.2, 9.5, 10.1, 12.2_
  - _Design: same as 7.1_

<!-- Original single-task envelope kept below for archival; superseded by 7.1a–7.1f above. -->

- [ ] ~~7.1 Replace `repos.scope` daemon-side routing with agent-driven repo selection~~ (split into 7.1a–7.1f; original envelope kept below for archival)
  - _Boundary:_ `crates/roki-daemon/src/config/{mod.rs,repos.rs}`, `crates/roki-daemon/src/routing.rs` (deleted), `crates/roki-daemon/src/orchestrator/{state.rs,core.rs,tracker_bridge.rs,recovery.rs}`, `crates/roki-daemon/src/workspace/` (REWRITTEN as `session/` and `worktrees/` modules; the `Workspace` trait is dropped), `crates/roki-daemon/src/tools/{mod.rs,roki_open_worktree.rs}` (new tool alongside existing `linear_graphql`), `crates/roki-daemon/src/tracker/{linear.rs,webhook.rs}`, `crates/roki-daemon/src/runtime.rs`, `SPEC.md` (§2.2, §2.3, §6, §7, §10 — major rewrites), `.kiro/specs/roki-mvp/design.md`. Tests: every existing e2e test under `crates/roki-daemon/tests/` refactors; new tests for cross-repo worker, allowlist rejection, single-webhook dispatch.
  - **Locked decisions** (from `.kiro/specs/roki-mvp/design-agent-driven-repo-selection.md`):
    1. Tool name = `roki_open_worktree`. Daemon-owned semantics, namespaced like `linear_graphql`.
    2. Tool input = `{ repo: string }` only. Branch is hard-locked to the issue id verbatim — no agent override.
    3. Repo allowlist enforcement is STRICT. The tool refuses any `repo` not in `[[repos]]`; returns a typed `RepoNotInAllowlist { repo, allowed }` error to the agent.
    4. Tool is idempotent: second call with the same `repo` for the same worker returns the existing path without re-running `wt switch --create`.
    5. Session tempdir lives at `~/Library/Caches/roki/sessions/<issue>` on macOS / `~/.cache/roki/sessions/<issue>` on Linux (via the `dirs` crate or equivalent XDG resolver).
    6. Single workspace-level `WORKFLOW.md` configured at `[workflow].path`. Per-repo policy override is removed.
    7. Admission filter = admit every Linear issue update. The agent decides whether to do work; WORKFLOW.md gates handle the cheap "this isn't for me" exit.
    8. CleanExit advances to `AwaitingReview` regardless of whether the agent ever called `roki_open_worktree` — a worker that never opened a worktree is still a valid no-op path.
    9. Restart recovery walks BOTH session tempdirs AND every configured repo's `git worktree list --porcelain` (filtered to issue-id-shaped branch names via the operator-configurable regex `^[A-Z]+-\d+$`).
    10. The `Workspace` trait is dropped. Concrete types `SessionManager` (tempdir lifecycle) and `WorktreeRegistry` (per-worker worktree tracking) replace it.
    11. Cleanup on `Cleaning` is daemon-side: walks `WorktreeRegistry` for the worker and calls `wt.remove` on each (subject to existing pre-cleanup hooks). On `TerminalFailure`, all worktrees AND the session tempdir are retained.
  - **Schema delta (breaking)**:
    - REMOVE `RepoConfig.id`, `RepoConfig.scope`, `RepoConfig.webhook_secret_env`, `RepoConfig.webhook_secret`, `RepoConfig.workflow_path`. After 7.1, `RepoConfig` is `{ repo: String }` only.
    - REMOVE the `LinearScope` enum and the `routing.rs` module entirely.
    - ADD `[linear]` config block: `token_env: Option<String>` (defaults to `"LINEAR_API_TOKEN"`), `webhook_secret_env: String` (required), `endpoint: Option<String>` (test-only override; production omits).
    - ADD `[workflow]` config block: `path: PathBuf` (required, single workspace-level policy file).
    - REJECT duplicate `[[repos]]` entries with the same `repo` value at config load (hard refusal naming the offending entry).
  - **State-machine impact**:
    - `(repo, issue)` collapses to `(issue,)`. `RepoId` stays as a type for `WorktreeRegistry` keying but is no longer in the state-machine key. Update `ActorRecord` keying, `TransitionEvent.repo` becomes `Option<RepoId>` populated post-tool-call (or removed entirely if the field has no observable consumers — implementer's call, but document it in the SPEC.md update).
    - `Queued → Active` no longer pre-creates a worktree. Instead it creates a session tempdir via `SessionManager::create_session(issue)` and that becomes the worker's CWD.
    - `Cleaning → [*]` iterates the worker's `WorktreeRegistry` entries and calls `wt.remove` on each (one-by-one, log per-arc, subject to pre-cleanup hooks); then removes the session tempdir.
    - `TrackerBridge` dedup keys collapse from `(repo, issue, target_state)` to `(issue, target_state)`.
  - **Webhook handler (single-route)**:
    - URL: `POST /linear/webhook` (no per-repo path segment).
    - HMAC verify against `[linear].webhook_secret_env` (single workspace-level secret).
    - Decode → `NormalizedIssue` (no repo association at this point).
    - Forward to orchestrator's `tracker_inbox` keyed by `IssueId`.
    - Spawn a worker for any new `IssueId` that isn't already in flight; the orchestrator never consults `[[repos]]` at admission time.
  - **Single `LinearTracker`**:
    - One poller for the entire Linear workspace (not per repo). Honor the existing global `polling_cadence` and 5-min cap.
    - No `scope` filtering. Every issue the API token can see produces a `NormalizedIssue` event.
  - **New agent tool `roki_open_worktree`**:
    - Registered in the agent's tool registry alongside `linear_graphql`.
    - Description (verbatim, render in the agent's tool surface): "Open a git worktree for the current Linear issue in one of the configured repos. The daemon resolves the repo via ghq, creates a worktree branch named after the issue id via wt, and returns the absolute path. Idempotent — calling twice with the same repo returns the same path. Use this once per repo you intend to modify; cross-repo tickets call this multiple times."
    - Input: `{ repo: string }` only. Strict allowlist.
    - Output: `{ path: string, repo: string, branch: string }` where `branch == issue.id`.
    - Errors (typed; route through existing tool-error taxonomy): `RepoNotInAllowlist { repo, allowed: [string] }`, `GhqResolutionFailed { repo, reason }`, `WorktreeCreationFailed { repo, branch, reason }`.
    - Handler flow: validate allowlist → `ghq.ensure_cloned(repo)` → `wt.switch_create(repo_path, issue.as_str())` → register `(worker_id, repo, branch, worktree_path)` in `WorktreeRegistry` → return path.
    - Idempotency: handler checks `WorktreeRegistry` first; if `(worker_id, repo)` already exists, returns the existing path without invoking `ghq`/`wt`.
  - **Restart recovery (folds task 5.2 into 7.1)**:
    - Walk session tempdirs under `~/Library/Caches/roki/sessions/` (or platform equivalent).
    - For each configured `[[repos]]` entry, run `git worktree list --porcelain` and filter to branches matching the operator-configurable regex (default `^[A-Z]+-\d+$`).
    - Reconcile every distinct issue id discovered (from either source) against Linear via the existing `RecoveryLinearReader` trait. Provide a production `LinearTracker`-backed impl as part of this task.
    - Decision matrix expanded: `ResumeActive` (issue active in Linear, session+worktree(s) on disk), `OrphanedSession` (session tempdir but no Linear state and no worktree), `OrphanedWorktree` (worktree but no session), `FreshQueued` (Linear issue active, nothing on disk → fresh worker), `NoOp` (Linear issue terminal, nothing on disk).
  - **Doc updates (same change set per §16)**:
    - `SPEC.md` §2.2 — drop the workspace-root + per-repo workflow_path bullets; add `[linear]` and `[workflow]` block descriptions; describe `[[repos]]` as the agent allowlist.
    - `SPEC.md` §2.3 — replace the deterministic-precedence-rule section with "agent-driven repo selection via `roki_open_worktree`."
    - `SPEC.md` §6 — replace the worktree path section with "session tempdir layout + `WorktreeRegistry` semantics + lifecycle invariants (open via tool, remove on Cleaning, retain on TerminalFailure)."
    - `SPEC.md` §7 — add `roki_open_worktree` to the registry table with input/output/error shape.
    - `SPEC.md` §10 — rewrite the recovery section to walk both session tempdirs and worktrees per the new four/five-cell matrix.
    - `.kiro/specs/roki-mvp/design.md` — fold the agent-driven model into the architecture-prose; show the new component breakdown (`SessionManager`, `WorktreeRegistry`, `RokiOpenWorktreeTool`).
  - **Refusal modes**: `[linear].webhook_secret_env` not set → hard refusal; `[workflow].path` missing or unreadable → hard refusal; no `[[repos]]` entries → WARN log, daemon starts but every `roki_open_worktree` call returns `RepoNotInAllowlist`; `wt`/`ghq`/`claude` absent → hard refusal (existing); duplicate `repo` in `[[repos]]` → hard refusal at config load; agent specifies a `repo` not in the allowlist → tool error to agent (worker continues).
  - Observable completion: (a) all tests across `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` clean; (b) every refactored e2e test passes deterministically across 3 sequential reps; (c) new cross-repo test where one worker opens worktrees in two configured repos passes; (d) new allowlist-rejection test where the agent specifies a non-allowlisted repo asserts the typed error and that no worktree was created; (e) `crates/roki-daemon/src/routing.rs` is gone from the file tree; (f) `RepoConfig` shrinks to a single field; (g) SPEC.md §2.2/§2.3/§6/§7/§10 reflect the new model; (h) restart recovery test exercises all five matrix cells with both session tempdirs and worktrees pre-seeded.
  - _Depends: 6.1, 5.1_
  - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.6, 2.7, 3.1, 3.2, 4.1, 4.2, 4.4, 4.5, 4.6, 6.1, 7.1, 7.4, 7.5, 7.6, 7.7, 8.2, 10.1, 10.2, 10.3, 10.4_
  - _Design: `.kiro/specs/roki-mvp/design-agent-driven-repo-selection.md`_

- [ ] 6. Workspace model migration: switch from sandbox dirs to git worktrees

- [x] 6.1 Replace the sandbox-dir workspace model with `wt` + `ghq` git worktrees
  - _Boundary:_ `crates/roki-daemon/src/config/{mod.rs,repos.rs}`, `crates/roki-daemon/src/tools/{mod.rs,wt.rs,ghq.rs}` (the latter two new), `crates/roki-daemon/src/workspace/{mod.rs,layout.rs}`, `crates/roki-daemon/src/runtime.rs` (bootstrap composition), `SPEC.md` §2.2 + §6 (rewrite), `.kiro/specs/roki-mvp/design.md`. Tests under `crates/roki-daemon/tests/orchestrator_workspace.rs`, `e2e_happy_path.rs`, `e2e_failure_retry.rs`, `e2e_multi_repo_routing.rs` (only struct-literal cascades), `e2e_vetoable_transition.rs` (only if needed), `e2e_bootstrap.rs`, plus new unit tests beside `tools/wt.rs` and `tools/ghq.rs`.
  - **Locked decisions** (from `.kiro/specs/roki-mvp/design-worktree-workspace.md`):
    1. Worktree backend = `wt` (worktrunk) external CLI. Operator installs; daemon assumes on `$PATH`. Hard refusal at startup if absent.
    2. Repo discovery = `ghq` external CLI. `RepoConfig.repo: String` carries an `owner/repo` (or `host/owner/repo`) identifier; local path resolved at runtime via `ghq list -p` / `ghq get`. Hard refusal at startup if `ghq` absent.
    3. Branch name = the Linear issue id verbatim (`IssueId.as_str()`).
    4. Worktree path layout = `{repo_path}/../{repo_name}.{branch_sanitized}` per monorail's `WtTool::switch_create`.
    5. Cleanup on `Cleaning` = `wt remove` on the worktree path. Branch is NOT deleted (`wt remove` does not delete branches).
    6. Retention on `TerminalFailure` = keep both worktree dir AND branch; the daemon simply does not call `wt remove`.
  - **Schema delta (breaking on `path`, dropping `workspace_root`)**:
    - Remove `workspace_root` from `Config` and from `ConfigFile`. Drop the `ROKI_WORKSPACE_ROOT` env override. Existing `roki.toml` referencing `workspace_root` must fail to load with a clear error naming the offending key.
    - Rename `RepoConfig.path` → `RepoConfig.repo: String` (ghq identifier). Validate at load: non-empty, matches `<token>/<token>` or `<host>/<token>/<token>` shape (no whitespace, no `..`, no leading `/`).
  - **New tools** (port from monorail):
    - `crates/roki-daemon/src/tools/wt.rs` — `WtTool` async trait with `switch_create(repo_path: &Path, branch: &str) -> Result<PathBuf>` and `remove(worktree_path: &Path) -> Result<()>`. `RealWt` shells out to `wt -C <repo_path> switch --create <branch>` and `wt -C <worktree_path> remove`. Branch sanitization (chars outside `[A-Za-z0-9_-]` → `-`) lives here. Pure unit tests for the sanitization.
    - `crates/roki-daemon/src/tools/ghq.rs` — `GhqTool` async trait with `list_path(full: &str) -> Result<Option<PathBuf>>` and `ensure_cloned(full: &str) -> Result<PathBuf>`. `RealGhq` shells out to `ghq list -p` and `ghq get`. Unit tests for failure-path classification (command missing → `Ok(None)` for list, distinct error for ensure).
    - `crates/roki-daemon/src/tools/mod.rs` — re-export `WtTool`, `RealWt`, `GhqTool`, `RealGhq`. Existing `linear_graphql` re-exports remain untouched.
  - **`Workspace` trait + `WorkspaceManager` rewrite**:
    - `WorkspaceManager` drops `workspace_root` field; gains `wt: Arc<dyn WtTool>` and `ghq: Arc<dyn GhqTool>`.
    - The `Workspace` trait signature stays identical. Implementations of `ensure(repo, issue)` flow: (a) look up the repo's ghq identifier from operator config (the manager carries a `HashMap<RepoId, GhqIdentifier>` populated at construction), (b) `ghq.ensure_cloned(identifier)` → repo_path, (c) `wt.switch_create(repo_path, issue.as_str())` → worktree_path, (d) return a `Workspace` whose `path` is the worktree_path.
    - `remove(repo, issue)` derives the worktree path the same way (deterministic from repo_path + sanitized branch) and calls `wt.remove(worktree_path)`.
    - `list_existing()` may stub-out for now (returns empty Vec) with a doc-comment pointing at task 5.2 (restart recovery) for the real impl. The current `list_existing` is only consumed by recovery, which is itself unwired (5.2 follow-up).
    - Path-safety invariants change: drop the "must descend from `workspace_root`" rule. Keep the collision rule (two distinct issue ids must not produce the same worktree path under the same repo). Reuse `wt.rs`'s sanitizer rather than re-rolling.
  - **Bootstrap composition** (`runtime::run_with_shutdown`):
    - At startup, refuse with a clear actionable error if `wt` or `ghq` are not on `$PATH`. Use `which::which("wt")` / `which::which("ghq")` (add `which` as a dep if not already present) or fall back to `Command::new("wt").arg("--version").output()` and treat `NotFound` as the refusal trigger.
    - Construct `RealWt` and `RealGhq`; thread them into `WorkspaceManager::new(wt, ghq, repo_index)` where `repo_index` is the operator-supplied map from `RepoId` to ghq identifier.
    - Remove `workspace_root` from the bootstrap path. Drop the `Config::workspace_root` reference and any `tokio::fs::create_dir_all` for it.
  - **Doc updates (same change set per §16)**:
    - `SPEC.md` §2.2 — drop the `workspace root` bullet; add a new bullet describing the `repo` ghq identifier and that the worktree path is derived at runtime.
    - `SPEC.md` §6 — rewrite the entire section: remove the `<workspace_root>/<repo>/<issue>/` layout description, replace with the `{repo_path}/../{repo_name}.{branch}` worktree layout, document the sanitization rule (lives in `wt`), document the lifecycle invariants (creation on `Queued → Active` via `wt switch --create`, deletion on `Cleaning → [*]` via `wt remove`, retention on `TerminalFailure` keeps both dir and branch).
    - `.kiro/specs/roki-mvp/design.md` — update the `WorkspaceManager` component prose to reflect the new dependencies (`WtTool`, `GhqTool`) and the elimination of a workspace root.
  - **Test refactor**:
    - `tests/orchestrator_workspace.rs` — inject mock `WtTool` + mock `GhqTool` via the trait. The mocks record invocations so the test can assert "ensure → ghq.ensure_cloned called once with the configured identifier; wt.switch_create called once with the resolved repo path and the issue id" and "remove → wt.remove called once with the same worktree path".
    - `tests/e2e_happy_path.rs`, `tests/e2e_failure_retry.rs`, `tests/e2e_bootstrap.rs` — replace the temp-dir workspace_root with a constructed `WorkspaceManager` whose `WtTool` and `GhqTool` are mocks returning a `tempfile::TempDir`-backed path that mimics the worktree layout. The orchestrator never calls into `wt`/`ghq` directly, so swapping the `WorkspaceManager` deps is sufficient.
    - `tests/e2e_multi_repo_routing.rs` — only mechanical struct-literal updates for the renamed `RepoConfig.repo` field and removed `RepoConfig.path` field.
    - `tests/e2e_vetoable_transition.rs` — same mechanical update if it constructs `RepoConfig` literals.
    - All e2e tests must remain deterministic and pass 3 sequential reps each.
  - **Refusal modes** — `runtime::run_with_shutdown` must `Err(...)` with a clear, actionable message when: `wt` not on PATH, `ghq` not on PATH, `RepoConfig.repo` malformed, `ghq.ensure_cloned` returns a network/clone failure (mark repo unhealthy and continue with other repos rather than aborting the daemon — matches existing 1.5 health-check seam), `wt switch --create` fails because the branch already exists elsewhere (escalate per `(repo, issue)`, do not abort the daemon).
  - Observable completion: (a) `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` all clean; (b) every existing e2e test passes deterministically across 3 sequential reps with mock `WtTool`/`GhqTool`; (c) new unit tests in `tools/wt.rs` and `tools/ghq.rs` exercise the sanitization and failure paths; (d) `SPEC.md` §2.2 and §6 reflect the new model; (e) `RepoConfig` struct fields, `Config` struct fields, and `Workspace` trait shape match the design; (f) the bootstrap refuses to start with a clear message when `wt` or `ghq` is absent (verifiable via a unit test that overrides PATH lookup or via inspection of the refusal-error message strings).
  - _Depends: 5.1, 2.2_
  - _Requirements: 4.3, 4.4, 4.5, 4.6, 10.1_
  - _Design: `.kiro/specs/roki-mvp/design-worktree-workspace.md`_

- [ ] 5. Bootstrap: make `roki run` actually run the daemon end-to-end

- [ ] ~~5.2~~ Wire restart recovery through the bootstrap (SUPERSEDED by 7.1e)
  - _Boundary:_ a new production `RecoveryLinearReader` impl backed by `LinearTracker` (live module path TBD — implementer chooses), `crates/roki-daemon/src/runtime.rs` (swap `Orchestrator::new` for `Orchestrator::with_recovery`), and a new integration test that pre-seeds workspace directories before invoking `runtime::run_with_shutdown`.
  - Implements Requirement 10.1 at the daemon-binary level. Today the recovery scan and reconciliation logic exist in `orchestrator/recovery.rs` (shipped in task 3.3) but the bootstrap calls `Orchestrator::new` and never invokes them, so a real restart of the daemon does not reconcile.
  - Acceptance: a fresh-restart integration test pre-seeds at least two workspace directories under the configured workspace root (one whose Linear state is "active", one whose Linear state is "done"), starts the daemon via `runtime::run_with_shutdown`, and asserts the orchestrator's per-issue actor records line up with the four-cell recovery matrix (`ResumeActive` / `OrphanedWorkspace` / `FreshQueued` / `NoOp`) per `crates/roki-daemon/src/orchestrator/recovery.rs::reconcile_decisions`.
  - _Depends: 5.1, 3.3_
  - _Requirements: 10.1, 10.2, 10.3, 10.4_

- [x] 5.1 Wire the daemon bootstrap end-to-end
  - _Boundary:_ `crates/roki-daemon/src/cli.rs`, `crates/roki-daemon/src/config/{mod.rs,repos.rs}`, `crates/roki-daemon/src/runtime.rs`, `crates/roki-daemon/src/engine/policy.rs` (new `EnginePolicy::from_workflow`), `crates/roki-daemon/src/orchestrator/core.rs` (only if a thin builder addition is needed; prefer existing `with_engine_policy`), `SPEC.md` §3.2 + new short startup-sequence subsection in §9, `.kiro/specs/roki-mvp/design.md` (architecture-prose update). Tests under `crates/roki-daemon/tests/e2e_bootstrap.rs` and additive unit tests next to changed modules.
  - **Config schema (additive)** — Extend `Config` / `ConfigFile` per `.kiro/specs/roki-mvp/design-bootstrap.md`:
    - New `[server]` section with `bind` (default `127.0.0.1`) and `port` (default `7878`).
    - New per-repo `webhook_secret_env: Option<String>` (preferred) and `webhook_secret: Option<SecretString>` (literal, flagged WARN on load). Exactly one must resolve to a non-empty value at runtime.
    - Optional top-level `claude_binary: Option<PathBuf>`. Default = `which("claude")` resolved at bootstrap; absence is a hard error with a clear remediation message.
    - Per-repo loaders read `WORKFLOW.md` from `repo.workflow_path` (already a config field).
  - **CLI flags** — Extend `RunArgs`: `--config <path>` (default `./roki.toml`), `--bind <addr>`, `--port <num>`, `--dangerously-skip-permissions`. CLI overrides config; document precedence in `--help` text. Default config path applies only when `--config` is omitted; explicit but missing paths must error.
  - **Bootstrap order** — In `runtime::run`, replace the current shutdown-only stub with: (a) load config; (b) initialize logging with the resolved secret list (Linear token + every per-repo webhook secret) so all are redacted; (c) install signal handlers; (d) start `WorkflowLoader` per repo (each repo's `WORKFLOW.md` is hot-watched); (e) build `WorkspaceManager`, `PermissionResolver`, `ClaudeEngineAdapter`; (f) build `Orchestrator` with `EnginePolicy::from_workflow(&policy)` per repo (or a single resolved policy if you prefer one runtime policy — call out which); (g) for each `RepoConfig` start a `LinearTracker` (poll task) and build a `WebhookState`; (h) construct one `axum::Router` mounting `/linear/webhook/<repo-id>` for every repo; (i) `axum::serve(TcpListener::bind(server.addr))` on a single port, all repos mounted; (j) plumb tracker outputs through `TrackerBridge` into the orchestrator; (k) `tokio::select!` on shutdown across the orchestrator, every tracker, the bridge, and the axum server; (l) on shutdown, route through the existing `await_workers_with_window` (SHUTDOWN_WINDOW=30s).
  - **`WorkflowPolicy → EnginePolicy` resolution (closes the 3.7 follow-up)** — Add `EnginePolicy::from_workflow(&WorkflowPolicy) -> EnginePolicy` translating `max_turns`, `stall_window_seconds`, `backoff`, and `max_attempts` from the parsed `WorkflowPolicy` into the runtime `EnginePolicy`. Used by the bootstrap; unit-tested.
  - **Webhook secret loading** — Resolve per repo at bootstrap: if `webhook_secret_env` is set, read the named env var (error if absent or empty); if `webhook_secret` is set as a literal, accept with a WARN log; if both are unset, error with a remediation message. Wrap the resolved value in `SecretString` and inject into `WebhookState`.
  - **Per-repo webhook routes** — Mount `/linear/webhook/<repo-id>` for each `RepoConfig`. Path component is the repo `id` verbatim (the existing `routing::sanitize_component` rule applies — verify and reuse, do not re-roll).
  - **Per-repo trackers** — Each `LinearTracker` is constructed per scope from `RepoConfig.scope` and the shared Linear token. Each tracker honors the global `polling_cadence` from config. Trackers run as separate tokio tasks owned by a `JoinSet`; their outputs are funneled into the existing `TrackerBridge`.
  - **`--dangerously-skip-permissions` flag** — Overrides `[permission_strategy].mode` to the dangerous fallback regardless of config; emits a WARN log on every worker launch via the existing `PermissionResolver` warn-log path.
  - **SPEC.md / design.md updates ship in the same change set** — Per SPEC.md §16. Add a `[server]` row to the §3.2 schema table, a `webhook_secret_env` row, and a new short subsection in §9 documenting the bootstrap order (the 12 numbered steps above) and the per-repo webhook path scheme.
  - **Smoke test (`tests/e2e_bootstrap.rs`)** — Drives `runtime::run` end-to-end with: (a) a temp `roki.toml` pointing at a temp `WORKFLOW.md` and a temp workspace root; (b) one `RepoConfig` whose webhook secret is read from a test-set env var; (c) a `wiremock::MockServer` standing in for Linear (matching the existing 4.2 / 4.3 fake-Linear shape); (d) `fake_claude` in `clean_exit` mode for a single happy path. The test (i) starts `runtime::run` in a tokio task; (ii) waits for the axum server to be ready (a `GET /linear/webhook/<repo-id>` returning 405 method-not-allowed is the cheapest readiness probe — verify what axum returns for a path-but-wrong-method; if not 405, use a different deterministic probe that does not require a valid HMAC signature); (iii) posts a properly HMAC-SHA256-signed Linear webhook envelope to `/linear/webhook/<repo-id>`; (iv) asserts the orchestrator drives the issue through the documented happy-path transition sequence; (v) shuts down via `SIGINT`-equivalent (drop the shutdown sender / abort the task with a graceful trigger) and confirms `runtime::run` returns `Ok(())` within the 30s shutdown window. The test must be deterministic across 3 sequential reps and complete in under ~15s wall (dominated by the `fake_claude` build + axum bind).
  - **Refusal modes** — `runtime::run` must `Err(...)` (clear, actionable message) when: config file missing at the resolved path; required field missing; `linear_token` unresolved; webhook secret unresolved for any repo; `claude` binary not found and no override; `[server]` port already in use. Errors are logged at ERROR level and propagated as the process exit code.
  - **Determinism note** — The bootstrap MUST NOT block on Linear connectivity at startup. Trackers retry their first poll asynchronously; webhook delivery and `roki run` startup are independent. Document this so an operator can ngrok-test before Linear is configured.
  - Observable completion: (a) running `roki run --config ./roki.toml` against the smoke-test fixture produces the documented startup log line, binds the configured port, mounts a route per repo, and reaches the documented terminal state for a posted webhook; (b) `e2e_bootstrap.rs` passes deterministically across 3 sequential reps; (c) `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo fmt --all -- --check` all clean; (d) SPEC.md §3.2 + §9 and design.md show the new subsection and rows.
  - _Depends: 3.7, 3.6, 4.2_
  - _Requirements: 1.1, 1.2, 1.3, 1.6, 2.1, 2.3, 2.5, 3.1, 3.3, 4.1, 8.2, 9.5, 10.1, 12.2_
  - _Design: `.kiro/specs/roki-mvp/design-bootstrap.md`_

## Implementation Notes

- 2.1: `legal_transition` includes `Queued -> TerminalFailure` (failure path before a worker runs, e.g. unrouteable issue) — additive supplement to design.md's lifecycle diagram; consider folding into design.md when next revised.
- 2.1: `legal_transition` doc-comment claims compile-time exhaustiveness but the body has a `_ => false` catch-all; the `legal_transition_rejects_undocumented_pairs` matrix test enforces exhaustiveness at test time. Either remove the wildcard or correct the doc-comment in a follow-up.
- 2.1: `TransitionEvent` carries an additional derived `vetoable: bool` field beyond the design.md sketch; it is purely derived from `(previous, next)` via `is_vetoable`. Fold into design.md's `TransitionEvent` shape when next revised.
- 2.1a: `PreCleanupHook::pre_cleanup` returns `VetoDecision` directly; design.md line 381 sketches `on_pre_cleanup(...) -> Result<VetoDecision, SubscriberError>`. Task 3.x must reconcile when the orchestrator wires hooks into the `TerminalSuccess -> Cleaning` dispatch — either widen the trait return type or wrap at orchestrator side to honor design's fail-closed-on-error stance.
- 2.1a: `register_pre_cleanup_hook` is on `HookRegistry` returning `usize` (current count); design.md sketches it on the `Orchestrator` trait returning `SubscriptionHandle`. Acceptable for 2.1a since `Orchestrator` doesn't exist yet; 3.x must republish the API on the `Orchestrator` trait and add deregistration.
- 2.3: `BackoffPolicy.max_seconds` default 300 is correct, but the JSON-Schema upper bound allows up to 3600 (1h). Design says "capped at 5min". Tighten the schema bound to 300 in a follow-up touch-up.
- 2.3: `routing::tests::route_issue_emits_routed_event_with_required_fields` (crates/roki-daemon/src/routing.rs:449) intermittently fails when run in the full workspace due to `tracing::subscriber::set_default` not isolating across parallel tests. Pre-existing — observed during 2.3 review; passes in isolation. Fix in 1.5 follow-up by switching to a per-test custom subscriber via `tracing::subscriber::with_default` scoped to the test thread.
- 2.10: `WorkerContext` shape diverges from design.md slightly — instead of separate `policy: WorkflowPolicy`, `permission: PermissionStrategy`, `max_turns`, `stall_window` fields, the supervisor consumes already-resolved `permission: ResolvedPermission` (from 2.9) and `policy: EnginePolicy` (from 2.8). Update design.md to reflect this when next revised.
- 2.10: `WorkerOutcome::TurnBudgetExhausted` and `Cancelled` are not produced by the single-launch supervisor — they belong to the orchestrator's continuation-prompt loop and shutdown path respectively. The orchestrator (3.x) must produce these outcomes itself.
- 3.5: state.rs has no explicit `Cleaning -> TerminalFailure` transition; on remove-failure during Cleaning the actor exits its loop and the poisoned set fences future tracker events. Add an explicit `Cleaning -> TerminalFailure` arc in state.rs so the lifecycle is observable end-to-end.
- 4.3: TASK PLAN INVALIDATED. Production retry-budget Backoff loop is missing. `WorkerActor::try_promote_to_active` routes all non-clean outcomes directly to `TerminalFailure` on first failure; `EnginePolicy` has no `max_attempts`; WORKFLOW.md schema has no `max_retries` key; `BACKOFF_FLOOR=10s` is unconditionally clamped. SPEC.md §4.2 + design.md line 761 mandate the loop. Task 3.2's happy-path-only build silently dropped this scope. A new src-level task (suggested `3.7`) must ship the loop + config knob + test override BEFORE 4.3 can run. Tasks 4.4 and 4.5 do not depend on retry-budget behavior and could proceed in parallel after human review re-orders the plan. **Resolved by task 3.7.**
- 3.7: Decision #1 (only `NonCleanExit` retries; `Stalled` and `TurnBudgetExhausted` go straight to `TerminalFailure`) recorded under `.kiro/specs/roki-mvp/design-retry-policy.md`. Schema key landed at the flat top level (`max_attempts`) to match the existing flat schema convention (`max_turns`, `stall_window_seconds`, `backoff`); the design-retry-policy.md prose mentioning `engine.max_attempts` was a path-prefix slip — flat is correct. `WorkflowPolicy → EnginePolicy` is currently a partial wiring: the parsed policy carries `max_attempts` and `Orchestrator::with_engine_policy` accepts a fully-built `EnginePolicy`, but the daemon's main wiring code does not yet build an `EnginePolicy` from a `WorkflowPolicy` (matches the existing state for `max_turns` / `stall_window_seconds`). End-to-end resolution wiring is a follow-up.
- 7.1a: APPROVED with two non-blocking notes. (a) The implementer's status report described `Config.linear_endpoint` as "mirrored" with `[linear].endpoint`; the actual code has a single path — `[linear].endpoint` in TOML is parsed via `LinearFile.endpoint` and assigned to `Config.linear_endpoint` (the 5.1 test-injection seam preserved). No code defect; report wording was misleading. (b) SPEC.md §2.3 still describes the deleted `route_issue` precedence rule, deleted "unhealthy repo" classification, and the soon-to-be-removed `(repository identifier, issue identifier)` keying. The 7.1a task brief scoped SPEC.md edits to §2.2 only, so this is intentionally out of scope here. Rewrite §2.3 in 7.1c (when the single tracker lands) or 7.1b (when the state-machine key collapses), whichever ships first.
- 7.1d: APPROVED. `SessionManager` (per-issue tempdir under `dirs::cache_dir()/roki/sessions`, idempotent `with_root` test seam) + `WorktreeRegistry` (`Arc<Mutex<HashMap<IssueId, Vec<RegisteredWorktree>>>>` with insertion-order preservation and short-circuit BEFORE invoking ghq/wt) + `roki_open_worktree` agent tool (verbatim description, strict allowlist via `Config.repos`, typed errors `RepoNotInAllowlist`/`GhqResolutionFailed`/`WorktreeCreationFailed` with stable `error_kind` strings). Orchestrator wiring: `Queued → Active` calls `SessionManager::create_session`; `Cleaning` evaluates pre-cleanup hooks → walks `WorktreeRegistry::take_for_issue` → `wt.remove` per worktree → `SessionManager::remove_session`; `TerminalFailure` retains both per design decision #6. `Workspace` trait dropped; `workspace/` modules deleted. `tests/orchestrator_workspace.rs` deleted (folded into new `tests/orchestrator_session.rs`). New cross-repo e2e (`tests/e2e_cross_repo_worktrees.rs`) confirms one worker can open worktrees in two repos under one issue with insertion-ordered cleanup. 281 passed / 1 ignored (pre-existing 7.1f bootstrap test). Two transitional shims tagged `TODO(7.1f)`: (i) `OpenWorktreeTool` not yet registered into the worker's tool registry through bootstrap (runtime.rs glue lands in 7.1f); (ii) `WorkerContext.repo` placeholder still `RepoId::new("")` since removal cascades into `engine/claude.rs` prelude (out of boundary). `recovery.rs` matrix-cell names retained (`OrphanedWorkspace` etc.) until 7.1e renames them to `OrphanedSession`/`OrphanedWorktree`; production `WorktreeRegistry::list_existing` returns `Vec::new()` since the in-memory registry is empty after a daemon restart (7.1e folds the disk walk).
- 7.1c: APPROVED. Single `LinearTracker` polling loop replaces per-scope fan-out; single `/linear/webhook` route with workspace-level HMAC secret verified before JSON deserialization (constant-time `Mac::verify_slice`). `NormalizedIssue.team_or_scope` dropped; cascade touched orchestrator/* and 4 test helpers (mechanical field-drops, no semantic change). 3 remaining `TODO(7.1c)` markers all live in `runtime.rs` outside boundary, deferred to 7.1f per task brief. `WebhookState::new` retains 3-arg shape with `_team_or_scope_fallback` ignored (build-compat shim until 7.1f rewrites runtime.rs to use `WebhookState::new_workspace`). `NormalizedIssue.repo` kept as vestigial stamp (already ignored by orchestrator post-7.1b); will be dropped in 7.1f. Each per-repo tracker that runtime.rs still spawns now collapses to one workspace poll loop internally — stream is not amplified by N repos. SPEC.md §2.3 rewrite deferred to 7.1f when bootstrap collapses to a single tracker instance.
- 7.1b: APPROVED. Implementer chose to remove `TransitionEvent.repo` entirely (vs `Option<RepoId>`) — internally consistent across all 14 changed files. Requirement 8.2's "include the originating repository identifier when one applies" remains forward-compatible: 7.1d will need to re-introduce a repo-carrying surface for `roki_open_worktree`-driven worktree-arc events (likely sourced from `WorktreeRegistry` lookup, not from the state-machine event itself). 7.1b added 6 `#[ignore]`-d tests beyond the pre-existing `e2e_bootstrap` ignore (`workspace_path_retained_across_backoff_loop`, 3 in `orchestrator_workspace.rs`, `e2e_happy_path`, `e2e_failure_retry`); each carries `TODO(7.1d):` reasons tied to the workspace NoOp-shim disconnect, and 7.1b's actual scope (state-key collapse, dedup, vetoable transitions, retry-budget loop) is preserved by non-ignored tests. `WorkerContext.repo = RepoId::new("")` placeholder and `Orchestrator::new`'s preserved `workspace: Arc<dyn Workspace>` arg are tagged `TODO(7.1d):` for removal. SPEC.md §2.3 rewrite still deferred to 7.1c or 7.1f.
- 6.1: APPROVED. Locked decisions #1-#6 honored exactly. Two cascade modifications outside the explicit boundary (orchestrator/core.rs and routing.rs) verified mechanical: orchestrator/core.rs only updates `workspace_error_path` match arms for the new `WorkspaceError` variant taxonomy (`Wt`/`UnknownRepo`/`Ghq` replacing `Io`/`EscapesRoot`) plus a test-only `StubWorkspace` swap; routing.rs is forced by `RepoConfig.path` → `RepoConfig.repo` rename and `classify_repo_health` is genuinely unreferenced from production code. Three tracked touch-ups: (a) `routing::classify_repo_health` has zero production callers (`config::validate_ghq_identifier` already enforces identifier shape at config load); recommend deletion in a follow-up. (b) `orchestrator/recovery.rs:7` doc comment still references the old `<workspace_root>/<repo>/<issue>/` layout; update when task 5.2 wires the real `list_existing` impl. (c) `e2e_bootstrap.rs` requires CI operators to pre-seed an `owner/<repo_id>` ghq checkout (skip is gated on `bootstrap_prerequisites_ready` — both `wt`/`ghq` on PATH AND `ghq list -p` returning a real path); document in the test runbook before the daemon ships. The `unsafe_code = "forbid"` lint blocks `set_var`-based env shimming, so the documented prerequisite is the cleanest available alternative for an unmocked bootstrap test.
- 5.1: APPROVED with five tracked follow-ups. (a) Tracker shutdown is NOT routed through `await_workers_with_window`; trackers exit on a oneshot signal, only the bridge and axum server are bounded by the 30s window. SPEC.md §9.7 step 10 was relaxed to match. Cooperative shutdown works in practice but a wedged tracker would block. Touch-up: collect the tracker `JoinSet` handles into the same bounded-shutdown collection so Requirement 1.3 holds uniformly. (b) Task description referenced `routing::sanitize_component`, which does not exist; the actual sibling function is `workspace::layout::sanitize_component` with `pub(super)` visibility. Implementer mirrored its character class into a private `runtime::sanitize_url_segment`. Future refactor task should promote a single `crate::ids::sanitize_path_segment` helper and route both webhook URLs and workspace path components through it. (c) `e2e_bootstrap.rs` asserts only the happy-path prefix (`Discovered → Queued → Active → AwaitingReview`); the full sequence is proven by `e2e_happy_path.rs` against the same component stack. Touch-up: extend the bootstrap smoke test with the progressive-mount pattern (`server.reset()` then `mount completed`) so the bootstrap-driven path is observed end-to-end. (d) CLI `--bind` / `--port` overrides land in the bootstrap but are not unit-tested; add a unit test that constructs `RunArgs { bind: Some(...), port: Some(...), .. }` and asserts `BootstrapHandles.bind_port` matches the override rather than the config-file value. (e) Restart recovery (Requirement 10.1) is NOT wired into the bootstrap — tracked as new task 5.2.
- 4.4: Test exercises `routing::route_issue` at the function-level integration boundary, not through the orchestrator's tracker-event admission path. Verified: `crate::routing` has zero production callers anywhere in `crates/roki-daemon/src/` (`grep -rn "use crate::routing\|routing::" crates/roki-daemon/src/` returns only an unrelated `axum::routing` import). In the current MVP each `LinearTracker` is per-scope, so the `TrackerBridge` never has to fan an issue across overlapping repo scopes — the single-`(repo, issue)` property is preserved by construction. SPEC.md §2.3 documents the routing contract abstractly and does not pin a hot-path call site, so there is no contract violation today. **Follow-up: open task 3.8** — "Wire `route_issue` into the orchestrator's tracker-event admission path" — only when the daemon moves to a multiplexed Linear connection or when overlapping scopes within the same tracker become possible. Until then the function-level test is the highest layer that exercises overlapping-scope precedence.
