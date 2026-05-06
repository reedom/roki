# roki: Config-Driven Engine Pivot — Design

Status: brainstorming output, awaiting user review
Scope: rewrite of `docs/fr/` only; `.kiro/specs/roki-mvp/*` and `docs/reference/*` deferred to a later phase.

## 1. Background and pivot

Today, roki ships a hard-coded workflow: an operator labels a Linear ticket, the daemon runs a fixed pre-admission judge, mode tagging (`SPEC_DRIVEN` / `NEEDS_CLASSIFY`), a long-lived orchestrator session, a fixed phase catalog (`classify` / `implement` / `review` / `validate` / `open_pr` / `ci_fix` / `finalize_review`), a 5-state per-issue state machine, daemon-managed retry budgets, and daemon-driven Linear writes for failure surfacing. The daemon contains substantial domain logic.

This design pivots the daemon to a generic, config-driven engine. The daemon retains exactly four responsibilities:

1. Receive Linear webhooks, gate them through an admission filter, and detect ticket-property changes (status / labels / assignee) against an in-memory diff cache.
2. Match each diff against an operator-authored rule list (cleanup / rule / on_failure) and run a cycle composed of three phases: pre, run, post.
3. Create per-ticket worktrees and session tempdirs, capture each phase's stdout / stderr to disk, parse a structured directive from each phase's last JSON object on stdout, and loop or terminate per the directive.
4. Expose log / event / repo-path access through small CLIs (`roki log`, `roki events`, `roki repo`) and an HTTP API.

All workflow knowledge — what to do when a ticket gets `roki:ready`, what counts as terminal, how to write a Linear comment, when to retry, which model to use — moves into operator-authored TOML and Markdown files. The daemon does not know about kiro skills, claude vs codex, or any specific phase semantics.

## 2. Engine model

### 2.1 Three layers

1. **Webhook intake + admission gate.** A Linear webhook arrives. The admission filter checks `assignee` and resolves the ticket's repo via the first matching `[[admission.repos]]` entry. Tickets that fail admission are silently evicted (logged but not surfaced to Linear).
2. **Rule dispatch + cycle engine.** The daemon updates an in-memory cache `(ticket_id) → {status, labels, assignee, repo, workflow_path}`. If status / labels / assignee changed since the previous webhook, the daemon evaluates lists in priority order: `[[cleanup]]` first-match, then `[[rule]]` first-match. The first matching entry starts a cycle.
3. **Phase execution + log capture.** Each cycle runs pre → run → post in a loop. Each phase is a subprocess (a long-lived AI session reused within the cycle, or a one-shot command). The daemon captures stdout/stderr to files and parses the last JSON object on stdout as a structured directive.

### 2.2 What is removed

- `SPEC_DRIVEN` / `NEEDS_CLASSIFY` modes.
- Phase catalog (`classify`, `implement`, `review`, `validate`, `open_pr`, `ci_fix`, `finalize_review`) as daemon-known concepts.
- The orchestrator session as a long-lived per-ticket "thinking" component. The "long-lived AI" survives, but only across one cycle's pre / post invocations, not across cycles.
- 5-state daemon state machine and 12-variant `Inactive.reason` discriminator.
- Daemon-side retry budget and exponential backoff. Operators encode retry via post directives.
- Daemon-driven Linear writes (no `daemon_directive → Linear MCP` path). Operators write to Linear from inside their pre/run/post invocations using whichever tools they install.
- The `materialize_spec` and pre-admission-judge LLM concepts (already removed in current FRs; this design confirms they stay gone).

### 2.3 What survives

