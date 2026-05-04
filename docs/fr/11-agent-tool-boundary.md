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
    - fr:08-pre-implementation-gate
    - fr:09-pre-pr-gate
    - fr:14-operator-notifications
    - fr:18-worker-skill-workflow
    - fr:19-orchestrator-session
---

# FR 11: Agent Tool Boundary

> The principle that the daemon **never registers, proxies, or wraps any agent-side tool**. Both agent invocation roles — the long-lived **orchestrator session A** and the short-lived **phase subprocesses** — inherit the operator's local Claude Code tool surface verbatim, narrowed only by per-process `allowed_tools` lists the daemon assembles from configuration. Each downstream gate spec may register exactly one **read-only self-diagnosis tool** for the agent's benefit.

## Purpose

Guarantee that the tools available to every agent invocation — Linear / git / gh / shell / other MCP — match exactly what the operator's Claude Code installation exposes. This way the daemon does not need to hold Linear write credentials, and additions / updates / replacements of agent-side tools are decoupled from daemon releases.

The same principle applies to both agent invocation roles:

- **Orchestrator session A** ([FR 19](19-orchestrator-session.md)) — long-lived `claude --input-format stream-json --output-format stream-json` per ticket. Tool surface restricted to **Linear MCP (write)** + **Read** only via `--settings`. No Bash, no Edit, no Write, no Agent dispatch, no other MCPs. Read-only filesystem sandbox regardless of operator overrides.
- **Phase subprocesses** ([FR 18](18-worker-skill-workflow.md), [FR 07](07-worker-execution.md)) — short-lived `claude -p '/<kiro-skill> <args>' --output-format stream-json` per phase A nominates. Tool surface = the operator's full Claude Code installation (built-ins + their MCPs), narrowed only by the per-phase `allowed_tools` list and the configured permission strategy ([FR 07 §Permission strategy](07-worker-execution.md)).

Neither is a special daemon-internal route — both are `claude` subprocesses inheriting the operator's MCP surface.

The narrow exception is the read-only self-diagnosis tools registered by gate specs. These let an agent invocation ask the daemon "what is my own gate state right now?" without granting any mutation power.

## User-visible Behavior

### Principle: the daemon adds nothing

- **Subprocess tool surface**: every agent subprocess (orchestrator session A, every phase subprocess) sees the built-in tool set exposed by the operator's Claude Code installation plus the MCP servers the operator has installed, as-is. The daemon adds nothing, removes nothing, and wraps nothing — it only narrows via `--settings` per process.
- **Linear writes**: every Linear write originates from an agent invocation through the operator-configured Linear MCP. Specifically:
  - **A** writes Linear via Linear MCP for admission rejections (`needs_split` / `allowlist_rejected`) and for daemon-only failure directives (`daemon_directive` events — see [FR 14: Operator Notifications](14-operator-notifications.md)).
  - **Phase subprocesses** write Linear via Linear MCP for normal in-phase status updates / PR linkage / fix-finding context, where the per-phase `allowed_tools` permits.
  - The daemon process never issues a Linear write.
- **`git` / `gh` / `ghq` / `wt` operations**: when the prompt instructs an agent to use these, the actual tools come from Bash + MCP servers in the operator's Claude Code installation. The daemon does not intercept, substitute, or augment them. A cannot reach these tools — its surface is Linear MCP + Read only — so any `git` / `gh` / shell call originates from a phase subprocess.
- **Secrets**: secrets held by the daemon (Linear API token / webhook secret / etc.) are **never** placed in prompt input, logs, or any other artifact reachable from inside any agent subprocess (see the redaction layer in [13-observability-logs](13-observability-logs.md)).

### Exception: read-only self-diagnosis tools registered by gates

Each gate spec may register exactly one **read-only tool** that lets the agent inspect that gate's state. Detailed contracts live in the consuming spec:

| Tool | Registered by | Detailed contract |
|---|---|---|
| `kiro_spec_status` | roki-spec-gate | [08-pre-implementation-gate](08-pre-implementation-gate.md) |
| `kiro_review_status` | roki-review-gate | [09-pre-pr-gate](09-pre-pr-gate.md) |

Common constraints (enforced by both gate specs):

- **Read-only**: no mutating action is provided (cannot, for example, force a retry).
- **Per-`(repo, issue)` independence**: cross-`(repo, issue)` listing is not supported.
- **Latest snapshot only**: historical retrieval is out of scope.
- **Secret isolation**: the response inherits the daemon's redaction policy.
- **No gate bypass**: the tool returns the daemon's view; an agent invocation reading it does not change the gate's outcome. It is a self-diagnostic, not a side-channel.

These tools are typically used by a phase subprocess's kiro skill to read context like "what's the artifact path for this issue's `requirements.md`?" or "what was the last review-gate failure reason on the previous turn?" — they do not gate the phase's own loop. A's narrow surface (Linear MCP write + Read) does not include these tools.

## Capabilities

- **No daemon-registered mutating tools**: the daemon does not expose any mutating tool to any agent subprocess.
- **No proxy/wrap**: a tool call from any agent subprocess never goes through the daemon.
- **Same boundary across both roles**: A and every phase subprocess inherit the operator's tool surface; per-process `allowed_tools` narrows. A's surface is the narrowest (Linear MCP write + Read only). Linear writes from A and from phase subprocesses both go through the operator's Linear MCP, not through any daemon-side write path.
- **Secret isolation**: any field that may contain a secret is redacted at the prompt-build phase and the log-emit phase.
- **Self-check tool exception**: each gate may register a single read-only tool (only reads state, so the agent can inspect the daemon's truth). The contract is defined inside the gate's own FR.

## Boundaries

- **Installing / configuring MCP servers** belongs to the operator's Claude Code installation (out of the daemon's concern).
- **The agent's policy for tool use** (which tool to call in which order) is the agent-side kiro skill's responsibility (for phase subprocesses) or A's `prompt_template_orchestrator` instructions (for A), decided by the prompt and the skill.
- **Adding mutating agent-facing tools** is permanently out of scope (including for gates).
- **Detailed contract for `kiro_spec_status` / `kiro_review_status`** (input shape, response fields, edge cases) lives in the consuming gate FR — this FR only states the principle and the constraint envelope.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > Out > "Daemon-registered, daemon-proxied, or daemon-wrapped agent-side tools of any kind"; Boundary Strategy > "Agent-side tool surface (no daemon registration)"
- **Requirements**:
  - `req:roki-mvp:7`: Agent Tooling Boundary
  - `roki-spec-gate Req 6`: kiro_spec_status Read-Only Agent Tool (detailed contract)
  - `roki-review-gate Req 7`: Read-Only kiro_review_status Tool (detailed contract)
- **Design**:
  - `Engine Adapter` section of `.kiro/specs/roki-mvp/design.md`
  - `Agent Tool` section of `.kiro/specs/roki-spec-gate/design.md`
  - `Agent Tool` section of `.kiro/specs/roki-review-gate/design.md`
- **Related FR**: [07-worker-execution](07-worker-execution.md), [08-pre-implementation-gate](08-pre-implementation-gate.md), [09-pre-pr-gate](09-pre-pr-gate.md), [14-operator-notifications](14-operator-notifications.md), [18-worker-skill-workflow](18-worker-skill-workflow.md), [19-orchestrator-session](19-orchestrator-session.md)
