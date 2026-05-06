---
refs:
  id: research:roki-skeleton
  kind: research
  title: "roki-skeleton Gap Analysis"
  spec: roki-skeleton
  related:
    - req:roki-skeleton
    - fr:01-engine-model
    - fr:02-configuration
    - fr:03-linear-admission
    - fr:04-phase-execution
    - fr:12-daemon-lifecycle
    - ref:config
    - ref:cli
---

# Gap Analysis: roki-skeleton

## 1. Current State Investigation

### Workspace inventory

- `Cargo.toml` declares workspace members `crates/roki-daemon` and `crates/roki-doctools`, but `crates/roki-daemon/` no longer exists (removed in commit `d05fec8`). The workspace fails `cargo metadata` until the member entry is dropped or the crate is re-created.
- `crates/roki-doctools/` is the only present crate. It is a docs cross-reference CLI (`clap`, `walkdir`, `serde_yaml_ng`, `glob`). Unrelated to daemon runtime; no reusable daemon code lives there.
- No `tests/` directory at the workspace root. `tests/e2e/skeleton_smoke.rs` (req 9) does not yet exist.
- `crates/index.md` exists but is a docs index, not Rust source.

### Documentation inventory

- `docs/fr/01..12-*.md` describe the post-pivot config-driven daemon. The skeleton's relevant FRs (01 engine, 02 configuration, 03 admission, 04 phase execution, 05 worktree/session, 12 lifecycle) are present and rich.
- `docs/reference/config.md` is the canonical config schema. `docs/reference/cli.md` is the canonical CLI surface. Both are authoritative per `.kiro/steering/grounded-design.md` Principle 1.
- `WORKFLOW.example.md` carries pre-pivot orchestrator-session content (Path A/B/C/D/E, `prompt_template_*`) that contradicts the post-pivot config-driven model. Not a skeleton input but flagged as ambient drift.
- FR module paths in `docs/fr/02-configuration.md` and `docs/fr/12-daemon-lifecycle.md` point at `crates/roki-daemon/...` which is gone; doctools `validate` reports four dangling module refs. Pre-existing tech debt, not blocking the skeleton requirements.

### Conventions extracted

- Rust 2024, `rust-version = "1.85"`, resolver 3, `unsafe_code = "forbid"`, clippy `all = warn`. Skeleton must obey.
- Workspace is additive — comment in `Cargo.toml` instructs leaving the layout open for downstream specs (`roki-tui`, `roki-api-types`).
- Tracing-based structured logs (per `fr:08`) are deferred to `roki-obs-tracing-pipeline` (Wave 5). Skeleton emits errors via whatever minimum surface is acceptable; align loosely so Wave 5 can swap implementations without rewriting call sites.

### Integration surfaces

- Linear API token (`[linear].token`) needed only to resolve `me` to a user id (req 4.2). No webhook signature verification (req 3.3).
- HTTP listener for webhook intake — first networking surface in the project. No prior axum / hyper / tokio code to align with.
- Subprocess execution for `run.cmd` — first child-process surface. No prior `tokio::process` / `std::process` precedent.

## 2. Requirements Feasibility Analysis

### Requirement-to-asset map

