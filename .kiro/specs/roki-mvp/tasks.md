---
refs:
  id: tasks:roki-mvp
  kind: tasks
  title: "roki-mvp Tasks"
  spec: roki-mvp
  depends_on:
    - design:roki-mvp
---

# Implementation Plan

- [ ] 1. Foundation: workspace, CLI, config, logging, shutdown

- [x] 1.1 Initialize Cargo workspace and roki-daemon crate
  - Create root `Cargo.toml` as `[workspace]` with `members = ["crates/roki-daemon"]`; reserve layout for additive future members (`crates/roki-tui`, `crates/roki-api-types`).
  - Create `crates/roki-daemon/` with `edition = "2024"`, binary name `roki`, dependencies (tokio, clap, tracing, tracing-subscriber, serde, serde_json, thiserror, anyhow, reqwest, axum, liquid, jsonschema, notify, dirs, serde_yaml, toml).
  - Create `src/main.rs` that bootstraps a tokio multi-thread runtime and hands control to the CLI shell.
  - Observable completion: `cargo metadata` reports a single workspace member at `crates/roki-daemon`; `cargo build` succeeds; `cargo run --bin roki -- --help` prints placeholder usage and exits zero.
  - _Requirements: 1.1_

- [x] 1.2 Implement CLI shell with all required flags
  - Define `roki run` subcommand with flags `--config <path>`, `--bind <addr>`, `--port <num>`, `--dangerously-skip-permissions`, `--debug` per [docs/reference/cli.md](../../../docs/reference/cli.md).
  - Wire `--help` for the binary and every subcommand; document each flag.
  - CLI flag values override config file values when supplied.
  - Observable completion: `cargo run --bin roki -- run --help` prints usage with all five documented flags; a unit test parses each flag and asserts the parsed `RunArgs` shape.
  - _Requirements: 1.6, 1.7_

- [x] 1.3 Build layered config loader with secret resolution and legacy-key refusal
  - Define typed config: `[linear]` (token source, webhook secret source, `assignee`, `admit_states`), `[workflow]` (path), `[server]` (bind, port), `[debug]` (per-issue log dir, level), `[[repos]]` (ghq identifier only), permission strategy.
  - Refuse to start on duplicate `[[repos]]` ghq identifier entries.
  - Refuse to start when `assignee` is empty / unresolvable, when `admit_states` resolves to empty (default `["Todo"]`), when `[linear]` token / webhook secret cannot be resolved, when `[workflow]` path is missing or unreadable.
  - Refuse to start with actionable error if `[judge].model`, `extension.linear_updater.*`, `extension.gates.*`, or `extension.distill.*` keys are present (legacy migration).
  - Observable completion: unit tests cover (a) valid config loads, (b) malformed value names the offending field, (c) duplicate ghq id refused, (d) legacy keys refused with actionable message, (e) `assignee = "me"` accepted as a placeholder for runtime resolution.
  - _Requirements: 1.2, 2.1, 2.2, 2.3, 2.4, 2.5, 2.7, 2.8, 2.9, 2.10, 2.12, 2.13_

- [x] 1.4 Initialize tracing + redaction + per-issue debug capture
  - Initialize `tracing-subscriber` with configurable level + destination (stdout, file, both); JSON output supported.
  - Add redaction layer scrubbing the Linear API token, the webhook HMAC secret, and operator-declared secret strings from every emitted event before egress.
  - Standardize structured fields (`issue`, `repo` when applicable, `correlation_id`, subprocess role tag) on every event that has them.
  - Implement per-issue debug capture sink (`--debug` flag / `[debug]` block) that appends every stdout + stderr line from each subprocess for the issue to `<debug_dir>/<issue>.log` with RFC 3339 nanosecond timestamps and `[STDOUT|STDERR]` stream tags + role tag (`orchestrator` or `phase:<phase-name>`).
  - On per-issue debug file open / append failure, log the failure with the offending path and continue running the subprocess without aborting the launch.
  - Observable completion: unit test asserts the configured token never appears in captured log output even when intentionally placed in a field value; second test captures debug-sink output and asserts timestamp + stream-tag + role-tag format; third test forces an open-failure and asserts the subprocess launch still succeeds.
  - _Requirements: 1.5, 7.4, 11.2, 11.3, 11.4, 11.6, 11.7_

- [x] 1.5 Implement bounded shutdown handling
  - Wire SIGINT and SIGTERM to a single `ShutdownSignal` propagated through orchestrator, tracker, webhook server, and engine adapters.
  - On shutdown: stop accepting new tracker events, send the orchestrator session a final `stop`-acknowledgement signal and close its stdin, SIGTERM in-flight phase subprocesses, await each within the configured per-subprocess shutdown window, then exit zero.
  - Bound the overall wind-down at `SHUTDOWN_WINDOW = 30s` via `await_workers_with_window`.
  - Observable completion: integration test starts the daemon with a fake long-running phase subprocess, sends SIGTERM, asserts the daemon exits cleanly within the documented window with the orchestrator stdin closed and phase subprocess SIGTERMed.
  - _Requirements: 1.4_

- [ ] 2. Core domain types

- [x] 2.1 (P) Define WorkerState, InactiveReason, Mode enums and transition table
  - Define `WorkerState { Pending, Active, Backoff, Inactive(InactiveReason), Cleaning }`.
  - Define `InactiveReason` with the 12 documented values: `AwaitingLinear`, `NeedsOperator`, `SpecIncomplete`, `NeedsSplit`, `AllowlistRejected`, `OrchestratorCrash`, `OrchestratorUnparseable`, `OrchestratorBudgetExhausted`, `Stall`, `RetryExhausted`, `FsPoison`, `Orphan`.
  - Define `Mode { SpecDriven, NeedsClassify }`.
  - Encode legal transitions per design state diagram; assert no vetoable transitions.
  - Define `TransitionEvent { issue, repo?, previous, next, trigger, mode?, inactive_reason?, correlation_id }` and `TransitionTrigger` enum (`TrackerEvent`, `AssignmentLost`, `RokiReadyRemoved`, `OrchestratorAction`, `PhaseEvent`, `DaemonDirective`, `OrchestratorDead`, `RecoveryScan`, `OperatorShutdown`).
  - Observable completion: unit test exercises every legal transition and asserts the resulting `TransitionEvent` shape, including correct `mode` on admission and correct `inactive_reason` on entries to `Inactive`; an attempt to transition `Active → Pending` with a phase-subprocess-exit-only trigger panics or returns a typed error per the design ("phase subprocess exit alone shall never trigger entry to `Cleaning`").
  - _Requirements: 2.6, 8.1, 8.2, 13.2_
  - _Boundary: orchestrator/state_

- [x] 2.2 (P) Define NormalizedIssue and Linear domain types
  - Define `IssueId`, `LinearStateName`, `LinearLabel`, `LinearUserId`, `RepoId` newtypes.
  - Define `NormalizedIssue { issue, title, body, current_linear_state, labels: BTreeSet<LinearLabel>, assignee: Option<LinearUserId> }`.
  - Recognize fixed labels `roki:ready` and `roki:impl` as constants (not operator-configurable).
  - Observable completion: unit test round-trips a sample Linear payload into `NormalizedIssue` and asserts label set membership for both fixed labels.
  - _Requirements: 3.5_
  - _Boundary: tracker/model_

- [x] 2.3 (P) Define OrchestratorAction schema and supporting types
  - Define `OrchestratorAction { action: ActionKind, phase: Option<PhaseName>, additional_context: Option<String>, outcome: Option<Outcome>, linear_writes: Option<Vec<LinearWriteAck>>, reason: BoundedString200 }`.
  - Define `ActionKind { RunPhase, LinearUpdateDone, Stop }`.
  - Define `Outcome { Success, Failure, Cancelled, NeedsOperator, SpecIncomplete, NeedsSplit, AllowlistRejected }`.
  - Define `LinearWriteAck { Label(String), CommentPosted(String) }` with grammar `label:<name>` / `comment_posted:<id>`; reserve other prefixes for additive forward-compat (round-tripped opaquely).
  - Publish JSON-Schema for the response envelope per design's "Orchestrator response schema" table.
  - Observable completion: unit test parses every documented `(action, phase, outcome)` combination and rejects illegal pairings (`phase` missing on `run_phase`, `outcome` missing on `stop`, `reason` over 200 chars, unknown enum value).
  - _Requirements: 4.2, 5.2, 5.11_
  - _Boundary: engine/orchestrator_session/action_parser_

- [x] 2.4 (P) Define daemon → orchestrator event catalog payloads
  - Define `DaemonEvent { PhaseComplete(payload), PhaseNonclean(payload), DaemonDirective(payload), TrackerTerminal(payload) }`.
  - `PhaseComplete` payload: `phase`, `result`, optional `pr_url` (`open_pr`), optional `review_artifact_path` (`finalize_review`), optional `classify.path` / `classify.suggested_command` / `classify.suggested_label` / `classify.target_feature` (`classify`).
  - `PhaseNonclean` payload: `phase`, `classification` (`non_zero` / `signal` / `stall` / `max_turns_exhausted` / `non_success_subtype` / `unknown_subtype`), `raw_subtype?`, `additional_context`.
  - `DaemonDirective` payload: `kind`, structured fields (`correlation_id`, `repos[]`, `worktree_path`, `last_subtype`, `attempts`, `window_ms`, `errno`), `timestamp`. Daemon contributes only `kind` + structured fields; never templates Linear text.
  - `TrackerTerminal` payload: `terminal_state` (`done` / `canceled` / `assignment_lost` / `roki_ready_removed`), `correlation_id`, `timestamp`.
  - Each event serializes to a single JSON object on its own line for stdin delivery.
  - Observable completion: unit test serializes every variant and asserts (a) one line per object, (b) no token / webhook-secret string ever appears in serialized output even when present in the surrounding context.
  - _Requirements: 4.2, 5.2, 12.5, 12.6_
  - _Boundary: engine/orchestrator_session_

- [x] 2.5 (P) Define PhaseName, PhaseLaunchContext, PhaseCatalog static lookup
  - Define `PhaseName { Classify, Implement, Review, Validate, OpenPr, CiFix, FinalizeReview }`.
  - Define `PhaseLaunchContext { issue, phase, mode, additional_context: Option<String>, worktree_path: Option<PathBuf>, session_tempdir, max_turns, workflow_policy, permission_strategy, allowed_tools }`.
  - Define `PhaseCatalogEntry { invocation: PhaseInvocation, default_max_turns: u32 }` and `PhaseInvocation { SlashCommand { skill, arg_template }, DaemonInternalTemplate { template_name } }`.
  - Encode the 7 × 2 default-invocation table from design (classify NEEDS_CLASSIFY only; implement / review / validate / ci_fix / finalize_review per mode; open_pr template-driven both modes).
  - Observable completion: unit test asserts every `(phase, mode)` pair returns the documented default invocation and `--max-turns`; mode-illegal pairs (`classify` outside `NEEDS_CLASSIFY` first turn) are rejected with a typed error.
  - _Requirements: 5.6, 5.12_
  - _Boundary: engine/phase_subprocess/catalog_

- [ ] 3. Tracker layer: Linear adapter, webhook, polling, pre-admission

