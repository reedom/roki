# Workspace Model: switch from sandbox dirs to git worktrees — Task 6.1

Status: PROPOSAL — needs user sign-off before task 6.1 is opened.

User pre-locked all 6 decisions; this doc is the implementation contract.

## What changes

Today (roki-mvp 5.1):

- `RepoConfig.path` = absolute local path to a Git working tree (operator-supplied).
- Workspace = empty `<workspace_root>/<repo>/<issue>/` — agent must `git clone` into it.
- Cleanup = `tokio::fs::remove_dir_all`.

After 6.1 (monorail-aligned):

- `RepoConfig.repo` = repo identifier (`owner/repo` or `host/owner/repo`); local path is resolved at runtime via `ghq`.
- Workspace = real `git worktree` of the configured repo, branch named exactly the issue id, sibling-pathed at `{repo_path}/../{repo_name}.{issue}` per `wt`'s convention.
- Cleanup = `wt remove`.
- On `TerminalFailure`: worktree dir AND branch are retained (operator can `cd <repo_path>` and `git checkout <issue-id>` to inspect).

## Decision matrix (locked by user)

| # | Decision | Locked answer |
|---|---|---|
| 1 | Worktree backend | Use `wt` (worktrunk) external CLI. Operator installs; daemon assumes on `$PATH`. |
| 2 | Repo discovery | Use `ghq` external CLI. Operator installs; `RepoConfig` carries an `owner/repo` (or `host/owner/repo`) identifier; local path resolved at runtime via `ghq list -p` / `ghq get`. |
| 3 | Branch name | The Linear issue id verbatim (e.g., `ENG-42`). |
| 4 | Worktree path layout | `{repo_path}/../{repo_name}.{issue}` (sibling of the original repo, matching monorail's `WtTool::switch_create`). |
| 5 | Cleanup on `Cleaning` | `wt remove`. (Branch is not deleted — `wt remove` does not delete branches.) |
| 6 | Retention on `TerminalFailure` | Keep worktree dir AND branch. Just don't call `wt remove`. |

## Schema delta (additive + one rename)

`Config`:

```toml
# REMOVED — sibling layout doesn't need a workspace root
# workspace_root = "..."

[[repos]]
id = "scratch"                                    # unchanged — stable identifier for (repo, issue) keys
repo = "owner/scratch"                            # NEW — ghq identifier, replaces `path`
# path = "/abs/path"                              # REMOVED
workflow_path = "/abs/path/WORKFLOW.md"           # unchanged
webhook_secret_env = "ROKI_WEBHOOK_SECRET_SCRATCH"
[repos.scope]
kind = "team"
key = "ENG"
```

Notes:
- `workspace_root` is dropped. The path layout has no per-daemon root anymore — every worktree lives next to its source repo.
- `path` is renamed to `repo` (ghq identifier). This is a **breaking change** for any existing config; since the daemon hasn't shipped, this is acceptable.
- The `Workspace` trait keeps its current shape; the implementation rewrites.

## New tools (monorail-aligned)

Following monorail's `src/tools/` pattern:

- `crates/roki-daemon/src/tools/wt.rs` — `WtTool` trait + `RealWt` impl. Methods: `switch_create(repo_path, branch) -> PathBuf` (computes the sibling path), `remove(worktree_path)`. Sanitization rule: branch chars outside `[A-Za-z0-9_-]` → `-` (matches monorail).
- `crates/roki-daemon/src/tools/ghq.rs` — `GhqTool` trait + `RealGhq` impl. Methods: `list_path(full) -> Option<PathBuf>` (lookup), `ensure_cloned(full) -> PathBuf` (lookup-or-clone).

These live alongside the existing `crates/roki-daemon/src/tools/linear_graphql.rs` (registry-mounted agent tool). The new tools are NOT agent-facing — they're daemon-internal CLIs used by the workspace boundary.

## Workspace module rewrite

`crates/roki-daemon/src/workspace/mod.rs::WorkspaceManager`:

- Drops `workspace_root` field; gains `wt: Arc<dyn WtTool>` and `ghq: Arc<dyn GhqTool>`.
- `ensure(repo, issue)` flow:
  1. `ghq.ensure_cloned(repo.ghq_identifier)` → repo_path
  2. `wt.switch_create(repo_path, issue.as_str())` → worktree_path
  3. Return `Workspace { path: worktree_path, repo, issue }`
  4. The orchestrator passes `worktree_path` as the engine's CWD (existing `WorkerContext.workspace_dir` field — name unchanged).
- `remove(repo, issue)` flow: derive worktree_path the same way, then `wt.remove(worktree_path)`.
- `list_existing()` flow: `ghq` doesn't enumerate worktrees, so we list via `git worktree list --porcelain` per configured repo. (This is for restart recovery, task 5.2 — task 6.1 just keeps the trait signature stable; the impl body for `list_existing` may TODO until 5.2.)
- The path-safety invariant changes: instead of "must be a descendant of `workspace_root`", it becomes "must be the path `wt` returns from `switch_create` for the given repo + issue, with no extra components". The collision check (two distinct issue ids sanitizing to the same worktree path) still applies.

## Failure modes

- `wt` not on `$PATH` → hard refusal at startup with: `"wt (worktrunk) not found on PATH. Install via <pointer to monorail's expected source>."`
- `ghq` not on `$PATH` → same shape: `"ghq not found on PATH. Install via 'go install github.com/x-motemen/ghq@latest' or 'brew install ghq'."`
- `ghq.ensure_cloned` returns error (network failure, repo not found) → marks repo unhealthy (existing health-check seam from 1.5), refuses to schedule work for that repo, continues with other repos.
- `wt switch --create` fails because the branch already exists at a different worktree → log the error, escalate to operator (existing escalation event mechanism); the orchestrator marks that `(repo, issue)` failed and moves on.

## Test impact

- New unit tests for `WtTool` and `GhqTool` traits with `RealWt` / `RealGhq` failure-path coverage. Mocks via the trait for orchestrator-level tests.
- `crates/roki-daemon/tests/orchestrator_workspace.rs` — already uses `WorkspaceManager`; rewritten to inject mock `WtTool` + mock `GhqTool` via the trait so no real `wt` / `ghq` CLI is needed in tests.
- `crates/roki-daemon/tests/e2e_happy_path.rs`, `e2e_failure_retry.rs`, `e2e_bootstrap.rs` — these spin up a real `WorkspaceManager`. Decision: do we mock or use real CLIs?
  - **Recommendation**: mock both via the trait in unit + integration tests. Real-CLI exercise only happens through the bootstrap smoke test, which should also mock unless we're willing to require `wt` and `ghq` for `cargo test` to pass. Given roki has been treating the agent's claude binary as a real-CLI dependency in tests (via `fake_claude` example binary), the same pattern works here: a `fake_wt` / `fake_ghq` example binary OR pure trait mocks. **Pure trait mocks** are simpler and faster.

## Touch list

- `crates/roki-daemon/src/config/repos.rs` — `RepoConfig::repo: String` (ghq identifier); remove `path`. Add validation that the identifier is a non-empty `<token>/<token>` or `<host>/<token>/<token>` shape.
- `crates/roki-daemon/src/config/mod.rs` — remove `workspace_root` field + validation + env var; remove `Config.workspace_root`.
- `crates/roki-daemon/src/tools/mod.rs` — re-export new `wt` and `ghq` modules.
- `crates/roki-daemon/src/tools/wt.rs` — NEW (port from monorail).
- `crates/roki-daemon/src/tools/ghq.rs` — NEW (port from monorail).
- `crates/roki-daemon/src/workspace/mod.rs` + `workspace/layout.rs` — rewrite. Drop `workspace_root` field; add `wt` + `ghq` deps. New `ensure` / `remove` bodies. `Workspace` trait signature unchanged.
- `crates/roki-daemon/src/runtime.rs` — bootstrap constructs `RealWt` + `RealGhq` and threads them into `WorkspaceManager`. Remove `workspace_root` from config-load path. Add startup-time PATH check for `wt` and `ghq` with clear refusal messages.
- `SPEC.md` §2.2 (config shape — drop `workspace_root`, swap `path` → `repo`), §6 (rewrite the path-layout section to describe the worktree model, sibling path, sanitization rules, lifecycle invariants).
- `.kiro/specs/roki-mvp/design.md` — update the WorkspaceManager component prose + lifecycle diagram references.
- All existing tests touching `WorkspaceManager` — refactor to inject mock `WtTool` + mock `GhqTool` (cleanest is one shared `tests/common/mod.rs` helper module).

## What does NOT change

- The `(repo, issue)` state-machine key is still `RepoId` + `IssueId`. `RepoId` continues to be the operator-chosen `RepoConfig.id` (stable across config edits), NOT the ghq identifier (which is a discovery hint). This preserves correlation-id stability in logs and the snapshot API.
- The `WorkerContext.workspace_dir` field stays — its content is now a worktree path instead of a sandbox dir, but the engine adapter doesn't care.
- Webhook routes still mount at `/linear/webhook/<repo-id>` (the stable id, not the ghq identifier).
- Pre-cleanup hooks still run before `wt remove` is called.
- `TerminalFailure` retention: the daemon already skips cleanup; it now also skips `wt remove`, so the branch is preserved by virtue of `wt remove` not running.

## Open question for you (last call)

The recommendation table is locked from your answers. The only thing I want to flag is the `workspace_root` removal — that's a breaking config change. Existing `roki.toml` files referencing `workspace_root` will fail to load with a clear error. Acceptable? (Reasonable since 5.1 just shipped and nothing in production uses it yet.)

If yes, I'll open task 6.1, dispatch the implementer, and review same as 5.1.
