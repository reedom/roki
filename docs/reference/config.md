---
refs:
  id: ref:config
  kind: reference
  title: "Configuration Schema"
  related:
    - ref:cli
    - fr:02-configuration
    - fr:01-engine-model
    - fr:03-linear-admission
    - fr:08-observability-logs
    - fr:10-http-api
---

# Reference: Configuration Schema

Schema for the three configuration files:

- `roki.toml` — per workspace, restart-only ([fr:02 §`roki.toml`](../fr/02-configuration.md)).
- `WORKFLOW.yaml` — per workspace, hot-reloadable. Admission filter + rules + cleanup + on_failure state machines.
- `workflow/*.md` — per workspace, hot-reloadable. State body (frontmatter + Liquid template).

Working samples in [`docs/examples/`](../examples/).

## `roki.toml` schema

Per workspace, specified with `--config <path>` ([cli.md](cli.md)). Not hot-reloaded — restart required.

| Block / Key | Required | Type | Default | Validation | Used by |
|---|---|---|---|---|---|
| `[linear].token` | yes | string | — | Refuses startup if missing or unresolvable | [fr:03](../fr/03-linear-admission.md) |
| `[linear].polling.cadence_seconds` | no | int | `300` | min `60`; refuses startup below | [fr:03 §Polling fallback](../fr/03-linear-admission.md) |
| `[linear.webhook].secret` | yes | string | — | Refuses startup if missing | [fr:03 §Webhook intake](../fr/03-linear-admission.md) |
| `[linear.webhook].bind` | yes | bind addr | — | Refuses startup on bind failure. Internet-facing — Linear cloud must reach it | [fr:03 §Webhook intake](../fr/03-linear-admission.md) |
| `[linear.webhook].port` | yes | port | — | Refuses startup on bind failure | [fr:03 §Webhook intake](../fr/03-linear-admission.md) |
| `[api].port` | no | port | — (unset → API disabled) | When unset, the observability HTTP server does not start. When set, refuses startup on bind failure | [fr:10 §Server gating and bind](../fr/10-http-api.md) |
| `[api].bind` | no | bind addr | `127.0.0.1` | Non-loopback emits a warn log noting the absence of authentication | [fr:10 §Server gating and bind](../fr/10-http-api.md) |
| `[api].ticket_events_window` | no | int | `50` | `1..=500` | [fr:10 §GET /api/tickets/{id}](../fr/10-http-api.md) |
| `[api].cycle_list_window` | no | int | `50` | `1..=500` | [fr:10 §GET /api/tickets/{id}/cycles](../fr/10-http-api.md) |
| `[default.ai].cli` | no | string (cli line) | — (no default) | Operator-authored; daemon does not parse the cli line. Liquid-rendered at state launch. Not validated at startup; first failure surfaces as `process_crash` on first state that uses it | [fr:04 §Subprocess shape](../fr/04-state-execution.md) |
| `[default.ai].stall_seconds` | no | int | `300` | min `1` | [fr:04 §Stall detection](../fr/04-state-execution.md) |
| `[engine].max_iterations` | no | int | `10` | min `1` | [fr:01 §Recursion bound](../fr/01-engine-model.md) |
| `[engine].shutdown_window_seconds` | no | int | `30` | min `1`, max `600` | [fr:12 §Normal shutdown](../fr/12-daemon-lifecycle.md) |
| `[paths].workflow` | yes | path | `./WORKFLOW.yaml` | Refuses startup if file missing / unreadable | [fr:02](../fr/02-configuration.md) |
| `[paths].session_root` | yes | path | — | Refuses startup if parent directory missing or not writable | [fr:05](../fr/05-worktree-and-session.md) |
| `[log].destination` | no | enum (`stdout` / `file` / `both`) | `stdout` | — | [fr:08 §Tier 1](../fr/08-observability-logs.md) |
| `[log].file_path` | yes when `destination ∈ {file, both}` | path | — | Refuses startup if parent directory missing or not writable | [fr:08 §Tier 1](../fr/08-observability-logs.md) |
| `[log].level` | no | enum (`error` / `warn` / `info` / `debug` / `trace`) | `info` | — | [fr:08 §Tier 1](../fr/08-observability-logs.md) |
| `[log].ring_size` | no | int | `1000` | min `0` | [fr:08 §Tier 3](../fr/08-observability-logs.md) |
| `[escalation].queue_size` | no | int | `64` | `1..=1024` | [fr:06 §Escalation queue](../fr/06-failure-handling.md) |

