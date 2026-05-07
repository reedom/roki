# Slice 1 Engine MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Layer the directive-driven `pre → run → post` iteration loop, the `[engine].max_iterations` cap, Liquid templating across argv / stdin / env, and the canonical per-iter capture layout on top of the existing `roki-skeleton` daemon, so that one ticket can drive a multi-iteration cycle with on-disk forensics and exit cleanly.

**Architecture:** The current `roki-daemon` crate gains an `engine` submodule (`src/engine/{cycle,phase,directive,template,context,outcome,mod}.rs`). `runtime::run_inner` keeps its orchestrator role (load → bind → drain → dispatch) and delegates the cycle to `engine::cycle::run_cycle`, which iterates phases through `engine::phase::run_command_phase`. The flat `cycle-<uuid>/{stdout,stderr}` capture is replaced by `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{phase}.{stdout,stderr}` plus parsed-derivative files. Subprocesses are command-shape only; `session = "session"` is rejected at config-load time. `runner.rs` is deleted.

**Tech Stack:** Rust 2024 (workspace edition), `tokio` async runtime, `liquid` for template rendering, `shell-words` for argv split, `async-trait` for the phase-executor seam, `serde_json` `StreamDeserializer` for last-JSON-object scanning, `tempfile` + `wiremock` + `reqwest` for tests (already present).

**Spec:** `docs/superpowers/specs/2026-05-07-slice1-engine-mvp-design.md` (commit 210645d on branch `slice1-engine-mvp`).

**Working branch:** `slice1-engine-mvp` (already created and contains the spec commit).

---

## File Structure

### Created

| Path                                                      | Responsibility                                                                                  |
| --------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `crates/roki-daemon/src/engine/mod.rs`                    | Public re-exports (`run_cycle`, `PhaseExecutor`, `CycleOutcome`, `FailureKind`, `PhaseKind`).   |
| `crates/roki-daemon/src/engine/outcome.rs`                | `PhaseKind`, `PhaseBody`, `PreDirective`, `PostDirective`, `PhaseOutcome`, `CycleOutcome`, `FailureKind`, `PhaseInfraError`. |
| `crates/roki-daemon/src/engine/directive.rs`              | `scan_last_json_object`, `parse_pre_directive`, `parse_post_directive`.                         |
| `crates/roki-daemon/src/engine/template.rs`               | `render_str(template, &PhaseContext)`, Liquid parse + render error mapping.                     |
| `crates/roki-daemon/src/engine/context.rs`                | `PhaseContext` (ticket / repo / cycle / config / pre / post / run views) + `roki_env_pairs`.    |
| `crates/roki-daemon/src/engine/phase.rs`                  | `PhaseExecutor` trait, `CommandPhaseExecutor`, `resolve_ghq_base`, subprocess spawn + capture wiring + outcome translation. |
| `crates/roki-daemon/src/engine/cycle.rs`                  | `run_cycle`: iteration loop, transitions, iter-cap enforcement, scratch-state plumbing.         |
| `crates/roki-daemon/tests/e2e/iteration_smoke.rs`         | New end-to-end smoke that drives a 2-iteration cycle through the binary.                        |

### Modified

| Path                                                      | Change                                                                                                |
| --------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `crates/roki-daemon/Cargo.toml`                           | Add `liquid`, `shell-words`, `async-trait`, `bytes`. Add `[[test]] name = "iteration_smoke"` entry.   |
| `crates/roki-daemon/src/main.rs`                          | Declare `mod engine;`. (Existing `mod runner;` is removed in Task 13.)                                |
| `crates/roki-daemon/src/error.rs`                         | Remove `RunnerError`. Add `PhaseInfraError`. Update `SkeletonError` aggregator.                       |
| `crates/roki-daemon/src/linear/ticket.rs`                 | Extend `NormalizedTicket` with `title: String` and `body: String`.                                    |
| `crates/roki-daemon/src/linear/webhook.rs`                | `parse_ticket` extracts `data.title` (default empty) and `data.description` (default empty).          |
| `crates/roki-daemon/src/config/workflow.rs`               | Replace `Rule::run_cmd: String` with `Rule { pre: Option<PhaseBody>, run: PhaseBody, post: Option<PhaseBody> }`. Reject `session = "session"` at load time. |
| `crates/roki-daemon/src/config/roki.rs`                   | Promote `EngineSection.max_iterations: Option<u32>` to applied default `10` (validated `>= 1`).       |
| `crates/roki-daemon/src/capture.rs`                       | Rewrite: `create_iter_dir`, `open_phase_files`, `write_response_json`, `write_run_exit_code`. Drop `CaptureLayout`. |
| `crates/roki-daemon/src/runtime.rs`                       | Replace direct `runner::spawn` call with `engine::cycle::run_cycle`. Map `CycleOutcome` to `ExitCode`. |
| `crates/roki-daemon/tests/e2e/skeleton_smoke.rs`          | Update assertion paths to the new layout (`<ticket-id>/cycle-<uuid>/iter-1/run.{stdout,stderr}`).     |

### Deleted

| Path                                       | Reason                                                                |
| ------------------------------------------ | --------------------------------------------------------------------- |
| `crates/roki-daemon/src/runner.rs`         | Behaviour absorbed into `engine::phase::CommandPhaseExecutor::execute`. |

---

## Cross-Task Conventions

### Test commands

- Whole crate: `cargo test -p roki-daemon --features test-support`
- Single unit: `cargo test -p roki-daemon --features test-support --lib <module>::tests::<name> -- --nocapture`
- Single integration: `cargo test -p roki-daemon --features test-support --test <name>`

The smoke tests already require `--features test-support` (the `ROKI_LINEAR_GRAPHQL_URL` seam). New tests follow the same convention.

### Commit style

- Conventional Commits.
- One commit per task end (after the task's final test passes).
- Title ≤ 50 chars, lowercase verb, scoped to `engine`, `capture`, `runtime`, `config`, `linear`, `tests`, or `deps`.
- Body explains the why when not obvious from the title.
- No emojis.
- Co-authored trailer included on every commit:

  ```
  Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
  ```

### Task dependency order

Tasks 1–4 establish primitives (deps, error types, ticket fields, workflow schema) the engine modules consume. Tasks 5–11 build the engine bottom-up. Tasks 12–15 wire it in and update tests.

---

## Task 1: Add dependencies

**Files:**
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Add `liquid`, `shell-words`, `async-trait` to `[dependencies]`**

Edit `crates/roki-daemon/Cargo.toml`. Insert these three lines into the alphabetical position inside `[dependencies]`:

```toml
async-trait = "0.1"
liquid = "0.26"
shell-words = "1"
```

The block should read (showing the alphabetical neighbours):

```toml
[dependencies]
anyhow = "1"
async-trait = "0.1"
axum = "0.7"
clap = { version = "4", features = ["derive"] }
liquid = "0.26"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
shell-words = "1"
thiserror = "2"
tokio = { version = "1", features = ["full"] }
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["fmt"] }
uuid = { version = "1", features = ["v4"] }
```

- [ ] **Step 2: Verify the workspace builds**

Run: `cargo build -p roki-daemon`
Expected: build succeeds, no compile errors. (Cargo will fetch the new crates.)

- [ ] **Step 3: Commit**

```bash
git add crates/roki-daemon/Cargo.toml Cargo.lock
git commit -m "deps: add liquid, shell-words, async-trait" -m "Slice 1 engine module needs Liquid templating, shell-words for argv split after render, and async-trait for the PhaseExecutor seam used in unit tests." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Extend `NormalizedTicket` with title and body

**Files:**
- Modify: `crates/roki-daemon/src/linear/ticket.rs`
- Modify: `crates/roki-daemon/src/linear/webhook.rs`

The skeleton's `NormalizedTicket` only carries `id`, `assignee_id`, `status`, `labels`. The Liquid context (`{{ ticket.title }}`, `{{ ticket.body }}`) needs both fields so operators can interpolate them into pre / run / post bodies.

- [ ] **Step 1: Write the failing ticket-extension test**

Append the following test to the `mod tests` block at the end of `crates/roki-daemon/src/linear/ticket.rs` (before the closing `}`):

```rust
    #[test]
    fn constructor_accepts_title_and_body() {
        let ticket = NormalizedTicket::new(
            "tid-3".to_string(),
            Some("u1".to_string()),
            "review".to_string(),
            vec!["needs-impl".to_string()],
            "Implement widget".to_string(),
            "Body paragraph one.\n\nBody paragraph two.".to_string(),
        );
        assert_eq!(ticket.title, "Implement widget");
        assert!(ticket.body.contains("paragraph two"));
    }
```

- [ ] **Step 2: Run the new test and confirm it fails to compile**

Run: `cargo test -p roki-daemon --features test-support --lib linear::ticket::tests::constructor_accepts_title_and_body`
Expected: compile error — `NormalizedTicket::new` takes 4 arguments, not 6.

- [ ] **Step 3: Add `title` and `body` fields plus a 6-arg constructor**

Replace the `NormalizedTicket` struct and `new` impl in `crates/roki-daemon/src/linear/ticket.rs` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedTicket {
    pub id: String,
    pub assignee_id: Option<String>,
    pub status: String,
    pub labels: Vec<String>,
    pub title: String,
    pub body: String,
}

impl NormalizedTicket {
    /// Build a `NormalizedTicket`.
    ///
    /// Crate-internal so only `linear::webhook::normalize` constructs instances;
    /// admission and rule evaluation read the public fields without depending
    /// on the Linear webhook envelope.
    pub(crate) fn new(
        id: String,
        assignee_id: Option<String>,
        status: String,
        labels: Vec<String>,
        title: String,
        body: String,
    ) -> Self {
        Self {
            id,
            assignee_id,
            status,
            labels,
            title,
            body,
        }
    }
}
```

- [ ] **Step 4: Update existing ticket unit tests to pass the new args**

In the same `mod tests` block, the existing `constructor_builds_ticket_with_all_fields`, `constructor_accepts_unassigned_ticket`, and `ticket_is_clonable_and_comparable` tests still call `NormalizedTicket::new` with 4 args. Update each call to pass two extra string arguments. Replace each existing test body with:

```rust
    #[test]
    fn constructor_builds_ticket_with_all_fields() {
        let ticket = NormalizedTicket::new(
            "tid-1".to_string(),
            Some("u1".to_string()),
            "in_progress".to_string(),
            vec!["bug".to_string(), "p0".to_string()],
            "Title".to_string(),
            "Body".to_string(),
        );
        assert_eq!(ticket.id, "tid-1");
        assert_eq!(ticket.assignee_id, Some("u1".to_string()));
        assert_eq!(ticket.status, "in_progress");
        assert_eq!(ticket.labels, vec!["bug".to_string(), "p0".to_string()]);
        assert_eq!(ticket.title, "Title");
        assert_eq!(ticket.body, "Body");
    }

    #[test]
    fn constructor_accepts_unassigned_ticket() {
        let ticket = NormalizedTicket::new(
            "t".to_string(),
            None,
            "todo".to_string(),
            Vec::new(),
            String::new(),
            String::new(),
        );
        assert!(ticket.assignee_id.is_none());
        assert_eq!(ticket.id, "t");
        assert_eq!(ticket.status, "todo");
        assert!(ticket.labels.is_empty());
        assert!(ticket.title.is_empty());
        assert!(ticket.body.is_empty());
    }

    #[test]
    fn ticket_is_clonable_and_comparable() {
        let ticket = NormalizedTicket::new(
            "tid-2".to_string(),
            Some("u2".to_string()),
            "review".to_string(),
            vec!["feature".to_string()],
            "T".to_string(),
            "B".to_string(),
        );
        let clone = ticket.clone();
        assert_eq!(ticket, clone);
    }
```

- [ ] **Step 5: Run the ticket tests**

Run: `cargo test -p roki-daemon --features test-support --lib linear::ticket::tests`
Expected: 4 tests pass (the new one + 3 updated).

- [ ] **Step 6: Update `webhook::parse_ticket` to populate title and body**

Edit `crates/roki-daemon/src/linear/webhook.rs`. Replace the `parse_ticket` function (around lines 123–159) with:

```rust
fn parse_ticket(body: &[u8]) -> Result<NormalizedTicket, String> {
    let value: Value = serde_json::from_slice(body).map_err(|err| format!("invalid json: {err}"))?;

    let id = value
        .pointer("/data/id")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing data.id".to_string())?
        .to_string();

    let assignee_id = value
        .pointer("/data/assignee/id")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing data.assignee.id".to_string())?
        .to_string();

    let status = value
        .pointer("/data/state/name")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing data.state.name".to_string())?
        .to_string();

    let label_nodes = value
        .pointer("/data/labels")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing data.labels".to_string())?;
    let labels = label_nodes
        .iter()
        .map(|node| {
            node.pointer("/name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Option<Vec<String>>>()
        .ok_or_else(|| "missing data.labels[].name".to_string())?;

    // Title and body are not required by Linear's webhook schema for every
    // event kind; treat them as optional and default to empty so the
    // engine's Liquid context can still expand `{{ ticket.title }}` /
    // `{{ ticket.body }}` to an empty string for events that omit them.
    let title = value
        .pointer("/data/title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let body = value
        .pointer("/data/description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    Ok(NormalizedTicket::new(
        id,
        Some(assignee_id),
        status,
        labels,
        title,
        body,
    ))
}
```

- [ ] **Step 7: Add a webhook test that asserts title and body propagate**

Append the following test inside the existing `mod tests` block in `crates/roki-daemon/src/linear/webhook.rs` (before its closing `}`):

```rust
    #[tokio::test]
    async fn good_body_propagates_title_and_description() {
        let (state, mut rx, _cycle) = make_state();
        let app = router(state);

        let mut body = good_body();
        body["data"]["title"] = serde_json::json!("Implement widget");
        body["data"]["description"] = serde_json::json!("Multi-line\ndescription");

        let res = post_json(app, serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let ticket = rx.recv().await.expect("ticket emitted");
        assert_eq!(ticket.title, "Implement widget");
        assert!(ticket.body.contains("description"));
    }

    #[tokio::test]
    async fn missing_title_and_description_default_to_empty() {
        let (state, mut rx, _cycle) = make_state();
        let app = router(state);

        // good_body() omits title/description; assert they default to "".
        let res = post_json(app, serde_json::to_vec(&good_body()).unwrap()).await;
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let ticket = rx.recv().await.expect("ticket emitted");
        assert_eq!(ticket.title, "");
        assert_eq!(ticket.body, "");
    }
```

- [ ] **Step 8: Run webhook tests**

Run: `cargo test -p roki-daemon --features test-support --lib linear::webhook::tests`
Expected: existing tests + 2 new ones all pass.

- [ ] **Step 9: Commit**

```bash
git add crates/roki-daemon/src/linear/ticket.rs crates/roki-daemon/src/linear/webhook.rs
git commit -m "feat(linear): add title and body to NormalizedTicket" -m "Engine Liquid context exposes {{ ticket.title }} and {{ ticket.body }}; both default to empty when the webhook envelope omits them so events without those fields still admit." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `EngineSection.max_iterations` becomes an applied default

**Files:**
- Modify: `crates/roki-daemon/src/config/roki.rs`

Today `EngineSection.max_iterations: Option<u32>` is loaded but never applied. Slice 1 needs a guaranteed `u32` with default 10 and a validator rejecting values below 1.

- [ ] **Step 1: Read the current `[engine]` parser**

Open `crates/roki-daemon/src/config/roki.rs` and locate the `EngineSection` struct (around lines 87–90) and the function that builds it from raw TOML (search for `EngineSection {`).

- [ ] **Step 2: Add the failing default-and-validation test**

Append the following two tests to the existing `mod tests` block in `crates/roki-daemon/src/config/roki.rs` (before its closing `}`). If your IDE inserts duplicates, ensure each name is unique.

```rust
    #[test]
    fn max_iterations_defaults_to_ten_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let toml_body = r#"
[linear]
token = "x"

[linear.webhook]
bind = "127.0.0.1"
port = 8000

[default.ai.command]
cli = "echo"

[engine]

[paths]
workflow = "/tmp/w.toml"
session_root = "/tmp/sess"

