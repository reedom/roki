---
refs:
  id: roadmap
  kind: roadmap
  title: "roki Roadmap"
---

# Roadmap

## Overview
roki is a Rust-based scheduler/runner for Linear-driven coding work. It launches an isolated implementation run per ticket, lets Claude Code (with kiro + superpowers skills) do the actual work, and enforces EARS-shaped acceptance gates between Linear state transitions. Goal: a near-one-way path from Linear ticket to PR, with guardrails preventing drift.

The architecture is symphony-aligned (openai/symphony) where it makes sense: no persistent DB, daemon-as-scheduler, agent-does-everything-else, `WORKFLOW.md` as user-repo policy boundary. The main worker is a single bounded stdio agent invocation per admitted issue (the agent-side kiro skill performs all internal orchestration and runs the ticket end-to-end within that one invocation; the daemon does not relaunch the worker on clean exit). The daemon also dispatches two short-lived bounded auxiliary invocations of the same engine: a setup judge before worker launch, and a linear-updater on daemon-only failures. roki diverges from symphony in four deliberate ways: a daemon-side Linear assignee admission filter that gates worker launch before the judge; a setup judge that constrains every ticket to a single allowlisted repo and rejects multi-repo tickets back to the operator via linear-updater; a daemon-enforced kiro-spec gate before implementation; and a daemon-enforced kiro-review gate before PR completion. Daemon-only failures (stall, retry exhaustion, filesystem poison, multi-repo rejection, etc.) reach the operator through linear-updater (Linear label + comment) plus the optional TUI escalation queue — the daemon itself never writes Linear directly.

## Approach Decision
- **Chosen**: Clean restart with vertical slices, Rust + skill-first, symphony-informed daemon shape.
- **Why**: The previous attempt (monorail) reached a "skill-first, small daemon" pivot but had not finished it. Symphony validates the small-daemon thesis and provides a battle-tested process model (long-lived stdio agent session, in-memory state, `WORKFLOW.md`). Rust carries forward language-level parity with monorail; the actual code is fresh. Vertical slices keep each spec deliverable end-to-end so progress is observable.
- **Rejected alternatives**:
  - Tight monorail port — would carry forward decisions (SQLite persistence, per-task shell-out, single-repo assumptions) that symphony shows are unnecessary.
  - Hybrid port (proven pieces + redesign contested) — same problem at lower volume; vertical slices are simpler.
  - Daemon-first orchestration (Rust drives phases) — conflicts with skill-first pivot and symphony's "agent does everything" principle.

## Scope
- **In**:
  - Linear-ticket-driven implementation runs against an operator-declared allowlist of repos
  - Daemon-side Linear assignee admission filter (config value `me` resolves against the configured Linear API token); only issues in the configured `admit_states` set (default `["Todo"]`) are admitted
  - Pre-flight setup judge: a one-shot `claude` invocation that classifies an admitted issue into `act` (with exactly one allowlisted repo) or `noop`. Tickets that touch more than one repo are rejected back to the operator (`needs-split`) via linear-updater
  - Daemon-driven worktree materialization (single repo per issue) via `wt` + `ghq` based on validated judge findings (per-issue state keyed by Linear issue id alone)
  - Single bounded `claude --print --output-format stream-json` invocation per admitted issue, streaming JSON event handling for structured logging and stall detection (no daemon relaunch on clean exit; review gate Deny may trigger an intentional re-launch with `additional_context`)
  - `WORKFLOW.md` loader (Liquid + Markdown, hot reload) with three named template blocks: `prompt_template_setup`, `prompt_template_worker`, and `prompt_template_linear_updater`
  - Per-issue session tempdir lifecycle, daemon-driven worktree cleanup on terminal Linear state or assignment loss
  - Configurable permission strategy (`--settings` allowlist with `--dangerously-skip-permissions` fallback); default agent sandbox `workspace-write` with elicitations rejected
  - linear-updater subagent: a setup-judge-shaped one-shot `claude` invocation that translates daemon-only failure events (stall, retry exhaustion, multi-repo rejection, judge unparseable, fs poison, orphan recovery) into Linear label additions and comments via the operator's installed Linear MCP. The daemon never issues a Linear write itself
  - Pre-implementation kiro-spec gate (daemon-enforced)
  - Pre-PR kiro-review gate (daemon-enforced); review.md is produced by the worker's `kiro-review` skill before clean exit, the gate is structural validation only
  - Optional HTTP API + ratatui TUI for observability (escalation queue is the secondary surface for daemon-only failures alongside linear-updater)

