# Daemon Bootstrap Design — Task 5.1

Status: PROPOSAL — needs user sign-off before task 5.1 is opened.

Goal: make `roki run` actually run. Today it loads logging, installs signal handlers, waits for shutdown — the orchestrator, tracker, and webhook receiver are all built but never instantiated by the binary.

## Decision matrix

| # | Decision | Options | Recommendation | Why |
|---|---|---|---|---|
| 1 | Config file location | A. `--config <path>` required<br>B. `./roki.toml` default + `--config` override<br>C. `~/.config/roki/config.toml` default + `--config` override | **B** | Project-local default matches the multi-repo, per-checkout-of-roki shape. `~/.config` would be a one-daemon-per-developer assumption; we already have that, but the project-local default lets you run multiple daemons on one box (one per work tree) without env juggling. |
| 2 | Webhook secret config | A. one shared secret<br>B. per-repo secret in `RepoConfig.webhook_secret` (env-overridable like `linear_token`)<br>C. no webhook receiver in MVP — polling only | **B** | Linear gives you a separate webhook signing secret per subscription, and `WebhookState` is already per-repo (`repo: RepoId` baked in). One subscription per repo is the natural shape. Env override: `ROKI_WEBHOOK_SECRET_<REPO_ID>` so secrets stay out of disk config. |
| 3 | Webhook routing path | A. one shared `/linear/webhook` + dispatch by team key in payload<br>B. one path per repo: `/linear/webhook/<repo-id>` | **B** | Matches the per-repo `WebhookState` shape that already exists. Lets ngrok point at exactly one repo's URL when testing. Cleaner failure mode (404 vs malformed dispatch). |
| 4 | HTTP bind address + port | A. CLI flags `--bind <addr> --port <num>`<br>B. config file `[server]` section<br>C. both, CLI overrides config | **C** | CLI for ad-hoc development (ngrok, port-changes); config file for production. Defaults: `127.0.0.1:7878` (loopback only — operator opts into wider exposure explicitly). |
| 5 | Per-repo trackers vs single multiplexed tracker | A. `N` `LinearTracker` instances (one per repo scope), each with its own poll loop<br>B. one tracker, one Linear connection, fan out via `route_issue` | **A** | Matches existing `LinearTracker` API (per-scope construction). Defers the `route_issue` wiring decision (the unresolved follow-up from 4.4) until there's a real need. Polling cadence is global; each tracker honors it independently. |
| 6 | `WorkflowPolicy → EnginePolicy` resolution | A. close it now (parse `WORKFLOW.md`, resolve `max_attempts` / `max_turns` / `stall_window` / `backoff` into `EnginePolicy` per worker launch)<br>B. defer | **A** | Without this, the `max_attempts` knob shipped in 3.7 has no runtime effect from a real `WORKFLOW.md`. Closing it now is small and removes a tracked follow-up. |
| 7 | What runs on `roki run` | A. config + logging + shutdown + workflow loader + per-repo trackers + webhook server + orchestrator + claude engine<br>B. just the orchestrator + claude engine (tracker + webhook in a follow-up) | **A** | The whole point of 5.1 is "the daemon actually runs." Half-wiring would just leave a different gap for tomorrow. |
| 8 | `claude` binary discovery | A. `$PATH` resolution<br>B. config-file override `claude_binary = "/path/to/claude"`<br>C. env var `ROKI_CLAUDE_BINARY` | **A + B** (default to `which claude`, allow config override) | Most users have `claude` on PATH; the override exists for testing (`fake_claude` already shows the pattern). |
| 9 | Smoke test for the bootstrap | A. inline e2e test that drives `runtime::run` with a real config + wiremock Linear + fake_claude binary + an HTTP client that posts a signed webhook<br>B. manual instructions + skip the test | **A** | Establishes the full-stack contract; matches 4.x e2e-test convention. Test runs in <5s with sub-second backoff and a single fake_claude invocation. |

## Bootstrap shape (the new function)

`runtime::run(args)` becomes (roughly):

