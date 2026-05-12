# Slice 11 — `roki log` / `roki events` / `roki repo` Design

Date: 2026-05-11
Scope: Implement `fr:09` end to end inside the `roki` binary. Add three subcommands (`log`, `events`, `repo`), the env / config plumbing they need, and patch the fr:09 reference text that drifted from the runtime conventions established in slices 1–10.

## 1. Position in the Roadmap

Slice 11 closes:

- `roki-cli-log` — `roki log` over the on-disk per-ticket capture layout. All streams (`stdout`, `stderr`, `events`, `terminal`, `directive`, `exit_code`), env-var defaults, absolute and relative `--iter`, `--tail <n>` lines / `--bytes <n>` byte tail, `--list-visits`, `--meta`, `--follow`, and same-ticket isolation per fr:09 §`roki log`.
- `roki-cli-events` — `roki events` HTTP API client. Live `--tail`, `--since <seq|rfc3339>`, `--kind`, `--ticket`, `--cycle`, `--format json|human`, with `--offline --file <path>` falling back to a JSON Lines file. Sanitization (ANSI / control strip) on human-format string fields per fr:10 §Sanitization.
- `roki-cli-repo` — `roki repo` returning the worktree path when materialized, ghq base otherwise. `--worktree`, `--auto-clone`, env-var defaults per fr:09 §`roki repo`.
- `roki-cli-env-glue` — Three runtime env hooks the CLIs need:
  1. `paths.session_root` is added to `globals.config`, so the existing scalar flattener exports `ROKI_CONFIG_SESSION_ROOT` to every state subprocess.
  2. When `[api].port` is set, the daemon exports `ROKI_API_URL=http://<bind>:<port>` to every state subprocess.
  3. fr:09 text is patched so the documented env defaults match runtime reality (`$ROKI_REPO_GHQ`, `cycle.json`).

Slices 1–10 provide: cycle engine, persistent daemon + diff cache, per-cycle `cycle.json`, per-visit captures (`<state_id>.stdout|.stderr|.events.jsonl|.terminal.json|.directive.json|.exit_code`), structured event writer + in-memory ring, observability HTTP API (`/api/events`, `/api/tickets`, ...), and the ratatui `roki-tui` that already consumes the same API.

Out of scope, deferred:

- **Server-side timestamp range on `/api/events`** (`since=<RFC3339>`). v1 keeps `since=<seq>` (fr:10 §Endpoints); `roki events --since <rfc3339>` resolves the cutoff client-side after `since=0`.
- **WebSocket / SSE push.** fr:09 §Boundaries; fr:10 §Boundaries.
- **Indexed search / DSL.** fr:09 §Boundaries.
- **Mutating CLIs.** fr:09 §Boundaries (CLIs are read-only).
- **Cross-ticket reads from inside a state subprocess.** fr:09 §Boundaries (the daemon refuses).
- **Windows support.** The daemon binary is Unix-only; the new subcommands inherit that posture.

---

## 2. Architecture

### 2.1 Module layout

The CLIs ship in the existing `[[bin]] name = "roki"` (`crates/roki-daemon/Cargo.toml`). The current single-file `src/cli.rs` is split into a module directory so each subcommand sits in its own file:

```
crates/roki-daemon/src/cli/
├── mod.rs                 // Parser, run() dispatcher (moves from src/cli.rs)
├── workflow.rs            // existing `workflow validate|graph`
├── log.rs                 // `roki log`
├── events.rs              // `roki events`
├── repo.rs                // `roki repo`
└── shared/
    ├── mod.rs
    ├── config_resolve.rs  // --config / env fallback → RokiConfig fragment
    ├── visit_lookup.rs    // absolute / relative iter resolution (cycle-wide)
    └── tail.rs            // line-tail + byte-tail readers
```

Storage projections in `crates/roki-daemon/src/api/projection/{cycles,visits,events}.rs` are reused by both the HTTP API and the new CLIs. `cli::log` uses `api::projection::visits` for visit-dir path math; `cli::events --offline` uses the same JSON Lines schema (`roki-api-types::ApiEvent`).

### 2.2 Dependencies

- `reqwest` (already in the workspace; used by `roki-tui`) is added to `roki-daemon` for `roki events --tail` HTTP polling.
- No new top-level crates. No ratatui / crossterm in this slice.

### 2.3 Async runtime

`#[tokio::main]` is already in place for the daemon. `roki events --tail` runs a single async polling loop; `roki log --follow` uses `tokio::fs` + `tokio::time::interval`. Daemon-mode subcommands (`run`, `cleanup`) are unaffected.

### 2.4 Failure-mode budget

