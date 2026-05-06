---
refs:
  id: roadmap
  kind: roadmap
  title: "roki Roadmap"
---

# Roadmap

## Overview

roki is a generic, config-driven engine for Linear-driven coding work. The daemon has four responsibilities and no domain logic:

1. Receive Linear webhooks, gate via admission filter (assignee + `[[admission.repos]]`), detect ticket-property changes against an in-memory diff cache.
2. Match each diff against operator-authored `[[cleanup]]` / `[[rule]]` / `[[on_failure]]` lists and run a cycle composed of three phases: pre → run → post.
3. Create per-ticket worktrees and session tempdirs, capture each phase's stdout/stderr to disk, parse a structured `directive` from the last JSON object on stdout, loop or terminate accordingly.
4. Expose log / event / repo-path access through `roki log`, `roki events`, `roki repo`, and an HTTP API.

All workflow knowledge — phase catalog, retry policy, Linear write formatting, model selection, kiro skills — lives in operator-authored TOML and Markdown. The daemon does not know about kiro, codex, or claude specifics.

Feature requirements: `docs/fr/01-..-12-..md`.

## Approach

Walking-skeleton + breadth-first. Wave 0 ships a minimal end-to-end backbone: CLI start → webhook receive → assignee filter → hardcoded repo resolve → first-match `[[rule]]` → cmd-form phase → exit. Wave 0 pins `tests/e2e/skeleton_smoke.rs`; every later wave adds one capability and must keep that smoke green.

Each spec is small enough to land in one PR. Within a wave, specs are mutually independent unless a `Deps:` line says otherwise, so `kiro-spec-batch` can dispatch a wave in parallel.

## Scope

- **In** (per design §2.3):
  - Webhook intake + admission gate (assignee filter, `[[admission.repos]]` first-match repo resolve).
  - Rule dispatch: `[[cleanup]]` then `[[rule]]` first-match per diff; cycle = pre → run → post loop driven by structured `directive` JSON.
  - Phase subprocess lifecycle: long-lived AI session reused across one cycle's pre/post, or one-shot command form. stdout/stderr capture to files. Stall detection + SIGTERM. `iteration_exhausted` cooperative termination.
  - Per-ticket worktree (lazy on first `pre.directive=run`) + session tempdir (eager at admission). Cleanup on `[[cleanup]]` cycle, admission-filter eviction, or cold-start orphan reconciliation.
  - In-memory diff cache `(ticket_id) → {status, labels, assignee, repo, workflow_path}`; no persistent DB.
  - `[[on_failure]]` first-match for daemon-detected failures (`process_crash`, `unparseable`, `schema_drift`, `stall`, `iter_exhausted`, `template_error`); failure cycles do not chain.
  - TUI escalation queue (in-memory ring) for failures with no operator handler.
  - Hot reload of `WORKFLOW.toml` + `workflow/*.md`; in-flight cycles unaffected; reload failure keeps previous policy.
  - Per-repo TOML override via `[[admission.repos]] workflow = "repos/<repo>.toml"`.
  - Liquid template variables (`ticket.* / repo.* / cycle.* / pre.* / post.* / run.* / failure.*`) with matching `ROKI_*` env vars.
  - Tracing-based structured event log (stdout / file / both); ring buffer for HTTP and TUI.
  - HTTP API under `/api/v1/` (axum); ratatui TUI client.
  - `roki repo` CLI for phases that need worktree / ghq base path.

- **Out** (per design §2.2):
  - Persistent database. State lives in Linear + filesystem; restart re-derives.
  - Daemon-driven Linear writes. Operators write to Linear from inside their phases.
  - Daemon-known phase catalog (`classify`, `implement`, `review`, `validate`, `open_pr`, `ci_fix`, `finalize_review`).
  - 5-state per-issue state machine; 12-variant `Inactive.reason`.
  - `SPEC_DRIVEN` / `NEEDS_CLASSIFY` mode flag.
  - Daemon-side retry budget / exponential backoff. Encoded in operator post directives.
  - Long-lived per-ticket orchestrator session spanning multiple cycles. AI sessions are scoped to one cycle.
  - `materialize_spec` / pre-admission-judge concepts.
  - Daemon-registered agent-side tools. Phases inherit the operator's local Claude Code / MCP installation as-is.
  - Container or VM isolation; multi-host SSH workers; multi-tenant orchestration.
  - Auto-merge orchestration; PR creation logic. Operators encode via their phases.
  - Multi-repo tickets handled by the daemon. The operator's pre can detect and emit `directive: end, outcome: needs_split` with a Linear write of its own.
  - Windows support.

