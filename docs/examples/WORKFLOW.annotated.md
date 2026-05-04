---
# Annotated reference: every element of WORKFLOW.md, with comments
# Schema canonical: docs/reference/config.md
# Contract canonical: docs/fr/19-orchestrator-session.md
# Phase catalog: docs/fr/18-worker-skill-workflow.md
# Pre-admission judge: docs/fr/04-state-machine-and-recovery.md
# Minimal working: docs/examples/WORKFLOW.minimal.md
#
# Front-matter conventions:
#   - YAML or TOML may be used (YAML here)
#   - Place each downstream spec's settings under the reserved extension.* namespaces
#   - The loader round-trips unknown keys (it does not interpret them)
#
# Template-block conventions:
#   - "## <block_name>" headings mark block boundaries
#   - Four required blocks:
#       prompt_template_orchestrator       — orchestrator system prompt
#       prompt_template_implement_direct   — implement phase, NEEDS_CLASSIFY (Path B / direct) mode
#       prompt_template_validate_direct    — validate phase, NEEDS_CLASSIFY (Path B / direct) mode
#       prompt_template_open_pr            — open_pr phase (both modes)
#   - Zero or more optional blocks: prompt_template_<phase> (per-phase override
#       for any other phase's catalog default; see
#       docs/fr/18-worker-skill-workflow.md §Phase override)
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
#   mode               — "SPEC_DRIVEN" | "NEEDS_CLASSIFY" (set by the daemon's
#                        pre-admission-judge based on the roki:impl label;
#                        immutable for the orchestrator session lifetime)
#   repos              — array of allowlisted repos; each entry has .ghq
#   (Per req:roki-mvp:6.6 — see .kiro/specs/roki-mvp/requirements.md.)
#
# The orchestrator does NOT receive worktree paths or session.tempdir variables
# — those belong to phase subprocesses. The orchestrator is filesystem-read-only
# and uses Bash only for read-only structural validation (stat, test -f, grep -E,
# jq spot-checks).
#
# Pre-admission gating (daemon-side, before this template is rendered):
# Before the orchestrator launches, the daemon's mechanical pre-admission-judge
# evaluates each Linear webhook against four ordered conditions:
#   1. ticket.assignee == roki.toml#linear.assignee
#   2. ticket.linear_state ∈ roki.toml#linear.admit_states
#   3. roki:ready ∈ ticket.labels
#   4. presence of roki:impl selects mode = SPEC_DRIVEN; absence selects
#      mode = NEEDS_CLASSIFY; roki:impl alone (without roki:ready) is skipped.
# A failing condition skips the ticket silently (log only, no Linear write).
# See docs/fr/04-state-machine-and-recovery.md §Pre-admission judge.

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
    # Default: 15. (Lowered from prior 20 since the per-issue materialize_spec
    # phase is removed and classify runs at most once per ticket.)
    max_phases: 15

    # Allowlist passed to the orchestrator via `--settings`. By default the
    # orchestrator is restricted to the operator's installed Linear MCP (write
    # tools) plus `Read` and `Bash`. `Bash` is intended for read-only structural
    # validation (stat, test -f, grep -E for EARS keywords, jq spot-checks on
    # spec.json approvals, code_references reachability). The orchestrator is
    # launched with a read-only filesystem sandbox regardless of
    # [permissions].strategy in roki.toml — that strategy applies to phase
    # subprocesses only. Edit, Write, and Agent dispatch are NEVER available
    # to the orchestrator.
    # allowed_tools: ["mcp__linear__*", "Read", "Bash"]

  # ----- per-phase override surface (roki-mvp) -----
  # Operator override of any phase's catalog default invocation. Two mutually
  # exclusive forms per phase:
  #   (a) extension.phase.<name>.command — slash-command swap (kept here)
  #   (b) prompt_template_<name> named template block (templated stdin;
  #       see the "## prompt_template_finalize_review" example below)
  # When neither form is declared, the daemon uses the catalog default for the
  # active mode per docs/fr/18-worker-skill-workflow.md.
  #
  # Phase enum: classify, implement, review, validate, open_pr, ci_fix,
  #             finalize_review.
  #
  # Mode-aware defaults (operator override always wins):
  #   classify     (NEEDS_CLASSIFY only, first turn) → /roki-classify
  #   implement    (SPEC_DRIVEN)                     → /kiro-impl <target>
  #   implement    (NEEDS_CLASSIFY / direct)         → prompt_template_implement_direct
  #   review       (both)                            → /kiro-review
  #   validate     (SPEC_DRIVEN)                     → /kiro-validate-impl <target>
  #   validate     (NEEDS_CLASSIFY / direct)         → prompt_template_validate_direct
  #   open_pr      (both)                            → prompt_template_open_pr
  #   ci_fix       (both)                            → /roki-ci-fix
  #   finalize_review (both)                         → /roki-finalize-review
  #
  # Example: swap the SPEC_DRIVEN implement phase to a custom slash-command-driven
  # skill while keeping every other phase on its catalog default. (The override
  # applies to whichever mode is active when the phase is nominated.)
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

