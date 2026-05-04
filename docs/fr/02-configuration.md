---
refs:
  id: fr:02-configuration
  kind: fr
  title: "Configuration"
  spec: roki-mvp
  implements:
    - req:roki-mvp:2
    - req:roki-mvp:6
  depends_on:
    - ref:config
  related:
    - ref:cli
    - fr:04-state-machine-and-recovery
    - fr:12-extension-surface
  modules:
    - crates/roki-daemon/src/config/
    - crates/roki-daemon/src/workflow/
---

# FR 02: Configuration

> Two configuration surfaces: `roki.toml` (per workspace) and `WORKFLOW.md` (Liquid + Markdown, hot-reloaded).
> The full schema (key names, defaults, validation rules) lives in [`docs/reference/config.md`](../reference/config.md).

## Purpose

Express "the daemon's startup conditions" (token, repo allowlist, admission filter, etc.) in `roki.toml`, and "the behavior of the agent and each gate" (prompts, gate parameters, observability config, etc.) in `WORKFLOW.md`. The former assumes a daemon restart on change; the latter can be **hot-reloaded without a restart**. Downstream specs each get a reserved namespace inside `WORKFLOW.md`, which lets them add their own configuration keys without forking the core schema.

## User-visible Behavior

### `roki.toml` (immutable at startup)

Operators specify the path with `--config <path>` ([01-daemon-lifecycle](01-daemon-lifecycle.md)).
The file falls into four broad groups:

- **Linear access**: API token / webhook secret / `assignee` (the Linear user whose tickets the daemon will admit; `me` resolves to the API token holder) / `admit_states`. The `assignee` value is consumed by the daemon's mechanical pre-admission-judge ([04-state-machine-and-recovery](04-state-machine-and-recovery.md) §Pre-admission judge); a ticket whose assignee does not match is silently skipped (log only, no state entry, no Linear write).
- **Workspace policy**: path to `WORKFLOW.md`
- **Network**: bind / port for the webhook receiver
- **Repo allowlist**: zero or more `ghq` identifiers + `[permissions].strategy`. The `[judge]` block is removed (orchestrator-session model selection lives in `extension.orchestrator.model` in `WORKFLOW.md`). The previous `extension.linear_updater.*` namespace is also removed and rejected by the loader; orchestrator-session-driven Linear writes replace it.

Any invalid value or resolution failure (`assignee` cannot be resolved / empty `admit_states` / `WORKFLOW.md` path missing / token missing) **refuses startup** and emits the offending field in the structured log.
**`roki.toml` itself is not hot-reloaded; changing it requires a daemon restart.** After restart, `assignee` applies from the next webhook.

The exact name, default, and validation rule for each key live in the "roki.toml schema" table in [`docs/reference/config.md`](../reference/config.md).

### Fixed Linear label conventions

Two label names are **fixed** (not operator-configurable) and read by the daemon's pre-admission-judge ([04-state-machine-and-recovery](04-state-machine-and-recovery.md) §Pre-admission judge):

| Label | Meaning |
|---|---|
| `roki:ready` | Operator authorizes the daemon to process this ticket. Without this label the ticket is silently skipped at pre-admission. |
| `roki:impl` | Operator declares that an existing project-level spec (`<repo>/.kiro/specs/<target>/`) is complete and the ticket should bypass classification and run `kiro-impl` directly. Implies `roki:ready` is also present; `roki:impl` alone (without `roki:ready`) is treated as not authorized and the ticket is skipped. |

Fixed names avoid per-workspace label drift and let operators move tickets between workspaces without relabeling. If an operator wants additional gating (`roki:hold`, `roki:debug`), those can be implemented as workspace-side conventions outside the daemon contract.

### `WORKFLOW.md` (hot-reloadable, Liquid + Markdown)

A single per-workspace file. It consists of front matter (YAML or TOML) and template blocks, and contains:

- **Named template blocks**:
  - `prompt_template_orchestrator` (required) — for the long-lived orchestrator session; rendered once at orchestrator launch, with the per-ticket mode flag (`SPEC_DRIVEN` or `NEEDS_CLASSIFY`, per [04-state-machine-and-recovery](04-state-machine-and-recovery.md) §Pre-admission judge) substituted into the prompt. When rendering fails, a deterministic fallback (issue identifier / title / description / mode flag only) is used so the orchestrator can still be launched.
  - `prompt_template_implement_direct` (required) — for the `implement` phase in NEEDS_CLASSIFY (Path B) mode, where there is no project-level spec. Rendered against the per-phase context envelope and written to the phase subprocess's stdin. The template receives the ticket body's `## Acceptance Criteria` (numbered EARS) as the sole acceptance source.
  - `prompt_template_validate_direct` (required) — for the `validate` phase in NEEDS_CLASSIFY (Path B) mode. Drives the two-stage mechanical / acceptance check against the ticket body's EARS criteria.
  - `prompt_template_open_pr` (required) — for the `open_pr` phase. Drives `gh pr create` with the orchestrator-supplied summary.
  - `prompt_template_<phase>` (optional, one per phase, named after the phase) — operator override for any other phase subprocess's prompt; rendered against the per-phase context envelope and written to the subprocess's stdin. The phases for which this is defined are listed in the [phase catalog](18-worker-skill-workflow.md). When the block is absent the daemon uses the phase's catalog default (slash-command skill or daemon-internal prompt fragment) per [FR 18 §Phase override](18-worker-skill-workflow.md).