- [x] 3.1 Implement Linear GraphQL client (read-only) with 429 backoff
  - Build a single workspace-level Linear GraphQL client over `reqwest`: `viewer` lookup (used at startup to resolve `[linear].assignee = "me"`), issue queries (id, title, body, state, labels, assignee).
  - Apply exponential backoff on HTTP 429 with documented window; log the backoff window. Generic 5xx and network failures use the same backoff curve.
  - Daemon-side adapter is read-only — never expose write operations on this surface.
  - Observable completion: integration test uses a wiremock Linear endpoint and asserts (a) `viewer` resolves to a single user id, (b) a 429 response triggers backoff before the next request, (c) no Linear write API path is reachable via this client.
  - _Requirements: 3.4, 3.5, 3.6, 7.2_
  - _Boundary: tracker/linear_

- [x] 3.2 (P) Implement webhook receiver with HMAC verify
  - Mount `POST /linear/webhook` on a single `axum::Router` with single workspace-level `WebhookState` carrying the resolved HMAC secret.
  - Verify HMAC against the workspace-level webhook secret before any normalization; reject mis-signed or missing-signature requests with the documented unauthorized status code without normalizing the payload.
  - On successful verification, normalize into `NormalizedIssue` and forward to the pre-admission-judge entry point.
  - Observable completion: integration test posts (a) a valid signed payload and asserts forward to pre-admission, (b) a tampered payload and asserts unauthorized status without normalization, (c) a payload with no signature header and asserts unauthorized.
  - _Depends: 2.2, 3.1_
  - _Requirements: 3.1, 3.2_
  - _Boundary: tracker/webhook_

- [x] 3.3 (P) Implement polling fallback with cadence cap
  - Build a single workspace-level poller that queries Linear for issues whose Linear state is in `[linear].admit_states` and whose assignee matches `[linear].assignee`, on a cadence of at most once every five minutes (configurable; cap enforced).
  - Each poll observation runs the pre-admission-judge identically to webhook-delivered payloads.
  - Respect 429 backoff state from the Linear client; nudges via `TrackerRefresh` honor the cadence cap and 429 backoff.
  - Observable completion: integration test asserts (a) poll cadence cap is enforced (consecutive nudges within 5 min do not trigger an extra HTTP request), (b) 429 backoff suspends polling for the documented window, (c) each polled issue runs through pre-admission identically to a webhook.
  - _Depends: 2.2, 3.1_
  - _Requirements: 3.3, 3.4_
  - _Boundary: tracker/linear_

- [x] 3.4 (P) Implement PreAdmissionJudge (4-condition + mode selection)
  - Resolve `[linear].assignee` once at startup (`me` → token-owner viewer id; explicit selectors must resolve to exactly one user id; missing / empty / ambiguous → configuration error).
  - Resolve `[linear].admit_states` once at startup (default `["Todo"]`; refuse on empty resolved set).
  - For every webhook + poll observation evaluate four ordered conditions: (a) `assignee == [linear].assignee`, (b) `linear_state ∈ admit_states`, (c) `roki:ready ∈ labels`, (d) presence of `roki:impl` selects `mode`: `SPEC_DRIVEN` if also present alongside `roki:ready`, `NEEDS_CLASSIFY` if only `roki:ready`, `RokiImplWithoutRokiReady` skip otherwise.
  - On any failed condition: silent skip — emit `tracker.pre_admission.skipped` log event with the failed condition; no state entry; no Linear write; no orchestrator launch.
  - On all conditions passing: emit `Admit { issue, mode }` to the orchestrator inbox.
  - Observable completion: unit test exercises the 16-row truth-table matrix (assignee × state × `roki:ready` × `roki:impl`), asserting silent-skip on failure and correct `mode` on each admit; a second test asserts mid-flight assignment loss and `roki:ready`-removal observations emit `AssignmentLost` and `RokiReadyRemoved` respectively.
  - _Depends: 2.2_
  - _Requirements: 2.14, 3.1, 3.3, 3.7, 3.8, 3.9, 3.10_
  - _Boundary: tracker/pre_admission_

- [x] 3.5 Publish TrackerRefresh nudge trait
  - Define `TrackerRefresh { fn nudge(&self) -> NudgeResult }` with `NudgeResult { Accepted, Throttled, BackoffActive }`.
  - Wire the nudge through the existing poller without bypassing the cadence cap or the 429 backoff state.
  - Observable completion: unit test issues two consecutive nudges within 5 min and asserts the second returns `Throttled`; a third issued during 429 backoff asserts `BackoffActive`.
  - _Requirements: 13.3_
  - _Boundary: tracker/refresh_

- [x] 3.6 Implement deduplication index and re-admission handling
  - Maintain an in-memory dedup index keyed by `IssueId` recording: current daemon state, per-issue `mode` (when non-terminal), most recently observed Linear state snapshot, in-flight orchestrator + phase handles.
  - On a webhook / poll observation of an issue already in a non-terminal state (`Pending`, `Active`, `Backoff`, `Cleaning`), update the snapshot in place; do not launch additional orchestrator or phase subprocess.
  - On startup-recovery + runtime webhook racing the same issue, serialize their effects so at most one orchestrator session and one phase subprocess are in flight per issue id at any instant.
  - On observation of an `Inactive` entry that newly passes pre-admission, clear the entry and start a fresh admission cycle from `Pending` with `mode` recomputed from the current Linear label set.
  - On mid-flight assignment loss or `roki:ready`-removal observation, terminate the orchestrator session and any in-flight phase subprocess for that issue, route to `Cleaning` without retry-budget consumption, and log the cause.
  - Observable completion: integration test seeds an in-flight issue, fires a duplicate webhook, asserts no second orchestrator is launched and the snapshot is updated; a second test seeds an `Inactive(reason)` entry, fires a passing webhook, asserts re-admission with recomputed `mode`; a third test fires an assignment-loss webhook mid-`implement`, asserts the orchestrator + phase are terminated and `Cleaning` is entered without consuming a retry attempt.
  - _Depends: 2.1, 3.4_
  - _Requirements: 3.10, 3.11, 3.12, 3.13, 3.14_
  - _Boundary: orchestrator/tracker_bridge_

- [ ] 4. WORKFLOW.md loader, schema, hot reload, rendering

- [x] 4.1 Implement WORKFLOW.md parser (front matter + Liquid + Markdown body)
  - Parse YAML (default) or TOML front matter; render Liquid + Markdown body; identify required and optional named template blocks within the rendered body.
  - Required blocks: `prompt_template_orchestrator`, `prompt_template_implement_direct`, `prompt_template_validate_direct`, `prompt_template_open_pr`. Refuse to start when any is missing.
  - Optional blocks: `prompt_template_<phase>` (per-phase override surface; phase ∈ {classify, implement, review, validate, open_pr, ci_fix, finalize_review}).
  - Observable completion: unit test parses a valid WORKFLOW.md with all four required blocks plus two optional `prompt_template_<phase>` blocks and asserts each block's body is reachable; a second test omits a required block and asserts startup refusal naming the missing block.
  - _Requirements: 2.15, 6.1_
  - _Boundary: workflow/parse_

- [x] 4.2 Implement WORKFLOW.md JSON-Schema validator with reserved namespaces
  - Validate the parsed policy shape against the published JSON-Schema. Reserved namespaces: `extension.orchestrator.*` (typed slice with `model`, `effort`, `max_phases`, `allowed_tools`, `stall_seconds`), `extension.phase.<name>.*` (typed slice with `command` xor `prompt_template_<phase>`, plus optional `max_turns`, `stall_seconds`, `max_attempts`), `extension.server.*` (round-tripped opaquely).
  - Apply canonical defaults from [fr:19-orchestrator-session](../../../docs/fr/19-orchestrator-session.md) when keys are omitted: `model = "claude-opus-4-7"`, `effort = "middle"`, `max_phases = 15`, `stall_seconds = 600`, `allowed_tools` permitting Linear MCP write + `Read` + `Bash`.
  - Refuse to start at startup or retain previous policy at hot reload when any reserved key fails type / enum / range validation; log the offending key path.
  - Refuse to start when both `extension.phase.<name>.command` and `prompt_template_<name>` are declared for the same phase; log the offending phase name.
  - Refuse to start when `[judge].model`, `extension.linear_updater.*`, `extension.gates.spec.*`, `extension.gates.review.*`, or `extension.distill.*` keys are present.
  - Round-trip unknown keys under reserved namespaces without interpreting them.
  - Observable completion: unit test asserts (a) canonical defaults applied when keys omitted, (b) out-of-range `max_phases` refused with key path in error, (c) both override forms declared on same phase refused, (d) legacy namespace key refused, (e) unknown `extension.server.foo` round-trips through the loader unchanged.
  - _Requirements: 2.11, 2.12, 6.2, 6.4, 6.5, 6.7_
  - _Boundary: workflow/schema_

- [x] 4.3 Implement hot reload with last-known-good fallback
  - Watch the configured workspace-level WORKFLOW.md via `notify` (debounced).
  - On file change: re-parse + re-validate before applying. On validation failure, retain the previously valid policy and log the validation error with offending key path. On success, apply the new policy from the next ticket admission; an in-flight orchestrator keeps its rendered prompt and an in-flight phase keeps its rendered prompt.
  - Observable completion: integration test starts the loader with a valid file, mutates the file to an invalid state, asserts the previous policy stays in effect and the validation error is logged; a second mutation back to valid form asserts the new policy takes effect on the next admission.
  - _Depends: 4.1, 4.2_
  - _Requirements: 6.3, 6.4_
  - _Boundary: workflow_

- [x] 4.4 Implement render contexts for orchestrator and per-phase templates
  - Render `prompt_template_orchestrator` against per-ticket render context: issue id, title, body, label list, `mode` flag (substituted in), bucketed lifecycle state. The rendered output becomes the orchestrator session's system prompt.
  - On render failure: log with offending block name; provide a deterministic fallback prompt that includes issue id, title, body, and `mode` flag so the orchestrator always receives non-empty context.
  - Render per-phase templates with phase-specific variables: issue id, target spec name (when SPEC_DRIVEN), worktree path (when applicable), mode, plus the orchestrator's `additional_context` verbatim through a stable, machine-extractable section of the prompt envelope (kept distinct from the skill body).
  - Observable completion: unit test renders `prompt_template_orchestrator` with `mode = SPEC_DRIVEN` and asserts the substituted mode appears in the output; a second test forces a render failure and asserts the deterministic fallback contains issue id + title + body + mode; a third test renders a per-phase template with a sample `additional_context` and asserts the verbatim string appears in the configured machine-extractable section.
  - _Depends: 4.1, 4.2_
  - _Requirements: 6.6, 13.4_
  - _Boundary: workflow/render_

- [ ] 5. Session and worktree managers; daemon-internal external CLIs

- [x] 5.1 Implement SessionManager (per-issue tempdir lifecycle)
  - Create per-issue session tempdir under platform user cache root (`~/Library/Caches/roki/sessions/<issue>` macOS / `~/.cache/roki/sessions/<issue>` Linux) on entry to `Pending`; idempotent on subsequent verification.
  - Use the session tempdir as the orchestrator session's CWD.
  - Remove the session tempdir on `Cleaning` after worktree cleanup completes.
  - Retain the session tempdir on every non-`AwaitingLinear` `Inactive(reason=*)` until the operator manually closes the Linear ticket.
  - Reject any session path that, after sanitization, escapes its expected root, contains traversal segments, or collides with another active issue's session.
  - Observable completion: unit test asserts (a) tempdir creation under the expected platform cache root, (b) idempotent re-entry does not error, (c) `Cleaning` removes the tempdir, (d) `Inactive(orphan)` retains it, (e) crafted issue ids attempting traversal are rejected.
  - _Requirements: 4.6, 4.8, 4.11, 10.5_
  - _Boundary: session_

