---
# Annotated reference: every element of WORKFLOW.md, with comments
# Schema canonical: docs/reference/config.md
# Contract canonical: docs/fr/19-orchestrator-session.md
# Phase catalog: docs/fr/18-worker-skill-workflow.md
# Minimal working: docs/examples/WORKFLOW.minimal.md
#
# Front-matter conventions:
#   - YAML or TOML may be used (YAML here)
#   - Place each downstream spec's settings under the reserved extension.* namespaces
#   - The loader round-trips unknown keys (it does not interpret them)
#
# Template-block conventions:
#   - "## <block_name>" headings mark block boundaries
#   - One required block: prompt_template_orchestrator (system prompt for the orchestrator)
#   - Zero or more optional blocks: prompt_template_<phase> (per-phase override
#       for phase subprocesses; see docs/fr/18-worker-skill-workflow.md
#       §Phase override)
#   - prompt_template_<phase> is mutually exclusive per phase with
#       extension.phase.<phase>.command (slash-command override). Declaring both
#       for the same phase is a configuration error.
#   - Wrap a region with {% raw %} ... {% endraw %} to escape Liquid
#
# Liquid variables the orchestrator receives at launch (rendered once into its
# system prompt):
#   issue.id           — Linear issue identifier (e.g. "ABC-123")
#   issue.title        — Linear issue title
#   issue.description  — Linear issue description (Markdown)
#   issue.labels       — array of Linear label names
#   repos              — array of allowlisted repos; each entry has .ghq
#   (Per req:roki-mvp:6.6 — see .kiro/specs/roki-mvp/requirements.md.)
#
# The orchestrator does NOT receive worktree paths or session.tempdir variables
# — those belong to phase subprocesses. The orchestrator is filesystem-read-only
# and uses Bash only for read-only artifact validation (stat, test -f, grep -E).

extension:
  # ----- orchestrator session (roki-mvp) -----
  # The long-lived `claude --input-format stream-json --output-format stream-json`
  # session per ticket. See docs/fr/19-orchestrator-session.md.
  orchestrator:
    # Claude model identifier for the orchestrator.
    # Default: "claude-opus-4-7".
    model: "claude-opus-4-7"

    # Extended-thinking budget for the orchestrator: "low" | "middle" | "high".
    # Default: "middle".
    effort: "middle"

    # Total phase subprocesses the orchestrator may nominate before the budget
    # is exhausted. On exhaustion the daemon routes the issue to
    # Inactive(reason=orchestrator_budget_exhausted) — TUI escalation only,
    # no Linear fallback (the orchestrator is unable to write).
    # Default: 20.
    max_phases: 20

    # Allowlist passed to the orchestrator via `--settings`. By default the
    # orchestrator is restricted to the operator's installed Linear MCP (write
    # tools) plus `Read` and `Bash`. `Bash` is intended for read-only artifact
    # validation (stat, test -f, grep -E for EARS keywords, code_references
    # reachability). The orchestrator is launched with a read-only filesystem
    # sandbox regardless of [permissions].strategy in roki.toml — that strategy
    # applies to phase subprocesses only. Edit, Write, and Agent dispatch are
    # NEVER available to the orchestrator.
    # allowed_tools: ["mcp__linear__*", "Read", "Bash"]

  # ----- per-phase override surface (roki-mvp) -----
  # Operator override of any phase's catalog default invocation. Two mutually
  # exclusive forms per phase:
  #   (a) extension.phase.<name>.command — slash-command swap (kept here)
  #   (b) prompt_template_<name> named template block (templated stdin;
  #       see the "## prompt_template_finalize_review" example below)
  # When neither form is declared, the daemon uses the catalog default per
  # docs/fr/18-worker-skill-workflow.md.
  #
  # Phase enum: materialize_spec, implement, review, validate, open_pr,
  #             ci_fix, finalize_review.
  #
  # Example: swap the implement phase to a custom slash-command-driven skill
  # while keeping every other phase on its catalog default.
  # phase:
  #   implement:
  #     command: "/my-impl {{ issue.id }}"

  # ----- HTTP API server (roki-observability) -----
  # Without a port specified the API does not start (default off)
  server:
    port: 7777
    bind: "127.0.0.1"           # default; non-loopback emits a warn log at startup
    min_refresh_interval_seconds: 30
    max_event_log_per_issue: 100
