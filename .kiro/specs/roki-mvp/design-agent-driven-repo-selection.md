---
refs:
  id: design:roki-mvp:agent-driven-repo-selection
  kind: design
  title: "Agent-Driven Repo Selection (Historical)"
  spec: roki-mvp
  depends_on:
    - design:roki-mvp
---

# Agent-driven repo selection — Task 7.1

Status: PROPOSAL — needs user sign-off before task 7.1 is opened.

## What changes

Today (post-6.1):

- Operator pre-declares per-repo Linear scope (`repos.scope`) so the daemon routes each issue to exactly one repo.
- Each repo has its own webhook URL, its own HMAC secret, its own `LinearTracker`, its own `WORKFLOW.md`.
- The state-machine key is `(repo, issue)`. Workspace is a per-issue git worktree of that repo, pre-created on `Queued → Active`.
- `route_issue` is the precedence-rule arbiter (still unwired; the 4.4 follow-up).

After 7.1:

- The daemon serves an **allowlist of repos**. No `repos.scope`. No `route_issue`. No precedence rule.
- ONE workspace-level Linear webhook (single secret), ONE polling tracker.
- The daemon admits only active Linear issues assigned to the configured Linear user. `[linear].assignee = "me"` resolves to the Linear token owner at startup.
- The state-machine key is `(issue,)`.
- On `Queued → Active`, the daemon creates an empty **session tempdir** as the worker's CWD. No git in it.
- The agent reads the ticket on its first turn, decides which configured repo(s) to operate in, and calls a new daemon-exposed agent tool `roki_open_worktree(repo, [branch])`. The daemon resolves via `ghq` + `wt` and returns the worktree path. Cross-repo tickets just call the tool multiple times.
- Single workspace-level `WORKFLOW.md`. Same policy applies regardless of which repo(s) the agent picks.
- On `Cleaning`, the daemon iterates every worktree the agent opened and calls `wt remove`. On `TerminalFailure`, all retained.

## Why

- The agent reads the ticket anyway. Repo classification is a free side effect of the first turn — no extra LLM cost.
- Cross-repo tickets fall out for free (multiple `roki_open_worktree` calls).
- Operator config drops three concepts (`scope`, `id`, per-repo webhook secret) and shrinks to "list of repos I'm willing to serve."
- `route_issue` and its precedence rule disappear (closes 4.4 follow-up by deletion).
- Per-repo `LinearTracker` collapses to one. Per-repo webhook routes collapse to one.
- Single `WORKFLOW.md` is operationally simpler and matches the "one worker per issue" identity.
- Assignee admission belongs in daemon config because it prevents worker launch before the agent sees a ticket; `WORKFLOW.md` remains post-admission agent policy.

## Decision matrix

