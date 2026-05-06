---
refs:
  id: fr:05-worktree-and-session
  kind: fr
  title: "Worktree and Session Lifecycle"
  spec: roki-skeleton
  related:
    - fr:02-configuration
    - fr:07-recovery
    - fr:04-phase-execution
    - fr:01-engine-model
    - fr:09-log-access-cli
---

# FR 05: Worktree and Session Lifecycle

> Per-ticket session tempdirs (always present once admitted) and per-ticket git worktrees (lazily materialized when a cycle reaches its first run phase). The daemon owns both directories' creation and deletion. Operators read paths through `roki repo` ([09-log-access-cli](09-log-access-cli.md)); phase subprocesses walk into the resolved path.

## Purpose

Concentrate worktree and session-tempdir lifecycle in the daemon so the operator-declared allowlist (`[[admission.repos]]` in WORKFLOW.toml) is the single boundary for git side effects. Lazy worktree creation avoids materializing a worktree for tickets that admit but never reach a `pre.directive: "run"` (e.g. tickets handled entirely by an admission-time cleanup entry).

## User-visible Behavior

### Session tempdir

- **Created at admission**: when the admission filter ([03-linear-admission §Admission filter](03-linear-admission.md)) accepts a ticket, the daemon creates `<session_root>/<ticket-id>/` (where `<session_root>` is `roki.toml [paths].session_root`). This directory is the per-iter capture root ([09-log-access-cli §Storage layout](09-log-access-cli.md)). It exists before any cycle starts so even pre-run inspection has somewhere to write logs.
- **Deleted on**: cleanup-cycle completion, admission-filter eviction (after any in-flight cycle terminates), and orphan reconciliation at cold start.

### Worktree

- **Created lazily**: when a cycle's pre returns `directive: "run"` and the worktree does not yet exist, the daemon creates it before spawning the run subprocess. The repo is the admission-resolved repo for this ticket ([03-linear-admission §Repo resolution](03-linear-admission.md)).
- **Tooling**: the daemon resolves the repo's local clone with `ghq list -p` and creates a worktree with `wt switch-create` (branch name = the Linear issue identifier verbatim). Idempotent on subsequent `directive: "run"` invocations: the daemon verifies the worktree's continued presence with `wt list` (or equivalent) without re-running `wt switch-create`. If the operator removed the worktree out-of-band between iterations, the daemon recreates it.
- **Working directory**: phase subprocesses are launched by the daemon with cwd set to the worktree if it exists, else to the **ghq base path** of the admission-resolved repo, per [04-phase-execution §Working directory](04-phase-execution.md). Session-shape subprocesses fix cwd at spawn (cycle start) — pre / post turns of a cycle whose worktree was created mid-cycle still use the cycle-start cwd. The session tempdir is for log capture, never used as cwd. Operators do not need to write `cd ...` inside the cli line. `roki repo` ([09-log-access-cli §`roki repo`](09-log-access-cli.md)) is provided for explicit lookups (TUI, external scripts, debugging) where the path must be named.
- **Reused across cycles**: the same worktree persists across cycles for the same ticket. New cycles inherit whatever git state the previous cycle left.
- **Branch name**: equals the Linear issue identifier verbatim. The daemon does not parse, transform, or namespace it.

### Cleanup

Auto-delete is gated by cycle kind ([01-engine-model §Cycle kinds](01-engine-model.md)): only `cleanup` cycles trigger auto-delete on completion. `rule` and `failure` cycle completions do not.

Three conditions actually invoke deletion:

1. **Cleanup cycle completion** (`cycle.kind == "cleanup"`): after the cycle's terminal directive is observed, the daemon enumerates worktrees in the allowlist whose branch name matches the issue identifier and runs `wt remove`, then `rm -rf` on the session tempdir. The branch itself is **not** deleted.
2. **Admission-filter eviction** (assignee revoked, repo allowlist match lost): the in-flight cycle (if any) runs to natural end first; afterward the daemon evicts and deletes as in (1).
3. **Orphan reconcile at cold start** ([07-recovery §Cold start](07-recovery.md)): residue not corresponding to any admission-passing Linear ticket is auto-deleted with a `reason: orphan` log entry.

Cleanup is a cycle kind, not a daemon-tracked state.

### Failure mode retention

When `[[on_failure]]` does not match a daemon-detected failure, the worktree and session tempdir are **retained** for forensics. Operators that want them cleaned up after a failure write a `[[cleanup]]` entry that triggers on whatever signal they choose (e.g. a Linear comment / label produced inside the failure-handler cycle's run / post phase).

When the daemon itself encounters a filesystem error during create or recover (worktree / session tempdir setup before a phase launch), it routes the failure through `[[on_failure]] when.kind = "fs_poison"` ([01-engine-model §Failure handling](01-engine-model.md)). Cleanup-time fs errors (worktree / session tempdir delete, orphan reconcile) do not match `[[on_failure]]` — they emit a structured event and add an escalation queue entry ([06-failure-handling §Escalation queue](06-failure-handling.md)).

### Multi-repo

One ticket → one repo by construction. The admission step resolves it; the worktree is for that repo only. Multi-repo concerns are operator-side: a pre that detects the work spans repos can return `directive: "end"` with whatever Linear write or label change it cares to make.

## Capabilities

- **Lazy worktree creation**: avoids unnecessary git operations for tickets whose first cycle ends at pre.
- **`wt` + `ghq` driven**: `ghq list -p` to resolve the local clone path, `wt switch-create` to materialize, `wt list` to verify, `wt remove` to delete.
- **Path safety**: any path that escapes the root after canonicalization (symlink / hardlink) or that conflicts with another ticket's path is rejected.
- **Discovery primitive**: cleanup and cold-start reconciliation both rely on "branch name = Linear issue identifier".
- **Per-issue keying**: both the session tempdir name and the worktree branch name are the Linear issue identifier alone. Repos may differ between tickets.
- **Operator access via `roki repo`**: phase subprocesses do not need to know the path layout; `roki repo` returns the right directory based on environment context.

## Boundaries

- **Branch deletion is not done**: the daemon `wt remove`s the worktree but leaves the branch. Operators / phase subprocesses delete branches if they want.
- **Container / VM isolation** is out of scope; the daemon depends on whatever sandbox the operator's cli line provides.
- **Editing code inside the worktree** is a phase subprocess responsibility; the daemon never touches the tree.
- **Multi-host / worktrees on remote machines** are out of scope.
- **Eager worktree creation** (creating the worktree at admission time) is rejected: tickets that never reach a `pre.directive: "run"` would otherwise materialize unused worktrees.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "Daemon-driven multi-repo worktree materialization via wt + ghq".
- **Requirements**:
  - `roki-mvp Req 4.3`, `Req 4.6` – `Req 4.9`: worktree creation, path safety, cleanup, terminal-failure retention, filesystem errors.
- **Design**:
  - `.kiro/specs/roki-mvp/design-worktree-workspace.md` (pending rewrite).
  - `Workspace Manager` section of `.kiro/specs/roki-mvp/design.md` (pending rewrite).
- **Related FR**: [02-configuration](02-configuration.md), [07-recovery](07-recovery.md), [04-phase-execution](04-phase-execution.md), [01-engine-model](01-engine-model.md), [09-log-access-cli](09-log-access-cli.md).