Validation failure refuses startup and emits the offending key path in the structured log. The default-value column lists canonical defaults; a future schema-version bump is the only path to changing them.

## `WORKFLOW.yaml` schema

Per workspace, referenced from `roki.toml [paths].workflow`. Restart-only.

### Top-level shape

```yaml
admission:                    # required, single block
  assignee: <string>
  repos:                      # 1+ entries, ordered first-match
    - ghq: <string>
      when: <WhenClause>      # optional
      workflow: <path>        # optional per-repo override file

rules:                        # 0..N, ordered first-match
  - when: <WhenClause>
    <SugarOrCanonical>

cleanup:                      # 0..N, ordered first-match; evaluated before rules
  - when: <WhenClause>
    <SugarOrCanonical>        # OR omitted entirely (immediate-delete shorthand)

on_failure:                   # 0..N, ordered first-match
  - when: <WhenClause>
    <SugarOrCanonical>
```

### `admission`

| Key | Required | Type | Meaning |
|---|---|---|---|
| `assignee` | yes | string | Linear assignee identifier; the literal `me` resolves to the API token holder |
| `repos` | yes | list | 1+ entries, ordered first-match |

Per-repo `workflow:` resolution runs once per cache entry at first admission ([fr:03 §Diff observation](../fr/03-linear-admission.md)). Subsequent webhook updates do not re-resolve.

### `WhenClause`

```yaml
when:
  status: <scalar>                            # equality
  status: { not: <scalar> }                   # negation
  status: { in: [<scalar>, ...] }             # set membership

  labels: { has_all:  [<string>, ...] }
  labels: { has_any:  [<string>, ...] }
  labels: { has_none: [<string>, ...] }

  assignee: <scalar>                          # rule-level refinement
  repo: <ghq path>                            # rule-level only
  kind: <scalar>                              # on_failure only
  phase: <scalar>                             # on_failure only: state id that emitted the failure

  title: { regex: <string> }                  # admission.repos only
  title: { starts_with: <string> }            # admission.repos only
  title: { contains: <string> }               # admission.repos only
  body:  { contains: <string> }               # admission.repos only
```

All `when.*` keys AND together. OR by writing more list entries.

| Field | Scope |
|---|---|
| `status` | Linear state name |
| `labels` | Linear label list |
| `assignee` | Linear assignee (rule-level only; `admission.assignee` does the coarse filter) |
| `repo` | admission-resolved ghq path (rule-level only) |
| `kind` | failure kind (`on_failure` only): `process_crash` / `unparseable` / `schema_drift` / `fs_poison` / `stall` / `recursion_bound` / `template_error` |
| `phase` | state id that emitted the failure (`on_failure` only) |
| `title`, `body` | Linear ticket strings (`admission.repos` only) |

### Sugar form (`tasks:`)

Linear chain. Each task becomes a state with `on_done` chained to the next; the last task's `on_done` defaults to `__success__`. `on_fail` defaults to the rule-level `on_fail`, else `__failure__`.

```yaml
tasks:
  - id: <state_id>
    run: <inline cmd>                         # OR uses: <path>
    if: <Liquid expr>                         # optional skip condition
    timeout: <duration>                       # optional; overrides default stall window
    on_fail: <state_id>                       # optional
    directives:
      <directive_name>: <state_id>            # short form
      <directive_name>:                       # long form
        target: <state_id>
        max_visits: <int>
    max_visits: <int>                         # optional
on_fail: <state_id>                           # rule-level default
states:                                       # optional inline canonical states
  <state_id>: <StateBody>
terminals:
  <state_id>: { outcome: <string> }
```

### Canonical form (explicit state machine)

```yaml
start: <state_id>
states:
  <state_id>:
    run: <inline cmd>                         # OR uses: <path>
    if: <Liquid expr>
    timeout: <duration>
    on_done: <state_id>
    on_fail: <state_id>
    directives: { <name>: <state_id>, ... }
    max_visits: <int>
terminals:
  <state_id>: { outcome: <string> }
```

