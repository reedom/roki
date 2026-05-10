---
refs:
  id: fr:02-configuration
  kind: fr
  title: "Configuration"
  spec: roki-skeleton
  depends_on:
    - ref:config
  related:
    - ref:cli
    - fr:07-recovery
    - fr:01-engine-model
  modules:
    - crates/roki-daemon/src/config/
    - crates/roki-daemon/src/workflow/
---

# FR 02: Configuration

> Three configuration files: `roki.toml` (per workspace, restart-only), `WORKFLOW.yaml` (per workspace, hot-reloadable), and `workflow/*.md` (per workspace, hot-reloadable). Together they describe everything the daemon does. The full schema (key names, defaults, validation rules) lives in [`docs/reference/config.md`](../reference/config.md).

## Purpose

`roki.toml` holds daemon startup conditions (Linear access, network, AI default CLI, log destination, paths); changes require restart. `WORKFLOW.yaml` and the `workflow/*.md` files referenced from it hold all workflow behavior — admission filter, rule / cleanup / on_failure entries, state bodies — and are **hot-reloaded without restart**. Workflow behavior is expressed entirely by operator-authored YAML and Markdown; the daemon knows no fixed template names.

## User-visible Behavior

### `roki.toml` (immutable at startup)

Operators specify the path with `--config <path>` ([12-daemon-lifecycle](12-daemon-lifecycle.md)). The file groups into:

- **Linear access**: API token, polling cadence. The assignee identifier lives in WORKFLOW.yaml `[admission]`, not here.
- **Linear webhook receiver** (`[linear.webhook]`, required): bind address, port, and HMAC secret for the **internet-facing** webhook ingress. Required because Linear strongly recommends webhook ingestion over polling.
- **Observability HTTP API** (`[api]`, optional): bind address and port for the **read-only** observability surface consumed by `roki-tui` and `roki events`. Default loopback. If `[api].port` is unset the server does not start.
- **AI default CLI**: the cli line and stall window the daemon uses when a state declares `uses:` with no per-file `cli:` override (or runs an inline `run:` that defers to the default).
- **Engine knobs**: per-cycle iteration cap.
- **Paths**: where to load WORKFLOW.yaml from, where to put session tempdirs.
- **Log destination**: the structured event log goes to stdout, a file, or both, with operator-set rotation policy.

Any invalid value or resolution failure (`[admission].assignee` cannot be resolved against the Linear API token holder, WORKFLOW.yaml path missing, token missing, `[linear.webhook]` missing, etc.) **refuses startup** and emits the offending field in the structured log. `[api]` is optional and its absence is logged at info severity but does not refuse startup. `[default.ai].cli` is **not** validated at startup (the daemon does not parse the cli string); its first failure surfaces as `process_crash` on the first state that uses it.

`roki.toml` itself is not hot-reloaded; changing it requires a daemon restart. The exact name, default, and validation rule for each key live in the "roki.toml schema" table in [`docs/reference/config.md`](../reference/config.md).

A canonical layout:

```toml
[linear]
token = "lin_api_..."
polling.cadence_seconds = 300   # default 300, validation min 60. Polling runs only as a fallback when webhook ingress is unavailable.

[linear.webhook]
secret = "..."
bind = "0.0.0.0"   # internet-facing ingress; Linear cloud must reach it
port = 9090

[api]
# Optional read-only observability surface for roki-tui and `roki events`.
# If `port` is omitted the API server does not start.
bind = "127.0.0.1"   # loopback default; non-loopback emits a warn log at startup
port = 8080

[default.ai]
cli = "claude -p --output-format stream-json --max-turns 100"
stall_seconds = 300

[engine]
max_iterations = 10

[paths]
workflow = "./WORKFLOW.yaml"
session_root = "~/.cache/roki/sessions"

[log]
destination = "stdout"      # "stdout" | "file" | "both"
file_path = "/var/log/roki/daemon.jsonl"
level = "info"
ring_size = 1000

[escalation]
queue_size = 64             # default 64; min 1; max 1024
```

Linear label names are not interpreted by the daemon. Operators express any label-driven gating inside `rules:` / `cleanup:` entries (see below). Example label values (`roki:ready`, `repo:bar`, etc.) below are conventions a particular operator might pick.

### `WORKFLOW.yaml` (hot-reloadable)

A single per-workspace YAML file referenced from `roki.toml [paths].workflow`. Two roles:

1. **Admission filter** — coarse gate evaluated on every webhook before any rule list is touched. Tickets that fail the filter are silently evicted (logged but not surfaced to Linear).
2. **Rule / cleanup / on_failure entries** — the lists [01-engine-model](01-engine-model.md) evaluates first-match to dispatch a cycle.

