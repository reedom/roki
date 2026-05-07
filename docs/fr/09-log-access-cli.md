---
refs:
  id: fr:09-log-access-cli
  kind: fr
  title: "Log Access CLIs"
  spec: roki-cli-log
  related:
    - fr:08-observability-logs
    - fr:10-http-api
    - fr:01-engine-model
    - fr:05-worktree-and-session
---

# FR 09: Log Access CLIs

> Three small CLIs (`roki log`, `roki events`, `roki repo`) that operators (and operator-authored phases) call to inspect per-ticket subprocess captures, the structured event stream, and per-ticket repo paths. The CLIs encapsulate the daemon's storage layout so the on-disk format can change without breaking workflow scripts.

## Purpose

The cycle engine ([01-engine-model](01-engine-model.md)) writes three kinds of artifact: per-iter subprocess captures, structured event log entries, and per-ticket worktree state. Workflow templates and operator tooling read all three. Exposing the on-disk layout directly would freeze the storage backend forever; exposing only Linux paths in environment variables also makes the API platform-specific. These CLIs sit between operator code and the storage layer, so the daemon can switch from flat files to SQLite or to a remote store later without breaking workflow templates.

## User-visible Behavior

### `roki log` — per-ticket subprocess capture

Reads stdout, stderr, exit code, and parsed structured response for a given (ticket, cycle, iteration, phase). Default arguments come from the environment variables the daemon injects when spawning a phase, so most invocations are short.

```bash
# Defaults: --ticket $ROKI_TICKET_ID --cycle $ROKI_CYCLE_ID --iter <latest completed>
roki log --phase run --stream stdout

# Relative iter (negative = N iterations back from current)
roki log --iter -1 --phase post --stream response   # last post's parsed response.json

# Range / tail
roki log --iter -1 --phase run --stream stdout --tail 50      # last 50 lines
roki log --iter -1 --phase run --stream stderr --bytes 4096   # last 4 KiB

# Cycle metadata
roki log --list-iters    # iter ids and per-phase completion status
roki log --meta          # cycle meta: kind, trigger, started_at, ended_at

# Cross-cycle within same ticket (operator must know the cycle id)
roki log --cycle <uuid> --iter -1 --phase run --stream stdout
```

Streams: `stdout`, `stderr`, `response` (the parsed `pre.response.json` / `post.response.json` — phase-specific), `events` (the per-line `<phase>.events.jsonl` of advisory stream-json events; session-shape pre / post only), `terminal` (the parsed `run.terminal.json` from claude/codex stream-json `result` events), `exit_code` (the captured numeric exit for run).

