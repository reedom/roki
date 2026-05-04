---
refs:
  id: brief:roki-mvp
  kind: brief
  title: "roki-mvp Brief"
  spec: roki-mvp
---

# Brief: roki-mvp

## Problem
Developers manually shepherd Linear tickets through implementation: read ticket, set up repo, prompt the agent, watch it work, transition Linear states, open PR. The supervision burden scales linearly with ticket volume, and humans drift between tickets while waiting on the agent. We want a near-one-way Linear → PR path with guardrails that does not require constant minding.

## Current State
- The previous attempt (monorail, Rust) reached a "skill-first, small daemon" pivot but never finished it.
- Symphony (openai/symphony, Elixir) demonstrates a working version of the same shape with codex: long-lived stdio agent subprocess, in-memory state, `WORKFLOW.md` as policy, no persistent DB.
- roki is greenfield with kiro + roki skills under `~/.claude/skills/`.

## Desired Outcome
- A `roki` Rust binary that runs as a daemon, polls Linear (or accepts webhooks), gates every incoming event through a mechanical 4-condition pre-admission-judge in Rust (`assignee` + Linear state ∈ `admit_states` + `roki:ready` label + optional `roki:impl` selecting `mode`), launches a long-lived per-ticket **orchestrator session** (`claude --input-format stream-json --output-format stream-json`) for thinking + Linear writes + structural artifact validation, and supervises short-lived bounded **phase subprocesses** (`claude -p '/<skill> <args>'` for skill-driven phases or `claude --input-format stream-json` with daemon-internal Liquid template for direct-mode and `open_pr`) for code-changing work.
- The daemon never writes Linear, never creates PRs, never edits code, and never registers / proxies / wraps any agent-side tool. Linear writes flow through the orchestrator session via the operator's installed Linear MCP. PR / code effects flow through phase subprocesses via `gh` / `git` / Bash inside their sandboxes.

## Approach
Symphony-parity for the daemon shape; Claude Code for the agent. Rust 2024 + tokio. In-memory orchestrator (no SQLite), tracker-driven recovery via Linear + filesystem on restart. Per-ticket `mode` flag (`SPEC_DRIVEN` vs `NEEDS_CLASSIFY`) is set on entry to `Pending` and immutable for the orchestrator-session lifetime. SPEC_DRIVEN uses an operator-completed project-level `<repo>/.kiro/specs/<target>/` and the orchestrator validates target spec docs structurally on its first turn. NEEDS_CLASSIFY first turn nominates the `classify` phase (`roki-classify`, `--max-turns 5`, tool surface pinned to `Read` + `Glob` + `Grep`); the orchestrator branches on `result.path` (Path B → direct implementation; Path A/C/D/E → operator hand-off via Linear comment). One ticket = one repo: orchestrator-resolved repo identifiers are validated against the operator-declared `[[repos]]` allowlist; multi-repo or off-allowlist tickets stop with `outcome ∈ {needs_split, allowlist_rejected}` and an orchestrator-driven Linear comment. `WORKFLOW.md` (Liquid + Markdown, hot reload) is the single workspace-level policy artifact with four required named template blocks: `prompt_template_orchestrator`, `prompt_template_implement_direct`, `prompt_template_validate_direct`, `prompt_template_open_pr`. Engine-adapter contract: one long-lived orchestrator session per ticket plus zero or more short-lived bounded phase subprocesses the orchestrator nominates via `action=run_phase`; each phase has its own `--max-turns` budget; orchestrator bounded by `extension.orchestrator.max_phases` (default 15) — daemon-internal phase replays consume zero `max_phases` slots. When the orchestrator is alive, daemon-only failures (phase stall, retry exhaustion, fs poison, recovery orphan) flow as `daemon_directive` events to its stdin and the orchestrator writes Linear via Linear MCP; when dead (`orchestrator_crash` / `orchestrator_unparseable` / `orchestrator_budget_exhausted`), surfacing falls back to structured log + TUI escalation queue only — the daemon never writes Linear directly.