### State body fields

Exactly one of `run:` / `uses:` per state. Every state is command-shape: each visit spawns a fresh subprocess.

| Field | Type | Notes |
|---|---|---|
| `run` | string (Liquid) | Inline shell command; spawned via `sh -c` (POSIX) / `cmd /C` (Windows) |
| `uses` | path | Path to `workflow/*.md`. Frontmatter `cli:` and `stall_seconds:` honored |

### Cleanup immediate-delete shorthand

A `cleanup:` entry with no `tasks:` / `states:` / `terminals:` deletes worktree + session_tempdir synchronously without a cycle. Recognized only inside `cleanup:`. An entry without a body inside `rules:` or `on_failure:` is a schema error.

### Reserved terminal ids

Auto-injected when referenced and not declared:

| Id | Default `outcome` | Auto-targeted by |
|---|---|---|
| `__success__` | `success` | `directives.end`, last-task `on_done` |
| `__failure__` | `failure` | `directives.fail`, default `on_fail` |
| `__no_action__` | `no_action` | `directives.skip` |
| `__cancelled__` | `cancelled` | Operator `directives.cancel` only — daemon never auto-targets |

State ids beginning with `__` are reserved and rejected at validate time.

### Built-in directive name defaults

When a directive name received at runtime is not in the state's `directives:` map, these defaults apply:

| Directive name | Default edge target |
|---|---|
| `end` | `__success__` |
| `skip` | `__no_action__` |
| `retry` | self (current state id) |
| `fail` | `__failure__` |
| `cancel` | `__cancelled__` |

A directive name not in the state's `directives:` ∪ defaults is a `schema_drift` failure.

### Path resolution

| Path field | Resolved relative to |
|---|---|
| `roki.toml [paths] workflow` | `roki.toml` directory (or absolute) |
| `admission.repos[].workflow` | top-level `WORKFLOW.yaml` directory |
| State `uses:` inside top-level `WORKFLOW.yaml` | top-level `WORKFLOW.yaml` directory |
| State `uses:` inside per-repo override file | the override file's directory |

All path fields accept absolute or `~`-prefixed paths.

### Validation rules (load-time)

The loader rejects the file (refuses startup) on any of the following. All errors accumulate before reporting.

1. Edge target id not in `states` ∪ `terminals`.
2. State has both `run:` and `uses:` (mutually exclusive).
3. State has neither `run:` nor `uses:` and is not a terminal (`OrphanBody`).
4. State id begins with `__` (reserved prefix).
5. Cycle in the state graph where no state on the cycle declares `max_visits` and Pass 5 auto-injection has not run.
6. Terminal `outcome` is empty.
7. `start:` references a non-existent state, or references a terminal in a non-shorthand machine.
8. State id does not match `[A-Za-z][A-Za-z0-9_]*` (must be safe for `ROKI_TASK_<ID>_*` env-var encoding).

`roki workflow validate <FILE>` runs the same expansion + validation pipeline ahead of daemon restart.

## Per-repo `WORKFLOW.yaml` (optional)

Set via `admission.repos[].workflow: <path>`. The file replaces this repo's `rules:` / `cleanup:` / `on_failure:` lists entirely. Top-level `admission:` stays in the parent file; per-repo files must not declare `admission:`.

Schema is identical to the top-level minus `admission:`.

## `workflow/*.md` schema

Each file referenced from a state's `uses:` field has YAML frontmatter and a Liquid body. Every state is command-shape: each visit spawns a fresh subprocess.

| Key | Required | Type | Default | Meaning |
|---|---|---|---|---|
| `cli` | no | string (cli line) | falls back to `[default.ai].cli` | Per-file override of the cli line |
| `stall_seconds` | no | int | falls back to `[default.ai].stall_seconds` | Per-file stall window override |

Body is a Liquid template, rendered against the variables in [fr:01 §Inter-state data flow](../fr/01-engine-model.md). The rendered text is delivered via stdin per [fr:04 §Input channels](../fr/04-state-execution.md).

## Hot reload

