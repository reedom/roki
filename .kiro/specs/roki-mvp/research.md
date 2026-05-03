---
refs:
  id: research:roki-mvp
  kind: research
  title: "Research & Design Decisions"
  spec: roki-mvp
---

# Research & Design Decisions

## Summary
- **Feature**: `roki-mvp`
- **Discovery Scope**: Extension / light discovery
- **Key Findings (post-Phase-18 amendment)**:
  - Assignee filtering remains an admission concern in daemon configuration and tracker admission (unchanged).
  - The agent-driven `roki_open_worktree` repo-selection model is replaced by a daemon-driven setup-judge: a one-shot `claude` invocation classifies admitted issues into `act` (with allowlisted repos) or `noop`, and the daemon itself materializes the corresponding worktrees via `wt`+`ghq`. This restores symmetry with cleanup (which the daemon already owned) and removes the need for the daemon to register an agent-side tool.
  - The agent tooling boundary becomes explicit: the daemon registers, proxies, and wraps NO agent-side tool. The worker subprocess inherits the operator's local Claude Code installation as-is — Bash plus the operator's installed MCP servers (notably their Linear MCP). The previously-bundled `linear_graphql` proxy and `roki_open_worktree` tool are dropped.
  - Cleanup discovery uses the operator-configured `[[repos]]` allowlist plus live `wt list` filtered by branch == issue id. This primitive is symmetric with restart recovery and tolerates worktrees the agent might have created via Bash with the same convention; the per-worker `WorktreeRegistry` is no longer needed.
  - `WORKFLOW.md` grows two named template blocks: `prompt_template_setup` (consumed by the judge, sees only the issue) and `prompt_template_worker` (consumed by the worker, also sees the validated worktree paths).
  - The orchestrator state machine adds `Judging` (judge in flight) and `Skipped` (terminal end reachable only from `Judging` on `noop`).

## Research Log

### Assignee Admission Location
- **Context**: The requirements changed from broad active-issue admission to "handle only tickets assigned to me".
- **Sources Consulted**: `.kiro/specs/roki-mvp/requirements.md`, `.kiro/specs/roki-mvp/design.md`, `.kiro/specs/roki-mvp/design-agent-driven-repo-selection.md`, current `crates/roki-daemon/src/config`, `tracker`, and `orchestrator` modules.
- **Findings**:
  - `WORKFLOW.md` is loaded after daemon configuration and feeds agent prompt/policy for already-admitted workers.
  - Worker launch side effects begin before the agent can apply any prompt-level policy.
  - The tracker currently normalizes issues before orchestrator admission, making it the correct boundary for assignment checks.
- **Implications**: `[linear].assignee` belongs in daemon config. `AssigneeAdmission` must resolve `me` to the Linear token owner at startup and filter webhook/poll/recovery observations before worker admission.

### Existing Integration Surface
- **Context**: The current design is post-agent-driven repo selection and has a single workspace-level Linear tracker.
- **Sources Consulted**: `design-agent-driven-repo-selection.md`, `crates/roki-daemon/src/tracker/model.rs`, `crates/roki-daemon/src/tracker/linear.rs`, `crates/roki-daemon/src/tracker/webhook.rs`, `crates/roki-daemon/src/orchestrator/recovery.rs`.
- **Findings**:
  - `NormalizedIssue` currently lacks assignee data and must grow an optional assignee id.
  - The polling query currently fetches active issues visible to the token; it should include the resolved assignee id in the Linear issue filter where possible.
  - Webhook delivery must still accept signed Issue payloads, normalize them, and filter locally because webhook sender-side filtering is not under daemon control.
  - Recovery already reconciles filesystem-discovered issues against Linear, so applying the same assignee matcher there keeps restart behavior consistent.
- **Implications**: The design adds one cohesive admission component rather than moving ownership into the orchestrator or the agent.

## Architecture Pattern Evaluation