| Req | Capability | Existing asset | Gap | Tag |
|---|---|---|---|---|
| 1 | `roki --config <path>` CLI startup | none | new binary, clap parser | Missing |
| 2 | Read `[linear] [network] [default.ai.command] [paths] [engine] [log]` from `roki.toml` | none | TOML loader (likely `serde` + `toml` crate), validation | Missing |
| 3 | HTTP webhook receive on bind/port | none | tokio + axum (or hyper) listener | Missing |
| 3 | No HMAC verify | n/a | inert; explicit non-requirement | — |
| 4 | Assignee filter; resolve `me` via Linear token | none | Linear GraphQL (`viewer { id }`) HTTP client | Missing |
| 4 | First `[[admission.repos]]` entry, no `when.*` | none | minimal WORKFLOW.toml parser of just `[admission]` + `[[admission.repos]][0].ghq` | Missing |
| 5 | `[[rule]]` first-match on status + labels equality | none | minimal `[[rule]]` parser of `when.status` + `when.labels.has_all` (or equivalent equality) and `run.cmd` only | Missing |
| 6 | Command-form phase execution (`run.cmd` only) | none | render-free cmd spawn (Liquid template variables are deferred to `roki-runtime-template-vars`) | Missing |
| 7 | Per-cycle stdout/stderr capture under `[paths].session_root` | none | per-cycle dir creation + redirect of child stdio to files | Missing |
| 8 | Single-cycle clean exit | none | engine state machine: stop accepting after first cycle finalizes | Missing |
| 9 | `tests/e2e/skeleton_smoke.rs` smoke harness | none | integration-test harness: stub Linear webhook poster, stub Linear API for `me`, stub `run.cmd` (e.g. `/bin/echo`), assertion on capture files + exit code | Missing |

Every cell is `Missing`. Greenfield daemon.

### Constraints from existing architecture and patterns

- Workspace must remain additive; the skeleton crate name should not foreclose later splits. Two viable shapes:
  - Single `crates/roki-daemon` binary crate (matches the now-stale Cargo.toml member entry); later specs may carve out library crates.
  - Library crate `crates/roki-core` + thin binary `crates/roki` (or `roki-daemon`); easier to share types with later `roki-api-types`.
  - Brief is silent on crate layout. Decision deferred to design.
- `unsafe_code = "forbid"` and clippy lints are workspace-level; skeleton automatically inherits.
- No persistent storage: in-memory only, restart re-derives (per `roadmap.md` and `fr:01`).

### Complexity signals

- External integrations: Linear GraphQL (single read `viewer { id }`), HTTP listener, subprocess spawn. Three known-territory integrations, each minimal.
- Algorithmic logic: trivial (one filter + one first-match comparison + one spawn).
- Concurrency: tokio runtime with one HTTP listener task and one cycle task; no shared mutable state across cycles since the skeleton stops after one cycle.

### Brief vs. canonical conflicts (Research Needed)

The brief uses informal config / CLI naming that does not match canonical references. These must be resolved during design.

| Brief wording | Canonical (`docs/reference/`) | Resolution options |
|---|---|---|
| `roki --config <path>` | `roki run --config <path>` (cli.md) | (a) accept `roki run --config` to align with canonical CLI from day 1; (b) hold off on the `run` subcommand until `roki-cli-daemon` (Wave 6) and let the skeleton present the bare `roki --config` surface; revisit when Wave 6 lands. |
| `[network].bind` / `[network].port` | `[linear.webhook].bind` / `[linear.webhook].port` (config.md) | (a) follow canonical; (b) follow brief; (c) raise with the operator before design. Recommended: (a) — canonical is the authoritative source per `grounded-design.md` Principle 1. |
| Assignee filter and `[[admission.repos]]` listed alongside `roki.toml` sections | Both live in `WORKFLOW.toml` per `ref:config` | Skeleton must load `WORKFLOW.toml` minimally. Brief omits this section. Recommended: read `[paths].workflow`, then load `WORKFLOW.toml` for `[admission]` + first `[[admission.repos]]` + `[[rule]]`. |
| `[[rule]]` "status / labels equality" | canonical condition vocabulary supports `=`, `.in`, `.has_all`, `.has_any`, `.has_none`, etc. | Pick the minimum set needed for one rule to match the smoke fixture. Recommended: support `when.status = "..."` + `when.labels.has_all = [...]` only; reject other `when.*` operators. |

Each of these is an alignment question, not a scope-expansion. Flag in design phase rather than re-opening requirements.

## 3. Implementation Approach Options

### Option A — Single binary crate `crates/roki-daemon`

Matches the existing (stale) `Cargo.toml` member entry. The skeleton implementation lives as modules inside one crate:

```
crates/roki-daemon/
  Cargo.toml
  src/
    main.rs            # clap parse, start runtime
    config/mod.rs      # roki.toml + minimal WORKFLOW.toml loaders
    linear/mod.rs      # token-auth Linear client (viewer query)
    webhook/mod.rs     # axum listener, payload parse
    admission.rs       # assignee + first-repo filter
    engine.rs          # rule first-match, cycle dispatch, single-cycle exit
    capture.rs         # per-cycle dir + stdout/stderr file sinks
tests/e2e/skeleton_smoke.rs
```

- **Trade-offs**:
  - Minimal indirection; matches FR module paths in `fr:02` / `fr:12` (so doctools dangling refs disappear once the crate exists).
  - No deliberate seam for later library reuse; Wave 7 (`roki-http-server`) and Wave 8 (TUI) may want shared types and will need a refactor then.
- **Effort**: S (1–3 days) for the crate scaffold + happy-path; +1–2 days for the smoke harness.

### Option B — Library + binary split

Library `crates/roki-core` exposes config + engine + admission types; binary `crates/roki-daemon` is a thin `tokio::main` wrapper.

- **Trade-offs**:
  - Cleaner seam for `roki-api-types`, future TUI client, and integration tests that drive the engine without spawning the binary.
  - More moving parts up front; the brief explicitly forbids invention.
  - Forces a decision about the public-facing API of `roki-core` before later specs have weighed in.
- **Effort**: M (3–7 days). Adds package wiring and crate-boundary discipline.

### Option C — Hybrid: single crate now, internal module discipline

Keep Option A's single crate but enforce module boundaries (`pub(crate)` only across modules; no cross-module struct field access) so a Wave-6 / Wave-7 split into Option B remains a mechanical extraction.

- **Trade-offs**:
  - Cheapest path that does not paint future waves into a corner.
  - Discipline must be maintained without compiler-level enforcement.
- **Effort**: S (1–3 days), same as Option A.

**Recommendation for design phase**: Option C. Brief's "smallest practical backbone" plus `grounded-design.md` Principle 4 (state and config minimalism) argue for a single crate now; module discipline keeps Option B reachable when Wave 6/7 actually pulls.

## 4. Out of Scope for Gap Analysis

- HMAC verify, diff cache, full admission matchers, iteration loop, cleanup / on_failure, session-shape phases, hot reload, TUI, HTTP API beyond webhook intake, Liquid template rendering, worktree creation, structured event catalog. All deferred to named Wave 1–8 specs in `roadmap.md`.
- Choice of Linear client crate (raw `reqwest` GraphQL POST vs. a third-party crate) — design phase.
- Webhook framework choice (`axum` vs. `hyper`) — design phase.
- Crate naming (`roki-daemon` vs. `roki-core` + `roki`) — design phase.

## 5. Implementation Complexity & Risk

- **Effort**: S (1–3 days) for the daemon scaffold + happy-path + smoke harness, assuming Option C. Add ~1 day if the brief-vs-canonical conflicts (`[network]` vs. `[linear.webhook]`, `roki run` vs. `roki`) produce design-phase rework.
- **Risk**: Low.
  - All integrations are well-trodden Rust territory (clap, serde + toml, tokio, axum/hyper, tokio::process, reqwest GraphQL).
  - Single concurrency path. No persistence. No retry budgets. No template rendering.
  - Main risk is scope creep into deferred waves during design — must hold the line on brief out-of-scope items.

## 6. Recommendations for Design Phase

### Preferred approach

- Adopt **Option C** (single `crates/roki-daemon`, internal module discipline). Re-create the workspace member; module layout per Option A above.
- Resolve canonical-vs-brief naming explicitly in `design.md`:
  - Use canonical config keys (`[linear.webhook]`, `[paths].workflow`, etc.) and document the deviation from the brief in the design's Boundary Commitments.
  - Decide whether the skeleton's CLI is `roki --config <path>` (brief) or `roki run --config <path>` (canonical). Recommend canonical to avoid a breaking-change later.
  - Read `WORKFLOW.toml` for `[admission].assignee`, the first `[[admission.repos]]`, and `[[rule]]` first-match in addition to `roki.toml`.