- [x] 5.2 (P) Implement WtTool trait and RealWt shellout
  - Define `WtTool` trait: `switch_create(repo_path, branch) -> PathBuf` (computes sibling worktree path `{repo_path}/../{repo_name}.{branch}`), `list(repo_path) -> Vec<WorktreeEntry>` (or `git worktree list --porcelain` fallback), `remove(worktree_path)`.
  - Branch sanitization: characters outside `[A-Za-z0-9_-]` mapped to `-`.
  - Daemon-internal only — never reachable from inside any agent subprocess.
  - Observable completion: unit test against a temp git repo asserts `switch_create("ENG-42")` produces the sibling-path worktree on the issue-id branch, `list` returns it, `remove` deletes the worktree without deleting the branch; a second test asserts a branch name with disallowed characters is sanitized before invocation.
  - _Requirements: 4.6_
  - _Boundary: exec/wt_

- [x] 5.3 (P) Implement GhqTool trait and RealGhq shellout
  - Define `GhqTool` trait: `list_path(ghq_id) -> Option<PathBuf>` (`ghq list -p` lookup), `ensure_cloned(ghq_id) -> PathBuf` (lookup-or-clone via `ghq get`).
  - Daemon-internal only — never reachable from inside any agent subprocess.
  - Observable completion: unit test asserts `list_path` returns `Some(_)` for a present repo and `None` for a missing one; integration test executes against a fixture ghq root and asserts `ensure_cloned` returns a usable path on the second call without re-cloning.
  - _Requirements: 4.6, 10.1_
  - _Boundary: exec/ghq_

- [x] 5.4 Implement WorktreeManager idempotent ensure + cleanup via allowlist iteration
  - Implement `WorktreeManager::ensure(issue, repo_id) -> PathBuf`: on first invocation, validate `repo_id` against `[[repos]]` allowlist, run `ghq list -p` to resolve the repo's local checkout, and run `wt switch-create <issue>` to create the sibling-pathed worktree on the issue-id branch; on subsequent invocations for the same issue, verify the worktree's continued presence via `wt list` (or fallback) and short-circuit without re-invoking `wt switch-create`.
  - On `Cleaning` entry, iterate the `[[repos]]` allowlist + `wt list` filtered by branch == issue id verbatim, and `wt remove` every match. Branches are not deleted.
  - On orchestrator-emitted repo identifiers that are out of allowlist or imply multi-repo touch, return a typed error so the orchestrator's outcome (`needs_split` or `allowlist_rejected`) maps to the matching `Inactive.reason`.
  - On filesystem error during ensure / remove / sanitization, surface as `FsPoison` so the orchestrator path can mark `Inactive(fs_poison)`.
  - Observable completion: integration test asserts (a) first ensure performs `ghq list -p` + `wt switch-create`, second ensure performs only `wt list` and short-circuits, (b) `Cleaning` removes only worktrees whose branch == issue id verbatim, (c) out-of-allowlist repo id returns the documented typed error, (d) tolerates worktrees the agent created manually with the same convention.
  - _Depends: 5.2, 5.3_
  - _Requirements: 4.5, 4.6, 4.9, 10.1, 10.2_
  - _Boundary: worktree_manager_

- [x] 5.5 Implement path sanitization + collision invariants
  - Provide a shared sanitizer that rejects paths that, after normalization, escape the expected root, contain `..` segments, are absolute when relative was expected, or sanitize-collide with another active issue's session / worktree.
  - Used by `SessionManager` and `WorktreeManager`.
  - Observable completion: unit test rejects a curated set of crafted identifiers (path traversal, absolute paths, identifiers colliding after sanitization) and accepts canonical valid identifiers.
  - _Requirements: 4.8_
  - _Boundary: session, worktree_manager_

- [ ] 6. Engine: orchestrator session and phase subprocess adapters

- [x] 6.1 Implement claude binary discovery + tokio::process primitive
  - Resolve the `claude` binary: config override (`claude_binary`) → `$PATH` discovery → hard refusal at startup with actionable remediation.
  - Build a shared `tokio::process::Command` primitive that spawns a `claude` subprocess with stdin / stdout pipes wired and signal-based termination support; consumed by both the orchestrator-session adapter and the phase-subprocess adapter.
  - Observable completion: unit test substitutes a `fake_claude` test binary and asserts (a) discovery succeeds via `--config` override, (b) discovery fails with actionable error when neither override nor `$PATH` resolves the binary, (c) the spawn primitive returns an open stdin and a line-by-line stdout reader.
  - _Requirements: 1.3_
  - _Boundary: engine/claude_

- [x] 6.2 (P) Implement StreamJsonParser
  - Convert newline-delimited stream-json from a phase subprocess into typed lifecycle events (start, tool-use, terminal `result`).
  - Preserve the raw `result.subtype` value verbatim when it does not match the daemon's compiled mapping (forwarded to the orchestrator via `phase_nonclean(unknown_subtype, raw_subtype=...)`).
  - Observable completion: unit test parses a curated stream-json transcript and asserts (a) terminal `result.subtype = success` produces a `phase_complete`-shaped lifecycle event, (b) every documented non-`success` `subtype` (`error_max_turns`, `error_during_execution`, etc.) maps to its classification, (c) an unrecognized `subtype` value is preserved verbatim under `raw_subtype`.
  - _Depends: 6.1_
  - _Requirements: 5.2, 5.9_
  - _Boundary: engine/stream_

- [x] 6.3 (P) Implement ActionParser (last-JSON-object-per-turn extractor + schema validator)
  - For each orchestrator turn (one logical batch of stdout output), extract the **last** complete JSON object after any extended-thinking block; earlier emissions are advisory progress and are written to the structured log at trace severity, not to the state machine.
  - Validate the extracted object against the published `OrchestratorAction` JSON-Schema. On schema drift: emit a `Drift` outcome that the adapter consumes to issue exactly one daemon-side reprompt with a schema reminder; on a second consecutive drift, emit a terminal `Drift` outcome that the orchestrator core routes to `Inactive(orchestrator_unparseable)`.
  - Observable completion: unit test parses a multi-emission turn (advisory progress + final action) and asserts only the final `OrchestratorAction` is returned; a second test injects an extended-thinking block before the final object and asserts the parser ignores it; a third test feeds two consecutive schema-drift turns and asserts the second produces the terminal `Drift` outcome.
  - _Depends: 2.3_
  - _Requirements: 4.7, 5.2, 5.4_
  - _Boundary: engine/orchestrator_session/action_parser_

- [x] 6.4 Implement OrchestratorSessionAdapter launch and bidirectional I/O
  - Launch one `claude --input-format stream-json --output-format stream-json` per ticket on entry to `Pending`; CWD = the per-issue session tempdir; `--settings` enforces `extension.orchestrator.allowed_tools`; filesystem sandbox pinned read-only and elicitations rejected regardless of operator overrides.
  - Render `prompt_template_orchestrator` with the per-ticket render context (issue / title / body / labels / `mode` / bucketed state) via the workflow renderer; deliver the rendered output as the system prompt input.
  - Write daemon → orchestrator JSON events to stdin (`phase_complete`, `phase_nonclean`, `daemon_directive`, `tracker_terminal`); each event is one JSON object on its own line; events are written only between phases except for `tracker_terminal` which preempts.
  - Read orchestrator stdout one turn at a time and forward to the `ActionParser`; emit `OrchestratorAction` outcomes to the orchestrator core.
  - On `Cleaning` entry: send `tracker_terminal` event, close stdin, await graceful exit within the configured shutdown window; SIGTERM if exit does not complete cleanly.
  - Capture stderr lines as warn-severity log events tagged `role=orchestrator`; append stdout + stderr to the per-issue debug file when `--debug` is enabled.
  - Observable completion: integration test launches the adapter against a `fake_claude` orchestrator stub, asserts (a) the rendered prompt contains the substituted `mode` flag, (b) writing a `phase_complete` event on stdin is followed by a parsed `run_phase` action on stdout, (c) `Cleaning` triggers stdin close + bounded SIGTERM, (d) stderr lines surface as warn-tagged log events.
  - _Depends: 6.1, 6.3, 4.4, 7.2_
  - _Requirements: 4.1, 4.2, 5.1, 5.2, 6.6, 9.6, 11.5, 11.8_
  - _Boundary: engine/orchestrator_session_

- [x] 6.5 Implement orchestrator session budget + stall + drift routing
  - Enforce `extension.orchestrator.max_phases` (default `15`): each `OrchestratorAction { action=run_phase }` consumes one slot; on exhaustion, emit a routing signal that the orchestrator core maps to `Inactive(orchestrator_budget_exhausted)` and refuses to spawn the additional phase. Daemon-internal phase replay (Task 6.10) consumes zero slots.
  - Detect orchestrator stall via `extension.orchestrator.stall_seconds` (default `600`): no stdout for that many seconds → SIGTERM the orchestrator and route to `Inactive(orchestrator_crash)`; no Linear-side notification on the daemon's behalf.
  - On a non-zero exit / SIGSEGV / non-zero exit without `action=stop`, route to `Inactive(orchestrator_crash)`. On second consecutive schema drift (after one daemon-side reprompt), route to `Inactive(orchestrator_unparseable)` with raw stdout captured in the structured log.
  - Observable completion: integration test seeds `max_phases = 2` and asserts the third `run_phase` directive routes to `Inactive(orchestrator_budget_exhausted)` without spawning the phase; a second test stalls the fake orchestrator for `stall_seconds + 1` and asserts SIGTERM + `Inactive(orchestrator_crash)`; a third test drives two consecutive schema-drift turns and asserts `Inactive(orchestrator_unparseable)` with raw stdout captured.
  - _Depends: 6.4_
  - _Requirements: 4.7, 5.3, 5.4, 5.5_
  - _Boundary: engine/orchestrator_session/budget_

- [x] 6.6 Implement OverrideResolver (per-phase override lookup with mutual exclusivity)
  - For each phase nomination, resolve from `WORKFLOW.md`: `extension.phase.<name>.command` (slash-command swap; daemon launches `claude -p '<command>'`) vs `prompt_template_<phase>` named template block (daemon-internal Liquid template; daemon launches `claude --input-format stream-json` and writes the rendered prompt to stdin). The two forms are mutually exclusive per phase (refused at startup or retained-as-previous at hot reload).
  - Independent additive scalars: `extension.phase.<name>.max_turns`, `extension.phase.<name>.stall_seconds`, `extension.phase.<name>.max_attempts`. May coexist with either prompt-override form.
  - Absent either prompt-override form, return the catalog default per `(phase, mode)`.
  - Observable completion: unit test asserts (a) `command`-only override returns the slash-command form, (b) `prompt_template_<phase>`-only override returns the templated-stdin form, (c) both declared returns the documented refusal / retain-previous decision, (d) neither returns the catalog default, (e) scalar overrides apply on top of any prompt-override choice.
  - _Depends: 4.2_
  - _Requirements: 6.7_
  - _Boundary: engine/phase_subprocess/override_

