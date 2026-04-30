# Requirements Document

## Project Description (Input)
Developers manually shepherd Linear tickets through implementation: read ticket, set up repo, prompt the agent, watch it work, transition Linear states, open PR. The supervision burden scales linearly with ticket volume, and humans drift between tickets while waiting on the agent. roki-mvp delivers a near-one-way Linear -> PR path with guardrails that doesn't require constant minding. It is the symphony-parity vertical slice: a `roki` Rust binary running as a daemon that polls Linear (or accepts webhooks), launches an isolated workspace per ticket, runs a long-lived `claude --print --output-format stream-json` session with kiro + superpowers skills available, and observes the agent through Linear state transitions to PR open. The daemon never writes Linear, never creates PRs, never edits code; the agent does all of that via Linear MCP / `linear_graphql` proxy / `gh` CLI inside its sandbox. Multi-repo from day one, workspaces keyed `(repo, issue)`. `WORKFLOW.md` (Liquid + Markdown, hot reload) is the user-facing policy artifact. A `SPEC.md` at the repo root captures the contract language-agnostically so future ports / forks remain consistent.

## Introduction

The roki-mvp specification defines the foundational vertical slice of the roki system: a single Rust daemon that watches Linear for active tickets, allocates per-issue workspaces across multiple repositories, and supervises long-lived Claude Code subprocess sessions that perform the actual implementation work. The daemon is a passive observer of Linear state and an active controller of subprocess lifecycle and workspace filesystem state — it never mutates Linear, never opens pull requests, and never edits code. All write effects on Linear, GitHub, and the working tree are delegated to the agent running inside the subprocess sandbox.

This MVP is symphony-aligned: in-memory orchestrator with no persistent database, recovery driven by re-reading Linear and the filesystem on restart, `WORKFLOW.md` as the user-facing policy boundary, and a long-lived stdio agent session per active issue. It diverges from symphony in being multi-repo from day one (workspaces keyed by `(repo, issue)`) and in publishing stable extension points so dependent specs (roki-spec-gate, roki-review-gate, roki-observability, roki-distill-postmerge) can plug in without forking the orchestrator.

## Boundary Context

- **In scope**: language-agnostic `SPEC.md` at the repo root; Rust binary with CLI, async runtime, structured logging; `WORKFLOW.md` loader with Liquid templating, Markdown front matter, schema validation, and hot reload; in-memory state machine for per-issue worker lifecycle; Linear GraphQL client (read-only on the daemon side) with webhook receiver and polling fallback; tracker normalization (issue model, state extraction, label extraction); per-issue workspace directory lifecycle with sanitized identifiers and path-safety invariants; long-lived Claude Code subprocess adapter (launch, stream JSON event parsing, state transitions, max_turns enforcement, stall detection by event-inactivity); `linear_graphql` proxy tool exposed to the agent (one GraphQL operation per call, daemon-owned auth); bounded loops (max_turns, exponential backoff between worker invocations, continuation retry on clean exit); multi-repo support from a single daemon keyed `(repo, issue)`; configurable permission strategy (`--settings` allowlist with `--dangerously-skip-permissions` fallback); default agent sandbox set to `workspace-write` with elicitations rejected, overridable per `WORKFLOW.md`.
- **Out of scope**: any logic that writes Linear state, opens or comments on pull requests, or edits source files — the agent owns those effects; persistent state stores (SQLite or otherwise); kiro-spec gate enforcement (deferred to roki-spec-gate); kiro-review gate enforcement (deferred to roki-review-gate); HTTP API and TUI observability surfaces (deferred to roki-observability); post-merge flow-document distill sweep (deferred to roki-distill-postmerge); container or VM isolation; multi-host SSH workers; auto-merge orchestration; Windows support.
- **Adjacent expectations**: the operator installs Claude Code locally with kiro skills available as personal skills under `~/.claude/skills/kiro-*` (not vendored, not plugin-namespaced); the operator provides a Linear API token and configures one or more repository roots; the operator authors a `WORKFLOW.md` per repo or relies on the bundled default; downstream specs (roki-spec-gate, roki-review-gate, roki-observability, roki-distill-postmerge) depend on stable state-machine hooks, the agent tool registry shape, and the `WORKFLOW.md` schema published by this MVP.

## Requirements

### Requirement 1: Daemon Lifecycle and CLI

**Objective:** As an operator, I want a single `roki` binary that runs as a long-running daemon and exposes a clear CLI, so that I can start, configure, and stop the system without bespoke scripting.