- Constrain `[[rule]]` evaluation to `when.status` equality + a single `when.labels` equality operator (`has_all` recommended). Reject any other `when.*` operator with a configuration error so no Wave-1+ matcher silently leaks into the skeleton.
- Forbid `run.path` and `run.prompt` at config-load time (req 6.2) so the contract is honored even if the operator-authored TOML drifts.

### Research items to carry forward

External-API doc lookups (no design judgment, resolved by reading current Linear docs during `/kiro-spec-design`):

- **Linear `viewer` GraphQL query**: minimal request shape and `Authorization` header form for resolving `[admission].assignee = "me"` against `roki.toml [linear].token`.
- **Linear webhook payload schema**: minimum field set the skeleton needs from a Linear `Issue` webhook for assignee gate, status comparison, label comparison, and ghq repo discrimination.

Forced calls (recorded so design cites them, not open):

- **Smoke harness shape**: binary-as-subprocess + loopback HTTP POST. In-process drive would let later specs regress the wire path silently, which Req 9.3 forbids.
- **`Cargo.toml` fix-up timing**: drop the stale `crates/roki-daemon` workspace member as the skeleton's first task in the same PR. PR-shape detail only.

---

# Gap Analysis Delta — post canonical alignment

Triggered after `requirements.md` was rewritten to align with `ref:cli`, `ref:config`, and the FR canon (CLI is `roki run --config`, webhook keys are `[linear.webhook].bind/port`, admission and rule lists live in `WORKFLOW.toml`, rule matchers narrowed to `when.status` + `when.labels.has_all`). Earlier conflicts in §2 of the original analysis are resolved at the requirements layer; the deltas below are what changed and what is newly surfaced.

## Status of original "Brief vs. canonical conflicts"

| Original conflict | New status | Surfacing in requirements |
|---|---|---|
| `roki --config` vs `roki run --config` | Resolved → canonical | Req 1.1 / 1.3, Req 9.1 |
| `[network].*` vs `[linear.webhook].*` | Resolved → canonical | Req 2.1, Req 3.1 |
| `[admission]` / `[[admission.repos]]` location | Resolved → loaded from `WORKFLOW.toml` via `[paths].workflow` | Req 2.2, Req 4.1 / 4.3 |
| Rule "status / labels equality" | Resolved → narrowed to `when.status` + `when.labels.has_all`; reject other `when.*` | Req 5.1 / 5.2 / 5.3 |

Recommendation outputs from the original analysis (Option C single crate, low risk, S effort) still hold.

## Forced deferrals from the canon (no design-phase decision needed)

These are stated for traceability so `design.md` can cite them and Wave 2+ specs know what to pick up. Each is a forced consequence of the brief's out-of-scope list, not an open question.