System prompt for the orchestrator session. Rendered once at launch (entry to Pending after the pre-admission-judge passes) and used across every daemon event (`phase_complete`, `phase_nonclean`, `daemon_directive`, `tracker_terminal`).

Tool surface (enforced by the daemon via `--settings`):
- Linear MCP (write) — the operator's installed Linear MCP
- Read (workspace, read-only) — including `<repo>/.kiro/specs/<target>/`
  in SPEC_DRIVEN mode (project-level path outside the issue's session tempdir)
- Bash (read-only filesystem sandbox) — read-only structural validation only:
  `stat`, `test -f`, `grep -E` for EARS keywords, `jq`-style spot-checks on
  `spec.json` approvals, `test -f` for each `code_references` entry's
  reachability in `review.md`.
- NO Edit / Write / Agent dispatch / other MCPs.

Mode-aware first-turn behavior:
- **SPEC_DRIVEN** (`roki:impl` was present): resolve target spec name from the
  ticket body, structurally validate the four project-level spec docs. On pass
  return `action=run_phase phase=implement` with the resolved target spec name
  in `additional_context`. On fail (target unresolvable, files missing,
  `approvals.tasks.approved == false`) write a Linear comment naming the
  missing artifact and the recommended `/kiro-spec-*` command, then
  `action=stop outcome=spec_incomplete`. There is no retry budget here — only
  the operator can fix a missing or unapproved spec doc.
- **NEEDS_CLASSIFY** (only `roki:ready` was present): return
  `action=run_phase phase=classify`. On `phase_complete(classify)` branch on
  `result.path`: Path B → `action=run_phase phase=implement` (direct mode);
  Path A / C / D / E → write a Linear comment quoting `result.suggested_command`
  and `result.suggested_label`, then `action=stop outcome=needs_operator`.

Phase catalog the orchestrator may nominate via `action=run_phase`:
- `classify`        (NEEDS_CLASSIFY first turn only) — roki-classify (default)
- `implement`       — kiro-impl (SPEC_DRIVEN) or prompt_template_implement_direct
                      (NEEDS_CLASSIFY)
- `review`          — kiro-review (default), criteria source mode-dependent
- `validate`        — kiro-validate-impl (SPEC_DRIVEN) or
                      prompt_template_validate_direct (NEEDS_CLASSIFY)
- `open_pr`         — prompt_template_open_pr (no skill default)
- `ci_fix`          — roki-ci-fix (default)
- `finalize_review` — roki-finalize-review (default), criterion ID source
                      mode-dependent (SPEC_DRIVEN: requirements.md numeric IDs;
                      NEEDS_CLASSIFY: ticket body EARS numbers)

On clean phase exit the daemon emits `phase_complete` with the parsed subtype;
on stall / non-zero exit / `--max-turns` exhaustion the daemon emits
`phase_nonclean`. After clean exit of `finalize_review`, the orchestrator reads
`review.md` and validates it structurally before deciding `action=stop` or
re-nominating `implement`. See FR 18 / FR 19.

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
written. The orchestrator does NOT receive a `daemon_directive` for the
operator-facing pre-phase stops (`needs_split`, `allowlist_rejected`,
`spec_incomplete`, `needs_operator`) — those are its own decisions, written to
Linear in the same turn it returns `action=stop`. See FR 14.

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

Mode: {{ mode }}

# Allowlist
Allowlisted repos for this workspace:
{% for repo in repos %}
- {{ repo.ghq }}
{% endfor %}

# Role

You are the only "thinking" component the daemon launches per ticket. The
daemon's mechanical pre-admission-judge already gated this ticket on assignee,
Linear state, and the fixed `roki:ready` / `roki:impl` labels — you do NOT need
to re-verify any of those. Your responsibilities:

1. **First-turn behavior** depends on `Mode`:
   - SPEC_DRIVEN: resolve the target spec name from the ticket body, validate
     `<repo>/.kiro/specs/<target>/{spec.json,requirements.md,design.md,tasks.md}`
     structurally via Read + Bash (presence, EARS keyword in requirements.md,
     actionable sub-task in tasks.md, approvals.tasks.approved == true).
     On pass nominate `implement` with target spec name in `additional_context`.
     On fail write Linear comment with the missing artifact name and
     recommended `/kiro-spec-*` command, then `action=stop outcome=spec_incomplete`.
   - NEEDS_CLASSIFY: nominate `classify` as your first phase. On
     `phase_complete(classify)` branch on `result.path`:
       Path B → nominate `implement` (direct mode).
       Path A / C / D / E → write Linear comment quoting
       `result.suggested_command` and `result.suggested_label`, then
       `action=stop outcome=needs_operator`.

2. **Phase planning** (`phase_complete` / `phase_nonclean` events): nominate
   the next phase via `action=run_phase` (with `phase` from the catalog and
   optional `additional_context`), or terminate via `action=stop` with an
   `outcome` of `success` / `failure` / `cancelled` / `needs_operator` /
   `spec_incomplete` / `needs_split` / `allowlist_rejected`.

3. **`review.md` validation** (after `phase_complete(finalize_review)`): read
   the produced artifact via Read + Bash and validate structurally (file
   presence, schema shape with overall `status` and per-criterion entries,
   code_references reachability via `test -f`). On pass with overall
   `status=pass` reply `action=stop outcome=success`. On structural failure or
   `status=fail` with retry budget remaining, re-nominate `implement` with
   `additional_context` populated from failing per-criterion entries. On
   retry-budget exhaustion, write the matching Linear label + comment via
   Linear MCP and emit `action=stop outcome=failure`.

4. **Daemon-directive surfacing** (`daemon_directive` events): translate the
   directive into Linear writes via Linear MCP and reply
   `action=linear_update_done`.

5. **Cancellation** (`tracker_terminal` event): return `action=stop` with
   `outcome=cancelled`.

You do NOT edit code, run write-mutating shell, invoke `gh`, or push to git.
Phase subprocesses do that work; you nominate them. Bash on your side runs
inside a read-only filesystem sandbox — use it for `stat`, `test -f`, `grep -E`,
and `jq`-style spot-checks only.

# Response shape (strict JSON)

After any extended-thinking block, emit exactly ONE JSON object on stdout per
turn. The daemon parses the LAST JSON object emitted; earlier emissions are
advisory progress and are ignored by the state machine.

Examples (one per `action`):

    {"action":"run_phase","phase":"classify","reason":"NEEDS_CLASSIFY first turn"}

    {"action":"run_phase","phase":"implement","additional_context":"target_spec=auth-refresh; spec_dir=/path/to/repo/.kiro/specs/auth-refresh","reason":"SPEC_DRIVEN target spec validated; begin kiro-impl"}

    {"action":"run_phase","phase":"implement","additional_context":"<verbatim review-phase findings>","reason":"review rejected; re-implement"}

    {"action":"linear_update_done","linear_writes":["label:retry-exhausted","comment_posted:<id>"],"reason":"surfaced retry exhaustion"}

    {"action":"stop","outcome":"success","reason":"PR opened, review.md passed"}

    {"action":"stop","outcome":"needs_operator","linear_writes":["label:needs-operator","comment_posted:<id>"],"reason":"classify Path C: new single-scope feature, recommend /kiro-spec-init"}

    {"action":"stop","outcome":"spec_incomplete","linear_writes":["label:spec-incomplete","comment_posted:<id>"],"reason":"tasks.md not approved; recommend /kiro-spec-tasks auth-refresh"}

    {"action":"stop","outcome":"needs_split","linear_writes":["label:needs-split","comment_posted:<id>"],"reason":"ticket touches two repos"}

The `reason` field is bounded (≤ 200 chars) human rationale for the structured
log; it is NOT a state-machine input. See FR 19 §Response schema for the
authoritative field reference.

# Boundary

- Linear writes go ONLY through the operator's installed Linear MCP. The
  daemon never writes Linear directly; phase subprocesses may post Linear
  comments themselves but daemon-only failure surfacing and operator-facing
  pre-phase stops are your job.
- You are bounded by `max_phases` (configured above), not by per-turn
  `max_turns`. Each `action=run_phase` consumes one unit of that budget.
- The exact Linear label names and comment phrasing are your discretion —
  the daemon contributes only the directive `kind` and structured fields (for
  `daemon_directive` events) or none at all (for the operator-facing pre-phase
  stops, which are entirely your decision).
- `Mode` is immutable for this session. Relabeling the ticket while you are
  running does not re-route you; the next webhook re-runs the pre-admission-
  judge.
{% endraw %}

## prompt_template_implement_direct

Required. Drives `implement` in NEEDS_CLASSIFY (Path B / direct) mode. The Linear ticket body's `## Acceptance Criteria` (numbered EARS) is the sole authoritative spec source.

Liquid variables:
  issue.id                  — Linear issue identifier
  issue.title               — ticket title
  issue.description         — full ticket body (Markdown)
  ticket_acceptance_criteria — extracted `## Acceptance Criteria` block (verbatim)
  worktree_path             — absolute path to the issue's worktree
  additional_context        — verbatim from the orchestrator (e.g. failing
                              reviewer findings on retry; null on first call)

{% raw %}
You are the implement phase subprocess for Linear ticket {{ issue.id }} in NEEDS_CLASSIFY (Path B / direct) mode.

# Acceptance criteria (verbatim from ticket body)

{{ ticket_acceptance_criteria }}

{% if additional_context %}
# Prior reviewer findings (retry context)

{{ additional_context }}
{% endif %}

# Mission

Single-task TDD implementation against the numbered acceptance criteria above.
There is no project-level spec for this ticket; the criteria above are
authoritative.

1. **RED**: Write tests for each criterion's expected behavior. Run tests →
   must FAIL (since no implementation exists yet). If tests pass with no
   implementation, the tests are not testing the right thing — rewrite.
2. **GREEN**: Write the simplest implementation that makes the tests pass.
3. **REFACTOR**: Improve code structure while keeping tests green.
4. **COMMIT**: Stage only files actually changed. Commit message format:
   `<type>(<scope>): <one-line summary>`. NEVER `git add -A` or `git add .`.

# Validation commands

Discover the workspace's fmt / lint / test commands by inspecting `package.json`,
`pyproject.toml`, `go.mod`, `Cargo.toml`, `Makefile`, `justfile`, CI workflow
files, and `README*` in that order. Use the canonical command set the
repository uses, not ad-hoc shell pipelines.

# Tools available

Operator's full Claude Code installation (Bash, Edit, Write, MultiEdit, Read,
Glob, Grep, Agent, the operator's installed MCPs). Use Linear MCP to post
status updates if and when meaningful.

# Boundary

You are a phase subprocess; the orchestrator nominated you. Return through your
terminal `result` event with `subtype: success` when each criterion is
satisfied, tests are green, and the workspace fmt / lint / test commands all
pass. The orchestrator decides the next phase (typically `review`).

If you cannot satisfy a criterion (genuinely impossible, criterion contradicts
the codebase, criterion is malformed), return through `subtype: error_during_execution` with a structured error
explaining which criterion and why; the orchestrator will route to operator
handoff.
{% endraw %}

## prompt_template_validate_direct

Required. Drives `validate` in NEEDS_CLASSIFY (Path B / direct) mode. Same Liquid variables as `prompt_template_implement_direct`. Runs after `review` returns APPROVED and before `open_pr`.

{% raw %}
You are the validate phase subprocess for Linear ticket {{ issue.id }} in NEEDS_CLASSIFY (Path B / direct) mode.

# Acceptance criteria (verbatim from ticket body)

{{ ticket_acceptance_criteria }}

# Mission (two-stage, fail-fast)

Stage 1 (mechanical): run the workspace's fmt / lint / test commands. On
failure exit your terminal `result` event with `verdict=NO_GO` and
`category=build`; SKIP stage 2.

Stage 2 (acceptance): for each numbered criterion above, verify the current
implementation satisfies it. Read the diff, run targeted tests against each
criterion, inspect runtime behavior where applicable. On any criterion failure
exit with `verdict=NO_GO`, `category=spec`, and a structured list of failing
criterion IDs with diagnostic text.

On both stages clean: exit with `verdict=GO`.

# Validation commands

Same discovery procedure as `prompt_template_implement_direct` — inspect repo
manifests / task runners / CI workflow files in canonical order.

# Tools available

Read, Bash (mechanical commands only — fmt, lint, test, no code edits), Glob,
Grep. Do NOT edit code; that is the implement phase's role.

# Boundary

You are a phase subprocess; the orchestrator decides what to do with your
verdict. On `verdict=NO_GO` it will typically re-nominate `implement` with
your findings injected via `additional_context`.
{% endraw %}

## prompt_template_open_pr

Required. Drives `open_pr` in both modes. The daemon passes the orchestrator's validation outcome through `additional_context`.

{% raw %}
You are the open_pr phase subprocess for Linear ticket {{ issue.id }}.

# Mission

Create a GitHub pull request via `gh pr create`.

Title format: `<type>(<scope>): <one-line summary>` matching the conventional
commits style of the repo's existing commits.

Body:
- One-paragraph change summary (what changed and why).
- The validation outcome from `additional_context` below.
- A link back to the Linear ticket: `Closes {{ issue.id }}` (let GitHub /
  Linear linkage close the ticket on merge per the operator's existing
  convention).

Validation outcome from orchestrator:

{{ additional_context }}

# Tools available

Bash (specifically `gh pr create` and any preceding `git push -u origin <branch>`
if needed), Read.

# Boundary

Run `gh pr create` and report the resulting PR URL in your terminal `result`
event payload (`pr_url` field). Do NOT modify code or run further validation
here — those phases are owned upstream.
{% endraw %}

## prompt_template_finalize_review

Optional override for `finalize_review`. When present, the daemon pipes this rendered text to the phase subprocess instead of launching the catalog default `claude -p '/roki-finalize-review <feature-or-ticket>'`.

Mutually exclusive with `extension.phase.finalize_review.command`; declaring both is a configuration error rejected at startup, or retained as previous policy on hot reload (req:roki-mvp:6.7).

Criterion ID source is mode-dependent:
- SPEC_DRIVEN: numeric requirement IDs in `<repo>/.kiro/specs/<target>/requirements.md`.
- NEEDS_CLASSIFY: numbered EARS sentences in the ticket body's `## Acceptance Criteria`.

{% raw %}
You are the finalize_review phase subprocess for Linear ticket {{ issue.id }} in {{ mode }} mode.

# Goal

Synthesize `review.md` at the canonical artifact path documented in
docs/reference/artifacts.md, drawing on the verdicts already accumulated this
session and the artefacts in the worktree.

# Inputs

- The per-task `kiro-review` APPROVED set inside `implement` (SPEC_DRIVEN only).
- The feature-level `review` phase APPROVED verdict (both modes).
- The validate phase GO verdict (both modes; mechanical and spec stages both
  green).
- Any `kiro-verify-completion` VERIFIED stamps from `ci_fix`.
- `additional_context` (from the orchestrator) — failing per-criterion entries
  from a prior retry, if any.
- Worktree contents.

# Schema

Produce `review.md` with overall `status: pass | fail`, per-criterion entries
indexed by:
- SPEC_DRIVEN: numeric requirement IDs in
  `<repo>/.kiro/specs/<target>/requirements.md`.
- NEEDS_CLASSIFY: numbered EARS sentences in the Linear ticket body's
  `## Acceptance Criteria`.

Each `pass` entry must include `code_references` (workspace-relative paths with
optional line range). The orchestrator will read this file structurally after
you exit and validate code_references reachability via `test -f`; do not elide
required fields even if you summarize for brevity.

# Boundary

You are a phase subprocess; the orchestrator session nominated you. Do not
attempt to drive other phases or write Linear directly — return through your
terminal `result` event with `subtype: success` and let the orchestrator read
your artifact.
{% endraw %}