#### Acceptance Criteria
1. When the operator invokes `roki run` with a valid configuration, the roki daemon shall start the orchestrator, the Linear adapter, and the workflow loader before reporting ready.
2. If the configuration file is missing, malformed, or fails schema validation, the roki daemon shall exit with a non-zero status and emit a structured log entry that names the offending field.
3. When the operator sends SIGINT or SIGTERM to a running daemon, the roki daemon shall stop accepting new work, signal each active worker subprocess to terminate, await a bounded shutdown window per worker, and exit cleanly.
4. The roki daemon shall emit structured logs through a tracing pipeline that records per-issue, per-repo, and per-worker context fields for every event it produces.
5. When the operator invokes `roki --help` or any subcommand with `--help`, the roki daemon shall print usage information that documents all configuration knobs surfaced through the CLI.

### Requirement 2: Configuration and Multi-Repo Support

**Objective:** As an operator, I want to configure one daemon to serve multiple Git repositories with shared Linear credentials, so that I can run a single roki instance across my whole project portfolio.

#### Acceptance Criteria
1. The roki daemon shall accept a configuration source that declares one or more repositories, each with its own local path, Linear team or label scope, and `WORKFLOW.md` location.
2. When two configured repositories declare overlapping Linear scopes, the roki daemon shall route each Linear issue to exactly one repository according to a deterministic precedence rule and shall log the decision.
3. If a configured repository path does not exist or is not a Git working tree, the roki daemon shall mark that repository as unhealthy, refuse to schedule work for it, and continue serving the remaining repositories.
4. The roki daemon shall key all per-issue runtime state by the tuple `(repository identifier, issue identifier)` so that the same Linear issue replicated across repositories produces independent workspaces and workers.
5. The roki daemon shall load the Linear API token from a configuration source that is not committed to the repository, and shall refuse to start if the token is absent.

### Requirement 3: Linear Tracker Integration

**Objective:** As an operator, I want roki to discover and track active Linear issues with low overhead and respect Linear's rate limits, so that the daemon stays responsive without exhausting the API quota.

#### Acceptance Criteria
1. When a Linear webhook payload arrives at the configured webhook endpoint, the roki daemon shall validate the payload signature, normalize it into the internal issue model, and update the orchestrator's in-memory state.
2. While webhook delivery is unavailable, the roki daemon shall poll Linear for active issues on a cadence that does not exceed once every five minutes per repository scope.
3. If Linear returns an HTTP 429 response, the roki daemon shall apply exponential backoff before its next request to the same endpoint and shall log the backoff window.
4. The roki daemon shall expose Linear data to the rest of the system only through a normalized issue model that includes at minimum the issue identifier, title, description, current state, label set, and team or scope identifier.
5. The roki daemon shall never issue Linear write operations from within its own process; all Linear writes must originate from the agent through the `linear_graphql` proxy tool.

### Requirement 4: Per-Issue Workspace Lifecycle

**Objective:** As an operator, I want each active Linear issue to receive an isolated working directory keyed by `(repo, issue)`, so that concurrent runs cannot collide on shared paths or leak state between tickets.

#### Acceptance Criteria
1. When an issue first transitions into an active state recognized by the orchestrator, the roki daemon shall create a workspace directory under the configured workspace root using a sanitized identifier derived from the repository and issue identifiers.
2. The roki daemon shall reject any workspace identifier that, after sanitization, escapes the workspace root, contains path traversal segments, or collides with another active workspace.
3. When an issue transitions into a terminal state recognized by the orchestrator, the roki daemon shall remove the issue's workspace directory after the associated worker has exited.
4. While a workspace exists, the roki daemon shall guarantee that the worker process for that issue runs with the workspace as its working directory.
5. If workspace creation or deletion fails, the roki daemon shall mark the corresponding worker as failed, log the filesystem error with the offending path, and refuse to start additional work for that `(repo, issue)` until the operator intervenes.

### Requirement 5: Long-Lived Claude Code Subprocess Adapter

**Objective:** As an operator, I want each active issue to be driven by a long-lived `claude --print --output-format stream-json` session whose lifecycle is observable and bounded, so that the daemon can supervise agent work without polling or blocking.

