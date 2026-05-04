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
- **Architecture (post-FR-18+19)**:
  - Pre-admission classification splits into (a) a mechanical 4-condition Rust filter (`assignee` + Linear state in `admit_states` + `roki:ready` + optional `roki:impl` selecting `mode`) at zero LLM cost, and (b) the orchestrator's own first-turn deliberation (target-spec resolution in SPEC_DRIVEN; nominating the `classify` phase in NEEDS_CLASSIFY).
  - Single-worker per-ticket shape is replaced by a long-lived per-ticket **orchestrator session** (`claude --input-format stream-json --output-format stream-json`) plus a series of short-lived bounded **phase subprocesses** the orchestrator nominates via `action=run_phase`. The orchestrator absorbs target-spec resolution, classify-driven path branching, phase planning, structural artifact validation (`review.md` after `finalize_review`; SPEC_DRIVEN target spec docs on first turn), daemon-only failure surfacing (via `daemon_directive` events), and Linear writes via the operator's installed Linear MCP.
  - Daemon-side mechanical artifact-validation gates are removed alongside the `Judging` state itself. Structural artifact validation is owned by the orchestrator inside its own phase-planning loop using `Read` + `Bash` (read-only sandbox).
  - **5 states** (`Pending`/`Active`/`Backoff`/`Inactive`/`Cleaning`) with a 12-value `Inactive.reason` discriminator including three orchestrator-dead reasons (`orchestrator_crash` / `orchestrator_unparseable` / `orchestrator_budget_exhausted`).
  - The agent tooling boundary remains explicit and unchanged: the daemon registers / proxies / wraps NO agent-side tool. Orchestrator tool surface = Linear MCP write + `Read` + `Bash` (read-only filesystem sandbox). Phase subprocesses inherit the operator's local Claude Code installation as-is, narrowed only by per-process `allowed_tools`. The `classify` phase is additionally pinned to `Read` + `Glob` + `Grep`.
  - `WORKFLOW.md` declares **four required named template blocks**: `prompt_template_orchestrator` (orchestrator system prompt with `mode` flag substitution), `prompt_template_implement_direct`, `prompt_template_validate_direct`, and `prompt_template_open_pr`. Per-phase override surface adds optional `prompt_template_<phase>` blocks alongside `extension.phase.<name>.command` (mutually exclusive per phase).
  - Single repo per ticket (multi-repo tickets rejected by the orchestrator with `outcome=needs_split`). Daemon-driven worktree materialization is idempotent on every non-classify phase nomination (`ensure(issue, repo_id)`: `ghq` + `wt switch-create` on the first call, `wt list` verify on subsequent calls); the `classify` phase MUST NOT receive a worktree.
  - Daemon-only failure surfacing splits into two paths per [fr:14-operator-notifications](../../../docs/fr/14-operator-notifications.md): when the orchestrator is alive, daemon-detected failures (phase stall, retry exhaustion, fs poison, recovery orphan) flow as `daemon_directive` events to the orchestrator's stdin and the orchestrator writes Linear via Linear MCP; when the orchestrator is dead (the three orchestrator-dead reasons), the daemon does NOT fall back to a Linear write — surface is structured log + TUI escalation queue only.
  - Cleanup discovery uses the operator-configured `[[repos]]` allowlist plus live `wt list` filtered by branch == issue id (symmetric with restart recovery).

## Architecture Pattern Evaluation

| Option | Description | Strengths | Risks / Limitations | Notes |
|--------|-------------|-----------|---------------------|-------|
| Daemon-side mechanical pre-admission + orchestrator first-turn deliberation | 4-condition Rust filter (assignee + state + label) before any subprocess; orchestrator deliberates on admitted tickets | Zero LLM cost on silent-skip path; orchestrator's first turn is paid only on admitted tickets; mode flag is a clean discriminator | Adds the `mode` flag as a second admission output (immutable for orchestrator-session lifetime) | Selected |
| Orchestrator-only admission | Push admission entirely into the orchestrator's first turn | No daemon-side filter | Wastes a thinking turn on a mechanical decision; doubles LLM cost on silent-skip traffic (the bulk of webhook volume) | Rejected |
| `WORKFLOW.md` prompt-policy admission | Let the agent read every ticket and exit early when not assigned | No config schema change | Still launches subprocess, creates session, may touch tools before deciding | Rejected |
| Generic admission rules | Add label/team/project/state filters now beyond the documented 4 conditions | More flexible | Expands scope beyond the stated MVP | Deferred |

## Key Design Decisions

### Mechanical 4-condition pre-admission in Rust

- **Context**: Linear's `roki:ready` / `roki:impl` label conventions plus assignee + admit-state filters cover MVP admission deterministically. Content-based admission rules can be expressed as additional Linear labels.
- **Selected**: `assignee` + Linear state ∈ `admit_states` + `roki:ready` present + optional `roki:impl` selecting `SPEC_DRIVEN` vs `NEEDS_CLASSIFY`. Failed ticket silently skipped (log only, no state entry, no Linear write).
- **Trade-offs**: The `mode` flag is rendered into the orchestrator's system prompt at launch and is immutable for the orchestrator-session lifetime. Relabeling mid-flight does not re-route — the next webhook re-runs pre-admission.

### Long-lived orchestrator session + short-lived phase subprocesses