- CLI errors: exit 1, message on stderr, no panic. Bad clap input: exit 2 (clap default).
- Network failures (`roki events`): no retry. The operator's shell decides whether to re-invoke.
- File-system failures (`roki log`, `roki repo`): bubble the underlying error with the offending path.
- `roki log --follow` survives transient `NotFound` (the capture file is still being created) by re-polling; SIGINT exits 0.

---

## 3. Env / config injection changes inside the daemon

Three small daemon-side changes precede the new subcommand modules, because the CLIs assume these env vars.

### 3.1 `config.session_root` joined to `globals.config`

`crates/roki-daemon/src/daemon/real_runner.rs::build_cycle_context` already populates `globals.config = { "max_iterations": ... }`. Extend the inserted object so the path is present whenever the daemon spawns a state:

```rust
globals.insert(
    "config".into(),
    serde_json::json!({
        "max_iterations": cfg.engine.max_iterations,
        "session_root": cfg.paths.session_root.to_string_lossy(),
    }),
);
```

The existing flattener (`real_state_runner::push_namespace_scalars`) treats this entry as a scalar string and emits `ROKI_CONFIG_SESSION_ROOT=<path>` automatically. No change in the runner.

### 3.2 `ROKI_API_URL` injection when `[api].port` is set

The same `build_cycle_context` site appends an `api_url` entry to `globals.config` when, and only when, the API server is configured:

```rust
if let Some(port) = cfg.api.port {
    let bind = cfg.api.bind.unwrap_or_else(|| "127.0.0.1".parse().unwrap());
    let url = format!("http://{bind}:{port}");
    if let Some(Value::Object(m)) = globals.get_mut("config") {
        m.insert("api_url".into(), Value::String(url));
    }
}
```

Subprocesses see `ROKI_API_URL=http://127.0.0.1:<port>` (or whatever `[api].bind` resolves to) when the daemon is running with the API enabled, and never see the key when it is not.

### 3.3 fr:09 reference fixes

`docs/fr/09-log-access-cli.md` is patched in the same slice. The patches are documentation only:

- `--repo $ROKI_REPO` → `--repo $ROKI_REPO_GHQ`. The runtime convention `ROKI_<NS>_<KEY>` (fr:01-engine-model; `crates/roki-daemon/src/engine/real_state_runner.rs` `push_namespace_scalars`) has been canon since slice 1; there is no bare `ROKI_REPO`.
- Storage-layout `meta.json` → `cycle.json` (the actual filename written by `crates/roki-daemon/src/daemon/cycle_metadata.rs`).
- `--meta` description: "read the cycle's `cycle.json`".
- `roki log` defaults paragraph: note `$ROKI_CONFIG_SESSION_ROOT` as the env source and `--config <PATH>` as the fallback for external callers.
- `roki events` defaults paragraph: note `$ROKI_API_URL` as the env source and `--api <URL>` as the override; explicit "no API URL resolved" error when neither is present and `--config` does not supply `[api]`.

Wording is the only change. No FR behavior moves.

---

## 4. `roki log`

### 4.1 Surface

```
roki log [OPTIONS]
roki log --list-visits [OPTIONS]
roki log --meta [OPTIONS]
```

| Flag | Type | Default | Notes |
|---|---|---|---|
| `--ticket <id>` | string | `$ROKI_TICKET_ID` | Required when env unset. |
| `--cycle <uuid>` | string | `$ROKI_CYCLE_ID` | Required when env unset. |
| `--state <state_id>` | string | required for stream reads | Operator-declared state id from `WORKFLOW.yaml`. |
| `--iter <n>` | i32 | latest completed visit | Absolute `>0` or relative `-N` (N visits back from latest). |
| `--stream <kind>` | enum | required for stream reads | `stdout` / `stderr` / `events` / `terminal` / `directive` / `exit_code`. |
| `--tail <n>` | usize | unset | Last N lines (line-oriented streams). Conflicts with `--bytes`. |
| `--bytes <n>` | usize | unset | Last N bytes. Conflicts with `--tail`. |
| `--list-visits` | flag | — | Emit per-visit JSON Lines `{visit_n, state_id, exit_code}`. |
| `--meta` | flag | — | Emit `cycle.json` content verbatim. |
| `--follow` | flag | — | Continue tailing `stdout` / `stderr` after EOF; polls every 200 ms (hidden `--follow-poll-ms` for tests). |
| `--config <PATH>` | path | `$ROKI_CONFIG_PATH` or absent | Required when `$ROKI_CONFIG_SESSION_ROOT` is unset. |

### 4.2 Behavior