- [x] 6.7 Implement PhaseSubprocessAdapter spawn + invocation + classify pinning
  - Spawn one bounded `claude` subprocess per `OrchestratorAction { action=run_phase, phase, additional_context, ... }`. Resolve invocation per `(phase, mode)` via `PhaseCatalog`; apply per-phase override via `OverrideResolver`.
  - Render the per-phase context envelope: issue id, target spec name (when SPEC_DRIVEN), worktree path (None for `classify`; required for every other phase), mode, plus the orchestrator's `additional_context` verbatim through the engine adapter's stable forwarding section ([Req 13.4]).
  - Apply `--max-turns` per the catalog default unless overridden by `extension.phase.<name>.max_turns`. The `classify` phase invocation is `claude -p '/roki-classify <ticket-context>' --max-turns 5`.
  - Apply phase-subprocess permission strategy via `Permissions` (workspace-write + reject elicitations default; `--settings` allowlist or `--dangerously-skip-permissions` fallback). The `classify` phase is additionally pinned to `Read` + `Glob` + `Grep` only per [fr:11-agent-tool-boundary](../../../docs/fr/11-agent-tool-boundary.md), regardless of operator's broader phase-subprocess sandbox.
  - Observable completion: integration test spawns each `(phase, mode)` pair against a `fake_claude` phase stub and asserts (a) the resolved invocation matches the catalog or the override, (b) the rendered envelope contains the verbatim `additional_context` in the documented section, (c) `classify` is spawned with `--max-turns 5` and the pinned `Read+Glob+Grep` allowlist and no `worktree_path`, (d) every other phase receives a non-`None` `worktree_path`.
  - _Depends: 6.1, 6.2, 2.5, 6.6, 5.4, 7.1, 7.3, 4.4_
  - _Requirements: 4.4, 5.6, 5.12, 6.7, 7.1, 9.1, 9.2, 9.3, 9.4, 13.4_
  - _Boundary: engine/phase_subprocess_

- [x] 6.8 Implement PhaseSubprocessAdapter exit translation + per-phase stall
  - Translate the phase subprocess's terminal stream-json `result` event:
    - `subtype = success` → `phase_complete { phase, result_payload, pr_url? (open_pr), review_artifact_path? (finalize_review), classify.* (classify) }`.
    - Any documented non-`success` `subtype` (`error_max_turns`, `error_during_execution`, …) → `phase_nonclean(non_success_subtype, raw_subtype=...)`.
    - Unknown `subtype` → `phase_nonclean(unknown_subtype, raw_subtype=<verbatim>)`. Daemon does not unilaterally route to `Inactive`.
    - Non-zero exit / signal / per-phase stall-detected SIGTERM / `--max-turns` exhausted → `phase_nonclean(<classification>)`.
  - Detect per-phase stall via `extension.phase.<name>.stall_seconds` (default `120` for every phase; per-phase override allowed). On exceed: SIGTERM the phase subprocess, emit `phase_nonclean(stall)`. If the orchestrator is dead at detection time, route to `Inactive(stall)` and surface via TUI escalation queue only.
  - **Tracker-terminal exception**: when the phase subprocess exit was caused by a daemon-issued SIGTERM in response to a tracker-terminal observation, do NOT translate the exit into `phase_complete` / `phase_nonclean`. Capture in the structured log only and let the orchestrator core deliver `tracker_terminal` solo (Task 8.7).
  - Capture stderr as warn-severity log events tagged `role=phase:<phase-name>`; append stdout + stderr to the per-issue debug file when `--debug` is enabled.
  - Emit a structured completion log event recording role, duration, parsed outcome (when parseable), and issue id on every phase exit.
  - Observable completion: integration test runs the adapter against a `fake_claude` phase stub for each documented exit path and asserts (a) every classification translates to the correct `DaemonEvent` variant, (b) unknown `subtype` is forwarded with `raw_subtype` preserved, (c) per-phase stall SIGTERMs and emits `phase_nonclean(stall)`, (d) tracker-terminal-induced SIGTERM does NOT emit a translated event, (e) a completion log event is recorded for every exit.
  - _Depends: 6.7, 6.2_
  - _Requirements: 5.7, 5.8, 5.9, 11.5, 11.8_
  - _Boundary: engine/phase_subprocess_

- [x] 6.9 Implement RetryPolicy daemon-internal replay loop
  - On `phase_nonclean` (non-`stall`, non-`max_turns_exhausted` classifications): drive `Active → Backoff → Active` daemon-internal replay using the same `PhaseLaunchContext` (phase / mode / additional_context / worktree_path / max_turns) until the ticket-level `max_attempts` budget is exhausted (default `3`, range `1..10`; configurable via `extension.phase.<name>.max_attempts`).
  - Apply exponential backoff bounded between 10 s and 5 min between attempts; tunable backoff floor for tests.
  - Replays consume **zero** `extension.orchestrator.max_phases` slots (no `phase_nonclean` re-delivery; no fresh `run_phase` nomination during replay).
  - Retain the session tempdir and worktree across retries.
  - On `max_attempts` exhaustion: emit a single `daemon_directive(retry_exhausted)` to the orchestrator session if alive (the orchestrator emits `action=stop outcome=failure`); if the orchestrator is dead at exhaustion, route to `Inactive(retry_exhausted)` directly.
  - On `phase_nonclean(stall)` and `phase_nonclean(max_turns_exhausted)`: bypass replay; emit `phase_nonclean` to the orchestrator (if alive) for routing, or route to `Inactive(stall)` / `Inactive(retry_exhausted)` directly if dead.
  - Observable completion: integration test seeds `max_attempts = 2` plus a backoff floor of 50 ms, drives two consecutive `phase_nonclean(non_zero)` exits, asserts (a) each replay re-spawns the same `PhaseLaunchContext` and consumes zero `max_phases`, (b) the second exit emits `daemon_directive(retry_exhausted)` to the live orchestrator stub, (c) the backoff between attempts respects the configured exponential curve; a second test asserts `phase_nonclean(stall)` bypasses replay regardless of remaining budget.
  - _Depends: 6.8_
  - _Requirements: 5.7, 5.10_
  - _Boundary: engine/phase_subprocess/policy_

- [ ] 7. Permissions

- [x] 7.1 Implement phase-subprocess permission strategy resolver
  - Default: `workspace-write` sandbox + rejected elicitations. Operator may override sandbox + elicitation policy via `WORKFLOW.md`.
  - Strategies: `--settings` allowlist (default; built from configuration) or `--dangerously-skip-permissions` (CLI flag / config opt-in fallback). On selection of the fallback, log the elevated-permission decision per phase launch.
  - Observable completion: unit test asserts (a) default strategy passes `workspace-write` + reject elicitations to the spawn primitive, (b) `--settings` strategy passes the configured allowlist via the documented mechanism, (c) `--dangerously-skip-permissions` strategy passes the flag and logs a per-launch warn entry.
  - _Requirements: 9.1, 9.2, 9.3, 9.4_
  - _Boundary: permissions_

- [x] 7.2 Pin orchestrator session permissions (read-only filesystem + reject elicitations)
  - Pin the orchestrator session to a read-only filesystem sandbox + rejected elicitations regardless of operator overrides.
  - Build `--settings` enforcing `extension.orchestrator.allowed_tools` (default: Linear MCP write + `Read` + `Bash`); the read-only sandbox prevents `Bash` mutations.
  - The `--dangerously-skip-permissions` fallback does NOT apply to the orchestrator session.
  - Observable completion: unit test asserts (a) the orchestrator's `--settings` payload reflects the configured `allowed_tools` and a read-only filesystem profile, (b) operator-set `--dangerously-skip-permissions` does not propagate to the orchestrator launch context, (c) elicitations are rejected regardless of `WORKFLOW.md` content.
  - _Requirements: 9.6_
  - _Boundary: permissions_

- [x] 7.3 Pin classify-phase tool surface and refuse missing strategy
  - For the `classify` phase specifically, intersect the phase-subprocess `allowed_tools` to `Read + Glob + Grep` only per [fr:11-agent-tool-boundary](../../../docs/fr/11-agent-tool-boundary.md), regardless of the operator's broader phase-subprocess sandbox.
  - Refuse to start when neither phase-subprocess permission strategy is configured (`--settings` allowlist or `--dangerously-skip-permissions` fallback); report the missing configuration with an actionable message.
  - Observable completion: unit test asserts (a) classify launch context's `allowed_tools` is `{Read, Glob, Grep}` even when broader sandbox is permissive, (b) startup refuses with the documented message when the strategy is absent.
  - _Requirements: 5.12, 7.1, 9.5_
  - _Boundary: permissions_

- [ ] 8. Orchestrator core (state machine, event bus, escalation, routing)

- [x] 8.1 Implement per-issue Orchestrator actor with state ownership and outcome mapping
  - Spawn one tokio task per `IssueId`. mpsc inboxes: tracker events, action outcomes (from `OrchestratorSessionAdapter`), phase events (from `PhaseSubprocessAdapter`), daemon-directive feedback. Broadcast bus out for transition events.
  - Drive transitions only from declared sources: tracker events, `OrchestratorAction` outputs, phase lifecycle events, daemon-directive deliveries, recovery scan, operator shutdown. No silent transitions.
  - On entry to `Pending` from a pre-admission-judge pass or re-admission, request `OrchestratorSessionAdapter::launch` with the per-issue render vars including `mode`. Mode is set on entry and immutable for the orchestrator-session lifetime.
  - On `OrchestratorAction { action=run_phase, phase=classify }`: spawn the phase subprocess directly (no worktree). For any other phase: call `WorktreeManager::ensure(issue, repo_id)` against the orchestrator-supplied repo id (validated against `[[repos]]`) before spawning.
  - On `OrchestratorAction { action=stop, outcome=O }`: record the terminal outcome and map to `Inactive.reason` per design ([fr:04-state-machine-and-recovery]):
    - `success` / `cancelled` → `Inactive(awaiting_linear)`.
    - `failure` → `Inactive(retry_exhausted)`.
    - `needs_operator` → `Inactive(needs_operator)`.
    - `spec_incomplete` → `Inactive(spec_incomplete)`.
    - `needs_split` → `Inactive(needs_split)`.
    - `allowlist_rejected` → `Inactive(allowlist_rejected)`.
  - Gracefully terminate the orchestrator session on `action=stop` per [fr:19-orchestrator-session > Lifecycle](../../../docs/fr/19-orchestrator-session.md).
  - Retention rules on `Inactive(reason=*)`: every non-`AwaitingLinear` reason preserves the worktree, branch, and session tempdir until the operator manually closes the Linear ticket; `Inactive(needs_operator)` and `Inactive(spec_incomplete)` are explicitly not auto-cleanup eligible.
  - Never call Linear write APIs and never invoke `gh` / `git` directly from the daemon process.
  - Observable completion: integration test drives a curated event sequence (`Admit{mode=SPEC_DRIVEN}` → `run_phase=classify` rejected as illegal in SPEC_DRIVEN → `run_phase=implement` accepted with worktree ensure → `phase_complete` → `run_phase=open_pr` → `phase_complete` → `stop{outcome=success}`) and asserts the resulting `Inactive(awaiting_linear)` plus the published transition events; a second test asserts every documented `outcome → Inactive.reason` mapping; a third test asserts mode immutability across orchestrator-session lifetime.
  - _Depends: 2.1, 6.4, 6.7, 5.4, 4.4_
  - _Requirements: 4.1, 4.10, 4.11, 5.6, 5.11, 8.1, 8.2_
  - _Boundary: orchestrator/core_

- [x] 8.2 Implement EventBus + SubscriberHooks (read-only, error-isolated)
  - Single tokio broadcast channel for `TransitionEvent`. Bounded channel with drop-newest-on-full and a logged drop counter per subscriber.
  - `SubscriberHooks` registry: `subscribe(subscriber: Arc<dyn TransitionSubscriber>) -> SubscriptionHandle`. There are NO vetoable transitions — subscribers observe read-only.
  - Subscriber failure on a transition is logged with the subscriber identifier and isolated; remaining subscribers still receive the event.
  - Observable completion: unit test asserts (a) two registered subscribers both observe a transition event in order, (b) one subscriber that returns an error does not prevent the other from receiving subsequent events, (c) the dropped-event counter increments and logs when the channel is saturated.
  - _Requirements: 8.2, 8.3, 8.4, 13.2_
  - _Boundary: orchestrator/events, orchestrator/hooks_