| # | Decision | Options | Recommendation |
|---|---|---|---|
| 1 | New agent tool name | A. `roki_open_worktree`<br>B. `workspace_open`<br>C. `wt_open` | **A** — daemon-owned semantics, namespaced like `linear_graphql` |
| 2 | Tool input shape | A. `{repo, branch?}` (branch defaults to issue id)<br>B. `{repo}` (branch hard-locked to issue id) | **B** — locks the contract so restart recovery can match worktrees by branch == issue id; if the agent needs a different branch they can `git checkout -b` after opening |
| 3 | Repo allowlist enforcement | A. Strict: tool rejects any `repo` not in `[[repos]]`<br>B. Permissive: any ghq identifier accepted | **A** — config is the trust boundary; permissive would let a hijacked agent clone arbitrary repos onto disk |
| 4 | Idempotency | A. Second call with same `repo` for the same worker returns existing path<br>B. Second call errors | **A** — cheap to make idempotent; lets the agent re-invoke after losing track |
| 5 | Session tempdir location | A. `~/Library/Caches/roki/sessions/<issue>` (XDG)<br>B. `tempfile::TempDir` under system temp<br>C. `~/.local/share/roki/sessions/<issue>` (XDG_DATA) | **A** on macOS / `~/.cache/roki/sessions/<issue>` on Linux. Ephemeral by category but human-discoverable for forensics. |
| 6 | `WORKFLOW.md` location | A. Single workspace-level path in `[workflow]` config block<br>B. Per-repo (status quo)<br>C. Layered (workspace base + per-repo override) | **A** — one worker, one policy. Per-repo backoff/turn-budget is over-optimization. |
| 7 | Linear admission filter | A. Admit every issue → agent decides whether to do work, exits early if not<br>B. Deterministic assignee pre-filter in daemon config (`[linear].assignee = "me"`)<br>C. Generic label/team/project filters | **B** — ownership is daemon admission, not prompt policy. It prevents session creation and worker launch for tickets assigned to someone else while keeping broader filters out of MVP. |
| 8 | What happens when agent never calls `roki_open_worktree` | A. CleanExit advances to `AwaitingReview` regardless<br>B. Daemon enforces "at least one worktree opened" before allowing AwaitingReview | **A** — an assigned issue might legitimately require no repo work or only Linear clarification. Validate via `MONORAIL_RESULT`-style structured exit signal in WORKFLOW.md gate (out of scope for 7.1 itself). |
| 9 | Restart recovery | A. Walk every configured repo's `git worktree list --porcelain`; reconcile branches that look like issue ids against Linear<br>B. Walk session tempdirs; cross-reference with Linear<br>C. Both | **C** — worktrees are the durable artifact; session tempdirs are ephemeral but their existence signals an in-progress run. Recovery walks both, reconciles via Linear. |
| 10 | `Workspace` trait | A. Keep the trait; rename `WorkspaceManager` → `SessionManager` (handles tempdirs only)<br>B. Drop `Workspace` trait entirely; introduce `SessionManager` + `WorktreeRegistry` as separate concrete types | **B** — the old trait was shaped around per-issue dirs; the new model is two distinct concerns (session lifecycle, worktree lifecycle). Bundling them under a renamed trait would be a misleading abstraction. |
| 11 | Cleanup of worktrees on `Cleaning` | A. Daemon walks `WorktreeRegistry` and calls `wt.remove` on each (subject to pre-cleanup hook)<br>B. Agent calls a `roki_close_worktree` tool explicitly | **A** — cleanup-is-daemon's-job is the existing contract; explicit close burdens the agent for no benefit. |

## Schema delta (additive + removals)

```toml
# ~/.config/roki/config.toml  (default; CLI --config overrides; ./roki.toml fallback)

polling_cadence_seconds = 300
max_concurrent_workers = 4
# claude_binary = "/usr/local/bin/claude"   # optional override; default = which("claude")

[server]
bind = "127.0.0.1"
port = 7878

[linear]
token_env = "LINEAR_API_TOKEN"
webhook_secret_env = "ROKI_LINEAR_WEBHOOK_SECRET"
assignee = "me"  # resolves to the Linear user associated with token_env
# endpoint = "https://api.linear.app/graphql"   # test-only override; production omits

[workflow]
path = "/abs/path/to/WORKFLOW.md"   # single workspace-level policy

[permissions]
strategy = "dangerously_skip_permissions"
# settings = "/abs/path/to/.claude/settings.json"   # for allowlist mode

[[repos]]
repo = "yourorg/core"

[[repos]]
repo = "yourorg/infra"
```

What gets removed:

- `Config.workspace_root` (already gone in 6.1).
- `RepoConfig.id` (use `repo` directly).
- `RepoConfig.path` (already renamed in 6.1).
- `RepoConfig.scope` and `LinearScope` enum.
- `RepoConfig.webhook_secret_env` and `webhook_secret`.
- `RepoConfig.workflow_path`.
- The whole `routing.rs` module (`route_issue`, `Specificity`, `UnhealthyReason`, `classify_repo_health`).

What gets added:

- `[linear]` config block with `token_env` (replaces the implicit `LINEAR_API_TOKEN` default), `webhook_secret_env`, and required `assignee`.
- `[workflow]` config block with `path` (single).
- Single agent tool `roki_open_worktree`.
- `SessionManager` and `WorktreeRegistry` modules under `crates/roki-daemon/src/workspace/` (or a new `session/` module — implementer picks).

## State-machine impact

State-machine shape remains anchored on `IssueId`: `Discovered → Queued → Active → AwaitingReview → TerminalSuccess → Cleaning` with `Backoff`/`TerminalFailure` branches. Assignee admission adds one stop trigger: if an admitted issue is reassigned away from the configured user, the daemon terminates any active worker, suppresses further launches, and routes to `Cleaning` without consuming retry budget. Other work done at each transition:

