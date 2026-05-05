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
    - fr:20-rule-and-cycle-engine
---

# FR 11: Agent Tool Boundary

> The principle that the daemon **never registers, proxies, or wraps any agent-side tool**. Every phase subprocess (pre / run / post, in either session or command shape) runs whatever cli line the operator wrote in WORKFLOW.toml or in its workflow/*.md frontmatter. The daemon does not parse permission flags, does not compose `--settings`, does not allowlist tools, and does not hold Linear write credentials.

## Purpose

Tools available to every phase subprocess — Linear / git / gh / shell / other MCP — match whatever the operator's cli line invokes, exactly. The daemon holds no Linear write credentials, agent-side tool changes are decoupled from daemon releases, and the safety posture is the operator's responsibility.

The previous orchestrator-vs-phase split is gone. Both shapes (long-lived AI session reused within a cycle and one-shot command per phase iteration) run with the cli line the operator authored. The orchestrator-specific `--settings` allowlist (Linear MCP write + Read + Bash + read-only filesystem sandbox) and the phase-specific permission strategy (`allowlist` / `dangerously-skip` / `refuse-to-start-when-unset`) are removed: operators that want a constrained tool surface put the relevant flags into their cli line; operators that want a free-for-all do likewise. The daemon never composes flags on the operator's behalf.

## User-visible Behavior

### The daemon adds nothing

- **Subprocess tool surface**: every phase subprocess sees exactly what the cli line invokes. The daemon adds nothing, removes nothing, and wraps nothing.
- **Linear writes**: every Linear write originates from inside a phase subprocess through whatever MCP / CLI / HTTP client the operator's cli line provides. The daemon process never issues a Linear write under any circumstance — including failure escalation, cleanup, and restart recovery.
- **`git` / `gh` / `ghq` / `wt` operations**: when a workflow template instructs an agent (or a shell script) to use these, the actual tools come from `PATH` and from MCP servers the operator's cli line invokes. The daemon does not intercept, substitute, or augment.
- **Secrets**: secrets held by the daemon (Linear API token, webhook secret, operator-declared secrets in `roki.toml`) are **never** placed in prompt input, captures, environment variables given to phase subprocesses, or structured log entries. The redaction layer in [13-observability-logs](13-observability-logs.md) enforces this at log emit time.

### Pass-through cli lines

Operators choose their safety posture by what they put on the cli line. A few examples:

```toml
# Strictly read-only session, allowlist-driven
[default.ai.session]
cli = "claude --input-format stream-json --output-format stream-json --model claude-opus-4-7 --settings ~/.config/roki/orchestrator.settings.json"

# Yolo command for short-lived implementation work
[default.ai.command]
cli = "claude -p '{{ prompt }}' --output-format stream-json --max-turns 100 --dangerously-skip-permissions"
```

Per-phase override is via the workflow/*.md frontmatter `cli` field (command shape) or via the inline `*.cmd` field. Neither route hands the daemon any decision power over the resulting tool surface.

### Operator responsibilities

Because the daemon adds nothing, the operator owns:

- Choosing a cli line whose default sandbox / allowlist matches the work each phase performs.
- Installing the MCP servers each phase needs (Linear MCP if Linear writes are wanted, git / gh / language-specific MCPs as the workflow requires).
- Auditing the resulting safety posture (e.g. ensuring `--dangerously-skip-permissions` is only used where appropriate).

## Capabilities

- **No daemon-registered tools at all**: the daemon does not expose any mutating tool, does not register any read-only self-diagnosis tool, and does not run an MCP server of its own.
- **No proxy/wrap**: a tool call from any phase subprocess never goes through the daemon.
- **One pass-through path for both shapes**: session and command subprocesses both run their operator-authored cli lines verbatim. No `--settings` composition, no permission-strategy switch, no allowlist mutation.
- **Secret isolation**: secrets held by the daemon are redacted at prompt-build time and at log-emit time. They never appear in subprocess stdin, env, or capture files.

## Boundaries

- **Installing / configuring MCP servers** belongs to the operator (out of the daemon's concern).
- **The agent's policy for tool use** (which tool to call in which order) is the cli line's / prompt's / skill's responsibility.
- **Adding mutating agent-facing tools** to the daemon is permanently out of scope.
- **Adding read-only self-diagnosis tools** to the daemon is permanently out of scope. Operators that want to inspect daemon state from inside a phase use the public observability surface (`roki log`, `roki events`, the HTTP API).
- **Daemon-side allowlists / sandbox profiles** are out of scope; the cli line is the only knob.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > Out > "Daemon-registered, daemon-proxied, or daemon-wrapped agent-side tools of any kind"; Boundary Strategy > "Agent-side tool surface (no daemon registration)".
- **Requirements**:
  - `req:roki-mvp:7`: Agent Tooling Boundary.
- **Design**:
  - `Engine Adapter` section of `.kiro/specs/roki-mvp/design.md` (pending rewrite).
- **Related FR**: [07-worker-execution](07-worker-execution.md), [14-operator-notifications](14-operator-notifications.md), [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md).