[log]
"#;
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml_body).unwrap();

        let cfg = RokiConfig::load(&path).expect("load");
        assert_eq!(cfg.engine.max_iterations, 10);
    }

    #[test]
    fn max_iterations_zero_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let toml_body = r#"
[linear]
token = "x"

[linear.webhook]
bind = "127.0.0.1"
port = 8000

[default.ai.command]
cli = "echo"

[engine]
max_iterations = 0

[paths]
workflow = "/tmp/w.toml"
session_root = "/tmp/sess"

[log]
"#;
        let path = dir.path().join("roki.toml");
        std::fs::write(&path, toml_body).unwrap();

        let err = RokiConfig::load(&path).expect_err("rejects zero");
        let msg = format!("{err}");
        assert!(msg.contains("max_iterations"), "msg: {msg}");
    }
```

The first test references `cfg.engine.max_iterations` as a plain `u32`, not `Option<u32>`. That's the change we're driving toward.

- [ ] **Step 3: Run the new tests and confirm they fail**

Run: `cargo test -p roki-daemon --features test-support --lib config::roki::tests::max_iterations`
Expected: compile error on the first test (`Option` vs `u32`) or value mismatch on the second.

- [ ] **Step 4: Change `max_iterations` to `u32`, default 10, validate `>= 1`**

In `crates/roki-daemon/src/config/roki.rs`, change the struct field:

```rust
#[derive(Clone, Debug)]
pub struct EngineSection {
    pub max_iterations: u32,
}

impl Default for EngineSection {
    fn default() -> Self {
        Self { max_iterations: 10 }
    }
}
```

(The previous `#[derive(Default)]` on this struct produced `max_iterations: None`. We replace it with an explicit `Default` so the field type can be `u32`.)

Find the function that parses `[engine]` (it currently reads `max_iterations` as `Option<u32>`). Replace its body so it returns `EngineSection { max_iterations }` where `max_iterations` defaults to `10` and is validated `>= 1`. The exact code is:

```rust
fn parse_engine(path: &Path, root: &toml::Value) -> Result<EngineSection, RokiConfigError> {
    let Some(engine_table) = root.get("engine").and_then(toml::Value::as_table) else {
        return Ok(EngineSection::default());
    };

    let max_iterations = match engine_table.get("max_iterations") {
        Some(value) => {
            let n = value
                .as_integer()
                .ok_or_else(|| RokiConfigError::TypeMismatch {
                    path: path.to_path_buf(),
                    key: "engine.max_iterations".to_string(),
                    expected: "u32",
                })?;
            if n < 1 {
                return Err(RokiConfigError::TypeMismatch {
                    path: path.to_path_buf(),
                    key: "engine.max_iterations".to_string(),
                    expected: "u32 >= 1",
                });
            }
            u32::try_from(n).map_err(|_| RokiConfigError::TypeMismatch {
                path: path.to_path_buf(),
                key: "engine.max_iterations".to_string(),
                expected: "u32",
            })?
        }
        None => 10,
    };

    Ok(EngineSection { max_iterations })
}
```

If the existing function is named differently (e.g. it lives inside `RokiConfig::load`), replace the equivalent block with the body above.

- [ ] **Step 5: Run config tests**

Run: `cargo test -p roki-daemon --features test-support --lib config::roki::tests`
Expected: all existing config tests still pass, plus the two new ones.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/config/roki.rs
git commit -m "feat(config): apply [engine].max_iterations with default 10" -m "Skeleton accepted the key without applying it. Slice 1's iteration cap requires a non-Option u32 with validation >= 1." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: New error types — drop `RunnerError`, add `PhaseInfraError`

**Files:**
- Modify: `crates/roki-daemon/src/error.rs`

`runner.rs` is being deleted, so `RunnerError` goes with it. The engine introduces `PhaseInfraError` for infrastructure-level failures (spawn / wait / capture / repo-resolution) that propagate up to `runtime`.

- [ ] **Step 1: Write the failing PhaseInfraError display test**

Append the following test to the `mod tests` block at the end of `crates/roki-daemon/src/error.rs`:

```rust
    #[test]
    fn phase_infra_display_carries_paths_and_cmds() {
        let e = PhaseInfraError::Spawn {
            cmd: "claude --foo".into(),
            source: io_err(),
        };
        assert!(format!("{e}").contains("claude --foo"));

        let e = PhaseInfraError::Wait {
            cmd: "claude --foo".into(),
            source: io_err(),
        };
        assert!(format!("{e}").contains("claude --foo"));

        let e = PhaseInfraError::RepoNotFound {
            ghq: "github.com/acme/widget".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("github.com/acme/widget"), "msg: {s}");

        let e = PhaseInfraError::Capture(CaptureError::CreateDir {
            path: PathBuf::from("/tmp/foo"),
            source: io_err(),
        });
        assert!(format!("{e}").contains("/tmp/foo"));
    }

    #[test]
    fn skeleton_error_aggregates_phase_infra() {
        let inner = PhaseInfraError::Spawn {
            cmd: "x".into(),
            source: io_err(),
        };
        let outer: SkeletonError = inner.into();
        assert!(format!("{outer}").contains("x"));
    }
```

- [ ] **Step 2: Run the failing tests**

Run: `cargo test -p roki-daemon --features test-support --lib error::tests::phase_infra`
Expected: compile error — `PhaseInfraError` does not exist.

- [ ] **Step 3: Add `PhaseInfraError` and remove `RunnerError`**

In `crates/roki-daemon/src/error.rs`:

1. Delete the entire `RunnerError` enum (the block under the doc comment "Errors raised by the subprocess runner.").
2. Delete the corresponding `Runner(#[from] RunnerError)` variant from `SkeletonError`.
3. Delete the `runner_display_carries_cmd` test in `mod tests`.
4. Delete the `RunnerError::Spawn` block in `skeleton_error_aggregates_via_from`.
5. Add the new error type. Place this block just above `SkeletonError`:

```rust
/// Errors raised by the engine's phase executor that are infrastructure-level
/// rather than directive-level failures. These propagate up through
/// `runtime::run_inner` and exit the binary with `ExitCode::FAILURE`. They
/// are distinct from `engine::outcome::FailureKind`, which represents
/// directive-level failures (`unparseable`, `schema_drift`, `process_crash`,
/// `template_error`, `iter_exhausted`) routed inside the cycle.
#[derive(Debug, Error)]
pub enum PhaseInfraError {
    #[error("phase failed to spawn '{cmd}': {source}")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("phase failed to wait on '{cmd}': {source}")]
    Wait {
        cmd: String,
        #[source]
        source: std::io::Error,
    },

    #[error("ghq base path not found for '{ghq}'")]
    RepoNotFound { ghq: String },

    #[error(transparent)]
    Capture(#[from] CaptureError),
}
```

6. Add the new `From` arm to `SkeletonError`:

```rust
#[derive(Debug, Error)]
pub enum SkeletonError {
    #[error(transparent)]
    Config(#[from] RokiConfigError),

    #[error(transparent)]
    Workflow(#[from] WorkflowError),

    #[error(transparent)]
    LinearClient(#[from] LinearClientError),

    #[error(transparent)]
    Webhook(#[from] WebhookError),

    #[error(transparent)]
    Admission(#[from] AdmissionError),

    #[error(transparent)]
    Capture(#[from] CaptureError),

    #[error(transparent)]
    PhaseInfra(#[from] PhaseInfraError),
}
```

- [ ] **Step 4: Run error tests**

Run: `cargo test -p roki-daemon --features test-support --lib error::tests`
Expected: all error display tests pass, including the two new `PhaseInfraError` ones.

(The crate as a whole will not yet compile because `runner.rs` still exists and references `RunnerError`. That's expected and fixed in Task 13.)

- [ ] **Step 5: Make `runner.rs` compile against the new error surface temporarily**

Until Task 13 deletes `runner.rs`, keep the workspace building. Edit `crates/roki-daemon/src/runner.rs`:

- Replace `use crate::error::RunnerError;` with `use crate::error::PhaseInfraError as RunnerError;` (rename via use-as).
- Update the `RunnerError::Spawn { ... }` and `RunnerError::Wait { ... }` constructors so they refer to `PhaseInfraError::Spawn` and `PhaseInfraError::Wait` literally — the field shapes match. Find the two `RunnerError::Spawn { cmd: cmd.to_string(), source }` and one `RunnerError::Wait { cmd: cmd.to_string(), source }` in the file and rewrite them as `PhaseInfraError::Spawn` / `PhaseInfraError::Wait` directly.

This is a temporary scaffold; the file is removed in Task 13. The change keeps `runtime::run_inner` and the smoke test green during the engine build-out.

- [ ] **Step 6: Verify the workspace still builds**

Run: `cargo build -p roki-daemon`
Expected: build succeeds.

- [ ] **Step 7: Run the full daemon test suite**

Run: `cargo test -p roki-daemon --features test-support`
Expected: all tests pass (skeleton smoke included).

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/error.rs crates/roki-daemon/src/runner.rs
git commit -m "feat(error): drop RunnerError, add PhaseInfraError" -m "Engine phase executor needs an infrastructure-level error type for spawn/wait/repo-resolve/capture failures, distinct from directive-level FailureKind. RunnerError becomes obsolete once runner.rs is removed in a later task; for now runner.rs is rebound to PhaseInfraError so the workspace stays buildable." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `engine::outcome` module — types only

**Files:**
- Create: `crates/roki-daemon/src/engine/mod.rs`
- Create: `crates/roki-daemon/src/engine/outcome.rs`
- Modify: `crates/roki-daemon/src/main.rs`

This task adds the type vocabulary the rest of the engine builds on. No behaviour yet beyond `Display` and `Default` impls.

- [ ] **Step 1: Declare the engine module**

Edit `crates/roki-daemon/src/main.rs`. Find the `mod runner;` line (search for `mod runner`) and add `mod engine;` immediately above it. The block of `mod` lines should now include:

```rust
mod admission;
mod capture;
mod cli;
mod config;
mod engine;
mod error;
mod linear;
mod rule;
mod runner;
mod runtime;
```

- [ ] **Step 2: Create `engine/mod.rs` with placeholder re-exports**

Write `crates/roki-daemon/src/engine/mod.rs`:

```rust
//! Engine submodule: directive-driven cycle execution.
//!
//! Layered bottom-up:
//! - `outcome` — type vocabulary (PhaseKind, PhaseBody, directives, FailureKind).
//! - `directive` — last-JSON-object scan + per-phase legal-set validation.
//! - `template` — Liquid render for argv and stdin body.
//! - `context` — PhaseContext (Liquid object + ROKI_* env builder).
//! - `phase` — PhaseExecutor trait + the production CommandPhaseExecutor.
//! - `cycle` — run_cycle: iteration loop, transitions, iter cap.

pub mod context;
pub mod cycle;
pub mod directive;
pub mod outcome;
pub mod phase;
pub mod template;

pub use cycle::{run_cycle, CycleOutcome};
pub use outcome::{
    FailureKind, PhaseBody, PhaseKind, PhaseOutcome, PostDirective, PreDirective,
};
pub use phase::{CommandPhaseExecutor, PhaseExecutor};
```

The file references modules that don't yet exist; we'll create them in subsequent tasks. To keep the workspace compiling after this task, we also create empty stubs in Step 4.

- [ ] **Step 3: Write the failing outcome test**

Create `crates/roki-daemon/src/engine/outcome.rs` with the test alone (no impl yet). The compiler error proves the test is the failing-first step.

```rust
#![allow(dead_code)]

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_directive_legal_set_excludes_pre() {
        // Only `run` and `end` are legal for Pre.
        assert!(PreDirective::try_from_str("run").is_some());
        assert!(PreDirective::try_from_str("end").is_some());
        assert!(PreDirective::try_from_str("pre").is_none());
        assert!(PreDirective::try_from_str("halt").is_none());
    }

    #[test]
    fn post_directive_legal_set_covers_pre_run_end() {
        assert!(PostDirective::try_from_str("pre").is_some());
        assert!(PostDirective::try_from_str("run").is_some());
        assert!(PostDirective::try_from_str("end").is_some());
        assert!(PostDirective::try_from_str("halt").is_none());
    }

    #[test]
    fn phase_kind_str_round_trip() {
        assert_eq!(PhaseKind::Pre.as_str(), "pre");
        assert_eq!(PhaseKind::Run.as_str(), "run");
        assert_eq!(PhaseKind::Post.as_str(), "post");
    }
}
```

- [ ] **Step 4: Add the implementation above the test module**

Insert the following at the top of `crates/roki-daemon/src/engine/outcome.rs` (above the `#[cfg(test)]` block):

```rust
//! Engine type vocabulary.
//!
//! Variant naming mirrors the FR 01 directive schema: pre returns
//! `run` / `end`; post returns `pre` / `run` / `end`. `FailureKind` enumerates
//! every directive-level failure the engine can route in slice 1.

use serde::Deserialize;

/// Which phase position the engine is executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    Pre,
    Run,
    Post,
}

impl PhaseKind {
    /// Lowercase canonical name used for capture file prefixes and tracing.
    pub fn as_str(self) -> &'static str {
        match self {
            PhaseKind::Pre => "pre",
            PhaseKind::Run => "run",
            PhaseKind::Post => "post",
        }
    }
}

/// Operator-authored body for one phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseBody {
    /// Inline `cmd = "<shell line>"`. Rendered, then run as `sh -c <rendered>`.
    /// stdin is closed immediately.
    InlineCmd { cmd: String },
    /// Inline `prompt = "<text>"`. Rendered as the stdin body. Argv comes from
    /// `[default.ai.command].cli` (or a frontmatter override, but inline form
    /// has no frontmatter, so always the default).
    InlinePrompt { prompt: String },
    /// `path = "workflow/<file>.md"`. The frontmatter optionally overrides
    /// `cli`; the body (post-frontmatter) is rendered as the stdin body.
    Path {
        body: String,
        cli_override: Option<String>,
    },
}

/// Pre-phase legal directive set: `run` or `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreDirective {
    Run,
    End,
}

impl PreDirective {
    pub fn try_from_str(value: &str) -> Option<Self> {
        match value {
            "run" => Some(PreDirective::Run),
            "end" => Some(PreDirective::End),
            _ => None,
        }
    }
}

/// Post-phase legal directive set: `pre`, `run`, or `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PostDirective {
    Pre,
    Run,
    End,
}

impl PostDirective {
    pub fn try_from_str(value: &str) -> Option<Self> {
        match value {
            "pre" => Some(PostDirective::Pre),
            "run" => Some(PostDirective::Run),
            "end" => Some(PostDirective::End),
            _ => None,
        }
    }
}

/// One phase invocation's outcome forwarded to `engine::cycle`.
#[derive(Debug, Clone)]
pub enum PhaseOutcome {
    PreDirective {
        directive: PreDirective,
        payload: serde_json::Value,
    },
    PostDirective {
        directive: PostDirective,
        payload: serde_json::Value,
    },
    RunDone {
        exit_code: i32,
        duration_seconds: u64,
    },
    Failure {
        kind: FailureKind,
    },
}

/// Directive-level failure kinds. Distinct from `PhaseInfraError`, which
/// represents infrastructure-level failures that escape the cycle as a
/// `Result::Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Pre/Post: stdout has no JSON object, or the last JSON object lacks
    /// `directive`.
    Unparseable,
    /// Pre/Post: `directive` value outside the legal set for the phase.
    SchemaDrift,
    /// Pre/Post: non-zero exit and stdout has no parseable JSON object.
    ProcessCrash,
    /// Liquid render of argv or stdin body failed before launch.
    TemplateError,
    /// Post returned `pre` or `run` while `iter == max_iterations`.
    IterExhausted,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::Unparseable => "unparseable",
            FailureKind::SchemaDrift => "schema_drift",
            FailureKind::ProcessCrash => "process_crash",
            FailureKind::TemplateError => "template_error",
            FailureKind::IterExhausted => "iter_exhausted",
        }
    }
}
```

- [ ] **Step 5: Create empty stubs for the modules `mod.rs` references**

Without these stubs the crate won't compile. Each will be filled in subsequent tasks.

Write `crates/roki-daemon/src/engine/directive.rs`:

```rust
#![allow(dead_code)]
//! Last-JSON-object scan + per-phase legal-set validation. Filled in Task 6.
```

Write `crates/roki-daemon/src/engine/template.rs`:

```rust
#![allow(dead_code)]
//! Liquid render for argv and stdin body. Filled in Task 8.
```

Write `crates/roki-daemon/src/engine/context.rs`:

```rust
#![allow(dead_code)]
//! PhaseContext + ROKI_* env builder. Filled in Task 7.
```

Write `crates/roki-daemon/src/engine/phase.rs`:

```rust
#![allow(dead_code)]
//! Phase executor. Filled in Task 10.