1. Resolve `session_root`: `$ROKI_CONFIG_SESSION_ROOT` → `--config`-loaded TOML's `paths.session_root` → error `roki log: cannot resolve session_root (set --config or run from a state subprocess)`.
2. Resolve `ticket` and `cycle`. Outside a state subprocess, both flags are required. Inside a state subprocess, both env vars are present and `--ticket` may only be passed when it equals `$ROKI_TICKET_ID` (cross-ticket guard, fr:09 §Scope).
3. Resolve `--iter`:
   - Enumerate `visit-NNN` directories under `<session_root>/<ticket>/cycle-<cycle>/` (lexicographic = numeric since the daemon writes fixed-width `visit-001`, `visit-002`, ...).
   - Absolute `n > 0` → `visit-{n:03}`; exit 1 if missing.
   - Relative `-N` → `dirs.len() - N` (1-indexed; `-1` = previous). Off-the-start → exit 1.
   - Unset → highest numbered `visit-NNN`. When `--state` is set, prefer the highest visit that has `<state_id>.exit_code` (the visit finished).
4. Stream read:
   - `stdout` / `stderr` → raw bytes from `<state_id>.stdout` / `<state_id>.stderr`.
   - `events` → `<state_id>.events.jsonl`.
   - `terminal` → `<state_id>.terminal.json`.
   - `directive` → `<state_id>.directive.json`.
   - `exit_code` → `<state_id>.exit_code` (a single line of decimal digits).
   - All output is byte-for-byte; no ANSI stripping (fr:08 §Tier 2 forbids transformation).
   - `--tail N`: open, seek to end, scan back to the N-th newline (or BOF if shorter), write the suffix.
   - `--bytes N`: seek `max(0, len - N)`, write the suffix.
   - `--follow` (only with `stdout` / `stderr`): after the initial write, open with an offset cursor, poll-read every 200 ms, write any new bytes, until SIGINT.
5. `--list-visits`: scan visit dirs, sort ascending, emit JSON Lines:
   ```json
   {"visit_n":1,"state_id":"impl","exit_code":0}
   {"visit_n":2,"state_id":"verdict"}
   ```
   `exit_code` is omitted when the per-state file is missing (the visit is still in flight). `state_id` is read from `<visit-NNN>/<state_id>.exit_code` filename (only one state per visit, fr:01 §Cycle iteration).
6. `--meta`: open `<session_root>/<ticket>/cycle-<cycle>/cycle.json`, write to stdout.
7. Cross-ticket refusal: when `$ROKI_TICKET_ID` is set and `--ticket` is passed with a mismatched value, exit 2 with `roki log: cross-ticket read refused`.

### 4.3 Errors

- Missing files / non-existent visit: exit 1 with stderr `roki log: <path> not found`.
- Cross-ticket attempt from inside a state: exit 2.
- Both `--tail` and `--bytes`: clap conflict (exit 2).

---

## 5. `roki events`

### 5.1 Surface

```
roki events [--tail] [--since <S>] [--kind <K>] [--ticket <T>] [--cycle <U>]
            [--format json|human] [--api <URL>] [--config <PATH>]
roki events --offline --file <PATH> [filters...]
```

| Flag | Default | Notes |
|---|---|---|
| `--tail` | unset | Continuous polling loop until SIGINT. |
| `--since <S>` | unset | `<u64>` → server-side cursor. RFC3339 timestamp → client-side filter after server `since=0`. |
| `--kind <K>` | unset | Filter on `event` (one value; AND with the rest). |
| `--ticket <T>` | `$ROKI_TICKET_ID` (online) | Forwarded to `/api/events?ticket=`. |
| `--cycle <U>` | `$ROKI_CYCLE_ID` (online) | Forwarded to `/api/events?cycle=`. |
| `--format` | `json` | `human` = one-line text reformatter. |
| `--api <URL>` | `$ROKI_API_URL` else `--config`-derived else error | HTTP base URL. |
| `--config <PATH>` | `$ROKI_CONFIG_PATH` | Used to synthesize `--api` from `[api]` when env unset. |
| `--offline --file <P>` | — | Read JSONL file directly. Ignores `--api` / `--config`. |
| `--cadence-ms <N>` | 1000 | `--tail` polling cadence. Hidden flag (testability). |

### 5.2 Online mode

1. Resolve base URL: `--api` → `$ROKI_API_URL` → `http://<[api].bind>:<[api].port>` from `--config` → exit 1 with `roki events: cannot resolve API URL`.
2. Initial fetch: `GET /api/events?since=<S>&...`. Cursor resolution:
   - `--since=<u64>` → that value.
   - `--since=<rfc3339>` → `0`; events with `ts < target` are dropped client-side.
   - Unset with `--tail` → fetch `/api/events?since=0&limit=1`, capture `next_since` without printing; subsequent polls use that cursor.
   - Unset without `--tail` → `0` (dump the full ring once, exit).
