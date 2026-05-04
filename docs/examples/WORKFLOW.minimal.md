---
# Minimal WORKFLOW.md — smallest configuration that boots
# Annotated reference for every element: docs/examples/WORKFLOW.annotated.md
# Schema canonical: docs/reference/config.md
---

## prompt_template_setup

{% raw %}
You are evaluating Linear ticket {{ issue.id }} ({{ issue.title }}).

Description:
{{ issue.description }}

Available repos in the operator's allowlist:
{% for repo in repos %}
- {{ repo.ghq }}
{% endfor %}

Decide whether this ticket requires action against one or more of these repos.
Output a single-line JSON object matching exactly one of:

    {"action": "act", "repos": ["github.com/org/repo-a", ...]}
    {"action": "noop"}

"act" MUST list exactly one repo from the allowlist above (multi-repo
tickets are rejected via linear-updater with a needs-split label).
{% endraw %}

## prompt_template_worker

{% raw %}
You are implementing Linear ticket {{ issue.id }}: {{ issue.title }}

Description:
{{ issue.description }}

Your worktree(s):
{% for wt in worktrees %}
- {{ wt.path }} (repo: {{ wt.repo }}, branch: {{ wt.branch }})
{% endfor %}

Use the kiro skill set (auto-invoked by description) to drive this ticket
end-to-end. By the time this prompt runs, roki-spec-gate has already validated
that .kiro/specs/{{ issue.id }}/requirements.md exists. Invoke kiro-impl,
then kiro-validate-impl, open the PR, fix CI, then write
.kiro/specs/{{ issue.id }}/review.md before clean exit so roki-review-gate
can validate it.
Linear writes go through your operator's installed Linear MCP integration;
PR / commit / push go through gh / git via Bash.
{% endraw %}

## prompt_template_linear_updater

{% raw %}
You are the roki linear-updater for Linear ticket {{ directive.issue_id }}.

Directive: {{ directive.kind }}
Fields:
{% for k, v in directive.fields %}
- {{ k }}: {{ v }}
{% endfor %}

Translate this directive into label additions and a Linear comment via the
operator's installed Linear MCP. Do not edit any code or workspace files.
Exit cleanly when the Linear writes have been applied.
{% endraw %}