- **Reserved extension namespaces**: regions where each downstream spec places its own keys.
  - `extension.orchestrator.*` — orchestrator session (model / effort / max_phases / allowed_tools / stall_seconds). `stall_seconds` (default `600`) sets the orchestrator-stall window; if the orchestrator emits no stdout for that long the daemon SIGTERMs it and routes the issue to `Inactive(reason=orchestrator_crash)` per [FR 19 §Failure modes](19-orchestrator-session.md). Default is sized for `effort=high` turns that combine extended-thinking blocks with tool calls.
  - `extension.phase.<name>.*` — per-phase override surface. Keys: `command` (slash-command override that swaps the catalog default skill, per [FR 18 §Phase override](18-worker-skill-workflow.md); mutually exclusive per phase with `prompt_template_<phase>`), `max_turns` (per-phase `--max-turns` override), and `stall_seconds` (per-phase stall window override; default `120` for every phase). The `max_turns` and `stall_seconds` keys are additive scalars: they may be set with or without `command`, and they coexist with `prompt_template_<phase>`.
  - `extension.server.*` — roki-observability HTTP API.
  - The loader **round-trips unknown keys** (it does not interpret them, and does not delete them). The legacy `extension.linear_updater.*`, `extension.gates.spec.*`, and `extension.gates.review.*` namespaces are rejected by the loader (or simply ignored as unknown); the orchestrator's processing of `daemon_directive` events plus the orchestrator's own artifact validation replaces them.

The consuming spec, requiredness, default, and behavior on invalid values for each namespace live in the "WORKFLOW.md schema" table in [`docs/reference/config.md`](../reference/config.md).

### Hot reload and validation

- **Schema validation failure at startup** → refuse to start + log the offending key path
- **Validation passes on hot reload** → apply the new policy from the next ticket admission (in-flight phase subprocesses keep their rendered prompts)
- **Validation fails on hot reload** → **keep the previous policy** + log the failure (do not stop the daemon)
- **Per-key invalidity inside `extension.*`** (e.g. non-positive `timeout_ms`) → the corresponding spec refuses evaluation + logs the misconfiguration

## Capabilities

- **One daemon for multiple repos**: a single developer runs a single daemon to handle tickets across multiple repos. The `assignee` filter ensures the daemon does not touch other people's tickets.
- **`me` resolution**: writing `me` for `assignee` resolves to the API token holder.
- **Fixed label gating**: `roki:ready` / `roki:impl` are fixed strings the operator applies in Linear; the daemon does not interpret arbitrary labels.
- **Per-spec namespaces**: each downstream spec consumes only its own namespace (no cross-namespace dependencies).
- **Defaulted-key logging**: when an unspecified key falls back to its default, the startup log records which key did so.
- **Single workspace policy**: no per-repo override (only the per-workspace `WORKFLOW.md`).
- **Hot-reload safe**: invalid values do not crash the daemon (the previous policy is retained).

## Boundaries

- **Hot reload of `roki.toml`** is out of scope (only `WORKFLOW.md` is hot-reloadable).
- **Per-repo `WORKFLOW.md` overrides** are out of scope.
- **Per-issue / per-attempt config overrides** are out of scope.
- **Operator-renamable label conventions** are out of scope. `roki:ready` / `roki:impl` are fixed strings.
- **Environment-variable / CLI configuration overrides** are limited to a few values exposed on the CLI (`--bind`, `--port`, etc.); a full override surface is not provided (see [cli reference](../reference/cli.md) for details).
- **Conditional includes / partial templates inside `WORKFLOW.md`** are out of scope.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "WORKFLOW.md loader (Liquid + Markdown, hot reload)" and Boundary Strategy > "Shared seams to watch" > "WORKFLOW.md schema"
- **Requirements**:
  - `roki-mvp Req 2`: Configuration, Assignee Admission, and Multi-Repo Allowlist
  - `roki-mvp Req 6`: Workspace-Level WORKFLOW.md Policy Loader
  - `roki-observability Req 1`, `Req 7`, `Req 15`: Server config gating
- **Design**:
  - `Configuration Schema` / `Workflow Loader` sections of `.kiro/specs/roki-mvp/design.md`
  - The Configuration sections of each spec's `design.md`
- **Related reference**: [config.md](../reference/config.md), [cli.md](../reference/cli.md)
- **Related FR**: [04-state-machine-and-recovery](04-state-machine-and-recovery.md) (pre-admission judge consumes `assignee` + fixed labels), [12-extension-surface](12-extension-surface.md) (the contract for using extension namespaces)