```yaml
admission:
  assignee: me
  repos:
    - ghq: github.com/foo/bar
      when:
        labels: { has_any: [repo:bar] }
      workflow: repos/bar.yaml          # optional per-repo override (see below)
    - ghq: github.com/foo/baz
      when:
        title: { regex: "^\\[baz\\]" }
    - ghq: github.com/foo/qux
      # `when` omitted → fallback for tickets that match no other repo entry

rules:
  - when:
      status: Todo
      labels: { has_all: [roki:ready] }
    tasks:
      - id: analyze
        uses: workflow/01-analyze.md
      - id: impl
        uses: workflow/01-impl.md
      - id: verdict
        uses: workflow/01-verdict.md

cleanup:
  - when:
      status: { in: [Done, Cancelled] }
    tasks:
      - id: ceremony
        run:
          cmd: "claude -p 'post final summary' --output-format stream-json --max-turns 5"

  # Shorthand: empty entry (no body, no when.*) → unconditional immediate worktree
  # + session_tempdir delete with no cycle. Place last so earlier guarded
  # cleanups win first-match.
  - {}

on_failure:
  - when:
      kind: { in: [unparseable, schema_drift] }
    tasks:
      - id: postmortem
        run:
          cmd: "claude -p '/post-mortem {{ failure.failed_cycle_id }}'"
```

#### Per-repo `WORKFLOW.yaml` (optional)

When `[[admission.repos]] workflow: <path>` is set, that file replaces this repo's `rules:` / `cleanup:` / `on_failure:` lists entirely. The top-level admission stays in WORKFLOW.yaml; the per-repo file inherits nothing else from the top-level rule set. Operators that want shared rules across repos either keep a single WORKFLOW.yaml (using `when.repo` matchers to dispatch) or duplicate the shared entries into each per-repo file.

#### Condition vocabulary (MVP)

Each entry inside the lists uses `when.<field>` keys; all `when.*` keys within an entry AND together. OR is expressed by writing additional entries.

| Operator | Form | Meaning |
|---|---|---|
| Equality | `when.<field>: "<scalar>"` | Field equals the scalar |
| Negation | `when.<field>.not: "<scalar>"` | Field does not equal the scalar |
| Set membership | `when.<field>.in: ["<a>", "<b>"]` | Field is in the set |
| List has-all | `when.labels.has_all: [...]` | Every entry is present in the ticket's labels |
| List has-any | `when.labels.has_any: [...]` | At least one entry is present |
| List has-none | `when.labels.has_none: [...]` | None of the entries is present |
| String regex | `when.title.regex: "..."` | (admission.repos only) Linear ticket title matches the regex |
| String prefix | `when.title.starts_with: "..."` | (admission.repos only) |
| String contains | `when.title.contains: "..."` / `when.body.contains: "..."` | (admission.repos only) |

Equality (`when.<field>`), set membership (`when.<field>.in`), and negation (`when.<field>.not`) are mutually exclusive on the same field within a single entry; declaring more than one is a config-load error. To express a multi-value match, use a single `.in` array; to express the complement, use a single `.not`.

Recognized fields:

- `status` — Linear state name.
- `labels` — Linear label list.
- `assignee` — Linear assignee (rule-level only; `[admission].assignee` does the coarse filter).
- `repo` — admission-resolved ghq path (rule-level only; admission resolves it before rule evaluation).
- `kind` — failure kind (`on_failure` entries only).
- `phase` — state id that emitted the failure (`on_failure` entries only). Every routed failure carries a state id.
- `title`, `body` — Linear ticket strings (admission.repos only, used for repo discrimination).

#### State body specification

Each rule entry declares a state machine via either the `tasks:` sugar form (linear chain with default-chained `on_done` edges) or the canonical `start:` / `states:` / `terminals:` form. Cleanup immediate-delete shorthand: a `cleanup:` entry with no body and no `when.*` keys deletes synchronously without a cycle. The full state-level field list (`run:`, `uses:`, `directives:`, `on_done:`, `on_fail:`, `if:`, `timeout:`, `max_visits:`) lives in [`ref:config §State body fields`](../reference/config.md).

Every state declares exactly one of `run:` / `uses:` (mutually exclusive):

- `run.cmd: <inline cmd>` — inline shell command form. Liquid-rendered, then spawned via `sh -c` (POSIX) / `cmd /C` (Windows).
- `uses: <path>` — file form. Path resolution per [`ref:config §Path resolution`](../reference/config.md). The file's frontmatter (`cli`, `stall_seconds`) overrides `roki.toml [default.ai].cli` / `stall_seconds` per file. Body is a Liquid template.