- Webhook intake.
- Admission filter (assignee + repo allowlist) — but expressed in WORKFLOW.toml, not roki.toml.
- In-memory diff cache (no persistent DB).
- Cycle / phase subprocess lifecycle, with stall detection and SIGTERM fallback.
- Per-ticket worktree and session tempdir lifecycle.
- TUI escalation queue for failure cases that have no operator handler.
- Structured event log via the tracing crate.
- HTTP API and TUI for observability.
- Hot reload of WORKFLOW.toml + workflow/*.md.

## 3. Configuration surface

Three files:

| File | Hot-reload | Role |
|---|---|---|
| `roki.toml` | no (restart) | Secrets, network, AI default CLIs, log destination, paths. |
| `WORKFLOW.toml` | yes | Admission filter and rule / cleanup / on_failure entries. |
| `workflow/*.md` | yes | Phase prompt / cmd bodies (frontmatter + Liquid template body), referenced from WORKFLOW.toml. |

### 3.1 `roki.toml`

```toml
[linear]
token = "lin_api_..."
webhook_secret = "..."

[network]
bind = "127.0.0.1"
port = 8080

[default.ai.session]
cli = "claude --input-format stream-json --output-format stream-json --model claude-opus-4-7"
stall_seconds = 600

[default.ai.command]
cli = "claude -p '{{ prompt }}' --output-format stream-json --max-turns 100"
stall_seconds = 300

[paths]
workflow = "./WORKFLOW.toml"
session_root = "~/.cache/roki/sessions"
worktree_root = "~/wt"

[engine]
max_iterations = 10

[log]
destination = "stdout"   # "stdout" | "file" | "both"
file_path = "/var/log/roki/daemon.jsonl"
level = "info"
ring_size = 1000
```

Fields are illustrative; the canonical schema lives in `docs/reference/config.md` after that file is updated in a later phase.

### 3.2 `WORKFLOW.toml`

```toml
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"
when.labels.has_any = ["repo:bar"]
workflow = "repos/bar.toml"      # optional per-repo TOML

[[admission.repos]]
ghq = "github.com/foo/baz"
when.title.regex = "^\\[baz\\]"

[[admission.repos]]
ghq = "github.com/foo/qux"
# no `when` → fallback for tickets that match no other repo entry

[[rule]]
when.status = "Todo"
when.labels.has_all = ["roki:ready"]
pre.path = "workflow/01-judge.md"
run.path = "workflow/01-impl.md"
post.path = "workflow/01-verdict.md"

[[cleanup]]
when.status.in = ["Done", "Cancelled"]
pre.prompt = "Final ceremony comment via Linear MCP. Output {directive: 'run'}"
run.cmd = "claude -p 'post final summary' --output-format stream-json --max-turns 5"
post.prompt = "Output {directive: 'end'}"

[[cleanup]]
when.labels.has_none = ["roki:ready"]
# all phases omitted → daemon performs immediate worktree + session_tempdir delete

[[on_failure]]
when.kind.in = ["unparseable", "schema_drift"]
run.cmd = "claude -p '/post-mortem {{ failure.failed_cycle_id }}'"
post.prompt = "Output {directive: 'end'}"
```

### 3.3 Per-repo TOML (`repos/bar.toml`)

When `[[admission.repos]] workflow = "..."` is set, that file replaces the top-level `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` for that repo. Top-level admission stays in WORKFLOW.toml. The two rule sets do not merge.

Operators that prefer a single-file layout can keep everything in WORKFLOW.toml and use `when.repo` matchers on rules to dispatch by repo.

### 3.4 `workflow/*.md`

```yaml
---
session: session       # or "command" (default = "session")
cli: ""                # session: ignored. command: optional override; falls back to roki.toml [default.ai.command].cli
---
{Liquid body}
```

### 3.5 Condition vocabulary (MVP)

- Equality: `when.<field> = <scalar>`, `when.<field>.not = <scalar>`.
- Set membership: `when.<field>.in = [...]`.
- List membership: `when.labels.has_all = [...]`, `.has_any = [...]`, `.has_none = [...]`.
- String matchers (admission.repos title/body only): `.regex`, `.starts_with`, `.contains`.

All `when.*` keys within an entry AND together. OR is expressed by writing additional entries.

Recognized fields: `status`, `labels`, `assignee`, `repo` (rule-only; admission resolves it first), `title` and `body` (admission.repos only).

### 3.6 Phase specification

Each phase declares exactly one of `path` / `prompt` / `cmd`:

- `path = "<file>"` — file form. Frontmatter chooses `session` or `command`. Body is a Liquid template.
- `prompt = "<inline string>"` — inline session form. Always uses `default.ai.session.cli`.
- `cmd = "<inline string>"` — inline command form. Operator writes the full command line.

### 3.7 Hot reload

When WORKFLOW.toml or any workflow/*.md changes:

1. Reload + schema validate.
2. On success: apply to the next webhook. In-flight cycles continue with their pre-reload policy.
3. On failure: keep the previous policy and log the offending field.

Per-key invalidity inside `when.*` rejects the whole entry; the entry is treated as if it had not matched.

## 4. Cycle and phase lifecycle

### 4.1 Cycle kinds

| Kind | Triggered by | Auto-cleanup at end |
|---|---|---|
| `rule` | `[[rule]]` first-match | None |
| `cleanup` | `[[cleanup]]` first-match | Daemon deletes worktree + session_tempdir, evicts the ticket from the cache |
| `failure` | Daemon-detected internal failure during another cycle, with `[[on_failure]]` first-match | None |

Evaluation order on every diff: cleanup before rule. Failure cycles are spawned only when an in-flight cycle hits an internal failure.

### 4.2 Phase loop

```
cycle start
  ↓
[iteration N]
  pre → response.directive ∈ {run, end}
    end → cycle terminates (run/post skipped)
    run → run → post → response.directive ∈ {pre, run, end}
      pre  → goto [iteration N+1] pre
      run  → goto [iteration N+1] run (skips pre)
      end  → cycle terminates
```

All three phases are optional. A `[[cleanup]]` entry with no phases at all means "delete immediately, no cycle starts". Omitted pre defaults to `directive: "run"`. Omitted post defaults to `directive: "end"`.

### 4.3 Directive schema

Each pre / post emits exactly one terminal JSON object on stdout. The daemon parses the last JSON object on the phase's stdout per invocation:

```json
{
  "directive": "run" | "end" | "pre",
  "outcome": "<operator string>",
  "repo": "<github.com/foo/bar>",
  "...": "operator fields"
}
```

Legal directives:

- pre: `run` | `end`.
- post: `pre` | `run` | `end`.

Illegal directive value is a `schema_drift` failure.

### 4.4 Inter-phase data flow

The daemon retains the last completed iteration's payloads and exposes them to subsequent phases as Liquid template variables. Older iterations are not retained.

| Variable | Scope |
|---|---|
| `{{ ticket.id }}`, `{{ ticket.title }}`, `{{ ticket.body }}`, `{{ ticket.labels }}`, `{{ ticket.assignee }}`, `{{ ticket.status }}` | Current Linear state |
| `{{ repo.ghq }}` | Admission-resolved repo |
| `{{ cycle.id }}`, `{{ cycle.kind }}`, `{{ cycle.trigger }}`, `{{ cycle.iter }}` | Current cycle |
| `{{ pre.* }}` | Most recent pre response |
| `{{ post.* }}` | Most recent post response (visible in iter N+1) |
| `{{ run.exit_code }}`, `{{ run.terminal.* }}`, `{{ run.duration_seconds }}` | Most recent run terminal data |
| `{{ failure.kind }}`, `{{ failure.failed_cycle_id }}`, `{{ failure.phase }}`, `{{ failure.iter }}`, `{{ failure.exit_code }}`, `{{ failure.error_text }}` | Failure-handler cycles only |

Each variable is also injected as an environment variable (`ROKI_TICKET_ID`, `ROKI_CYCLE_ID`, `ROKI_CYCLE_KIND`, `ROKI_CYCLE_TRIGGER`, `ROKI_CYCLE_ITER`, `ROKI_FAILURE_KIND`, etc.) for shell-form phases.

### 4.5 Iteration cap and cooperative termination

`[engine].max_iterations` (default 10) caps a cycle's iteration count. When hit:

1. If the active phase is in a session, the daemon writes an `iteration_exhausted` directive to the session's stdin and waits for the AI to emit `directive: "end"` cooperatively.
2. If the AI does not exit within the session stall window, the daemon SIGTERMs the session and marks the cycle as `iter_exhausted` failure.
3. For command-form phases (one-shot), there is no cooperative path. The daemon ends the cycle and routes through `[[on_failure]] when.kind = "iter_exhausted"`.

### 4.6 Queue-mode preemption

A new webhook arriving while a ticket has an in-flight cycle:

- Updates the in-memory diff cache to the new state.
- Defers rule re-evaluation until the current cycle ends.
- After the cycle ends, the daemon re-evaluates against the latest cached state. The retained webhooks are not replayed individually; only the final state matters.

Admission-filter failure mid-cycle (assignee revoked, repo allowlist match lost) does not preempt. The in-flight cycle runs to natural end. After the cycle, the daemon evicts the ticket and deletes worktree + session_tempdir as orphan cleanup.

### 4.7 Stall detection

Each subprocess has a stall window (`roki.toml [default.ai.session].stall_seconds` for session phases; `[default.ai.command].stall_seconds` for command phases; frontmatter can override on a per-file basis). If stdout is silent for that duration, the daemon SIGTERMs the subprocess and routes through `[[on_failure]] when.kind = "stall"`.

## 5. Admission, repo resolution, worktree management

### 5.1 Admission flow

1. Webhook arrives.
2. Verify webhook signature against `[linear].webhook_secret`.
3. Apply assignee filter: `assignee == [admission].assignee` (with `"me"` resolving to the API token holder). Failure → silent eviction.
4. Evaluate `[[admission.repos]]` first-match. The matched entry's `ghq` becomes the ticket's repo; its optional `workflow` path becomes the rule set source. No match → silent eviction (logged as `repo_unresolvable`).
5. Update the in-memory cache for this ticket.

### 5.2 Worktree management

- session_tempdir is created at admission (the daemon needs a place to capture logs even before the first phase runs).
- Worktree is created lazily on the first `pre.directive = "run"` of the ticket's first cycle. Until then, only ghq base is available via `roki repo`.
- On `[[cleanup]]` cycle completion, on admission-filter eviction, and on cold-start orphan reconciliation, the daemon deletes both worktree and session_tempdir.

### 5.3 `roki repo` CLI

```bash
roki repo                          # admission-resolved repo path: worktree if present, else ghq base
roki repo github.com/foo/bar       # explicit repo: same logic
roki repo --auto-clone             # ghq get if ghq base does not exist
roki repo --worktree               # worktree required (exit 1 if not yet created)
```

Defaults read `ROKI_TICKET_ID` / `ROKI_REPO` from the environment.

### 5.4 Multi-repo tickets

One ticket → one repo by construction (admission resolves the first match). Multi-repo concerns are operator-side: a pre that detects the work spans two repos can return `directive: "end"` with `outcome: "needs_split"` and a Linear write executed inside the same pre.

## 6. Cleanup and failure handling

### 6.1 Cleanup priority

Every diff triggers `[[cleanup]]` evaluation before `[[rule]]`. Cleanup with all three phases omitted is shorthand for "delete immediately"; the daemon performs the cleanup directly without spawning a cycle.

### 6.2 Failure kinds

| Kind | Trigger |
|---|---|
| `process_crash` | Subprocess SIGSEGV or non-zero exit without a parseable terminal response |
| `unparseable` | Last JSON object on stdout failed to parse, or the `directive` field is missing |
| `schema_drift` | `directive` value is outside the legal set for the phase |
| `stall` | Stall window exceeded; daemon SIGTERMed the subprocess |
| `iter_exhausted` | `max_iterations` exceeded and the AI did not cooperate (or the phase was command-form) |
| `template_error` | Liquid render failure |

### 6.3 Failure cycle

When an internal failure is detected:

1. The originating cycle is marked aborted.
2. The daemon evaluates `[[on_failure]]` first-match against the failure kind (and optional phase scope).
3. On match: spawn a new failure-handler cycle with kind = `failure`, with `{{ failure.* }}` populated. The handler can read the failed cycle's logs via `roki log --cycle <failed_cycle_id> ...`.
4. No match: silent log + TUI escalation queue entry. Worktree retained for forensics.

A failure cycle that itself fails does not chain into another failure cycle. The default behavior (silent log + escalation entry) applies.

### 6.4 Escalation queue

In-memory ring of structured failure entries: `{cycle_id, ticket_id, kind, phase, timestamp, error_text}`. Cleared on daemon restart. Read-only via TUI / HTTP API; operator dismisses by closing the corresponding Linear ticket.

## 7. Cold start and restart recovery

A single flow handles both. On daemon process start:

1. Load roki.toml and WORKFLOW.toml. Validate. Refuse to start on validation failure.
2. Query Linear API for tickets matching admission (`assignee` filter; status filter derived from the union of `when.status` values across all `[[rule]]` and `[[cleanup]]` entries; pagination).
3. For each ticket: resolve repo via `[[admission.repos]]`, register in the cache, and evaluate cleanup/rule first-match. On match, start a cycle with `cycle.trigger = "cold_start"` (env `ROKI_CYCLE_TRIGGER=cold_start`).
4. Reconcile disk residue: enumerate session_tempdirs and worktrees. Anything not corresponding to a Linear-API-hit ticket is auto-deleted as orphan (logged with `reason=orphan`).

The trigger value can be extended later (`restart_recovery`, `manual`, etc.); MVP uses `cold_start` for both first launch and post-crash relaunch.

Concurrency: the daemon may run cycles for distinct tickets in parallel during cold start. Same-ticket queue ordering still applies. A future `[engine].max_concurrent_cycles` knob is left for after MVP.

## 8. Observability and CLIs

### 8.1 Three storage / access tiers

1. **Per-ticket subprocess capture** under `<session_root>/<ticket_id>/cycle-<uuid>/iter-<n>/{pre,run,post}.{stdout,stderr}` plus `pre.response.json`, `run.exit_code`, `run.terminal.json`, `post.response.json`. Read via `roki log`.
2. **Structured event log** via tracing crate. Destination configurable in `roki.toml [log]`. JSON Lines format. Read via `roki events`.
3. **In-memory ring buffer** for live event subscription via the HTTP API.

### 8.2 CLIs

| CLI | Purpose |
|---|---|
| `roki <daemon-subcmd>` | Daemon process control. |
| `roki log` | Per-ticket subprocess raw capture. Scope is the same ticket only. Supports `--iter N` (relative or absolute), `--phase pre|run|post`, `--stream stdout|stderr|response`, `--tail N`, `--bytes N`, `--list-iters`, `--meta`, and `--cycle <uuid>` for cross-cycle access within the ticket. Default arguments come from `ROKI_TICKET_ID` / `ROKI_CYCLE_ID` / `ROKI_CYCLE_ITER`. |
| `roki events` | Structured event stream. Default = HTTP API client (live tail or range). `--offline --file <path>` reads JSON Lines directly. Filters: `--since`, `--kind`, `--ticket`, `--cycle`. Output: JSON (default) or human. |
| `roki repo` | Path resolution. See §5.3. |

### 8.3 HTTP API

| Endpoint | Returns |
|---|---|
| `GET /api/tickets` | Cache snapshot: id, repo, status, labels, assignee, in_flight_cycle_id, last_event_at. |
| `GET /api/tickets/{id}` | One ticket. |
| `GET /api/tickets/{id}/cycles` | Cycle ids for the ticket. |
| `GET /api/tickets/{id}/cycles/{cycle_id}/iters/{n}/{phase}/{stream}` | Raw subprocess capture (HTTP wrapper around `roki log`). |
| `GET /api/events?since=<seq>&kind=...` | Structured event range. |
| `GET /api/escalations` | Escalation queue dump. |
| `GET /api/healthz` | Daemon health. |

WebSocket / SSE push is deferred. TUI polls.

### 8.4 TUI

- Ticket list (cache snapshot).
- Ticket detail (cycle history, iter breakdown, log tail).
- Live event view.
- Escalation queue.

The TUI does not display state-machine internals (there are only two now: cycling vs idle).

### 8.5 Structured event catalog (MVP)

| Event | When |
|---|---|
| `webhook_received` | Webhook arrives |
| `webhook_skipped` | Admission failed or no diff |
| `cycle_started` | Cycle begins |
| `phase_started` | Phase subprocess spawned |
| `phase_completed` | Phase clean exit |
| `phase_failed` | Phase failure (kind included) |
| `cycle_completed` | Cycle ends with terminal directive |
| `cycle_aborted` | Cycle aborted (failure or admission lost) |
| `escalation_added` | Escalation queue entry added |
| `worktree_created` / `worktree_deleted` | Worktree lifecycle |
| `cold_start_began` / `cold_start_completed` | Daemon startup |

Subprocess advisory output (claude stream-json thinking turns, etc.) is not parsed by the daemon; it is captured as raw stdout/stderr accessible through `roki log`.

## 9. `docs/fr/` packaging (Approach C)

### 9.1 Rewrite, number preserved

| File | Scope of change |
|---|---|
| `01-daemon-lifecycle.md` | Minor (drop `--debug` flag; keep `--config` and shutdown grace). |
| `02-configuration.md` | Full rewrite. New roki.toml + WORKFLOW.toml + workflow/*.md schema. |
| `03-linear-integration.md` | Webhook diff cache, admission.repos matchers, `me` resolution. |
| `04-state-machine-and-recovery.md` | Major shrink: 2-state model, cold-start = restart-recovery, orphan reconcile. |
| `06-worktree-and-session.md` | Lazy worktree create, cleanup auto-delete, orphan auto-delete, `roki repo` access. |
| `07-worker-execution.md` | Full rewrite: pre/run/post subprocess lifecycle, capture, parsing, stall, SIGTERM. Permission strategy collapses into pass-through. |
| `11-agent-tool-boundary.md` | Minor. roki passes the cli line through; tool boundary is whatever claude-code (or any other engine) enforces. |
| `12-extension-surface.md` | Major shrink. Most surfaces collapse into config + template variables + HTTP event subscription. Candidate for outright removal — to be decided during the rewrite itself. |
| `13-observability-logs.md` | Three-tier model: structured event log, per-ticket capture, ring buffer. `roki log` / `roki events`. |
| `14-operator-notifications.md` | No daemon-driven Linear writes. Daemon surfaces failures via TUI escalation queue + structured event log only. Operator writes Linear feedback inside `[[on_failure]]` cycles. |
| `15-http-api.md` | Endpoint set above. State-machine-detail endpoints removed. |
| `16-roki-tui.md` | Four-view layout (tickets / detail / events / escalations). |
| `17-doc-cross-references.md` | Minor. |

### 9.2 New files

| File | Role |
|---|---|
| `20-rule-and-cycle-engine.md` | Engine semantics: cycle kinds, evaluation order, phase loop, directive schema, template variables, env vars, queue mode, max_iterations, stall, failure kinds. |
| `21-log-access.md` | `roki log` / `roki events` / `roki repo` CLI specifications and storage abstraction. |

### 9.3 Removed files

| File | Reason |
|---|---|
| `18-worker-skill-workflow.md` | Phase catalog concept removed. |
| `19-orchestrator-session.md` | Long-lived per-ticket orchestrator session removed. |

### 9.4 Cross-reference impact

- `docs/fr/index.md` regenerates via `roki-doctools index`.
- `refs:` graph: every `related: fr:18-...` / `fr:19-...` becomes dangling. Each rewriting pass clears them as it touches the relevant file. The validate hook reports any leftovers.
- `docs/reference/{config,extension-surface,log-events,artifacts,cli}.md`, `.kiro/specs/roki-mvp/{requirements,design,tasks}.md`, `.kiro/steering/roadmap.md` — out of scope for this phase. They will lag the FR rewrite; the validator will surface stale cross-references.

## 10. Deferred decisions

These came up but are explicitly out of scope for MVP:

- `include = [...]` directive in WORKFLOW.toml or per-repo TOML (yagni until needed).
- `[engine].max_concurrent_cycles` for parallelism throttling.
- Distinguishing `restart_recovery` from `cold_start` in `cycle.trigger`.
- WebSocket / SSE push for the events API.
- Persistent storage backend for `roki log` and `roki events`.
- A roki-side helper for Linear writes (operators use whatever MCP / CLI / HTTP they have).
