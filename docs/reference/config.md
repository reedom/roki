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
- `WORKFLOW.toml` — per workspace, hot-reloadable. Admission filter + rule / cleanup / on_failure entries.
- `workflow/*.md` — per workspace, hot-reloadable. Phase prompt / cmd bodies (frontmatter + Liquid template).

Working samples in [`docs/examples/`](../examples/) (pending post-pivot rewrite).

## `roki.toml` schema

Per workspace, specified with `--config <path>` ([cli.md](cli.md)). Not hot-reloaded — restart required.

| Block / Key | Required | Type | Default | Validation | Used by |
|---|---|---|---|---|---|
| `[linear].token` | yes | string | — | Refuses startup if missing or unresolvable | [fr:03](../fr/03-linear-admission.md) |
| `[linear].polling.cadence_seconds` | no | int | `300` | min `60`; refuses startup below | [fr:03 §Polling fallback](../fr/03-linear-admission.md) |
| `[linear.webhook].secret` | yes | string | — | Refuses startup if missing | [fr:03 §Webhook intake](../fr/03-linear-admission.md) |
| `[linear.webhook].bind` | yes | bind addr | — | Refuses startup on bind failure. Internet-facing — Linear cloud must reach it | [fr:03 §Webhook intake](../fr/03-linear-admission.md) |
| `[linear.webhook].port` | yes | port | — | Refuses startup on bind failure | [fr:03 §Webhook intake](../fr/03-linear-admission.md) |
| `[api].port` | no | port | — (unset → API disabled) | When unset, the observability HTTP server does not start. When set, refuses startup on bind failure | [fr:10 §Server gating](../fr/10-http-api.md) |
| `[api].bind` | no | bind addr | `127.0.0.1` | Non-loopback emits a warn log noting the absence of authentication | [fr:10 §Server gating](../fr/10-http-api.md) |
| `[default.ai.session].cli` | yes | string (cli line) | — | Refuses startup if missing. Operator-authored; daemon does not parse the cli line. Liquid-rendered at phase launch | [fr:04 §Subprocess shapes](../fr/04-phase-execution.md) |
| `[default.ai.session].stall_seconds` | no | int | `600` | min `1` | [fr:04 §Stall detection](../fr/04-phase-execution.md) |
| `[default.ai.command].cli` | yes | string (cli line) | — | Refuses startup if missing | [fr:04 §Subprocess shapes](../fr/04-phase-execution.md) |
| `[default.ai.command].stall_seconds` | no | int | `300` | min `1` | [fr:04 §Stall detection](../fr/04-phase-execution.md) |
| `[engine].max_iterations` | no | int | `10` | min `1` | [fr:01 §Iteration cap](../fr/01-engine-model.md) |
| `[engine].shutdown_window_seconds` | no | int | `30` | min `1`, max `600` | [fr:12 §Normal shutdown](../fr/12-daemon-lifecycle.md) |
| `[paths].workflow` | yes | path | — | Refuses startup if file missing / unreadable | [fr:02](../fr/02-configuration.md) |
| `[paths].session_root` | yes | path | — | Refuses startup if parent directory missing or not writable | [fr:05](../fr/05-worktree-and-session.md) |
| `[log].destination` | no | enum (`stdout` / `file` / `both`) | `stdout` | — | [fr:08 §Tier 1](../fr/08-observability-logs.md) |
| `[log].file_path` | yes when `destination ∈ {file, both}` | path | — | Refuses startup if parent directory missing or not writable | [fr:08 §Tier 1](../fr/08-observability-logs.md) |
| `[log].level` | no | enum (`error` / `warn` / `info` / `debug` / `trace`) | `info` | — | [fr:08 §Tier 1](../fr/08-observability-logs.md) |
| `[log].ring_size` | no | int | `1000` | min `0` | [fr:08 §Tier 3](../fr/08-observability-logs.md) |
| `[escalation].queue_size` | no | int | `64` | `1..=1024` | [fr:06 §Escalation queue](../fr/06-failure-handling.md) |

Validation failure refuses startup and emits the offending key path in the structured log. The default-value column lists canonical defaults; a future schema-version bump is the only path to changing them.