Scope: a `roki log` invocation is bound to a single ticket. The daemon refuses cross-ticket reads (by-design isolation; an operator-authored on_failure cycle for ticket A cannot accidentally read ticket B's logs). The `--ticket` flag exists for TUI / external tooling, not for cross-ticket reads from inside a phase: when invoked without environment, the operator must supply both `--ticket` and `--cycle`.

### `roki events` — structured event stream

Reads from the daemon's structured event pipeline (the tracing-crate JSON Lines emitted per [08-observability-logs](08-observability-logs.md)). Default mode connects to the daemon's HTTP API ([10-http-api](10-http-api.md)) so live tail and ring-buffer queries work without any additional config; `--offline` reads a JSON Lines file directly when the daemon is not reachable.

```bash
# Live (HTTP API client)
roki events --tail
roki events --since "2026-05-06T12:00:00Z"
roki events --kind cycle_started
roki events --ticket ABC-123
roki events --cycle <uuid>
roki events --format human       # default: json

# Offline (file reader)
roki events --offline --file /var/log/roki/daemon.jsonl --since 2026-05-06T12:00:00Z --kind cycle_started
```

Filters compose with AND. The default output is JSON Lines (the same format as the file destination); `--format human` is a one-line-per-event reformatter for terminal use.

Scope: cross-ticket. The structured event stream covers the daemon as a whole (admission decisions, cycle starts, phase outcomes, escalations, cold-start progress).

### `roki repo` — per-ticket repo path resolution

Returns a directory the operator's run command (or external tooling) can `cd` into.

```bash
# Defaults: --ticket $ROKI_TICKET_ID --repo $ROKI_REPO
roki repo                          # worktree path if it exists, otherwise ghq base path
roki repo github.com/foo/bar       # explicit repo argument
roki repo --auto-clone             # ghq get if the ghq base does not exist
roki repo --worktree               # require worktree (exit 1 if not yet created)
```

When the requested repo matches the admission-resolved repo for the current ticket and the daemon has created the worktree (i.e. the cycle has reached at least one `pre.directive: "run"`), `roki repo` returns the worktree path. Otherwise it returns the ghq base path so pre-run inspection still has somewhere to read from. `--auto-clone` enables `ghq get` for the ghq base; the daemon never auto-clones implicitly.

Pre-run callers receive the ghq base path — the operator's main checkout — and any write operation there (`git commit`, `git checkout`, etc.) pollutes the shared clone. Treat the default `roki repo` result as **read-only** unless `--worktree` confirms a per-ticket worktree has been materialized. Pre / pre-cycle templates that need to write should either return `directive: "run"` first (so the daemon materializes the worktree), or pass `--worktree` and handle the exit-1 case explicitly.

### Storage layout (current implementation)

The CLIs encapsulate this layout. Operators that need raw file access for debugging can find captures under the session root:

```
<session_root>/<ticket-id>/
  cycle-<uuid>/
    meta.json
    iter-001/
      pre.stdout
      pre.stderr
      pre.events.jsonl       (session-shape pre only)
      pre.response.json
      run.stdout
      run.stderr
      run.exit_code
      run.terminal.json      (when run cli emits stream-json `result`)
      post.stdout
      post.stderr
      post.events.jsonl      (session-shape post only)
      post.response.json
    iter-002/
      ...
```

`<phase>.events.jsonl` carries one parseable JSON object per line — the advisory stream-json events (thinking blocks, tool-use messages, etc.) emitted by the long-lived AI between turns. It is present only for session-shape pre / post phases ([04-phase-execution §Event handling](04-phase-execution.md)).

`meta.json` is the per-cycle summary file (cycle id, kind, trigger, started_at, ended_at, terminal directive or failure kind). Schema is defined in [`docs/reference/artifacts.md`](../reference/artifacts.md).

The structured event log destination is set in `roki.toml [log]` (stdout / file / both). The HTTP API mirrors the live ring buffer ([10-http-api §Endpoints](10-http-api.md)).

This layout is **not** part of the operator-facing contract. Future versions may move some files into a SQLite database, compress old iters, or delegate to a remote store; only the CLIs are stable.

## Capabilities

- **One-line accessors for the common case**: env-var defaults make `roki log --phase run --stream stdout` a useful shorthand inside any workflow template.
- **Relative iteration arithmetic**: `--iter -N` for "N iters back" lets a post template inspect the previous run trivially.
- **Same-ticket isolation**: phase subprocesses cannot accidentally read a different ticket's captures.
- **Cross-cycle access within a ticket**: an `[[on_failure]]` cycle reads the failed cycle's logs by passing `--cycle <failed_cycle_id>`.
- **Online + offline event reads**: `roki events` works against a live daemon (HTTP API) or against an archived JSON Lines file (`--offline`).
- **Auto-clone helper**: `roki repo --auto-clone` keeps workflow scripts from having to call `ghq get` directly.
- **Storage backend opacity**: the on-disk file layout is intentionally not part of the operator contract; the daemon may switch backends without breaking workflows.

## Boundaries

- **Cross-ticket reads from inside a phase** are not supported. The TUI and external scripts may pass `--ticket` explicitly, but a workflow template launched by the daemon for ticket A cannot read ticket B's captures via these CLIs.
- **Mutating the captures** is out of scope. The CLIs are read-only.
- **Streaming protocols** (WebSocket, SSE) are deferred for `roki events`. MVP supports `--tail` over HTTP polling; richer push is post-MVP.
- **Indexed search** (full-text, structured query DSL) is out of scope. MVP filters are equality / range / kind / ticket / cycle.
- **Log retention / rotation** is the responsibility of external tools for the file destination ([08-observability-logs §Boundaries](08-observability-logs.md)). Per-ticket captures under the session root are deleted when the ticket is evicted (cleanup cycle, admission failure, or orphan reconcile).
- **Authentication and authorization** for the HTTP API are governed by [10-http-api](10-http-api.md); these CLIs do not introduce a separate auth path.

## Traceability

- **Roadmap**: `roadmap.md` > Boundary Strategy > "Shared seams to watch".
- **Requirements**: pending — to be added in the requirements rewrite that follows the FR rewrite.
- **Related FR**: [08-observability-logs](08-observability-logs.md), [10-http-api](10-http-api.md), [01-engine-model](01-engine-model.md), [05-worktree-and-session](05-worktree-and-session.md).