## Constraints

- **Language**: Rust 2024, tokio async runtime.
- **Engine surface**: the daemon spawns subprocesses described by operator config (`cli` strings, `path` Liquid templates, `prompt` / `cmd` inline forms). Claude Code is one possible engine, not hardcoded; codex or any CLI that emits stream-json or a final JSON line on stdout works.
- **Linear API**: webhook hot path (HMAC verify against `[linear].webhook_secret`); polling fallback ≤5min cadence with 429 backoff. Daemon-side Linear access is read-only.
- **External CLIs**: operator must install `wt` (worktrunk) and `ghq` for worktree materialization and cleanup.
- **Permissions / sandboxing**: phases inherit the operator's Claude Code / shell permissions. The daemon does not impose its own sandbox.
- **Operator notifications**: when an `[[on_failure]]` handler matches, the handler writes Linear (or wherever) itself. When no handler matches, the daemon emits a tracing event and an in-memory escalation entry visible via TUI / HTTP.
- **Platform**: macOS + Linux. TUI primary terminals: iTerm2, Ghostty, WezTerm, Alacritty. Terminal.app limited (RGB caveats).

## Specs (dependency order)

Specs marked `[ ]` are scaffold-only (`brief.md` + `spec.json`). Requirements / design / tasks come from `/kiro-spec-*` skills.

### Wave 0 — backbone

- [ ] roki-skeleton — CLI start + roki.toml read + webhook receive (no signature verify) + assignee filter + hardcoded single-repo resolve + `[[rule]]` first-match + cmd-form phase + stdout/stderr capture + process exit. Pins `tests/e2e/skeleton_smoke.rs`.

### Wave 1 — engine breadth

- [ ] roki-engine-iteration-loop — pre → run → post directive loop; legal directive sets per phase. Deps: roki-skeleton.
- [ ] roki-engine-iter-cap — `[engine].max_iterations`; cooperative `iteration_exhausted` directive; SIGTERM fallback; `iter_exhausted` failure routing for command form. Deps: roki-engine-iteration-loop.
- [ ] roki-engine-cleanup-cycle — `[[cleanup]]` first-match; immediate-delete shorthand; cleanup-before-rule eval order. Deps: roki-engine-iteration-loop.
- [ ] roki-engine-failure-cycle — `[[on_failure]]` first-match; six failure kinds; no recursive failure. Deps: roki-engine-iteration-loop.
- [ ] roki-engine-stall — per-phase stall window; SIGTERM; routes via `kind=stall`. Deps: roki-engine-iteration-loop.
- [ ] roki-engine-queue-preemption — webhook arriving mid-cycle defers re-eval; final-state-only semantics; admission-filter loss does not preempt. Deps: roki-engine-iteration-loop.

### Wave 2 — runtime breadth

- [ ] roki-runtime-session-mode — long-lived `--input-format stream-json --output-format stream-json` session for `session`-mode phases; CLI from `[default.ai.session].cli` or frontmatter override. Deps: roki-engine-iteration-loop.
- [ ] roki-runtime-worktree-lazy — session_tempdir at admission; worktree on first `pre.directive=run`; cleanup on cycle / eviction / orphan. Deps: roki-skeleton.
- [ ] roki-runtime-template-vars — Liquid render of phase bodies; `ticket.* / repo.* / cycle.* / pre.* / post.* / run.* / failure.*`; matching `ROKI_*` env vars. Deps: roki-skeleton.
- [ ] roki-runtime-capture-layout — per-cycle / per-phase stdout/stderr capture file layout under session_tempdir; the read surface for `roki log`. Deps: roki-skeleton.

### Wave 3 — Linear adapter breadth