## Scope
- **In**:
  - Rust binary (clap CLI, tokio runtime, tracing logs with redaction).
  - `WORKFLOW.md` loader: Liquid + Markdown front matter, hot reload, schema validation; four required named template blocks plus optional per-phase `prompt_template_<phase>` blocks.
  - In-memory orchestrator state machine: 5 states (`Pending` / `Active` / `Backoff` / `Inactive` / `Cleaning`) keyed by Linear issue id alone, with a 12-value `Inactive.reason` discriminator including the three orchestrator-dead reasons.
  - Linear GraphQL client (read-only on the daemon side); single workspace-level webhook receiver + polling fallback (cap ≤5 min cadence, 429 backoff).
  - Per-issue session-tempdir lifecycle (created on entry to `Pending`); daemon-driven worktree materialization idempotent on every non-`classify` phase nomination (`ghq` + `wt switch-create` first call, `wt list` verify subsequent calls); cleanup on terminal Linear state via allowlist iteration filtered by branch == issue id (`wt remove`; branches not deleted).
  - Long-lived orchestrator-session adapter: launch on entry to `Pending` with `mode` rendered into `prompt_template_orchestrator`, JSON event writes to stdin, JSON action parsing on stdout (last object per turn after extended-thinking), schema validation, `max_phases` enforcement, schema-drift detection (two consecutive turns → `Inactive(orchestrator_unparseable)`), stall detection via `extension.orchestrator.stall_seconds` (default 600).
  - Short-lived bounded phase-subprocess adapter: spawn one per `action=run_phase`; phase catalog with mode-aware defaults — `classify` (NEEDS_CLASSIFY first turn only), `implement` (SPEC_DRIVEN: `kiro-impl`; NEEDS_CLASSIFY: daemon-internal `prompt_template_implement_direct`), `review` (`kiro-review`), `validate` (SPEC_DRIVEN: `kiro-validate-impl`; NEEDS_CLASSIFY: `prompt_template_validate_direct`), `open_pr` (`prompt_template_open_pr`), `ci_fix` (`roki-ci-fix`), `finalize_review` (`roki-finalize-review`); per-phase `--max-turns`; per-phase stall detection; daemon-internal replay loop on `phase_nonclean` (`max_attempts` default 3, exponential backoff 10s..5min, zero `max_phases` consumption).
  - Orchestrator-driven artifact validation: `review.md` validated by the orchestrator after each `finalize_review` clean exit using `Read` + `Bash` (read-only sandbox); on structural failure the orchestrator re-nominates `implement` with `additional_context` carrying failing per-criterion entries; on retry-budget exhaustion it writes Linear via Linear MCP and emits `action=stop outcome=failure`.
  - Daemon-only failure surfacing: `daemon_directive` events to a live orchestrator (kinds: phase stall, retry exhaustion, fs poison, recovery orphan); three orchestrator-dead reasons routed to `Inactive(reason)` + structured log + TUI escalation queue without any Linear write.
  - Configurable phase-subprocess permissions (`--settings` allowlist default; `--dangerously-skip-permissions` fallback); `workspace-write` sandbox + reject elicitations default. Orchestrator session always pinned read-only filesystem + reject elicitations regardless of operator overrides; classify phase additionally pinned to `Read` + `Glob` + `Grep`.

- **Out** (deferred / scoped to follow-up specs):
  - HTTP / TUI observability — `roki-observability`.
  - Post-merge flow-doc sweep — handled in CI, not by the daemon.
  - Container / VM isolation; SSH multi-host workers; auto-merge orchestration; persistent state stores.
  - Multi-repo tickets; operator-renamable label conventions; mode mutation mid-flight.
  - Orchestrator-side context compaction across long-running tickets (deferred — `max_phases` bounds session length for MVP).

## Boundary Candidates
- **CLI shell vs orchestrator core** — the binary entrypoint and arg parsing are testable separately from the actor system.
- **Linear adapter + pre-admission-judge vs orchestrator core** — tracker reading, webhook decoding, and the 4-condition Rust filter can be mocked behind traits; orchestrator only sees `Admit { issue, mode }` plus `AssignmentLost` / `RokiReadyRemoved` signals.
- **Orchestrator-session adapter vs phase-subprocess adapter vs orchestrator core** — two engine shapes share lifecycle / stall / stream-json primitives but expose distinct adapter surfaces; orchestrator core only sees `OrchestratorAction` outcomes and `DaemonEvent`s.
- **Session manager + worktree manager vs orchestrator core** — per-issue tempdir lifecycle, idempotent worktree ensure, and allowlist-iteration cleanup are isolated concerns.
- **`WORKFLOW.md` loader vs orchestrator core** — schema validation, hot reload, and rendering (mode-flag substitution + `additional_context` channel) are independent.

## Out of Boundary
- Persistent state — deliberately rejected per symphony precedent.
- Any logic that writes Linear state, creates PRs, or commits code — agent's job, full stop.
- Daemon-registered, daemon-proxied, or daemon-wrapped agent-side tools.
- Daemon-side mechanical artifact-validation gates — owned by the orchestrator.
- TUI / HTTP server — `roki-observability`.
- Multi-repo tickets, operator-renamable labels, mode mutation mid-flight, post-merge distill, Slack push channels, multi-host workers, Windows support.

## Upstream / Downstream
- **Upstream**: Linear API; Claude Code CLI in two invocation shapes (`claude --input-format stream-json --output-format stream-json` for the orchestrator session; `claude -p '/<skill> <args>'` and `claude --input-format stream-json` for phase subprocesses); operator-installed kiro + roki skills under `~/.claude/skills/{kiro,roki}-*`; `wt` (worktrunk) and `ghq` external CLIs on `$PATH`; operator's installed Linear MCP integration.
- **Downstream**: `roki-observability` depends on this MVP's state-machine subscription hooks, `OrchestratorRead` snapshot, `TrackerRefresh` nudge, the four required `WORKFLOW.md` template blocks, the reserved `extension.*` namespaces, and the engine-adapter `additional_context` channel.

## Existing Spec Touchpoints
- **Extends**: none (greenfield).
- **Adjacent**: monorail (separate repo, prior attempt; reference for domain naming, NOT for code reuse).

## Constraints
- Slash commands (`/<skill> <args>`) work in `claude -p` headless mode as the initial prompt argument; phase subprocesses select their kiro/roki skill that way. Slash commands do NOT work mid-session inside the orchestrator's stream-json input — the orchestrator drives phase choice via JSON action directives, not slash commands.
- Plugin-namespaced skills do not auto-trigger; kiro + roki must live as personal skills under `~/.claude/skills/`.
- Linear discourages polling; cap at ≤5 min cadence for active tickets, respect 429 backoff. Webhooks preferred when reachable.
- `permissions.allow` allowlist is flaky in 2026; expose a config knob for `--dangerously-skip-permissions` fallback (phase subprocesses only; the orchestrator always runs read-only).
- macOS + Linux. Windows is out of scope for v1.
