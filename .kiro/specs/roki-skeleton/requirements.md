---
refs:
  id: req:roki-skeleton
  kind: requirements
  title: "roki-skeleton Requirements"
  spec: roki-skeleton
  provides:
    - req:roki-skeleton#1
    - req:roki-skeleton#2
    - req:roki-skeleton#3
    - req:roki-skeleton#4
    - req:roki-skeleton#5
    - req:roki-skeleton#6
    - req:roki-skeleton#7
    - req:roki-skeleton#8
    - req:roki-skeleton#9
  related:
    - fr:01-engine-model
    - fr:02-configuration
    - fr:03-linear-admission
    - fr:04-phase-execution
    - fr:05-worktree-and-session
    - fr:12-daemon-lifecycle
    - ref:cli
    - ref:config
---

# Requirements Document

## Introduction

`roki-skeleton` establishes the walking-skeleton daemon: a single `roki` binary that boots, receives one Linear webhook, runs admission, matches one `[[rule]]`, executes a single command-form phase, captures its output, and exits. The skeleton is the smallest end-to-end backbone that every later spec must keep green via `tests/e2e/skeleton_smoke.rs`. Anything not listed here belongs to a later spec.

Naming, CLI surface, and config keys follow the canonical references `ref:cli` (`docs/reference/cli.md`) and `ref:config` (`docs/reference/config.md`). Where the brief used informal shorthand (`roki --config`, `[network].bind`), the requirements adopt the canonical form (`roki run --config`, `[linear.webhook].bind`) per `.kiro/steering/grounded-design.md` Principle 1.

## Boundary Context

- **In scope**:
  - `roki run --config <path>` CLI entry point.
  - Reading the canonical `roki.toml` sections needed for the skeleton path: `[linear]`, `[linear.webhook]`, `[default.ai.command]`, `[engine]`, `[paths]`, `[log]`.
  - Reading `WORKFLOW.toml` (located via `roki.toml [paths].workflow`) for `[admission].assignee`, the first `[[admission.repos]]` entry, and the `[[rule]]` array.
  - HTTP webhook reception on `[linear.webhook].bind` and `[linear.webhook].port` without HMAC verification.
  - Single-shot in-memory ticket cache scoped to one cycle.
  - Assignee admission filter (`[admission].assignee`, with `me` resolved via the Linear API token).
  - Single-repo resolution using only the first `[[admission.repos]]` entry, with no `when.*` matcher evaluation.
  - `[[rule]]` first-match using only `when.status` equality and `when.labels.has_all`.
  - Inline command-form phase execution (`run.cmd = "..."`).
  - Per-cycle stdout/stderr capture under `[paths].session_root`.
  - Process exit on first cycle end.
- **Out of scope**:
  - HMAC signature verification of `[linear.webhook].secret` (deferred to `roki-linear-signature-verify`).
  - Any condition vocabulary beyond `when.status =` and `when.labels.has_all` (deferred to `roki-config-workflow-toml-full`, `roki-linear-admission-repos`).
  - `[[cleanup]]` and `[[on_failure]]` evaluation (deferred to `roki-engine-cleanup-cycle`, `roki-engine-failure-cycle`).
  - Iteration loop directives (deferred to `roki-engine-iteration-loop`).
  - Session-shape phases and the `[default.ai.session]` enforcement on startup (deferred to `roki-runtime-session-mode`).
  - `pre` / `post` phases of any shape; `run.path` and `run.prompt` forms.
  - Liquid rendering of `cmd` / `path` / `prompt` (deferred to `roki-runtime-template-vars`).
  - Worktree creation and `cwd` selection rules from `fr:04` (deferred to `roki-runtime-worktree-lazy`, `roki-runtime-capture-layout`).
  - The full per-cycle / per-phase capture-file layout from `fr:04` and `fr:09` (deferred to `roki-runtime-capture-layout`).
  - Diff cache (deferred to `roki-linear-diff-cache`).
  - Polling fallback, refresh nudge, 429 backoff (deferred to later Wave 3 specs).
  - HMAC, hot reload, observability HTTP API (`[api]`), tracing pipeline, ring buffer, TUI, and any CLI subcommand other than `roki run`.