use async_trait::async_trait;

use crate::error::PhaseInfraError;

use super::context::PhaseContext;
use super::outcome::{PhaseBody, PhaseKind, PhaseOutcome};

/// Trait the cycle uses to invoke phases. The production implementation is
/// `CommandPhaseExecutor`; tests substitute a deterministic fake.
#[async_trait]
pub trait PhaseExecutor: Send + Sync {
    async fn execute(
        &self,
        kind: PhaseKind,
        body: &PhaseBody,
        ctx: &PhaseContext,
        iter_dir: &std::path::Path,
    ) -> Result<PhaseOutcome, PhaseInfraError>;
}

/// Production phase executor. Implementation lives in Task 10.
pub struct CommandPhaseExecutor;
```

Write `crates/roki-daemon/src/engine/cycle.rs`:

```rust
#![allow(dead_code)]
//! Cycle driver. Filled in Task 11.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed { iters: u32 },
    Failed {
        kind: super::outcome::FailureKind,
        iter: u32,
    },
}

// `run_cycle` body lands in Task 11. Stub re-export so `engine::mod` compiles.
pub use stub::run_cycle;
mod stub {
    use super::CycleOutcome;
    use crate::admission::AdmittedTicket;
    use crate::config::roki::RokiConfig;
    use crate::config::workflow::Rule;
    use crate::error::PhaseInfraError;
    use std::path::Path;

    pub async fn run_cycle(
        _admitted: &AdmittedTicket,
        _rule: &Rule,
        _session_root: &Path,
        _cfg: &RokiConfig,
    ) -> Result<CycleOutcome, PhaseInfraError> {
        unimplemented!("run_cycle implemented in Task 11");
    }
}
```

The `Rule` reference will be valid after Task 9 reshapes `config::workflow::Rule`. For now `Rule` is the existing struct (with `run_cmd`); the stub never actually uses the field, so it compiles regardless.

- [ ] **Step 6: Run the outcome unit tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::outcome::tests`
Expected: 3 tests pass.

- [ ] **Step 7: Verify the whole crate compiles**

Run: `cargo build -p roki-daemon`
Expected: build succeeds (the engine stubs compile because nothing calls `run_cycle` yet beyond the type system).

- [ ] **Step 8: Commit**

```bash
git add crates/roki-daemon/src/main.rs crates/roki-daemon/src/engine/
git commit -m "feat(engine): add outcome types + module skeleton" -m "Slice 1 type vocabulary: PhaseKind, PhaseBody, PreDirective, PostDirective, PhaseOutcome, FailureKind. Module stubs declared so subsequent tasks fill in directive parse, template render, context, phase executor, and cycle driver." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `engine::directive` — last JSON object scan + legal-set validation

**Files:**
- Modify: `crates/roki-daemon/src/engine/directive.rs`

- [ ] **Step 1: Write failing tests**

Replace the current contents of `crates/roki-daemon/src/engine/directive.rs` with the test module + the public function signatures, leaving function bodies as `unimplemented!()`. The tests fail because the implementations panic.

```rust
#![allow(dead_code)]

//! Scan stdout for the last top-level JSON object and validate the contained
//! `directive` value against the phase's legal set.

use serde_json::Value;

use super::outcome::{FailureKind, PostDirective, PreDirective};

/// Walk top-level values in `stdout` and return the **last** parsed
/// `Value::Object`. Bytes between objects are ignored. Items that fail to
/// parse are dropped. Non-object top-level values (string, number, array,
/// null) are ignored. Returns `None` if no top-level object parsed
/// successfully.
pub fn scan_last_json_object(stdout: &[u8]) -> Option<Value> {
    use serde_json::Deserializer;

    let mut last: Option<Value> = None;
    let stream = Deserializer::from_slice(stdout).into_iter::<Value>();
    for item in stream {
        match item {
            Ok(value @ Value::Object(_)) => last = Some(value),
            Ok(_) => {}
            Err(_) => {}
        }
    }
    last
}

/// Pre-phase result: the parsed directive plus the full payload, or a
/// `FailureKind` describing the parse / validation failure.
pub enum PreParse {
    Ok {
        directive: PreDirective,
        payload: Value,
    },
    Failed(FailureKind),
}

/// Parse a Pre stdout slice into `PreParse`.
///
/// `exit_status_success`: whether the subprocess returned exit code 0. Used
/// to disambiguate Unparseable (zero exit + no JSON) from ProcessCrash
/// (non-zero exit + no JSON).
pub fn parse_pre_directive(stdout: &[u8], exit_status_success: bool) -> PreParse {
    let Some(value) = scan_last_json_object(stdout) else {
        return if exit_status_success {
            PreParse::Failed(FailureKind::Unparseable)
        } else {
            PreParse::Failed(FailureKind::ProcessCrash)
        };
    };
    let Some(directive_str) = value.get("directive").and_then(Value::as_str) else {
        return PreParse::Failed(FailureKind::Unparseable);
    };
    match PreDirective::try_from_str(directive_str) {
        Some(directive) => PreParse::Ok {
            directive,
            payload: value,
        },
        None => PreParse::Failed(FailureKind::SchemaDrift),
    }
}

/// Post-phase analogue of `parse_pre_directive`.
pub enum PostParse {
    Ok {
        directive: PostDirective,
        payload: Value,
    },
    Failed(FailureKind),
}

