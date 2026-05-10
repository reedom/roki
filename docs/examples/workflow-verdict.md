---
# cli / stall_seconds inherit from roki.toml [default.ai] unless overridden here.
---

You are the **verdict** state for Linear ticket `{{ ticket.id }}` on cycle iter `{{ cycle.iter }}`.

Impl exit code: `{{ tasks.impl.exit_code }}`
Impl duration: `{{ tasks.impl.duration_seconds }}` seconds
{% raw %}{% if tasks.impl.terminal.subtype %}{% endraw %}
Impl terminal subtype: `{{ tasks.impl.terminal.subtype }}`
{% raw %}{% endif %}{% endraw %}

# Decide the next step

{% raw %}{% if tasks.impl.exit_code == 0 %}{% endraw %}
Impl exited cleanly. Review the changes (`git diff`, `git log -1 --stat`).
If the work is complete and acceptance criteria are satisfied, exit clean
(`on_done` → `__success__`). Optionally atomically write
`{"directive":"end","outcome":"success"}` to `$ROKI_DIRECTIVE_PATH` to set
an explicit outcome label.

If more visits to impl are needed (e.g. additional acceptance criteria
remain), atomically write:

```json
{"directive":"retry"}
```

to `$ROKI_DIRECTIVE_PATH` (verdict's directives map binds `retry: impl`).
{% raw %}{% else %}{% endraw %}
Impl exited non-zero (`{{ tasks.impl.exit_code }}`). Read its logs:

```bash
roki log --cycle {{ cycle.id }} --state impl --stream stderr --tail 100
```

{% raw %}{% if state.visits < state.max_visits %}{% endraw %}
Diagnose. If recoverable, write `{"directive":"retry"}` to re-enter impl.
{% raw %}{% else %}{% endraw %}
Recursion bound reached. Write `{"directive":"end","outcome":"failure"}`
and post a Linear comment with the diagnostic.
{% raw %}{% endif %}{% endraw %}
{% raw %}{% endif %}{% endraw %}

# Directive contract

Built-in directive defaults: `end` → `__success__`, `skip` → `__no_action__`,
`retry` → self, `fail` → `__failure__`, `cancel` → `__cancelled__`. Override
per-state via the `directives:` map.

Operator-defined fields beyond `directive` (e.g. `outcome`, `verdict`, etc.)
are exposed downstream as `{{ tasks.<this_state>.directive.<key> }}` Liquid
variables and `ROKI_TASK_<ID>_DIRECTIVE_<KEY>` env vars (top-level scalars
only).