- [ ] roki-linear-signature-verify — `[linear].webhook_secret` HMAC check; reject unsigned / invalid. Deps: roki-skeleton.
- [ ] roki-linear-diff-cache — in-memory ticket cache; diff-on-webhook drives rule eval; eviction on assignee loss. Deps: roki-skeleton.
- [ ] roki-linear-admission-repos — full `[[admission.repos]]` matcher set (`when.labels.*`, `when.title.regex|starts_with|contains`, `when.body.*`, fallback no-`when` entry); resolves repo + optional per-repo workflow path. Deps: roki-skeleton.
- [ ] roki-linear-cold-start — at daemon start, fetch all assigned admitted tickets; reconcile against on-disk worktrees; orphan cleanup. Deps: roki-linear-admission-repos, roki-runtime-worktree-lazy.

### Wave 4 — config breadth

- [ ] roki-config-workflow-toml-full — full `WORKFLOW.toml` schema (admission + rule + cleanup + on_failure); first-match semantics; AND-within-entry, OR-via-entries; condition vocabulary. Deps: roki-skeleton.
- [ ] roki-config-workflow-md — `workflow/*.md` loader: YAML frontmatter (`session` / `command`, `cli`, `stall_seconds` override) + Liquid body. Deps: roki-config-workflow-toml-full.
- [ ] roki-config-hot-reload — file watcher triggers reload + schema validate; in-flight cycles unaffected; reload failure keeps previous policy. Deps: roki-config-workflow-toml-full.
- [ ] roki-config-per-repo-toml — `[[admission.repos]] workflow="repos/bar.toml"` replaces top-level rule / cleanup / on_failure for that repo; no merge. Deps: roki-config-workflow-toml-full.

### Wave 5 — observability

- [ ] roki-obs-tracing-pipeline — tracing layer config (level, destination=stdout|file|both, file_path); JSONL line format. Deps: roki-skeleton.
- [ ] roki-obs-event-catalog — structured event names + fields per design §8.5; emitted by daemon, not by phases. Deps: roki-obs-tracing-pipeline.
- [ ] roki-obs-ring-buffer — in-memory ring (`[log].ring_size`); read-only feed for HTTP / TUI. Deps: roki-obs-tracing-pipeline.
- [ ] roki-obs-redaction — `[linear].token` and other secrets redacted before tracing emission. Deps: roki-obs-tracing-pipeline.

### Wave 6 — CLI sub-commands

- [ ] roki-cli-daemon — `roki` (start daemon), graceful shutdown, config-path resolution. Deps: roki-skeleton.
- [ ] roki-cli-log — `roki log --cycle <id> [--phase ...]`; reads capture-layout files. Deps: roki-runtime-capture-layout.
- [ ] roki-cli-events — `roki events --since <ts>`; reads ring buffer or file. Deps: roki-obs-ring-buffer.
- [ ] roki-cli-repo — `roki repo [<ghq>] [--auto-clone] [--worktree]`; reads `ROKI_TICKET_ID` / `ROKI_REPO`. Deps: roki-runtime-worktree-lazy.

### Wave 7 — HTTP API

- [ ] roki-http-server — axum bind + `[network].bind/port`; `/api/v1/*` versioned. Deps: roki-skeleton.
- [ ] roki-http-tickets — GET active tickets + cycle state. Deps: roki-http-server.
- [ ] roki-http-events — GET event stream / since-cursor; backed by ring buffer. Deps: roki-http-server, roki-obs-ring-buffer.
- [ ] roki-http-escalations — GET escalation queue entries. Deps: roki-http-server, roki-engine-failure-cycle.

### Wave 8 — TUI

- [ ] roki-tui-foundation — ratatui app shell, view router, key map; HTTP API client. Deps: roki-http-tickets.
- [ ] roki-tui-tickets-view — ticket list / cycle state. Deps: roki-tui-foundation.
- [ ] roki-tui-detail-view — ticket detail + recent phase output. Deps: roki-tui-foundation, roki-http-events.
- [ ] roki-tui-events-view — live event stream. Deps: roki-tui-foundation, roki-http-events.
- [ ] roki-tui-escalations-view — escalation queue read; operator dismiss = close Linear ticket. Deps: roki-tui-foundation, roki-http-escalations.

## Filling each spec

`/kiro-spec-init <name>` → `/kiro-spec-requirements <name>` → `/kiro-spec-design <name>` → `/kiro-spec-tasks <name>` → `/kiro-impl <name>`. Or `/kiro-spec-quick <name> --auto` to fast-track. `/kiro-spec-batch` dispatches a whole wave at once.