3. If response `gap == true`: emit `# roki events: ring gap detected; consult [log].file_path` to stderr, then continue with `next_since`.
4. Print events (filtered) in `--format`. Sanitization on string payload fields in `--format human` (re-strip on the client side: defense-in-depth per fr:10 §Sanitization).
5. If `--tail`: `tokio::time::sleep(cadence_ms)`, loop with `since=next_since`.
6. SIGINT → exit 0. HTTP non-2xx → exit 1, body on stderr. Connection refused → exit 1.

### 5.3 Offline mode

- `--file` is required; if absent, attempt `[log].file_path` from `--config` and exit 1 if also unresolved.
- Parse line-by-line as `roki-api-types::ApiEvent`. Apply all filters in process. Honor `--format`.
- `--tail` against a file is rejected in v1 (`roki events: --tail not supported with --offline`); operators use `tail -f` plus a separate filter.
- Malformed line → stderr warn, skip. File open error → exit 1.

### 5.4 `--format human`

Fixed-shape line per event:

```
<seq>  <ts>  <event>  ticket=<id|->  cycle=<short_uuid|->  <k>=<v>...
```

`<k>=<v>` pairs are scalar fields from `payload` (string / number / bool); object / array fields are omitted. String values pass through the ANSI / control-char stripper. `<short_uuid>` is the first eight hex characters of `cycle_id`.

### 5.5 Filter composition