- **Context**: The single-worker shape gave the daemon no structured handle for per-phase budgets and forced auxiliary one-shot subprocesses (setup-judge, linear-updater) for admission and notification.
- **Selected**: One `claude --input-format stream-json --output-format stream-json` per admitted ticket absorbs the prior auxiliary roles. It speaks a strict JSON action enum (`run_phase` / `linear_update_done` / `stop`) one turn at a time. For each `action=run_phase` the daemon spawns a short-lived bounded phase subprocess with its own `--max-turns`. Phase catalog: 7 phases × 2 modes with mode-aware defaults and per-phase override surface. Bounded thinking budget on the orchestrator: `extension.orchestrator.{model, effort, max_phases, allowed_tools, stall_seconds}` — `max_phases=15` default; no per-process `--max-turns` on the orchestrator.
- **Trade-offs**: Daemon parses strict JSON action objects (last-JSON-object-per-turn extraction with one reprompt-on-drift); orchestrator-stall detection needed in addition to phase-stall detection.

### Orchestrator-owned artifact validation (no daemon-side gates)

- **Context**: Prior architecture had daemon-side mechanical gates duplicating regex / schema / reachability checks the orchestrator can do itself.
- **Selected**: Remove both gates and the prior `roki-spec-gate` / `roki-review-gate` specs. Orchestrator's `Read` + `Bash` (read-only sandbox) suffices for structural checks. SPEC_DRIVEN target spec docs validated once on the orchestrator's first turn (no retry budget — operator is the only fix). `review.md` validated after each `finalize_review` clean exit; retry-with-context re-nomination of `implement` on failure (orchestrator-internal budget bounded by `max_phases`).
- **Trade-offs**: Orchestrator must handle `phase_complete(finalize_review)` and read `review.md` itself, costing a small amount of `effort` per cycle.

### Three orchestrator-dead reasons + TUI-only surface

- **Context**: When the orchestrator is alive it writes Linear via the operator's Linear MCP. When dead (process crash, schema drift, `max_phases` exhaustion, stall) the daemon needs a fallback — but a daemon-side Linear-write path would re-introduce the credential surface and config namespace that the orchestrator-session redesign was meant to eliminate.
- **Selected**: Daemon does NOT fall back to a Linear write of its own. Routes the issue to one of three `Inactive.reason` values (`orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted`), populates the in-memory escalation queue, and surfaces via the `roki-observability` TUI. These three reasons preserve the worktree + session tempdir for human inspection.
- **Trade-offs**: Operators without `roki-observability` running notice these failures via structured log only.

### Allowlist-iteration cleanup discovery

- **Context**: With agent-driven `roki_open_worktree` removed, the per-worker registry lost its source of truth.
- **Selected**: Cleanup iterates `[[repos]]` allowlist + `wt list` filtered by branch == issue id verbatim + `wt remove`. Same primitive drives `RecoveryReconciler` at startup.
- **Trade-offs**: Cleanup cost scales with `len([[repos]])` rather than with the number of materialized worktrees. Acceptable at the documented MVP target.

### Four required `WORKFLOW.md` template blocks

- **Selected**: `prompt_template_orchestrator` (system prompt; `mode` flag substituted in), `prompt_template_implement_direct`, `prompt_template_validate_direct`, `prompt_template_open_pr`. Optional `prompt_template_<phase>` blocks for per-phase overrides; `extension.phase.<name>.command` is the mutually-exclusive alternative per phase.
- **Rationale**: One template block per "owns the prompt" boundary. The `mode` flag substitution lets a single orchestrator template adapt to both SPEC_DRIVEN and NEEDS_CLASSIFY without operators maintaining two templates.

## Risks & Mitigations

- Linear user resolution may fail at startup → fail closed with a structured configuration error naming `[linear].assignee`.
- Webhook payloads may omit assignee information → treat missing assignee as unassigned, silent-skip, log the mismatch.
- Reassignment away during a run → route to `Cleaning` without consuming retry budget; log assignment loss separately.
- Operator omits Linear MCP from their Claude Code installation → orchestrator + phase subprocesses cannot write Linear; mitigated by README / `WORKFLOW.example.md` documentation. The daemon does not detect this at startup because doing so would require introspecting the agent's tool surface, which is precisely the boundary Req 7 establishes.

## References

- `docs/fr/19-orchestrator-session.md` — orchestrator session lifecycle, response schema, event catalog, tool surface, configuration, artifact validation, failure modes (canonical).
- `docs/fr/18-worker-skill-workflow.md` — phase catalog (7 phases × 2 modes), per-phase exit envelope, override surface, skill set (kiro + roki).
- `docs/fr/04-state-machine-and-recovery.md` — pre-admission-judge, 5-state machine, 12-value `Inactive.reason` set, mode flag.
- `docs/fr/14-operator-notifications.md` — daemon-only failure surfacing split (orchestrator-alive Linear MCP path vs orchestrator-dead TUI escalation queue).
- `docs/fr/11-agent-tool-boundary.md` — orchestrator and `classify` phase tool-surface constraints.
- `docs/fr/12-extension-surface.md` — `extension.orchestrator.*` / `extension.phase.<name>.*` / `extension.server.*` reserved namespaces; `additional_context` channel.
- `.kiro/specs/roki-mvp/requirements.md` — FR-aligned requirements.
- `.kiro/specs/roki-mvp/design.md` — design canonical for components, contracts, and bootstrap composition order.