#### Acceptance Criteria
1. When the orchestrator promotes an issue to an active worker slot, the roki daemon shall launch a `claude --print --output-format stream-json` subprocess in the issue's workspace and stream its stdout as newline-delimited JSON events.
2. While a worker subprocess is running, the roki daemon shall parse each emitted JSON event into a typed lifecycle event and feed it into the per-issue state machine.
3. If a worker subprocess emits no events for longer than a configurable stall window, the roki daemon shall treat the worker as stalled, terminate the subprocess, and record a stall event for the issue.
4. The roki daemon shall enforce a configurable per-worker turn budget; once that budget is exhausted, the daemon shall stop sending further continuation prompts to that worker session for the current invocation.
5. When a worker subprocess exits cleanly with the issue still in an active state, the roki daemon shall wait one second and then attempt one continuation retry by relaunching a new subprocess for the same `(repo, issue)`.
6. When a worker subprocess exits non-cleanly or after exhausting its turn budget, the roki daemon shall apply exponential backoff before the next launch attempt for the same `(repo, issue)`, with the backoff bounded between ten seconds and five minutes.
7. The roki daemon shall pass agent-launch flags so that kiro skills are discoverable from `~/.claude/skills/kiro-*` (no `--bare`) and shall not depend on slash commands at runtime.

### Requirement 6: WORKFLOW.md Policy Loader

**Objective:** As an operator, I want a `WORKFLOW.md` file per repository that defines policy in Liquid + Markdown with schema validation and hot reload, so that I can adjust agent behavior without restarting the daemon or recompiling Rust.

#### Acceptance Criteria
1. When the roki daemon starts, the WORKFLOW.md loader shall read each configured repository's `WORKFLOW.md`, parse its YAML or TOML front matter, render its Liquid body, and validate the result against the published schema.
2. If a `WORKFLOW.md` fails schema validation at startup, the roki daemon shall refuse to schedule work for that repository and shall log the validation error with the offending key path.
3. While the daemon is running, the WORKFLOW.md loader shall watch each loaded `WORKFLOW.md` for filesystem changes and shall re-validate the file before applying any changes.
4. If a hot-reload attempt produces a `WORKFLOW.md` that fails validation, the roki daemon shall keep the previously valid policy in effect and log the validation failure.
5. The WORKFLOW.md loader shall expose its parsed policy to the orchestrator through a stable schema shape that downstream specs can extend with additional keys without breaking existing consumers.

### Requirement 7: Agent Tool Registry and `linear_graphql` Proxy

**Objective:** As the agent, I want the daemon to expose a small, audited set of tools — most importantly a `linear_graphql` proxy — so that I can read and write Linear without ever holding the Linear API token directly.

#### Acceptance Criteria
1. The roki daemon shall publish an agent tool registry whose entries each declare a stable name, an input schema, and an output schema, and shall provide that registry to every worker subprocess at launch.
2. When the agent invokes the `linear_graphql` tool with a single GraphQL operation and variables, the roki daemon shall forward the request to Linear using the daemon-owned token and shall return the response to the agent unmodified except for credential redaction.
3. If a `linear_graphql` invocation contains more than one GraphQL operation in the same call, the roki daemon shall reject the call with a structured error rather than forwarding it.
4. The roki daemon shall never embed the Linear API token in any tool input, output, or error message visible to the agent.
5. The agent tool registry shall remain extensible so that downstream specs can register additional read-only tools (for example `kiro_spec_status`, `kiro_review_status`) without breaking existing tool consumers.

### Requirement 8: Orchestrator State Machine and Extension Points

**Objective:** As a downstream spec author, I want the orchestrator to expose stable per-issue state-machine hooks, so that roki-spec-gate, roki-review-gate, roki-observability, and roki-distill-postmerge can subscribe to lifecycle transitions without forking the core.

#### Acceptance Criteria
1. The roki daemon shall maintain an in-memory state machine for each `(repo, issue)` whose states cover at minimum discovered, queued, active, awaiting-review, terminal-success, and terminal-failure.
2. When a state transition occurs, the roki daemon shall publish a structured transition event whose payload identifies the previous state, the next state, the trigger source, and the `(repo, issue)` key.
3. The roki daemon shall expose subscription hooks that allow other components to observe transition events and to veto a specific set of declared-as-vetoable transitions, and shall document which transitions are vetoable.
4. If a subscriber raises an unhandled error while processing a transition event, the roki daemon shall isolate the failure to that subscriber, log the error with the subscriber's identifier, and continue processing transitions for other subscribers.
5. The roki daemon shall recover the per-issue state on restart by re-reading Linear and the workspace directory layout, without depending on a persistent database.

### Requirement 9: Permission Strategy and Default Sandbox

**Objective:** As an operator, I want roki to run agents under the safest workable permission profile while letting me opt into a fallback when Claude's allowlist is unreliable, so that I can balance safety against operability.

