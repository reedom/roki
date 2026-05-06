---
session: command
# cli omitted → falls back to roki.toml [default.ai.command].cli
# stall_seconds omitted → falls back to roki.toml [default.ai.command].stall_seconds
---

You are the **run** phase for Linear ticket `{{ ticket.id }}` (cycle iteration `{{ cycle.iter }}`).

Title: {{ ticket.title }}
Repo: {{ repo.ghq }}
Working directory: the daemon launched you with cwd set to the per-ticket worktree (or the ghq base path if the worktree was not yet materialized — treat that as read-only).

{% raw %}{% if pre.outcome %}{% endraw %}
Pre-phase outcome: `{{ pre.outcome }}`
{% raw %}{% endif %}{% endraw %}

{% raw %}{% if cycle.iter > 1 and run.exit_code %}{% endraw %}
Previous iteration's run exit code: `{{ run.exit_code }}`. Diagnose any
regression before proceeding.
{% raw %}{% endif %}{% endraw %}

# Mission

Single-task TDD implementation against the ticket's acceptance criteria. Write
tests first (RED), implement minimal code (GREEN), refactor while keeping
tests green. Stage only files actually changed. Commit when each criterion is
satisfied.

# Tools available

Whatever the operator's cli line provides — typically the full Claude Code
installation: `Bash`, `Edit`, `Write`, `MultiEdit`, `Read`, `Glob`, `Grep`,
`Agent`, plus operator-installed MCPs (Linear, etc.).

# Boundary

You are a one-shot subprocess. The daemon does not interpret reasoning text.
The cycle engine routes the next phase based on the post directive (which
will read your `run.exit_code` and `run.terminal.*`). Run is not expected to
emit a JSON directive; clean exit (code 0) signals success.