- [x] 8.3 Implement EscalationQueue (latest entry per issue)
  - In-memory queue keyed by `IssueId`. Each entry: `EscalationEntry { issue, repo?, kind: EscalationKind, correlation_id, timestamp, structured_fields }` where `EscalationKind ∈ { PhaseStall, RetryExhausted, FsPoison, Orphan, OrchestratorCrash, OrchestratorUnparseable, OrchestratorBudgetExhausted }`.
  - Daemon-detected failures with the orchestrator alive: enqueue and forward as `daemon_directive` to the orchestrator's stdin (Task 8.5).
  - Daemon-detected failures with the orchestrator dead (the three orchestrator-dead reasons + `Stall` and `Orphan` when no orchestrator exists at detection time): enqueue without Linear-write attempt; structured log + TUI escalation queue snapshot only.
  - Observable completion: unit test enqueues each `EscalationKind`, asserts the latest entry per issue id replaces older entries, and asserts the queue snapshot returned by read access is a plain copy (no mutation rights).
  - _Requirements: 12.1, 12.3_
  - _Boundary: orchestrator/escalation_

- [x] 8.4 Implement OrchestratorRead trait and snapshot projection
  - Define `OrchestratorRead { snapshot() -> SnapshotResponse, issue(&IssueId) -> Option<IssueState>, escalation_queue() -> Vec<EscalationEntry> }`.
  - `IssueState` snapshot exposes the current `WorkerState` (with `Inactive.reason` discriminator), the per-issue `mode` (when in any non-terminal state), the most recently observed Linear state snapshot.
  - Grants no state-mutation rights through this trait.
  - Observable completion: unit test asserts (a) `snapshot` returns the expected projection for a seeded set of issue ids, (b) the trait API exposes no setter / mutator, (c) `escalation_queue` returns the in-memory queue from Task 8.3 in deterministic order.
  - _Depends: 8.3_
  - _Requirements: 12.1, 13.1_
  - _Boundary: orchestrator/read_

- [x] 8.5 Implement daemon_directive routing (live-orchestrator + three-orchestrator-dead)
  - For daemon-detected failures with the orchestrator alive (phase stall after stall detection, retry-budget exhaustion, fs poison, recovery orphan), build a `daemon_directive` event with `kind` + structured fields (no Linear text) and write it to the orchestrator's stdin via `OrchestratorSessionAdapter`. The orchestrator returns `action=linear_update_done`; on partial Linear writes (a subset of expected `linear_writes` returned), log the partial-write entry, retain the escalation entry, and do not retry on the orchestrator's behalf.
  - Do NOT send `daemon_directive` for events the orchestrator self-reports through Linear (normal phase completions, agent-recoverable errors, operator-facing pre-phase stops `outcome ∈ {needs_split, allowlist_rejected, spec_incomplete, needs_operator}`).
  - For the three orchestrator-dead `Inactive.reason` values (`orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted`), do NOT attempt a Linear write. Enqueue an escalation entry, log structurally, surface via TUI escalation queue snapshot only. These are not auto-cleanup eligible — preserve worktree + session tempdir until operator closes the ticket.
  - If the orchestrator fails entirely on a `daemon_directive` (turn ends with error or orchestrator crashes mid-turn), log the failure and route to `Inactive(orchestrator_crash)` while retaining the escalation queue entry.
  - Never include the Linear API token, the webhook secret, or any operator-declared secret in directive payloads.
  - Observable completion: integration test asserts (a) phase-stall detection with a live orchestrator delivers `daemon_directive(phase_stall)` and consumes the orchestrator's `action=linear_update_done`, (b) retry-budget exhaustion delivers `daemon_directive(retry_exhausted)` and the orchestrator emits `action=stop outcome=failure`, (c) the three orchestrator-dead reasons enqueue an escalation entry without delivering any directive, (d) operator-facing `outcome=needs_operator` does not trigger a `daemon_directive`, (e) a partial `linear_writes` array logs the partial write and retains the escalation entry.
  - _Depends: 6.4, 8.3_
  - _Requirements: 4.12, 12.2, 12.3, 12.4, 12.5, 12.6, 12.7_
  - _Boundary: orchestrator/escalation, orchestrator/core_

- [x] 8.6 Implement tracker_terminal preemption + Cleaning entry
  - Treat assignment loss / `roki:ready` removal / Linear terminal state as daemon-side stop conditions: SIGTERM the orchestrator session and any in-flight phase subprocess, await their exit, **discard the resulting phase-exit translation** without translating it into `phase_complete` / `phase_nonclean`, and deliver `tracker_terminal` solo to the orchestrator's stdin so its next turn returns `action=stop outcome=cancelled`.
  - Enter `Cleaning` from `Pending` / `Active` / `Backoff` / `Inactive` (from `Inactive` only when the Linear state is observed terminal for non-`AwaitingLinear` reasons). A phase subprocess exit alone shall never trigger entry to `Cleaning`.
  - In `Cleaning`: invoke `WorktreeManager` cleanup via allowlist iteration filtered by branch == issue id verbatim; remove the per-issue session tempdir after worktree cleanup completes; do not delete branches; no retry budget consumed.
  - Observable completion: integration test asserts (a) tracker-terminal observation mid-`implement` SIGTERMs the phase, drops the phase exit translation, delivers `tracker_terminal` solo, the orchestrator returns `action=stop outcome=cancelled`, the issue enters `Cleaning`, the worktree branch == issue id is removed, and the session tempdir is removed; (b) phase subprocess exit alone does not trigger `Cleaning`.
  - _Depends: 5.4, 5.1, 6.4, 6.7_
  - _Requirements: 4.9, 8.1_
  - _Boundary: orchestrator/core, worktree_manager, session_

- [x] 8.7 Implement fs_poison routing
  - On any session tempdir / worktree creation, removal, rename, or sanitization failure, mark the issue as `Inactive(fs_poison)`, log with the offending path, deliver `daemon_directive(fs_poison)` to the orchestrator session if alive, and refuse further work for the issue until operator intervention. If the orchestrator is dead at detection, surface via structured log + TUI escalation queue snapshot only.
  - Observable completion: integration test forces a worktree-creation failure (read-only fs simulation), asserts (a) the issue lands in `Inactive(fs_poison)`, (b) `daemon_directive(fs_poison)` is delivered to the live orchestrator stub, (c) subsequent work for the issue is refused, (d) the failure is structurally logged with the offending path.
  - _Depends: 5.4, 5.1, 8.5_
  - _Requirements: 4.12_
  - _Boundary: orchestrator/core, worktree_manager, session_

- [ ] 9. Restart recovery (without persistent storage)

- [x] 9.1 Implement RecoveryReconciler scan (sessions + worktrees)
  - On daemon start, walk every session tempdir under the platform user cache root (`~/Library/Caches/roki/sessions/` macOS / `~/.cache/roki/sessions/` Linux). Each name is an `IssueId`.
  - For every configured `[[repos]]` entry, resolve the local checkout via `ghq list -p` and run `wt list` (or `git worktree list --porcelain` fallback) filtered by branches matching the operator-configurable issue-id regex (default `^[A-Z]+-\d+$`).
  - Produce the union of distinct `IssueId` values for the reconciliation phase.
  - Observable completion: integration test seeds (a) a session tempdir without a worktree, (b) a worktree without a session tempdir, (c) both, (d) neither — and asserts the reconciler discovers each distinct issue id with the correct presence flags.
  - _Depends: 5.1, 5.4, 5.3_
  - _Requirements: 8.5, 10.1_
  - _Boundary: orchestrator/recovery_

- [x] 9.2 Implement 5-cell decision matrix + pre-admission re-application + mode recomputation
  - For every distinct discovered `IssueId`, query Linear via the read-only client, apply `PreAdmissionJudge` (4-condition; mode recomputed from the current Linear label set), then apply the 5-cell decision matrix:
    - **ResumeActive** — pre-admission passes, session present, ≥1 worktree on disk → `Pending`; launch a fresh orchestrator session with the recomputed mode (an in-flight orchestrator never persists across daemon restarts).
    - **OrphanedSession** — session exists but pre-admission fails or no Linear active state → `Inactive(orphan)`; retain and log; deliver `daemon_directive(orphan)` to the next live orchestrator if any.
    - **OrphanedWorktree** — worktree exists but pre-admission fails or no session → `Inactive(orphan)`; retain and log.
    - **FreshQueued** — pre-admission passes, nothing on disk → `Pending` and launch a fresh orchestrator (the session tempdir is created on entry; the worktree is materialized on the first non-`classify` phase nomination via idempotent ensure).
    - **NoOp** — Linear issue terminal and nothing on disk.
  - Do not write any per-issue runtime state to disk except the session tempdir contents the agent itself produces and the structured logs the daemon emits.
  - Observable completion: integration test seeds the 5 cells against a wiremock Linear and asserts (a) each cell maps to the documented daemon state, (b) `mode` is recomputed from the current Linear label set on `ResumeActive` and `FreshQueued`, (c) `Inactive(orphan)` retains the residue on disk, (d) `daemon_directive(orphan)` is delivered to the next live orchestrator stub.
  - _Depends: 9.1, 3.4, 3.1_
  - _Requirements: 8.5, 10.2, 10.3, 10.4, 10.5_
  - _Boundary: orchestrator/recovery_

- [ ] 10. Bootstrap composition

- [x] 10.1 Wire runtime::run_with_shutdown composition order (umbrella; rolls up sub-tasks 10.1.1–10.1.6)
  - Compose the daemon in the order documented in `design.md > Daemon bootstrap` (steps 1–12): config load → secret resolve + redaction list → `[linear].assignee` resolution → `[linear].admit_states` resolution → signal handlers → external binary discovery → workflow load → `Orchestrator::with_recovery` → start single workspace-level `LinearTracker` → mount `POST /linear/webhook` on a single `axum::Router` → funnel polling + webhook through `PreAdmissionJudge` → `tokio::select!` on shutdown across orchestrator + bridge + server + tracker.
  - Pass the assembled engine adapters (`OrchestratorSessionAdapter`, `PhaseSubprocessAdapter`), `SessionManager`, `WorktreeManager`, `PermissionResolver`, `WorkflowLoader`, and `EscalationQueue` into `Orchestrator::run`. The orchestrator does not receive any agent tool factory — the daemon registers no agent-side tools (Req 7.1).
  - Observable completion: marked `[x]` only when 10.1.1–10.1.6 are all `[x]`; the e2e suite (13.1–13.12) drives the full `runtime::run_with_shutdown` against a `fake_claude` binary, a wiremock Linear, and a signed webhook posted via HTTP.
  - _Depends: 10.1.1, 10.1.2, 10.1.3, 10.1.4, 10.1.5, 10.1.6_
  - _Requirements: 1.1, 7.1, 7.2, 7.3_
  - _Boundary: runtime_