`WORKFLOW.yaml` + `workflow/*.md` changes are picked up without restart ([fr:02 §Hot reload and validation](../fr/02-configuration.md)). Validation passes apply the new policy from the next webhook; in-flight cycles keep their pre-reload policy until they terminate. Validation failures retain the previous policy and log the offending entry; the daemon does not stop. A `workflow/*.md` change is treated identically to a `WORKFLOW.yaml` change.

`roki.toml` is restart-only.

## Removed legacy keys

The following pre-pivot keys are removed and **explicitly refused** by the loader at startup with an actionable error naming the offending key path:

| Legacy key | Source | Removed because |
|---|---|---|
| `[server].bind` / `[server].port` | `roki.toml` | Single `[server]` block split into `[linear.webhook]` (internet-facing) and `[api]` (loopback observability) |
| `[linear].webhook_secret` | `roki.toml` | Moved into `[linear.webhook].secret` alongside the webhook receiver's bind/port |
| `[linear].admit_states` | `roki.toml` | Status filter now derived from the union of `when.status` values across `rules:` and `cleanup:` entries; explicit allowlist no longer needed |
| `[[repos]]` | `roki.toml` | Repo allowlist moved into `WORKFLOW.yaml admission.repos` |
| `[default.ai.session]` (entire block) | `roki.toml` | Session-shape subprocesses removed; every state is command-shape. Use `[default.ai]` |
| `[default.ai.command]` (entire block) | `roki.toml` | Renamed to `[default.ai]` (single block; no shape distinction) |
| `[permissions].strategy` | `roki.toml` | Permission strategy is now pass-through: whatever the operator's cli line declares (`--dangerously-skip-permissions`, `--settings`, etc.) is what the subprocess sees |
| `[judge].*` | `roki.toml` | Pre-admission judge removed; admission is purely mechanical (assignee + repo allowlist) |
| `extension.orchestrator.*` | `WORKFLOW.md` | Orchestrator session removed |
| `extension.phase.<name>.command` | `WORKFLOW.md` | Phase catalog removed; phase invocations are operator-authored via `path` / `prompt` / `cmd` per phase block |
| `extension.phase.<name>.max_turns` | `WORKFLOW.md` | Daemon does not enforce a turn budget; operator's cli line owns it |
| `extension.phase.<name>.stall_seconds` | `WORKFLOW.md` | Replaced by per-file `stall_seconds` in `workflow/*.md` frontmatter |
| `extension.linear_updater.*` | `WORKFLOW.md` | Daemon never writes Linear; writes happen inside operator-authored phase subprocesses |
| `extension.gates.*` | `WORKFLOW.md` | Daemon never validates artifacts |
| `extension.distill.*` | `WORKFLOW.md` | `materialize_spec` flow removed |
| `extension.server.*` | `WORKFLOW.toml` | HTTP API config moved to top-level `roki.toml [api]` |
| `prompt_template_orchestrator` (named template block) | `WORKFLOW.toml` | Orchestrator session removed |
| `prompt_template_<phase>` (named template block) | `WORKFLOW.toml` | Phase prompts removed; state bodies live in `workflow/*.md` (`uses:`) or inline `run.cmd` |
| `WORKFLOW.toml` (entire file) | `[paths].workflow` | Schema migrated to `WORKFLOW.yaml` (state-machine model). Loader refuses any path with `.toml` extension |
| `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` (TOML array-of-tables form) | `WORKFLOW.toml` | YAML lists `rules:` / `cleanup:` / `on_failure:` |
| `pre` / `run` / `post` (phase blocks) | `WORKFLOW.toml` | State machine replaces fixed phase loop. Use `tasks:` sugar or canonical `start:` / `states:` / `terminals:` |
| `session:` frontmatter key | `workflow/*.md` | Session-shape removed. Every state is command-shape |

The loader emits a startup error that names the offending key path. At hot reload the loader retains the previous policy and logs the failure; the daemon does not stop.

## When adding a new key

1. Add a row to the corresponding table above with the canonical default and validation rule.
2. Link to this table from the FR page that uses it.
3. Update the spec the key is owned by (post spec rebuild).

## Related reference

- [cli.md](cli.md): override via CLI flags
- [`docs/examples/`](../examples/): working samples (pending post-pivot rewrite)