## `WORKFLOW.toml` schema

Per workspace, referenced from `roki.toml [paths].workflow`. Hot-reloadable.

### `[admission]` (single block, required)

| Key | Required | Type | Meaning |
|---|---|---|---|
| `assignee` | yes | string | Linear assignee identifier; the literal `me` resolves to the API token holder |

### `[[admission.repos]]` (array, 1+ entries, ordered first-match)

Repo allowlist + dispatch.

| Key | Required | Type | Meaning |
|---|---|---|---|
| `ghq` | yes | string | ghq path (`github.com/foo/bar` or `host/owner/repo`) |
| `workflow` | no | path (relative to WORKFLOW.toml) | Per-repo TOML overriding the top-level `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` lists for this repo |
| `when.*` | no | matcher set | Optional repo-discrimination matchers; entry with no `when.*` is the fallback for tickets matching no other repo entry |

Resolution runs once per cache entry at first admission ([fr:03 §Diff observation](../fr/03-linear-admission.md)). Subsequent webhook updates do not re-resolve.

### `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` (arrays, 0+ entries each, ordered first-match)

| Key | Required | Type | Meaning |
|---|---|---|---|
| `when.*` | yes | matcher set | Conditions; all `when.*` keys within an entry AND together |
| `pre` | no | phase block | Optional. Synthesized `directive: "run"` when omitted |
| `run` | yes | phase block | Required for any cycle-spawning entry. The only legal omission is a `[[cleanup]]` entry with all three phases omitted (immediate-delete shorthand) |
| `post` | no | phase block | Optional. Synthesized `directive: "end"` when omitted |

Phase block declares exactly one of:

| Key | Type | Meaning |
|---|---|---|
| `path = "<file>"` | path (relative to WORKFLOW.toml) | File-form. Frontmatter chooses session vs command. Body is a Liquid template |
| `prompt = "<inline string>"` | string | Inline session-form. Always uses `default.ai.session.cli` |
| `cmd = "<inline string>"` | string | Inline command-form. Operator-authored full cli line, Liquid-rendered |

### Condition vocabulary (`when.*`)

| Operator | Form | Meaning |
|---|---|---|
| Equality | `when.<field> = "<scalar>"` | Field equals the scalar |
| Negation | `when.<field>.not = "<scalar>"` | Field does not equal the scalar |
| Set membership | `when.<field>.in = [...]` | Field is in the set |
| List has-all | `when.labels.has_all = [...]` | Every entry present in ticket labels |
| List has-any | `when.labels.has_any = [...]` | At least one entry present |
| List has-none | `when.labels.has_none = [...]` | None of the entries present |
| String regex | `when.title.regex = "..."` | (admission.repos only) ticket title matches the regex |
| String prefix | `when.title.starts_with = "..."` | (admission.repos only) |
| String contains | `when.title.contains = "..."` / `when.body.contains = "..."` | (admission.repos only) |

Recognized fields:

| Field | Scope |
|---|---|
| `status` | Linear state name |
| `labels` | Linear label list |
| `assignee` | Linear assignee (rule-level only; `[admission].assignee` does the coarse filter) |
| `repo` | admission-resolved ghq path (rule-level only) |
| `kind` | failure kind (`[[on_failure]]` only): `process_crash` / `unparseable` / `schema_drift` / `fs_poison` / `stall` / `iter_exhausted` / `template_error` |
| `phase` | phase name (`[[on_failure]]` only): `pre` / `run` / `post` |
| `title`, `body` | Linear ticket strings (`[[admission.repos]]` only) |

OR is expressed by writing additional entries.

## Per-repo `WORKFLOW.toml` (optional)

Set via `[[admission.repos]] workflow = "<path>"`. The file replaces this repo's `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` lists entirely. Top-level admission stays in WORKFLOW.toml; the per-repo file inherits nothing else.

Schema is identical to the top-level WORKFLOW.toml minus the `[admission]` block.

## `workflow/*.md` schema

Each file referenced from a `*.path` field has YAML frontmatter and a Liquid body.

