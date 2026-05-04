---
refs:
  id: fr:11-agent-tool-boundary
  kind: fr
  title: "Agent Tool Boundary"
  spec: roki-mvp
  implements:
    - req:roki-mvp:7
  related:
    - fr:07-worker-execution
    - fr:14-operator-notifications
    - fr:18-worker-skill-workflow
    - fr:19-orchestrator-session
---

# FR 11: Agent Tool Boundary

> The principle that the daemon **never registers, proxies, or wraps any agent-side tool**. Both agent invocation roles — the long-lived **orchestrator session A** and the short-lived **phase subprocesses** — inherit the operator's local Claude Code tool surface verbatim, narrowed only by per-process `allowed_tools` lists the daemon assembles from configuration.

## Purpose

Guarantee that the tools available to every agent invocation — Linear / git / gh / shell / other MCP — match exactly what the operator's Claude Code installation exposes. This way the daemon does not need to hold Linear write credentials, and additions / updates / replacements of agent-side tools are decoupled from daemon releases.

The same principle applies to both agent invocation roles:

- **Orchestrator session A** ([FR 19](19-orchestrator-session.md)) — long-lived `claude --input-format stream-json --output-format stream-json` per ticket. Tool surface restricted to **Linear MCP (write)** + **Read** + **Bash** only via `--settings`. No Edit, no Write, no Agent dispatch, no other MCPs. Bash invocations execute inside a read-only filesystem sandbox (regardless of operator overrides) so they cannot mutate the worktree or session tempdir; they are intended for artifact validation (`stat`, `test -f`, `grep -E`) per [FR 19 §Artifact validation](19-orchestrator-session.md).
- **Phase subprocesses** ([FR 18](18-worker-skill-workflow.md), [FR 07](07-worker-execution.md)) — short-lived `claude -p '/<kiro-skill> <args>' --output-format stream-json` per phase A nominates. Tool surface = the operator's full Claude Code installation (built-ins + their MCPs), narrowed only by the per-phase `allowed_tools` list and the configured permission strategy ([FR 07 §Permission strategy](07-worker-execution.md)).

Neither is a special daemon-internal route — both are `claude` subprocesses inheriting the operator's MCP surface.

There are no agent-side self-diagnosis tools. The prior gate specs registered `kiro_spec_status` / `kiro_review_status` read-only tools for the daemon-side gate state; with gates absorbed into A those tools are removed. A reads `requirements.md` / `review.md` directly via `Read` and validates them via `Bash`; phase subprocesses needing prior-phase context inherit it through the engine adapter's `additional_context` channel ([FR 19 §Event catalog](19-orchestrator-session.md)).

## User-visible Behavior

### Principle: the daemon adds nothing

- **Subprocess tool surface**: every agent subprocess (orchestrator session A, every phase subprocess) sees the built-in tool set exposed by the operator's Claude Code installation plus the MCP servers the operator has installed, as-is. The daemon adds nothing, removes nothing, and wraps nothing — it only narrows via `--settings` per process.
- **Linear writes**: every Linear write originates from an agent invocation through the operator-configured Linear MCP. Specifically:
  - **A** writes Linear via Linear MCP for admission rejections (`needs_split` / `allowlist_rejected`) and for daemon-only failure directives (`daemon_directive` events — see [FR 14: Operator Notifications](14-operator-notifications.md)).
  - **Phase subprocesses** write Linear via Linear MCP for normal in-phase status updates / PR linkage / fix-finding context, where the per-phase `allowed_tools` permits.
  - The daemon process never issues a Linear write.
- **`git` / `gh` / `ghq` / `wt` operations**: when the prompt instructs an agent to use these, the actual tools come from Bash + MCP servers in the operator's Claude Code installation. The daemon does not intercept, substitute, or augment them. A's Bash runs inside a read-only filesystem sandbox so any mutating call (`git commit`, `gh pr create`, `wt switch-create`, etc.) originates from a phase subprocess; A's Bash is for read-only artifact validation only.
- **Secrets**: secrets held by the daemon (Linear API token / webhook secret / etc.) are **never** placed in prompt input, logs, or any other artifact reachable from inside any agent subprocess (see the redaction layer in [13-observability-logs](13-observability-logs.md)).

## Capabilities

- **No daemon-registered tools at all**: the daemon does not expose any mutating tool, and no longer registers any read-only self-diagnosis tool either (the prior `kiro_spec_status` / `kiro_review_status` were tied to gates that have been removed).
- **No proxy/wrap**: a tool call from any agent subprocess never goes through the daemon.
- **Same boundary across both roles**: A and every phase subprocess inherit the operator's tool surface; per-process `allowed_tools` narrows. A's surface is the narrower of the two (Linear MCP write + Read + Bash, with Bash running inside a read-only filesystem sandbox). Linear writes from A and from phase subprocesses both go through the operator's Linear MCP, not through any daemon-side write path.
- **Secret isolation**: any field that may contain a secret is redacted at the prompt-build phase and the log-emit phase.

## Boundaries

- **Installing / configuring MCP servers** belongs to the operator's Claude Code installation (out of the daemon's concern).
- **The agent's policy for tool use** (which tool to call in which order) is the agent-side kiro skill's responsibility (for phase subprocesses) or A's `prompt_template_orchestrator` instructions (for A), decided by the prompt and the skill.
- **Adding mutating agent-facing tools** is permanently out of scope.
- **Adding read-only self-diagnosis tools** is also out of scope at this layer — A reads workspace state directly via `Read` + `Bash` and inherits prior-phase context through `additional_context`.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > Out > "Daemon-registered, daemon-proxied, or daemon-wrapped agent-side tools of any kind"; Boundary Strategy > "Agent-side tool surface (no daemon registration)"
- **Requirements**:
  - `req:roki-mvp:7`: Agent Tooling Boundary
- **Design**:
  - `Engine Adapter` section of `.kiro/specs/roki-mvp/design.md`
- **Related FR**: [07-worker-execution](07-worker-execution.md), [14-operator-notifications](14-operator-notifications.md), [18-worker-skill-workflow](18-worker-skill-workflow.md), [19-orchestrator-session](19-orchestrator-session.md)