| Option | Description | Strengths | Risks / Limitations | Notes |
|--------|-------------|-----------|---------------------|-------|
| Daemon config admission | `[linear].assignee` resolved at startup; tracker admission filters before worker launch | Prevents unwanted sessions and subprocesses; deterministic; testable without agent behavior | Requires Linear lookup at startup for `me` | Selected |
| `WORKFLOW.md` prompt policy | Let the agent read a ticket and exit early when not assigned | No config schema change | Still launches worker, creates session, spends agent time, and may touch tools before deciding | Rejected |
| Generic admission rules | Add label/team/project/state filters now | More flexible | Expands scope beyond the stated "assigned to me" requirement | Deferred |

## Design Decisions

### Decision: Store Assignee Filter in Daemon Config
- **Context**: Roki must only handle tickets assigned to the operator.
- **Alternatives Considered**:
  1. `roki.toml` / daemon config — enforce before worker creation.
  2. `WORKFLOW.md` — instruct the agent to self-filter after launch.
- **Selected Approach**: Add required `[linear].assignee` in daemon config. The value `me` resolves to the Linear token owner; explicit selectors must resolve to exactly one user.
- **Rationale**: Assignment determines whether a worker should exist at all. That is daemon admission, not agent policy.
- **Trade-offs**: Startup now depends on resolving the configured Linear user. This is acceptable because unresolved ownership would make the daemon unsafe to run.
- **Follow-up**: Implementation must test `me`, explicit user selector, missing/empty value, mismatch, reassignment away, and recovery filtering.

### Decision: Add a Narrow AssigneeAdmission Component
- **Context**: Assignment filtering touches polling, webhooks, and restart recovery.
- **Alternatives Considered**:
  1. Inline filtering separately in `linear.rs`, `webhook.rs`, and `recovery.rs`.
  2. A shared admission component consumed by all three paths.
- **Selected Approach**: Use a shared `AssigneeAdmission` service that owns resolution and matching.
- **Rationale**: One component prevents divergent behavior between hot path, cold path, and recovery.
- **Trade-offs**: Adds one small module, but avoids spreading business rules across adapters.
- **Follow-up**: Keep the component scoped to assignee filtering only; label/team/project filters remain out of boundary.

### Decision: Daemon-driven Setup Judge (Phase 18)
- **Context**: The pre-Phase-18 design exposed a daemon-registered `roki_open_worktree` agent tool plus a `linear_graphql` proxy. Trial running RDM-7 surfaced two structural problems: (a) the worker subprocess saw daemon-advertised tools that were not actually wired through any MCP transport, so the agent's first attempts to call them failed silently; (b) the daemon held an agent-facing Linear write path that the operator's existing Linear MCP could already provide, creating a credential surface and a transport problem the operator's local Claude Code installation already solves.
- **Alternatives Considered**:
  1. Wire the existing tools through a real MCP transport. — Rejected: large engineering surface, duplicates capabilities the operator's Linear MCP already provides, keeps the daemon holding agent-facing credentials.
  2. Drop the daemon-side agent tools and let the agent create worktrees and call Linear via Bash + the operator's installed MCP. — Acceptable for cleanup symmetry but loses the deterministic repo-selection signal.
  3. Drop the daemon-side agent tools AND introduce a pre-flight setup judge that decides repo selection up-front, then have the daemon materialize worktrees itself. — Selected.
- **Selected Approach**: A short-lived one-shot `claude` invocation rendered against `prompt_template_setup` returns structured findings (`{ action: "act"|"noop", repos?: [string] }`). The daemon validates findings against the configured `[[repos]]` allowlist, materializes worktrees via `ghq`+`wt`, then launches the main worker. The daemon registers, proxies, and wraps no agent-side tool.
- **Rationale**: Restores setup/cleanup symmetry (both daemon-owned). Removes the daemon's need to hold agent-facing Linear or git write credentials. Cross-repo tickets fall out for free as multiple validated identifiers. The setup judge is a small, easily-replaced component (operator-configurable model identifier) and its findings schema is published in `SPEC.md` so future implementations can author the same prompt and produce conformant findings.
- **Trade-offs**: One extra `claude` invocation per admitted issue. Mitigated by capping the judge to `--max-turns 1` and by selecting a small fast Claude model (default: a small fast model from the same family as the worker; operator-configurable via `[judge].model`). Judge duration is logged on completion so operators can observe and tune.
- **Follow-up**: Implementation must enforce judge-always-read-only at the type level (`JudgeContext.sandbox` is a unit-variant enum) so future code changes cannot accidentally widen the judge's permissions.

