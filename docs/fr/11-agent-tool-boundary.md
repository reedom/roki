---
refs:
  id: fr:11-agent-tool-boundary
  kind: fr
  title: "Agent Tool Boundary"
  spec: roki-mvp
  implements:
    - req:roki-mvp:7
---

# FR 11: Agent Tool Boundary

> The principle that the daemon never registers / proxies / wraps any agent-side tool. The exception: each gate spec registers exactly one **read-only self-diagnosis tool** (`kiro_spec_status` / `kiro_review_status`).

## Purpose

Guarantee that **the tools available to the agent (Linear / git / gh / shell / other MCP) match exactly what the operator's Claude Code installation exposes**. This way the daemon does not need to hold Linear write credentials, and additions / updates / replacements of agent-side tools are decoupled from daemon releases. Self-diagnosis tools introduced by gates are treated as exceptions, **constrained to non-mutating local self-checks only**.

## User-visible Behavior

### Principle: the daemon adds nothing

- **Worker subprocess tool surface**: the worker sees the built-in tool set exposed by the operator's Claude Code installation plus the MCP servers the operator has installed, as-is. The daemon adds nothing, removes nothing, and wraps nothing.
- **Linear writes**: the agent writes through the operator-configured Linear MCP integration. The daemon process never issues a Linear write.
- **`git` / `gh` / `ghq` / `wt` operations**: when the prompt instructs the agent to use these, the actual tools come from Bash + MCP servers in the operator's Claude Code installation. The daemon does not intercept, substitute, or augment them.
- **Secrets**: secrets held by the daemon (Linear API token / webhook secret / etc.) are **never** placed in prompt input, logs, or any other artifact reachable from inside the worker (see the redaction layer in [13-observability-logs](13-observability-logs.md)).

### Exception: the self-diagnosis tools each gate registers

Each gate spec registers exactly one **read-only tool of its own**:

#### `kiro_spec_status` (registered by roki-spec-gate)

- Input: a `(repo, issue)` reference.
- Response: current spec artifact path / artifact-present flag / latest validation outcome / attempt count / remaining attempts.
- Read-only: never mutates the gate state or any on-disk content.
- Unknown `(repo, issue)`: a key not tracked by the orchestrator → return a structured not-found, never raise an error to the orchestrator.
- Secrets: the response does not include the Linear API token / daemon credentials / workspace paths of other issues.

#### `kiro_review_status` (registered by roki-review-gate)

- Input: the current `(repo, issue)`.
- Response: artifact presence flag / latest gate result / current attempt counter / configured `max_attempts` / latest failure reason.
- Read-only: does not mutate state, no side effect on the workspace, does not call Linear / `gh`.
- View consistent with the daemon: identical to the basis of the most recent veto / allow decision (the agent cannot hold a divergent view).
- Secrets: inherits the redaction policy of the tool registry.

Common constraints for both tools:

- No mutating action is provided (cannot, for example, force a retry).
- No cross-`(repo, issue)` listing is supported.
- Historical retrieval is out of scope (latest snapshot only).

## Capabilities

- **No daemon-registered tools (mutating)**: the daemon does not expose any mutating tool to the agent.
- **No proxy/wrap**: a tool call from the agent never goes through the daemon.
- **Secret isolation**: any field that may contain a secret is redacted at the prompt-build phase and the log-emit phase.
- **Self-check tool exception**: each gate may register a single read-only tool (only reads state, so the agent can decide based on the daemon's truth).

## Boundaries

- **Installing / configuring MCP servers** belongs to the operator's Claude Code installation (out of the daemon's concern).
- **The agent's policy for tool use** (which tool to call in which order) is the agent-side kiro skill's responsibility, decided by the prompt and the skill.
- **Adding mutating agent-facing tools** is permanently out of scope (including for gates).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > Out > "Daemon-registered, daemon-proxied, or daemon-wrapped agent-side tools of any kind"; Boundary Strategy > "Agent-side tool surface (no daemon registration)"
- **Requirements**:
  - `roki-mvp Req 7`: Agent Tooling Boundary
  - `roki-spec-gate Req 6`: kiro_spec_status Read-Only Agent Tool
  - `roki-review-gate Req 7`: Read-Only kiro_review_status Tool
- **Design**:
  - `Engine Adapter` section of `.kiro/specs/roki-mvp/design.md`
  - `Agent Tool` section of `.kiro/specs/roki-spec-gate/design.md`
  - `Agent Tool` section of `.kiro/specs/roki-review-gate/design.md`
- **Related FR**: 07-worker-execution, 08-pre-implementation-gate, 09-pre-pr-gate