---

## prompt_template_orchestrator

System prompt for the orchestrator session. Rendered once at orchestrator
launch (Discovered → Pending) against the Linear issue context. The orchestrator
consumes it across every event the daemon delivers: admission_request,
phase_complete, phase_nonclean, daemon_directive, tracker_terminal.

The orchestrator's tool surface (enforced by the daemon via `--settings`):
- Linear MCP (write) — the operator's installed Linear MCP
- Read (workspace, read-only)
- Bash (read-only filesystem sandbox) — read-only artifact validation only:
  `stat`, `test -f`, `grep -E` for EARS keywords in `requirements.md`,
  schema spot-checks on `review.md` per-criterion entries, `test -f` for each
  `code_references` entry's reachability.
- NO Edit / Write / Agent dispatch / other MCPs.

Phase catalog the orchestrator may nominate via `action=run_phase`:
- `materialize_spec`  — kiro-discovery (default) writes per-issue requirements.md
- `implement`         — kiro-impl (default) drives task-by-task implementation
- `review`            — kiro-review (default) runs feature-level adversarial code review
- `validate`          — kiro-validate-impl (default) runs two-stage check
                        (mechanical fmt/lint/test, then spec acceptance)
- `open_pr`           — daemon-internal prompt (no skill default); opens the PR via gh
- `ci_fix`            — roki-ci-fix (default) fetches CI logs via gh, categorizes
                        failures, delegates fix to kiro-debug, gates push via
                        kiro-verify-completion
- `finalize_review`   — roki-finalize-review (default) synthesizes review.md
On clean phase exit the daemon emits `phase_complete` with the parsed subtype;
on stall / non-zero exit / `--max-turns` exhaustion the daemon emits
`phase_nonclean`. After clean exit of `materialize_spec` and `finalize_review`,
the orchestrator reads the produced artifact (requirements.md / review.md) and
validates it structurally before deciding next phase. See FR 18 / FR 19.

Operators MAY override any phase's invocation via `extension.phase.<name>.command`
(slash-command swap, in front matter above) or `prompt_template_<name>` named
template block (templated stdin, in this body). Mutually exclusive per phase.

`daemon_directive` event `kind` values the orchestrator should expect, with
one-line meaning:
- `stall`           — a phase subprocess stalled and the daemon SIGTERM'd it
- `retry_exhausted` — ticket-level retry budget for phase non-clean exits is gone
- `fs_poison`       — filesystem error during session/worktree create / remove / rename
- `orphan`          — restart-recovery saw a session/worktree with no matching Linear issue
The orchestrator writes the matching Linear label + comment via Linear MCP and
returns `action=linear_update_done` with `linear_writes` listing what was
written. The orchestrator does NOT receive a `daemon_directive` for
`needs_split` / `allowlist_rejected` — those are its own admission decisions,
written to Linear in the same turn it returns the `admission_decision`.
See FR 14.

When the orchestrator is dead — process crash, schema drift on two consecutive
turns, or `max_phases` exhausted — the daemon routes the issue to one of three
Inactive.reason values and does NOT fall back to a Linear write of its own:
- `orchestrator_crash`              — orchestrator crashed / stalled / exited without a `stop`
- `orchestrator_unparseable`        — orchestrator stdout failed JSON-shape on two consecutive turns
- `orchestrator_budget_exhausted`   — `max_phases` is gone
These three surface via the TUI escalation queue + structured log only — there
is no Linear-side notification because the orchestrator is the only Linear
writer. The operator notices via TUI and reconciles Linear manually. See FR 19
§Failure modes and FR 14.

{% raw %}
You are the roki orchestrator session for Linear ticket {{ issue.id }}.

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

