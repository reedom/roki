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
- A `roki` Rust binary that runs as a daemon, polls Linear (or accepts webhooks), launches an isolated workspace per ticket, runs a long-lived **orchestrator session** (`claude --input-format stream-json --output-format stream-json`) for thinking + Linear writes, drives short-lived bounded phase subprocesses (`claude -p '/kiro-* <args>' --output-format stream-json`) for code-changing work, and observes the agents through Linear state transitions to PR open.
- The daemon never writes Linear, never creates PRs, never edits code. Linear writes happen through the orchestrator session via the operator's installed Linear MCP. PR / code effects happen through phase subprocesses via `gh` / `git` / Bash inside their sandboxes.

## Approach
Symphony-parity for the daemon shape; Claude Code for the agent. Rust 2024 + tokio. In-memory orchestrator (no SQLite), tracker-driven recovery via Linear + filesystem on restart. Per-issue workspace dir (sanitized identifier under workspace root), keyed by Linear issue id alone. One ticket = one repo: orchestrator session A classifies an admitted issue (`act` / `noop` / `needs_split` / `allowlist_rejected`); the daemon validates `act` against the allowlist; multi-repo or off-allowlist tickets are rejected back to the operator (`needs-split` / `allowlist_rejected` Linear label + comment) by A itself via Linear MCP. `WORKFLOW.md` (Liquid + Markdown, hot reload) is the user-facing policy artifact, with one named template block: `prompt_template_orchestrator`. Engine-adapter contract: a long-lived orchestrator session A per ticket plus zero or more short-lived bounded phase subprocesses A nominates via `action=run_phase`; events drive a state machine; per-phase `--max-turns`, A bounded by `extension.orchestrator.max_phases`; clean exit is terminal (no daemon-side retry on clean exit) — the review gate may Deny the clean exit and trigger an intentional re-launch with `additional_context` plumbed through `gate_deny → run_phase`; non-clean phase exits use a configurable retry budget with exponential backoff between attempts. Setup-judge subprocess and linear-updater subagent are removed: the admission decision is A's job (`admission_request` event), and daemon-only failure surfacing is A's job (`daemon_directive` event). When A is dead (`orchestrator_crash` / `orchestrator_unparseable` / `orchestrator_budget_exhausted`), surfacing falls back to structured log + TUI escalation queue — the daemon does not write Linear directly.

## Scope
- **In**:
  - Rust binary (clap CLI, tokio runtime, tracing logs)
  - `WORKFLOW.md` loader: Liquid + Markdown front matter, hot reload, schema validation; one named template block (`prompt_template_orchestrator`)
  - In-memory orchestrator state machine (per-issue lifecycle, keyed by issue id alone, six states with an `Inactive` reason discriminator including `orchestrator_crash`, `orchestrator_unparseable`, `orchestrator_budget_exhausted`)
  - Linear GraphQL client (read-only for daemon; webhook receiver + polling fallback)
  - Tracker normalization (issue model, state extraction, label extraction)
  - Per-issue workspace directory lifecycle (create on `act`-and-validated admission decision, delete on terminal Linear state, sanitize identifier, path-safety invariants); single repo per issue
  - Long-lived orchestrator session A adapter (`claude --input-format stream-json --output-format stream-json`): launch on `Discovered → Pending`, JSON event writes to stdin, JSON action parsing on stdout (last object per turn after extended-thinking), schema validation, `max_phases` enforcement, schema-drift detection (two consecutive turns → `orchestrator_unparseable`), stall detection
  - Short-lived bounded phase subprocess adapter (`claude -p '/kiro-* <args>' --output-format stream-json`): launch on A's `run_phase`, stream-json event parsing, lifecycle event mapping, `--max-turns`, stall detection by event-inactivity. Phase catalog: `implement` (kiro-impl), `validate` (kiro-validate-impl), `open_pr` (custom prompt, no skill), `ci_fix` (kiro-debug + kiro-verify-completion), `finalize_review` (review.md synthesis)
  - Daemon-only failure surfacing through A processing `daemon_directive` events (Linear label + comment via Linear MCP) when A is alive; structured log + TUI escalation queue only for the three orchestrator-dead failure paths
  - Bounded loops: `max_phases` per ticket on A (default 20); `--max-turns` per phase (configurable); exponential backoff between phase re-launches on non-clean exit (10s -> 5min); retry budget on phase non-clean exit; clean exit is terminal except for review-gate-driven intentional re-launches
  - Single repo per ticket: A's `admission_decision` yields exactly one allowlisted repo (`act`) or no repo (`noop`); A-returned `act` classifications naming more than one repo or an off-allowlist repo are rejected back to the operator by A itself via Linear MCP, and the daemon double-validates against the allowlist before materializing the worktree
  - Configurable phase-subprocess permissions: prefer `--settings` allowlist, fallback to `--dangerously-skip-permissions`. Orchestrator session A always runs read-only via `--settings` with `extension.orchestrator.allowed_tools` (Linear MCP write + `Read`)
  - Default phase-subprocess sandbox = `workspace-write` + reject elicitations (overridable via `WORKFLOW.md`); A always runs with a read-only filesystem sandbox regardless of operator overrides

