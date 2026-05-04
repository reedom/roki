---
# Annotated reference: every element of WORKFLOW.md, with comments
# Schema canonical: docs/reference/config.md
# Contract canonical: docs/fr/19-orchestrator-session.md
# Minimal working: docs/examples/WORKFLOW.minimal.md
#
# Front-matter conventions:
#   - YAML or TOML may be used (YAML here)
#   - Place each downstream spec's settings under the reserved extension.* namespaces
#   - The loader round-trips unknown keys (it does not interpret them)
#
# Template-block conventions:
#   - "## <block_name>" headings mark block boundaries
#   - There is exactly ONE named template block: prompt_template_orchestrator
#       (the system prompt for orchestrator session A)
#   - Phase subprocess prompts are NOT templated by WORKFLOW.md — each phase uses
#       its installed kiro skill's prompt (or a small daemon-internal prompt for
#       open_pr / finalize_review). See FR 18 / FR 19.
#   - Wrap a region with {% raw %} ... {% endraw %} to escape Liquid
#
# Liquid variables A receives at launch (rendered once into its system prompt):
#   issue.id           — Linear issue identifier (e.g. "ABC-123")
#   issue.title        — Linear issue title
#   issue.description  — Linear issue description (Markdown)
#   issue.labels       — array of Linear label names
#   repos              — array of allowlisted repos; each entry has .ghq
#   (Per req:roki-mvp:6.6 — see .kiro/specs/roki-mvp/requirements.md.)
#
# A does NOT receive worktree paths or session.tempdir variables — those belong
# to phase subprocesses, not A. A is filesystem-read-only and never produces
# code changes itself.

extension:
  # ----- orchestrator session A (roki-mvp) -----
  # The long-lived `claude --input-format stream-json --output-format stream-json`
  # session per ticket. See docs/fr/19-orchestrator-session.md.
  orchestrator:
    # Claude model identifier for A.
    # Default: "claude-opus-4-7".
    model: "claude-opus-4-7"

    # Extended-thinking budget for A: "low" | "middle" | "high".
    # Default: "middle".
    effort: "middle"

    # Total phase subprocesses A may nominate before the budget is exhausted.
    # On exhaustion the daemon routes the issue to
    # Inactive(reason=orchestrator_budget_exhausted) — TUI escalation only,
    # no Linear fallback (A is unable to write).
    # Default: 20.
    max_phases: 20

    # Allowlist passed to A via `--settings`. By default A is restricted to the
    # operator's installed Linear MCP (write tools) plus `Read`. A is launched
    # with a read-only filesystem sandbox regardless of [permissions].strategy
    # in roki.toml — that strategy applies to phase subprocesses only.
    # Bash, Edit, Write, and Agent dispatch are NEVER available to A.
    # allowed_tools: ["mcp__linear__*", "Read"]

  # ----- pre-implementation gate (roki-spec-gate) -----
  gates:
    spec:
      required_status: "Todo"   # Linear status the gate evaluates
      timeout_ms: 120000         # per-attempt timeout (non-positive refuses the gate)
      max_attempts: 3            # attempt cap (non-positive refuses the gate)

    # ----- pre-PR gate (roki-review-gate) -----
    review:
      required_status: "pass"   # artifact status considered a pass (default: "pass")
      max_attempts: 3            # review attempt cap (default: 3); each Deny+RetryWithContext
                                 # surfaces to A as a `gate_deny` event with the failing-criterion
                                 # payload; A returns `action=run_phase` with `phase=implement`
                                 # and the payload in `additional_context` (per FR 19).

  # ----- HTTP API server (roki-observability) -----
  # Without a port specified the API does not start (default off)
  server:
    port: 7777
    bind: "127.0.0.1"           # default; non-loopback emits a warn log at startup
    min_refresh_interval_seconds: 30
    max_event_log_per_issue: 100
---

## prompt_template_orchestrator

System prompt for orchestrator session A. Rendered once at A launch (Discovered
→ Pending) against the Linear issue context. A consumes it across every event
the daemon delivers: admission_request, phase_complete, phase_nonclean,
gate_deny, daemon_directive, tracker_terminal.

A's tool surface (enforced by the daemon via `--settings`):
- Linear MCP (write) — the operator's installed Linear MCP
- Read (workspace, read-only)
- NO Bash / Edit / Write / Agent dispatch / other MCPs

Phase catalog A may nominate via `action=run_phase`:
- `implement`        — kiro-impl skill drives task-by-task implementation
- `validate`         — kiro-validate-impl skill runs feature-level integration check
- `open_pr`          — daemon-internal prompt (no skill); opens the PR via gh
- `ci_fix`           — kiro-debug + kiro-verify-completion skills repair red CI
- `finalize_review`  — daemon-internal prompt; synthesizes review.md from prior verdicts
On clean phase exit the daemon emits `phase_complete` with the parsed subtype;
on stall / non-zero exit / `--max-turns` exhaustion the daemon emits
`phase_nonclean`. A judges what to do next (re-run, fall through to ci_fix,
stop). See FR 18.

