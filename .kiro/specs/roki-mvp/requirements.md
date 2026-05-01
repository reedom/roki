# Requirements Document

## Project Description (Input)
Developers manually shepherd Linear tickets through implementation: read ticket, set up repo, prompt the agent, watch it work, transition Linear states, open PR. The supervision burden scales linearly with ticket volume, and humans drift between tickets while waiting on the agent. roki-mvp delivers a near-one-way Linear -> PR path with guardrails that doesn't require constant minding. It is the symphony-parity vertical slice: a `roki` Rust binary running as a daemon that polls Linear (or accepts webhooks), launches an isolated session per ticket, runs a long-lived `claude --print --output-format stream-json` session with kiro + superpowers skills available, and observes the agent through Linear state transitions to PR open. The daemon never writes Linear, never creates PRs, never edits code; the agent does all of that via Linear MCP / `linear_graphql` proxy / `gh` CLI inside its sandbox. The daemon configures an allowlist of repos and exposes a `roki_open_worktree` tool so the agent decides — on its first turn — which configured repo(s) the ticket actually applies to, and the daemon opens git worktrees on demand via `wt` + `ghq`. Cross-repo tickets fall out for free because the agent can call the tool multiple times. Per-issue state is keyed by Linear issue id alone. `WORKFLOW.md` (Liquid + Markdown, hot reload) is a single workspace-level policy artifact. A `SPEC.md` at the repo root captures the contract language-agnostically so future ports / forks remain consistent.

## Introduction

The roki-mvp specification defines the foundational vertical slice of the roki system: a single Rust daemon that watches Linear for active tickets, allocates per-issue sessions, and supervises long-lived Claude Code subprocess sessions that perform the actual implementation work. The daemon is a passive observer of Linear state and an active controller of subprocess lifecycle, session filesystem state, and on-demand git worktree provisioning — it never mutates Linear, never opens pull requests, and never edits code. All write effects on Linear, GitHub, and the working tree are delegated to the agent running inside the subprocess sandbox.

This MVP is symphony-aligned: in-memory orchestrator with no persistent database, recovery driven by re-reading Linear and the filesystem on restart, a single workspace-level `WORKFLOW.md` as the user-facing policy boundary, and a long-lived stdio agent session per active issue. It diverges from symphony in being multi-repo-capable from day one through agent-driven repo selection (the daemon publishes an allowlist of configured repos and exposes a `roki_open_worktree` agent tool) and in publishing stable extension points so dependent specs (roki-spec-gate, roki-review-gate, roki-observability, roki-distill-postmerge) can plug in without forking the orchestrator.

## Boundary Context