- [x] 10.1.1 Assemble engine adapters + managers in runtime::run_with_shutdown
  - In `runtime::run_with_shutdown`, after the existing workflow-load step, construct `SessionManager`, `WorktreeManager`, `PermissionResolver`, `OrchestratorSessionAdapter`, and `PhaseSubprocessAdapter` from the loaded `WorkflowPolicy`, resolved `claude` binary path, resolved `wt` + `ghq` paths, and config-derived permission strategy.
  - Hand the assembled adapters + managers to the orchestrator factory through a typed `RuntimeComponents` (or equivalent) struct so subsequent composition steps consume a single anchor.
  - Refuse to start if any adapter / manager factory returns an error; surface the offending component name in the error.
  - Observable completion: unit test against a fixture config + workflow asserts `RuntimeComponents` contains non-`None` adapters + managers; a second test forces a permission-resolver construction error and asserts startup refusal naming the component.
  - _Depends: 1.3, 4.2, 4.4, 5.1, 5.4, 5.5, 6.4, 6.7, 7.1, 7.2, 7.3_
  - _Requirements: 1.1, 7.1_
  - _Boundary: runtime_

- [x] 10.1.2 Compose Orchestrator actor map (no recovery seed yet)
  - Construct the per-issue Orchestrator actor map from the assembled `RuntimeComponents`, plus `EventBus`, `EscalationQueue`, `SubscriberHooks` registry, and the `OrchestratorRead` snapshot projection. Spawn the actor-map supervisor task; expose an `OrchestratorInbox` handle.
  - Seed the map empty (recovery wiring lands in 10.1.3).
  - Observable completion: integration test constructs the actor map via `runtime::run_with_shutdown` (with recovery scan stubbed empty), pushes a synthetic `Admit { issue, mode }` into the inbox, and asserts the orchestrator session adapter is invoked exactly once for the issue.
  - _Depends: 8.1, 8.2, 8.3, 8.4, 10.1.1_
  - _Requirements: 7.1, 7.3, 13.1, 13.2_
  - _Boundary: runtime_

- [x] 10.1.3 Wire RecoveryReconciler scan via Orchestrator::with_recovery
  - Replace the empty-seed orchestrator construction from 10.1.2 with `Orchestrator::with_recovery(...)`. Drive the 5-cell decision matrix at startup before the actor-map supervisor accepts new tracker events; seed the actor map with `ResumeActive` and `FreshQueued` issues plus `Inactive(orphan)` retentions.
  - Block the bootstrap progression past this step until the recovery scan completes (or times out per the configured window); on scan failure, refuse to start and log the offending path.
  - Observable completion: integration test seeds session tempdirs + worktrees on disk for each of the 5 cells against a wiremock Linear, starts the daemon, and asserts the actor map matches the documented daemon state per cell.
  - _Depends: 9.1, 9.2, 10.1.2_
  - _Requirements: 8.5, 10.1, 10.2, 10.3, 10.4, 10.5_
  - _Boundary: runtime_

- [x] 10.1.4 Start workspace-level LinearTracker poller
  - Construct + spawn the single workspace-level `LinearTracker` poller using the Linear read-only client, resolved `[linear].assignee`, resolved `[linear].admit_states`, and the configured cadence cap. Wire the 429 backoff state with the existing client backoff.
  - Expose a `TrackerHandle` carrying the poller's observation stream (`NormalizedIssue` events) and the `TrackerRefresh` nudge endpoint.
  - Observable completion: integration test against a wiremock Linear asserts (a) the poller emits a `NormalizedIssue` for an admit-states + assignee match within the cadence window, (b) a 429 response suspends polling for the documented backoff window, (c) `TrackerRefresh::nudge` honours throttle / backoff per Task 3.5.
  - _Depends: 3.1, 3.3, 3.5, 10.1.2_
  - _Requirements: 3.3, 3.4, 13.3_
  - _Boundary: runtime_

- [x] 10.1.5 Pipe webhook + poll observations through PreAdmissionJudge into orchestrator inbox
  - Funnel the existing `POST /linear/webhook` receiver (Task 10.3 / 3.2) and the poller observation stream (10.1.4) through `PreAdmissionJudge` (Task 3.4). Route `Admit { issue, mode }` into the orchestrator inbox (10.1.2); route `AssignmentLost` / `RokiReadyRemoved` into the dedicated channels consumed by the dedup index (Task 3.6).
  - Pre-admission failures emit `tracker.pre_admission.skipped` log events with the failed condition and drop without inbox delivery.
  - Race-safe: concurrent webhook + poll observation of the same issue must result in at most one orchestrator launch and at most one in-flight phase subprocess (Task 3.6 invariant).
  - Observable completion: integration test posts a signed webhook for an admit-passing issue, asserts the orchestrator actor receives exactly one `Admit`; a second test posts a webhook for a pre-admission-failing issue and asserts the `tracker.pre_admission.skipped` log event without inbox delivery; a third test fires concurrent webhook + poll for the same issue and asserts a single orchestrator launch.
  - _Depends: 3.2, 3.4, 3.6, 10.1.3, 10.1.4_
  - _Requirements: 3.1, 3.2, 3.7, 3.8, 3.9, 3.10, 3.11, 3.12, 3.13, 3.14_
  - _Boundary: runtime_

- [x] 10.1.6 Wire shutdown across orchestrator + tracker + reconciler + server
  - Extend the existing `tokio::select!` shutdown loop in `runtime::run_with_shutdown` to await the orchestrator actor map, the tracker poller, the webhook server, and any in-flight reconciler tasks within `SHUTDOWN_WINDOW = 30s` via `await_workers_with_window`.
  - On shutdown signal: stop accepting new tracker events first (tracker + webhook), then send each live orchestrator session a final `stop`-acknowledgement signal and close its stdin, then SIGTERM in-flight phase subprocesses, then await each within the configured per-subprocess shutdown window.
  - Observable completion: integration test starts the daemon with a fake long-running phase subprocess, fires a signed webhook for an admit-passing issue, sends SIGTERM mid-phase, and asserts (a) the daemon exits cleanly within the documented window, (b) orchestrator stdin closed, (c) phase subprocess SIGTERMed, (d) tracker + webhook stop accepting new events before orchestrator teardown begins.
  - _Depends: 1.5, 10.1.1, 10.1.2, 10.1.3, 10.1.4, 10.1.5_
  - _Requirements: 1.4, 7.1, 7.2, 7.3_
  - _Boundary: runtime_

- [x] 10.2 Implement startup binary discovery refusals
  - Refuse to start with actionable remediation messages naming the missing executable when `wt`, `ghq`, or the configured `claude` binary is not discoverable at startup.
  - Refuse to start when legacy config keys are present (`[judge].model`, `extension.linear_updater.*`, `extension.gates.*`, `extension.distill.*` in `roki.toml` or `WORKFLOW.md`).
  - Observable completion: unit + e2e test asserts a non-zero exit and an actionable log message for each missing binary scenario, and for each legacy config-key scenario.
  - _Depends: 1.3, 4.2, 6.1, 5.2, 5.3_
  - _Requirements: 1.3, 2.12_
  - _Boundary: runtime_

- [x] 10.3 Bind webhook server with port-conflict refusal
  - Mount the single `POST /linear/webhook` route on a single `axum::Router` with a single workspace-level `WebhookState`. Bind on `[server].bind:[server].port` with CLI override (`--bind`, `--port`).
  - Refuse to start (hard) on port conflict; log the offending bind address.
  - Observable completion: integration test binds the server on an in-use port and asserts a hard refusal with the offending address in the log.
  - _Depends: 3.2, 1.3_
  - _Requirements: 1.1, 2.5_
  - _Boundary: runtime_

- [x] 10.4 Add configurable Linear endpoint (config slot or test seam)
  - `runtime::bootstrap` currently hardcodes `LinearClient::new(DEFAULT_LINEAR_ENDPOINT, api_token)`. Add EITHER (a) a `[linear].endpoint` config slot (default `DEFAULT_LINEAR_ENDPOINT`, validated as a non-empty URL) consumed by `bootstrap`, OR (b) a `runtime::testing` seam that injects a pre-built `Arc<LinearClient>` into the production composition path so e2e tests can redirect `viewer()` and `list_issues()` against a wiremock without modifying production paths.
  - Pick one approach (the config slot is the more durable choice; the test seam is the lower-cost choice). Document the rationale in the commit message.
  - Update `docs/reference/config.md` if the config-slot path is chosen.
  - Observable completion: integration test against a wiremock Linear redirects the production `bootstrap` viewer + list_issues lookups to the mock; assertion confirms the daemon resolves `me` against the mock and the recovery scan + poller hit the mock instead of the real Linear endpoint.
  - _Depends: 1.3, 3.1, 10.1.3, 10.1.4_
  - _Requirements: 2.13, 3.4_
  - _Boundary: runtime, config (if config-slot path), tracker/linear (if endpoint accessor needed)_

- [x] 10.5 Expose shutdown trigger via runtime::testing seam
  - `runtime::bootstrap` constructs `(ShutdownSignal, ShutdownTrigger)` and immediately consumes the trigger inside `install_signal_handlers`. Add a `runtime::testing::run_with_env_and_trigger` (or equivalent shape — `bootstrap_for_test` returning the `Bootstrapped` struct + a clonable `ShutdownTrigger`) so e2e tests can fire shutdown without sending SIGINT to the test harness process.
  - Production path is unchanged; the seam replaces the `install_signal_handlers` call only when the test entry is used.
  - Observable completion: integration test composes the runtime via the new seam, fires the trigger, and asserts `serve()` returns Ok within `SHUTDOWN_WINDOW + 1s` without sending any OS signals.
  - _Depends: 1.5, 10.1.6_
  - _Requirements: 1.4_
  - _Boundary: runtime_

- [x] 10.6 Wire `--debug` + `[debug].dir` into engine adapters via DebugSinkFactory
  - `runtime::bootstrap` reads `RunArgs.debug` + `[debug]` config but never threads the per-issue debug sink into the engine launch contexts. `OrchestratorEngineImpl::launch` and `PhaseSubprocessAdapter::launch` (or its caller) hardcode `debug_sink: None`.
  - Add a `DebugSinkFactory` runtime-level type (or extend the existing `logging::PerIssueDebugSink` surface) that, given an `IssueId`, returns a sink writing to `<debug_dir>/<issue>.log` per Req 11.5/11.7. Plumb the factory through `RuntimeComponents` (or a sibling holder) into both engine adapters' launch contexts.
  - On per-issue debug file open / append failure, log the failure with the offending path and continue running the subprocess (per existing Task 1.4 contract).
  - Observable completion: integration test enables `--debug` + a temp `[debug].dir`, drives an admit-passing issue end-to-end, asserts `<debug_dir>/<issue>.log` exists AND contains at least one line in the documented `[STDOUT|STDERR]` + role-tag + RFC 3339 nanosecond timestamp format.
  - _Depends: 1.4, 10.1.1_
  - _Requirements: 1.5, 11.2, 11.3, 11.4, 11.6, 11.7_
  - _Boundary: runtime, engine/orchestrator_session, engine/phase_subprocess (debug-sink threading only; no behavior change to subprocess launch otherwise)_

- [ ] 11. Reference docs of record (technical contracts consumed by code + tests)

- [x] 11.1 (P) Author docs/reference/cli.md
  - Document the canonical CLI surface: `roki run` subcommand and every flag (`--config`, `--bind`, `--port`, `--dangerously-skip-permissions`, `--debug`); `--help` output shape.
  - Surface the file path in `--help` and the startup banner (Task 10.1 step 7).
  - Observable completion: file exists at `docs/reference/cli.md` with the documented flag table; doc-graph validator (`roki-doctools validate`) passes; `cargo run --bin roki -- --help` references the doc path.
  - _Requirements: 1.6, 1.7_
  - _Boundary: docs/reference_