- `Queued → Active`: instead of `WorkspaceManager.ensure(repo, issue) → ghq + wt`, the daemon creates a session tempdir (`~/Library/Caches/roki/sessions/<issue>` on macOS) and that becomes the worker's CWD. No git. No worktree. Just an empty workdir.
- `Active`: agent runs `roki_open_worktree(repo)` whenever it decides to work in a repo. The tool handler does `ghq.ensure_cloned + wt.switch_create` and registers `(worker_id, repo, branch, path)` in `WorktreeRegistry`.
- `Cleaning`: pre-cleanup hooks run as today. Daemon iterates `WorktreeRegistry` for the worker; calls `wt.remove` on each. Removes the session tempdir.
- `TerminalFailure`: nothing removed (matches 6.1 decision #6 — extended from "the one worktree" to "all opened worktrees plus the session tempdir").

`(repo, issue)` → `(issue,)` everywhere. `RepoId` type stays for `WorktreeRegistry` keying but is no longer in the state-machine key. `TransitionEvent` loses its `repo` field (or keeps it as `Option<RepoId>` populated post-tool-call for observability).

## Webhook handler

```
POST /linear/webhook
Headers: Linear-Signature: <hex hmac sha256 of body, keyed by [linear].webhook_secret_env>

1. Extract signature header (401 if absent)
2. HMAC-verify against [linear].webhook_secret_env (401 if mismatch)
3. Deserialize header (400 if malformed)
4. If type != "Issue": 204 (acknowledge but ignore)
5. Deserialize envelope
6. Normalize → NormalizedIssue (no repo association at this point)
7. Apply AssigneeAdmission against the resolved `[linear].assignee`
8. If unassigned or assigned to another user: log mismatch and return 204 without creating a session or worker
9. Forward matching assigned issues to orchestrator's tracker_inbox keyed by IssueId
10. Return 204
```

No per-repo dispatch. The orchestrator spawns a worker only for matching assigned IssueIds it sees that are not already in flight.

## Agent tool: `roki_open_worktree`

Schema (rendered into the agent's tool registry):

```json
{
  "name": "roki_open_worktree",
  "description": "Open a git worktree for the current Linear issue in one of the configured repos. The daemon resolves the repo via ghq, creates a worktree branch named after the issue id via wt, and returns the absolute path. Idempotent — calling twice with the same repo returns the same path. Use this once per repo you intend to modify; cross-repo tickets call this multiple times.",
  "input_schema": {
    "type": "object",
    "properties": {
      "repo": {
        "type": "string",
        "description": "Ghq identifier (owner/name or host/owner/name) of a configured repo. Must be in the daemon's allowlist."
      }
    },
    "required": ["repo"]
  }
}
```

Output (JSON):

```json
{ "path": "/Users/me/ghq/github.com/yourorg/core.ENG-42", "repo": "yourorg/core", "branch": "ENG-42" }
```

Errors (typed, returned as tool errors per the existing tool-error taxonomy):

- `RepoNotInAllowlist { repo, allowed }` — agent specified a repo not in `[[repos]]`.
- `GhqResolutionFailed { repo, reason }` — ghq.ensure_cloned errored.
- `WorktreeCreationFailed { repo, branch, reason }` — wt.switch_create errored (e.g., branch already exists at a conflicting worktree).

## Restart recovery rethink

Today: walk `<workspace_root>/<repo>/<issue>/` (already non-functional after 6.1; `list_existing` stubs to empty per 5.2 follow-up).

After 7.1:
1. List all session tempdirs under `~/Library/Caches/roki/sessions/`. Each names an `IssueId`.
2. For each configured repo, run `git worktree list --porcelain`. Filter to branches whose name matches the Linear issue-id pattern (operator-configurable regex; default `^[A-Z]+-\d+$`).
3. For every distinct issue id discovered (from either source), query Linear for the current state and assignee.
4. Apply AssigneeAdmission before resuming or queueing work.
5. Reconcile per the existing four-cell matrix (`ResumeActive` / `OrphanedSession` / `FreshQueued` / `NoOp`). Note: `OrphanedWorkspace` becomes `OrphanedSession`-or-`OrphanedWorktree` depending on what survived. A discovered issue that is not active and assigned to the configured user is treated as orphaned/no-op according to the disk artifacts present.

This shifts the recovery code from `workspace::list_existing` to a new module that walks worktrees + sessions. Task 5.2's scope (real `RecoveryLinearReader`) folds into 7.1's scope.

## Touch list

Production source:
- `crates/roki-daemon/src/config/mod.rs` — drop `RepoConfig.id` etc.; add `[linear]` (including required `assignee`) and `[workflow]` blocks.
- `crates/roki-daemon/src/config/repos.rs` — shrink `RepoConfig` to `{ repo: String }` only.
- `crates/roki-daemon/src/routing.rs` — DELETE the file.
- `crates/roki-daemon/src/orchestrator/state.rs` — `TransitionEvent.repo` becomes `Option<RepoId>` (or remove entirely; downstream callers update). State-machine key becomes `IssueId` only.
- `crates/roki-daemon/src/orchestrator/core.rs` — `ActorRecord` keyed by `IssueId`; `try_promote_to_active` invokes `SessionManager.create_session(issue)` instead of `WorkspaceManager.ensure(repo, issue)`. Cleanup iterates `WorktreeRegistry` for the worker.
- `crates/roki-daemon/src/orchestrator/tracker_bridge.rs` — dedup keys collapse from `(repo, issue, target_state)` to `(issue, target_state)`.
- `crates/roki-daemon/src/workspace/{mod.rs,layout.rs}` — REWRITE as `session/` and `worktrees/` modules. Drop `Workspace` trait. Add `SessionManager` and `WorktreeRegistry` concrete types.
- `crates/roki-daemon/src/tools/{mod.rs,roki_open_worktree.rs}` — new agent tool implementation alongside existing `linear_graphql`.
- `crates/roki-daemon/src/tracker/{admission.rs,linear.rs,webhook.rs,model.rs}` — single `LinearTracker` (no scope), single webhook route, assignee resolution/filtering, `NormalizedIssue.assignee`.
- `crates/roki-daemon/src/runtime.rs` — bootstrap composition reflects all of the above.
- `SPEC.md` — major rewrite of §2.2 (config), §2.3 (multi-repo: now an allowlist + agent-driven selection), §6 (path layout: session + worktree registry), §7 (tool registry: add `roki_open_worktree`), §10 (recovery).
- `.kiro/specs/roki-mvp/design.md` — fold the agent-driven model into the architecture prose.

Tests:
- Every existing e2e test refactors. The happy-path test now spawns a worker for one issue, the agent (mocked) calls `roki_open_worktree("yourorg/scratch")`, asserts the worktree path is created, and verifies cleanup tears it down.
- Cross-repo test added: agent opens two worktrees in one worker.
- Allowlist-rejection test added: agent calls `roki_open_worktree("evil/repo")` → tool error.
- Assignee-admission test added: active issue assigned to `me` starts a worker; active issue assigned to another user is logged and ignored; reassignment away stops the worker and enters cleanup without retry.
- Restart recovery test rewritten to seed both session tempdirs and pre-existing worktrees.

## Refusal modes

- `[linear].webhook_secret_env` not set → hard refusal at startup.
- `[linear].assignee` missing, empty, or unresolvable to exactly one Linear user → hard refusal at startup.
- `[workflow].path` missing or unreadable → hard refusal.
- No `[[repos]]` entries → WARN log, daemon starts but every `roki_open_worktree` call returns `RepoNotInAllowlist`.
- `wt`/`ghq`/`claude` not on PATH → hard refusal (existing 5.1+6.1 behavior, unchanged).
- Agent calls `roki_open_worktree` with a repo not in the allowlist → tool error to agent (worker continues; agent can recover).
- Two configured `[[repos]]` with the same `repo` value → hard refusal at config load.

## Out of scope for 7.1

- Generic Linear admission filters beyond assignee (e.g., labels, teams, projects, priority, custom state).
- Per-repo WORKFLOW.md override. Single workspace-level policy is enough for MVP; revisit if real-world repos need wildly different turn budgets.
- Cross-repo PR coordination (e.g., "open one PR per repo for a multi-repo ticket"). The agent decides; daemon doesn't orchestrate.

## Open questions for you

The decision matrix locks 11 calls based on what I think is right. Two are worth your explicit confirmation because they change observable behavior:

- **#6 (single WORKFLOW.md)** — kills per-repo policy. If you want per-repo overrides, say so now; otherwise it's gone.
- **#7 (assignee filter)** — daemon admits only issues assigned to `[linear].assignee`; `WORKFLOW.md` does not own primary assignment filtering.

Anything else, flag and I'll edit. Otherwise: confirm and I'll open task 7.1.