pub fn parse_post_directive(stdout: &[u8], exit_status_success: bool) -> PostParse {
    let Some(value) = scan_last_json_object(stdout) else {
        return if exit_status_success {
            PostParse::Failed(FailureKind::Unparseable)
        } else {
            PostParse::Failed(FailureKind::ProcessCrash)
        };
    };
    let Some(directive_str) = value.get("directive").and_then(Value::as_str) else {
        return PostParse::Failed(FailureKind::Unparseable);
    };
    match PostDirective::try_from_str(directive_str) {
        Some(directive) => PostParse::Ok {
            directive,
            payload: value,
        },
        None => PostParse::Failed(FailureKind::SchemaDrift),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_returns_last_object_when_multiple_present() {
        let stdout = br#"
        {"directive":"run","note":"first"}
        advisory text
        {"directive":"end","note":"second"}
        "#;
        let v = scan_last_json_object(stdout).expect("must find last object");
        assert_eq!(v["note"], "second");
    }

    #[test]
    fn scan_ignores_non_object_top_level_values() {
        let stdout = br#"42 "string-value" {"directive":"run"} [1,2,3]"#;
        let v = scan_last_json_object(stdout).expect("must find the object");
        assert_eq!(v["directive"], "run");
    }

    #[test]
    fn scan_returns_none_when_no_object_present() {
        let stdout = b"plain text with no JSON whatsoever";
        assert!(scan_last_json_object(stdout).is_none());
    }

    #[test]
    fn scan_tolerates_trailing_partial_object() {
        let stdout = br#"{"directive":"run"} {"directive":"en"#; // truncated
        let v = scan_last_json_object(stdout).expect("must keep the parsed first object");
        assert_eq!(v["directive"], "run");
    }

    #[test]
    fn parse_pre_run_succeeds_with_payload() {
        let bytes = br#"{"directive":"run","extra":1}"#;
        match parse_pre_directive(bytes, true) {
            PreParse::Ok { directive, payload } => {
                assert_eq!(directive, PreDirective::Run);
                assert_eq!(payload["extra"], 1);
            }
            PreParse::Failed(k) => panic!("unexpected failure {k:?}"),
        }
    }

    #[test]
    fn parse_pre_end_succeeds() {
        let bytes = br#"{"directive":"end"}"#;
        match parse_pre_directive(bytes, true) {
            PreParse::Ok { directive, .. } => assert_eq!(directive, PreDirective::End),
            PreParse::Failed(k) => panic!("unexpected failure {k:?}"),
        }
    }

    #[test]
    fn parse_pre_rejects_pre_directive() {
        // `pre` is illegal as a Pre directive (legal set is run/end).
        let bytes = br#"{"directive":"pre"}"#;
        match parse_pre_directive(bytes, true) {
            PreParse::Failed(FailureKind::SchemaDrift) => {}
            other => panic!("expected SchemaDrift, got {other:?}"),
        }
    }

    #[test]
    fn parse_pre_no_json_zero_exit_is_unparseable() {
        match parse_pre_directive(b"plain text", true) {
            PreParse::Failed(FailureKind::Unparseable) => {}
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    fn parse_pre_no_json_nonzero_exit_is_process_crash() {
        match parse_pre_directive(b"plain text", false) {
            PreParse::Failed(FailureKind::ProcessCrash) => {}
            other => panic!("expected ProcessCrash, got {other:?}"),
        }
    }

    #[test]
    fn parse_pre_object_missing_directive_field_is_unparseable() {
        let bytes = br#"{"foo":"bar"}"#;
        match parse_pre_directive(bytes, true) {
            PreParse::Failed(FailureKind::Unparseable) => {}
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    fn parse_post_pre_run_end_all_legal() {
        let cases = [("pre", PostDirective::Pre), ("run", PostDirective::Run), ("end", PostDirective::End)];
        for (s, expected) in cases {
            let body = format!(r#"{{"directive":"{}"}}"#, s);
            match parse_post_directive(body.as_bytes(), true) {
                PostParse::Ok { directive, .. } => assert_eq!(directive, expected),
                PostParse::Failed(k) => panic!("{s} should be legal, got {k:?}"),
            }
        }
    }

    #[test]
    fn parse_post_rejects_unknown_value() {
        let bytes = br#"{"directive":"halt"}"#;
        match parse_post_directive(bytes, true) {
            PostParse::Failed(FailureKind::SchemaDrift) => {}
            other => panic!("expected SchemaDrift, got {other:?}"),
        }
    }

    // Implement Debug manually for use in panic! formatting in tests.
    impl std::fmt::Debug for PreParse {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                PreParse::Ok { directive, .. } => write!(f, "Ok({directive:?})"),
                PreParse::Failed(k) => write!(f, "Failed({k:?})"),
            }
        }
    }

    impl std::fmt::Debug for PostParse {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                PostParse::Ok { directive, .. } => write!(f, "Ok({directive:?})"),
                PostParse::Failed(k) => write!(f, "Failed({k:?})"),
            }
        }
    }
}
```

- [ ] **Step 2: Run the directive tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::directive::tests`
Expected: all 12 tests pass.

- [ ] **Step 3: Verify the whole crate still compiles**

Run: `cargo build -p roki-daemon`
Expected: build succeeds.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/engine/directive.rs
git commit -m "feat(engine): directive parse with last-JSON scan" -m "scan_last_json_object iterates serde_json::StreamDeserializer top-level values, drops parse errors and non-object types, and returns the final object. Pre/Post parsers map missing JSON to Unparseable (zero exit) or ProcessCrash (non-zero exit), missing 'directive' key to Unparseable, and out-of-set values to SchemaDrift." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: `engine::context` — PhaseContext + env builder

**Files:**
- Modify: `crates/roki-daemon/src/engine/context.rs`

- [ ] **Step 1: Write the implementation with test-first ordering**

Replace `crates/roki-daemon/src/engine/context.rs` with:

```rust
//! PhaseContext: Liquid object + ROKI_* env builder.
//!
//! Every phase invocation rebuilds the Liquid object from the current
//! context state via `serde_json::to_value`. The env builder produces
//! `(name, value)` pairs scoped to the current phase: ticket / repo / cycle
//! / config fields are always exported; pre / post / run fields are only
//! exported when populated. Top-level scalars from pre/post payloads are
//! exported as `ROKI_PRE_<KEY>` / `ROKI_POST_<KEY>` per FR 01 §Inter-phase
//! data flow.

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::admission::AdmittedTicket;
use crate::config::roki::RokiConfig;
use crate::linear::ticket::NormalizedTicket;

#[derive(Debug, Clone, Serialize)]
pub struct TicketView {
    pub id: String,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignee: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoView {
    pub ghq: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CycleView {
    pub id: String,
    pub kind: &'static str,
    pub trigger: &'static str,
    pub iter: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigView {
    pub max_iterations: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub exit_code: i32,
    pub duration_seconds: u64,
}

/// Engine-side execution context. Mutated through `set_iter`, `set_pre`,
/// `set_post`, `set_run` between phase invocations.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseContext {
    pub ticket: TicketView,
    pub repo: RepoView,
    pub cycle: CycleView,
    pub config: ConfigView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunView>,
}

impl PhaseContext {
    pub fn new(admitted: &AdmittedTicket, cycle_id: Uuid, cfg: &RokiConfig) -> Self {
        Self {
            ticket: TicketView::from(&admitted.ticket),
            repo: RepoView {
                ghq: admitted.ghq.clone(),
            },
            cycle: CycleView {
                id: cycle_id.to_string(),
                kind: "rule",
                trigger: "runtime",
                iter: 0,
            },
            config: ConfigView {
                max_iterations: cfg.engine.max_iterations,
            },
            pre: None,
            post: None,
            run: None,
        }
    }

    pub fn set_iter(&mut self, iter: u32) {
        self.cycle.iter = iter;
    }

    pub fn set_pre(&mut self, payload: Value) {
        self.pre = Some(payload);
    }

    pub fn set_post(&mut self, payload: Value) {
        self.post = Some(payload);
    }

    pub fn set_run(&mut self, exit_code: i32, duration_seconds: u64) {
        self.run = Some(RunView {
            exit_code,
            duration_seconds,
        });
    }
}

impl From<&NormalizedTicket> for TicketView {
    fn from(ticket: &NormalizedTicket) -> Self {
        Self {
            id: ticket.id.clone(),
            title: ticket.title.clone(),
            body: ticket.body.clone(),
            labels: ticket.labels.clone(),
            assignee: ticket.assignee_id.clone(),
            status: ticket.status.clone(),
        }
    }
}

/// Build the `ROKI_*` env pairs the phase subprocess receives.
///
/// Returns `(name, value)` tuples ready for `Command::envs`.
pub fn roki_env_pairs(ctx: &PhaseContext) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();

    pairs.push(("ROKI_TICKET_ID".to_string(), ctx.ticket.id.clone()));
    pairs.push(("ROKI_REPO".to_string(), ctx.repo.ghq.clone()));
    pairs.push(("ROKI_CYCLE_ID".to_string(), ctx.cycle.id.clone()));
    pairs.push(("ROKI_CYCLE_KIND".to_string(), ctx.cycle.kind.to_string()));
    pairs.push((
        "ROKI_CYCLE_TRIGGER".to_string(),
        ctx.cycle.trigger.to_string(),
    ));
    pairs.push(("ROKI_CYCLE_ITER".to_string(), ctx.cycle.iter.to_string()));
    pairs.push((
        "ROKI_CONFIG_MAX_ITERATIONS".to_string(),
        ctx.config.max_iterations.to_string(),
    ));

    if let Some(payload) = ctx.pre.as_ref() {
        push_payload_scalars(&mut pairs, "ROKI_PRE_", payload);
    }
    if let Some(payload) = ctx.post.as_ref() {
        push_payload_scalars(&mut pairs, "ROKI_POST_", payload);
    }
    if let Some(run) = ctx.run.as_ref() {
        pairs.push((
            "ROKI_RUN_EXIT_CODE".to_string(),
            run.exit_code.to_string(),
        ));
        pairs.push((
            "ROKI_RUN_DURATION_SECONDS".to_string(),
            run.duration_seconds.to_string(),
        ));
    }

    pairs
}

fn push_payload_scalars(pairs: &mut Vec<(String, String)>, prefix: &str, payload: &Value) {
    let Some(map) = payload.as_object() else {
        return;
    };
    for (key, value) in map {
        let scalar = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            // Nested objects, arrays, and null are reachable through Liquid
            // (`{{ pre.foo.bar }}`) but never through env.
            _ => continue,
        };
        let upper = key.to_ascii_uppercase();
        if !upper.bytes().all(is_legal_env_char) {
            tracing::info!(
                key = %key,
                "ROKI_{prefix}* skip: key '{key}' has non [A-Z0-9_] characters",
                prefix = prefix
            );
            continue;
        }
        pairs.push((format!("{prefix}{upper}"), scalar));
    }
}

fn is_legal_env_char(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

/// Convert the context into a Liquid object (`liquid::Object`) for use as the
/// render globals.
pub fn to_liquid_object(ctx: &PhaseContext) -> liquid::Object {
    // serde_json -> liquid value via the liquid integration. Round-tripping
    // through serde_json is the simplest path that respects the existing
    // serde derives on the view types.
    let value = serde_json::to_value(ctx).expect("PhaseContext serialises");
    let object = match value {
        Value::Object(map) => map
            .into_iter()
            .map(|(k, v)| (k.into(), liquid::model::to_value(&v).unwrap_or_else(|_| liquid::model::Value::Nil)))
            .collect(),
        _ => liquid::Object::new(),
    };
    object
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> NormalizedTicket {
        NormalizedTicket::new(
            "ENG-1".to_string(),
            Some("u1".to_string()),
            "in_progress".to_string(),
            vec!["bug".to_string()],
            "Title".to_string(),
            "Body".to_string(),
        )
    }

    fn admitted() -> AdmittedTicket {
        AdmittedTicket {
            ticket: ticket(),
            ghq: "github.com/acme/widget".to_string(),
        }
    }

    fn cfg(max_iterations: u32) -> RokiConfig {
        // Build a minimal RokiConfig in-test. Reach into the public fields
        // directly; Default + struct literal is sufficient.
        use crate::config::roki::*;
        use std::path::PathBuf;
        RokiConfig {
            linear: LinearSection {
                token: "x".to_string(),
            },
            linear_webhook: LinearWebhookSection {
                bind: "127.0.0.1".to_string(),
                port: 8000,
                secret: None,
            },
            default_ai_command: DefaultAiCommandSection {
                cli: "echo".to_string(),
            },
            engine: EngineSection { max_iterations },
            paths: PathsSection {
                workflow: PathBuf::from("/tmp/w"),
                session_root: PathBuf::from("/tmp/s"),
            },
            log: LogSection::default(),
            default_ai_session: None,
        }
    }

    #[test]
    fn env_pairs_include_ticket_repo_cycle_config_at_iter_zero() {
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(7));
        let pairs = roki_env_pairs(&ctx);
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_TICKET_ID" && v == "ENG-1"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_REPO" && v == "github.com/acme/widget"));
        assert!(pairs.iter().any(|(k, _v)| k == "ROKI_CYCLE_ID"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_CYCLE_KIND" && v == "rule"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_CYCLE_TRIGGER" && v == "runtime"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_CYCLE_ITER" && v == "0"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_CONFIG_MAX_ITERATIONS" && v == "7"));
    }

    #[test]
    fn env_pairs_export_pre_top_level_scalars_only() {
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(10));
        ctx.set_pre(serde_json::json!({
            "directive": "run",
            "outcome": "success",
            "count": 3,
            "ready": true,
            "nested": {"inner": "x"},
            "list": [1, 2]
        }));
        let pairs = roki_env_pairs(&ctx);
        let names: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"ROKI_PRE_DIRECTIVE"));
        assert!(names.contains(&"ROKI_PRE_OUTCOME"));
        assert!(names.contains(&"ROKI_PRE_COUNT"));
        assert!(names.contains(&"ROKI_PRE_READY"));
        // Nested objects and arrays must be skipped.
        assert!(!names.contains(&"ROKI_PRE_NESTED"));
        assert!(!names.contains(&"ROKI_PRE_LIST"));
    }

    #[test]
    fn env_pairs_skip_keys_with_non_ascii_chars() {
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(10));
        ctx.set_pre(serde_json::json!({
            "directive": "run",
            "my-field": "x", // hyphen — uppercase is "MY-FIELD", '-' is not legal.
        }));
        let pairs = roki_env_pairs(&ctx);
        let names: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"ROKI_PRE_DIRECTIVE"));
        assert!(!names.iter().any(|n| n.contains("MY-FIELD")));
    }

    #[test]
    fn env_pairs_export_run_exit_code_and_duration() {
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(10));
        ctx.set_run(7, 42);
        let pairs = roki_env_pairs(&ctx);
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_RUN_EXIT_CODE" && v == "7"));
        assert!(pairs.iter().any(|(k, v)| k == "ROKI_RUN_DURATION_SECONDS" && v == "42"));
    }

    #[test]
    fn liquid_object_carries_ticket_repo_and_cycle_iter() {
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg(10));
        ctx.set_iter(3);
        let obj = to_liquid_object(&ctx);
        // Values are nested liquid Objects; project to JSON for cheap assertions.
        let json = serde_json::to_value(&obj).unwrap();
        assert_eq!(json["ticket"]["id"], "ENG-1");
        assert_eq!(json["repo"]["ghq"], "github.com/acme/widget");
        assert_eq!(json["cycle"]["iter"], 3);
        assert_eq!(json["config"]["max_iterations"], 10);
    }
}
```

- [ ] **Step 2: Run the context tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::context::tests`
Expected: 5 tests pass.

- [ ] **Step 3: Run the whole daemon test suite**

Run: `cargo test -p roki-daemon --features test-support`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/engine/context.rs
git commit -m "feat(engine): PhaseContext + ROKI_* env builder" -m "Carries ticket/repo/cycle/config plus most-recent pre/post/run views. roki_env_pairs flattens scalars per FR 01 inter-phase data flow; non-scalar payload fields and non-[A-Z0-9_] key chars are skipped." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: `engine::template` — Liquid render

**Files:**
- Modify: `crates/roki-daemon/src/engine/template.rs`

- [ ] **Step 1: Write failing tests + impl together**

Replace `crates/roki-daemon/src/engine/template.rs`:

```rust
//! Liquid render of argv strings and stdin bodies.
//!
//! The same `render_str` API serves all render channels: argv (the
//! pre-shell-words cli line), stdin body (path body, inline prompt), and
//! the inline cmd string. Failures map to `FailureKind::TemplateError` at
//! the call site (`engine::phase`).

use thiserror::Error;

use super::context::{to_liquid_object, PhaseContext};

/// Render error wrapper. The engine maps this to `FailureKind::TemplateError`
/// when surfacing it through `PhaseOutcome`.
#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("template parse failed: {0}")]
    Parse(String),
    #[error("template render failed: {0}")]
    Render(String),
}

/// Render `template` against `ctx`'s Liquid object. Missing variables expand
/// to the Liquid default (empty string) per Shopify Liquid semantics.
pub fn render_str(template: &str, ctx: &PhaseContext) -> Result<String, TemplateError> {
    let parser = liquid::ParserBuilder::with_stdlib()
        .build()
        .map_err(|err| TemplateError::Parse(err.to_string()))?;
    let parsed = parser
        .parse(template)
        .map_err(|err| TemplateError::Parse(err.to_string()))?;
    let object = to_liquid_object(ctx);
    parsed
        .render(&object)
        .map_err(|err| TemplateError::Render(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::AdmittedTicket;
    use crate::config::roki::*;
    use crate::linear::ticket::NormalizedTicket;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn admitted() -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                "ENG-7".to_string(),
                Some("u1".to_string()),
                "review".to_string(),
                vec!["needs-impl".to_string()],
                "Implement widget".to_string(),
                "Body".to_string(),
            ),
            ghq: "github.com/acme/widget".to_string(),
        }
    }

    fn cfg() -> RokiConfig {
        RokiConfig {
            linear: LinearSection { token: "x".to_string() },
            linear_webhook: LinearWebhookSection {
                bind: "127.0.0.1".to_string(),
                port: 8000,
                secret: None,
            },
            default_ai_command: DefaultAiCommandSection { cli: "echo".to_string() },
            engine: EngineSection { max_iterations: 10 },
            paths: PathsSection {
                workflow: PathBuf::from("/tmp/w"),
                session_root: PathBuf::from("/tmp/s"),
            },
            log: LogSection::default(),
            default_ai_session: None,
        }
    }

    #[test]
    fn renders_ticket_id_and_iter() {
        let mut ctx = super::PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        ctx.set_iter(2);
        let out = render_str("ticket {{ ticket.id }} iter {{ cycle.iter }}", &ctx).unwrap();
        assert_eq!(out, "ticket ENG-7 iter 2");
    }

    #[test]
    fn renders_pre_payload_field() {
        let mut ctx = super::PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        ctx.set_pre(serde_json::json!({"directive":"run","note":"hello"}));
        let out = render_str("pre note: {{ pre.note }}", &ctx).unwrap();
        assert_eq!(out, "pre note: hello");
    }

    #[test]
    fn missing_variable_expands_to_empty_string() {
        let ctx = super::PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        // `pre` is None at iter 0 before any pre runs; the dereference returns nil.
        let out = render_str("got [{{ pre.note }}]", &ctx).unwrap();
        assert_eq!(out, "got []");
    }

    #[test]
    fn parse_error_returns_template_error() {
        let ctx = super::PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        // Unmatched `{%` confuses the parser.
        let result = render_str("{% if foo %}", &ctx);
        match result {
            Err(TemplateError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn renders_run_exit_code_when_set() {
        let mut ctx = super::PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        ctx.set_run(5, 12);
        let out = render_str("exit={{ run.exit_code }} dur={{ run.duration_seconds }}", &ctx).unwrap();
        assert_eq!(out, "exit=5 dur=12");
    }
}
```

- [ ] **Step 2: Run the template tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::template::tests`
Expected: 5 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/roki-daemon/src/engine/template.rs
git commit -m "feat(engine): Liquid render for argv and stdin body" -m "render_str builds the Liquid object from PhaseContext and parses + renders the template. Parse and render failures map to TemplateError, which engine::phase converts to FailureKind::TemplateError." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Reshape `WorkflowConfig::Rule` for pre/run/post bodies

**Files:**
- Modify: `crates/roki-daemon/src/config/workflow.rs`
- Modify: `crates/roki-daemon/src/runtime.rs`
- Modify: `crates/roki-daemon/src/runner.rs`
- Modify: `crates/roki-daemon/src/rule.rs`

This is the biggest config change. The skeleton enforces `Rule { run_cmd: String }` and rejects `pre` / `post` blocks. Slice 1 needs `pre`, `run`, `post` as `Option<PhaseBody> / PhaseBody / Option<PhaseBody>` plus rejecting `session = "session"` at load time.

- [ ] **Step 1: Write failing tests for the new schema acceptance**

Append the following tests to the `mod tests` block in `crates/roki-daemon/src/config/workflow.rs` (before its closing `}`):

```rust
    #[test]
    fn accepts_pre_run_post_inline_cmds() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.pre]
cmd = "echo pre"
[rule.run]
cmd = "echo run"
[rule.post]
cmd = "echo post"
"#;
        let path = write_toml(&dir, body);

        let cfg = WorkflowConfig::load(&path).expect("loads ok");
        let rule = &cfg.rules[0];
        match &rule.pre {
            Some(crate::engine::outcome::PhaseBody::InlineCmd { cmd }) => assert_eq!(cmd, "echo pre"),
            other => panic!("expected pre InlineCmd, got {other:?}"),
        }
        match &rule.run {
            crate::engine::outcome::PhaseBody::InlineCmd { cmd } => assert_eq!(cmd, "echo run"),
            other => panic!("expected run InlineCmd, got {other:?}"),
        }
        match &rule.post {
            Some(crate::engine::outcome::PhaseBody::InlineCmd { cmd }) => assert_eq!(cmd, "echo post"),
            other => panic!("expected post InlineCmd, got {other:?}"),
        }
    }

    #[test]
    fn accepts_inline_prompt_form() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.pre]
prompt = "decide what to do"
[rule.run]
cmd = "echo run"
"#;
        let path = write_toml(&dir, body);

        let cfg = WorkflowConfig::load(&path).expect("loads ok");
        match &cfg.rules[0].pre {
            Some(crate::engine::outcome::PhaseBody::InlinePrompt { prompt }) => {
                assert_eq!(prompt, "decide what to do");
            }
            other => panic!("expected pre InlinePrompt, got {other:?}"),
        }
    }

    #[test]
    fn rejects_session_shape() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.pre]
session = "session"
prompt = "x"
[rule.run]
cmd = "echo run"
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path).expect_err("session shape rejected");
        match err {
            WorkflowError::UnsupportedRunForm { key, .. } => {
                assert!(key.contains("session"), "key path: {key}");
            }
            other => panic!("expected UnsupportedRunForm, got {other:?}"),
        }
    }

    #[test]
    fn rejects_run_with_both_cmd_and_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "echo a"
prompt = "do x"
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path).expect_err("both cmd+prompt is ambiguous");
        match err {
            WorkflowError::UnsupportedRunForm { .. } => {}
            other => panic!("expected UnsupportedRunForm, got {other:?}"),
        }
    }
```

The first two reference `crate::engine::outcome::PhaseBody`, which already exists from Task 5.

- [ ] **Step 2: Run the new tests and confirm failure**

Run: `cargo test -p roki-daemon --features test-support --lib config::workflow::tests::accepts_pre_run_post_inline_cmds`
Expected: compile error — `Rule.pre` doesn't exist; `rule.run` is a `String`, not `PhaseBody`.

- [ ] **Step 3: Replace `Rule` and the parsing helpers**

Edit `crates/roki-daemon/src/config/workflow.rs`. The `Rule` struct and the `parse_rule_entry` / `parse_run` helpers all change.

Replace the `Rule` struct (around lines 56–61):

```rust
/// One `[[rule]]` entry. Restricts to command-shape phases per slice 1; the
/// `session` shape is rejected at load time.
#[derive(Clone, Debug)]
pub struct Rule {
    pub when_status: String,
    pub when_labels_has_all: Vec<String>,
    pub pre: Option<crate::engine::outcome::PhaseBody>,
    pub run: crate::engine::outcome::PhaseBody,
    pub post: Option<crate::engine::outcome::PhaseBody>,
}
```

Replace `parse_rule_entry` (around lines 202–237):

```rust
fn parse_rule_entry(
    path: &Path,
    idx: usize,
    entry: &Value,
) -> Result<Rule, WorkflowError> {
    let table = entry.as_table().ok_or_else(|| {
        WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: format!("rule[{idx}]"),
        }
    })?;

    let when = parse_when(path, idx, table)?;

    // run is required, pre and post are optional.
    let run = table
        .get("run")
        .ok_or_else(|| WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: format!("rule[{idx}].run"),
        })
        .and_then(|val| parse_phase_body(path, &format!("rule[{idx}].run"), val))?;

    let pre = match table.get("pre") {
        Some(val) => Some(parse_phase_body(path, &format!("rule[{idx}].pre"), val)?),
        None => None,
    };
    let post = match table.get("post") {
        Some(val) => Some(parse_phase_body(path, &format!("rule[{idx}].post"), val)?),
        None => None,
    };

    Ok(Rule {
        when_status: when.status,
        when_labels_has_all: when.labels_has_all,
        pre,
        run,
        post,
    })
}
```

Replace `parse_run` (around lines 352–406) with a more general `parse_phase_body`:

```rust
fn parse_phase_body(
    path: &Path,
    key_prefix: &str,
    value: &Value,
) -> Result<crate::engine::outcome::PhaseBody, WorkflowError> {
    let table = value.as_table().ok_or_else(|| {
        WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: key_prefix.to_string(),
        }
    })?;

    // session = "session" is recognised but not implemented in slice 1.
    if let Some(session_val) = table.get("session") {
        let kind = session_val.as_str().unwrap_or("");
        return Err(WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: format!("{key_prefix}.session={kind}"),
        });
    }

    // Allow-list of recognised phase-body keys.
    for key in table.keys() {
        match key.as_str() {
            "cmd" | "prompt" | "path" | "cli" => {}
            other => {
                return Err(WorkflowError::UnsupportedRunForm {
                    path: path.to_path_buf(),
                    key: format!("{key_prefix}.{other}"),
                });
            }
        }
    }

    let has_cmd = table.contains_key("cmd");
    let has_prompt = table.contains_key("prompt");
    let has_path = table.contains_key("path");

    // Exactly one of cmd / prompt / path must be present.
    let count = [has_cmd, has_prompt, has_path]
        .iter()
        .filter(|present| **present)
        .count();
    if count != 1 {
        return Err(WorkflowError::UnsupportedRunForm {
            path: path.to_path_buf(),
            key: key_prefix.to_string(),
        });
    }

    if has_cmd {
        let cmd = table
            .get("cmd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| WorkflowError::UnsupportedRunForm {
                path: path.to_path_buf(),
                key: format!("{key_prefix}.cmd"),
            })?
            .to_string();
        Ok(crate::engine::outcome::PhaseBody::InlineCmd { cmd })
    } else if has_prompt {
        let prompt = table
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| WorkflowError::UnsupportedRunForm {
                path: path.to_path_buf(),
                key: format!("{key_prefix}.prompt"),
            })?
            .to_string();
        Ok(crate::engine::outcome::PhaseBody::InlinePrompt { prompt })
    } else {
        // path body
        let path_str = table
            .get("path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| WorkflowError::UnsupportedRunForm {
                path: path.to_path_buf(),
                key: format!("{key_prefix}.path"),
            })?
            .to_string();
        // Read the workflow body file lazily at phase-launch time. Until
        // then store an empty body and the path. (The phase executor
        // resolves and reads this in Task 10.)
        let cli_override = table.get("cli").and_then(Value::as_str).map(str::to_string);
        Ok(crate::engine::outcome::PhaseBody::Path {
            body: path_str,
            cli_override,
        })
    }
}
```

Note: the `Path` variant currently stores the path string in the `body` field. The phase executor in Task 10 reads the file at launch time and renders frontmatter + body. The variant name keeps `body` but its semantics here is "path string"; we'll widen the variant in Task 10 if needed. For now this representation lets the workflow loader stay file-system free.

(The `parse_phase_body` body uses `body: path_str`. That's a design choice the executor will honour — read `path_str` as a filesystem path at launch.)

- [ ] **Step 4: Drop the now-obsolete pre/post rejections from `parse_rule_entry`**

The old `parse_rule_entry` had:

```rust
if table.contains_key("pre") {
    return Err(WorkflowError::UnsupportedRunForm { ... });
}
if table.contains_key("post") {
    return Err(WorkflowError::UnsupportedRunForm { ... });
}
```

Those are gone with the rewrite above. Confirm they're absent.

- [ ] **Step 5: Adjust the existing `rejects_pre_block_on_rule` test to verify session rejection instead**

The skeleton's tests include `rejects_pre_block_on_rule`. Slice 1 accepts `pre` blocks, so this test is no longer valid. Replace it with a more useful test of the session rejection.

Find the existing `rejects_pre_block_on_rule` test (around lines 697–727 in the current file) and replace it with:

```rust
    #[test]
    fn rejects_run_with_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
[admission]
assignee = "me"

[[admission.repos]]
ghq = "github.com/acme/widget"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.run]
cmd = "echo hi"
foo = "bar"
"#;
        let path = write_toml(&dir, body);

        let err = WorkflowConfig::load(&path).expect_err("unknown run key rejected");
        match err {
            WorkflowError::UnsupportedRunForm { key, .. } => {
                assert!(key.contains("foo"), "key path: {key}");
            }
            other => panic!("expected UnsupportedRunForm, got {other:?}"),
        }
    }
```

The other rejection tests (`rejects_run_path_with_key_path`, `rejects_run_prompt`, `rejects_rule_missing_run_with_key_path`, `rejects_when_assignee_with_key_path`, `rejects_when_labels_has_any`) all stay valid against the new schema (path is now allowed, prompt is now allowed — those two tests need to be deleted because they test the old skeleton-only rejection).

Delete the existing `rejects_run_path_with_key_path` (around lines 493–522) and `rejects_run_prompt` (around lines 668–695). These tests assert that `path` and `prompt` are rejected; in slice 1 they're accepted.

- [ ] **Step 6: Update existing happy-path test to use the new `Rule` shape**

The existing `happy_path_loads_admission_repo_and_rule` test asserts `rule.run_cmd == "echo hello"`. The field is gone. Replace the assertion section with:

```rust
    #[test]
    fn happy_path_loads_admission_repo_and_rule() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, HAPPY_PATH_TOML);

        let cfg = WorkflowConfig::load(&path).expect("happy path should load");

        assert_eq!(cfg.admission.assignee, "me");
        let repo = cfg.repo.as_ref().expect("first repo present");
        assert_eq!(repo.ghq, "github.com/acme/widget");
        assert_eq!(cfg.rules.len(), 1);
        let rule = &cfg.rules[0];
        assert_eq!(rule.when_status, "In Progress");
        assert_eq!(rule.when_labels_has_all, vec!["needs-impl".to_string()]);
        match &rule.run {
            crate::engine::outcome::PhaseBody::InlineCmd { cmd } => assert_eq!(cmd, "echo hello"),
            other => panic!("expected InlineCmd, got {other:?}"),
        }
        assert!(rule.pre.is_none());
        assert!(rule.post.is_none());
    }
```

- [ ] **Step 7: Update call sites that read `rule.run_cmd`**

`crates/roki-daemon/src/runtime.rs` reads `matched_rule.run_cmd`. We need to keep that compiling for now — Task 13 swaps it out for `engine::cycle::run_cycle`. Until then, replace the access with a temporary extractor inside `runtime.rs`.

Edit `crates/roki-daemon/src/runtime.rs`. Find the line:

```rust
let _outcome = runner::spawn(&matched_rule.run_cmd, &layout).await?;
```

Replace it with:

```rust
let temp_cmd = match &matched_rule.run {
    crate::engine::outcome::PhaseBody::InlineCmd { cmd } => cmd.clone(),
    // The skeleton smoke uses InlineCmd only. Other shapes are exercised
    // through the engine::cycle path landed in Task 13.
    other => panic!("skeleton runtime path supports only InlineCmd, got {other:?}"),
};
let _outcome = runner::spawn(&temp_cmd, &layout).await?;
```

(This is intentionally hacky; Task 13 deletes the entire block.)

`crates/roki-daemon/src/rule.rs`'s tests construct `Rule` literals with `run_cmd:`. Update each `Rule { ... }` literal in `crates/roki-daemon/src/rule.rs`'s `mod tests`. The helper `fn rule(status: &str, has_all: &[&str], cmd: &str) -> Rule` should now build:

```rust
fn rule(status: &str, has_all: &[&str], cmd: &str) -> Rule {
    Rule {
        when_status: status.to_string(),
        when_labels_has_all: has_all.iter().map(|s| s.to_string()).collect(),
        pre: None,
        run: crate::engine::outcome::PhaseBody::InlineCmd { cmd: cmd.to_string() },
        post: None,
    }
}
```

The tests reference `hit.run_cmd`. Update each such assertion (`assert_eq!(hit.run_cmd, "echo a");` etc.) to:

```rust
match &hit.run {
    crate::engine::outcome::PhaseBody::InlineCmd { cmd } => assert_eq!(cmd, "echo a"),
    other => panic!("expected InlineCmd, got {other:?}"),
}
```

There are 4 such assertions in `rule.rs` (`echo a`, `echo first`, `echo a`, `echo hit`). Update all of them.

`crates/roki-daemon/src/runner.rs`: search for any `Rule` references; there should be none (runner doesn't see the rule, only the cmd string). If grep finds none, leave it.

- [ ] **Step 8: Run the workflow + rule tests**

Run: `cargo test -p roki-daemon --features test-support --lib config::workflow::tests`
Expected: all tests pass (existing + new, with the deletions noted above).

Run: `cargo test -p roki-daemon --features test-support --lib rule::tests`
Expected: all 7 tests pass.

- [ ] **Step 9: Run the full daemon test suite**

Run: `cargo test -p roki-daemon --features test-support`
Expected: all tests pass.

- [ ] **Step 10: Commit**

```bash
git add crates/roki-daemon/src/config/workflow.rs crates/roki-daemon/src/runtime.rs crates/roki-daemon/src/rule.rs
git commit -m "feat(config): rule.{pre,run,post} as PhaseBody" -m "Slice 1 accepts pre/run/post blocks per rule with cmd, prompt, or path bodies. session = \"session\" is rejected at load time. runtime.rs is patched temporarily so the existing skeleton smoke keeps working until Task 13 routes everything through engine::cycle." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: `engine::phase` — production phase executor

**Files:**
- Modify: `crates/roki-daemon/src/engine/phase.rs`

This task wires Liquid render, `ghq` base resolution, subprocess spawn, capture, and directive parsing into one `PhaseExecutor` impl.

- [ ] **Step 1: Replace `phase.rs` with the production implementation**

Write `crates/roki-daemon/src/engine/phase.rs`:

```rust
//! Phase executor for command-shape phases.
//!
//! Resolves the ghq base path once per phase invocation, Liquid-renders argv
//! and stdin body, spawns the subprocess with stdout/stderr redirected into
//! the per-iter capture files, and translates the exit status + stdout
//! contents into a `PhaseOutcome` for the cycle driver.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::error::PhaseInfraError;

use super::context::{roki_env_pairs, PhaseContext};
use super::directive::{parse_post_directive, parse_pre_directive, PostParse, PreParse};
use super::outcome::{FailureKind, PhaseBody, PhaseKind, PhaseOutcome};
use super::template::render_str;

#[async_trait]
pub trait PhaseExecutor: Send + Sync {
    async fn execute(
        &self,
        kind: PhaseKind,
        body: &PhaseBody,
        ctx: &PhaseContext,
        iter_dir: &Path,
    ) -> Result<PhaseOutcome, PhaseInfraError>;
}

/// Production phase executor for command-shape phases.
pub struct CommandPhaseExecutor {
    /// `[default.ai.command].cli` from `roki.toml`. Used as the argv source
    /// for inline-prompt and path bodies that don't carry a `cli` override.
    pub default_cli: String,
}

#[async_trait]
impl PhaseExecutor for CommandPhaseExecutor {
    async fn execute(
        &self,
        kind: PhaseKind,
        body: &PhaseBody,
        ctx: &PhaseContext,
        iter_dir: &Path,
    ) -> Result<PhaseOutcome, PhaseInfraError> {
        // 1. Resolve cwd via `ghq list -p <ghq>`.
        let cwd = resolve_ghq_base(&ctx.repo.ghq).await?;

        // 2. Build argv + stdin body.
        let (argv_template, stdin_template_opt) = match body {
            PhaseBody::InlineCmd { cmd } => {
                // sh -c <rendered>
                (format!("sh -c {}", shell_words::quote(cmd)), None)
            }
            PhaseBody::InlinePrompt { prompt } => {
                (self.default_cli.clone(), Some(prompt.clone()))
            }
            PhaseBody::Path { body: path_str, cli_override } => {
                // Read the workflow body from disk. Frontmatter is stripped;
                // anything after a closing `---` (or the whole file if no
                // frontmatter) is the rendered body. cli_override wins over
                // default_cli when present.
                let raw = match tokio::fs::read_to_string(path_str).await {
                    Ok(s) => s,
                    Err(source) => {
                        return Err(PhaseInfraError::Spawn {
                            cmd: format!("read {path_str}"),
                            source,
                        });
                    }
                };
                let body_text = strip_frontmatter(&raw).to_string();
                let cli = cli_override.clone().unwrap_or_else(|| self.default_cli.clone());
                (cli, Some(body_text))
            }
        };

        // 3. Liquid render argv + stdin.
        let argv_rendered = match render_str(&argv_template, ctx) {
            Ok(s) => s,
            Err(_) => return Ok(PhaseOutcome::Failure { kind: FailureKind::TemplateError }),
        };
        let stdin_rendered = match stdin_template_opt {
            Some(t) => match render_str(&t, ctx) {
                Ok(s) => Some(s),
                Err(_) => return Ok(PhaseOutcome::Failure { kind: FailureKind::TemplateError }),
            },
            None => None,
        };

        // 4. shell-words split argv.
        let argv = shell_words::split(&argv_rendered).map_err(|err| PhaseInfraError::Spawn {
            cmd: argv_rendered.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string()),
        })?;
        let Some((bin, rest)) = argv.split_first() else {
            return Err(PhaseInfraError::Spawn {
                cmd: argv_rendered,
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"),
            });
        };

        // 5. Open stdout / stderr capture files.
        let (stdout_file, stderr_file) = crate::capture::open_phase_files(iter_dir, kind)?;
        let stdout_handle = stdout_file.try_clone().map_err(|source| PhaseInfraError::Spawn {
            cmd: argv_rendered.clone(),
            source,
        })?;
        let stderr_handle = stderr_file.try_clone().map_err(|source| PhaseInfraError::Spawn {
            cmd: argv_rendered.clone(),
            source,
        })?;

        // 6. Build the Command.
        let env_pairs = roki_env_pairs(ctx);
        let mut cmd = Command::new(bin);
        cmd.args(rest)
            .current_dir(&cwd)
            .stdout(Stdio::from(stdout_handle))
            .stderr(Stdio::from(stderr_handle));
        if stdin_rendered.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        // env_clear so only ROKI_* + a small passthrough set is present.
        cmd.env_clear();
        for var in ["PATH", "HOME", "USER"] {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }
        for (k, v) in env_pairs {
            cmd.env(k, v);
        }

        // 7. Spawn and write stdin.
        let started = Instant::now();
        let mut child = cmd.spawn().map_err(|source| PhaseInfraError::Spawn {
            cmd: argv_rendered.clone(),
            source,
        })?;
        if let Some(body) = stdin_rendered.as_ref() {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(body.as_bytes())
                    .await
                    .map_err(|source| PhaseInfraError::Spawn {
                        cmd: argv_rendered.clone(),
                        source,
                    })?;
                drop(stdin);
            }
        }

        // 8. Wait.
        let exit_status = child.wait().await.map_err(|source| PhaseInfraError::Wait {
            cmd: argv_rendered.clone(),
            source,
        })?;
        let duration_seconds = started.elapsed().as_secs();

        // Drop the capture handles we kept so the post-exit reads see the
        // child's bytes flushed.
        drop(stdout_file);
        drop(stderr_file);

        // 9. Translate exit + stdout into PhaseOutcome.
        match kind {
            PhaseKind::Run => {
                let exit_code = exit_status.code().unwrap_or(-1);
                crate::capture::write_run_exit_code(iter_dir, exit_code)?;
                Ok(PhaseOutcome::RunDone {
                    exit_code,
                    duration_seconds,
                })
            }
            PhaseKind::Pre => {
                let stdout_path = iter_dir.join(format!("{}.stdout", kind.as_str()));
                let bytes = std::fs::read(&stdout_path)
                    .map_err(|source| PhaseInfraError::Spawn {
                        cmd: argv_rendered.clone(),
                        source,
                    })?;
                match parse_pre_directive(&bytes, exit_status.success()) {
                    PreParse::Ok { directive, payload } => {
                        crate::capture::write_response_json(iter_dir, kind, &payload)?;
                        Ok(PhaseOutcome::PreDirective { directive, payload })
                    }
                    PreParse::Failed(kind) => Ok(PhaseOutcome::Failure { kind }),
                }
            }
            PhaseKind::Post => {
                let stdout_path = iter_dir.join(format!("{}.stdout", kind.as_str()));
                let bytes = std::fs::read(&stdout_path)
                    .map_err(|source| PhaseInfraError::Spawn {
                        cmd: argv_rendered.clone(),
                        source,
                    })?;
                match parse_post_directive(&bytes, exit_status.success()) {
                    PostParse::Ok { directive, payload } => {
                        crate::capture::write_response_json(iter_dir, kind, &payload)?;
                        Ok(PhaseOutcome::PostDirective { directive, payload })
                    }
                    PostParse::Failed(kind) => Ok(PhaseOutcome::Failure { kind }),
                }
            }
        }
    }
}

/// Strip optional YAML frontmatter (`---` … `---` at file start) and return
/// the body. Returns the input unchanged when no frontmatter is present.
fn strip_frontmatter(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return raw;
    }
    let after_open = match trimmed.strip_prefix("---") {
        Some(rest) => rest.trim_start_matches('\n'),
        None => return raw,
    };
    if let Some(close_idx) = after_open.find("\n---") {
        let after_close = &after_open[close_idx + 4..]; // skip "\n---"
        return after_close.trim_start_matches('\n');
    }
    raw
}

/// Resolve the absolute path of the operator's checkout via
/// `ghq list -p <ghq>`. Returns `RepoNotFound` when ghq has no entry.
async fn resolve_ghq_base(ghq: &str) -> Result<std::path::PathBuf, PhaseInfraError> {
    let out = Command::new("ghq")
        .arg("list")
        .arg("-p")
        .arg(ghq)
        .output()
        .await
        .map_err(|source| PhaseInfraError::Spawn {
            cmd: format!("ghq list -p {ghq}"),
            source,
        })?;
    if !out.status.success() {
        return Err(PhaseInfraError::RepoNotFound {
            ghq: ghq.to_string(),
        });
    }
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PhaseInfraError::RepoNotFound {
            ghq: ghq.to_string(),
        })?;
    Ok(std::path::PathBuf::from(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::AdmittedTicket;
    use crate::config::roki::*;
    use crate::linear::ticket::NormalizedTicket;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn admitted() -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                "ENG-9".to_string(),
                Some("u1".to_string()),
                "in_progress".to_string(),
                vec![],
                "T".to_string(),
                "B".to_string(),
            ),
            // Use the workspace root as a fake ghq base in tests; the inline
            // cmds run `printf` which doesn't depend on cwd contents.
            ghq: env!("CARGO_MANIFEST_DIR").to_string(),
        }
    }

    fn cfg() -> RokiConfig {
        RokiConfig {
            linear: LinearSection { token: "x".to_string() },
            linear_webhook: LinearWebhookSection { bind: "127.0.0.1".to_string(), port: 8000, secret: None },
            default_ai_command: DefaultAiCommandSection { cli: "echo".to_string() },
            engine: EngineSection { max_iterations: 10 },
            paths: PathsSection { workflow: PathBuf::from("/tmp"), session_root: PathBuf::from("/tmp") },
            log: LogSection::default(),
            default_ai_session: None,
        }
    }

    /// Test executor that bypasses ghq resolution and uses a caller-supplied cwd.
    struct DirectExec {
        default_cli: String,
        cwd: std::path::PathBuf,
    }

    #[async_trait]
    impl PhaseExecutor for DirectExec {
        async fn execute(
            &self,
            kind: PhaseKind,
            body: &PhaseBody,
            ctx: &PhaseContext,
            iter_dir: &Path,
        ) -> Result<PhaseOutcome, PhaseInfraError> {
            // Re-implement the inline-cmd path of CommandPhaseExecutor without
            // calling `ghq list`. The test focuses on capture + directive
            // parse; ghq resolution gets its own dedicated test.
            let argv_template = match body {
                PhaseBody::InlineCmd { cmd } => format!("sh -c {}", shell_words::quote(cmd)),
                _ => panic!("DirectExec only supports InlineCmd"),
            };
            let argv_rendered = render_str(&argv_template, ctx)
                .map_err(|_| PhaseInfraError::Spawn {
                    cmd: argv_template.clone(),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "render failed"),
                })?;
            let argv = shell_words::split(&argv_rendered).unwrap();
            let (bin, rest) = argv.split_first().unwrap();
            let (stdout_file, stderr_file) = crate::capture::open_phase_files(iter_dir, kind)?;
            let stdout_handle = stdout_file.try_clone().unwrap();
            let stderr_handle = stderr_file.try_clone().unwrap();
            let started = Instant::now();
            let mut child = Command::new(bin)
                .args(rest)
                .current_dir(&self.cwd)
                .stdout(Stdio::from(stdout_handle))
                .stderr(Stdio::from(stderr_handle))
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            let exit_status = child.wait().await.unwrap();
            let duration_seconds = started.elapsed().as_secs();
            drop(stdout_file);
            drop(stderr_file);
            let _ = self.default_cli.len(); // silence dead_code

            match kind {
                PhaseKind::Run => {
                    let exit_code = exit_status.code().unwrap_or(-1);
                    crate::capture::write_run_exit_code(iter_dir, exit_code)?;
                    Ok(PhaseOutcome::RunDone { exit_code, duration_seconds })
                }
                PhaseKind::Pre => {
                    let bytes = std::fs::read(iter_dir.join("pre.stdout")).unwrap();
                    match parse_pre_directive(&bytes, exit_status.success()) {
                        PreParse::Ok { directive, payload } => {
                            crate::capture::write_response_json(iter_dir, kind, &payload)?;
                            Ok(PhaseOutcome::PreDirective { directive, payload })
                        }
                        PreParse::Failed(k) => Ok(PhaseOutcome::Failure { kind: k }),
                    }
                }
                PhaseKind::Post => {
                    let bytes = std::fs::read(iter_dir.join("post.stdout")).unwrap();
                    match parse_post_directive(&bytes, exit_status.success()) {
                        PostParse::Ok { directive, payload } => {
                            crate::capture::write_response_json(iter_dir, kind, &payload)?;
                            Ok(PhaseOutcome::PostDirective { directive, payload })
                        }
                        PostParse::Failed(k) => Ok(PhaseOutcome::Failure { kind: k }),
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn run_phase_writes_exit_code_and_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir = crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let exec = DirectExec {
            default_cli: "echo".to_string(),
            cwd: tmp.path().to_path_buf(),
        };
        let mut ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        ctx.set_iter(1);
        let body = PhaseBody::InlineCmd { cmd: "printf hello; printf err 1>&2; exit 5".into() };

        let out = exec.execute(PhaseKind::Run, &body, &ctx, &iter_dir).await.unwrap();
        match out {
            PhaseOutcome::RunDone { exit_code, .. } => assert_eq!(exit_code, 5),
            other => panic!("unexpected outcome: {other:?}"),
        }
        let exit_text = std::fs::read_to_string(iter_dir.join("run.exit_code")).unwrap();
        assert_eq!(exit_text.trim(), "5");
        let stdout_bytes = std::fs::read_to_string(iter_dir.join("run.stdout")).unwrap();
        assert!(stdout_bytes.contains("hello"));
        let stderr_bytes = std::fs::read_to_string(iter_dir.join("run.stderr")).unwrap();
        assert!(stderr_bytes.contains("err"));
    }

    #[tokio::test]
    async fn pre_phase_parses_directive_and_writes_response_json() {
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir = crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let exec = DirectExec {
            default_cli: "echo".to_string(),
            cwd: tmp.path().to_path_buf(),
        };
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        let body = PhaseBody::InlineCmd {
            cmd: r#"printf '{"directive":"run","outcome":"ok"}'"#.to_string(),
        };

        let out = exec.execute(PhaseKind::Pre, &body, &ctx, &iter_dir).await.unwrap();
        match out {
            PhaseOutcome::PreDirective { directive, payload } => {
                assert_eq!(directive, super::PreDirective::Run);
                assert_eq!(payload["outcome"], "ok");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        let resp_path = iter_dir.join("pre.response.json");
        let resp = std::fs::read_to_string(&resp_path).unwrap();
        assert!(resp.contains("\"directive\""));
    }

    #[tokio::test]
    async fn pre_phase_unparseable_yields_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let iter_dir = crate::capture::create_iter_dir(tmp.path(), "ENG-9", Uuid::nil(), 1).unwrap();
        let exec = DirectExec {
            default_cli: "echo".to_string(),
            cwd: tmp.path().to_path_buf(),
        };
        let ctx = PhaseContext::new(&admitted(), Uuid::nil(), &cfg());
        let body = PhaseBody::InlineCmd { cmd: r#"printf 'not json'"#.to_string() };

        let out = exec.execute(PhaseKind::Pre, &body, &ctx, &iter_dir).await.unwrap();
        match out {
            PhaseOutcome::Failure { kind: FailureKind::Unparseable } => {}
            other => panic!("expected Unparseable failure, got {other:?}"),
        }
    }

    #[test]
    fn strip_frontmatter_returns_body_after_yaml() {
        let raw = "---\nfoo: bar\n---\nbody-line\nmore\n";
        assert_eq!(super::strip_frontmatter(raw), "body-line\nmore\n");
    }

    #[test]
    fn strip_frontmatter_passthrough_when_absent() {
        let raw = "no frontmatter here";
        assert_eq!(super::strip_frontmatter(raw), raw);
    }
}

use super::outcome::{PostDirective, PreDirective}; // used by the test module
```

The trailing `use` re-imports the directive enums for the test module's `PhaseOutcome::PreDirective` pattern. (It's harmless at the file level — `PreDirective` is already in scope for the parse helpers.)

- [ ] **Step 2: Run engine::phase tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::phase::tests`
Expected: 5 tests pass.

The phase tests don't actually exercise `CommandPhaseExecutor` directly because that calls `ghq list` (which would need a real ghq install). They cover the equivalent inline-cmd path via `DirectExec`. The `CommandPhaseExecutor::resolve_ghq_base` path is exercised end-to-end by the iteration smoke (Task 15), which uses a temp directory created by hand and skips ghq via a follow-up runtime tweak.

Note: the `iteration_smoke` test in Task 15 needs `CommandPhaseExecutor` to run without a real `ghq` install. We address that by adding a `ROKI_GHQ_BIN` test-support env var in Task 15. For now, leave `resolve_ghq_base` as-is.

- [ ] **Step 3: Run the full daemon test suite**

Run: `cargo test -p roki-daemon --features test-support`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/roki-daemon/src/engine/phase.rs
git commit -m "feat(engine): command-shape phase executor" -m "Renders argv/stdin via Liquid, spawns the subprocess with ROKI_* env, captures stdout/stderr to the per-iter files, and translates exit + stdout into PhaseOutcome (RunDone for run; PreDirective/PostDirective for pre/post). Path-body frontmatter strip + ghq base resolution included." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: `engine::cycle` — iteration loop with cap

**Files:**
- Modify: `crates/roki-daemon/src/engine/cycle.rs`

- [ ] **Step 1: Write failing tests + impl**

Replace `crates/roki-daemon/src/engine/cycle.rs`:

```rust
//! Cycle driver: iteration loop, transitions, iter cap.
//!
//! `run_cycle` consumes a `PhaseExecutor` (production or fake) so unit tests
//! exercise every directive transition deterministically. The daemon's
//! `runtime::run_inner` builds a `CommandPhaseExecutor` and passes it in.

use std::path::Path;

use uuid::Uuid;

use crate::admission::AdmittedTicket;
use crate::config::roki::RokiConfig;
use crate::config::workflow::Rule;
use crate::error::PhaseInfraError;

use super::context::PhaseContext;
use super::outcome::{
    FailureKind, PhaseKind, PhaseOutcome, PostDirective, PreDirective,
};
use super::phase::PhaseExecutor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed { iters: u32 },
    Failed { kind: FailureKind, iter: u32 },
}

/// Drive one cycle to completion or failure.
pub async fn run_cycle(
    executor: &dyn PhaseExecutor,
    admitted: &AdmittedTicket,
    rule: &Rule,
    session_root: &Path,
    cfg: &RokiConfig,
) -> Result<CycleOutcome, PhaseInfraError> {
    let cycle_id = Uuid::new_v4();
    let mut ctx = PhaseContext::new(admitted, cycle_id, cfg);
    let max_iter = cfg.engine.max_iterations;
    let ticket_id = admitted.ticket.id.clone();
    let mut skip_pre = false;

    for iter in 1..=max_iter {
        ctx.set_iter(iter);
        let iter_dir =
            crate::capture::create_iter_dir(session_root, &ticket_id, cycle_id, iter)?;

        // Pre.
        if let Some(pre_body) = rule.pre.as_ref() {
            if !skip_pre {
                match executor.execute(PhaseKind::Pre, pre_body, &ctx, &iter_dir).await? {
                    PhaseOutcome::Failure { kind } => {
                        return Ok(CycleOutcome::Failed { kind, iter });
                    }
                    PhaseOutcome::PreDirective {
                        directive: PreDirective::End,
                        payload,
                    } => {
                        ctx.set_pre(payload);
                        return Ok(CycleOutcome::Completed { iters: iter });
                    }
                    PhaseOutcome::PreDirective {
                        directive: PreDirective::Run,
                        payload,
                    } => {
                        ctx.set_pre(payload);
                    }
                    other => panic!("Pre executor returned non-Pre outcome: {other:?}"),
                }
            }
        }
        skip_pre = false;

        // Run.
        match executor.execute(PhaseKind::Run, &rule.run, &ctx, &iter_dir).await? {
            PhaseOutcome::Failure { kind } => {
                return Ok(CycleOutcome::Failed { kind, iter });
            }
            PhaseOutcome::RunDone {
                exit_code,
                duration_seconds,
            } => {
                ctx.set_run(exit_code, duration_seconds);
            }
            other => panic!("Run executor returned non-Run outcome: {other:?}"),
        }

        // Post.
        let next = if let Some(post_body) = rule.post.as_ref() {
            match executor.execute(PhaseKind::Post, post_body, &ctx, &iter_dir).await? {
                PhaseOutcome::Failure { kind } => {
                    return Ok(CycleOutcome::Failed { kind, iter });
                }
                PhaseOutcome::PostDirective { directive, payload } => {
                    ctx.set_post(payload);
                    directive
                }
                other => panic!("Post executor returned non-Post outcome: {other:?}"),
            }
        } else {
            PostDirective::End
        };

        match next {
            PostDirective::End => return Ok(CycleOutcome::Completed { iters: iter }),
            PostDirective::Pre => {
                if iter == max_iter {
                    return Ok(CycleOutcome::Failed { kind: FailureKind::IterExhausted, iter });
                }
                skip_pre = false;
            }
            PostDirective::Run => {
                if iter == max_iter {
                    return Ok(CycleOutcome::Failed { kind: FailureKind::IterExhausted, iter });
                }
                skip_pre = true;
            }
        }
    }

    // Unreachable: every transition either continues, returns Completed, or
    // returns IterExhausted. Defensive return for type completeness.
    Ok(CycleOutcome::Failed {
        kind: FailureKind::IterExhausted,
        iter: max_iter,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    use crate::admission::AdmittedTicket;
    use crate::config::roki::*;
    use crate::engine::outcome::{PhaseBody, PreDirective};
    use crate::linear::ticket::NormalizedTicket;
    use std::path::PathBuf;

    fn admitted() -> AdmittedTicket {
        AdmittedTicket {
            ticket: NormalizedTicket::new(
                "ENG-CYC".to_string(),
                Some("u1".to_string()),
                "in_progress".to_string(),
                vec![],
                "T".to_string(),
                "B".to_string(),
            ),
            ghq: "github.com/acme/widget".to_string(),
        }
    }

    fn cfg(max_iter: u32) -> RokiConfig {
        RokiConfig {
            linear: LinearSection { token: "x".to_string() },
            linear_webhook: LinearWebhookSection {
                bind: "127.0.0.1".to_string(),
                port: 8000,
                secret: None,
            },
            default_ai_command: DefaultAiCommandSection { cli: "echo".to_string() },
            engine: EngineSection { max_iterations: max_iter },
            paths: PathsSection {
                workflow: PathBuf::from("/tmp/w"),
                session_root: PathBuf::from("/tmp/s"),
            },
            log: LogSection::default(),
            default_ai_session: None,
        }
    }

    fn rule(pre: Option<PhaseBody>, post: Option<PhaseBody>) -> Rule {
        Rule {
            when_status: "in_progress".to_string(),
            when_labels_has_all: vec![],
            pre,
            run: PhaseBody::InlineCmd { cmd: "true".to_string() },
            post,
        }
    }

    /// Fake executor. Returns canned outcomes per (iter, phase). Records calls.
    struct FakeExec {
        scripted: Mutex<Vec<(u32, PhaseKind, PhaseOutcome)>>,
        calls: Mutex<Vec<(u32, PhaseKind)>>,
    }

    impl FakeExec {
        fn new(scripted: Vec<(u32, PhaseKind, PhaseOutcome)>) -> Self {
            Self {
                scripted: Mutex::new(scripted),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl PhaseExecutor for FakeExec {
        async fn execute(
            &self,
            kind: PhaseKind,
            _body: &PhaseBody,
            ctx: &PhaseContext,
            _iter_dir: &Path,
        ) -> Result<PhaseOutcome, PhaseInfraError> {
            self.calls.lock().unwrap().push((ctx.cycle.iter, kind));
            // Find the first scripted entry matching (iter, kind) and remove it.
            let mut scripted = self.scripted.lock().unwrap();
            let pos = scripted
                .iter()
                .position(|(i, k, _)| *i == ctx.cycle.iter && *k == kind)
                .unwrap_or_else(|| panic!("no scripted outcome for ({}, {:?})", ctx.cycle.iter, kind));
            let (_, _, out) = scripted.remove(pos);
            Ok(out)
        }
    }

    #[tokio::test]
    async fn pre_end_short_circuits_before_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![(
            1,
            PhaseKind::Pre,
            PhaseOutcome::PreDirective {
                directive: PreDirective::End,
                payload: serde_json::json!({"directive":"end"}),
            },
        )]);
        let r = rule(Some(PhaseBody::InlineCmd { cmd: "true".into() }), None);
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 1 });
        let calls = exec.calls.lock().unwrap().clone();
        assert_eq!(calls, vec![(1, PhaseKind::Pre)]);
    }

    #[tokio::test]
    async fn full_iter_pre_run_post_end() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![
            (
                1,
                PhaseKind::Pre,
                PhaseOutcome::PreDirective {
                    directive: PreDirective::Run,
                    payload: serde_json::json!({"directive":"run"}),
                },
            ),
            (
                1,
                PhaseKind::Run,
                PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 1 },
            ),
            (
                1,
                PhaseKind::Post,
                PhaseOutcome::PostDirective {
                    directive: PostDirective::End,
                    payload: serde_json::json!({"directive":"end"}),
                },
            ),
        ]);
        let r = rule(
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
        );
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 1 });
    }

    #[tokio::test]
    async fn post_run_skips_pre_in_next_iteration() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![
            (1, PhaseKind::Pre, PhaseOutcome::PreDirective {
                directive: PreDirective::Run,
                payload: serde_json::json!({}),
            }),
            (1, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (1, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::Run,
                payload: serde_json::json!({}),
            }),
            // Iter 2: pre must be SKIPPED. Run again, then post=end.
            (2, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (2, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::End,
                payload: serde_json::json!({}),
            }),
        ]);
        let r = rule(
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
            Some(PhaseBody::InlineCmd { cmd: "true".into() }),
        );
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 2 });
        let calls = exec.calls.lock().unwrap().clone();
        let pre_iter2 = calls.iter().find(|(i, k)| *i == 2 && *k == PhaseKind::Pre);
        assert!(pre_iter2.is_none(), "iter 2 pre must be skipped, calls: {calls:?}");
    }

    #[tokio::test]
    async fn iter_cap_with_post_run_yields_iter_exhausted() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![
            (1, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (1, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::Run,
                payload: serde_json::json!({}),
            }),
            (2, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (2, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::Run,
                payload: serde_json::json!({}),
            }),
        ]);
        let r = rule(None, Some(PhaseBody::InlineCmd { cmd: "true".into() }));
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(2)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Failed { kind: FailureKind::IterExhausted, iter: 2 });
    }

    #[tokio::test]
    async fn post_absent_terminates_after_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![(
            1,
            PhaseKind::Run,
            PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 },
        )]);
        let r = rule(None, None);
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 1 });
    }

    #[tokio::test]
    async fn pre_absent_starts_at_run() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![
            (1, PhaseKind::Run, PhaseOutcome::RunDone { exit_code: 0, duration_seconds: 0 }),
            (1, PhaseKind::Post, PhaseOutcome::PostDirective {
                directive: PostDirective::End,
                payload: serde_json::json!({}),
            }),
        ]);
        let r = rule(None, Some(PhaseBody::InlineCmd { cmd: "true".into() }));
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Completed { iters: 1 });
    }

    #[tokio::test]
    async fn pre_failure_returns_failed_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = FakeExec::new(vec![(
            1,
            PhaseKind::Pre,
            PhaseOutcome::Failure { kind: FailureKind::Unparseable },
        )]);
        let r = rule(Some(PhaseBody::InlineCmd { cmd: "true".into() }), None);
        let outcome =
            run_cycle(&exec, &admitted(), &r, tmp.path(), &cfg(10)).await.unwrap();
        assert_eq!(outcome, CycleOutcome::Failed { kind: FailureKind::Unparseable, iter: 1 });
    }
}
```

Note: `cycle.rs` references `crate::capture::create_iter_dir`. That function is added in Task 12. To get a clean TDD progression, build cycle and capture together.

- [ ] **Step 2: Run the cycle tests (will fail because capture API doesn't exist yet)**

Run: `cargo build -p roki-daemon --tests`
Expected: compile error referencing `crate::capture::create_iter_dir`. That sets up Task 12 cleanly.

Defer the cycle test run until after Task 12.

- [ ] **Step 3: Commit (fail-forward)**

```bash
git add crates/roki-daemon/src/engine/cycle.rs
git commit -m "feat(engine): cycle driver with iter cap" -m "run_cycle iterates pre/run/post per FR 01 directive transitions, skipping pre on post=run, terminating on pre=end / post=end / post-absent. Iter cap raises IterExhausted when post returns pre/run on the final iteration. Capture API integration follows in the next task." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: Rewrite `capture.rs` for the canonical layout

**Files:**
- Modify: `crates/roki-daemon/src/capture.rs`

The skeleton's `capture::create` returns a `CaptureLayout { dir, stdout, stderr }`. Slice 1 splits this into per-iter directory creation, per-phase file open, and helpers for `<phase>.response.json` / `run.exit_code`.

- [ ] **Step 1: Rewrite the module**

Replace `crates/roki-daemon/src/capture.rs` with:

```rust
//! Per-iter capture layout.
//!
//! Layout: `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{pre,run,post}.{stdout,stderr}`
//! plus parsed-derivative files (`pre.response.json`, `run.exit_code`,
//! `post.response.json`). The skeleton's flat `cycle-<uuid>/{stdout,stderr}`
//! layout is gone.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::engine::outcome::PhaseKind;
use crate::error::CaptureError;

/// Sanitise a ticket id for filesystem use. Keeps `[A-Za-z0-9_-]`; replaces
/// every other byte (slashes, spaces, unicode) with `_`. Linear-style ids
/// like `ENG-123` survive verbatim.
pub fn sanitize_ticket_id(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => b as char,
            _ => '_',
        })
        .collect()
}

/// Create `<session_root>/<sanitised_ticket>/cycle-<uuid>/iter-<n>/` and
/// return its path. The directory is empty until `open_phase_files` is
/// called for each phase.
pub fn create_iter_dir(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
    iter: u32,
) -> Result<PathBuf, CaptureError> {
    let safe_ticket = sanitize_ticket_id(ticket_id);
    let path = session_root
        .join(safe_ticket)
        .join(format!("cycle-{cycle_id}"))
        .join(format!("iter-{iter}"));
    fs::create_dir_all(&path).map_err(|source| CaptureError::CreateDir {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Open `<phase>.stdout` and `<phase>.stderr` inside `iter_dir`. Returns the
/// pair `(stdout, stderr)` ready for `Stdio::from(File)` redirection.
pub fn open_phase_files(
    iter_dir: &Path,
    phase: PhaseKind,
) -> Result<(File, File), CaptureError> {
    let stdout_path = iter_dir.join(format!("{}.stdout", phase.as_str()));
    let stderr_path = iter_dir.join(format!("{}.stderr", phase.as_str()));
    let stdout = File::create(&stdout_path).map_err(|source| CaptureError::OpenFile {
        path: stdout_path,
        source,
    })?;
    let stderr = File::create(&stderr_path).map_err(|source| CaptureError::OpenFile {
        path: stderr_path,
        source,
    })?;
    Ok((stdout, stderr))
}

/// Write `<phase>.response.json` (pretty-printed) inside `iter_dir`. Used
/// after a successful Pre or Post directive parse.
pub fn write_response_json(
    iter_dir: &Path,
    phase: PhaseKind,
    value: &serde_json::Value,
) -> Result<(), CaptureError> {
    let path = iter_dir.join(format!("{}.response.json", phase.as_str()));
    let pretty = serde_json::to_vec_pretty(value).map_err(|err| CaptureError::Write {
        path: path.clone(),
        source: std::io::Error::other(err),
    })?;
    let mut file = File::create(&path).map_err(|source| CaptureError::OpenFile {
        path: path.clone(),
        source,
    })?;
    file.write_all(&pretty).map_err(|source| CaptureError::Write {
        path,
        source,
    })
}

/// Write `run.exit_code` inside `iter_dir`. The text contents are
/// `"<exit>\n"`.
pub fn write_run_exit_code(iter_dir: &Path, exit_code: i32) -> Result<(), CaptureError> {
    let path = iter_dir.join("run.exit_code");
    let mut file = File::create(&path).map_err(|source| CaptureError::OpenFile {
        path: path.clone(),
        source,
    })?;
    let body = format!("{exit_code}\n");
    file.write_all(body.as_bytes()).map_err(|source| CaptureError::Write {
        path,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_iter_dir_builds_full_path() {
        let tmp = TempDir::new().unwrap();
        let path = create_iter_dir(tmp.path(), "ENG-1", Uuid::nil(), 3).unwrap();
        assert!(path.exists());
        let s = path.to_string_lossy();
        assert!(s.contains("ENG-1"));
        assert!(s.contains(&format!("cycle-{}", Uuid::nil())));
        assert!(s.ends_with("iter-3"));
    }

    #[test]
    fn sanitiser_keeps_safe_chars_replaces_others() {
        assert_eq!(sanitize_ticket_id("ENG-123"), "ENG-123");
        assert_eq!(sanitize_ticket_id("a/b c"), "a_b_c");
        assert_eq!(sanitize_ticket_id("x_y-z"), "x_y-z");
    }

    #[test]
    fn open_phase_files_creates_stdout_and_stderr() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let (out, err) = open_phase_files(&dir, PhaseKind::Run).unwrap();
        drop(out);
        drop(err);
        assert!(dir.join("run.stdout").is_file());
        assert!(dir.join("run.stderr").is_file());
    }

    #[test]
    fn write_response_json_writes_pretty_payload() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let value = serde_json::json!({"directive":"run","note":"hi"});
        write_response_json(&dir, PhaseKind::Pre, &value).unwrap();
        let body = std::fs::read_to_string(dir.join("pre.response.json")).unwrap();
        assert!(body.contains("\"directive\""));
        assert!(body.contains("\"hi\""));
    }

    #[test]
    fn write_run_exit_code_writes_text() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        write_run_exit_code(&dir, 7).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("run.exit_code")).unwrap(),
            "7\n"
        );
    }

    #[test]
    fn unwritable_session_root_returns_create_dir_error() {
        let tmp = TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let bad_root = blocker.join("subdir");
        match create_iter_dir(&bad_root, "X", Uuid::nil(), 1) {
            Err(CaptureError::CreateDir { .. }) => {}
            other => panic!("expected CreateDir error, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run capture tests**

Run: `cargo test -p roki-daemon --features test-support --lib capture::tests`
Expected: 6 tests pass.

- [ ] **Step 3: Run cycle tests (now that capture API exists)**

Run: `cargo test -p roki-daemon --features test-support --lib engine::cycle::tests`
Expected: 7 tests pass.

- [ ] **Step 4: Run engine::phase tests**

Run: `cargo test -p roki-daemon --features test-support --lib engine::phase::tests`
Expected: 5 tests pass.

- [ ] **Step 5: Run the whole crate's library tests**

Run: `cargo test -p roki-daemon --features test-support --lib`
Expected: every unit test passes.

The skeleton smoke test (`tests/e2e/skeleton_smoke.rs`) still asserts the **old** layout (`cycle-<uuid>/{stdout,stderr}`), so it will fail. That update is Task 14.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/capture.rs
git commit -m "feat(capture): per-iter directory + per-phase file API" -m "<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{phase}.{stdout,stderr} layout, plus write_response_json and write_run_exit_code helpers. CaptureLayout struct is gone; engine::phase opens files inline." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Wire `runtime::run_inner` to `engine::cycle` and delete `runner.rs`

**Files:**
- Modify: `crates/roki-daemon/src/runtime.rs`
- Modify: `crates/roki-daemon/src/main.rs`
- Delete: `crates/roki-daemon/src/runner.rs`

- [ ] **Step 1: Replace the cycle execution block in `runtime::run_inner`**

Edit `crates/roki-daemon/src/runtime.rs`. Locate the post-loop block (currently lines 154–171 in the skeleton — search for `cycle_started.store(true, Ordering::Release);`). Replace everything from that line up to and including the final `drop(layout);` with:

```rust
    // 7. Lock the cycle, drop the receiver, dispatch into the engine.
    cycle_started.store(true, Ordering::Release);
    drop(rx);

    let executor = crate::engine::CommandPhaseExecutor {
        default_cli: cfg.default_ai_command.cli.clone(),
    };
    let outcome = crate::engine::run_cycle(
        &executor,
        &admitted,
        &matched_rule,
        &cfg.paths.session_root,
        &cfg,
    )
    .await?;
```

Then add cycle-outcome → exit-result mapping. Find the current shutdown block (lines beginning with `let _ = shutdown_tx.send(());`). Replace its body so failures map to `Err(...)`:

```rust
    // 8. Graceful shutdown.
    let _ = shutdown_tx.send(());
    let listener_result = listener_handle.await;

    // Cycle failures terminate the binary with exit 1, matching the spec's
    // "exit 1 on Failed" mapping. The listener result still propagates so
    // the operator sees both kinds of failure.
    match outcome {
        crate::engine::CycleOutcome::Completed { .. } => match listener_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(SkeletonError::Webhook(err)),
            Err(join_err) => Err(SkeletonError::Webhook(WebhookError::BindFailed {
                addr: addr.to_string(),
                source: std::io::Error::other(join_err),
            })),
        },
        crate::engine::CycleOutcome::Failed { kind, iter } => {
            tracing::error!(
                failure_kind = %kind.as_str(),
                iter,
                "cycle failed"
            );
            // Drain the listener join handle so it doesn't leak; ignore
            // shutdown errors after a cycle failure.
            let _ = listener_result;
            Err(SkeletonError::PhaseInfra(crate::error::PhaseInfraError::RepoNotFound {
                ghq: format!("cycle failed: {} at iter {}", kind.as_str(), iter),
            }))
        }
    }
```

The `Failed` arm reuses `PhaseInfraError::RepoNotFound` only because we don't yet have a dedicated `CycleFailed` variant. To keep the surface tidy, add one. Edit `crates/roki-daemon/src/error.rs` and add to `PhaseInfraError`:

```rust
    #[error("cycle failed: {kind} at iter {iter}")]
    CycleFailed {
        kind: &'static str,
        iter: u32,
    },
```

Then change the runtime `Err` arm to use it:

```rust
            Err(SkeletonError::PhaseInfra(
                crate::error::PhaseInfraError::CycleFailed {
                    kind: kind.as_str(),
                    iter,
                },
            ))
```

- [ ] **Step 2: Remove the temporary `temp_cmd` shim and the `runner` import**

In `runtime.rs`, delete the temporary block from Task 9 Step 7:

```rust
let temp_cmd = match &matched_rule.run { ... };
let _outcome = runner::spawn(&temp_cmd, &layout).await?;
```

(That block has already been replaced by the engine dispatch in Step 1; this step ensures it's gone.)

Find and remove `use crate::runner;` (or any `crate::runner` reference) at the top of `runtime.rs`. Find and remove `use crate::capture;` and the `let layout = capture::create(...)` block — the engine handles capture per phase.

The `use crate::admission;` and the admission/rule matching logic above section 6 stay intact.

- [ ] **Step 3: Delete `runner.rs`**

```bash
git rm crates/roki-daemon/src/runner.rs
```

- [ ] **Step 4: Drop `mod runner;` from main.rs**

Edit `crates/roki-daemon/src/main.rs`. Remove the line `mod runner;`.

- [ ] **Step 5: Build and confirm compile**

Run: `cargo build -p roki-daemon`
Expected: clean build, no references to `runner` or `RunnerError`.

- [ ] **Step 6: Run the lib unit tests**

Run: `cargo test -p roki-daemon --features test-support --lib`
Expected: every unit test passes.

- [ ] **Step 7: Commit**

```bash
git add crates/roki-daemon/src/runtime.rs crates/roki-daemon/src/main.rs crates/roki-daemon/src/error.rs
git commit -m "feat(runtime): dispatch through engine::cycle, drop runner.rs" -m "runtime::run_inner now builds a CommandPhaseExecutor from [default.ai.command].cli and delegates the cycle to engine::cycle::run_cycle. CycleFailed surfaces as PhaseInfraError::CycleFailed -> ExitCode::FAILURE." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 14: Update `skeleton_smoke.rs` for the new layout

**Files:**
- Modify: `crates/roki-daemon/tests/e2e/skeleton_smoke.rs`

The skeleton smoke posts one webhook, runs one rule with `cmd = "printf out; printf err 1>&2"`, and used to read `<session_root>/cycle-<uuid>/{stdout,stderr}`. The new layout is `<session_root>/<ticket-id>/cycle-<uuid>/iter-1/run.{stdout,stderr}`.

Also: the skeleton smoke spawns the binary which calls `ghq list -p github.com/example/repo` from inside `CommandPhaseExecutor::resolve_ghq_base`. That command must succeed. Since the test runs in a tempdir and ghq probably has no entry for `github.com/example/repo`, we need a test-support seam to bypass it — the simplest path is to honour an env var that overrides cwd.

- [ ] **Step 1: Add the cwd override seam to `engine::phase`**

Edit `crates/roki-daemon/src/engine/phase.rs`. Replace the body of `resolve_ghq_base` with:

```rust
async fn resolve_ghq_base(ghq: &str) -> Result<std::path::PathBuf, PhaseInfraError> {
    // Test-support seam: if `ROKI_GHQ_BASE_OVERRIDE` is set, use it directly.
    // The release binary never reads this env var because the integration
    // test sets it per-spawn; production env never has it.
    if let Ok(override_path) = std::env::var("ROKI_GHQ_BASE_OVERRIDE") {
        if !override_path.is_empty() {
            return Ok(std::path::PathBuf::from(override_path));
        }
    }
    let out = Command::new("ghq")
        .arg("list")
        .arg("-p")
        .arg(ghq)
        .output()
        .await
        .map_err(|source| PhaseInfraError::Spawn {
            cmd: format!("ghq list -p {ghq}"),
            source,
        })?;
    if !out.status.success() {
        return Err(PhaseInfraError::RepoNotFound {
            ghq: ghq.to_string(),
        });
    }
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PhaseInfraError::RepoNotFound {
            ghq: ghq.to_string(),
        })?;
    Ok(std::path::PathBuf::from(line))
}
```

The override is intentionally honoured in production binaries too — operators that want to point cwd at a local checkout regardless of ghq can use it. We only document this for slice 2; for slice 1 it's a test-support convenience that doesn't require a `#[cfg(feature)]` gate.

- [ ] **Step 2: Update the smoke assertions**

Edit `crates/roki-daemon/tests/e2e/skeleton_smoke.rs`. Replace the section after the binary spawn with the following. Find the block starting `let cycle_dir = std::fs::read_dir(&session_root)` (around line 203) and replace from there to the end of the function with:

```rust
    // 9. Locate the per-iter capture dir and read the run stdout / stderr
    //    files. Layout per `capture::create_iter_dir`:
    //    `<session_root>/<ticket-id>/cycle-<uuid>/iter-1/run.{stdout,stderr}`.
    let ticket_dir = session_root.join("tid-1");
    assert!(
        ticket_dir.is_dir(),
        "Req 7.2: ticket dir must exist at {ticket_dir:?}"
    );
    let cycle_entry = std::fs::read_dir(&ticket_dir)
        .expect("ticket dir readable")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("cycle-"))
        .expect("cycle-<uuid> dir should exist under ticket dir");
    let iter1 = cycle_entry.path().join("iter-1");
    assert!(iter1.is_dir(), "iter-1 dir must exist at {iter1:?}");

    let stdout_bytes = std::fs::read_to_string(iter1.join("run.stdout"))
        .expect("read run.stdout");
    let stderr_bytes = std::fs::read_to_string(iter1.join("run.stderr"))
        .expect("read run.stderr");
    assert!(
        stdout_bytes.contains("out"),
        "Req 7.2: run.stdout must contain `out`, got {stdout_bytes:?}"
    );
    assert!(
        stderr_bytes.contains("err"),
        "Req 7.2: run.stderr must contain `err`, got {stderr_bytes:?}"
    );

    let exit_code_text = std::fs::read_to_string(iter1.join("run.exit_code"))
        .expect("read run.exit_code");
    assert_eq!(
        exit_code_text.trim(),
        "0",
        "exit code file must contain the run subprocess exit (0 here)"
    );
}
```

- [ ] **Step 3: Set `ROKI_GHQ_BASE_OVERRIDE` when spawning the binary**

In the same file, find the `Command::new(binary)` block (around line 114). Add an extra `.env(...)` call before `.spawn()`:

```rust
    let mut child = Command::new(binary)
        .arg("run")
        .arg("--config")
        .arg(&roki_path)
        .env("ROKI_LINEAR_GRAPHQL_URL", linear.uri())
        .env("ROKI_GHQ_BASE_OVERRIDE", work.path())
        .kill_on_drop(true)
```

This points cwd at the test workspace tempdir. The `printf` cmd doesn't depend on cwd contents.

- [ ] **Step 4: Run the smoke test**

Run: `cargo test -p roki-daemon --features test-support --test skeleton_smoke`
Expected: pass.

- [ ] **Step 5: Run the full test suite**

Run: `cargo test -p roki-daemon --features test-support`
Expected: every test passes.

- [ ] **Step 6: Commit**

```bash
git add crates/roki-daemon/src/engine/phase.rs crates/roki-daemon/tests/e2e/skeleton_smoke.rs
git commit -m "test(skeleton-smoke): assert new per-iter capture layout" -m "Asserts <session_root>/<ticket-id>/cycle-<uuid>/iter-1/run.{stdout,stderr,exit_code}. Adds ROKI_GHQ_BASE_OVERRIDE seam in engine::phase so the smoke test pins cwd at the test tempdir without requiring ghq to know github.com/example/repo." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 15: New end-to-end smoke — multi-iteration cycle

**Files:**
- Create: `crates/roki-daemon/tests/e2e/iteration_smoke.rs`
- Modify: `crates/roki-daemon/Cargo.toml`

- [ ] **Step 1: Register the new test target**

Edit `crates/roki-daemon/Cargo.toml`. Below the existing `[[test]] name = "skeleton_smoke" ...` block, add:

```toml
[[test]]
name = "iteration_smoke"
path = "tests/e2e/iteration_smoke.rs"
```

- [ ] **Step 2: Write the new smoke test**

Create `crates/roki-daemon/tests/e2e/iteration_smoke.rs`:

```rust
//! End-to-end smoke for the slice 1 engine: drives a 2-iteration cycle
//! through the `roki` binary and asserts the per-iter layout.
//!
//! Pre returns `directive: "run"` in iter 1 and 2; post returns
//! `directive: "run"` in iter 1 (forcing a second iteration that skips pre)
//! and `directive: "end"` in iter 2. Run is a trivial printf that emits
//! known stdout / stderr.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn cycle_loops_two_iterations_then_ends() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral webhook port")
        .local_addr()
        .expect("local_addr")
        .port();

    let linear = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"viewer": {"id": "u1"}}
        })))
        .mount(&linear)
        .await;

    let work = TempDir::new().expect("workspace tempdir");
    let session_root = work.path().join("sessions");
    std::fs::create_dir_all(&session_root).unwrap();

    // The pre/post fake AI uses a tempfile counter so iter 1 and iter 2 emit
    // different directives without daemon-side state.
    let counter_path = work.path().join("counter");
    std::fs::write(&counter_path, "1").unwrap();

    // Pre: always emit `directive: "run"`.
    let pre_cmd = r#"printf '{"directive":"run","note":"pre-iter"}'"#;

    // Run: write known stdout/stderr.
    let run_cmd = r#"printf 'run-out'; printf 'run-err' 1>&2"#;

    // Post: read the counter; if 1, increment to 2 and emit `directive: "run"`;
    // if 2, emit `directive: "end"`.
    let post_cmd = format!(
        r#"
n=$(cat {counter})
if [ "$n" = "1" ]; then
    printf 2 > {counter}
    printf '{{"directive":"run","note":"post-iter-1"}}'
else
    printf '{{"directive":"end","note":"post-iter-2"}}'
fi
"#,
        counter = counter_path.display()
    );

    let workflow_path = work.path().join("WORKFLOW.toml");
    let workflow_body = format!(
        r#"
[admission]
assignee = "u1"

[[admission.repos]]
ghq = "github.com/example/repo"

[[rule]]
[rule.when]
status = "in_progress"
[rule.when.labels]
has_all = []
[rule.pre]
cmd = {pre_cmd}
[rule.run]
cmd = {run_cmd}
[rule.post]
cmd = {post_cmd}
"#,
        pre_cmd = toml_string(pre_cmd),
        run_cmd = toml_string(run_cmd),
        post_cmd = toml_string(&post_cmd),
    );
    std::fs::write(&workflow_path, workflow_body).unwrap();

    let roki_path = work.path().join("roki.toml");
    let roki_body = format!(
        r#"
[linear]
token = "linear-test-token"

[linear.webhook]
bind = "127.0.0.1"
port = {port}

[default.ai.command]
cli = "echo"

[engine]
max_iterations = 5

[paths]
workflow = "{workflow}"
session_root = "{session_root}"

[log]
"#,
        port = port,
        workflow = workflow_path.display(),
        session_root = session_root.display(),
    );
    std::fs::write(&roki_path, roki_body).unwrap();

    let binary = env!("CARGO_BIN_EXE_roki");
    let mut child = Command::new(binary)
        .arg("run")
        .arg("--config")
        .arg(&roki_path)
        .env("ROKI_LINEAR_GRAPHQL_URL", linear.uri())
        .env("ROKI_GHQ_BASE_OVERRIDE", work.path())
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn roki binary");

    let webhook_addr: SocketAddr = ([127, 0, 0, 1], port).into();
    wait_for_listener(webhook_addr).await;

    let webhook_url = format!("http://127.0.0.1:{port}/");
    let payload = serde_json::json!({
        "action": "update",
        "type": "Issue",
        "data": {
            "id": "ENG-9",
            "assignee": {"id": "u1"},
            "state": {"name": "in_progress"},
            "labels": []
        }
    });
    let client = reqwest::Client::new();
    let resp = client.post(&webhook_url).json(&payload).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 202);

    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("binary should exit within 15s")
        .expect("child wait succeeds");
    assert!(status.success(), "binary should exit success, got {status:?}");

    let ticket_dir = session_root.join("ENG-9");
    let cycle_entry = std::fs::read_dir(&ticket_dir)
        .expect("ticket dir readable")
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("cycle-"))
        .expect("cycle-<uuid> dir present");
    let cycle_path = cycle_entry.path();

    // Iter 1 must contain pre + run + post artefacts.
    let iter1 = cycle_path.join("iter-1");
    assert!(iter1.join("pre.stdout").is_file());
    assert!(iter1.join("pre.response.json").is_file());
    assert!(iter1.join("run.stdout").is_file());
    assert!(iter1.join("run.exit_code").is_file());
    assert!(iter1.join("post.stdout").is_file());
    assert!(iter1.join("post.response.json").is_file());

    let pre_resp = std::fs::read_to_string(iter1.join("pre.response.json")).unwrap();
    assert!(pre_resp.contains("\"directive\": \"run\""));
    let post_resp = std::fs::read_to_string(iter1.join("post.response.json")).unwrap();
    assert!(post_resp.contains("\"directive\": \"run\""));
    let run_out = std::fs::read_to_string(iter1.join("run.stdout")).unwrap();
    assert!(run_out.contains("run-out"));
    let run_exit = std::fs::read_to_string(iter1.join("run.exit_code")).unwrap();
    assert_eq!(run_exit.trim(), "0");

    // Iter 2 must skip pre (post=run skips pre on the next iteration), so
    // pre.stdout must NOT exist; run + post must.
    let iter2 = cycle_path.join("iter-2");
    assert!(iter2.is_dir(), "iter-2 dir must exist");
    assert!(!iter2.join("pre.stdout").exists(), "iter-2 must skip pre");
    assert!(iter2.join("run.stdout").is_file());
    assert!(iter2.join("run.exit_code").is_file());
    assert!(iter2.join("post.stdout").is_file());
    assert!(iter2.join("post.response.json").is_file());

    let post2_resp = std::fs::read_to_string(iter2.join("post.response.json")).unwrap();
    assert!(post2_resp.contains("\"directive\": \"end\""));
}

