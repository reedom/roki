# FR Index — feature × spec matrix

A list of FR pages by feature, with the kiro spec requirements that each one corresponds to.

## Features

| # | Feature | Summary | Primary spec |
|---|---|---|---|
| 01 | [daemon-lifecycle](01-daemon-lifecycle.md) | `roki run` startup/shutdown, dependency CLI checks, SIGINT/SIGTERM | roki-mvp |
| 02 | [configuration](02-configuration.md) | All `roki.toml` and `WORKFLOW.md` namespaces lined up in one place | roki-mvp / spec-gate / review-gate / observability / distill-postmerge |
| 03 | [linear-integration](03-linear-integration.md) | Webhook + polling, admission filter, deduplication | roki-mvp |
| 04 | [state-machine-and-recovery](04-state-machine-and-recovery.md) | Per-issue states, transitions, restart recovery, vetoable hooks | roki-mvp (downstream subscribers are described on their own per-feature pages) |
| 05 | [setup-judge](05-setup-judge.md) | Pre-flight one-shot `claude` to classify the repo | roki-mvp |
| 06 | [worktree-and-session](06-worktree-and-session.md) | Materialize / clean up per-issue worktrees + session tempdirs | roki-mvp |
| 07 | [worker-execution](07-worker-execution.md) | Bounded `claude` subprocess + permission strategy + retry | roki-mvp |
| 08 | [pre-implementation-gate](08-pre-implementation-gate.md) | Gate `Queued -> Active` and require `requirements.md` | roki-spec-gate |
| 09 | [pre-pr-gate](09-pre-pr-gate.md) | Gate `AwaitingReview -> TerminalSuccess` and require `review.md` | roki-review-gate |
| 10 | [post-merge-distill](10-post-merge-distill.md) | Post-merge artifact sweep (delete / archive / distill) | roki-distill-postmerge |
| 11 | [agent-tool-boundary](11-agent-tool-boundary.md) | Principle that the daemon does not proxy agent-side tools, plus the per-gate status tools | roki-mvp / spec-gate / review-gate |
| 12 | [extension-surface](12-extension-surface.md) | Traits, hooks, and contracts that downstream specs depend on | roki-mvp (consumers are described in each gate / observability / distill page) |
| 13 | [observability-logs](13-observability-logs.md) | Structured logging, debug capture, stderr surfacing (cross-cutting across all specs) | roki-mvp / spec-gate / review-gate / distill-postmerge |
| 14 | [operator-notifications](14-operator-notifications.md) | Slack notifications for daemon-only failures | roki-mvp |
| 15 | [http-api](15-http-api.md) | `GET /state`, `GET /<issue>`, `POST /refresh` + sanitization | roki-observability |
| 16 | [roki-tui](16-roki-tui.md) | Ratatui TUI binary, escalation ack, refresh action | roki-observability |

## Spec → feature reverse lookup

### roki-mvp

01 / 02 / 03 / 04 / 05 / 06 / 07 / 11 / 12 / 13 / 14

### roki-spec-gate

02 (`extension.gates.spec.*`) / 04 (vetoable subscriber) / 08 (main feature) / 11 (`kiro_spec_status` tool) / 13 (gate logs)

### roki-review-gate

02 (`extension.gates.review.*`) / 04 (vetoable subscriber) / 09 (main feature) / 11 (`kiro_review_status` tool) / 12 (`additional_context` consumption) / 13 (gate logs)

### roki-observability

02 (`extension.server.*`) / 04 (via `OrchestratorRead`) / 12 (`TrackerRefresh` consumption) / 15 (main feature) / 16 (TUI)

### roki-distill-postmerge

02 (`extension.distill.*`) / 04 (pre-cleanup vetoable hook) / 06 (shared path safety) / 10 (main feature) / 13 (distill logs)