```
1. load_config(args.config)                              -> Config
2. install logging (already done)
3. install signal handlers (already done)
4. WorkflowLoader::start(repo.workflow_path)             -> WorkflowPolicy + watcher (per repo)
5. WorkspaceManager::new(config.workspace_root)
6. PermissionResolver::new(config.permission_strategy)
7. ClaudeEngineAdapter::with_binary(claude_binary)
8. Orchestrator::new(workspace, engine_launcher, event_bus, ...)
       .with_engine_policy(EnginePolicy::from_workflow(policy))
9. for each RepoConfig:
       a. LinearTracker::new(scope, token, cadence)            (poll task)
       b. WebhookState::new(secret, repo_id, scope_fallback, sink)
       c. mount router at /linear/webhook/<repo_id>
       d. tracker_bridge.bind(repo_id, tracker_rx)
10. axum::serve(TcpListener::bind(server.addr))                (single server, all repos mounted)
11. tokio::select on shutdown across orchestrator + trackers + axum + bridge
12. on shutdown: orchestrator.shutdown() -> wait workers up to SHUTDOWN_WINDOW
```

Each numbered step already exists as a building block; 5.1 is composition + the missing config keys.

## Schema delta (additive)

Config root gains:

```toml
# roki.toml
workspace_root = "/Users/me/var/roki/workspaces"
polling_cadence_seconds = 300
max_concurrent_workers = 4

[server]
bind = "127.0.0.1"        # default loopback
port = 7878               # default

[permission_strategy]
mode = "allowlist"        # or "dangerously_skip_permissions"

[[repos]]
id = "core"
path = "/Users/me/src/core"
scope = { kind = "team", key = "ENG" }
workflow_path = "/Users/me/src/core/WORKFLOW.md"
webhook_secret_env = "ROKI_WEBHOOK_SECRET_CORE"   # NEW: env var holding the HMAC secret
# OR webhook_secret = "literal"                    # discouraged; flagged on load

# claude_binary is optional; defaults to `which claude`
# claude_binary = "/Users/me/.local/bin/claude"
```

CLI flags added to `RunArgs`:

```
roki run [--config <path>] [--bind <addr>] [--port <num>] [--dangerously-skip-permissions]
```

The `--dangerously-skip-permissions` flag overrides `[permission_strategy].mode` and emits a `WARN` log on every worker launch.

## Touch list

- `crates/roki-daemon/src/config/mod.rs` — add `[server]` section, per-repo `webhook_secret_env`/`webhook_secret`, optional `claude_binary` resolution.
- `crates/roki-daemon/src/cli.rs` — extend `RunArgs` with `--config`, `--bind`, `--port`, `--dangerously-skip-permissions`.
- `crates/roki-daemon/src/runtime.rs` — replace stub `run` with the orchestrated bootstrap above. Wire `JoinSet` for background tasks; `tokio::select!` on shutdown.
- `crates/roki-daemon/src/engine/policy.rs` — add `EnginePolicy::from_workflow(&WorkflowPolicy)` constructor closing decision #6.
- `crates/roki-daemon/src/orchestrator/core.rs` — accept `EnginePolicy` from bootstrap (already does via `with_engine_policy`).
- `crates/roki-daemon/tests/e2e_bootstrap.rs` — new smoke test driving `runtime::run` end-to-end (decision #9).
- `SPEC.md` — §3.2 (config schema row for `[server]` and `webhook_secret_env`), §17 enumerated extension points (no change), §9 startup sequence (new short subsection describing the bootstrap order).
- `design.md` — fold the bootstrap composition into the architecture diagram prose.

## Open questions for you

The decisions with real semantic choice are **#1, #2, #3, #4** — everything else is mechanical. If you want different defaults (e.g., `~/.config/roki/` over `./roki.toml`, or shared `/linear/webhook` over per-repo paths), tell me and I'll update the doc before opening the task.

After sign-off I'll open task 5.1 with this scope, dispatch the implementer, and review same as 3.7.