/// Minimal TOML quoter for embedding shell snippets in WORKFLOW.toml. The
/// inputs use single quotes inside the printf JSON so basic escaping is
/// enough.
fn toml_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

async fn wait_for_listener(addr: SocketAddr) {
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("webhook listener never came up at {addr}");
}
```

- [ ] **Step 3: Run the new smoke test**

Run: `cargo test -p roki-daemon --features test-support --test iteration_smoke`
Expected: pass. Two iterations completed, post=end terminates the cycle, on-disk artefacts in `iter-1/` and `iter-2/` match the assertions.

- [ ] **Step 4: Run the entire test suite**

Run: `cargo test -p roki-daemon --features test-support`
Expected: every test passes — unit tests, skeleton smoke, iteration smoke.

- [ ] **Step 5: Commit**

```bash
git add crates/roki-daemon/Cargo.toml crates/roki-daemon/tests/e2e/iteration_smoke.rs
git commit -m "test(e2e): iteration smoke for two-iter cycle" -m "Posts one webhook, drives pre -> run -> post -> run -> post -> end through the binary, and asserts the per-iter layout including pre.response.json, run.exit_code, post.response.json, and the iter-2 pre-skip semantics." -m "Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review Checklist (run after all tasks land)

Before declaring slice 1 done:

1. **Spec coverage:**
   - §2 module layout — Tasks 5–13 land each module.
   - §3 cycle semantics — Task 11 (`engine::cycle`) implements transitions; Task 15 e2e exercises them.
   - §4 phase execution — Task 10 (`engine::phase`) builds the executor; Task 6 (`engine::directive`) parses outcomes.
   - §4.4 capture layout — Task 12 (`capture.rs` rewrite); Task 14 (skeleton smoke) and Task 15 (iteration smoke) verify the shape.
   - §5 templating — Task 8 (`engine::template`) + Task 7 (context fields and env builder).
   - §6 error routing — Task 4 (`PhaseInfraError`) + Task 13 (cycle-failure mapping in `runtime`).
   - §7 testing — unit tests in Tasks 5–12, integration coverage in Tasks 14–15.
   - §8 migration — Tasks 9 (workflow schema reshape), 13 (runner deletion), 14 (smoke update).
   - §8.3 dependencies — Task 1.