- [x] 11.2 (P) Author docs/reference/config.md
  - Document the canonical config-key reference: `[linear]`, `[workflow]`, `[server]`, `[debug]`, `[[repos]]`, plus reserved `extension.orchestrator.*`, `extension.phase.<name>.*`, `extension.server.*` namespaces with type / range / default per key.
  - Document the legacy keys explicitly refused at startup: `[judge].model`, `extension.linear_updater.*`, `extension.gates.*`, `extension.distill.*`.
  - Observable completion: file exists with each documented key; the loader (Task 1.3 + 4.2) cross-references this doc on validation errors; doc-graph validator passes.
  - _Requirements: 2.13, 2.11, 2.12_
  - _Boundary: docs/reference_

- [x] 11.3 (P) Author docs/reference/log-events.md
  - Document the canonical structured event catalog: every event the daemon emits (pre-admission evaluation, orchestrator lifecycle, phase lifecycle, session-tempdir, worktree, Linear poll / webhook, backoff / stall, retry attempt, `daemon_directive` deliveries, orchestrator response, state-machine transition).
  - Each event row: name, severity, fields (incl. `issue`, `repo?`, `correlation_id`, `mode?`, `inactive_reason?`).
  - Observable completion: file exists with each event the implementation emits cross-referenced; doc-graph validator passes.
  - _Requirements: 1.5, 11.1_
  - _Boundary: docs/reference_