#### Acceptance Criteria
1. The roki daemon shall launch each worker subprocess with the agent sandbox set to `workspace-write` and with elicitations rejected by default.
2. When `WORKFLOW.md` declares an alternative sandbox or elicitation policy, the roki daemon shall apply that override only for workers serving the corresponding repository.
3. When the operator selects the `--settings` allowlist permission strategy, the roki daemon shall pass the configured allowlist to each worker subprocess through Claude Code's settings interface.
4. When the operator selects the `--dangerously-skip-permissions` fallback strategy, the roki daemon shall pass that flag to each worker subprocess and shall log the elevated-permission decision per worker launch.
5. If neither permission strategy is configured, the roki daemon shall refuse to start and shall report the missing configuration.

### Requirement 10: Restart Recovery Without Persistent Storage

**Objective:** As an operator, I want roki to recover its working state after a restart by re-reading Linear and the filesystem rather than relying on a database, so that I never carry stale or corrupted local state across restarts.

#### Acceptance Criteria
1. When the roki daemon starts, it shall list the workspace root, match each existing workspace directory to a `(repo, issue)` key, and re-fetch the corresponding Linear issue state before resuming work.
2. If a workspace directory has no matching active Linear issue at startup, the roki daemon shall mark that workspace as orphaned and log it without deleting the directory automatically.
3. If a Linear issue is in an active state at startup but has no workspace, the roki daemon shall create the workspace and resume the normal active-state lifecycle for that issue.
4. The roki daemon shall not write any per-issue runtime state to disk except the workspace contents the agent itself produces and the structured logs the daemon emits.

### Requirement 11: Language-Agnostic SPEC.md

**Objective:** As a future maintainer or a port author, I want a `SPEC.md` at the repository root that captures roki's contract independently of Rust, so that alternative implementations remain consistent with the original.

#### Acceptance Criteria
1. The roki repository shall include a `SPEC.md` at its root that defines the daemon contract, the agent tool registry contract, the `WORKFLOW.md` schema, the per-issue state machine, and the workspace lifecycle in language-agnostic terms.
2. The SPEC.md shall describe behavior in terms that any compliant implementation can satisfy without depending on Rust-specific types, libraries, or runtime constructs.
3. When the Rust implementation changes a contract documented in `SPEC.md`, the change shall be accompanied by a corresponding `SPEC.md` update in the same change set.
4. The SPEC.md shall enumerate the extension points that downstream specs depend on so that future ports preserve them.

### Requirement 12: Observability of Daemon Internals

**Objective:** As an operator debugging a stuck ticket, I want enough structured observability from the daemon itself to diagnose worker, workspace, and tracker issues without an external UI, so that the MVP is operable before the dedicated observability spec ships.

#### Acceptance Criteria
1. The roki daemon shall emit a structured log event for every worker lifecycle change, every workspace creation or deletion, every Linear poll or webhook receipt, every backoff or stall decision, and every state-machine transition.
2. Every structured log event shall include the `(repo, issue)` key when one applies and shall include a correlation identifier for the originating worker invocation.
3. The roki daemon shall support a configurable log level and a configurable log destination so that the operator can route logs to stdout, a file, or both.
4. The roki daemon shall redact the Linear API token and any other configured secrets from every structured log event before emission.

### Requirement 13: Cross-Spec Extension Surface

**Objective:** As a downstream spec author, I want roki-mvp to publish a stable, additive extension surface, so that roki-spec-gate, roki-review-gate, roki-observability, and roki-distill-postmerge can integrate without forking the core or breaking sibling specs.

#### Acceptance Criteria
1. The roki daemon shall publish a read-only `OrchestratorRead` trait that exposes a snapshot of the per-`(repo, issue)` state plus a single-issue lookup, and shall grant no state-mutation rights through that trait.
2. The roki daemon shall expose a vetoable pre-cleanup hook between terminal success and workspace removal so that downstream specs can perform deferred work while the workspace still exists; a `Deny` decision shall block workspace removal and shall be logged.
3. The roki tracker adapter shall publish a `TrackerRefresh` nudge trait that allows external callers to request an out-of-cycle poll without bypassing the documented cadence cap or the 429 backoff state.
4. The roki engine adapter shall accept an additive optional `additional_context` field on `WorkerContext` and shall forward its value verbatim to the agent through a documented session prelude envelope, without interpreting the contents.
5. The `WORKFLOW.md` schema shall reserve the `extension.gates.spec.*`, `extension.gates.review.*`, `extension.server.*`, and `extension.distill.*` sub-namespaces for the canonical roki specs; the loader shall round-trip unknown keys under those namespaces without interpreting them.
