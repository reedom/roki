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

> A short pre-flight one-shot `claude` invocation that classifies which repo (or repos) an admitted ticket targets.

## Purpose

Decide which repo (possibly more than one) the ticket actually targets before launching a worker. Delegating this decision to a dedicated Claude turn — rather than embedding it in daemon logic — keeps ticket interpretation agentic, while leaving the daemon to focus on allowlist validation and worktree materialization.

## User-visible Behavior

- **Right after admission**: the orchestrator transitions the issue to `Judging` → the daemon renders `prompt_template_setup` and invokes `claude` once.
- **Judge output**: structured findings on stdout (`action`: `act` or `noop`; for `act`, a list of repo identifiers).
- **`act` + every repo in the allowlist**: create the worktree → create the session tempdir → launch the main worker ([06-worktree-and-session](06-worktree-and-session.md) / [07-worker-execution](07-worker-execution.md)).
- **`act` but containing repos not in the allowlist**: route to `Skipped`, log the offending identifier and the contents of the allowlist.
- **`noop`**: do not create a session tempdir, do not create a worktree, do not launch a worker; go straight to `Skipped` (terminal).
- **Parse failure / timeout**: retry once with the same input. If it still fails, go to `TerminalFailure`, save the raw stdout in the structured log, and notify the operator on Slack ([14-operator-notifications](14-operator-notifications.md)).

## Capabilities

- **Dedicated prompt block**: uses the `prompt_template_setup` block in the workspace-level `WORKFLOW.md`. The issue's identifier / title / description / labels are passed as named variables.
- **Judging model**: configurable (default is a small model in the same Claude family as the worker).
- **Sandbox enforcement**: the judge subprocess is launched with **read-only sandbox + elicitations rejected**, ignoring any operator override.
- **Multi-repo from day one**: if the judge returns multiple repos, the run becomes a multi-repo run as-is.
- **Allowlist validation**: if even one of the repo identifiers returned by the judge is outside the allowlist, the entire `act` decision is rejected (no partial acceptance).
- **Per-attempt timeout**: an attempt that exceeds the configured timeout is treated as a failure.

## Boundaries

- **The content of the judge prompt itself** is defined by the operator inside `WORKFLOW.md` (the daemon does not embed any prompt).
- **No semantic validation of the judge's output** is performed (only parsing and allowlist checking).
- **Session tempdir / worktree creation is not done by the judge itself** ([06-worktree-and-session](06-worktree-and-session.md) owns that; the judge only classifies).

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Pre-flight setup judge"; Boundary Strategy
- **Requirements**:
  - `roki-mvp Req 4.1`, `Req 4.2`, `Req 4.4`, `Req 4.5`: Judge invocation, allowlist validation, noop, retry
  - `roki-mvp Req 9.6`: Read-only sandbox enforcement for the judge subprocess
- **Design**:
  - `Setup Judge` section of `.kiro/specs/roki-mvp/design.md`
  - `.kiro/specs/roki-mvp/design-agent-driven-repo-selection.md`
- **Related FR**: 02-configuration, 06-worktree-and-session, 07-worker-execution