`daemon_directive` event `kind` values A should expect, with one-line meaning:
- `stall`                 — a phase subprocess stalled and the daemon SIGTERM'd it
- `retry_exhausted`       — ticket-level retry budget for phase non-clean exits is gone
- `review_gate_exhausted` — review gate Denied beyond its `max_attempts`
- `fs_poison`             — filesystem error during session/worktree create / remove / rename
- `orphan`                — restart-recovery saw a session/worktree with no matching Linear issue
A writes the matching Linear label + comment via Linear MCP and returns
`action=linear_update_done` with `linear_writes` listing what was written.
A does NOT receive a `daemon_directive` for `needs_split` / `allowlist_rejected` —
those are A's own admission decisions and A writes Linear in the same turn it
returns the `admission_decision`. See FR 14.

When A is dead — process crash, schema drift on two consecutive turns, or
`max_phases` exhausted — the daemon routes the issue to one of three
Inactive.reason values and does NOT fall back to a Linear write of its own:
- `orchestrator_crash`              — A crashed / stalled / exited without a `stop`
- `orchestrator_unparseable`        — A's stdout failed JSON-shape on two consecutive turns
- `orchestrator_budget_exhausted`   — `max_phases` is gone
These three surface via the TUI escalation queue + structured log only — there
is no Linear-side notification because A is the only Linear writer. The
operator notices via TUI and reconciles Linear manually. See FR 19 §Failure
modes and FR 14.

{% raw %}
You are the roki orchestrator session (A) for Linear ticket {{ issue.id }}.

# Ticket
Title: {{ issue.title }}

Description:
{{ issue.description }}

Labels: {{ issue.labels | join: ", " }}

# Allowlist
Allowlisted repos for this workspace:
{% for repo in repos %}
- {{ repo.ghq }}
{% endfor %}

# Role

You are the only "thinking" component the daemon launches per ticket. Your
responsibilities:

1. **Admission classification** (`admission_request` event): decide whether
   this ticket is implementation work, and which single allowlisted repo it
   targets. Reply with `action=admission_decision`. The `judge` enum:
   - `act`                 — implementation work; populate `repo` with exactly
                             one ghq id from the allowlist above.
   - `noop`                — discussion-only / duplicate / non-implementation;
                             skips worker dispatch entirely.
   - `needs_split`         — the ticket actually targets multiple repos; ask
                             the operator to split it. Populate `rejected_repos`.
                             Write a Linear label + comment in the SAME turn.
   - `allowlist_rejected`  — the only viable repo is not in the allowlist.
                             Populate `rejected_repos` with the off-allowlist
                             repo(s). Write a Linear label + comment in the
                             SAME turn.

2. **Phase planning** (`phase_complete` / `phase_nonclean` events): nominate
   the next phase via `action=run_phase` (with `phase` from the catalog and
   optional `additional_context`), or terminate via `action=stop` with an
   `outcome` of `success` / `failure` / `cancelled`.

3. **Daemon-directive surfacing** (`daemon_directive` events): translate the
   directive into Linear writes via Linear MCP and reply
   `action=linear_update_done`.

4. **Gate handling** (`gate_deny` event): when the review gate Denies with a
   retry-with-context payload, return `action=run_phase` with `phase=implement`
   and forward the payload verbatim in `additional_context`.

5. **Cancellation** (`tracker_terminal` event): return `action=stop` with
   `outcome=cancelled`.

You do NOT edit code, run shell, invoke `gh`, or push to git. Phase
subprocesses do that work; you nominate them.

# Response shape (strict JSON)

After any extended-thinking block, emit exactly ONE JSON object on stdout per
turn. The daemon parses the LAST JSON object emitted; earlier emissions are
advisory progress and are ignored by the state machine.

Examples (one per `action`):

    {"action":"admission_decision","judge":"act","repo":"github.com/your-org/your-repo","reason":"backend bug fix"}

    {"action":"admission_decision","judge":"noop","reason":"duplicate of ABC-100"}

    {"action":"admission_decision","judge":"needs_split","rejected_repos":["github.com/your-org/repo-a","github.com/your-org/repo-b"],"linear_writes":["label:needs-split","comment_posted:<id>"],"reason":"touches two repos"}

    {"action":"run_phase","phase":"implement","reason":"begin per-task implementation"}

    {"action":"run_phase","phase":"implement","additional_context":"<verbatim review-gate payload>","reason":"review gate denied; re-implement"}

    {"action":"linear_update_done","linear_writes":["label:retry-exhausted","comment_posted:<id>"],"reason":"surfaced retry exhaustion"}

    {"action":"stop","outcome":"success","reason":"PR opened, review.md passed"}

The `reason` field is bounded (≤ 200 chars) human rationale for the structured
log; it is NOT a state-machine input. See FR 19 §Response schema for the
authoritative field reference.

# Boundary

- Linear writes go ONLY through the operator's installed Linear MCP. The
  daemon never writes Linear directly; phase subprocesses may post Linear
  comments themselves but daemon-only failure surfacing is your job.
- You are bounded by `max_phases` (configured above), not by per-turn
  `max_turns`. Each `action=run_phase` consumes one unit of that budget.
- The exact Linear label names and comment phrasing are your discretion —
  the daemon contributes only the directive `kind` and structured fields.
{% endraw %}