3. **Artifact validation** (after `phase_complete(materialize_spec)` and
   `phase_complete(finalize_review)`): read the produced artifact via Read +
   Bash and validate structurally (file presence, EARS keyword presence,
   schema, code_references reachability). On pass, proceed to the next phase
   (or `action=stop` for review.md). On structural failure with retry budget
   remaining, re-nominate the producing phase (or `implement` for review.md
   failures) with `additional_context` populated from the failure detail. On
   retry-budget exhaustion, write the matching Linear label + comment via
   Linear MCP and emit `action=stop` with `outcome=failure`.

4. **Daemon-directive surfacing** (`daemon_directive` events): translate the
   directive into Linear writes via Linear MCP and reply
   `action=linear_update_done`.

5. **Cancellation** (`tracker_terminal` event): return `action=stop` with
   `outcome=cancelled`.

You do NOT edit code, run write-mutating shell, invoke `gh`, or push to git.
Phase subprocesses do that work; you nominate them. Bash on your side runs
inside a read-only filesystem sandbox — use it for `stat`, `test -f`, and
`grep -E` only.

# Response shape (strict JSON)

After any extended-thinking block, emit exactly ONE JSON object on stdout per
turn. The daemon parses the LAST JSON object emitted; earlier emissions are
advisory progress and are ignored by the state machine.

Examples (one per `action`):

    {"action":"admission_decision","judge":"act","repo":"github.com/your-org/your-repo","reason":"backend bug fix"}

    {"action":"admission_decision","judge":"noop","reason":"duplicate of ABC-100"}

    {"action":"admission_decision","judge":"needs_split","rejected_repos":["github.com/your-org/repo-a","github.com/your-org/repo-b"],"linear_writes":["label:needs-split","comment_posted:<id>"],"reason":"touches two repos"}

    {"action":"run_phase","phase":"materialize_spec","reason":"materialize per-issue requirements.md"}

    {"action":"run_phase","phase":"implement","reason":"begin per-task implementation"}

    {"action":"run_phase","phase":"implement","additional_context":"<verbatim review-phase findings>","reason":"review rejected; re-implement"}

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

## prompt_template_finalize_review

Optional per-phase override for the `finalize_review` phase. When present, the
daemon writes the rendered text to the phase subprocess's stdin instead of
launching the catalog default `claude -p '/roki-finalize-review <feature>'`.

Mutually exclusive per phase with `extension.phase.finalize_review.command` —
declaring both is a configuration error rejected at startup or retained as the
previous policy at hot reload (per req:roki-mvp:6.7).

Same Liquid variables apply, plus the per-phase context envelope (issue id,
feature name, repo, additional_context from the orchestrator). The block below is a thin
example illustrating the override pattern; in practice operators are likely
to keep the default `roki-finalize-review` skill and only override the prose
or section emphasis when their workspace has unusual conventions.

{% raw %}
You are the finalize_review phase subprocess for Linear ticket {{ issue.id }}.

# Goal

Synthesize `review.md` at the canonical artifact path documented in
docs/reference/artifacts.md, drawing on the verdicts already accumulated this
session and the artefacts in the worktree.

# Inputs

- The per-task `kiro-review` APPROVED set inside `implement`.
- The feature-level `review` phase APPROVED verdict.
- The `kiro-validate-impl` GO verdict (with mechanical and spec stages both
  green).
- Any `kiro-verify-completion` VERIFIED stamps from `ci_fix`.
- `additional_context` (from the orchestrator) — failing per-criterion entries from a prior
  retry, if any.
- Worktree contents.

# Schema

Produce `review.md` with overall `status: pass | fail`, per-criterion entries
indexed by the numeric requirement IDs in the ticket's `requirements.md`,
and `code_references` (workspace-relative paths with optional line range) on
each `pass` entry. The orchestrator will read this file structurally after you
exit; do not elide required fields even if you summarize for brevity.

# Boundary

You are a phase subprocess; the orchestrator session nominated you. Do not
attempt to drive other phases or write Linear directly — return through your
terminal `result` event with `subtype: success` and let the orchestrator read
your artifact.
{% endraw %}