Every state is command-shape: each visit spawns a fresh subprocess. There is no long-lived AI session shared across states or visits. Operators relying on Claude / Codex conversational continuity drive it inside a single state's process (e.g. one stream-json invocation that holds the conversation).

### `workflow/*.md`

Each file referenced from a state's `uses:` field has YAML frontmatter and a Liquid body:

```yaml
---
cli: ""                # optional override; falls back to roki.toml [default.ai].cli
stall_seconds: 600     # optional override of [default.ai].stall_seconds
---
{Liquid body}
```

`cli: ""` (empty string) is treated as "not set" and falls back to the daemon default. The Liquid body and the cli line are both rendered against the variables documented in [01-engine-model §Inter-state data flow](01-engine-model.md). The daemon delivers the rendered output to the subprocess on three fixed channels (full mechanics in [04-state-execution §Input channels](04-state-execution.md)):

- **argv** — the rendered cli line.
- **environment variables** — `ROKI_*` scalars from the data-flow table, plus `ROKI_DIRECTIVE_PATH` pointing at the per-visit sentinel file.
- **stdin** — the rendered Liquid body for `uses:` states. Inline `run:` shell commands receive nothing on stdin by default.

### Hot reload and validation

- **Schema validation failure at startup** → refuse to start + log the offending key path.
- **Validation passes on hot reload** → apply the new policy from the next webhook. In-flight cycles keep their pre-reload policy until they terminate; once a cycle terminates, the daemon evaluates the post-reload policy on the next diff for that ticket.
- **Validation fails on hot reload** → keep the previous policy + log the failure (the daemon does not stop).
- **Per-key invalidity inside a single entry** → that entry is rejected as if it had not matched; other entries continue to apply. The structured log records the offending entry.
- **`workflow/*.md` change** is treated identically to a `WORKFLOW.yaml` change for the purposes of hot reload.

## Capabilities

- **Three files, three responsibilities**: `roki.toml` for restart-time concerns, `WORKFLOW.yaml` for the dispatch tables, `workflow/*.md` for state bodies. Each file's hot-reload behavior matches its contents.
- **One daemon for multiple repos**: a single developer runs a single daemon. The `[admission].assignee` filter ensures the daemon does not touch other people's tickets; the `[[admission.repos]]` matchers dispatch each ticket to the correct repo.
- **Operator-defined label gating**: there are no fixed label names. Operators encode whatever labels they want inside `rules:` / `cleanup:` `when.labels.*` clauses.
- **Per-repo workflow split (optional)**: operators with multiple repos can keep a top-level WORKFLOW.yaml plus per-repo files via `[[admission.repos]] workflow: ...`.
- **Defaulted-key logging**: when an unspecified key falls back to its default, the startup log records which key did so.
- **Hot-reload safe**: invalid values do not crash the daemon (the previous policy is retained).
- **Engine-agnostic CLI line**: `[default.ai].cli` accepts any cli that speaks the operator's chosen protocol (exit-code + sentinel-file directive). Operators can switch between claude, codex, or any equivalent without touching the daemon.

## Boundaries

- **Hot reload of `roki.toml`** is out of scope (only WORKFLOW.yaml + workflow/*.md are hot-reloadable).
- **Per-issue / per-attempt config overrides** are out of scope.
- **A daemon-managed canonical label set** is out of scope. Operators choose their own label conventions.
- **Environment-variable / CLI configuration overrides** are limited to a few values exposed on the CLI (`--bind`, `--port`, `--config`); a full override surface is not provided (see [cli reference](../reference/cli.md) for details).
- **Conditional includes / partial templates inside `WORKFLOW.yaml`** are out of scope for MVP.
- **Daemon-known state template names** are out of scope. Operators name their state files however they like under `workflow/`.

## Traceability

- **Roadmap**: `roadmap.md` > Scope > In > "WORKFLOW loader (hot reload)" and Boundary Strategy > "Shared seams to watch" > "WORKFLOW schema".
- **Requirements**:
  - `roki-mvp Req 2`: Configuration, Assignee Admission, and Multi-Repo Allowlist.
  - `roki-mvp Req 6`: Workspace-Level WORKFLOW Policy Loader.
  - `roki-observability Req 1`, `Req 7`, `Req 15`: Server config gating.
- **Related reference**: [config.md](../reference/config.md), [cli.md](../reference/cli.md).
- **Related FR**: [07-recovery](07-recovery.md) (admission filter and diff cache consume `[admission]`), [01-engine-model](01-engine-model.md) (the rule / cleanup / on_failure lists this file populates).