- **Adjacent expectations**:
  - Later specs may extend each scope item but must not regress the skeleton smoke test `crates/roki-daemon/tests/e2e/skeleton_smoke.rs`.
  - The skeleton does not own worktree lifecycle, the canonical capture-file layout, the structured event catalog, polling, signature verification, or recovery semantics; later specs introduce them.
  - Per `fr:02`, `[linear].token`, `[linear.webhook].secret`, `[linear.webhook].bind`, `[linear.webhook].port`, `[default.ai.session].cli`, `[default.ai.command].cli`, `[paths].workflow`, and `[paths].session_root` are required keys at the canonical schema level. The skeleton may relax `[linear.webhook].secret` and `[default.ai.session].cli` requirement enforcement so deferred specs can tighten them later without contradiction.

## Requirements

### Requirement 1: CLI Startup
**Objective:** As an operator, I want to launch the daemon with the canonical `roki run` subcommand and an explicit config path, so that the skeleton aligns with `ref:cli` from day one.

#### Acceptance Criteria
1. When the operator invokes `roki run --config <path>` with a readable file at `<path>`, the roki daemon shall start and proceed to load the configuration.
2. If `--config` is omitted or `<path>` cannot be opened, then the roki daemon shall exit with a non-zero status and emit a startup error identifying the missing or unreadable path.
3. When `roki --help` or `roki run --help` is invoked, the roki daemon shall print usage text that lists the `--config` flag together with the configuration file it identifies.

### Requirement 2: Configuration Loading
**Objective:** As an operator, I want the daemon to load only the canonical configuration sections the skeleton path needs, so that misconfiguration in deferred sections cannot block the smoke path.

#### Acceptance Criteria
1. When the configuration file is loaded, the roki daemon shall read the `[linear]`, `[linear.webhook]`, `[default.ai.command]`, `[engine]`, `[paths]`, and `[log]` sections of `roki.toml` per `ref:config`.
2. The roki daemon shall resolve `[paths].workflow` and load that file as `WORKFLOW.toml`, reading `[admission]`, `[[admission.repos]]`, and `[[rule]]`.
3. If a required field within those sections is missing or fails type validation, then the roki daemon shall exit with a non-zero status and emit a configuration error identifying the offending field.
4. Where `[default.ai.session]`, `[linear.webhook].secret`, or any other configuration key not listed above is present, the roki daemon shall accept the value without applying it during the skeleton phase.
5. Where `[[cleanup]]`, `[[on_failure]]`, or any per-repo `[[admission.repos]] workflow` override is present in `WORKFLOW.toml`, the roki daemon shall accept the configuration without evaluating those entries during the skeleton phase.

### Requirement 3: Linear Webhook Reception
**Objective:** As an operator, I want the daemon to accept a Linear webhook on the canonical receiver endpoint without signature verification, so that the smoke path can drive a cycle from a posted payload.

#### Acceptance Criteria
1. When configuration loading succeeds, the roki daemon shall bind an HTTP listener to `[linear.webhook].bind` and `[linear.webhook].port`.
2. When an HTTP POST arrives at the webhook path with a JSON body, the roki daemon shall normalize the body into the internal issue model per `fr:03` and forward it to the admission filter.
3. The roki daemon shall not verify any HMAC or signature header during the skeleton phase, even if `[linear.webhook].secret` is configured.
4. If the HTTP body cannot be parsed as a Linear webhook payload, then the roki daemon shall reject the request with a client error response (HTTP 4xx) and emit a parse-error log entry. Severity tagging is deferred to the canonical structured event catalog (`roki-obs-event-catalog`); the skeleton may use any operator-visible level.

### Requirement 4: Admission Filtering
**Objective:** As an operator, I want a minimal admission filter so that only my own tickets in a single allowed repository proceed to rule evaluation.

#### Acceptance Criteria
1. When the admission filter evaluates a ticket, the roki daemon shall accept the ticket only if the ticket assignee equals `WORKFLOW.toml [admission].assignee`.
2. Where `[admission].assignee` is the literal value `me`, the roki daemon shall resolve `me` to the authenticated user identified by `roki.toml [linear].token` before comparison.
3. When admission resolves the target repository, the roki daemon shall use the first `[[admission.repos]]` entry of `WORKFLOW.toml` only, taking its `ghq` value, and shall not evaluate any `when.*` matcher.
4. If no `[[admission.repos]]` entry is configured, then the roki daemon shall reject admission for the ticket and emit an admission error.
5. If admission rejects the ticket, then the roki daemon shall not spawn a cycle for it.