- **Out**:
  - Persistent database (deliberately; restart-recovery via Linear + filesystem)
  - Linear write logic in Rust (the agent does Linear writes via the operator's installed Linear MCP integration; the daemon never registers, proxies, or wraps any Linear write path. The linear-updater subagent is an agent invocation, not a daemon-side write path)
  - Daemon-registered, daemon-proxied, or daemon-wrapped agent-side tools of any kind (worker subprocess inherits the operator's local Claude Code tool surface as-is — Bash plus the operator's installed MCP servers)
  - PR creation / commit / push logic in Rust (agent owns these via `gh` / `git` CLIs reachable through Bash)
  - Container or VM isolation (rely on Claude Code's `workspace-write` sandbox + filesystem path safety)
  - Auto-merge orchestration (deferred; v1 stops at PR open / human review)
  - Multi-host SSH workers (symphony has it; roki defers)
  - Multi-tenant orchestration (one daemon per developer)
  - Per-repo `WORKFLOW.md` overrides (single workspace-level policy artifact only)
  - Multi-repo tickets (one ticket = one repo; multi-repo tickets are rejected back to the operator with a `needs-split` Linear label and an explanatory comment via linear-updater)
  - Generic team / label / project admission filtering beyond the configured assignee constraint
  - Slack and other push-style operator notification channels (covered by linear-updater + TUI escalation queue; can be re-introduced if Linear-routed feedback proves insufficient)
  - Post-merge flow-document distill / archive sweep performed by the daemon (handled in CI; not a roki concern)
  - Windows support

## Constraints
- **Language**: Rust 2024, tokio async runtime.
- **Engine**: Claude Code, headless mode (`claude --print --output-format stream-json`); slash commands not available in `-p`, so roki contracts via natural-language prompts + kiro skills auto-invoking by description.
- **Skills**: kiro skills depended on as personal skills under `~/.claude/skills/kiro-*` (not vendored, not plugin-namespaced; namespacing breaks auto-trigger).
- **Agent tool surface**: the worker subprocess inherits the operator's local Claude Code installation verbatim — its built-in tool set plus the operator's installed MCP servers (including the operator's Linear MCP for Linear reads/writes). The daemon adds nothing to that surface.
- **External CLIs**: operator must install `wt` (worktrunk) and `ghq` and ensure both are on `$PATH`; the daemon shells out to them for worktree materialization and cleanup.
- **Admission**: only issues whose Linear assignee matches the configured assignee filter and whose Linear workflow state is in the configured `admit_states` set (default `["Todo"]`) are admitted. The setup judge runs only after both checks pass.
- **Linear API**: rate limit 5,000 req/hr; Linear discourages polling — webhook receiver is the hot path, polling is the fallback for admitted tickets at <=5min cadence with 429 backoff. Daemon-side Linear access is read-only.
- **Permissions**: `permissions.allow` allowlist is flaky in 2026; daemon supports config-driven fallback to `--dangerously-skip-permissions`. The setup judge and the linear-updater subprocess both run with a read-only filesystem sandbox regardless of operator overrides; their access to write-capable Linear MCP tools is governed by the operator's MCP allowlist.
- **Operator notifications**: linear-updater is the destination for daemon-only failures the agent inside the worker subprocess cannot self-report (stall, max-turns exhaustion, unknown `result.subtype`, retry-budget exhaustion, judge final failure, filesystem poison, orphaned recovery residue, multi-repo rejection). The TUI escalation queue is the secondary surface; both are populated from the same structured event. If the operator's Linear MCP is unavailable, linear-updater logs the failure and the daemon continues without crashing.
- **Platform**: macOS + Linux. Terminal compatibility for TUI: iTerm2 / Ghostty / WezTerm / Alacritty primary; macOS Terminal.app limited (RGB color caveats).

## Boundary Strategy
- **Why this split**: roki-mvp is the symphony-parity vertical slice — without it, nothing else has anywhere to plug in. The two kiro gates (spec, review) are roki's actual differentiators and are independent of each other; they bolt onto the same state-machine seam in roki-mvp. Observability (HTTP + TUI) is one cohesive UX, not two; splitting it would create a synthetic seam.
- **Shared seams to watch**:
  - **State machine extension points**: roki-mvp publishes a read-only `OrchestratorRead` snapshot trait and structured transition events with two declared-vetoable arcs (`Judging → Active` for the spec gate, `Active → Inactive` for the review gate) — spec-gate, review-gate, and observability subscribe without forking the orchestrator.
  - **Tracker nudge**: roki-mvp publishes a `TrackerRefresh` trait that lets external callers request an out-of-cycle Linear poll without bypassing the cadence cap or the 429 backoff state.
  - **Worker prompt extension**: roki-mvp's engine adapter accepts an additive optional `additional_context` field on `WorkerContext` and forwards it verbatim to the agent through a stable, machine-extractable section of the worker prompt, kept distinct from the rendered `prompt_template_worker` body — the review gate uses this to inject fix-finding context for its intentional re-launches, and the spec gate may use it analogously.
  - **Subprocess invocation taxonomy**: the engine adapter supervises three bounded `claude` invocations — the long-lived main worker, the short-lived setup judge, and the short-lived linear-updater — using the same lifecycle, stall detection, and stream-json parsing for each.
  - **Agent-side tool surface (no daemon registration)**: gates and the linear-updater reach Linear / git / gh through the operator's installed MCP servers and Bash, not through daemon-registered tools — the daemon does not expose any agent-facing tool registry.
  - **`WORKFLOW.md` schema**: the loader reserves `extension.gates.spec.*`, `extension.gates.review.*`, `extension.server.*`, and `extension.linear_updater.*` namespaces and round-trips unknown keys without interpretation, so gates and the linear-updater add keys without breaking sibling specs.
  - **Workspace path layout**: `.kiro/specs/<issue>/` lives inside the workspace; the spec gate writes `requirements.md` there and the worker's `kiro-review` skill writes `review.md` there.

## Specs (dependency order)
- [x] roki-mvp -- symphony-parity vertical slice: Rust skeleton + Linear poll + claude session + workspace + run loop. Single repo per ticket; multi-repo tickets are rejected at the setup judge with a `needs-split` Linear label posted by linear-updater. Dependencies: none
- [x] roki-spec-gate -- daemon-enforced pre-implementation kiro-spec gate; pre-impl distill flow merging ticket EARS into project EARS. Dependencies: roki-mvp
- [x] roki-review-gate -- daemon-enforced pre-PR kiro-review gate; structurally validates `review.md` produced by the worker's `kiro-review` skill before clean exit. Dependencies: roki-mvp
- [x] roki-observability -- optional HTTP API (axum) + ratatui TUI client; symphony /api/v1/state schema. Dependencies: roki-mvp
- ~~roki-distill-postmerge~~ -- removed from roki scope; flow-document distill / archive sweep is now handled in CI, not by the daemon. The historical spec under `.kiro/specs/roki-distill-postmerge/` is retained for reference but is not active.
