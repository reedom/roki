# FR 06: Worktree and Session Lifecycle

> The daemon materializes / cleans up per-issue session tempdirs and git worktrees. The path-safety module is reused by the distill phase as well.

## Purpose

Let the agent walk into a "prepared workspace". Concentrating worktree creation / deletion / path-safety verification in the daemon makes the operator-declared allowlist the single boundary for git side effects, so the agent can focus on implementing the ticket. The path-safety module is also reused by the manifest validation in [10-post-merge-distill](10-post-merge-distill.md).

## User-visible Behavior

- **Right after the judge returns `act`**:
  - The daemon resolves each repo's local clone with `ghq list -p`.
  - Creates a worktree with `wt` (branch name = the Linear issue identifier verbatim).
  - Creates a per-issue session tempdir under the platform's standard user cache root (directory name = the Linear issue identifier).
  - Passes the resulting worktree paths to `prompt_template_worker` as named template variables.
- **Cleanup triggers** (conditions to enter the `Cleaning` state):
  - The Linear issue transitioned to a terminal state (Done / Canceled / etc.), or
  - The Linear issue was reassigned to someone else.
  - The state is entered **only when the tracker observes** one of these. A clean subprocess exit alone does not cause this transition.
- **Cleanup behavior**:
  - If a worker is still running, terminate it first.
  - For every repo in the allowlist, enumerate worktrees and `wt remove` those whose branch name equals the Linear issue identifier verbatim.
  - Delete the session tempdir.
  - **Do not delete the branch.**
- **On TerminalFailure**: keep the worktree, the branch, and the session tempdir all intact (so the operator can inspect them).
- **On filesystem error**: mark the issue as failed, log the offending path, and refuse additional work until the operator intervenes (a Slack notification is also fired → [14-operator-notifications](14-operator-notifications.md)).

## Capabilities

- **Driven through `wt` + `ghq`**: worktrees are created with `wt switch-create` and removed with `wt remove`. The local location of each repo is resolved with `ghq list -p`.
- **Path safety**: any path that escapes the root after sanitization, that contains path traversal, or that conflicts with another worker's path is rejected. Paths that resolve outside the root after canonicalization (escapes via symlink / hardlink) are also rejected.
- **Discovery primitive**: cleanup and restart-time recovery ([04-state-machine-and-recovery](04-state-machine-and-recovery.md)) both rely on the same "branch name = Linear issue identifier" scan.
- **Per-issue keying**: both the session tempdir and the worktree branch name are keyed by the Linear issue identifier alone. Repos may differ.
- **Public path-safety module**: reused by the manifest validation in [10-post-merge-distill](10-post-merge-distill.md).

## Boundaries

- **Branch deletion is not done** (it is the responsibility of the agent / operator).
- **Container / VM isolation** is out of scope (we depend on Claude Code's `workspace-write` sandbox plus path safety).
- **Editing code inside the worktree** is the agent's responsibility; the daemon never touches it.
- **Multi-host / worktrees on remote machines** are out of scope.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Daemon-driven multi-repo worktree materialization via wt + ghq"
- **Requirements**:
  - `roki-mvp Req 4.3`, `Req 4.6` - `Req 4.9`: worktree creation / path safety / cleanup / TerminalFailure retention / filesystem errors
  - `roki-distill-postmerge Req 11`: contract for reusing the same path-safety module
- **Design**:
  - `.kiro/specs/roki-mvp/design-worktree-workspace.md`
  - `Workspace Manager` section of `.kiro/specs/roki-mvp/design.md`
- **Related FR**: 04-state-machine-and-recovery, 10-post-merge-distill
