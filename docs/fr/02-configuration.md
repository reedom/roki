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

- **Linear access**: API token / webhook secret / assignee filter / `admit_states`
- **Workspace policy**: path to `WORKFLOW.md`
- **Network**: bind / port for the webhook receiver
- **Repo allowlist**: zero or more `ghq` identifiers + `[permissions].strategy`. The `[judge]` block is removed (orchestrator-session model selection lives in `extension.orchestrator.model` in `WORKFLOW.md`). The previous `extension.linear_updater.*` namespace is also removed and rejected by the loader; orchestrator-session-driven Linear writes replace it.

Any invalid value or resolution failure (assignee cannot be resolved / empty `admit_states` / `WORKFLOW.md` path missing / token missing) **refuses startup** and emits the offending field in the structured log.
**`roki.toml` itself is not hot-reloaded; changing it requires a daemon restart.**

The exact name, default, and validation rule for each key live in the "roki.toml schema" table in [`docs/reference/config.md`](../reference/config.md).

### `WORKFLOW.md` (hot-reloadable, Liquid + Markdown)

A single per-workspace file. It consists of front matter (YAML or TOML) and template blocks, and contains:

- **Named template blocks**:
  - `prompt_template_orchestrator` (required) — for the long-lived orchestrator session A; rendered once at A launch. When rendering fails, a deterministic fallback (issue identifier / title / description only) is used so A can still be launched.
  - `prompt_template_<phase>` (optional, one per phase, named after the phase) — operator override for a specific phase subprocess's prompt; rendered against the per-phase context envelope and written to the subprocess's stdin. The phases for which this is defined are listed in the [phase catalog](18-worker-skill-workflow.md). When the block is absent the daemon uses the phase's catalog default (slash-command skill or daemon-internal prompt fragment) per [FR 18 §Phase override](18-worker-skill-workflow.md).
- **Reserved extension namespaces**: regions where each downstream spec places its own keys.
  - `extension.orchestrator.*` — orchestrator session A (model / effort / max_phases / allowed_tools).
  - `extension.phase.<name>.*` — per-phase override surface; the canonical key under each phase is `command`, the slash-command override that swaps the catalog default skill (per [FR 18 §Phase override](18-worker-skill-workflow.md)). Mutually exclusive per phase with `prompt_template_<phase>`.
  - `extension.server.*` — roki-observability HTTP API.
  - The loader **round-trips unknown keys** (it does not interpret them, and does not delete them). The legacy `extension.linear_updater.*`, `extension.gates.spec.*`, and `extension.gates.review.*` namespaces are rejected by the loader (or simply ignored as unknown); A's processing of `daemon_directive` events plus A's own artifact validation replaces them.

The consuming spec, requiredness, default, and behavior on invalid values for each namespace live in the "WORKFLOW.md schema" table in [`docs/reference/config.md`](../reference/config.md).

### Hot reload and validation

- **Schema validation failure at startup** → refuse to start + log the offending key path
- **Validation passes on hot reload** → apply the new policy
- **Validation fails on hot reload** → **keep the previous policy** + log the failure (do not stop the daemon)
- **Per-key invalidity inside `extension.*`** (e.g. non-positive `timeout_ms`) → the corresponding spec refuses evaluation + logs the misconfiguration

## Capabilities

- **One daemon for multiple repos**: a single developer runs a single daemon to handle tickets across multiple repos. The assignee filter ensures the daemon does not touch other people's tickets.
- **`me` resolution**: writing `me` for the assignee resolves to the API token holder.
- **Per-spec namespaces**: each downstream spec consumes only its own namespace (no cross-namespace dependencies).
- **Defaulted-key logging**: when an unspecified key falls back to its default, the startup log records which key did so.
- **Single workspace policy**: no per-repo override (only the per-workspace `WORKFLOW.md`).
- **Hot-reload safe**: invalid values do not crash the daemon (the previous policy is retained).

## Boundaries

- **Hot reload of `roki.toml`** is out of scope (only `WORKFLOW.md` is hot-reloadable).
- **Per-repo `WORKFLOW.md` overrides** are out of scope.
- **Per-issue / per-attempt config overrides** are out of scope.
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
- **Related FR**: 12-extension-surface (the contract for using extension namespaces)
