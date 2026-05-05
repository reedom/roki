---
refs:
  id: fr:02-configuration
  kind: fr
  title: "Configuration"
  spec: roki-mvp
  implements:
    - req:roki-mvp:2
    - req:roki-mvp:6
  depends_on:
    - ref:config
  related:
    - ref:cli
    - fr:04-state-machine-and-recovery
    - fr:20-rule-and-cycle-engine
  modules:
    - crates/roki-daemon/src/config/
    - crates/roki-daemon/src/workflow/
---

# FR 02: Configuration

> Three configuration files: `roki.toml` (per workspace, restart-only), `WORKFLOW.toml` (per workspace, hot-reloadable), and `workflow/*.md` (per workspace, hot-reloadable). Together they describe everything the daemon does. The full schema (key names, defaults, validation rules) lives in [`docs/reference/config.md`](../reference/config.md).

## Purpose

`roki.toml` holds daemon startup conditions (Linear access, network, AI default CLIs, log destination, paths); changes require restart. `WORKFLOW.toml` and the `workflow/*.md` files referenced from it hold all workflow behavior — admission filter, rule / cleanup / on_failure entries, phase prompts and commands — and are **hot-reloaded without restart**. The hard-coded `prompt_template_orchestrator` / `prompt_template_implement_direct` / `prompt_template_validate_direct` / `prompt_template_open_pr` / `prompt_template_<phase>` schema and the `extension.*` namespace from earlier versions are removed: workflow behavior is now expressed by operator-authored TOML and Markdown, not by daemon-known template names.

## User-visible Behavior

### `roki.toml` (immutable at startup)

Operators specify the path with `--config <path>` ([01-daemon-lifecycle](01-daemon-lifecycle.md)). The file groups into:

- **Linear access**: API token, webhook secret, the assignee identifier whose tickets the daemon admits (`me` resolves to the API token holder).
- **Network**: bind address and port for the webhook receiver and HTTP API.
- **AI default CLIs**: the cli line and stall window the daemon uses when a workflow phase declares `session = "session"` (long-lived stream-json AI reused within one cycle's pre/post chain) or `session = "command"` (one-shot subprocess) without specifying its own cli line.
- **Engine knobs**: per-cycle iteration cap and (future) concurrency cap.
- **Paths**: where to load WORKFLOW.toml from, where to put session tempdirs, where to put worktrees.
- **Log destination**: the structured event log goes to stdout, a file, or both, with operator-set rotation policy.

Any invalid value or resolution failure (`linear.assignee` cannot be resolved, `[default.ai.session].cli` missing, WORKFLOW.toml path missing, token missing, etc.) **refuses startup** and emits the offending field in the structured log.

`roki.toml` itself is not hot-reloaded; changing it requires a daemon restart. The exact name, default, and validation rule for each key live in the "roki.toml schema" table in [`docs/reference/config.md`](../reference/config.md).

A canonical layout:

```toml
[linear]
token = "lin_api_..."
webhook_secret = "..."
assignee = "me"

[network]
bind = "127.0.0.1"
port = 8080

[default.ai.session]
cli = "claude --input-format stream-json --output-format stream-json --model claude-opus-4-7"
stall_seconds = 600

[default.ai.command]
cli = "claude -p '{{ prompt }}' --output-format stream-json --max-turns 100"
stall_seconds = 300

[engine]
max_iterations = 10

[paths]
workflow = "./WORKFLOW.toml"
session_root = "~/.cache/roki/sessions"
worktree_root = "~/wt"

[log]
destination = "stdout"      # "stdout" | "file" | "both"
file_path = "/var/log/roki/daemon.jsonl"
level = "info"
ring_size = 1000
```

The previous fixed Linear label conventions (`roki:ready`, `roki:impl`) are no longer hard-coded. Operators express any label-driven gating inside `[[rule]]` / `[[cleanup]]` entries (see below). Conventional names are still recommended in operator docs, but the daemon does not interpret them.

### `WORKFLOW.toml` (hot-reloadable)

A single per-workspace TOML file referenced from `roki.toml [paths].workflow`. Two roles:

1. **Admission filter** — coarse gate evaluated on every webhook before any rule list is touched. Tickets that fail the filter are silently evicted (logged but not surfaced to Linear).
2. **Rule / cleanup / on_failure entries** — the lists [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md) evaluates first-match to dispatch a cycle.

```toml
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/foo/bar"
when.labels.has_any = ["repo:bar"]
workflow = "repos/bar.toml"      # optional per-repo TOML (see below)

[[admission.repos]]
ghq = "github.com/foo/baz"
when.title.regex = "^\\[baz\\]"

[[admission.repos]]
ghq = "github.com/foo/qux"
# `when` omitted → fallback for tickets that match no other repo entry

[[rule]]
when.status = "Todo"
when.labels.has_all = ["roki:ready"]
pre.path = "workflow/01-judge.md"
run.path = "workflow/01-impl.md"
post.path = "workflow/01-verdict.md"

[[cleanup]]
when.status.in = ["Done", "Cancelled"]
pre.prompt = "Final ceremony comment via Linear MCP. Output {directive: 'run'}"
run.cmd = "claude -p 'post final summary' --output-format stream-json --max-turns 5"
post.prompt = "Output {directive: 'end'}"

[[cleanup]]
when.labels.has_none = ["roki:ready"]
# All phases omitted → daemon performs immediate worktree + session_tempdir delete with no cycle.

[[on_failure]]
when.kind.in = ["unparseable", "schema_drift"]
run.cmd = "claude -p '/post-mortem {{ failure.failed_cycle_id }}'"
post.prompt = "Output {directive: 'end'}"
```

#### Per-repo `WORKFLOW.toml` (optional)

When `[[admission.repos]] workflow = "<path>"` is set, that file replaces this repo's `[[rule]]` / `[[cleanup]]` / `[[on_failure]]` lists entirely. The top-level admission stays in WORKFLOW.toml; the per-repo file inherits nothing else from the top-level rule set. Operators that want shared rules across repos either keep a single WORKFLOW.toml (using `when.repo` matchers to dispatch) or duplicate the shared entries into each per-repo file.

#### Condition vocabulary (MVP)

Each entry inside the lists uses `when.<field>` keys; all `when.*` keys within an entry AND together. OR is expressed by writing additional entries.

| Operator | Form | Meaning |
|---|---|---|
| Equality | `when.<field> = "<scalar>"` | Field equals the scalar |
| Negation | `when.<field>.not = "<scalar>"` | Field does not equal the scalar |
| Set membership | `when.<field>.in = ["<a>", "<b>"]` | Field is in the set |
| List has-all | `when.labels.has_all = [...]` | Every entry is present in the ticket's labels |
| List has-any | `when.labels.has_any = [...]` | At least one entry is present |
| List has-none | `when.labels.has_none = [...]` | None of the entries is present |
| String regex | `when.title.regex = "..."` | (admission.repos only) Linear ticket title matches the regex |
| String prefix | `when.title.starts_with = "..."` | (admission.repos only) |
| String contains | `when.title.contains = "..."` / `when.body.contains = "..."` | (admission.repos only) |

Recognized fields:

- `status` — Linear state name.
- `labels` — Linear label list.
- `assignee` — Linear assignee (rule-level only; `[admission].assignee` does the coarse filter).
- `repo` — admission-resolved ghq path (rule-level only; admission resolves it before rule evaluation).
- `kind` — failure kind (on_failure entries only).
- `phase` — phase name (on_failure entries only; values `pre` / `run` / `post`).
- `title`, `body` — Linear ticket strings (admission.repos only, used for repo discrimination).

#### Phase specification

Each phase declares exactly one of `path` / `prompt` / `cmd` (mutually exclusive):

- `path = "<file>"` — file form. The file's frontmatter chooses `session: "session"` (long-lived AI reused within the cycle) or `session: "command"` (one-shot subprocess). The body is a Liquid template.
- `prompt = "<inline string>"` — inline session form. Always uses `default.ai.session.cli` from `roki.toml`.
- `cmd = "<inline string>"` — inline command form. The operator writes the full command line; the daemon spawns the process directly.

### `workflow/*.md`

Each file referenced from a `*.path` field has YAML frontmatter and a Liquid body:

```yaml
---
session: session       # or "command" (default = "session")
cli: ""                # session: ignored. command: optional override; falls back to roki.toml [default.ai.command].cli
stall_seconds: 600     # optional override of default.ai.{session,command}.stall_seconds
---
{Liquid body, rendered against the per-phase context envelope}
```

The Liquid body is rendered against the variables documented in [20-rule-and-cycle-engine §Inter-phase data flow](20-rule-and-cycle-engine.md). The rendered text is what the daemon passes to the subprocess: as the system / first-turn prompt for `session: "session"` mode, or as stdin for `session: "command"` mode. (Inline command-form phases bypass rendering; the operator's `cmd` string is itself rendered as a Liquid template, but no separate prompt body is supplied.)

### Hot reload and validation

- **Schema validation failure at startup** → refuse to start + log the offending key path.
- **Validation passes on hot reload** → apply the new policy from the next webhook (in-flight cycles keep their pre-reload policy until they terminate).
- **Validation fails on hot reload** → keep the previous policy + log the failure (the daemon does not stop).
- **Per-key invalidity inside a single entry** → that entry is rejected as if it had not matched; other entries continue to apply. The structured log records the offending entry.
- **`workflow/*.md` change** is treated identically to a `WORKFLOW.toml` change for the purposes of hot reload.

## Capabilities

- **Three files, three responsibilities**: `roki.toml` for restart-time concerns, `WORKFLOW.toml` for the dispatch tables, `workflow/*.md` for the phase bodies. Each file's hot-reload behavior matches its contents.
- **One daemon for multiple repos**: a single developer runs a single daemon. The `[admission].assignee` filter ensures the daemon does not touch other people's tickets; the `[[admission.repos]]` matchers dispatch each ticket to the correct repo.
- **Operator-defined label gating**: there are no fixed label names. Operators encode whatever labels they want inside `[[rule]]` / `[[cleanup]]` `when.labels.*` clauses.
- **Per-repo workflow split (optional)**: operators with multiple repos can keep a top-level WORKFLOW.toml plus per-repo files via `[[admission.repos]] workflow = "..."`.
- **Defaulted-key logging**: when an unspecified key falls back to its default, the startup log records which key did so.
- **Hot-reload safe**: invalid values do not crash the daemon (the previous policy is retained).
- **Engine-agnostic CLI lines**: `[default.ai.session]` and `[default.ai.command]` accept any cli line that speaks the appropriate protocol (stream-json bidirectional for session, exit-code-and-stdout for command). Operators can switch between claude, codex, or any equivalent without touching the daemon.

## Boundaries

- **Hot reload of `roki.toml`** is out of scope (only WORKFLOW.toml + workflow/*.md are hot-reloadable).
- **Per-issue / per-attempt config overrides** are out of scope.
- **A daemon-managed canonical label set** is out of scope. Operators choose their own label conventions.
- **Environment-variable / CLI configuration overrides** are limited to a few values exposed on the CLI (`--bind`, `--port`, `--config`); a full override surface is not provided (see [cli reference](../reference/cli.md) for details).
- **Conditional includes / partial templates inside `WORKFLOW.toml`** are out of scope for MVP. The `include = [...]` directive is reserved for a future iteration.
- **Daemon-known phase template names** (`prompt_template_orchestrator`, `prompt_template_implement_direct`, `prompt_template_validate_direct`, `prompt_template_open_pr`, `prompt_template_<phase>`) are removed. Operators name their phase files however they like under `workflow/`.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "WORKFLOW loader (hot reload)" and Boundary Strategy > "Shared seams to watch" > "WORKFLOW schema".
- **Requirements**:
  - `roki-mvp Req 2`: Configuration, Assignee Admission, and Multi-Repo Allowlist.
  - `roki-mvp Req 6`: Workspace-Level WORKFLOW Policy Loader.
  - `roki-observability Req 1`, `Req 7`, `Req 15`: Server config gating.
- **Design**:
  - `Configuration Schema` / `Workflow Loader` sections of `.kiro/specs/roki-mvp/design.md` (pending rewrite).
  - The Configuration sections of each spec's `design.md`.
- **Related reference**: [config.md](../reference/config.md), [cli.md](../reference/cli.md).
- **Related FR**: [04-state-machine-and-recovery](04-state-machine-and-recovery.md) (admission filter and diff cache consume `[admission]`), [20-rule-and-cycle-engine](20-rule-and-cycle-engine.md) (the rule / cleanup / on_failure lists this file populates).