- **`wt`/`ghq` PATH check** (`fr:12 §Missing dependency CLI`): skipped. Skeleton creates no worktree and resolves no ghq base, so the dependency is unused. Wave 2 `roki-runtime-worktree-lazy` adopts the canonical check.
- **Working directory** (`fr:04 §Working directory`, `fr:05 §Worktree`): skeleton's `run.cmd` is spawned with the daemon's process cwd. The canonical worktree-or-ghq-base rule lands with `roki-runtime-worktree-lazy`. The smoke harness must use a `run.cmd` that does not depend on cwd.
- **Session tempdir at admission** (`fr:05 §Session tempdir`): canonical creates `<session_root>/<ticket-id>/` at admission; skeleton creates the per-cycle capture dir at run-launch time. Final layout lands with `roki-runtime-capture-layout`. Already noted in `requirements.md` Boundary Context.
- **`[linear.webhook].secret` validation** (`ref:config`): canonical marks the key required; skeleton accepts the field without acting on it (Req 2.4). The skeleton uses a hand-rolled minimal schema, not the canonical-schema crate (which doesn't exist yet anyway). `roki-linear-signature-verify` adopts canonical enforcement.

## Updated requirement-to-asset delta

| Requirement (revised) | New asset surface | Notes |
|---|---|---|
| Req 2.2 — `WORKFLOW.toml` load | TOML parser invoked twice (`roki.toml` + `WORKFLOW.toml`) | No prior code; one new module under `crates/roki-daemon/src/config/` and/or `src/workflow/` (matches stale FR `modules:` paths and would clear two of the four pre-existing doctools errors once the crate exists). |
| Req 5.3 — reject `when.*` other than `when.status` / `when.labels.has_all` | `WORKFLOW.toml` schema validator | Hand-rolled. Cheaper than reusing a future canonical-schema crate. |
| Req 6.2 — reject `run.path` / `run.prompt` / missing `run` | Phase-block validator | Same validator pass. |
| Req 4.2 — Linear `viewer` resolve | Single GraphQL query, one-shot at admission startup; cache the resolved id for the rest of the run | No prior client; one new module. |

The other requirements' asset gaps are unchanged from the original analysis.

## Net effect on Effort / Risk

- **Effort**: still S (1–3 days) for happy path + crate scaffold, +1 day for the smoke harness. Alignment removed design-phase decision overhead by collapsing four ambiguities up front.
- **Risk**: still Low. The one genuinely open ordering call is whether the design lands the workspace `Cargo.toml` fix-up before the skeleton crate or as the skeleton's first task — both work, the choice affects PR shape only.

---

# Design Synthesis Outcomes

Recorded after `/kiro-spec-design roki-skeleton`. The design draft is at `.kiro/specs/roki-skeleton/design.md`.

## Generalization

No generalization is warranted. The nine requirements all hang on a single linear pipeline (CLI → config load → bind listener → admit → first-match rule → spawn run.cmd → capture → exit). Req 5.3 explicitly forbids generalizing the matcher engine beyond `when.status` + `when.labels.has_all`; Req 6.2 explicitly forbids generalizing the phase form beyond `run.cmd`. Generalizing further would invite scope creep into Wave 1+ specs.

## Build vs Adopt

Adopt for every external concern:

| Concern | Adopted | Rejected | Why |
|---|---|---|---|
| Async runtime | `tokio` (full features) | `async-std`, `smol` | Roadmap names tokio. axum and reqwest both align with tokio. |
| HTTP server | `axum` | `hyper` direct, `actix-web`, `warp` | Roadmap names axum for the future `/api/v1/`; aligning the skeleton ahead of `roki-http-server` removes a later rewrite. |
| HTTP client | `reqwest` (rustls-tls + json) | `ureq` (sync), raw `hyper` | One sync-feeling JSON POST + tokio integration; rustls keeps the binary self-contained. |
| CLI parsing | `clap` v4 derive | `argh`, hand-rolled | Already in `roki-doctools`; matching the existing crate keeps the workspace consistent. |
| Config parsing | `serde` + `toml` | `toml_edit`, JSON5 | Canonical TOML; serde derives are sufficient for the skeleton's hand-rolled validators. |
| Subprocess | `tokio::process::Command` | `std::process` blocking | Subprocess wait sits inside the tokio runtime. |
| Capture file IO | `std::fs::File` (sync create) + `Stdio::from` | async `tokio::fs::File` for stdio | Child stdio pipes read fine into a sync `File`; async only adds noise. |
| UUID | `uuid` v4 | timestamps | Per-cycle dir name unambiguity; deterministic output is not required for the smoke. |
| Logging | `tracing` + default `tracing-subscriber` | bespoke logger | Same crate the canonical pipeline picks; the skeleton uses the default formatter only and lets `roki-obs-tracing-pipeline` install a real subscriber. |
| Errors | `thiserror` (per-module) + `anyhow` (top-level) | one `Box<dyn Error>` enum | Typed module errors keep boundaries; anyhow only at the binary edge. |

## Simplification

- **No actor / event-bus surface.** The cycle is single-shot, so the runtime is a sequential function: `parse argv → load configs → resolve me → bind listener → await one webhook → admit → match → run → exit`. Wave-1 engine specs introduce concurrency where it is needed.
- **No `Tracker` trait abstraction.** `linear::client::resolve_viewer` is a free async function over `reqwest::Client`. A trait could be introduced when a second tracker (Wave 3+) actually needs it.
- **No Liquid renderer.** Req 6.4 forbids it; the skeleton spawns `sh -c <run.cmd>` verbatim.
- **No diff cache.** Req 8 (single-cycle exit) means the cache has at most one entry; modeled as a one-shot channel + atomic flag.
- **No structured event catalog.** `tracing::error!` / `warn!` / `info!` calls only; the canonical event names land with `roki-obs-event-catalog`.
- **Capture layout flattened.** `<session_root>/cycle-<uuid>/{stdout,stderr}` instead of the canonical `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{phase}.{stdout,stderr}`. The canonical layout is owned by `roki-runtime-capture-layout`. Documented in design Out of Boundary.

## Decision: Linear GraphQL endpoint test seam

- **Context**: smoke test (`tests/e2e/skeleton_smoke.rs`) must redirect the Linear `viewer { id }` request to a `wiremock` server. `ref:config` does not own a Linear endpoint key (Linear's GraphQL URL is fixed at `https://api.linear.app/graphql`).
- **Alternatives considered**:
  1. Add a canonical `[linear].graphql_url` key — pollutes the schema with a test-only knob; rejected.
  2. Inject via a public test trait — extra plumbing for one purpose; rejected.
  3. Hidden env var `ROKI_LINEAR_GRAPHQL_URL`, read only by `linear::client`, undocumented in `ref:config`. **Selected.**
- **Trade-off**: a small undocumented surface, but it is a single env var owned by one module and disclosed in `design.md`. Removing it requires a code change in one place.
- **Follow-up**: Wave 3 specs that touch Linear may formalize a real configuration knob if a non-test use case emerges.

## Risks & Mitigations

- **Module-path validation noise** — the skeleton `design.md` and the existing `fr:02` / `fr:12` module entries point at `crates/roki-daemon/...` paths that do not exist yet. `roki-doctools validate` reports six errors against this state. Mitigation: skeleton implementation tasks recreate the crate in one PR; all six clear together. The design intentionally retains the `modules:` block so that the daemon crate has its design-of-record once it lands.
- **axum graceful shutdown ordering** — the smoke test asserts a 503 from a second POST after the cycle. Mitigation: `axum::serve(...).with_graceful_shutdown(...)` waits for in-flight handlers before binary exit; the runtime flips the rejecting flag before flushing capture and signaling shutdown.
- **`run.cmd` shell semantics** — invoking `sh -c <cmd>` ties the skeleton to POSIX shell. Roadmap targets macOS + Linux only, so this is acceptable. Documented.

## References

- `docs/reference/cli.md` — canonical CLI surface (`roki run --config`).
- `docs/reference/config.md` — canonical `roki.toml` and `WORKFLOW.toml` schemas.
- `docs/fr/01-engine-model.md`, `docs/fr/02-configuration.md`, `docs/fr/03-linear-admission.md`, `docs/fr/04-phase-execution.md`, `docs/fr/12-daemon-lifecycle.md`.
- Linear GraphQL webhooks: `https://developers.linear.app/docs/graphql/webhooks` (envelope shape; skeleton parses by path with `serde_json::Value`).
- Linear API auth: `https://developers.linear.app/docs/graphql/working-with-the-graphql-api` (personal API token in the `Authorization` header verbatim).