- [x] 11.4 (P) Author docs/reference/artifacts.md
  - Document the canonical `review.md` schema (consumed by the orchestrator's structural validation flow) per [fr:18-worker-skill-workflow](../../../docs/fr/18-worker-skill-workflow.md): per-criterion entry shape, criterion id source per mode (SPEC_DRIVEN: `requirements.md` numeric IDs; NEEDS_CLASSIFY: ticket body EARS numbers), code-reference reachability requirements.
  - Observable completion: file exists with the schema documented; doc-graph validator passes; the orchestrator's `prompt_template_orchestrator` references the doc path for the validation contract.
  - _Requirements: 4.4_
  - _Boundary: docs/reference_

- [x] 11.5 (P) Author docs/reference/extension-surface.md
  - Document the published extension contracts consumed by `roki-observability`: `OrchestratorRead` (per-issue snapshot + escalation-queue snapshot, read-only), `TrackerRefresh` (out-of-cycle nudge, throttle / backoff aware), `WorkflowPolicy.extension` reserved namespaces (`extension.orchestrator.*` / `extension.phase.<name>.*` / `extension.server.*`), engine-adapter `additional_context` channel, removal of legacy seams (`Registry::register`, `prompt_template_setup`, `prompt_template_worker`, pre-cleanup hook, `Skipped` / `Judging` states).
  - Observable completion: file exists with each contract documented; doc-graph validator passes; downstream `roki-observability` spec references the doc.
  - _Requirements: 13.1, 13.2, 13.3, 13.4_
  - _Boundary: docs/reference_

- [x] 11.6 (P) Author WORKFLOW.example.md (bundled default)
  - Bundle a default `WORKFLOW.example.md` declaring the four required template blocks (`prompt_template_orchestrator`, `prompt_template_implement_direct`, `prompt_template_validate_direct`, `prompt_template_open_pr`).
  - Each template block contains a documented prompt body that exercises every render variable (`mode` for orchestrator; `additional_context` for direct-mode phases and `open_pr`).
  - Observable completion: file exists at the project root; the workflow loader test fixtures consume it; loading it with the canonical defaults (Task 4.2) produces a valid `WorkflowPolicy`.
  - _Requirements: 2.15, 6.1, 6.6_
  - _Boundary: workspace root_

- [ ] 12. Unit and integration tests

- [x] 12.1 (P) Write unit_pre_admission test
  - Cover the 4-condition truth-table (16 rows) over `assignee × linear_state × roki:ready × roki:impl`; assert silent skip with the failed condition logged on every failing row; assert `mode = SPEC_DRIVEN` only when both `roki:ready` and `roki:impl` are present, `mode = NEEDS_CLASSIFY` only when `roki:ready` alone is present.
  - Cover `me` resolution success / ambiguity (multiple users) / failure (no users); assignment-loss + `roki:ready`-removal mid-flight signal emission.
  - Observable completion: `cargo test unit_pre_admission` passes with all 16 + ancillary scenarios green; running test with a tampered `me` resolution asserts the documented configuration error.
  - _Depends: 3.4_
  - _Requirements: 2.14, 3.1, 3.7, 3.8, 3.9, 3.10_
  - _Boundary: tracker/pre_admission_

- [x] 12.2 (P) Write unit_action_parser test
  - Cover last-JSON-object-per-turn extraction across multi-emission turns (advisory progress + final action); ignored extended-thinking blocks; schema validation of `OrchestratorAction` (every `action / phase / outcome` enum value; bounded `reason` length); reprompt-once on first drift; second drift produces terminal `Drift` outcome.
  - Observable completion: `cargo test unit_action_parser` passes; a canonical multi-turn transcript fixture exercises every documented action variant.
  - _Depends: 6.3_
  - _Requirements: 4.7, 5.2, 5.4_
  - _Boundary: engine/orchestrator_session/action_parser_

- [x] 12.3 (P) Write unit_phase_catalog test
  - Cover every `(phase, mode)` pair returns the documented default invocation and `--max-turns`; mode-illegal pairs (`classify` outside `NEEDS_CLASSIFY` first turn) rejected with a typed error.
  - Observable completion: `cargo test unit_phase_catalog` passes with every documented pair asserted.
  - _Depends: 2.5_
  - _Requirements: 5.6, 5.12_
  - _Boundary: engine/phase_subprocess/catalog_

- [x] 12.4 (P) Write unit_override_resolution test
  - Cover `extension.phase.<name>.command` only → command override applied; `prompt_template_<phase>` only → template override applied; both declared → startup refusal / hot-reload retain-previous; neither → catalog default; scalar overrides (`max_turns`, `stall_seconds`, `max_attempts`) compose with both prompt-override forms.
  - Observable completion: `cargo test unit_override_resolution` passes with every combination asserted.
  - _Depends: 6.6_
  - _Requirements: 6.7_
  - _Boundary: engine/phase_subprocess/override_

- [x] 12.5 (P) Write integration_orchestrator_session test
  - Drive the full long-lived session lifecycle against a `fake_claude` orchestrator stub: launch with `mode=SPEC_DRIVEN` and `mode=NEEDS_CLASSIFY` (two scenarios) and assert the substituted `mode` flag in the rendered prompt; deliver `phase_complete(classify)` and assert `additional_context` propagation on the next `run_phase=implement` directive; enforce `max_phases` budget exhaustion; simulate schema drift twice → `Inactive(orchestrator_unparseable)`; simulate stall → `Inactive(orchestrator_crash)`; simulate non-zero orchestrator exit without `action=stop` → `Inactive(orchestrator_crash)`.
  - Observable completion: `cargo test integration_orchestrator_session` passes; raw stdout is captured in the structured log on the unparseable case; budget-exhausted case asserts the additional phase is NOT spawned.
  - _Depends: 6.4, 6.5_
  - _Requirements: 4.1, 4.2, 4.7, 5.1, 5.2, 5.3, 5.4, 5.5_
  - _Boundary: engine/orchestrator_session_

- [x] 12.6 (P) Write integration_phase_subprocess test
  - Spawn each `(phase, mode)` pair against a `fake_claude` phase stub and assert the resolved invocation + `--max-turns` matches the catalog or the override; translate clean exit / non-`success` `subtype` / unknown `subtype` / stall / `--max-turns` exhausted / non-zero exit / signal into the documented `phase_complete` / `phase_nonclean` classifications; tracker-terminal-induced SIGTERM does NOT translate; ticket-level `max_attempts` retry-budget loop with exponential backoff bounded between 10 s and 5 min.
  - Observable completion: `cargo test integration_phase_subprocess` passes; the retry-budget scenario asserts each replay re-spawns the same `PhaseLaunchContext` and consumes zero `max_phases`; stall classification SIGTERMs the phase and emits `phase_nonclean(stall)`.
  - _Depends: 6.7, 6.8, 6.9_
  - _Requirements: 5.6, 5.7, 5.8, 5.9, 5.10, 5.12, 13.4_
  - _Boundary: engine/phase_subprocess_

- [x] 12.7 (P) Write integration_tracker test
  - Cover webhook HMAC verify (valid / tampered / missing-signature); polling cadence cap (5-min minimum between polls); 429 backoff suspends polling; `[linear].assignee` filter (admit / silent-skip); `[linear].admit_states` filter; label-set normalization for `roki:ready` / `roki:impl`; deduplication index correctness across concurrent webhook + poll observations of the same issue (single in-flight orchestrator + phase per issue id).
  - Observable completion: `cargo test integration_tracker` passes against a wiremock Linear; concurrent webhook + poll for the same issue asserts a single orchestrator launch.
  - _Depends: 3.1, 3.2, 3.3, 3.4, 3.6_
  - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5, 3.11, 3.12, 3.13_
  - _Boundary: tracker_

- [x] 12.8 (P) Write integration_workflow_loader test
  - Cover four required template blocks present (success); one required missing (startup refusal naming the block); optional `prompt_template_<phase>` blocks parsed; reserved-namespace round-trip for `extension.server.*` opaque keys; legacy-key refusal (`[judge].model`, `extension.linear_updater.*`, `extension.gates.*`); hot-reload last-known-good on validation failure; mode-flag substitution into `prompt_template_orchestrator`; deterministic fallback prompt on render failure; both override forms declared → refusal / retain-previous.
  - Observable completion: `cargo test integration_workflow_loader` passes; canonical defaults apply when keys are omitted; deterministic fallback contains issue id + title + body + mode.
  - _Depends: 4.1, 4.2, 4.3, 4.4_
  - _Requirements: 2.15, 6.1, 6.2, 6.3, 6.4, 6.5, 6.6, 6.7_
  - _Boundary: workflow_

- [x] 12.9 (P) Write integration_worktree_lifecycle test
  - Cover idempotent ensure on every non-`classify` phase nomination — first call performs `ghq list -p` + `wt switch-create <issue>`, subsequent calls verify presence via `wt list` only; `classify` phase nomination MUST NOT trigger ensure; cleanup via `[[repos]]` allowlist iteration + `wt list` filtered by branch == issue id verbatim + `wt remove`; no branch deletion; tolerate worktrees the agent created via Bash with the same convention; out-of-allowlist repo id returns the documented typed error mapped to `outcome=allowlist_rejected`.
  - Observable completion: `cargo test integration_worktree_lifecycle` passes; assertions on shellout invocations confirm the idempotent path (no second `wt switch-create`).
  - _Depends: 5.4_
  - _Requirements: 4.5, 4.6, 4.9_
  - _Boundary: worktree_manager_

- [x] 12.10 (P) Write integration_recovery test
  - Cover the 5-cell decision matrix (resume-active / orphaned-session / orphaned-worktree / fresh-queued / no-op) against a wiremock Linear; pre-admission re-application during reconciliation (mode recomputed from current label set); orphan retention on disk; `daemon_directive(orphan)` delivery to the next live orchestrator stub.
  - Observable completion: `cargo test integration_recovery` passes; each cell maps to the documented daemon state; on `ResumeActive`, a fresh orchestrator is launched (no in-flight orchestrator persists across restarts).
  - _Depends: 9.1, 9.2_
  - _Requirements: 8.5, 10.1, 10.2, 10.3, 10.4, 10.5_
  - _Boundary: orchestrator/recovery_

- [ ] 13. End-to-end tests

- [x] 13.1 (P) Write e2e_bootstrap test
  - Drive `runtime::run_with_shutdown` end-to-end against a real config, wiremock Linear, `fake_claude` binary, and an HTTP client posting a signed webhook. Assert (a) composition order completes, (b) refusals fire on missing `wt` / `ghq` / `claude`, (c) Linear token + webhook secret resolve and never appear in log output, (d) `--debug` activates the per-issue debug sink, (e) `[judge].model` in config refuses at startup.
  - Observable completion: `cargo test e2e_bootstrap` passes; refusal scenarios assert non-zero exit + actionable error message.
  - _Depends: 10.1, 10.2, 10.3_
  - _Blocked: full (a) composition-order assert + (d) `--debug` per-issue sink coverage depends on prereq tasks 10.4 (configurable Linear endpoint / test seam), 10.5 (shutdown trigger test seam), 10.6 (DebugSinkFactory wiring through engine adapters). Sub-assertions (b)/(c)/(e) covered + tracing-test (c) positive-control strengthening landed in this branch._
  - _Requirements: 1.1, 1.2, 1.3, 1.4, 11.6, 11.7_
  - _Boundary: runtime_

- [x] 13.2 (P) Write e2e_spec_driven_happy test
  - SPEC_DRIVEN end-to-end: orchestrator first turn structurally validates the target spec docs (using `Read` + `Bash` in the read-only sandbox), nominates `implement` (`/kiro-impl <target>`) → `review` → `validate` → `open_pr` → `finalize_review` → orchestrator reads `review.md` and validates → `action=stop outcome=success` → daemon maps to `Inactive(awaiting_linear)`.
  - Observable completion: `cargo test e2e_spec_driven_happy` passes; the resulting transition log shows the documented sequence and final `Inactive(awaiting_linear)`.
  - _Depends: 10.1, 11.6, 11.4_
  - _Note (10.1 complete; rewrite to use full runtime composition): Current `tests/e2e_spec_driven_happy.rs` drives the OrchHarness stub-engine seam (stub OrchestratorEngine + PhaseEngine + WorktreeOps + SessionDirOps) instead of spawning `runtime::run_with_shutdown` with real fake_claude orchestrator + phase subprocesses._
  - _Requirements: 4.3, 5.6, 5.11_
  - _Boundary: runtime_

- [ ] 13.3 (P) Write e2e_needs_classify_path_b test
  - NEEDS_CLASSIFY Path B end-to-end: `classify` returns `Path B` → `implement` (direct mode, daemon-internal `prompt_template_implement_direct` rendered with the ticket body's numbered acceptance criteria as `additional_context`) → `review` → `validate` → `open_pr` → `finalize_review` → `outcome=success`.
  - Observable completion: `cargo test e2e_needs_classify_path_b` passes; the rendered direct-mode prompt contains the verbatim `additional_context` in the documented section.
  - _Depends: 10.1, 11.6_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_needs_classify_path_b.rs` drives the OrchHarness stub-engine seam; verbatim additional_context forwarding is verified at the seam level rather than through real prompt rendering by the engine adapter._
  - _Requirements: 4.4, 5.6_
  - _Boundary: runtime_

- [ ] 13.4 (P) Write e2e_needs_classify_path_a test
  - NEEDS_CLASSIFY Path A end-to-end: `classify` returns `Path A` → orchestrator writes Linear comment + label via Linear MCP in the same turn → `action=stop outcome=needs_operator` → daemon maps to `Inactive(needs_operator)`; worktree + session preserved.
  - Observable completion: `cargo test e2e_needs_classify_path_a` passes; the daemon does NOT issue a Linear write itself; the issue's worktree and session tempdir are retained.
  - _Depends: 10.1_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_needs_classify_path_a.rs` drives the OrchHarness stub-engine seam — confirms outcome=needs_operator → Inactive(NeedsOperator) mapping but does not exercise real Linear MCP write paths because no real orchestrator session is spawned._
  - _Requirements: 4.4, 4.11, 5.11, 7.2_
  - _Boundary: runtime_

- [ ] 13.5 (P) Write e2e_phase_nonclean_retry test
  - Drive a phase non-clean exit on `implement`: first → `Active → Backoff → Active`; second non-clean exhausts `max_attempts = 2` → daemon emits `daemon_directive(retry_exhausted)` to the orchestrator → orchestrator emits `action=stop outcome=failure` → daemon maps to `Inactive(retry_exhausted)`.
  - Observable completion: `cargo test e2e_phase_nonclean_retry` passes; replays consume zero `extension.orchestrator.max_phases` slots; backoff between attempts respects the configured curve.
  - _Depends: 10.1, 6.9_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_phase_nonclean_retry.rs` injects `daemon_directive(retry_exhausted)` directly through the OrchHarness seam; does not exercise the real RetryPolicy schedule wired through runtime composition — backoff curve and max_phases isolation are unit-tested separately in src/engine/phase_subprocess/policy.rs._
  - _Requirements: 5.10, 12.2_
  - _Boundary: runtime_

- [ ] 13.6 (P) Write e2e_orchestrator_crash test
  - Force the orchestrator session to non-zero-exit without `action=stop` → daemon routes to `Inactive(orchestrator_crash)`; assert no Linear write occurs; an escalation-queue entry is present; worktree + session are preserved.
  - Observable completion: `cargo test e2e_orchestrator_crash` passes; the escalation queue snapshot via `OrchestratorRead` contains the entry; the daemon emits no Linear-related side effects.
  - _Depends: 10.1, 8.5_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_orchestrator_crash.rs` drives the OrchHarness stub-engine seam; ProcessExit is synthesized at the seam rather than produced by a real fake_claude orchestrator subprocess._
  - _Requirements: 12.3_
  - _Boundary: runtime_

- [ ] 13.7 (P) Write e2e_orchestrator_unparseable test
  - Drive two consecutive schema-drift turns (after one daemon-side reprompt) → daemon routes to `Inactive(orchestrator_unparseable)`; raw stdout captured in the log.
  - Observable completion: `cargo test e2e_orchestrator_unparseable` passes; the structured log contains the raw drift payload.
  - _Depends: 10.1, 6.5_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_orchestrator_unparseable.rs` drives the OrchHarness stub-engine seam; TerminalDrift is synthesized rather than produced by a real fake_claude orchestrator emitting two consecutive schema-drift turns._
  - _Requirements: 5.4, 12.3_
  - _Boundary: runtime_

- [ ] 13.8 (P) Write e2e_orchestrator_budget_exhausted test
  - Configure `extension.orchestrator.max_phases = 2`. Drive the orchestrator stub to nominate a third phase → daemon routes to `Inactive(orchestrator_budget_exhausted)`; the additional phase is NOT spawned.
  - Observable completion: `cargo test e2e_orchestrator_budget_exhausted` passes; the assertion on the spawn primitive confirms only two phase subprocesses were created.
  - _Depends: 10.1, 6.5_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_orchestrator_budget_exhausted.rs` injects the budget-exhausted directive at the OrchHarness seam; the spawn-primitive count assertion is satisfied trivially because no real phase subprocess is spawned through this path. Real budget enforcement is tested in src/engine/orchestrator_session/budget.rs._
  - _Requirements: 5.5, 12.3_
  - _Boundary: runtime_

- [ ] 13.9 (P) Write e2e_assignment_loss test
  - Drive a webhook reporting assignment moved away mid-`implement` → orchestrator + phase terminated → `Cleaning` without retry-budget consumption → allowlist-iteration cleanup removes the worktree (branch == issue id verbatim) → session tempdir removed.
  - Observable completion: `cargo test e2e_assignment_loss` passes; the resulting transition log shows `Active → Cleaning` without entering `Backoff`; the cleaned worktree is gone but the branch is retained.
  - _Depends: 10.1, 8.6_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_assignment_loss.rs` drives the OrchHarness stub-engine seam; webhook-delivered assignment-loss is synthesized as a TrackerAssignmentLost message at the seam rather than posted via signed HTTP through the runtime webhook handler._
  - _Requirements: 3.10, 4.9_
  - _Boundary: runtime_

- [ ] 13.10 (P) Write e2e_review_md_validation_retry test
  - `finalize_review` clean exit but the orchestrator's structural validation of `review.md` reports overall `status = fail` → orchestrator re-nominates `implement` with `additional_context` populated from failing per-criterion entries → eventually `review.md` validation passes → `outcome=success`.
  - Observable completion: `cargo test e2e_review_md_validation_retry` passes; the implement re-nomination's rendered envelope contains the failing per-criterion entries verbatim.
  - _Depends: 10.1, 11.4_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_review_md_validation_retry.rs` drives the OrchHarness stub-engine seam; review.md structural validation is simulated by canned orchestrator action emissions rather than orchestrator's actual Read+Bash validation flow against an on-disk review.md._
  - _Requirements: 4.4, 13.4_
  - _Boundary: runtime_

- [ ] 13.11 (P) Write e2e_multi_repo_rejection test
  - Drive the orchestrator stub to detect a classify Path B context naming two repos OR an out-of-allowlist repo → orchestrator emits `outcome=needs_split` or `outcome=allowlist_rejected` with a Linear comment in the same turn → daemon maps to `Inactive(needs_split)` or `Inactive(allowlist_rejected)`.
  - Observable completion: `cargo test e2e_multi_repo_rejection` passes; the daemon does NOT issue a Linear write itself; the worktree is NOT materialized for an out-of-allowlist repo id.
  - _Depends: 10.1, 5.4_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_multi_repo_rejection.rs` drives the OrchHarness stub-engine seam; outcome=needs_split / outcome=allowlist_rejected are produced by stub action emissions rather than by a real orchestrator session writing a Linear comment via Linear MCP._
  - _Requirements: 4.5_
  - _Boundary: runtime_

- [ ] 13.12 (P) Write e2e_recovery test
  - Kill the daemon mid-phase (orchestrator + phase both alive); restart; assert the recovery scan reconciles sessions + worktrees + Linear; the resume-active issue gets a fresh orchestrator with `mode` recomputed from the current Linear label set; orphan paths surface via the escalation queue.
  - Observable completion: `cargo test e2e_recovery` passes; no in-flight orchestrator is persisted across restart; the fresh orchestrator's rendered prompt contains the recomputed `mode`.
  - _Depends: 10.1, 9.2_
  - _Note (10.1 complete; rewrite to use full runtime composition): `tests/e2e_recovery.rs` drives the RecoveryReconciler directly + assembles an OrchHarness; does not exercise the full daemon kill+restart cycle through `runtime::run_with_shutdown` (no real process restart, no real fresh orchestrator session)._
  - _Requirements: 8.5, 10.1, 10.2_
  - _Boundary: runtime_

## Implementation Notes

- **Mid-phase abort SIGTERM gap (10.1.6 follow-up):** when the runtime aborts an actor mid-phase via `JoinHandle::abort()` at the `SHUTDOWN_WINDOW` boundary, the held `OrchestratorSessionHandle` (`engine/orchestrator_session/adapter.rs`) and the spawned `tokio::process::Child` (`engine/claude.rs::ClaudeSpawn::spawn`) have NO `Drop` impl that issues SIGTERM, and the child is not spawned with `kill_on_drop(true)`. Production effect is unreachable today because runtime currently wires `PendingPhaseEngine` (placeholder); when a future task replaces the placeholder with the real `PhaseSubprocessAdapter`, that task MUST either set `kill_on_drop(true)` on the spawned child or implement `Drop` on the phase handle issuing SIGTERM, otherwise observable "(c) phase subprocess SIGTERMed" cannot be guaranteed under mid-phase abort.
