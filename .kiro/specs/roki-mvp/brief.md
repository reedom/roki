# Brief: roki-mvp

## Problem
Developers manually shepherd Linear tickets through implementation: read ticket, set up repo, prompt the agent, watch it work, transition Linear states, open PR. The supervision burden scales linearly with ticket volume, and humans drift between tickets while waiting on the agent. We want a near-one-way Linear -> PR path with guardrails that doesn't require constant minding.

## Current State
- The previous attempt (monorail, Rust) reached a "skill-first, small daemon" pivot but never finished it: the orchestrator commands and step agents were never built, and the Rust pipeline modules became safety-net deprecated code.
- Symphony (openai/symphony, Elixir) demonstrates a working version of the same shape with codex: long-lived stdio agent subprocess, in-memory state, `WORKFLOW.md` as policy, no persistent DB.
- roki is greenfield: only `.claude/skills/kiro-*` and `CLAUDE.md` exist.

## Desired Outcome
- A `roki` Rust binary that runs as a daemon, polls Linear (or accepts webhooks), launches an isolated workspace per ticket, runs a long-lived `claude` session with kiro + superpowers skills available, and observes the agent through Linear state transitions to PR open.
- The daemon never writes Linear, never creates PRs, never edits code. The agent does all of that via Linear MCP / `linear_graphql` proxy / `gh` CLI inside its sandbox.
- A `SPEC.md` at the repo root captures the contract language-agnostically so future ports / forks remain consistent.

## Approach
Symphony-parity for the daemon shape; Claude Code for the agent. Rust 2024 + tokio. In-memory orchestrator (no SQLite), tracker-driven recovery via Linear + filesystem on restart. Per-issue workspace dir (sanitized identifier under workspace root), multi-repo from day one keyed `(repo, issue)`. `WORKFLOW.md` (Liquid + Markdown, hot reload) is the user-facing policy artifact. Engine-adapter contract: long-lived `claude --print --output-format stream-json` subprocess; events drive a state machine; `max_turns` per worker, exponential backoff between worker sessions, continuation retry on clean exit.

## Scope
- **In**:
  - `SPEC.md` at repo root (language-agnostic)
  - Rust binary (clap CLI, tokio runtime, tracing logs)
  - `WORKFLOW.md` loader: Liquid + Markdown front matter, hot reload, schema validation
  - In-memory orchestrator state machine (per-issue worker lifecycle)
  - Linear GraphQL client (read-only for daemon; webhook receiver + polling fallback)
  - Tracker normalization (issue model, state extraction, label extraction)
  - Per-issue workspace directory lifecycle (create on active, delete on terminal, sanitize identifier, path-safety invariants)
  - Long-lived `claude` subprocess adapter: launch, stream JSON event parser, state machine, max_turns, stall detection by event-inactivity
  - `linear_graphql` proxy tool exposed to the agent (single GraphQL operation per call; daemon owns auth)
  - Bounded loops: max_turns per worker (default 20), exponential backoff between worker invocations (10s -> 5min), 1s continuation retry on clean exit
  - Multi-repo: workspace keyed `(repo, issue)`; one daemon serves multiple repos
  - Configurable permissions: prefer `--settings` allowlist, fallback to `--dangerously-skip-permissions`
  - Default sandbox = `workspace-write` + reject elicitations (override via `WORKFLOW.md`)

- **Out** (scoped to follow-up specs):
  - kiro-spec gate enforcement (roki-spec-gate)
  - kiro-review gate enforcement (roki-review-gate)
  - HTTP / TUI observability (roki-observability)
  - Post-merge flow-doc sweep (roki-distill-postmerge)
  - Container / VM isolation
  - SSH multi-host workers
  - Auto-merge

## Boundary Candidates
- **CLI shell vs orchestrator core**: the binary entrypoint and arg parsing are testable separately from the supervisor / actor system.
- **Linear adapter vs orchestrator core**: tracker reading and webhook decoding can be mocked behind a trait; orchestrator only sees normalized issues.
- **Engine adapter (claude session) vs orchestrator core**: the stdio subprocess + event parser is its own seam; orchestrator only sees lifecycle events.
- **Workspace lifecycle vs orchestrator core**: workspace create / delete is a separate concern with its own path-safety invariants.
- **`WORKFLOW.md` loader vs orchestrator core**: schema validation and hot reload are independent.

## Out of Boundary
- Persistent state (SQLite, etc.) -- deliberately rejected per symphony precedent.
- Any logic that writes Linear state, creates PRs, or commits code -- that is the agent's job, full stop.
- Any kiro-specific phase logic -- gates are separate specs.
- TUI / HTTP server -- separate observability spec.
- Distillation -- separate spec.
- Multi-host (SSH) workers -- deferred.

## Upstream / Downstream
- **Upstream**: Linear API; Claude Code CLI (`claude --print --output-format stream-json`); user-installed kiro skills under `~/.claude/skills/kiro-*`; `gh` CLI (in-agent, not daemon).
- **Downstream**: roki-spec-gate, roki-review-gate, roki-observability, roki-distill-postmerge all depend on this MVP's state-machine extension points and tool registry.

## Existing Spec Touchpoints
- **Extends**: none (greenfield).
- **Adjacent**: monorail (separate repo, prior attempt; reference for domain naming, NOT for code reuse).

## Constraints
- Slash commands (`/kiro-*`) do not work in `claude -p` headless mode. Engine-adapter contract uses natural-language prompts that auto-trigger kiro skills via their description fields, plus optional `--agents` JSON for explicit step agents.
- Plugin-namespaced skills do not auto-trigger; kiro must live as personal skills under `~/.claude/skills/kiro-*`.
- `--bare` skips skill discovery; daemon must pass `--plugin-dir` / `--settings` explicitly.
- Linear discourages polling; cap at <=5min cadence for active tickets, respect 429 backoff. Webhooks preferred when reachable.
- `permissions.allow` allowlist is flaky in 2026; expose a config knob for `--dangerously-skip-permissions` fallback.
- macOS + Linux. Windows is out of scope for v1.
