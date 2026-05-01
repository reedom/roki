# roki — Language-Agnostic Specification

This document is the canonical, language-agnostic contract for the roki daemon.
It describes what a conformant implementation must do regardless of the
implementation language, runtime, or library choices. The Rust implementation
in `crates/roki-daemon/` is one conformant implementation among possibly many;
alternate ports (Go, TypeScript, etc.) and downstream specs
(roki-spec-gate, roki-review-gate, roki-observability, roki-distill-postmerge)
depend on the contracts pinned here.

> **Contract-change rule.** Any change to a contract documented in this file
> MUST be accompanied by the corresponding change to the reference Rust
> implementation in the same change set. Downstream specs depend on the
> stability of every published surface in the "Cross-Spec Extension Surface"
> section; breaking that stability without a coordinated update is a defect.

## Table of Contents

1. [Overview](#1-overview)
2. [Daemon Contract](#2-daemon-contract)
3. [WORKFLOW.md Schema](#3-workflowmd-schema)
4. [Per-Issue State Machine](#4-per-issue-state-machine)
5. [Lifecycle Event Taxonomy](#5-lifecycle-event-taxonomy)
6. [Workspace Path Layout](#6-workspace-path-layout)
7. [Agent Tool Registry](#7-agent-tool-registry)
8. [`linear_graphql` Semantics](#8-linear_graphql-semantics)
9. [Engine Adapter and Claude Session](#9-engine-adapter-and-claude-session)
10. [Cross-Spec Extension Surface](#10-cross-spec-extension-surface)
11. [`required_status` Disambiguation](#11-required_status-disambiguation)
12. [Restart Recovery](#12-restart-recovery)
13. [Permission Strategies and Sandbox](#13-permission-strategies-and-sandbox)
14. [Linear API Contract](#14-linear-api-contract)
15. [Observability Contract](#15-observability-contract)
16. [Contract-Change Rule](#16-contract-change-rule)
17. [Enumerated Extension Points](#17-enumerated-extension-points)

---

## 1. Overview

### 1.1 What roki is

roki is a long-running daemon that observes a project tracker (Linear),
allocates an isolated workspace per active issue across one or more
repositories, and supervises a long-lived `claude --print --output-format
stream-json` subprocess that performs the implementation work for that issue.
The daemon is a passive observer of tracker state and an active controller of
subprocess lifecycle and workspace filesystem state. It is multi-repo from day
one: workspaces are keyed by the tuple `(repo, issue)`.

### 1.2 What roki explicitly does NOT do

The daemon never performs any of the following effects on its own:

- **No Linear writes.** The daemon never calls Linear's mutation endpoints.
  Every Linear write originates from the agent through the `linear_graphql`
  proxy tool described in §8.
- **No PR creation, branch creation, or comment posting.** GitHub side effects
  are produced by the agent inside its workspace using the `gh` CLI; the
  daemon never invokes `gh`.
- **No code edits.** The agent owns the working tree. The daemon only creates
  and removes the workspace directory, and runs the agent inside it.
- **No commits.** Git commits are produced by the agent.
- **No persistent runtime state on disk.** The daemon writes nothing to disk
  except (a) the workspace contents the agent itself produces and (b) the
  structured logs the daemon emits. State on restart is rebuilt by re-reading
  Linear and the workspace directory layout.

### 1.3 Architecture summary

The daemon is structured as an in-memory orchestrator core surrounded by
narrow adapter ports:

- **Tracker adapter** — read-only Linear adapter (webhook hot path, polling
  fallback, 429 backoff).
- **Engine adapter** — launches and supervises the `claude` subprocess.
- **Workspace manager** — per-`(repo, issue)` directory lifecycle with path
  safety.
- **Workflow loader** — reads, validates, and hot-reloads `WORKFLOW.md`.
- **Agent tool registry** — exposes the audited tool surface to the agent.

Adapters implement narrow traits; the orchestrator depends only on those
traits. The daemon never opens an HTTP API beyond the Linear webhook receiver
described in §14; broader observability surfaces are deferred to
roki-observability.

### 1.4 Symphony alignment and divergence

roki is symphony-aligned: in-memory orchestrator with no persistent database,
recovery driven by re-reading the tracker and the filesystem on restart,
`WORKFLOW.md` as the user-facing policy boundary, long-lived stdio agent
session per active issue. roki diverges from symphony in being multi-repo
from day one (workspaces keyed by `(repo, issue)`) and in publishing stable
extension points (this document) so dependent specs can plug in without
forking the orchestrator.

---

## 2. Daemon Contract

### 2.1 Lifecycle

A conformant implementation provides a single binary entry point (the
reference implementation calls it `roki run`) that:

1. Loads its configuration from a layered source (file + environment + CLI
   flags) and refuses to start with a non-zero exit status if the
   configuration is missing, malformed, or fails schema validation. The exit
   path emits a structured log entry that names the offending field.
2. Loads the Linear API token from a non-committed source (environment or
   secret file). Refuses to start when the token is absent.
3. Starts the orchestrator, the tracker adapter, and the workflow loader
   before reporting ready.
4. On `SIGINT` or `SIGTERM`: stops accepting new work, signals each active
   worker subprocess to terminate, awaits a bounded shutdown window per
   worker, and exits cleanly.

### 2.2 Configuration shape

The configuration must declare:

- **An agent allowlist of repositories** under `[[repos]]`. Each entry
  carries only a `repo` ghq identifier (`<owner>/<repo>` or
  `<host>/<owner>/<repo>`); the daemon refuses to start when two entries
  declare the same identifier and names the offending entry. The local
  checkout path is resolved at runtime via `ghq list -p` (cloning on miss
  via `ghq get`); workspaces are git worktrees laid out by the external
  `wt` CLI (see §6). Operators must install both `wt` and `ghq` on
  `$PATH`; absence of either at startup is a hard refusal. An empty
  allowlist starts the daemon with a WARN log; every `roki_open_worktree`
  invocation then returns a typed allowlist-rejection error to the agent.
- **A workspace-level `[linear]` block** carrying the Linear API token
  source (`token_env`, defaults to `LINEAR_API_TOKEN`; or `token_file`
  for file-backed tokens), the workspace-level webhook secret env-var
  name (`webhook_secret_env`, required), and an optional
  `endpoint` override (test-only — production callers leave this unset
  so the daemon hits `https://api.linear.app/graphql`). The daemon
  refuses to start if the API token or the webhook secret cannot be
  resolved.
- **A workspace-level `[workflow]` block** carrying the path to the
  single workspace-level `WORKFLOW.md` policy file (`path`, required).
  The same policy applies regardless of which configured repo(s) the
  agent operates in. Missing or unreadable paths are a hard refusal.
- **A permission strategy**: either an allowlist (the path to a Claude Code
  settings file) or the explicit dangerous-fallback flag (see §13). If
  neither is configured, the daemon refuses to start. The dangerous fallback
  is also reachable via the CLI flag `--dangerously-skip-permissions`, which
  overrides the configured strategy and emits a WARN log on every worker
  launch.
- **An HTTP server bind**: the optional `[server]` block declares the
  `bind` address (default `127.0.0.1` — loopback) and the `port` (default
  `7878`). CLI flags `--bind <addr>` and `--port <num>` override the
  configured values when present.
- **An optional `claude_binary`** path. When absent the daemon resolves
  `claude` via `$PATH` discovery; absence from PATH with no override is a
  hard refusal at startup.
- **A log level** and **log destination** (stdout, file, or both).

### 2.3 Multi-repo routing

When two configured repositories declare overlapping Linear scopes, the
daemon routes each Linear issue to exactly one repository according to a
deterministic precedence rule and logs the decision. If a configured
repository path does not exist or is not a Git working tree, the daemon
marks that repository unhealthy, refuses to schedule work for it, and
continues serving the remaining repositories.

All per-issue runtime state is keyed by the tuple
`(repository identifier, issue identifier)` so that the same issue replicated
across repositories produces independent workspaces and workers.

### 2.4 What the daemon owns vs. what the agent owns

| Concern | Owner |
| --- | --- |
| Linear reads (webhook + poll) | daemon |
| Linear writes (state transitions, comments, etc.) | agent (via `linear_graphql`) |
| PR creation / GitHub interaction | agent (via `gh` CLI in workspace) |
| Code edits, commits, branch management | agent |
| Workspace directory creation / deletion | daemon |
| Subprocess lifecycle (spawn, supervise, terminate) | daemon |
| Tool catalog (what the agent can call) | daemon (publishes), agent (consumes) |
| Agent prompt rendering | daemon (Liquid + Markdown) |
| Daemon-owned secrets (Linear token, webhook secret) | daemon (never crosses subprocess boundary) |

The daemon must never embed daemon-owned credentials into any value visible
to the agent (tool input, tool output, error messages).

---

## 3. WORKFLOW.md Schema

### 3.1 File shape

`WORKFLOW.md` is the user-facing policy artifact. It is a single Markdown
file with a structured front-matter block followed by a Liquid + Markdown
body:

```text
---
# YAML front matter (validated against the WorkflowSchema)
sandbox: workspace-write
elicitations: reject
max_turns: 20
stall_window_seconds: 300
backoff:
  min_seconds: 10
  max_seconds: 300
extension:
  gates:
    spec:
      required_status: "Todo"
      timeout_ms: 600000
      max_attempts: 3
---

# Liquid + Markdown body
You are working on {{ issue.id }} ...
```

The front matter is the policy. The body is the Liquid template the engine
adapter renders into the agent's prompt at worker launch.

### 3.2 Front-matter schema (top-level keys)

| Key | Type | Default | Required | Notes |
| --- | --- | --- | --- | --- |
| `sandbox` | enum (`workspace-write` \| `read-only` \| `unrestricted`) | `workspace-write` | no | Default sandbox mode. |
| `elicitations` | enum (`reject` \| `allow`) | `reject` | no | Default elicitation policy. |
| `max_turns` | unsigned integer | `20` | no | Per-worker turn budget. |
| `stall_window_seconds` | unsigned integer | `300` | no | Event-inactivity stall window. |
| `max_attempts` | unsigned integer | `3` | no | Retry budget for the `Active → Backoff → Active` loop (range `1..=10`; `1` = one shot, no retry). Only `NonCleanExit` consumes the budget; see §9.5. |
| `backoff.min_seconds` | unsigned integer | `10` | no | Lower bound for non-clean retry backoff (≥ 10). |
| `backoff.max_seconds` | unsigned integer | `300` | no | Upper bound for non-clean retry backoff (≤ 300). |
| `extension` | object | `{}` | no | See §3.3. |

The schema is additive: unknown keys at the top level are rejected, but the
`extension` object accepts arbitrary values under reserved sub-namespaces.

### 3.3 Reserved extension namespaces

The four sub-namespaces under `extension` are reserved for canonical roki
specs. The MVP loader does NOT interpret keys under these namespaces; it
round-trips them verbatim through the in-memory policy struct so downstream
specs can deserialize their own slice into their own typed structs.

| Namespace | Owning spec | Purpose |
| --- | --- | --- |
| `extension.gates.spec.*` | roki-spec-gate | Pre-`Active` spec materialization gate keys. |
| `extension.gates.review.*` | roki-review-gate | Pre-`TerminalSuccess` review gate keys. |
| `extension.server.*` | roki-observability | HTTP/TUI observability server keys. |
| `extension.distill.*` | roki-distill-postmerge | Post-merge distill sweep keys. |

A conformant loader:

1. Accepts unknown keys only under `extension.*`. Unknown keys at the top
   level are a validation error.
2. Round-trips the contents of every reserved sub-namespace verbatim. The
   loader never collapses, normalizes, or re-shapes them.
3. Exposes the `extension` value as a JSON-equivalent value so downstream
   specs can deserialize their reserved sub-slice into their own typed
   structs (e.g., `extension.gates.spec` → `SpecGateConfig`).

### 3.4 Validation, hot reload, and last-known-good

- **At startup**: the loader parses each repository's `WORKFLOW.md`,
  renders the Liquid body for syntactic validation, and validates the
  front matter against the published schema. On validation failure, the
  daemon refuses to schedule work for that repository and logs the
  validation error with the offending key path.
- **At runtime**: the loader watches each loaded `WORKFLOW.md` for
  filesystem changes (debounced).
- **On hot-reload validation failure**: the previous valid policy stays in
  effect, and a structured warn event is emitted naming the offending key
  path. This is the "last-known-good fallback".
- **On hot-reload success**: the in-memory policy is replaced atomically.
  In-flight workers continue using the policy active at their launch time;
  new launches see the new policy.

### 3.5 Schema extensibility rules

A change to the `WORKFLOW.md` schema is **additive (safe)** if it only adds
optional fields under `extension.*` or adds optional fields with documented
defaults. A change is **breaking** if it removes a field, retypes a field,
or adds a required field.

Downstream specs MUST register only under their reserved sub-namespace.
They MUST NOT introduce keys outside `extension.*` and MUST NOT depend on
the absence of unknown keys under another spec's reserved sub-namespace.

---

## 4. Per-Issue State Machine

### 4.1 States

A conformant implementation maintains an in-memory state machine per
`(repo, issue)` over the following nine states:

| State | Meaning |
| --- | --- |
| `Discovered` | Issue first observed by the tracker; not yet queued. |
| `Queued` | Routed and waiting for a worker slot or a spec-gate decision. |
| `Active` | Worker subprocess running, or in continuation retry. |
| `AwaitingReview` | PR is open; waiting for reviewer or a tracker move to terminal success. |
| `Backoff` | Backoff window between worker launches after a non-clean exit, stall, or turn-budget exhaustion. |
| `Stalled` | Event-inactivity exceeded the stall window; worker terminated. |
| `TerminalSuccess` | Tracker reports the issue resolved; pre-cleanup hooks have not yet run. |
| `Cleaning` | Interim state between `TerminalSuccess` and workspace removal. The pre-cleanup hook target. |
| `TerminalFailure` | Max retries exceeded or operator intervention; workspace retained. |

### 4.2 Legal transitions (full table)

The legal transition set is exhaustive — any transition not listed below
MUST be rejected by the orchestrator. Transitions whose `Vetoable` column
is "yes" are subject to subscriber veto (see §4.3).

| From | To | Trigger source | Vetoable |
| --- | --- | --- | --- |
| `Discovered` | `Queued` | tracker | no |
| `Queued` | `Active` | engine (worker slot available + workspace ready) | **yes** |
| `Queued` | `TerminalFailure` | engine / tracker (unrouteable) | no |
| `Active` | `Active` | engine (continuation retry on clean exit) | no |
| `Active` | `AwaitingReview` | tracker (agent moved issue to review state) | no |
| `Active` | `Backoff` | engine (`NonCleanExit` while retry budget remains; see §9.5) | no |
| `Active` | `Stalled` | engine (event-inactivity over stall window) | no |
| `Active` | `TerminalFailure` | engine (retry budget exhausted) or operator | no |
| `Backoff` | `Active` | engine (backoff window elapsed) | no |
| `Stalled` | `Backoff` | engine (subprocess terminated; schedule retry) | no |
| `AwaitingReview` | `TerminalSuccess` | tracker (terminal-success state) | **yes** |
| `AwaitingReview` | `Active` | tracker (issue moved back to active) | no |
| `TerminalSuccess` | `Cleaning` | engine (pre-cleanup hook target) | **yes** |

`Cleaning` and `TerminalFailure` have no outgoing legal transitions.
`Cleaning -> [*]` is the canonical workspace-removal step (the workspace is
deleted only after every registered pre-cleanup hook returns `Allow`).
`TerminalFailure -> [*]` retains the workspace for operator inspection.

### 4.3 Vetoable transitions (full list)

Exactly three transitions are vetoable. A subscriber may return a `Deny`
decision on these transitions; on any other transition, subscribers are
observers only.

1. **`Queued -> Active`** — consumed by **roki-spec-gate** to enforce that a
   structured `requirements.md` exists for the issue before the worker
   starts.
2. **`AwaitingReview -> TerminalSuccess`** — consumed by **roki-review-gate**
   to enforce that a structured `review.md` artifact validates before the
   issue is marked successful.
3. **`TerminalSuccess -> Cleaning`** — the **pre-cleanup hook**, consumed by
   **roki-distill-postmerge** to perform deferred work (e.g., post-merge
   distill) while the workspace still exists. A `Deny` decision blocks
   workspace removal and is logged.

Any subscriber returning `Deny` on a non-vetoable transition is treated as a
programmer error: the orchestrator logs and ignores the vote and proceeds
with the transition.

### 4.4 Trigger sources

A conformant orchestrator drives transitions only from the following declared
sources. Any transition originating from outside this set is a programmer
error.

| Trigger | Description |
| --- | --- |
| `TrackerEvent` | Normalized issue event from the tracker (webhook or polling). |
| `EngineEvent` | Engine lifecycle event (subprocess started, exited, stalled, ...). |
| `RecoveryScan` | Restart-time reconciliation against Linear and the workspace layout (see §12). |
| `OperatorShutdown` | `SIGINT`/`SIGTERM` handler asking workers to wind down. |
| `SubscriberVeto` | A vetoable transition was denied; the orchestrator records the denial event with this trigger so logs distinguish a "subscriber said no" from a "tracker said move". |

### 4.5 Subscriber error isolation

If a subscriber raises an unhandled error while processing a transition
event, the daemon isolates the failure to that subscriber, logs the error
with the subscriber's identifier, and continues processing transitions for
other subscribers. Subscriber failure on a non-vetoable event is logged and
ignored. Subscriber failure on a **vetoable** event is treated as `Deny` to
fail closed.

### 4.6 Transition event payload

Every committed transition publishes a structured event with at minimum the
following fields:

```text
TransitionEvent {
  repo:           RepoId,
  issue:          IssueId,
  previous:       WorkerState,
  next:           WorkerState,
  trigger:        TransitionTrigger,   // one of §4.4
  correlation_id: CorrelationId,       // UUID v4 per worker invocation
  vetoable:       boolean,             // true iff (previous, next) is in §4.3
}
```

The `vetoable` flag is derived from the `(previous, next)` pair so subscribers
and observability pipelines do not have to reimplement the table.

---

## 5. Lifecycle Event Taxonomy

The engine adapter emits a typed lifecycle event stream parsed from the
subprocess' stream-json output. The taxonomy is stable; downstream specs
(notably roki-observability) depend on it.

### 5.1 Engine lifecycle events

| Variant | Meaning |
| --- | --- |
| `Started` | Subprocess session bootstrapped. Emitted on the documented `{"type":"system","subtype":"init",...}` line at the start of a stream-json session. |
| `AgentMessage` | Generic non-empty event observed. Used for `assistant` / `user` text envelopes and as the catch-all bucket for unknown `type` values to keep the supervisor loop's progress timestamps advancing across schema drift. |
| `ToolCall { name }` | Agent invoked a registered tool. `name` is the tool identifier. |
| `ToolResult { name, ok }` | Tool invocation completed. `ok` reports whether the tool reported a non-error result. |
| `Error { message }` | Either a stream-json line failed to parse, or the line carried a `result`-shaped payload signalling an error. |
| `Exited(WorkerOutcome)` | Terminal event for a launch. Emitted exactly once per launch. |

A bad JSON line yields exactly one `Error` event plus exactly one structured
warn log; subsequent lines parse independently.

### 5.2 WorkerOutcome variants

`Exited` carries one of:

| Variant | Meaning |
| --- | --- |
| `CleanExit` | Subprocess exited with status 0. |
| `NonCleanExit { code }` | Subprocess exited with a non-zero status. Signal-only terminations on Unix are reported as `128 + signal`. |
| `TurnBudgetExhausted` | The configurable per-worker turn budget was reached; no further continuation prompt was sent for the current invocation. |
| `Stalled { reason }` | Event-inactivity exceeded the stall window; the supervisor killed the subprocess. |

### 5.3 Termination guarantees

- Exactly one terminal `Exited` event per successful launch.
- If both the stall watchdog fires and the subprocess exits at the same
  instant, the stall outcome wins (event-inactivity is the canonical
  termination reason for that race).
- Killing the child if the supervisor task is dropped is mandatory so a
  panicking supervisor cannot leak orphan subprocesses.

---

## 6. Workspace Path Layout

### 6.1 Path layout

Workspaces are real git worktrees of the configured source repository,
not bare sandbox directories. For a repo whose ghq identifier is
`<owner>/<repo>`, the local checkout sits at the path `ghq list -p`
returns (typically `<ghq_root>/<host>/<owner>/<repo>`); the per-issue
worktree is created as a sibling of that checkout:

```text
{repo_path}/../{repo_name}.{branch_sanitized}
```

The branch name is the Linear issue id verbatim. The sanitizer in §6.2
is applied to the branch component of the path; the original issue id is
preserved as the branch name itself wherever the underlying VCS allows
it. Each `(repo, issue)` worktree is independent, even across
repositories that share the same Linear scope.

### 6.2 Sanitization rules

The sanitization rule lives inside the `wt` adapter and is the only
sanitizer applied to issue identifiers when they are mapped to branch /
worktree path components:

1. **Allowed character class**: `[A-Za-z0-9_-]`. Any character outside
   this class is replaced with `-`.
2. **Reject collisions**: two distinct issue ids that sanitize to the
   same worktree path under the same repo are not permitted
   simultaneously; the second `ensure` is rejected with a typed
   identifier-collision error.

The pre-task-6.1 path-safety rules (descendant-of-workspace-root,
canonicalization escape rejection, `.`/`..` traversal sentinels) no
longer apply: the worktree path is computed deterministically from the
repo path returned by `ghq` and is created only by `wt switch --create`,
so a smuggled `..` segment in an issue id collapses to `-` via
sanitization rather than being interpreted as a path component.

### 6.3 Lifecycle invariants

- Workspace creation happens on the first transition into an active
  state via `wt switch --create <branch>` against the resolved repo
  path (deterministically the `Queued -> Active` edge in the reference
  implementation).
- The worker subprocess MUST run with the worktree as its current
  working directory.
- Workspace deletion happens only after `Cleaning -> [*]` via
  `wt remove <worktree_path>`, that is, only after every registered
  pre-cleanup hook returns `Allow` and the worker has exited.
  `wt remove` does NOT delete the underlying branch, so the operator
  can still `git checkout <issue-id>` from the source repo to inspect
  the agent's history if a follow-up task is required.
- `TerminalFailure` retains BOTH the worktree directory AND the branch
  for inspection — the daemon simply does not call `wt remove`.
- If `ghq` cannot resolve or clone the configured identifier, the
  daemon marks that repo unhealthy and refuses to schedule work for it
  while continuing to serve other repos. If `wt switch --create` or
  `wt remove` fails for a specific `(repo, issue)`, the daemon logs the
  failure with the offending path and refuses to start additional work
  for that `(repo, issue)` until operator intervention.

---

## 7. Agent Tool Registry

### 7.1 Tool contract

A tool registered with the registry MUST declare a stable, immutable
identity for the life of the process:

| Field | Stable for life of process | Notes |
| --- | --- | --- |
| `name` | yes | Stable kebab-case identifier exposed to the agent (e.g. `linear_graphql`). |
| `description` | yes | Human-readable summary. |
| `input_schema` | yes | JSON-Schema document describing accepted inputs. |
| `output_schema` | yes | JSON-Schema document describing the response shape. |
| `invoke` (or equivalent) | n/a | Async call entry point. |

### 7.2 Registry contract

The registry exposes:

- `register(tool)` — registers a tool. Duplicate names are rejected with a
  structured `DUPLICATE_TOOL` error.
- `catalog()` — returns a serializable snapshot of every registered tool as
  a list of `ToolDescriptor { name, description, input_schema, output_schema }`.
  The catalog is shipped to every worker subprocess at launch (see §9).
- `dispatch(name, input)` — invokes a tool by stable name. Returns
  `UNKNOWN_TOOL` if no tool is registered under `name`.

### 7.3 Tool error taxonomy

Tools return a structured error variant from a fixed taxonomy. Conformant
errors include at minimum:

- `MULTIPLE_OPERATIONS` — see §8.
- `INVALID_INPUT { reason }` — caller-supplied JSON failed schema validation.
- `RATE_LIMITED { retry_after_seconds }` — provider asked us to back off.
- `LINEAR_HTTP_ERROR { status }` — provider returned a non-2xx status.
- `REDACTION_FAILED` — redaction discovered a token leak it could not scrub
  safely; the call is failed loudly rather than risking returning the raw
  secret.
- `DUPLICATE_TOOL { name }` — registration rejected.
- `UNKNOWN_TOOL { name }` — dispatch under an unregistered name.

### 7.4 Extensibility

The registry is extensible: downstream specs may register additional
**read-only** tools (for example `kiro_spec_status`, `kiro_review_status`)
without breaking existing tool consumers. A breaking change to an existing
tool's `name`, `input_schema`, or `output_schema` is a contract violation.

### 7.5 Catalog stability

`catalog()` returns entries in a deterministic order (sorted by `name` in the
reference implementation) so the worker launch payload stays stable across
orchestrator restarts.

---

## 8. `linear_graphql` Semantics

`linear_graphql` is the agent's only path to Linear. It is implemented by the
daemon and registered into the agent tool registry at startup.

### 8.1 Single-operation enforcement

The tool accepts a single GraphQL operation per call. Specifically:

- **Input shape**: `{ query: string, variables: object }`.
- **Validation**: the daemon parses the GraphQL document and rejects with
  `MULTIPLE_OPERATIONS` if the document contains more than one operation
  definition. The rejection is **structural** — it happens before any HTTP
  request is sent.

### 8.2 Daemon-owned token

The Linear API token is held only by the daemon. It MUST NEVER appear in:

- A tool input visible to the agent.
- A tool output returned to the agent.
- An error message returned to the agent.
- Any log line emitted by the daemon (a redaction layer scrubs known secret
  strings before emission).

A redaction layer that discovers an unscrubbable token leak in any error
string MUST fail the call with `REDACTION_FAILED` rather than risk returning
the raw secret.

### 8.3 Pass-through semantics

For accepted single-operation calls, the daemon forwards the request to
Linear using its own credentials and returns the Linear response payload to
the agent unmodified except for credential redaction.

### 8.4 Shared rate-limit state

The proxy and the tracker (§14) share a single rate-limit state. A 429 from
either side advances the shared backoff window so the daemon never sends a
second request while a 429 backoff is in flight.

---

## 9. Engine Adapter and Claude Session

### 9.1 Subprocess invocation

For each launch the engine adapter spawns:

```text
claude --print --output-format stream-json [--settings <path> | --dangerously-skip-permissions]
```

with:

- the issue workspace as the current working directory,
- stdin / stdout / stderr piped,
- `kill_on_drop` enabled so a panicking supervisor cannot leak orphan
  subprocesses,
- the `--bare` flag intentionally omitted so kiro-* skills under
  `~/.claude/skills/` remain discoverable.

The daemon does NOT depend on slash commands at runtime.

### 9.2 Permission strategy passthrough

The engine adapter passes exactly one of the operator-resolved permission
strategy flags to the subprocess:

- **Allowlist**: `--settings <path>`, where `<path>` is the resolved Claude
  Code settings file.
- **Dangerous fallback**: `--dangerously-skip-permissions`. Every launch in
  this mode emits a structured warn log naming the elevated-permission
  decision.

If neither strategy is configured the daemon refuses to start (see §13).

### 9.3 Prelude envelope

The agent prompt is delivered through a stable, machine-extractable "prelude
envelope" prepended to the session input on stdin. The envelope shape is:

```text
<<<ROKI_PRELUDE>>>
{ ... JSON object with version, repo, issue, tools, additional_context, ... }
<<<END_PRELUDE>>>
<rendered prompt>
```

| Sentinel / key | Stable value | Purpose |
| --- | --- | --- |
| Opening sentinel | `<<<ROKI_PRELUDE>>>` | Marks the start of the JSON body. |
| Closing sentinel | `<<<END_PRELUDE>>>` | Marks the end of the JSON body. |
| `version` | unsigned integer (`1` at MVP) | Schema version. Bumped only on a breaking shape change. |
| `repo` | string | Repository identifier (contextual; not interpreted at MVP). |
| `issue` | string | Issue identifier (contextual; not interpreted at MVP). |
| `tools` | array of `ToolDescriptor` | Tool catalog snapshot (see §7). |
| `additional_context` | any (optional) | Verbatim downstream-injected context (see §10.4). Skipped when absent. |

The sentinels exist so downstream specs can locate the envelope
deterministically without depending on a JSON parser at the agent end.

### 9.4 WorkerContext

The per-launch context handed to the engine adapter must contain at minimum:

```text
WorkerContext {
  repo:               RepoId,
  issue:              IssueId,
  correlation_id:     CorrelationId,
  workspace_dir:      Path,
  prompt:             string,                  // rendered Liquid body
  tool_catalog:       [ToolDescriptor],
  permission:         ResolvedPermission,      // see §13
  policy:             EnginePolicy,            // turn budget, stall, backoff
  additional_context: any | null,              // §10.4 — verbatim forward
}
```

`additional_context` is an additive optional field. A conformant engine
adapter forwards its value verbatim through the prelude envelope under the
stable key `additional_context`. The MVP daemon never interprets the
contents. New optional fields may be added under the same forwarding contract;
removing or retyping fields is a breaking change.

### 9.5 Bounded-loop semantics

The engine adapter enforces:

- **Turn budget** (`max_turns`, default `20`): once the budget is exhausted,
  the daemon stops sending further continuation prompts to that worker
  session for the current invocation. The terminal outcome is
  `TurnBudgetExhausted`.
- **Stall detection** (`stall_window`, default `300s`): if no engine
  lifecycle events arrive for longer than the stall window, the daemon
  treats the worker as stalled, terminates the subprocess, and reports
  `Stalled { reason: EventInactivity }`.
- **Continuation retry**: when a worker exits cleanly with the issue still
  in `Active`, the daemon waits **one second** (`CLEAN_EXIT_RETRY_DELAY`) and
  attempts one continuation retry by relaunching a new subprocess for the
  same `(repo, issue)`.
- **Exponential backoff**: when a worker exits non-cleanly or after
  exhausting its turn budget, the daemon applies exponential backoff before
  the next launch attempt for the same `(repo, issue)`. The computed delay
  is always clamped to **`[10s, 300s]`** (`BACKOFF_FLOOR` and
  `BACKOFF_CEILING`) regardless of operator overrides, so a misconfigured
  `WORKFLOW.md` cannot produce a delay outside the documented envelope.
- **Retry budget (`max_attempts`)**: only `NonCleanExit` outcomes consume
  the per-`(repo, issue)` retry budget configured by `max_attempts`
  (default `3`, range `1..=10`). When a `NonCleanExit` occurs and the
  budget remains, the worker actor drives `Active → Backoff →
  Active` and re-launches; when the budget is exhausted, it routes
  `Active → TerminalFailure` and retains the workspace for inspection.
  `Stalled` and `TurnBudgetExhausted` are agent-authored failures that
  repeat under the same prompt and budget, so they route directly from
  `Active → TerminalFailure` without consuming the retry budget. The
  workspace is retained across the entire Backoff loop — no
  delete/recreate between attempts. The prelude / `additional_context`
  is re-emitted unchanged on each launch; failure-history accumulation
  is a downstream-spec concern.

### 9.6 Schema-drift tolerance

The stream-json parser is keyed on the stable `type` field. Unknown values
map to `AgentMessage` so the supervisor loop continues to record progress
timestamps when Claude Code adds new event shapes. One bad line cannot abort
the worker.

### 9.7 Bootstrap startup sequence

A conformant `roki run` invocation composes its components in this order so
secrets are redacted before they appear in any log line and so refusals
land before any subsystem holds resources:

1. Load the config from `--config <path>` (default `./roki.toml`). Apply
   CLI overrides for `--bind`, `--port`, and `--dangerously-skip-permissions`.
2. Resolve every secret (Linear API token plus per-repo webhook secret) and
   reinitialise the redaction-aware logging pipeline with the resolved
   secret list.
3. Install OS signal handlers wired to a single `ShutdownSignal`.
4. Resolve the `claude` binary (`claude_binary` config override → `$PATH`
   discovery → hard refusal with an actionable message).
5. Build per-repo `WorkflowLoader`s with debounced hot-reload, the
   workspace manager, the permission resolver, and the engine adapter.
6. Build the orchestrator with `EnginePolicy::from_workflow(&policy)`
   resolved from the parsed `WORKFLOW.md`.
7. For each repo, spawn a `LinearTracker` (poll task) and build a
   `WebhookState`; mount the route at `/linear/webhook/<sanitised-repo-id>`
   on a single `axum::Router`.
8. Bind the HTTP server at `[server].bind:[server].port` (default
   `127.0.0.1:7878`). A bind failure is a hard refusal naming the
   conflicting address.
9. Funnel polling and webhook outputs through the `TrackerBridge` into the
   orchestrator inbox.
10. Drive `tokio::select!` on the shared `ShutdownSignal` across the
    orchestrator, the bridge, the server, and every tracker. On shutdown
    the trackers receive their oneshot signal, then the bridge and the
    server are awaited through `await_workers_with_window` with the
    documented 30s shutdown window.

The bootstrap MUST NOT block on Linear connectivity at startup. Trackers
retry their first poll asynchronously, so the webhook server comes up
regardless of whether Linear is reachable.

---

## 10. Cross-Spec Extension Surface

This section pins every published cross-spec contract. Downstream specs
(roki-spec-gate, roki-review-gate, roki-observability,
roki-distill-postmerge) build on these. A breaking change to any item
listed here triggers the contract-change rule in §16.

### 10.1 `OrchestratorRead`

A read-only projection trait exposing:

- `snapshot()` — return a self-contained owned snapshot of the per-`(repo,
  issue)` state for every tracked worker. The snapshot is safe to serialize
  to JSON and ship over a wire.
- `issue(repo, issue)` — return the projection for a single `(repo, issue)`,
  or `None` if no worker for that key is being tracked.

The trait is read-only by construction:

- Every method takes an immutable `&self`. There is no mutator method.
- Returned types are owned clones of internal projections; consumers cannot
  reach back into orchestrator state through them.
- Implementations MUST NOT panic on unknown `(repo, issue)` keys.

The snapshot wire shape is versioned. The MVP version string is `"v1"`; it
is bumped only on a breaking change to the JSON shape. Additive fields do
not bump the version.

### 10.2 `TrackerRefresh`

A nudge-only trait exposing:

- `nudge()` — request that the next per-scope poll be scheduled sooner.
  Returns the window within which the next poll will occur.

The trait grants no read or mutation surface beyond the nudge:

- It cannot bypass the documented 5-minute per-scope polling cadence cap.
- It cannot shorten an active 429 exponential-backoff window.

When the tracker is idle within its cadence, `will_poll_within` is
approximately zero because the nudge advances the next-poll deadline to
"now". When the tracker is in 429 backoff, `will_poll_within` is the
remaining backoff window — the nudge does not shorten the backoff.

### 10.3 Pre-cleanup hook

A vetoable observer of the `TerminalSuccess -> Cleaning` transition:

```text
trait PreCleanupHook {
  async fn pre_cleanup(ctx: &PreCleanupContext) -> VetoDecision;
}

PreCleanupContext { repo, issue, correlation_id, ... }
VetoDecision = Allow | Deny { reason: string }
```

Hooks are registered through `Orchestrator::register_pre_cleanup_hook(hook)`.
The orchestrator dispatches every registered hook in registration order
before the workspace is removed. Aggregation policy:

- If every hook returns `Allow`, the aggregated decision is `Allow` and the
  workspace is removed.
- If any hook returns `Deny`, the aggregated decision is the **first** `Deny`
  encountered (preserving its `reason` for logging). The workspace is NOT
  removed. Subsequent hooks are still invoked so each gets a chance to
  perform any side-effect-free observation it wants.

A `Deny` decision blocks workspace removal and is logged with the supplied
reason. The operator-intervention path (manual cleanup) still applies to a
denied workspace.

Hooks must NOT mutate orchestrator state, must NOT call back into the
tracker or engine adapters, and must be cheap to clone (typically wrapping
a shared inner type).

### 10.4 `WorkerContext.additional_context` and the prelude-forwarding contract

`WorkerContext.additional_context` is an additive optional field reserved
for downstream specs to inject prelude context into the worker session. The
MVP engine adapter forwards this value verbatim through the workspace
prelude / Claude session prompt envelope under the stable JSON key
`additional_context`; the MVP itself does NOT interpret the contents.

Forwarding contract:

- The value MUST appear under the stable JSON key `additional_context`
  inside the prelude envelope (see §9.3).
- The value MUST round-trip verbatim. The daemon does not normalize, reorder,
  or rewrite it.
- When the value is absent (`None` / `null`), the key is omitted from the
  envelope so the JSON shape stays minimal in the common case.

`WorkerContext` is extensible by additional optional additive fields under
the same forwarding contract. Removing or retyping fields is a breaking
change.

Documented consumers:

- **roki-review-gate** injects a `.review-findings.json` summary so the
  agent's next session can address findings.

### 10.5 Reserved `WorkflowPolicy.extension` sub-namespaces

The `WorkflowPolicy.extension` value reserves four sub-namespaces (see §3.3
for the table). The MVP loader round-trips these verbatim. Every downstream
spec MUST register only under its assigned sub-namespace.

### 10.6 Agent tool registry extensibility

Downstream specs may register additional **read-only** tools through
`Registry::register`. The tool surface is intentionally additive: a
breaking change to an existing tool's name or schemas is a contract
violation.

---

## 11. `required_status` Disambiguation

Two distinct gate extensions both publish a key named `required_status`
inside the `WORKFLOW.md` `extension` namespace. They share a name but are
different semantic fields. SPEC.md calls out this distinction explicitly so
implementers do not conflate the two.

| Field path | Owner spec | Field type | What it gates against | Where the value comes from |
| --- | --- | --- | --- | --- |
| `extension.gates.spec.required_status` | **roki-spec-gate** | string (Linear issue state name) | The Linear workflow-state name in which the spec gate evaluates. Transitions arising from any other Linear state do not trigger gate evaluation. | The Linear team's workflow-state catalog. Default is `"Todo"`. |
| `extension.gates.review.required_status` | **roki-review-gate** | string (`review.md` frontmatter `status` value) | The artifact status in `review.md`'s YAML frontmatter that the gate treats as a pass. A `review.md` whose overall status differs from this configured value is treated as a fail. | The `status` field inside the `review.md` artifact written by the agent. Default is `"pass"`. |

These are different semantic fields:

- `extension.gates.spec.required_status` is consumed by the **roki-spec-gate
  subscriber** on the `Queued -> Active` vetoable transition. It is matched
  against the Linear workflow-state name carried on the issue's tracker
  metadata.
- `extension.gates.review.required_status` is consumed by the
  **roki-review-gate validator** on the `AwaitingReview -> TerminalSuccess`
  vetoable transition. It is matched against a YAML frontmatter status
  value inside a `review.md` file at
  `<workspace_root>/<repo>/<issue>/.kiro/specs/<issue>/review.md`.

A conformant implementation MUST NOT treat these two fields as
interchangeable. They live in different reserved sub-namespaces, are
consumed by different subscribers, and operate on different value spaces.
SPEC.md mentions both `extension.gates.spec.required_status` and
`extension.gates.review.required_status` here in this disambiguation
paragraph so that any search for either path lands on this section.

---

## 12. Restart Recovery

### 12.1 Recovery without persistent storage

The daemon writes no per-issue runtime state to disk except the workspace
contents the agent itself produces and the structured logs the daemon
emits. There is no SQLite, sled, or sidecar state file.

### 12.2 Recovery scan

On daemon start, the orchestrator runs a one-shot reconciliation:

1. **Inventory the workspace root**. List every directory under
   `<workspace_root>/<repo>/<issue>/` and produce a set of `(repo, issue)`
   keys.
2. **Re-fetch the corresponding Linear issue state** for each scope the
   daemon serves.
3. **Classify** each key in the union of both sets per the matrix below.

### 12.3 Recovery decision matrix

| Workspace exists | Linear issue is active | Decision | Action |
| --- | --- | --- | --- |
| yes | yes | `ResumeActive` | Emit a synthetic active event into the tracker inbox; the worker actor re-uses the workspace and resumes the active-state lifecycle. |
| yes | no | `OrphanedWorkspace` | Retain the workspace, emit a structured warn naming the path, do NOT spawn a worker. |
| no | yes | `FreshQueued` | Create the workspace and enter `Queued` so the active-state lifecycle proceeds normally. |
| no | no | `NoOp` | Absent on both sides; do nothing. |

`OrphanedWorkspace` directories are never deleted automatically — operator
intervention is required.

### 12.4 Disk-write budget

Recovery honours the daemon's disk-write budget by never persisting a
sidecar state file. The only filesystem touch the recovery scan performs is
through the workspace adapter when a `FreshQueued` decision is realised.

---

## 13. Permission Strategies and Sandbox

### 13.1 Strategies

A conformant implementation supports exactly two permission strategies for
worker launches:

1. **Allowlist** (`--settings <path>`): the daemon passes a path to a Claude
   Code settings file declaring the allowlist. This is the default safe
   choice when Claude's allowlist is reliable for the operator's workload.
2. **DangerousFallback** (`--dangerously-skip-permissions`): the daemon
   passes the dangerous-fallback flag and emits a structured warn log naming
   the elevated-permission decision **per worker launch**.

If neither strategy is configured, the daemon refuses to start.

### 13.2 Sandbox defaults

By default, every worker launch uses:

- **`sandbox = workspace-write`** (the agent may write only inside its
  workspace).
- **`elicitations = reject`** (any elicitation request from the agent is
  refused).

When `WORKFLOW.md` declares an alternative sandbox or elicitation policy,
the daemon applies that override only for workers serving the corresponding
repository.

### 13.3 Resolved permission shape

The resolver combines the operator's selection with any per-repo
`WORKFLOW.md` override and produces a value containing at minimum:

```text
ResolvedPermission {
  mode:           PermissionMode,        // Allowlist { settings_path } | DangerousFallback
  sandbox:        SandboxMode,
  elicitations:   ElicitationsMode,
  mode_source:    PermissionSource,      // Operator | WorkflowOverride (recorded for the warn log)
}
```

---

## 14. Linear API Contract

### 14.1 Daemon-side read-only

The daemon adapter is read-only against Linear (Requirement 3.5). Writes are
routed through the agent via `linear_graphql` (§8).

### 14.2 Polling cadence cap

While webhook delivery is unavailable or as a cold-path fallback, the daemon
polls Linear for active issues on a cadence that does not exceed **once
every five minutes** per repository scope. The cap is enforced by a per-scope
token bucket.

### 14.3 429 backoff

If Linear returns an HTTP 429, the daemon applies **exponential backoff**
before its next request to the same endpoint. The backoff window is bounded
by **`MIN_BACKOFF = 10s`** and **`MAX_BACKOFF = 5min`** and honours the
`Retry-After` header when present. The backoff window is logged.

The polling adapter and the `linear_graphql` proxy share a single rate-limit
state so a 429 observed on either path advances the shared backoff window.

### 14.4 Webhook receiver

The webhook receiver is the hot path; polling is the cold-path fallback.

- **Endpoint**: `POST /linear/webhook` (default path).
- **Signature**: the receiver verifies an HMAC-SHA256 of the **raw request
  body** against the configured shared secret using the
  `Linear-Signature` header **before any deserialization**. The comparison is
  constant-time.
- **On valid signature + parseable issue payload**: the receiver normalizes
  the payload into the `NormalizedIssue` model and dispatches it to the
  orchestrator. Returns **`204`**.
- **On invalid signature**: returns **`401`** with an empty response body
  (payload contents are never echoed).
- **On malformed payload (after signature verification)**: returns **`400`**
  with an empty response body.

Non-`Issue` event types are acknowledged and ignored without dispatch.

### 14.5 NormalizedIssue model

The daemon exposes Linear data to the rest of the system only through a
normalized issue model that includes at minimum:

```text
NormalizedIssue {
  repo:          RepoId,             // resolved by the multi-repo router
  issue:         IssueId,
  title:         string,
  description:   string,
  state:         IssueState,         // active | review | terminal | other
  labels:        [string],
  team_or_scope: string,
}
```

### 14.6 Idempotency

Orchestrator transitions are idempotent on `(repo, issue, target_state)` so
that webhook duplicate deliveries do not double-schedule work.

---

## 15. Observability Contract

### 15.1 Structured events

A conformant implementation emits a structured log event for at least every
one of the following:

- Every worker lifecycle change.
- Every workspace creation or deletion.
- Every Linear poll or webhook receipt.
- Every backoff or stall decision.
- Every state-machine transition.

### 15.2 Required context fields

Every structured event MUST include:

- The `(repo, issue)` key when one applies.
- A correlation identifier for the originating worker invocation.

### 15.3 Redaction

The daemon redacts the Linear API token and any other configured secrets
from every structured log event before emission. A redaction failure on a
tool error path fails the call with `REDACTION_FAILED` (see §8.2) rather
than risking leaking the raw secret.

### 15.4 Configurable destination

The log destination is configurable: stdout, a file, or both. The log level
is configurable. Both knobs are surfaced through the CLI / configuration.

---

## 16. Contract-Change Rule

A change to any contract documented in this file MUST be accompanied by the
corresponding change to the reference Rust implementation in the same change
set. Concretely, the following kinds of change are gated by this rule:

- Changes to the orchestrator state set, the legal-transition table, or the
  vetoable-transition list (§4).
- Changes to the agent tool registry contract or `linear_graphql` semantics
  (§7, §8).
- Breaking changes to the `WORKFLOW.md` schema (§3). Additive changes under
  reserved `extension.*` sub-namespaces are safe.
- Changes to the workspace path layout or sanitization rules (§6).
- Changes to the lifecycle event taxonomy emitted by the engine adapter (§5).
- Changes to subscriber error-isolation semantics (§4.5).
- Changes to the published read-side traits (`OrchestratorRead`,
  `TrackerRefresh`) consumed by roki-observability (§10.1, §10.2).
- Changes to the pre-cleanup hook contract consumed by roki-distill-postmerge
  (§10.3).
- Changes to the `WorkerContext` field set or the prelude-forwarding
  mechanism for `additional_context` (§9.3, §9.4, §10.4).

Downstream specs (roki-spec-gate, roki-review-gate, roki-observability,
roki-distill-postmerge) depend on the stability of the surfaces enumerated
in §17. A reference-implementation change that lands without its
SPEC.md update — or a SPEC.md change that lands without its
reference-implementation update — is a defect that downstream specs are
entitled to reject.

---

## 17. Enumerated Extension Points

For convenience, the full set of extension points downstream specs depend
on, with the consuming spec named for each:

### 17.1 State-machine subscription hooks

| Extension point | Consumer | Section |
| --- | --- | --- |
| Vetoable `Queued -> Active` | roki-spec-gate | §4.3 |
| Vetoable `AwaitingReview -> TerminalSuccess` | roki-review-gate | §4.3 |
| Vetoable `TerminalSuccess -> Cleaning` (pre-cleanup hook) | roki-distill-postmerge | §4.3, §10.3 |

### 17.2 Read-side traits

| Trait | Consumer | Section |
| --- | --- | --- |
| `OrchestratorRead` | roki-observability | §10.1 |
| `TrackerRefresh` | roki-observability | §10.2 |

### 17.3 Engine extension surface

| Extension point | Consumer | Section |
| --- | --- | --- |
| `WorkerContext.additional_context` (verbatim prelude forwarding) | roki-review-gate | §9.4, §10.4 |
| Prelude envelope sentinels and stable JSON keys (`<<<ROKI_PRELUDE>>>` / `<<<END_PRELUDE>>>`, `additional_context`, `tools`) | roki-review-gate, roki-observability | §9.3 |
| Engine lifecycle event taxonomy (`Started`, `AgentMessage`, `ToolCall`, `ToolResult`, `Error`, `Exited(WorkerOutcome)`) | roki-observability | §5 |

### 17.4 Agent tool registry

| Extension point | Consumer | Section |
| --- | --- | --- |
| `Registry::register` (additional read-only tools) | roki-spec-gate (`kiro_spec_status`), roki-review-gate (`kiro_review_status`) | §7.4 |
| `linear_graphql` proxy (single-operation, daemon-owned token, redaction) | agent (all specs indirectly) | §8 |

### 17.5 `WORKFLOW.md` reserved extension sub-namespaces

| Sub-namespace | Consumer | Section |
| --- | --- | --- |
| `extension.gates.spec.*` (including `required_status`, `timeout_ms`, `max_attempts`) | roki-spec-gate | §3.3, §11 |
| `extension.gates.review.*` (including `required_status`, `timeout_ms`, `max_attempts`) | roki-review-gate | §3.3, §11 |
| `extension.server.*` | roki-observability | §3.3 |
| `extension.distill.*` | roki-distill-postmerge | §3.3 |

### 17.6 Recovery and observability

| Extension point | Consumer | Section |
| --- | --- | --- |
| Recovery decision matrix (`ResumeActive` / `OrphanedWorkspace` / `FreshQueued` / `NoOp`) | operators, roki-observability | §12.3 |
| Structured event taxonomy with `(repo, issue, correlation_id)` context | roki-observability | §15 |

A breaking change to any row in the tables above MUST follow the
contract-change rule in §16.