- **In scope**: language-agnostic `SPEC.md` at the repo root; Rust binary with CLI, async runtime, structured logging; single workspace-level `WORKFLOW.md` loader with Liquid templating, Markdown front matter, schema validation, and hot reload; in-memory state machine for per-issue worker lifecycle keyed by Linear issue id; Linear GraphQL client (read-only on the daemon side) with a single workspace-level webhook receiver and a single workspace-level polling fallback; tracker normalization (issue model, state extraction, label extraction); per-issue session tempdir lifecycle (the worker's working directory, ephemeral per issue) plus an agent-opened `WorktreeRegistry` of git worktrees per worker; long-lived Claude Code subprocess adapter (launch, stream JSON event parsing, state transitions, `max_turns` enforcement, stall detection by event-inactivity, configurable retry budget on non-clean exits); `linear_graphql` proxy tool exposed to the agent (one GraphQL operation per call, daemon-owned auth) and `roki_open_worktree` tool exposed to the agent (daemon resolves the configured repo via `ghq`, creates a worktree branch named after the issue id via `wt`); operator-declared allowlist of repos that constrains which targets the agent may open worktrees in; bounded loops (`max_turns`, exponential backoff between worker invocations bounded between ten seconds and five minutes, continuation retry on clean exit, configurable retry budget on non-clean exit); configurable permission strategy (`--settings` allowlist with `--dangerously-skip-permissions` fallback); default agent sandbox set to `workspace-write` with elicitations rejected, overridable through the workspace-level `WORKFLOW.md`.
- **Out of scope**: any logic that writes Linear state, opens or comments on pull requests, or edits source files — the agent owns those effects; persistent state stores (SQLite or otherwise); kiro-spec gate enforcement (deferred to roki-spec-gate); kiro-review gate enforcement (deferred to roki-review-gate); HTTP API and TUI observability surfaces (deferred to roki-observability); post-merge flow-document distill sweep (deferred to roki-distill-postmerge); container or VM isolation; multi-host SSH workers; auto-merge orchestration; per-repo `WORKFLOW.md` overrides; Linear admission pre-filtering (every admitted issue spawns a worker; the agent decides whether to do real work); Windows support.
- **Adjacent expectations**: the operator installs Claude Code locally with kiro skills available as personal skills under `~/.claude/skills/kiro-*` (not vendored, not plugin-namespaced); the operator installs the `wt` (worktrunk) and `ghq` external CLIs and ensures both are on `$PATH`; the operator provides a Linear API token and a single workspace-level Linear webhook secret; the operator configures one or more repository allowlist entries by their `ghq` identifier; the operator authors a single workspace-level `WORKFLOW.md` or relies on the bundled default; downstream specs (roki-spec-gate, roki-review-gate, roki-observability, roki-distill-postmerge) depend on stable state-machine hooks, the agent tool registry shape (including `roki_open_worktree`), and the workspace-level `WORKFLOW.md` schema published by this MVP.

## Requirements

### Requirement 1: Daemon Lifecycle and CLI

**Objective:** As an operator, I want a single `roki` binary that runs as a long-running daemon and exposes a clear CLI, so that I can start, configure, and stop the system without bespoke scripting.

#### Acceptance Criteria
1. When the operator invokes `roki run` with a valid configuration, the roki daemon shall start the orchestrator, the Linear adapter, the workflow loader, and the webhook server before reporting ready.
2. If the configuration file is missing, malformed, or fails schema validation, the roki daemon shall exit with a non-zero status and emit a structured log entry that names the offending field.
3. If the `wt` external CLI, the `ghq` external CLI, or the configured `claude` binary is not present at startup, the roki daemon shall refuse to start and shall log an actionable remediation message that names the missing executable.
4. When the operator sends SIGINT or SIGTERM to a running daemon, the roki daemon shall stop accepting new work, signal each active worker subprocess to terminate, await a bounded shutdown window per worker, and exit cleanly.
5. The roki daemon shall emit structured logs through a tracing pipeline that records per-issue and per-worker context fields for every event it produces, plus a per-repo field for events scoped to a specific repo (for example `roki_open_worktree` outcomes and worktree cleanup).
6. The roki daemon shall accept the CLI flags `--config <path>`, `--bind <addr>`, `--port <num>`, and `--dangerously-skip-permissions`, where flags override configuration values; the daemon shall document each flag in `--help` output for `roki run` and any subcommand.
7. When the operator invokes `roki --help` or any subcommand with `--help`, the roki daemon shall print usage information that documents all configuration knobs surfaced through the CLI.

### Requirement 2: Configuration and Multi-Repo Allowlist

**Objective:** As an operator, I want to configure one daemon to serve an allowlist of Git repositories with shared Linear credentials, so that I can run a single roki instance across my whole project portfolio while letting the agent choose which repo(s) a given ticket touches.

#### Acceptance Criteria
1. The roki daemon shall accept a configuration source that declares zero or more repository allowlist entries, where each entry is identified by its `ghq` repository identifier (`owner/repo` or `host/owner/repo`) and where the local clone path is resolved at runtime through `ghq`.
2. If two configured repository allowlist entries declare the same `ghq` identifier, the roki daemon shall refuse to start and shall log the offending duplicate entry.
3. The roki daemon shall accept a single workspace-level `[linear]` configuration block declaring the Linear API token source and the Linear webhook secret source, and shall refuse to start if the Linear API token or the webhook secret cannot be resolved.
4. The roki daemon shall accept a single workspace-level `[workflow]` configuration block declaring the path to the workspace-level `WORKFLOW.md`, and shall refuse to start if that path is missing or unreadable.
5. The roki daemon shall accept a `[server]` configuration block declaring the bind address and port for the webhook receiver, and shall apply CLI overrides for both fields when supplied.
6. The roki daemon shall key all per-issue runtime state by the Linear issue identifier alone, independently of any specific repository.
7. If the configured repository allowlist is empty, the roki daemon shall start with a warning and shall reject every `roki_open_worktree` invocation with a typed allowlist-rejection error.

### Requirement 3: Linear Tracker Integration

**Objective:** As an operator, I want roki to discover and track active Linear issues with low overhead and respect Linear's rate limits, so that the daemon stays responsive without exhausting the API quota.

#### Acceptance Criteria
1. When a Linear webhook payload arrives at the single workspace-level webhook endpoint, the roki daemon shall verify the HMAC signature against the workspace-level webhook secret, normalize the payload into the internal issue model, and update the orchestrator's in-memory state.
2. If a webhook payload arrives without a valid signature header or with a signature that does not match the workspace-level webhook secret, the roki daemon shall reject the request with the documented unauthorized status code without normalizing the payload.
3. While webhook delivery is unavailable, the roki daemon shall poll Linear for active issues on a single workspace-level cadence that does not exceed once every five minutes.
4. If Linear returns an HTTP 429 response, the roki daemon shall apply exponential backoff before its next request and shall log the backoff window.
5. The roki daemon shall expose Linear data to the rest of the system only through a normalized issue model that includes at minimum the issue identifier, title, description, current state, and label set.
6. The roki daemon shall never issue Linear write operations from within its own process; all Linear writes must originate from the agent through the `linear_graphql` proxy tool.

### Requirement 4: Per-Issue Session and Worktree Lifecycle

**Objective:** As an operator, I want each active Linear issue to receive an isolated session working directory plus on-demand git worktrees in the configured repos, so that concurrent runs cannot collide on shared paths or leak state between tickets.

#### Acceptance Criteria
1. When an issue first transitions into an active state recognized by the orchestrator, the roki daemon shall create a session tempdir under the platform-appropriate user cache root using the Linear issue identifier as the directory name, and shall set that tempdir as the worker subprocess's working directory.
2. While a worker is running, the roki daemon shall record every git worktree opened on the worker's behalf in a per-worker registered set of worktrees keyed by the configured repository identifier.
3. The roki daemon shall reject any session or worktree path that, after sanitization, escapes its expected root, contains path traversal segments, or collides with another active worker's session.
4. When the agent invokes the `roki_open_worktree` tool with a repository in the configured allowlist, the roki daemon shall resolve the local clone path through `ghq`, create a git worktree through `wt` whose branch name is the Linear issue identifier verbatim, register the worktree against the worker's registered set of worktrees, and return the worktree path; subsequent invocations with the same repository for the same worker shall return the previously registered path without re-resolving.
5. When an issue transitions into the `Cleaning` state recognized by the orchestrator, the roki daemon shall iterate every worktree registered for the worker, remove each worktree through `wt remove`, and remove the session tempdir after the worker has exited; the daemon shall not delete any branches.
6. When an issue lands in the `TerminalFailure` state, the roki daemon shall retain every registered worktree, every branch, and the session tempdir so that the operator can inspect the residue.
7. If session creation, worktree creation, or worktree removal fails, the roki daemon shall mark the corresponding worker as failed, log the filesystem error with the offending path, and refuse to start additional work for that issue until the operator intervenes.

### Requirement 5: Long-Lived Claude Code Subprocess Adapter

**Objective:** As an operator, I want each active issue to be driven by a long-lived `claude --print --output-format stream-json` session whose lifecycle is observable and bounded, so that the daemon can supervise agent work without polling or blocking.

#### Acceptance Criteria
1. When the orchestrator promotes an issue to an active worker slot, the roki daemon shall launch a `claude --print --output-format stream-json` subprocess in the issue's session tempdir and stream its stdout as newline-delimited JSON events.
2. While a worker subprocess is running, the roki daemon shall parse each emitted JSON event into a typed lifecycle event and feed it into the per-issue state machine.
3. If a worker subprocess emits no events for longer than a configurable stall window, the roki daemon shall treat the worker as stalled, terminate the subprocess, and route the issue to `TerminalFailure` without further retry.
4. The roki daemon shall enforce a configurable per-worker turn budget; once the budget is exhausted the daemon shall stop sending further continuation prompts to that worker session and shall route the issue to `TerminalFailure` without further retry.
5. When a worker subprocess exits cleanly with the issue still in an active state, the roki daemon shall wait one second and then attempt one continuation retry by relaunching a new subprocess for the same issue.
6. When a worker subprocess exits non-cleanly, the roki daemon shall apply a configurable retry budget (default three attempts, range one through ten); while remaining attempts exist the daemon shall apply exponential backoff between launches bounded between ten seconds and five minutes, retain the session tempdir and every registered worktree across the retry, and re-launch a new subprocess for the same issue; once the retry budget is exhausted the daemon shall route the issue to `TerminalFailure`.
7. The roki daemon shall pass agent-launch flags so that kiro skills are discoverable from `~/.claude/skills/kiro-*` (no `--bare`) and shall not depend on slash commands at runtime.

### Requirement 6: Workspace-Level WORKFLOW.md Policy Loader

**Objective:** As an operator, I want a single workspace-level `WORKFLOW.md` file that defines policy in Liquid + Markdown with schema validation and hot reload, so that I can adjust agent behavior without restarting the daemon or recompiling Rust.

#### Acceptance Criteria
1. When the roki daemon starts, the WORKFLOW.md loader shall read the configured workspace-level `WORKFLOW.md`, parse its YAML or TOML front matter, render its Liquid body, and validate the result against the published schema.
2. If the workspace-level `WORKFLOW.md` fails schema validation at startup, the roki daemon shall refuse to start and shall log the validation error with the offending key path.
3. While the daemon is running, the WORKFLOW.md loader shall watch the workspace-level `WORKFLOW.md` for filesystem changes and shall re-validate the file before applying any changes.
4. If a hot-reload attempt produces a `WORKFLOW.md` that fails validation, the roki daemon shall keep the previously valid policy in effect and log the validation failure.
5. The WORKFLOW.md loader shall expose its parsed policy to the orchestrator through a stable schema shape that downstream specs can extend with additional keys without breaking existing consumers, and the loader shall round-trip unknown keys under the reserved extension namespaces without interpreting them.

### Requirement 7: Agent Tool Registry, `linear_graphql` Proxy, and `roki_open_worktree`

**Objective:** As the agent, I want the daemon to expose a small, audited set of tools — most importantly a `linear_graphql` proxy and a `roki_open_worktree` worktree opener — so that I can read and write Linear and provision per-repo working trees without ever holding the Linear API token directly or running `git`/`ghq`/`wt` myself.

#### Acceptance Criteria
1. The roki daemon shall publish an agent tool registry whose entries each declare a stable name, an input schema, and an output schema, and shall provide that registry to every worker subprocess at launch.
2. When the agent invokes the `linear_graphql` tool with a single GraphQL operation and variables, the roki daemon shall forward the request to Linear using the daemon-owned token and shall return the response to the agent unmodified except for credential redaction.
3. If a `linear_graphql` invocation contains more than one GraphQL operation in the same call, the roki daemon shall reject the call with a structured error rather than forwarding it.
4. When the agent invokes the `roki_open_worktree` tool with a `repo` field that is present in the configured allowlist, the roki daemon shall ensure the repository is cloned through `ghq`, open a worktree through `wt` whose branch name is the Linear issue identifier verbatim, and return an output containing the worktree path, the repository identifier, and the branch name.
5. If the agent invokes `roki_open_worktree` with a `repo` field that is not present in the configured allowlist, the roki daemon shall reject the call with a typed allowlist-rejection error that names the offending repository and the configured allowlist; the worker shall continue running.
6. When the agent invokes `roki_open_worktree` a second time within the same worker for a repository it has already opened, the roki daemon shall return the previously registered worktree path without invoking `ghq` or `wt` again.
7. If `ghq` clone resolution or `wt` worktree creation fails for a `roki_open_worktree` invocation, the roki daemon shall return a typed tool error to the agent that names the repository and the underlying failure reason; the worker shall continue running.
8. The roki daemon shall never embed the Linear API token or the Linear webhook secret in any tool input, output, or error message visible to the agent.
9. The agent tool registry shall remain extensible so that downstream specs can register additional read-only tools (for example `kiro_spec_status`, `kiro_review_status`) without breaking existing tool consumers.

### Requirement 8: Orchestrator State Machine and Extension Points

**Objective:** As a downstream spec author, I want the orchestrator to expose stable per-issue state-machine hooks, so that roki-spec-gate, roki-review-gate, roki-observability, and roki-distill-postmerge can subscribe to lifecycle transitions without forking the core.

#### Acceptance Criteria
1. The roki daemon shall maintain an in-memory state machine per Linear issue identifier whose states cover at minimum discovered, queued, active, awaiting-review, terminal-success, cleaning, terminal-failure, and the retry-loop intermediate states between `Active` and the configurable retry budget.
2. When a state transition occurs, the roki daemon shall publish a structured transition event whose payload identifies the previous state, the next state, the trigger source, and the issue identifier, and shall include the originating repository identifier when one applies (for example for transitions caused by `roki_open_worktree` outcomes or worktree cleanup arcs).
3. The roki daemon shall expose subscription hooks that allow other components to observe transition events and to veto a specific set of declared-as-vetoable transitions, and shall document which transitions are vetoable.
4. If a subscriber raises an unhandled error while processing a transition event, the roki daemon shall isolate the failure to that subscriber, log the error with the subscriber's identifier, and continue processing transitions for other subscribers.
5. The roki daemon shall recover the per-issue state on restart by re-reading Linear and the filesystem layout, without depending on a persistent database.

### Requirement 9: Permission Strategy and Default Sandbox

**Objective:** As an operator, I want roki to run agents under the safest workable permission profile while letting me opt into a fallback when Claude's allowlist is unreliable, so that I can balance safety against operability.

#### Acceptance Criteria
1. The roki daemon shall launch each worker subprocess with the agent sandbox set to `workspace-write` and with elicitations rejected by default.
2. When the workspace-level `WORKFLOW.md` declares an alternative sandbox or elicitation policy, the roki daemon shall apply that override to every worker.
3. When the operator selects the `--settings` allowlist permission strategy, the roki daemon shall pass the configured allowlist to each worker subprocess through Claude Code's settings interface.
4. When the operator selects the `--dangerously-skip-permissions` fallback strategy through configuration or the `--dangerously-skip-permissions` CLI flag, the roki daemon shall pass that flag to each worker subprocess and shall log the elevated-permission decision per worker launch.
5. If neither permission strategy is configured, the roki daemon shall refuse to start and shall report the missing configuration.

### Requirement 10: Restart Recovery Without Persistent Storage

**Objective:** As an operator, I want roki to recover its working state after a restart by re-reading Linear and the filesystem rather than relying on a database, so that I never carry stale or corrupted local state across restarts.

#### Acceptance Criteria
1. When the roki daemon starts, it shall list every session tempdir under the platform-appropriate user cache root and, for every configured allowlisted repository, list every existing git worktree whose branch name matches the operator-configurable Linear issue-identifier pattern.
2. The roki daemon shall reconcile every distinct issue identifier discovered from session tempdirs and from worktree branches against Linear before resuming work, and shall classify each discovered issue into one of the documented recovery states (resume-active, orphaned-session, orphaned-worktree, fresh-queued, no-op).
3. If a session tempdir or worktree exists for an issue identifier that has no matching active Linear issue at startup, the roki daemon shall mark that residue as orphaned and log it without deleting it automatically.
4. If a Linear issue is in an active state at startup but has no matching session tempdir or worktree, the roki daemon shall create a fresh session tempdir and resume the normal active-state lifecycle for that issue.
5. The roki daemon shall not write any per-issue runtime state to disk except the session tempdir and worktree contents the agent itself produces and the structured logs the daemon emits.

### Requirement 11: Language-Agnostic SPEC.md

**Objective:** As a future maintainer or a port author, I want a `SPEC.md` at the repository root that captures roki's contract independently of Rust, so that alternative implementations remain consistent with the original.

#### Acceptance Criteria
1. The roki repository shall include a `SPEC.md` at its root that defines the daemon contract, the agent tool registry contract (including `roki_open_worktree`), the workspace-level `WORKFLOW.md` schema, the per-issue state machine, the session tempdir layout, and the worktree registry semantics in language-agnostic terms.
2. The SPEC.md shall describe behavior in terms that any compliant implementation can satisfy without depending on Rust-specific types, libraries, or runtime constructs.
3. When the Rust implementation changes a contract documented in `SPEC.md`, the change shall be accompanied by a corresponding `SPEC.md` update in the same change set.
4. The SPEC.md shall enumerate the extension points that downstream specs depend on (including the `OrchestratorRead` snapshot trait, the pre-cleanup hook, the `TrackerRefresh` nudge trait, the `WorkerContext.additional_context` prelude-forwarding contract, and the reserved `WorkflowPolicy.extension` sub-namespaces) so that future ports preserve them.

### Requirement 12: Observability of Daemon Internals

**Objective:** As an operator debugging a stuck ticket, I want enough structured observability from the daemon itself to diagnose worker, session, worktree, and tracker issues without an external UI, so that the MVP is operable before the dedicated observability spec ships.

#### Acceptance Criteria
1. The roki daemon shall emit a structured log event for every worker lifecycle change, every session tempdir creation or deletion, every worktree creation or removal, every Linear poll or webhook receipt, every backoff or stall decision, every retry attempt with its attempt counter, and every state-machine transition.
2. Every structured log event shall include the Linear issue identifier when one applies, the repository identifier when one applies (for example for worktree-scoped events), and a correlation identifier for the originating worker invocation.
3. The roki daemon shall support a configurable log level and a configurable log destination so that the operator can route logs to stdout, a file, or both.
4. The roki daemon shall redact the Linear API token, the Linear webhook secret, and any other operator-declared secrets from every structured log event before emission.

### Requirement 13: Cross-Spec Extension Surface

**Objective:** As a downstream spec author, I want roki-mvp to publish a stable, additive extension surface, so that roki-spec-gate, roki-review-gate, roki-observability, and roki-distill-postmerge can integrate without forking the core or breaking sibling specs.

#### Acceptance Criteria
1. The roki daemon shall publish a read-only `OrchestratorRead` trait that exposes a snapshot of the per-issue state plus a single-issue lookup, and shall grant no state-mutation rights through that trait.
2. The roki daemon shall expose a vetoable pre-cleanup hook between terminal success and worktree-and-session cleanup so that downstream specs can perform deferred work while the worker's worktrees and session tempdir still exist; a `Deny` decision shall block cleanup and shall be logged.
3. The roki tracker adapter shall publish a `TrackerRefresh` nudge trait that allows external callers to request an out-of-cycle poll without bypassing the documented cadence cap or the 429 backoff state.
4. The roki engine adapter shall accept an additive optional `additional_context` field on `WorkerContext` and shall forward its value verbatim to the agent through a documented session prelude envelope, without interpreting the contents.
5. The workspace-level `WORKFLOW.md` schema shall reserve the `extension.gates.spec.*`, `extension.gates.review.*`, `extension.server.*`, and `extension.distill.*` sub-namespaces for the canonical roki specs; the loader shall round-trip unknown keys under those namespaces without interpreting them.
