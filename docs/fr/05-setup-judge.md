---
refs:
  id: fr:05-setup-judge
  kind: fr
  title: "Setup Judge"
  spec: roki-mvp
  implements:
    - req:roki-mvp:4
---

# FR 05: Setup Judge

> A short pre-flight one-shot `claude` invocation that classifies whether to run an admitted ticket, and if so against which **single** allowlisted repo. Multi-repo classifications are rejected back to the operator via linear-updater.

## Purpose

Decide whether the ticket actually needs work and, if so, which one repo it targets, before launching a worker. Delegating this decision to a dedicated Claude turn — rather than embedding it in daemon logic — keeps ticket interpretation agentic, while leaving the daemon to focus on allowlist validation, single-repo enforcement, and worktree materialization.

A single-repo constraint is intentional: tickets that genuinely span multiple repos are a Linear-side decomposition problem, not a roki orchestration problem. roki rejects multi-repo classifications back to the operator (via the linear-updater subagent → Linear label + comment) so the operator can split the ticket. This keeps the per-issue state machine simple (one issue = one worktree, keyed by Linear issue id alone) and avoids cross-repo DAGs / coordination.

## User-visible Behavior

- **Right after admission**: the orchestrator transitions the issue to `Judging` → the daemon renders `prompt_template_setup` and invokes `claude` once.
- **Judge output**: structured findings on stdout (`action`: `act` or `noop`; for `act`, a list of repository identifiers).
- **`act` + exactly one repo, in allowlist**: create the worktree → create the session tempdir → publish the `Judging → Active` transition (vetoable by [08-pre-implementation-gate](08-pre-implementation-gate.md)) → on `Allow` launch the main worker ([06-worktree-and-session](06-worktree-and-session.md) / [07-worker-execution](07-worker-execution.md)).
- **`act` + two or more repos**: route to `Inactive(reason=needs_split)`, dispatch the linear-updater subagent with a `needs_split` directive (label addition + structured comment listing the classified repos), log the rejection. No session tempdir, no worktree, no worker launch.
- **`act` + a single repo not in allowlist**: route to `Inactive(reason=allowlist_rejected)`, dispatch linear-updater with an `allowlist_rejected` directive, log the offending identifier and the configured allowlist contents.
- **`noop`**: route to `Inactive(reason=noop)` directly. No session tempdir, no worktree, no worker launch.
- **Parse failure / timeout**: retry once with the same input. If it still fails, route to `Inactive(reason=judge_unparseable)`, dispatch linear-updater with a `judge_unparseable` directive, save the raw stdout in the structured log.

## Capabilities

- **Dedicated prompt block**: uses the `prompt_template_setup` block in the workspace-level `WORKFLOW.md`. The issue's identifier / title / description / labels are passed as named variables.
- **Judge model**: configurable (default is a small model in the same Claude family as the worker).
- **Sandbox enforcement**: the judge subprocess is launched with **read-only filesystem sandbox + elicitations rejected**, ignoring any operator override. Whether the judge can call write-capable Linear MCP tools is governed by the operator's Claude Code MCP allowlist; the daemon does not restrict that surface.
- **Single-repo enforcement**: validation accepts exactly one allowlisted repo. Multi-repo and out-of-allowlist findings each have their own `Inactive` reason and corresponding linear-updater directive.
- **Per-attempt timeout**: an attempt that exceeds the configured timeout is treated as a failure.

## Boundaries

- **The content of the judge prompt itself** is defined by the operator inside `WORKFLOW.md` (the daemon does not embed any prompt).
- **No semantic validation of the judge's output** is performed (only parsing and allowlist + cardinality checking).
- **Session tempdir / worktree creation is not done by the judge itself** ([06-worktree-and-session](06-worktree-and-session.md) owns that; the judge only classifies).
- **Multi-repo orchestration / cross-repo DAGs** are out of scope. A multi-repo ticket is rejected; the operator splits it.
- **Daemon-side Linear writes for the rejection feedback** do not exist; the linear-updater subagent (an agent invocation, not a daemon write path) performs the Linear label + comment via the operator's installed Linear MCP ([14-operator-notifications](14-operator-notifications.md)).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Pre-flight setup judge"; Boundary Strategy
- **Requirements**:
  - `roki-mvp Req 4.1`, `Req 4.2`, `Req 4.4`, `Req 4.5`: Judge invocation, single-repo + allowlist validation, multi-repo rejection, noop, retry
  - `roki-mvp Req 9.6`: Read-only filesystem sandbox enforcement for the judge subprocess
  - `roki-mvp Req 5.10`: linear-updater dispatch contract (used for rejection feedback)
- **Design**:
  - `Setup Judge` section of `.kiro/specs/roki-mvp/design.md`
  - `.kiro/specs/roki-mvp/design-agent-driven-repo-selection.md`
- **Related FR**: 02-configuration, 06-worktree-and-session, 07-worker-execution, 14-operator-notifications
