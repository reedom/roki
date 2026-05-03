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
      timeout_ms: 300000         # upper bound on the review turn's duration
      max_attempts: 3            # review attempt cap (default: 3)

  # ----- HTTP API server (roki-observability) -----
  # Without a port specified the API does not start (default off)
  server:
    port: 7777
    bind: "127.0.0.1"           # default; non-loopback emits a warn log at startup
    min_refresh_interval_seconds: 30
    max_event_log_per_issue: 100

  # ----- post-merge distill (roki-distill-postmerge) -----
  distill:
    # path patterns to sweep (workspace-relative)
    paths:
      - ".kiro/specs/{{ issue.id }}/"
      - ".superpowers/specs/"
      - "plans/"
      - "notes/"
    # classification rules
    # - pattern: glob
    # - disposition: "delete" | "archive" | "distill"
    # - target: only for distill (the canonical extraction destination)
    routes:
      - { id: "scratch-delete",   pattern: "**/scratch.md",        disposition: "delete" }
      - { id: "design-archive",   pattern: "**/design.md",         disposition: "archive" }
      - { id: "decisions-distill", pattern: "**/decisions/*.md",   disposition: "distill", target: ".kiro/decisions/" }
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
- If "act", every listed repo MUST appear in the allowlist above
  (else the daemon will reject the findings and route the ticket to Skipped).
- If "noop", the daemon will skip worker dispatch entirely; choose this for
  tickets that are not implementation work (e.g. discussion-only, duplicates).
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
Use the kiro skill (auto-invoked by description) to drive this ticket end-to-end:

1. **Spec phase**: the kiro-discovery skill merges the Linear ticket and the
   project EARS docs and produces `.kiro/specs/{{ issue.id }}/requirements.md`.
   You cannot enter implementation until roki-spec-gate validates this artifact.

2. **Implementation**: implement against the EARS criteria in requirements.md.
   Commit / branch operations go through git via Bash; progress updates to
   Linear go through the operator's Linear MCP.

3. **Review phase**: the kiro-review skill produces
   `.kiro/specs/{{ issue.id }}/review.md` with per-criterion pass + code
   references. roki-review-gate validates it. If failing findings come back,
   re-implement via the fix loop.

4. **PR open**: open the PR via the gh CLI.

5. **Post-merge**: once you observe that Linear became Done, the
   roki-distill-postmerge sweep turn fires and classifies the artifacts under
   .kiro/specs/{{ issue.id }}/ as delete / archive / distill.

# Boundary
- The daemon does not proxy agent-side tools. Your tool surface is exactly
  the one in your local Claude Code installation (Bash + the MCP servers
  you have installed) as-is.
- Linear writes go only through the operator's Linear MCP integration.
- The daemon does not write Linear / GitHub / code. Everything is your
  responsibility.
{% endraw %}