### Requirement 5: Rule First-Match
**Objective:** As an operator, I want the engine to dispatch the first matching `[[rule]]` based only on `when.status` equality and `when.labels.has_all`, so that the skeleton can route one ticket to one cycle deterministically.

#### Acceptance Criteria
1. When admission accepts a ticket, the roki daemon shall evaluate `WORKFLOW.toml [[rule]]` entries in declared order and select the first entry whose declared `when.status` equals the ticket's status and whose declared `when.labels.has_all` is fully contained in the ticket's labels.
2. The roki daemon shall treat `when.status` as a single string equality and `when.labels.has_all` as a set-containment check during the skeleton phase.
3. If a `[[rule]]` entry declares any `when.*` key other than `when.status` or `when.labels.has_all`, then the roki daemon shall reject the configuration with a validation error during loading.
4. If no `[[rule]]` entry matches, then the roki daemon shall not spawn a cycle and shall emit a no-match outcome for the ticket.
5. The roki daemon shall not evaluate `[[cleanup]]` or `[[on_failure]]` lists during the skeleton phase.

### Requirement 6: Command-Form Phase Execution
**Objective:** As an operator, I want the daemon to execute the matched rule as a single inline-command-form `run` phase, so that the skeleton stays minimal and deterministic.

#### Acceptance Criteria
1. When a `[[rule]]` matches, the roki daemon shall spawn the matched entry's `run.cmd` value as a one-shot subprocess.
2. If the matched entry declares `run.path` or `run.prompt`, or omits `run` entirely, then the roki daemon shall reject the configuration with a validation error during loading.
3. The roki daemon shall not invoke any pre or post phase during the skeleton phase.
4. The roki daemon shall not perform Liquid template rendering of `run.cmd` during the skeleton phase.
5. When the run subprocess exits, the roki daemon shall record its exit status as the cycle outcome.

### Requirement 7: Per-Cycle Output Capture
**Objective:** As an operator, I want the run subprocess's stdout and stderr captured to disk under the configured session root, so that I can inspect the cycle's output after exit.

#### Acceptance Criteria
1. When the run subprocess starts, the roki daemon shall create a per-cycle capture directory under `roki.toml [paths].session_root`.
2. While the run subprocess is executing, the roki daemon shall write its stdout to a stdout capture file and its stderr to a stderr capture file inside that per-cycle directory.
3. If the capture directory or files cannot be created or written, then the roki daemon shall fail the cycle and emit a capture error.

### Requirement 8: Single-Cycle Process Exit
**Objective:** As an operator, I want the skeleton daemon to exit cleanly after one cycle, so that the smoke test can assert end-to-end behavior without long-running state.

#### Acceptance Criteria
1. When the run subprocess of the first matched cycle exits, the roki daemon shall finalize capture files and terminate the process.
2. The roki daemon shall exit with status zero when the cycle completed without an internal daemon error, regardless of the subprocess exit code.
3. If an internal daemon error prevented cycle completion, then the roki daemon shall exit with a non-zero status.
4. Once the first admitted-and-matched cycle has begun execution, the roki daemon shall reject all subsequent webhooks (HTTP 5xx) until process exit, during the skeleton phase. (Literal "after process exit" is trivially true; this acceptance criterion captures the operational cutoff that the smoke test asserts.)

### Requirement 9: Smoke Test as Acceptance Gate
**Objective:** As a maintainer, I want a single end-to-end smoke test that exercises the skeleton path, so that every later spec is forced to preserve the backbone.

#### Acceptance Criteria
1. The roki repository shall include `crates/roki-daemon/tests/e2e/skeleton_smoke.rs` exercising the path from `roki run --config <path>` through one webhook, one admitted ticket, one matched `[[rule]]`, one inline `run.cmd` execution, captured stdout/stderr, and clean process exit. The smoke test lives inside the daemon crate's `tests/` directory because Cargo's virtual workspace does not expose a workspace-root `tests/` directory and `env!("CARGO_BIN_EXE_roki")` only resolves inside the binary's own crate.
2. When the smoke test runs against the skeleton implementation, the roki test suite shall report it as passing.
3. While later specs add functionality, the roki test suite shall continue to report `tests/e2e/skeleton_smoke.rs` as passing.
