---
refs:
  id: design:roki-mvp:retry-policy
  kind: design
  title: "Retry Policy Design"
  spec: roki-mvp
  depends_on:
    - design:roki-mvp
---

# Retry Policy Design — Task 3.7

Status: PROPOSAL — needs user sign-off before task 3.7 is opened.

## Decision matrix

| # | Decision | Options | Recommendation | Why |
|---|---|---|---|---|
| 1 | Which `WorkerOutcome` variants retry? | A. `NonCleanExit` only<br>B. `NonCleanExit` + `Stalled`<br>C. all three (incl. `TurnBudgetExhausted`) | **A** | `NonCleanExit` is genuinely transient (subprocess crash, network blip during agent stream). `Stalled` and `TurnBudgetExhausted` are agent-authored failures — re-running with the same prompt and budget repeats the same outcome. Sending these to `TerminalFailure` immediately keeps the loop tight and matches operator intuition: "this needs human eyes." SPEC.md §4.2 currently lumps all three; we tighten it. |
| 2 | Schema key + default | A. `engine.max_retries` (count of *retries*, 0 = one shot)<br>B. `engine.max_attempts` (count of *attempts*, 1 = one shot, 3 = up to 3 launches) | **B**, default `3` | SPEC.md already uses `max_attempts: 3` for `extension.gates.spec.*`. Reuse the name and semantics so operators don't have to remember two conventions. |
| 3 | Schema location | A. flat `engine.max_attempts`<br>B. nested `engine.retry.max_attempts` | **A** | Mirrors existing flat keys (`engine.turn_budget`, `engine.stall_window`). No premature nesting. |
| 4 | `BACKOFF_FLOOR` test override | A. `cfg(test)` constructor<br>B. `EnginePolicy.backoff_floor: Duration` field, default = current `BACKOFF_FLOOR` constant<br>C. `Option<Duration>` override field, honored when set | **B** | No `cfg` gates, no test-only smell; the constant becomes the documented default. Production callers still get the 10 s floor without doing anything; tests construct with `Duration::from_millis(50)`. |
| 5 | Workspace handling during Backoff | A. keep<br>B. delete + recreate | **A** | Lets the agent observe partial state across retries (a half-applied edit, a created branch). Matches design.md "Worker aggregate owns retry count and current backoff window." |
| 6 | `additional_context` across retries | A. unchanged (same prelude every attempt)<br>B. append last failure reason | **A** for roki-mvp | Failure-history accumulation is a roki-spec-gate / roki-review-gate concern, not core orchestration. Keep mvp boring; the gates can opt in via `additional_context` later. |
| 7 | Vetoability of `Active → Backoff`, `Backoff → Active`, retry-exhausted `Active → TerminalFailure` | A. all non-vetoable<br>B. some vetoable | **A** | Outcome-driven, deterministic. Matches the existing vetoable subset (Queued→Active, AwaitingReview→TerminalSuccess, TerminalSuccess→Cleaning). |
| 8 | Counter reset semantics | A. counter resets on `CleanExit`<br>B. no reset (state machine prevents `Active` resumption after `AwaitingReview`) | **B** (no code) | The state machine forbids re-entering `Active` after `AwaitingReview`, so reset is unreachable. Document the invariant; do not write dead reset code. |
| 9 | Logging contract | — | One `transition` log per arc with `attempt`, `delay_ms`, `outcome_reason`; on terminal failure include `final_attempt` and `last_outcome_reason`. | Existing `tracing` schema; lets the e2e test (4.3) assert via `tracing-test`. |

## State machine deltas (driver-side)

Currently in `WorkerActor::try_promote_to_active` (orchestrator/core.rs:597-628):

```
CleanExit                                 -> Active -> AwaitingReview
NonCleanExit | TurnBudgetExhausted | Stalled -> Active -> TerminalFailure
```

After 3.7:

```
CleanExit                          -> Active -> AwaitingReview
NonCleanExit  & attempt < max     -> Active -> Backoff -> sleep -> Backoff -> Active
NonCleanExit  & attempt >= max    -> Active -> TerminalFailure
TurnBudgetExhausted | Stalled     -> Active -> TerminalFailure  (no retry)
```

`state.rs::legal_transition` already permits `Active↔Backoff`; only the driver changes.

## Schema delta (additive)

`WorkflowPolicy.engine` gains:

```yaml
engine:
  turn_budget: 20            # existing
  stall_window: 5m           # existing
  max_attempts: 3            # NEW. 1 = no retry. Applies to NonCleanExit only.
```

JSON-Schema bound: `1 <= max_attempts <= 10`. SPEC.md §3.2 table gets one row; §9.5 gets a paragraph noting only `NonCleanExit` retries.

## Touch list (for the 3.7 task description)

- `crates/roki-daemon/src/engine/policy.rs` — add `max_attempts: u32` and `backoff_floor: Duration` (default = `BACKOFF_FLOOR`); update `compute_backoff` to use the field.
- `crates/roki-daemon/src/workflow/schema.rs` (or wherever JSON-Schema lives) — add `max_attempts` row.
- `crates/roki-daemon/src/orchestrator/core.rs` — extend `ActorRecord` with `consecutive_failures: u32`; rewrite the post-launch arm to drive `Active→Backoff→Active` for `NonCleanExit` while attempt budget remains.
- `crates/roki-daemon/src/orchestrator/state.rs` — no change (transitions already legal); update doc comment.
- `SPEC.md` §3.2 schema table + §9.5 retry semantics paragraph (same change set, per §16 contract-change rule).
- `design.md` — fold the "only `NonCleanExit` retries" rule into the lifecycle prose around line 761.
- New unit tests in `engine::policy` (`max_attempts` validation) and `orchestrator_core` (retry-loop trace assertion with stub launcher).

## Open question for the user

Decision #1 is the only one with a real semantic choice. If you want `Stalled` to also retry (rationale: a network hang on a long agent turn is plausibly transient), say so — I'll flip the recommendation. `TurnBudgetExhausted` should stay non-retryable; the agent ran out of authorized turns, the budget didn't change, retrying is a noop.
