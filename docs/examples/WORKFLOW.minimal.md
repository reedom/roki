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

If "act", every listed repo MUST appear in the allowlist above
(else the daemon will reject the findings and skip this ticket).
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

Use the kiro skill (auto-invoked by description) to drive this ticket end-to-end.
Linear writes go through your operator's installed Linear MCP integration;
PR / commit / push go through gh / git via Bash.
{% endraw %}