All `--kind` / `--ticket` / `--cycle` / `--since` filters AND together (matches fr:09 §`roki events` and the server's `EventsQuery`).

---

## 6. `roki repo`

### 6.1 Surface

```
roki repo [<ghq>] [--ticket <id>] [--worktree] [--auto-clone] [--config <PATH>]
```

| Arg / Flag | Default | Notes |
|---|---|---|
| `<ghq>` positional | `$ROKI_REPO_GHQ` | E.g. `github.com/foo/bar`. |
| `--ticket <id>` | `$ROKI_TICKET_ID` | Needed only when worktree resolution is attempted. |
| `--worktree` | flag | Require worktree (exit 1 if absent). |
| `--auto-clone` | flag | Run `ghq get <ghq>` before resolving the ghq base. |
| `--config <PATH>` | `$ROKI_CONFIG_PATH` | Currently optional (the ghq + worktree lookup is config-free). |

### 6.2 Behavior

1. Resolve `ghq`. If unset and `$ROKI_REPO_GHQ` is empty → exit 2 with `roki repo: ghq slug required`.
2. If `--auto-clone`: spawn `ghq get <ghq>`. Non-zero exit → exit 1 with the captured stderr.
3. Call `engine::cwd::resolve(ghq, ticket)` (worktree-first, ghq-base fallback). The function is already implemented; the slice promotes its visibility from "`#[allow(dead_code)]`-tolerated" to a documented seam.
4. `--worktree`: when `engine::worktree::exists` returns `None`, exit 1 with `roki repo: worktree not yet materialized`.
5. Print the resolved path to stdout, newline-terminated.

### 6.3 Reuse seam

`engine::cwd::resolve` and `engine::cwd::resolve_ghq_base` become `pub` consumers of the cli module. No behavior change; the `#[allow(dead_code)]` attribute on the module is removed once the cli module wires the call.

---

## 7. Sanitization

- `roki log` outputs raw capture bytes. No stripping (fr:08 §Tier 2).
- `roki events --format json` echoes the server payload verbatim (the API already strips per fr:10 §Sanitization).
- `roki events --format human` re-strips ANSI / control chars on string payload scalars. Defense in depth for the offline path, where the API server's strip has not run.
- The ANSI / control strip is centralized in a `cli::shared::sanitize` helper (or, if a suitable helper already lives in `crates/roki-daemon/src/api/sanitize.rs`, that helper is published into `roki-api-types` and used by both `roki-tui` and the new CLI).

---

## 8. Configuration touch points

- `paths.session_root` (already required) is consumed by `cli::log`.
- `[api].bind` / `[api].port` (already present) are consumed by `cli::events` when env is unset.
- No new TOML keys are introduced.

---

## 9. Logging from the CLIs

CLI subcommands are clients, not the daemon. They do not emit structured events; errors go to stderr only. The daemon's event log is untouched by `roki log`, `roki events`, `roki repo`. (`roki events --tail` consumes the event log over HTTP; it does not write back to it.)

---

## 10. Tests

### 10.1 Unit (under `cli::log`, `cli::events`, `cli::repo`, `cli::shared`)

- `visit_lookup`: absolute, relative, "latest completed", missing visit, off-by-one at `-N` boundary.
- `tail::lines(N)`: short file, exact-N, more-than-N, file with no trailing newline.
- `tail::bytes(N)`: file shorter than N, equal, larger.
- `log::resolve_session_root`: env wins, `--config` fallback, neither → error.
- `log::cross_ticket_refusal`: env `ABC-1`, flag `XYZ-9` → exit 2.
- `events::since_cutoff`: client-side `<rfc3339>` cutoff drops strictly older events, keeps equals.
- `events::format_human`: rendering for representative event kinds (`webhook_received`, `state_completed`, `cycle_completed`, `failure_unhandled`, `escalation_added`).
- `events::resolve_api_url`: `--api` beats env beats `--config`; neither → error.
- `repo`: worktree present / worktree absent / `--worktree` strict failure, via `ROKI_WT_ROOT_OVERRIDE` + `ROKI_GHQ_BASE_OVERRIDE`.

### 10.2 Integration (`crates/roki-daemon/tests/e2e/`)

- `cli_log_smoke.rs` — spawn the daemon with a workflow that writes stdout; run `roki log --state ... --stream stdout` against the produced capture; assert byte-equality with the on-disk `visit-001/<state_id>.stdout`.
- `cli_log_follow.rs` — fixture writer task appends to a stdout file while `roki log --follow ...` runs; assert the child observes the late bytes within N polling intervals; SIGINT-exit cleanly.
- `cli_events_online_smoke.rs` — daemon with `[api].port` set, post a webhook fixture, run `roki events --tail --cadence-ms 50` for one second; assert at least the `webhook_received` line appears.
- `cli_events_offline_smoke.rs` — build a JSONL fixture, run `roki events --offline --file <p> --kind cycle_started`; assert filtered output.
- `cli_repo_smoke.rs` — fake ghq base via `ROKI_GHQ_BASE_OVERRIDE`; run `roki repo github.com/x/y`; create a worktree under `ROKI_WT_ROOT_OVERRIDE`; re-run; assert both branches.

### 10.3 Env-injection regression

- `real_state_runner::build_env`: update the existing names-contains test (`crates/roki-daemon/src/engine/real_state_runner.rs:957`) to assert `ROKI_CONFIG_SESSION_ROOT` appears, and (when the test config sets `[api].port`) `ROKI_API_URL` appears.

---

## 11. Spec impact

- `docs/fr/09-log-access-cli.md`: patch `$ROKI_REPO` → `$ROKI_REPO_GHQ`; `meta.json` → `cycle.json` (storage layout and `--meta` text); document `$ROKI_API_URL` / `--api` for `roki events`; document `$ROKI_CONFIG_SESSION_ROOT` / `--config` for `roki log`.
- `docs/fr/08-observability-logs.md`: no behavioral change. The capture-layout snippet in §Tier 2 already cross-references fr:09 §Storage layout; once fr:09 is patched, no editing is needed here.
- `docs/fr/10-http-api.md`: no change. The API surface this slice consumes is identical to slice 9's.
- `docs/reference/cli.md`: this slice owns the canonical flag tables. The file gains the `roki log` / `roki events` / `roki repo` sections defined above so the kusara graph picks up `ref:cli` claims from `modules:` in the new cli modules.
- `docs/reference/config.md`: no change (`globals.config.api_url` is internal scaffolding, not a user TOML key).

---

## 12. Risks / open verification

- **`engine::cwd` promotion.** `engine::cwd::resolve` and `resolve_ghq_base` were `#[allow(dead_code)]` outside engine internals. Promoting them to a published seam introduces no new logic, but the worktree-canonicalization branch (symlink resolution) and the ghq-not-found branch each need a test confirming the CLI receives the right exit path. Both branches are already covered in `crates/roki-daemon/src/engine/cwd.rs::tests`; the cli wrapper test confirms the outer surface.
- **`roki log --follow` and file truncation.** Capture writers in `real_state_runner` open with append, never truncate mid-cycle; the follower can keep its file handle open across writes. Confirmed against the writer code path; no separate guard required.
- **`/api/events?since=<seq>` ring overflow.** The CLI surfaces `gap: true` to stderr but otherwise continues from `next_since`. Operators inspecting historical ranges across a ring overflow are expected to use `--offline --file <log.file_path>` instead, matching fr:09 §`roki events` Boundaries.