2. **Placeholder scan:** every step contains the actual code, not a description. Verify by reading every step's code block.

3. **Type consistency:**
   - `PhaseBody`, `PhaseKind`, `PhaseOutcome`, `PreDirective`, `PostDirective`, `FailureKind` — defined in Task 5, referenced verbatim in Tasks 6–12.
   - `PhaseContext::new(admitted, cycle_id, cfg)` — defined in Task 7, called in Tasks 8, 10, 11.
   - `crate::capture::create_iter_dir(session_root, ticket_id, cycle_id, iter)` — defined in Task 12, called in Tasks 10, 11.
   - `crate::capture::open_phase_files(iter_dir, phase)` — defined in Task 12, called in Task 10.
   - `crate::capture::write_response_json(iter_dir, phase, value)` — defined in Task 12, called in Task 10.
   - `crate::capture::write_run_exit_code(iter_dir, exit_code)` — defined in Task 12, called in Task 10.
   - `crate::engine::run_cycle(executor, admitted, rule, session_root, cfg)` — defined in Task 11, called in Task 13.
   - `PhaseInfraError::CycleFailed { kind, iter }` — defined in Task 13, used in Task 13.
   - `EngineSection { max_iterations: u32 }` — Task 3 introduces; Task 7 reads `cfg.engine.max_iterations`.

If any inconsistency surfaces during execution, fix the offending task's code block immediately and rebuild.

---

## Open Questions Surfaced During Planning

None outstanding. The spec already defers session-shape, stall, on_failure, worktree, hot reload, and cold start to later slices; this plan honours those boundaries strictly.