- **Out** (scoped to follow-up specs):
  - kiro-spec gate enforcement (roki-spec-gate)
  - kiro-review gate enforcement (roki-review-gate)
  - HTTP / TUI observability (roki-observability)
  - Post-merge flow-doc sweep (handled in CI, not a roki concern)
  - Container / VM isolation
  - SSH multi-host workers
  - Auto-merge
  - A-side context compaction across long-running tickets (deferred — `max_phases` bounds session length for MVP)

## Boundary Candidates
- **CLI shell vs orchestrator core**: the binary entrypoint and arg parsing are testable separately from the supervisor / actor system.
- **Linear adapter vs orchestrator core**: tracker reading and webhook decoding can be mocked behind a trait; orchestrator only sees normalized issues.
- **Engine adapter (claude session) vs orchestrator core**: two subprocess shapes — A's stdio session and the per-phase `-p` invocations — share the same adapter surface (lifecycle, stall detection, stream-json parsing); orchestrator core only sees lifecycle events.
- **Workspace lifecycle vs orchestrator core**: workspace create / delete is a separate concern with its own path-safety invariants.
- **`WORKFLOW.md` loader vs orchestrator core**: schema validation and hot reload are independent.

## Out of Boundary
- Persistent state (SQLite, etc.) -- deliberately rejected per symphony precedent.
- Any logic that writes Linear state, creates PRs, or commits code -- that is the agent's job, full stop. Linear writes are A's job (orchestrator session) via Linear MCP; the daemon never writes Linear directly.
- Any kiro-specific phase logic -- gates are separate specs.
- TUI / HTTP server -- separate observability spec (the escalation queue is the live surface for the three orchestrator-dead failure paths).
- Multi-repo tickets -- one ticket = one repo; A's admission decision rejects multi-repo classifications back to the operator via Linear MCP.
- Post-merge flow-document distill / archive sweep -- handled in CI, not by the daemon.
- Slack and other push-style operator notification channels -- daemon-only failures surface through A → Linear MCP (when A is alive) or via TUI escalation queue + structured log only (when A is dead).
- Multi-host (SSH) workers -- deferred.

## Upstream / Downstream
- **Upstream**: Linear API; Claude Code CLI in two invocation shapes (`claude --input-format stream-json --output-format stream-json` for orchestrator A; `claude -p '/kiro-* <args>' --output-format stream-json` for phase subprocesses); user-installed kiro skills under `~/.claude/skills/kiro-*`; `gh` CLI (in-agent, not daemon).
- **Downstream**: roki-spec-gate, roki-review-gate, roki-observability all depend on this MVP's state-machine extension points and the engine adapter's two-shape invocation taxonomy. (Post-merge flow-document distill is handled in CI, not by roki.)

## Existing Spec Touchpoints
- **Extends**: none (greenfield).
- **Adjacent**: monorail (separate repo, prior attempt; reference for domain naming, NOT for code reuse).

## Constraints
- Slash commands (`/kiro-*`) work in `claude -p` headless mode as the initial prompt argument; they are how phase subprocesses select their kiro skill. They do not work mid-session inside A's stream-json input — A drives phase choice via JSON action directives, not slash commands.
- Plugin-namespaced skills do not auto-trigger; kiro must live as personal skills under `~/.claude/skills/kiro-*`.
- `--bare` skips skill discovery; daemon must pass `--plugin-dir` / `--settings` explicitly for phase subprocesses.
- Linear discourages polling; cap at <=5min cadence for active tickets, respect 429 backoff. Webhooks preferred when reachable.
- `permissions.allow` allowlist is flaky in 2026; expose a config knob for `--dangerously-skip-permissions` fallback (phase subprocesses only; A always runs read-only).
- macOS + Linux. Windows is out of scope for v1.
