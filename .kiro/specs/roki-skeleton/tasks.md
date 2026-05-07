---
refs:
  id: tasks:roki-skeleton
  kind: tasks
  title: "roki-skeleton Tasks"
  spec: roki-skeleton
  implements:
    - design:roki-skeleton
  depends_on:
    - design:roki-skeleton
    - req:roki-skeleton
  related:
    - fr:01-engine-model
    - fr:02-configuration
    - fr:03-linear-admission
    - fr:04-phase-execution
    - fr:12-daemon-lifecycle
  modules:
    - crates/roki-daemon/
    - crates/roki-daemon/tests/e2e/skeleton_smoke.rs
---

# Implementation Plan

Tasks are ordered to match implementation order: foundation first, then layers in the dependency direction declared in `design.md` (`error → config → linear → admission → rule → capture → runner → runtime → cli → main → validation`). `(P)` marks sub-tasks that may run concurrently with their immediate peers because their boundaries do not overlap and their prerequisites are already complete.

- [x] 1. Foundation: reintroduce the `roki-daemon` crate so the workspace builds and downstream module work has a home
- [x] 1.1 Create the `roki-daemon` crate manifest, dependency set, and binary entry skeleton
  - Add `crates/roki-daemon/Cargo.toml` declaring the binary `roki`, edition 2024, MSRV 1.85, the workspace lints, and the dependencies named in design Technology Stack (`tokio` full, `axum`, `clap` 4 derive, `serde`, `toml`, `reqwest` rustls+json, `uuid` v4, `tracing`, `tracing-subscriber`, `anyhow`, `thiserror`).
  - Declare an optional `test-support` Cargo feature gated only over the `linear::client` env-var seam.
  - Add `crates/roki-daemon/src/main.rs` with a minimal `#[tokio::main]` entry that installs the default `tracing-subscriber` (`fmt`, stdout, `INFO` max level) as the very first action and returns `ExitCode::SUCCESS`.
  - Stub the module tree referenced by later tasks so concurrent module work does not contend for the same files: create `src/config/mod.rs` containing `pub mod roki; pub mod workflow;`, `src/linear/mod.rs` containing `pub mod ticket; pub mod client; pub mod webhook;`, plus empty `src/{config,linear}/{*}.rs` and flat `src/{error,admission,rule,capture,runner,runtime,cli}.rs` files (each compiling as `// stub`).
  - Declare the nested integration test target in the manifest: `[[test]] name = "skeleton_smoke" path = "tests/e2e/skeleton_smoke.rs"` so `cargo test --test skeleton_smoke` discovers the file under `tests/e2e/`.
  - Observable completion: `cargo build -p roki-daemon` and `cargo build -p roki-daemon --features test-support` both succeed; `cargo metadata` no longer reports the missing crate; the `[[test]]` entry is visible in `cargo metadata --format-version 1`.
  - _Requirements: 9.1_

- [x] 1.2 Define the typed error surface used across modules
  - Introduce `error.rs` with a top-level `SkeletonError` enum aggregating typed module errors (`RokiConfigError`, `WorkflowError`, `LinearClientError`, `WebhookError`, `AdmissionError`, `CaptureError`, `RunnerError`).
  - Use `thiserror` for each module variant; carry the offending file path, key path, address, or endpoint as appropriate so the `tracing::error!` log line can identify the cause.
  - Observable completion: `cargo check -p roki-daemon` compiles the error module with no warnings; the variants enumerated in design Error Categories table are all present.
  - _Requirements: 1.2, 2.3, 3.4, 4.4, 5.3, 6.2, 7.3, 8.3_

- [x] 2. Config layer: load and validate the canonical `roki.toml` and `WORKFLOW.toml` slices
- [x] 2.1 (P) Implement the `roki.toml` loader covering the six sections required by the skeleton
  - Deserialize `[linear]`, `[linear.webhook]`, `[default.ai.command]`, `[engine]`, `[paths]`, `[log]` per `ref:config`; tolerate unknown keys and accepted-without-applying keys (`[default.ai.session]`, `[linear.webhook].secret`).
  - Enforce the required-field set listed in design `config::roki` (`[linear].token`, `[linear.webhook].bind`, `[linear.webhook].port`, `[default.ai.command].cli`, `[paths].workflow`, `[paths].session_root`); on missing or type-mismatched fields, return a `RokiConfigError` whose message names the offending key path.
  - Hand-roll `Debug` for the loaded config so `[linear].token` is masked in log output.
  - Add unit tests covering: missing required field rejection, unknown / accepted-without-applying keys silently retained, `Debug` token masking.
  - Observable completion: `cargo test -p roki-daemon config::roki` passes; loading a happy-path TOML yields a populated `RokiConfig` and loading a TOML missing `[linear].token` returns a key-path-bearing error.
  - _Requirements: 2.1, 2.3, 2.4_
  - _Boundary: config::roki_
  - _Depends: 1.2_