| Key | Required | Type | Default | Meaning |
|---|---|---|---|---|
| `session` | no | enum (`session` / `command`) | `session` | Subprocess shape ([fr:04](../fr/04-phase-execution.md)) |
| `cli` | no | string (cli line) | (falls back to `[default.ai.{session,command}].cli`) | Per-file override of the cli line |
| `stall_seconds` | no | int | (falls back to `[default.ai.{session,command}].stall_seconds`) | Per-file stall window override |

Body is a Liquid template, rendered against the variables in [fr:01 §Inter-phase data flow](../fr/01-engine-model.md). The rendered text is delivered via stdin per [fr:04 §Input channels](../fr/04-phase-execution.md).

## Hot reload

WORKFLOW.toml + workflow/*.md changes:

| Outcome | Behavior |
|---|---|
| Validation passes on initial load | Apply policy; daemon proceeds to ready |
| Validation fails on initial load | Refuse to start; log offending key path |
| Validation passes on hot reload | Apply on next webhook; in-flight cycles keep their pre-reload policy until they terminate |
| Validation fails on hot reload | Keep the previous policy + log the failure (daemon does not stop) |
| Per-key invalidity inside a single entry | That entry is rejected as if it had not matched; other entries continue to apply |

`roki.toml` is **not** hot-reloaded; changes require a daemon restart.

## Removed legacy keys

The following pre-pivot keys are removed and **explicitly refused** by the loader at startup with an actionable error naming the offending key path:

| Legacy key | Source | Removed because |
|---|---|---|
| `[server].bind` / `[server].port` | `roki.toml` | Single `[server]` block split into `[linear.webhook]` (internet-facing) and `[api]` (loopback observability) |
| `[linear].webhook_secret` | `roki.toml` | Moved into `[linear.webhook].secret` alongside the webhook receiver's bind/port |
| `[linear].admit_states` | `roki.toml` | Status filter now derived from the union of `when.status` values across `[[rule]]` and `[[cleanup]]` entries; explicit allowlist no longer needed |
| `[[repos]]` | `roki.toml` | Repo allowlist moved into `WORKFLOW.toml [[admission.repos]]` |
| `[permissions].strategy` | `roki.toml` | Permission strategy is now pass-through: whatever the operator's cli line declares (`--dangerously-skip-permissions`, `--settings`, etc.) is what the subprocess sees |
| `[judge].*` | `roki.toml` | Pre-admission judge removed; admission is purely mechanical (assignee + repo allowlist) |
| `extension.orchestrator.*` | `WORKFLOW.md` | Orchestrator session removed |
| `extension.phase.<name>.command` | `WORKFLOW.md` | Phase catalog removed; phase invocations are operator-authored via `path` / `prompt` / `cmd` per phase block |
| `extension.phase.<name>.max_turns` | `WORKFLOW.md` | Daemon does not enforce a turn budget; operator's cli line owns it |
| `extension.phase.<name>.stall_seconds` | `WORKFLOW.md` | Replaced by per-file `stall_seconds` in `workflow/*.md` frontmatter |
| `extension.linear_updater.*` | `WORKFLOW.md` | Daemon never writes Linear; writes happen inside operator-authored phase subprocesses |
| `extension.gates.*` | `WORKFLOW.md` | Daemon never validates artifacts |
| `extension.distill.*` | `WORKFLOW.md` | `materialize_spec` flow removed |
| `extension.server.*` | `WORKFLOW.md` | HTTP API config moved to top-level `roki.toml [api]` (not in WORKFLOW.toml) |
| `prompt_template_orchestrator` (named template block) | `WORKFLOW.md` | Orchestrator session removed |
| `prompt_template_<phase>` (named template block) | `WORKFLOW.md` | Phase prompts now live in `workflow/*.md` and inline `prompt` / `cmd` strings |

The loader emits a startup error that names the offending key path. At hot reload the loader retains the previous policy and logs the failure; the daemon does not stop.

## When adding a new key

1. Add a row to the corresponding table above with the canonical default and validation rule.
2. Link to this table from the FR page that uses it.
3. Update the spec the key is owned by (post spec rebuild).

## Related reference

- [cli.md](cli.md): override via CLI flags
- [`docs/examples/`](../examples/): working samples (pending post-pivot rewrite)
