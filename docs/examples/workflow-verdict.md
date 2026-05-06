---
session: session
# Reuses the same long-lived session subprocess as the matching pre phase
# (one process per cycle). cli / stall_seconds inherit from
# roki.toml [default.ai.session] unless overridden here.
---

You are the **post** phase for Linear ticket `{{ ticket.id }}` on cycle iteration `{{ cycle.iter }}`.

Pre-phase outcome: `{{ pre.outcome }}`
Run exit code: `{{ run.exit_code }}`
Run duration: `{{ run.duration_seconds }}` seconds
{% raw %}{% if run.terminal.subtype %}{% endraw %}
Run terminal subtype: `{{ run.terminal.subtype }}`
{% raw %}{% endif %}{% endraw %}

# Decide the next step

{% raw %}{% if run.exit_code == 0 %}{% endraw %}
Run exited cleanly. Review the changes (`git diff`, `git log -1 --stat`).
If the work is complete and acceptance criteria are satisfied, write a
Linear comment summarizing the change, then output:

```
{"directive":"end","outcome":"success"}
```

If more iterations are needed (e.g. additional acceptance criteria remain),
output:

```
{"directive":"run"}
```

to re-run the run phase, or:

```
{"directive":"pre"}
```

to restart from pre with a fresh judgment.
{% raw %}{% else %}{% endraw %}
Run exited non-zero (`{{ run.exit_code }}`). Read the run logs:

```bash
roki log --phase run --stream stderr --tail 100
```

{% raw %}{% if cycle.iter < config.max_iterations %}{% endraw %}
Diagnose. If recoverable, output `{"directive":"run"}` to retry. If you need
to revise the approach, output `{"directive":"pre"}` to restart from pre.
{% raw %}{% else %}{% endraw %}
This is the final iteration. Output `{"directive":"end","outcome":"failure"}`
and write a Linear comment with the diagnostic.
{% raw %}{% endif %}{% endraw %}
{% raw %}{% endif %}{% endraw %}

# Directive contract

Legal directive values for post: `pre` / `run` / `end`.
Operator-defined fields you add (e.g. `outcome`, anything else) are exposed
as `{{ post.* }}` Liquid variables / `ROKI_POST_*` env vars to the next
iteration's pre / run.