- [x] 2.2 (P) Implement the `WORKFLOW.toml` loader covering admission, first repo, and rules with strict `when.*` / `run.*` validation
  - Deserialize `[admission]` (`assignee`), the first `[[admission.repos]]` entry's `ghq` value (or `None` when missing/empty), and the `[[rule]]` array per design `config::workflow`.
  - Reject any `[[rule]] when.*` key other than `when.status` and `when.labels.has_all` with a load-time error; reject `run.path`, `run.prompt`, missing `run`, and any `pre.*` / `post.*` block on a `[[rule]]` entry.
  - Accept the presence of `[[cleanup]]`, `[[on_failure]]`, and per-repo `[[admission.repos]] workflow` overrides as opaque values without evaluating them.
  - Add unit tests covering: happy-path canonical TOML, rejection of `when.assignee = "..."`, rejection of `run.path = "..."`, rejection of a `[[rule]]` lacking `run`, acceptance of `[[cleanup]]` presence.
  - Observable completion: `cargo test -p roki-daemon config::workflow` passes; the loader rejects unsupported `when.*` and `run.*` forms with key-path-bearing errors before the binary binds the listener.
  - _Requirements: 2.2, 2.3, 2.5, 5.3, 6.2_
  - _Boundary: config::workflow_
  - _Depends: 1.2_

- [x] 3. Linear adapter: normalized ticket type, viewer resolver, webhook receiver
- [x] 3.1 Define the internal `NormalizedTicket` value object consumed by admission and rule evaluation
  - Carry the minimum fields downstream modules consult: `id`, `assignee_id` (`Option`), `status`, `labels`.
  - Mark the constructor as crate-internal; only `linear::webhook::normalize` may build instances.
  - Observable completion: the type compiles; admission and rule modules can later import it without exposing the Linear envelope shape.
  - _Requirements: 3.2, 4.1, 5.1_
  - _Depends: 1.1_

- [x] 3.2 (P) Implement the Linear `viewer { id }` resolver with the `test-support` env-var seam
  - Issue one `POST https://api.linear.app/graphql` with body `{"query":"query { viewer { id } }"}` and `Authorization: <[linear].token>` header (token applied verbatim, no `Bearer` prefix).
  - Gate the `ROKI_LINEAR_GRAPHQL_URL` env-var override behind `#[cfg(any(test, feature = "test-support"))]` so the release binary always targets the hardcoded endpoint.
  - On non-200, malformed body, or missing `viewer.id`, return `LinearClientError::ViewerResolveFailed` carrying the endpoint string.
  - Add an integration test using `wiremock` covering: success returning `u1`, non-200 failure, missing-field failure.
  - Observable completion: `cargo test -p roki-daemon --features test-support linear::client` passes against the wiremock stub; the override takes effect only when the feature is enabled.
  - _Requirements: 4.2_
  - _Boundary: linear::client_
  - _Depends: 1.2, 2.1_

- [x] 3.3 (P) Implement the axum webhook receiver with the cycle-started backpressure pair
  - Bind axum on `[linear.webhook].bind` and `[linear.webhook].port`; route `POST /*` to the handler.
  - Handler holds `Arc<tokio::sync::mpsc::Sender<NormalizedTicket>>` (channel capacity 1) and `Arc<AtomicBool> cycle_started` (init `false`); per accepted POST: parse body → load `cycle_started` (`Acquire`); if `true` → 503; else `sender.try_send(ticket)` → `Ok(())` = 202, `TrySendError::Full` = 503, `TrySendError::Closed` = 503.
  - Reject malformed JSON or payloads missing `data.id` / `data.assignee.id` / `data.state.name` / `data.labels[].name` with HTTP 400 + `tracing::warn!` parse-error log carrying an `error_id`; response body `{"error":"invalid_payload"}`.
  - Do not verify any HMAC or signature header even when `[linear.webhook].secret` is configured.
  - Add an integration test using `tower::ServiceExt::oneshot` covering: 400 on bad body, 202 on good body when channel has capacity and `cycle_started == false`, 503 when `cycle_started == true`, 503 when the receiver is dropped, concurrent good-body POSTs producing one 202 and one 503 via `TrySendError::Full`.
  - Observable completion: `cargo test -p roki-daemon linear::webhook` passes; the listener emits `NormalizedTicket` over the channel for accepted payloads and replies 4xx / 503 per the contract above.
  - _Requirements: 3.1, 3.2, 3.3, 3.4, 8.4_
  - _Boundary: linear::webhook_
  - _Depends: 1.2, 2.1, 3.1_