### Decision: Allowlist-iteration Cleanup Discovery
- **Context**: With agent-driven `roki_open_worktree` removed, the per-worker `WorktreeRegistry` lost its source of truth. The cleanup pass needs to find all worktrees the daemon materialized for an issue across an arbitrary subset of `[[repos]]`.
- **Alternatives Considered**:
  1. Maintain an in-memory registry per worker, populated by `WorktreeManager::setup`. — Functional but creates a state-of-truth that diverges from the filesystem under crash conditions and cannot recover agent-created worktrees with the same branch convention.
  2. Persist the registry to disk. — Violates the no-database principle.
  3. Iterate the operator-configured `[[repos]]` allowlist + live `wt list` filtered by branch == issue id at cleanup time. — Selected.
- **Selected Approach**: Discovery iterates the allowlist, runs `wt list` against each repo's local checkout, filters to branches whose name equals the Linear issue identifier verbatim, and removes each match via `wt remove`. The same primitive drives `RecoveryReconciler` at startup.
- **Rationale**: Symmetric with restart recovery. No daemon-side registry to corrupt across crashes. Tolerates worktrees the agent may have created via Bash with the same branch convention. The branch-equals-issue-id invariant means there are no false positives.
- **Trade-offs**: Cleanup cost scales with `len([[repos]])` rather than with the number of materialized worktrees. Acceptable at the documented target (tens of repos).
- **Follow-up**: The `wt list` shellout must tolerate repos that are not on disk yet (skip with structured log) and repos that have no matching branch (no-op).

### Decision: Two Named Template Blocks in WORKFLOW.md
- **Context**: The judge and worker need different prompt content. The judge sees only the issue and must produce structured findings; the worker also needs the validated worktree paths.
- **Selected Approach**: `WORKFLOW.md` exposes two named template blocks: `prompt_template_setup` (consumed by the judge, named variables: `{ issue }`) and `prompt_template_worker` (consumed by the worker, named variables: `{ issue, worktree_paths }`). Both are required at startup.
- **Rationale**: Single file keeps operator config small. Named blocks make the contract explicit. Schema validation rejects either-missing at startup (hard refusal). On render failure, the deterministic fallback prompt always includes the issue id, title, and description so the subprocess receives non-empty issue context.
- **Follow-up**: Bundled `WORKFLOW.example.md` must demonstrate both blocks plus the operator-prerequisite documentation (Linear MCP, `wt`/`ghq` on `$PATH`).

## Risks & Mitigations
- Linear user resolution may fail at startup — fail closed with a structured configuration error naming `[linear].assignee`.
- Webhook payloads may omit assignee information — treat missing assignee as unassigned, ignore for worker admission, and log the mismatch.
- Reassignment away during a run could otherwise look like a failure — route to `Cleaning` without consuming retry budget and log assignment loss separately.
- Setup judge produces unparseable output — retry exactly once with the same input; persistent unparseability routes to `TerminalFailure` with raw stdout captured.
- Setup judge returns a repo not in the allowlist — route directly to `TerminalFailure` (no retry) with both the offending identifier and the configured allowlist contents in the log; never fall through to `Act`.
- Operator omits Linear MCP from their Claude Code installation — workers will be unable to move Linear state. Mitigated by SPEC.md / README / `WORKFLOW.example.md` documentation; the daemon does not detect this at startup because doing so would require introspecting the agent's tool surface, which is precisely the boundary Req 7 establishes.

## References
- `.kiro/specs/roki-mvp/requirements.md` — Phase 18 amendment (setup judge, agent tooling boundary, Judging/Skipped, two named template blocks, allowlist-iteration cleanup).
- `.kiro/specs/roki-mvp/design-agent-driven-repo-selection.md` — historical sidecar; superseded for `roki_open_worktree` / `WorktreeRegistry` decisions but still authoritative for assignee admission, single-tracker, single-WORKFLOW.md, per-issue keying.
- `crates/roki-daemon/src/tracker/model.rs` — normalized issue model carrying assignee data.
