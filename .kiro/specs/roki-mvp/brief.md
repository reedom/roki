---
refs:
  id: brief:roki-mvp
  kind: brief
  title: "roki-mvp Brief"
  spec: roki-mvp
---

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
Symphony-parity for the daemon shape; Claude Code for the agent. Rust 2024 + tokio. In-memory orchestrator (no SQLite), tracker-driven recovery via Linear + filesystem on restart. Per-issue workspace dir (sanitized identifier under workspace root), keyed by Linear issue id alone. One ticket = one repo: the setup judge classifies an admitted issue into `act` (with exactly one allowlisted repo) or `noop`; multi-repo tickets are rejected back to the operator (`needs-split` Linear label + comment) via the linear-updater subagent. `WORKFLOW.md` (Liquid + Markdown, hot reload) is the user-facing policy artifact, with three named template blocks: `prompt_template_setup`, `prompt_template_worker`, `prompt_template_linear_updater`. Engine-adapter contract: a long-lived `claude --print --output-format stream-json` subprocess for the main worker plus two short-lived bounded one-shot invocations of the same engine for the setup judge and the linear-updater; events drive a state machine; `max_turns` per worker; clean exit is terminal (no daemon-side retry on clean exit) — the review gate may Deny the clean exit and trigger an intentional re-launch with `additional_context`; non-clean exits use a configurable retry budget with exponential backoff between attempts.

## Scope
- **In**:
  - `SPEC.md` at repo root (language-agnostic)
  - Rust binary (clap CLI, tokio runtime, tracing logs)
  - `WORKFLOW.md` loader: Liquid + Markdown front matter, hot reload, schema validation; three named template blocks (`prompt_template_setup`, `prompt_template_worker`, `prompt_template_linear_updater`)
  - In-memory orchestrator state machine (per-issue worker lifecycle, keyed by issue id alone, six states with an `Inactive` reason discriminator)
  - Linear GraphQL client (read-only for daemon; webhook receiver + polling fallback)
  - Tracker normalization (issue model, state extraction, label extraction)
  - Per-issue workspace directory lifecycle (create on active, delete on terminal, sanitize identifier, path-safety invariants); single repo per issue
  - Long-lived `claude` subprocess adapter for the main worker plus two short-lived bounded one-shot invocations of the same engine for the setup judge and the linear-updater: launch, stream JSON event parser, lifecycle event mapping, max_turns, stall detection by event-inactivity
  - linear-updater subagent: a setup-judge-shaped one-shot `claude` invocation that translates daemon-only failure events (stall, retry exhaustion, multi-repo rejection, judge unparseable, fs poison, orphan recovery) into Linear label additions and comments via the operator's installed Linear MCP. The daemon never issues a Linear write itself
  - Bounded loops: max_turns per worker (default 20), exponential backoff between worker re-launches on non-clean exit (10s -> 5min), retry budget on non-clean exit; clean exit is terminal except for review-gate-driven intentional re-launches
  - Single repo per ticket: setup judge yields exactly one allowlisted repo (`act`) or no repo (`noop`); judge findings naming more than one repo are rejected back to the operator via linear-updater
  - Configurable permissions: prefer `--settings` allowlist, fallback to `--dangerously-skip-permissions`
  - Default sandbox = `workspace-write` + reject elicitations for the main worker (overridable via `WORKFLOW.md`); the setup judge and linear-updater always run with a read-only filesystem sandbox regardless of operator overrides
  - Daemon-only failure surfacing through linear-updater (Linear label + comment) plus the optional TUI escalation queue. No Slack or other push channel

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
- Any logic that writes Linear state, creates PRs, or commits code -- that is the agent's job, full stop. The linear-updater subagent is an agent invocation, not a daemon-side write path.
- Any kiro-specific phase logic -- gates are separate specs.
- TUI / HTTP server -- separate observability spec.
- Multi-repo tickets -- one ticket = one repo; the setup judge rejects multi-repo classification back to the operator via linear-updater.
- Post-merge flow-document distill / archive sweep -- handled in CI, not by the daemon.
- Slack and other push-style operator notification channels -- daemon-only failures surface through linear-updater (Linear) and the TUI escalation queue.
- Multi-host (SSH) workers -- deferred.

## Upstream / Downstream
- **Upstream**: Linear API; Claude Code CLI (`claude --print --output-format stream-json`); user-installed kiro skills under `~/.claude/skills/kiro-*`; `gh` CLI (in-agent, not daemon).
- **Downstream**: roki-spec-gate, roki-review-gate, roki-observability all depend on this MVP's state-machine extension points and the engine adapter's bounded-invocation taxonomy. (roki-distill-postmerge is no longer active; flow-document distill is handled in CI.)

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