- [x] 4. Engine: admission, rule, capture, runner pure / near-pure modules
- [x] 4.1 (P) Implement the admission filter
  - Pure function `Admission::accept(&NormalizedTicket, &WorkflowConfig, &MeId) -> Result<AdmittedTicket, AdmissionError>`.
  - Accept the ticket only when its assignee equals `WorkflowConfig::admission::assignee`, with `me` resolved by the caller to the viewer id passed via `MeId`.
  - Resolve the target repo as the first `[[admission.repos]]` entry only; when `WorkflowConfig::repo` is `None`, return `AdmissionError::NoRepos`.
  - Add unit tests covering: assignee mismatch → `Reject`; `me`-resolved id matches → `Accept`; missing `[[admission.repos]]` → `NoRepos`.
  - Observable completion: `cargo test -p roki-daemon admission` passes; the function returns an `AdmittedTicket` carrying the `ghq` repo on the happy path.
  - _Requirements: 4.1, 4.3, 4.4, 4.5_
  - _Boundary: admission_
  - _Depends: 2.2, 3.1_

- [x] 4.2 (P) Implement the first-match rule evaluator
  - Pure function `Rule::first_match(&AdmittedTicket, &[Rule]) -> Option<&Rule>` evaluating rules in declared order.
  - Match on `when.status` string equality and `when.labels.has_all` set containment only; do not evaluate `[[cleanup]]` or `[[on_failure]]`.
  - Add unit tests covering: status equality + `has_all` containment hit returns the rule; status mismatch returns `None`; `has_all` not contained returns `None`.
  - Observable completion: `cargo test -p roki-daemon rule` passes; rules later in the array are never reached when an earlier rule matches.
  - _Requirements: 5.1, 5.2, 5.4, 5.5_
  - _Boundary: rule_
  - _Depends: 2.2, 3.1_

- [x] 4.3 (P) Implement the per-cycle capture layout
  - Sync function `Capture::create(session_root: &Path, ticket_id: &str) -> Result<CaptureLayout, CaptureError>` that creates `<session_root>/cycle-<uuid>/` and opens stdout / stderr file handles inside it.
  - On any directory-create or file-open failure, return `CaptureError` carrying the offending path.
  - Add unit tests covering: happy-path layout creation under a `tempfile::TempDir`; error when `session_root` is unwritable.
  - Observable completion: `cargo test -p roki-daemon capture` passes; on the happy path the layout reports its directory and yields `File` handles ready for the runner.
  - _Requirements: 7.1, 7.3_
  - _Boundary: capture_
  - _Depends: 1.2, 2.1_

- [x] 4.4 Implement the command-form subprocess runner
  - Async function `Runner::spawn(cmd: &str, layout: &CaptureLayout) -> Result<RunOutcome, RunnerError>` invoking `tokio::process::Command::new("sh").arg("-c").arg(cmd)` with `Stdio::from(File)` redirects to the capture layout's stdout / stderr files.
  - Do not perform Liquid template rendering of `cmd`; do not run `pre` / `post` phases.
  - Record the subprocess `ExitStatus` in `RunOutcome`; capture-write failures during execution surface as `RunnerError`.
  - Add an integration test against `sh -c "echo hi; echo err >&2; exit 7"`: stdout file contains `hi`, stderr file contains `err`, `RunOutcome::exit_status` is 7.
  - Observable completion: `cargo test -p roki-daemon runner` passes; bytes written to the capture files match the subprocess output.
  - _Requirements: 6.1, 6.3, 6.4, 6.5, 7.2_
  - _Boundary: runner_
  - _Depends: 4.3_

- [x] 5. Runtime, CLI, and binary entry: wire the pipeline and own shutdown
- [x] 5.1 Implement the runtime orchestrator
  - Pipeline: load `RokiConfig` → load `WorkflowConfig` → resolve `me` via `LinearClient` when `[admission].assignee == "me"` → create `mpsc::channel::<NormalizedTicket>(1)` + `Arc<AtomicBool> cycle_started` (init `false`) → bind webhook listener (handler holds `Sender` clone + atomic clone) → loop `receiver.recv().await` → admission (on reject: info log + `continue`) → rule first-match (on no-match: info log + `continue`) → on match: `cycle_started.store(true, Release)` → drop receiver → `Capture::create` → `Runner::spawn` → await exit → flush capture files → break → `axum::serve(...).with_graceful_shutdown(...)` drains in-flight handler → return `ExitCode::SUCCESS`.
  - Treat startup-bound failures (`RokiConfig`, `WorkflowConfig`, `me` resolve, bind) and cycle-bound failures (capture, runner) as `Err(SkeletonError)` causing `ExitCode::FAILURE`; admission rejection and rule no-match are not failures and re-arm the loop.
  - Hold `Option<MeId>` plus the `Sender` / `Receiver` / `cycle_started` triple as the entire runtime state; no mutex, no swap, no placeholder window.
  - Observable completion: `cargo check -p roki-daemon` passes; on the happy in-process path the orchestrator returns `ExitCode::SUCCESS` regardless of subprocess exit code, and on cycle error returns `ExitCode::FAILURE`.
  - _Requirements: 1.1, 3.1, 4.5, 5.4, 8.1, 8.2, 8.3, 8.4_
  - _Boundary: runtime_
  - _Depends: 1.2, 2.1, 2.2, 3.2, 3.3, 4.1, 4.2, 4.3, 4.4_

