---
# Annotated reference: every element of WORKFLOW.md, with comments
# Schema canonical: docs/reference/config.md
# Minimal working: docs/examples/WORKFLOW.minimal.md
#
# Front-matter conventions:
#   - YAML or TOML may be used (YAML here)
#   - Place each downstream spec's settings under the reserved extension.* namespaces
#   - The loader round-trips unknown keys (it does not interpret them)
#
# Template-block conventions:
#   - "## <block_name>" headings mark block boundaries
#   - Block names are fixed by roki-mvp:
#       prompt_template_setup  : prompt for the setup judge
#       prompt_template_worker : prompt for the main worker
#   - Wrap a region with {% raw %} ... {% endraw %} to escape Liquid
#   - Available Liquid variables (the main ones):
#       issue.id, issue.title, issue.description, issue.labels
#       repos          (setup judge only; array of allowlisted repos)
#       worktrees      (worker only; array of worktrees the judge validated;
#                       each entry is { path, repo, branch })
#       session.tempdir (worker only; path of the per-issue session tempdir)

extension:
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
                                 # re-launches the worker with the failing-criterion payload
                                 # in additional_context. No per-attempt timeout: the worker
                                 # produces review.md inside its own max_turns budget.

  # ----- HTTP API server (roki-observability) -----
  # Without a port specified the API does not start (default off)
  server:
    port: 7777
    bind: "127.0.0.1"           # default; non-loopback emits a warn log at startup
    min_refresh_interval_seconds: 30
    max_event_log_per_issue: 100

  # ----- linear-updater subagent (roki-mvp) -----
  # Used to send daemon-only failure events (stall, retry exhaustion,
  # multi-repo rejection, fs poison, etc.) to Linear via the operator's
  # installed Linear MCP. The daemon never writes Linear directly.
  linear_updater:
    timeout_ms: 60000              # per-invocation timeout
    # model: "claude-haiku-..."    # defaults to the same small model as the setup judge
    # allowed_tools: [...]         # optional restriction on Linear MCP tool names
---

## prompt_template_setup

Prompt block for the setup judge subprocess.
- Launched in a read-only sandbox + elicitations rejected (operator override is not honored)
- The stdout output is parsed as structured JSON

{% raw %}
You are the roki setup judge for Linear ticket {{ issue.id }}.

Title: {{ issue.title }}

Description:
{{ issue.description }}

Labels: {{ issue.labels | join: ", " }}

Available repos in the operator's allowlist:
{% for repo in repos %}
- {{ repo.ghq }}
{% endfor %}

Decide whether this ticket requires action against one or more of these repos.
Output a single-line JSON object matching exactly one of:

    {"action": "act", "repos": ["github.com/org/repo-a", ...]}
    {"action": "noop"}

Rules:
- "act" MUST list exactly one repo from the allowlist above. If two or more,
  the daemon routes the ticket to Inactive(reason=needs_split) and the
  linear-updater posts a Linear comment asking the operator to split it.
  If the listed repo is not in the allowlist, the daemon routes to
  Inactive(reason=allowlist_rejected).
- "noop" skips worker dispatch entirely; choose this for tickets that are
  not implementation work (e.g. discussion-only, duplicates).
- Do not output any text before or after the JSON object.
{% endraw %}

## prompt_template_worker

Prompt block for the main worker subprocess.
- The permission strategy is determined by `[permissions].strategy` in roki.toml
- The worktree paths validated by the judge land in the `worktrees` variable
- Additional context from the gates (spec / review) is automatically appended at
  the end of the prompt as a machine-extractable `additional_context` region
  (no need to write anything in this template; the daemon forwards it)

{% raw %}
You are implementing Linear ticket {{ issue.id }}.

# Ticket
Title: {{ issue.title }}

Description:
{{ issue.description }}

Labels: {{ issue.labels | join: ", " }}

# Workspace
{% for wt in worktrees %}
- Worktree: {{ wt.path }}
  - repo: {{ wt.repo }}
  - branch: {{ wt.branch }}
{% endfor %}
Session tempdir: {{ session.tempdir }}

# Workflow
Use the kiro skill set (auto-invoked by description) to drive this ticket
end-to-end. By the time this prompt runs, roki-spec-gate has already validated
that `.kiro/specs/{{ issue.id }}/requirements.md` exists with EARS-shaped
acceptance criteria — your first task is implementation, not spec creation.

1. **Implementation**: invoke the `kiro-impl` skill against
   `.kiro/specs/{{ issue.id }}/`. It dispatches an implementer subagent per
   task with a per-task `kiro-review` reviewer. Commit / branch operations go
   through git via Bash.

2. **Feature-level validation**: invoke `kiro-validate-impl` to catch
   cross-task issues that per-task review cannot see. Use
   `kiro-verify-completion` (claim type `TEST_OR_BUILD`) before claiming any
   build / test passes.

3. **Open PR**: open the PR via the gh CLI. Linear comments / labels go
   through the operator's Linear MCP.

4. **CI fix loop**: poll GitHub Actions; on red, use `kiro-debug` to
   root-cause + propose a fix, push, and re-poll, up to a small budget.

5. **Review artifact**: synthesize `.kiro/specs/{{ issue.id }}/review.md`
   from the verdicts accumulated above (per-task `kiro-review` approvals,
   `kiro-validate-impl` GO, verify-cmd outcome). Each EARS criterion gets one
   entry with `code_evidence` + `test_evidence`. roki-review-gate validates
   this artifact structurally on clean exit. If failing findings come back,
   the worker re-launches with `additional_context` carrying the failures.

# Boundary
- The daemon does not proxy agent-side tools. Your tool surface is exactly
  the one in your local Claude Code installation (Bash + the MCP servers
  you have installed) as-is.
- Linear writes go only through the operator's Linear MCP integration.
- The daemon does not write Linear / GitHub / code. Everything is your
  responsibility.
{% endraw %}

## prompt_template_linear_updater

Prompt block for the linear-updater subagent.
- Launched as a one-shot bounded `claude` subprocess on daemon-only failures
  (stall, retry exhaustion, multi-repo rejection, fs poison, etc.)
- Its only job is to translate a structured directive into Linear label
  additions and comments via the operator's installed Linear MCP
- The subprocess runs read-only on the workspace filesystem regardless of
  operator overrides; it must not edit code

{% raw %}
You are the roki linear-updater for Linear ticket {{ directive.issue_id }}.

Directive: {{ directive.kind }}
Fields:
{% for k, v in directive.fields %}
- {{ k }}: {{ v }}
{% endfor %}

Apply the appropriate Linear label addition(s) and post a structured comment
via the operator's installed Linear MCP that explains the directive in terms
useful to the operator.

Rules:
- Do not edit any code or workspace files.
- Do not invoke `gh` or push to git.
- Exit cleanly once the Linear writes have been applied.
- On Linear API error, log the failure and exit with a non-zero status; the
  daemon will retry once and otherwise log without crashing.
{% endraw %}
