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
- **Key Findings (post-FR-18+19 amendment)**:
  - Setup-judge subprocess shape is removed. Pre-admission classification splits into (a) a mechanical 4-condition Rust filter (`assignee` + `Linear state` + `roki:ready` + optional `roki:impl` selecting `mode`) at zero LLM cost, and (b) the orchestrator's own first-turn deliberation (target-spec resolution in SPEC_DRIVEN; nominating the `classify` phase in NEEDS_CLASSIFY).
  - Single-worker per-ticket shape is replaced by a long-lived per-ticket **orchestrator session** (`claude --input-format stream-json --output-format stream-json`) plus a series of short-lived bounded **phase subprocesses** the orchestrator nominates via `action=run_phase`. The orchestrator absorbs target-spec resolution, classify-driven path branching, phase planning, structural artifact validation (`review.md` after `finalize_review`; SPEC_DRIVEN target spec docs on first turn), daemon-only failure surfacing (via `daemon_directive` events), and Linear writes via the operator's installed Linear MCP.
  - Daemon-side mechanical artifact-validation gates (the prior `roki-spec-gate` / `roki-review-gate` vetoable hooks on `Judging → Active` and `Active → Inactive`) are removed alongside the `Judging` state itself. Structural artifact validation is owned by the orchestrator inside its own phase-planning loop using `Read` + `Bash` (read-only sandbox).
  - State set collapses from the prior 8-state machine (`Discovered`/`Queued`/`Judging`/`Active`/`AwaitingReview`/`Backoff`/`TerminalSuccess`/`TerminalFailure`/`Cleaning`/`Skipped`) to **5 states** (`Pending`/`Active`/`Backoff`/`Inactive`/`Cleaning`) with a 12-value `Inactive.reason` discriminator including three orchestrator-dead reasons (`orchestrator_crash` / `orchestrator_unparseable` / `orchestrator_budget_exhausted`).
  - The agent tooling boundary remains explicit and unchanged: the daemon registers / proxies / wraps NO agent-side tool. Orchestrator tool surface = Linear MCP write + `Read` + `Bash` (read-only filesystem sandbox). Phase subprocesses inherit the operator's local Claude Code installation as-is, narrowed only by per-process `allowed_tools`. The `classify` phase is additionally pinned to `Read` + `Glob` + `Grep`.
  - `WORKFLOW.md` declares **four required named template blocks**: `prompt_template_orchestrator` (orchestrator system prompt with `mode` flag substitution), `prompt_template_implement_direct`, `prompt_template_validate_direct`, and `prompt_template_open_pr`. The prior `prompt_template_setup` and `prompt_template_worker` blocks are removed. Per-phase override surface adds optional `prompt_template_<phase>` blocks alongside `extension.phase.<name>.command` (mutually exclusive per phase).
  - Single repo per ticket (multi-repo tickets rejected by the orchestrator with `outcome=needs_split`). Daemon-driven worktree materialization is idempotent on every non-classify phase nomination (`ensure(issue, repo_id)`: `ghq` + `wt switch-create` on the first call, `wt list` verify on subsequent calls); the `classify` phase MUST NOT receive a worktree (it runs against ticket context alone and the orchestrator's `Bash` (read-only sandbox) is sufficient for SPEC_DRIVEN target-spec validation against the project-level spec dir without a worktree).
  - Daemon-only failure surfacing splits into two paths per [fr:14-operator-notifications](../../../docs/fr/14-operator-notifications.md): when the orchestrator is alive, daemon-detected failures (phase stall, retry exhaustion, fs poison, recovery orphan) flow as `daemon_directive` events to the orchestrator's stdin and the orchestrator writes Linear via Linear MCP; when the orchestrator is dead (the three orchestrator-dead reasons), the daemon does NOT fall back to a Linear write — surface is structured log + TUI escalation queue only.

- **Key Findings (prior, retained for context)**:
  - Assignee filtering remains an admission concern in daemon configuration and tracker admission (unchanged).
  - Cleanup discovery uses the operator-configured `[[repos]]` allowlist plus live `wt list` filtered by branch == issue id (unchanged primitive; symmetric with restart recovery).

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

## Design Decisions (FR-18+19 amendment)

### Decision: Replace Setup Judge with Orchestrator Session + Mechanical Pre-Admission

- **Context**: The Phase-18 setup-judge made every admitted ticket pay one extra `claude` invocation just to classify into `act { repos[] }` / `noop`. With Linear's `roki:ready` / `roki:impl` label conventions in place, the same classification splits into a mechanical 4-condition Rust filter (`assignee` + `Linear state` + `roki:ready` + optional `roki:impl`) plus the orchestrator's own first-turn deliberation. The setup-judge invocation became redundant.
- **Alternatives Considered**:
  1. Keep the setup-judge for content-based admission (e.g., body-keyword matching). — Rejected: every content rule we considered is better expressed as a Linear label or as a phase-internal check.
  2. Push admission entirely into the orchestrator's first turn. — Rejected: wastes a thinking turn on a mechanical decision the daemon can do for free; doubles the LLM cost of the silent-skip path which is the bulk of webhook traffic.
  3. **Selected**: 4-condition mechanical pre-admission in Rust, then a long-lived orchestrator session that absorbs target-spec resolution (SPEC_DRIVEN) or classify-driven path branching (NEEDS_CLASSIFY) on its own first turn.
- **Rationale**: Mechanical filter is zero-LLM-cost on every webhook; orchestrator's first turn is paid only on admitted tickets and uses thinking effort it would have spent anyway on phase planning.
- **Trade-offs**: Adds the `mode` flag (`SPEC_DRIVEN` | `NEEDS_CLASSIFY`) as a second admission output, immutable for the orchestrator-session lifetime.

### Decision: Long-Lived Orchestrator Session + Short-Lived Phase Subprocesses

- **Context**: The single-worker-per-ticket shape gave the daemon no structured handle for per-phase budgets, no clean way to cap thinking effort, and forced two extra one-shot subprocesses (the setup-judge and the linear-updater) to handle pre-phase admission and daemon-only failure surfacing — each with their own `prompt_template_*` block, lifecycle, and config namespace.
- **Selected Approach**: One long-lived `claude --input-format stream-json --output-format stream-json` process per admitted ticket (the **orchestrator session**) absorbs the prior setup-judge, linear-updater, and daemon-side artifact-validation roles. It speaks a strict JSON action enum (`run_phase` / `linear_update_done` / `stop`) one turn at a time. For each `action=run_phase` the daemon spawns a short-lived bounded **phase subprocess** with its own `--max-turns`. Phase catalog: 7 phases × 2 modes with mode-aware defaults and per-phase override surface. Bounded thinking budget on the orchestrator: `extension.orchestrator.{model, effort, max_phases, allowed_tools}` — `max_phases=15` default replaces per-process `--max-turns`.
- **Rationale**: One LLM "thinking" component per ticket. Per-phase budgets are explicit. Removes 3 prior subprocess shapes (`setup_judge`, `linear_updater`, single-worker) and 2 prior config namespaces (`[judge]`, `extension.linear_updater.*`).
- **Trade-offs**: Daemon now parses strict JSON action objects (last-JSON-object-per-turn extraction with one reprompt-on-drift); orchestrator-stall detection is needed in addition to phase-stall detection.

### Decision: Orchestrator-Owned Artifact Validation Replaces Daemon-Side Gates

- **Context**: The prior architecture had daemon-side mechanical gates on `Judging → Active` (kiro-spec) and `Active → Inactive` (kiro-review) that did regex / schema / reachability checks on `requirements.md` and `review.md`. Substantive judgment was already done by LLM inside phase subprocesses; the daemon-side gates duplicated that work in a thinner shape and added their own config namespaces, retry counters, and `Inactive.reason` discriminator values.
- **Selected Approach**: Remove both gates and the `roki-spec-gate` / `roki-review-gate` specs. The orchestrator's tool surface (Linear MCP write + `Read` + `Bash` inside a read-only filesystem sandbox) is sufficient for the structural checks, and it can decide retry-with-context vs `action=stop` directly. SPEC_DRIVEN target spec docs are validated once on the orchestrator's first turn (no retry budget — operator is the only fix). `review.md` is validated after each `finalize_review` clean exit, with retry-with-context re-nomination of `implement` on failure (orchestrator-internal budget bounded by `max_phases`).
- **Rationale**: One actor owns the artifact validation decision. Removes a config namespace and an `Inactive.reason` set per gate.
- **Trade-offs**: The orchestrator must handle `phase_complete(finalize_review)` events and read `review.md` itself, costing a small amount of `effort` per cycle.

### Decision: Three Orchestrator-Dead Reasons + TUI-Only Surface

- **Context**: When the orchestrator is alive, it writes Linear via the operator's installed Linear MCP. When it is dead (process crash, schema drift, `max_phases` exhaustion, stall), the daemon needs a fallback. The prior architecture had a one-shot `linear_updater` subprocess for this; that subprocess is removed.
- **Selected Approach**: The daemon does NOT fall back to a Linear write of its own. Instead it routes the issue to one of three `Inactive.reason` values (`orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted`), logs the failure structurally, populates an in-memory escalation queue, and surfaces the issue exclusively via the `roki-observability` TUI escalation queue. Operators notice via the TUI in those three cases.
- **Rationale**: Adding a daemon-side Linear-write path back in for the orchestrator-dead cases would re-introduce the credential surface and second config namespace that the orchestrator-session redesign was meant to eliminate. The TUI-only path scales to one operator (the documented MVP target) and the failure cases are operator-actionable (the orchestrator-dead reasons all preserve the worktree + session tempdir for human inspection).
- **Trade-offs**: Operators without `roki-observability` running notice these failures via structured log only.

### Decision: Four Required `WORKFLOW.md` Template Blocks

- **Context**: The prior two-block schema (`prompt_template_setup` + `prompt_template_worker`) maps to a removed subprocess shape. The new shape needs one orchestrator prompt plus one prompt for each daemon-internal phase.
- **Selected Approach**: Four required blocks: `prompt_template_orchestrator` (system prompt for the orchestrator session, with `mode` flag substituted in), `prompt_template_implement_direct`, `prompt_template_validate_direct`, `prompt_template_open_pr`. Optional `prompt_template_<phase>` blocks support the per-phase override surface alongside `extension.phase.<name>.command` (mutually exclusive per phase).
- **Rationale**: One template block per "owns the prompt" boundary. The `mode` flag substitution lets a single orchestrator template adapt to both SPEC_DRIVEN and NEEDS_CLASSIFY without operators maintaining two templates.
- **Follow-up**: Bundled `WORKFLOW.example.md` must demonstrate all four blocks and document the `mode` substitution semantics.

## References

- `docs/fr/19-orchestrator-session.md` — orchestrator session lifecycle, response schema, event catalog, tool surface, configuration, artifact validation, failure modes (canonical).
- `docs/fr/18-worker-skill-workflow.md` — phase catalog (7 phases × 2 modes), per-phase exit envelope, override surface, skill set (kiro + roki).
- `docs/fr/04-state-machine-and-recovery.md` — pre-admission-judge, 5-state machine, 12-value `Inactive.reason` set, mode flag.
- `docs/fr/14-operator-notifications.md` — daemon-only failure surfacing split (orchestrator-alive Linear MCP path vs orchestrator-dead TUI escalation queue).
- `docs/fr/11-agent-tool-boundary.md` — orchestrator and `classify` phase tool-surface constraints.
- `docs/fr/12-extension-surface.md` — `extension.orchestrator.*` / `extension.phase.<name>.*` / `extension.server.*` reserved namespaces; `additional_context` channel.
- `.kiro/specs/roki-mvp/requirements.md` — FR-aligned requirements (post-FR-18+19).
- `.kiro/specs/roki-mvp/design-agent-driven-repo-selection.md` — historical sidecar; superseded for `roki_open_worktree` / `WorktreeRegistry` decisions but still authoritative for assignee admission, single-tracker, single-WORKFLOW.md, per-issue keying.
- `crates/roki-daemon/src/tracker/model.rs` — normalized issue model carrying assignee + label data.