- [x] 5.2 Implement the `roki run --config <path>` CLI surface
  - Define a top-level `roki` `clap` command with one subcommand (`run`) and one flag (`--config <path>`); produce a typed `CliCommand` enum and dispatch the `Run` variant to `runtime::run`.
  - On missing or unreadable `--config`, surface the path in the error so the resulting `tracing::error!` log line names the offending file.
  - Ensure `roki --help` and `roki run --help` list `--config` together with the configuration file it identifies (`roki.toml`).
  - Observable completion: `cargo run -p roki-daemon -- run --help` prints usage that names `--config` and `roki.toml`; invoking `run` without `--config` exits non-zero.
  - _Requirements: 1.1, 1.2, 1.3_
  - _Boundary: cli_
  - _Depends: 5.1_

- [x] 5.3 Wire the binary entry point
  - Replace the `main.rs` stub from task 1.1 with the production wiring: install the default `tracing-subscriber` as the very first action, then call `cli::run().await` and propagate its `ExitCode`.
  - Return `anyhow::Result<ExitCode>` from `cli::run` and let `main` exit via the returned `ExitCode` so internal daemon errors surface as a non-zero exit.
  - Observable completion: `cargo run -p roki-daemon -- run --config /nonexistent` exits with a non-zero status and emits a startup error naming the missing path; a happy-path invocation prints no panics.
  - _Requirements: 1.1, 1.2, 8.2, 8.3_
  - _Boundary: main, cli_
  - _Depends: 5.2_

- [x] 6. Validation: end-to-end smoke gate and graph integrity
- [x] 6.1 Implement the end-to-end smoke test as the acceptance gate
  - Add `crates/roki-daemon/tests/e2e/skeleton_smoke.rs` that drives the binary as a subprocess via `env!("CARGO_BIN_EXE_roki")` and posts one Linear-shaped JSON body over loopback HTTP.
  - Setup: a `tempfile::TempDir` workspace containing a generated `roki.toml` (port chosen by `TcpListener::bind("127.0.0.1:0")` and written into the file), and a `WORKFLOW.toml` with `[admission].assignee = "u1"`, one `[[admission.repos]]`, and one `[[rule]]` whose `run.cmd` is `sh -c 'printf out; printf err 1>&2; exit 0'`.
  - Stub Linear `viewer { id }` with a `wiremock` server returning `u1`; set `ROKI_LINEAR_GRAPHQL_URL` to the wiremock base URL before spawning the binary so the `test-support`-gated seam takes effect.
  - Assertions: process exit code is zero; the per-cycle `stdout` capture file contains `out`; the `stderr` file contains `err`; a second POST issued before exit receives HTTP 503.
  - Observable completion: `cargo test -p roki-daemon --features test-support --test skeleton_smoke` passes against the skeleton implementation.
  - _Requirements: 7.2, 8.2, 8.4, 9.1, 9.2, 9.3_
  - _Depends: 5.3_

- [x] 6.2 Resolve the pre-existing dangling-module entries surfaced by `roki-doctools validate`
  - Run `roki-doctools validate` and confirm `fr:12`'s `crates/roki-daemon/src/runtime.rs` and `crates/roki-daemon/src/config/mod.rs` entries resolve now that the crate and stubbed module tree exist (created in task 1.1).
  - Reconcile `fr:02`'s `modules:` list with the design's chosen file layout: design places workflow under `src/config/workflow.rs`, so update `docs/fr/02-configuration.md` `refs.modules:` to drop `crates/roki-daemon/src/workflow/` (and add `crates/roki-daemon/src/config/workflow.rs` if appropriate). Make this the only fr-doc edit; do not retitle or restructure `fr:02`.
  - Confirm `design:roki-skeleton` and `tasks:roki-skeleton` `modules:` entries (`crates/roki-daemon/` and `crates/roki-daemon/tests/e2e/skeleton_smoke.rs`) resolve once the crate and the smoke test file exist.
  - If any other dangling references appear because of this spec's file additions, update the relevant `refs:` frontmatter to point at the materialized files.
  - Observable completion: `roki-doctools validate` exits zero with no dangling-module entries attributable to `crates/roki-daemon`.
  - _Requirements: 9.1_
  - _Depends: 6.1_
