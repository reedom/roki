# Roadmap

## Overview
roki is a Rust-based scheduler/runner for Linear-driven coding work. It launches an isolated implementation run per ticket, lets Claude Code (with kiro + superpowers skills) do the actual work, and enforces EARS-shaped acceptance gates between Linear state transitions. Goal: a near-one-way path from Linear ticket to PR, with guardrails preventing drift.

The architecture is symphony-aligned (openai/symphony) where it makes sense: no persistent DB, daemon-as-scheduler, agent-does-everything-else, `WORKFLOW.md` as user-repo policy boundary, long-lived stdio agent session. roki diverges from symphony in three deliberate ways: multi-repo from day one (workspaces keyed by `(repo, issue)`), kiro-spec gate before implementation, and daemon-enforced kiro-review gate before PR. Distill (post-merge flow-doc sweep) is roki's third pillar.

## Approach Decision
- **Chosen**: Clean restart with vertical slices, Rust + skill-first, symphony-informed daemon shape.
- **Why**: The previous attempt (monorail) reached a "skill-first, small daemon" pivot but had not finished it. Symphony validates the small-daemon thesis and provides a battle-tested process model (long-lived stdio agent session, in-memory state, `WORKFLOW.md`). Rust carries forward language-level parity with monorail; the actual code is fresh. Vertical slices keep each spec deliverable end-to-end so progress is observable.
- **Rejected alternatives**:
  - Tight monorail port — would carry forward decisions (SQLite persistence, per-task shell-out, single-repo assumptions) that symphony shows are unnecessary.
  - Hybrid port (proven pieces + redesign contested) — same problem at lower volume; vertical slices are simpler.
  - Daemon-first orchestration (Rust drives phases) — conflicts with skill-first pivot and symphony's "agent does everything" principle.

## Scope
- **In**:
  - Linear-ticket-driven implementation runs against one or more Git repos
  - Long-lived `claude` subprocess per worker, streaming JSON event handling
  - `WORKFLOW.md` loader (Liquid + Markdown, hot reload)
  - Per-issue workspace dir lifecycle (multi-repo via `(repo, issue)` key)
  - `linear_graphql` proxy tool exposed to the agent
  - Pre-implementation kiro-spec gate (daemon-enforced)
  - Pre-PR kiro-review gate (daemon-enforced)
  - Post-merge flow-doc distill (kiro / superpowers / plan output sweep)
  - Optional HTTP API + ratatui TUI for observability
  - `SPEC.md` (language-agnostic) alongside the Rust impl

- **Out**:
  - Persistent database (deliberately; restart-recovery via Linear + filesystem)
  - Linear write logic in Rust (the agent does Linear writes via Linear MCP / `linear_graphql` proxy)
  - PR creation / commit / push logic in Rust (agent owns these via `gh` CLI)
  - Container-based isolation (rely on Claude Code's `workspace-write` sandbox + filesystem path safety)
  - Auto-merge orchestration (deferred; v1 stops at PR open / human review)
  - Multi-host SSH workers (symphony has it; roki defers)
  - Multi-tenant orchestration (one daemon per developer)

## Constraints
- **Language**: Rust 2024, tokio async runtime.
- **Engine**: Claude Code, headless mode (`claude --print --output-format stream-json`); slash commands not available in `-p`, so roki contracts via natural-language prompts + kiro skills auto-invoking by description, plus optional `--agents` JSON for explicit step agents.
- **Skills**: kiro skills depended on as personal skills under `~/.claude/skills/kiro-*` (not vendored, not plugin-namespaced; namespacing breaks auto-trigger).
- **Linear API**: rate limit 5,000 req/hr; Linear discourages polling — webhook receiver is the hot path, polling is the fallback for active tickets at <=5min cadence with 429 backoff.
- **Permissions**: `permissions.allow` allowlist is flaky in 2026; daemon supports config-driven fallback to `--dangerously-skip-permissions`.
- **Platform**: macOS + Linux. Terminal compatibility for TUI: iTerm2 / Ghostty / WezTerm / Alacritty primary; macOS Terminal.app limited (RGB color caveats).

## Boundary Strategy
- **Why this split**: roki-mvp is the symphony-parity vertical slice — without it, nothing else has anywhere to plug in. The two kiro gates (spec, review) are roki's actual differentiators and are independent of each other; they bolt onto the same state-machine seam in roki-mvp. Observability (HTTP + TUI) is one cohesive UX, not two; splitting it would create a synthetic seam. Distill-postmerge is independent of all gates and can ship anytime after MVP.
- **Shared seams to watch**:
  - **State machine extension points**: roki-mvp must publish stable hook points so spec-gate, review-gate, and observability can subscribe without forking the orchestrator.
  - **Agent-side tool registry**: roki-mvp exposes `linear_graphql`; spec-gate and review-gate may need additional read-only tools (e.g. `kiro_spec_status`, `kiro_review_status`).
  - **`WORKFLOW.md` schema**: each gate adds keys (e.g. `gates.spec.required_status`, `gates.review.required_status`) — schema must remain stable across spec evolutions.
  - **Workspace path layout**: `.kiro/specs/<issue>/` lives inside the workspace; spec-gate and distill-postmerge both touch it.

## Specs (dependency order)
- [ ] roki-mvp -- symphony-parity vertical slice: SPEC.md + Rust skeleton + Linear poll + claude session + workspace + run loop. Multi-repo from day one. Dependencies: none
- [ ] roki-spec-gate -- daemon-enforced pre-implementation kiro-spec gate; pre-impl distill flow merging ticket EARS into project EARS. Dependencies: roki-mvp
- [ ] roki-review-gate -- daemon-enforced pre-PR kiro-review gate; refuses In Review transition without review-pass artifact. Dependencies: roki-mvp
- [ ] roki-observability -- optional HTTP API (axum) + ratatui TUI client; symphony /api/v1/state schema. Dependencies: roki-mvp
- [ ] roki-distill-postmerge -- post-merge classifier for flow-type docs (design.md, tasks.md, plan outputs); routes delete / archive / distill. Dependencies: roki-mvp
