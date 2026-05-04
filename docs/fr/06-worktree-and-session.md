---
refs:
  id: fr:06-worktree-and-session
  kind: fr
  title: "Worktree and Session Lifecycle"
  spec: roki-mvp
  implements:
    - req:roki-mvp:4
  related:
    - design:roki-mvp:worktree-workspace
---

# FR 06: Worktree and Session Lifecycle

> The daemon materializes / cleans up per-issue session tempdirs and git worktrees for the phase subprocesses to walk into.

## Purpose

Let phase subprocesses walk into a "prepared workspace". Concentrating worktree creation / deletion / path-safety verification in the daemon makes the operator-declared allowlist the single boundary for git side effects, so phase subprocesses can focus on implementing the ticket.

## User-visible Behavior

- **Right after the orchestrator session returns `judge=act` in its `admission_decision`** (with a single allowlisted `repo` populated):
  - The daemon resolves the repo's local clone with `ghq list -p`.
  - Creates a worktree with `wt` (branch name = the Linear issue identifier verbatim).
  - Creates a per-issue session tempdir under the platform's standard user cache root (directory name = the Linear issue identifier).
  - The resulting worktree path + session tempdir are passed to each phase subprocess via the daemon-controlled context envelope (per [07-worker-execution](07-worker-execution.md), [12-extension-surface](12-extension-surface.md)). The orchestrator itself never receives worktree paths — the orchestrator is filesystem-read-only and never produces code changes.
- **Cleanup triggers** (conditions to enter the `Cleaning` state):
  - The Linear issue transitioned to a terminal state (Done / Canceled / etc.), or
  - The Linear issue was reassigned to someone else.
  - The state is entered **only when the tracker observes** one of these. A clean subprocess exit alone does not cause this transition.
- **Cleanup behavior**:
  - If a phase subprocess or the orchestrator session is still running, terminate it first.
  - For every repo in the allowlist, enumerate worktrees and `wt remove` those whose branch name equals the Linear issue identifier verbatim.
  - Delete the session tempdir.
  - **Do not delete the branch.**
- **On TerminalFailure**: keep the worktree, the branch, and the session tempdir all intact (so the operator can inspect them).
- **On filesystem error**: mark the issue as failed, log the offending path, and refuse additional work until the operator intervenes (the daemon emits a `daemon_directive` event of `kind=fs_poison` to the orchestrator, which writes the matching Linear label + comment via Linear MCP — see [14-operator-notifications](14-operator-notifications.md)).

## Capabilities

- **Driven through `wt` + `ghq`**: worktrees are created with `wt switch-create` and removed with `wt remove`. The local location of each repo is resolved with `ghq list -p`.
- **Path safety**: any path that escapes the root after sanitization, that contains path traversal, or that conflicts with another worker's path is rejected. Paths that resolve outside the root after canonicalization (escapes via symlink / hardlink) are also rejected.
- **Discovery primitive**: cleanup and restart-time recovery ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)) both rely on the same "branch name = Linear issue identifier" scan.
- **Per-issue keying**: both the session tempdir and the worktree branch name are keyed by the Linear issue identifier alone. Repos may differ.

## Boundaries

- **Branch deletion is not done** (it is the responsibility of phase subprocesses / the operator).
- **Container / VM isolation** is out of scope (we depend on Claude Code's `workspace-write` sandbox for phase subprocesses plus path safety; the orchestrator always runs read-only).
- **Editing code inside the worktree** is a phase subprocess responsibility; the daemon never touches it.
- **Multi-host / worktrees on remote machines** are out of scope.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Daemon-driven multi-repo worktree materialization via wt + ghq"
- **Requirements**:
  - `roki-mvp Req 4.3`, `Req 4.6` - `Req 4.9`: worktree creation / path safety / cleanup / TerminalFailure retention / filesystem errors
- **Design**:
  - `.kiro/specs/roki-mvp/design-worktree-workspace.md`
  - `Workspace Manager` section of `.kiro/specs/roki-mvp/design.md`
- **Related FR**: 04-state-machine-and-recovery, 07-worker-execution, 14-operator-notifications
