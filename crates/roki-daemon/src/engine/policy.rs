//! Engine policy controller (turn budget, stall, backoff, retry).
//!
//! Task 2.8 of the roki-mvp spec. Implements the pure decision logic the
//! subprocess supervisor (task 2.10) calls between worker invocations to
//! enforce the bounded-loop semantics from design.md §Engine and
//! requirements.md §Requirement 5:
//!
//! * **5.3** — stall detection over a configurable event-inactivity window.
//! * **5.4** — configurable per-worker turn budget; once exhausted, no new
//!   continuation prompt is sent for the current invocation.
//! * **5.5** — clean exit on an active issue waits one second, then retries
//!   once with a fresh subprocess.
//! * **5.6** — non-clean exit and turn-budget exhaustion fall into exponential
//!   backoff between launches, bounded between 10s and 5min.
//!
//! The controller is intentionally a *pure* module: types, configuration, and
//! decision functions over an explicit [`WorkerState`]. The supervisor owns
//! the I/O, the subprocess handle, and the wall-clock timer. Keeping this
//! module pure lets the observable-completion matrix (clean / non-clean /
//! turn-budget / stall) be unit-tested deterministically.
//!
//! The [`BackoffPolicy`] in this module is distinct from
//! [`crate::workflow::BackoffPolicy`] on purpose. The workflow loader's
//! `BackoffPolicy` carries the operator-facing min/max seconds bounds parsed
//! from `WORKFLOW.md`. The engine policy's [`BackoffPolicy`] adds the
//! exponential-growth knobs (`initial`, `multiplier`) the supervisor uses to
//! compute the next-launch delay. The two are layered: the engine policy
//! always clamps its computed delay to the documented [10s, 5min] envelope
//! regardless of operator overrides, so a misconfigured `WORKFLOW.md` cannot
//! produce a delay outside the bounds the design promises.

use std::time::Duration;

/// Documented absolute floor for non-`CleanExit` next-launch delays
/// (requirements.md §5.6, design.md §Engine).
///
/// This is the documented default for [`EnginePolicy::backoff_floor`]. The
/// constant remains exported so callers that want the published default can
/// reference it directly (tests and downstream specs alike).
pub const BACKOFF_FLOOR: Duration = Duration::from_secs(10);

/// Default per-`(repo, issue)` retry budget. One launch per attempt;
/// `1` means "one shot, no retry"; the documented default is `3`. The
/// JSON-Schema in `WORKFLOW.md` rejects values outside `1..=10`. See
/// [`EnginePolicy::max_attempts`] and SPEC.md §3.2 / §9.5.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Maximum value the `WORKFLOW.md` schema accepts for `engine.max_attempts`.
/// Mirrored here for use by `EnginePolicy::with_max_attempts` validation.
pub const MAX_ATTEMPTS_CEILING: u32 = 10;

/// Documented absolute ceiling for non-`CleanExit` next-launch delays
/// (requirements.md §5.6, design.md §Engine).
pub const BACKOFF_CEILING: Duration = Duration::from_secs(300);

/// Fixed continuation-retry delay after a clean exit on an active issue
/// (requirements.md §5.5).
pub const CLEAN_EXIT_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Default per-worker turn budget. Mirrors the symphony-precedent value
/// referenced by design.md §Engine; operators may override via `WORKFLOW.md`.
pub const DEFAULT_TURN_BUDGET: u32 = 20;

/// Default event-inactivity window before a worker is considered stalled
/// (design.md §Engine, five minutes).
pub const DEFAULT_STALL_WINDOW: Duration = Duration::from_secs(300);

/// Exponential-growth knobs for the next-launch delay after a non-clean
/// outcome. The computed delay is always clamped to
/// `[BACKOFF_FLOOR, BACKOFF_CEILING]` by [`EnginePolicy::next_launch_delay`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackoffPolicy {
    /// Starting delay before any failures have accumulated.
    pub initial: Duration,
    /// Hard upper bound for the computed delay before the documented
    /// `BACKOFF_CEILING` clamp is applied. Operators may set this lower than
    /// `BACKOFF_CEILING` to tighten retry behavior; setting it higher has no
    /// effect because the ceiling clamp wins.
    pub max: Duration,
    /// Per-failure exponential growth factor. Values below `1.0` are treated
    /// as `1.0` so the delay never shrinks with consecutive failures.
    pub multiplier: f64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial: BACKOFF_FLOOR,
            max: BACKOFF_CEILING,
            multiplier: 2.0,
        }
    }
}

/// Why the supervisor concluded a worker had stalled. Carried inside
/// [`WorkerOutcome::Stalled`] so logs and downstream subscribers can
/// distinguish stall reasons without re-deriving them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallReason {
    /// No engine lifecycle event observed for longer than the configured
    /// stall window (requirements.md §5.3).
    EventInactivity,
}

/// Terminal outcome of a single worker invocation, as defined by design.md
/// §Engine. The supervisor produces this value when the worker either exits
/// or is terminated by the policy controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerOutcome {
    /// Subprocess exited with status 0 while the issue was still active.
    /// Triggers the one-second continuation retry (requirements.md §5.5).
    CleanExit,
    /// Subprocess exited with a non-zero status. The exact status is
    /// preserved for logging; the policy controller does not branch on it.
    NonCleanExit { code: i32 },
    /// Per-worker turn budget exhausted; supervisor stopped sending
    /// continuation prompts for the current invocation
    /// (requirements.md §5.4). Counts as a "non-clean" outcome for the
    /// purposes of the next-launch backoff computation
    /// (requirements.md §5.6).
    TurnBudgetExhausted,
    /// Worker was terminated by the policy controller after the configured
    /// event-inactivity window elapsed (requirements.md §5.3).
    Stalled { reason: StallReason },
}

/// Per-worker runtime state the supervisor mutates as events arrive and the
/// invocation progresses. The policy controller's decision functions read
/// (never mutate) this struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerState {
    /// Number of continuation prompts the supervisor has already issued in
    /// the current invocation. Incremented by the supervisor before each
    /// prompt; the policy compares it against [`EnginePolicy::turn_budget`].
    pub turns_consumed: u32,
    /// Wall-clock instant of the most recent engine lifecycle event observed
    /// for this worker. The supervisor refreshes this on every parsed event;
    /// the policy uses it for stall detection. Encoded as elapsed time since
    /// some monotonic origin (e.g., `Instant::elapsed_since(base)`); the
    /// policy only ever subtracts `Duration` values so it does not depend on
    /// any specific clock implementation.
    pub last_event_at: Duration,
    /// Number of consecutive non-clean outcomes the supervisor has recorded
    /// for this `(repo, issue)` pair without an intervening clean exit. Used
    /// as the exponent for the next-launch backoff computation.
    pub consecutive_failures: u32,
}

impl WorkerState {
    /// Construct a state for a freshly-launched worker that has not yet
    /// observed any events or failures.
    pub fn fresh(now: Duration) -> Self {
        Self {
            turns_consumed: 0,
            last_event_at: now,
            consecutive_failures: 0,
        }
    }
}

/// Configuration of the per-worker policy controller. Populated by the
/// supervisor at worker launch from the validated `WorkflowPolicy`
/// (design.md §Engine).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnginePolicy {
    /// Maximum number of continuation prompts the supervisor may send in a
    /// single invocation (requirements.md §5.4).
    pub turn_budget: u32,
    /// Window of event-inactivity tolerated before the supervisor terminates
    /// the worker as stalled (requirements.md §5.3).
    pub stall_window: Duration,
    /// Exponential-growth knobs for the next-launch delay
    /// (requirements.md §5.6).
    pub backoff: BackoffPolicy,
    /// Retry budget for the orchestrator's `Active → Backoff → Active` loop:
    /// the maximum number of launch attempts the worker actor will make for a
    /// single `(repo, issue)` against repeated `NonCleanExit` outcomes. `1`
    /// means "one shot, no retry"; the documented default is `3`. Only
    /// `NonCleanExit` consumes this budget — `Stalled` and
    /// `TurnBudgetExhausted` route directly to `TerminalFailure` because
    /// re-running with the same prompt and budget repeats the same outcome
    /// (see SPEC.md §9.5).
    pub max_attempts: u32,
    /// Documented floor applied to the computed next-launch delay before
    /// the absolute `[BACKOFF_FLOOR, BACKOFF_CEILING]` clamp. Defaults to the
    /// [`BACKOFF_FLOOR`] constant; tests construct policies with sub-second
    /// values so retry loops complete deterministically in well under one
    /// second. The constant remains the documented default for production
    /// callers.
    pub backoff_floor: Duration,
}

impl Default for EnginePolicy {
    fn default() -> Self {
        Self {
            turn_budget: DEFAULT_TURN_BUDGET,
            stall_window: DEFAULT_STALL_WINDOW,
            backoff: BackoffPolicy::default(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff_floor: BACKOFF_FLOOR,
        }
    }
}

/// Error raised when constructing or validating an [`EnginePolicy`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnginePolicyError {
    /// `max_attempts` was outside the documented `1..=10` envelope.
    #[error(
        "engine.max_attempts must be between 1 and {MAX_ATTEMPTS_CEILING} inclusive, got {value}"
    )]
    InvalidMaxAttempts { value: u32 },
}

impl EnginePolicy {
    /// Decide whether the supervisor may send another continuation prompt to
    /// the current worker session. Returns `false` once the per-worker turn
    /// budget is exhausted (requirements.md §5.4).
    pub fn allow_continuation(&self, state: &WorkerState) -> bool {
        state.turns_consumed < self.turn_budget
    }

    /// Decide whether the worker should be considered stalled given the most
    /// recent engine-event timestamp and the supervisor's current wall-clock
    /// reading. Returns `Some(StallReason::EventInactivity)` once the
    /// configured stall window has elapsed without an event
    /// (requirements.md §5.3).
    pub fn detect_stall(&self, last_event_at: Duration, now: Duration) -> Option<StallReason> {
        let elapsed = now.saturating_sub(last_event_at);
        if self.stall_window < elapsed {
            Some(StallReason::EventInactivity)
        } else {
            None
        }
    }

    /// Compute the next-launch delay after a worker invocation terminates.
    ///
    /// * [`WorkerOutcome::CleanExit`] always returns
    ///   [`CLEAN_EXIT_RETRY_DELAY`] (requirements.md §5.5), independent of
    ///   `consecutive_failures`.
    /// * Every other outcome returns an exponentially-growing delay clamped
    ///   to `[BACKOFF_FLOOR, BACKOFF_CEILING]` (requirements.md §5.6). The
    ///   formula is `min(initial * multiplier^consecutive_failures, max)`,
    ///   then clamped to the documented envelope so that no operator
    ///   override can push the delay outside the published bounds.
    pub fn next_launch_delay(&self, outcome: WorkerOutcome, consecutive_failures: u32) -> Duration {
        match outcome {
            WorkerOutcome::CleanExit => CLEAN_EXIT_RETRY_DELAY,
            WorkerOutcome::NonCleanExit { .. }
            | WorkerOutcome::TurnBudgetExhausted
            | WorkerOutcome::Stalled { .. } => self.compute_backoff(consecutive_failures),
        }
    }

    fn compute_backoff(&self, consecutive_failures: u32) -> Duration {
        // Treat sub-1.0 multipliers as 1.0 so the delay never *shrinks* with
        // consecutive failures; that would violate the spirit of "exponential
        // backoff" even if the caller misconfigures the policy.
        let multiplier = self.backoff.multiplier.max(1.0);
        let initial_secs = self.backoff.initial.as_secs_f64();
        let raw_secs = initial_secs * multiplier.powi(consecutive_failures as i32);
        // Pre-clamp to the ceiling-as-seconds so we never hand
        // `Duration::from_secs_f64` a NaN/infinite/overflowing value
        // (which would panic). Any out-of-range or non-finite result is
        // treated as "saturated at the documented ceiling".
        let ceiling_secs = BACKOFF_CEILING.as_secs_f64();
        let safe_secs = if raw_secs.is_finite() {
            raw_secs.clamp(0.0, ceiling_secs)
        } else {
            ceiling_secs
        };
        let raw = Duration::from_secs_f64(safe_secs);
        let bounded_by_policy = raw.min(self.backoff.max);
        // Floor is configurable per-policy (defaults to `BACKOFF_FLOOR`);
        // the absolute ceiling is fixed at `BACKOFF_CEILING` so a misconfigured
        // operator cannot push delays beyond the documented envelope.
        let floor = self.backoff_floor.min(BACKOFF_CEILING);
        bounded_by_policy.clamp(floor, BACKOFF_CEILING)
    }

    /// Validate that `max_attempts` is inside the documented `1..=10`
    /// envelope. Used by the `WORKFLOW.md` policy resolver before constructing
    /// an [`EnginePolicy`]; the JSON-Schema also enforces the bound, so this
    /// function exists primarily as a defense-in-depth check for callers that
    /// build policies programmatically.
    pub fn validate_max_attempts(value: u32) -> Result<u32, EnginePolicyError> {
        if !(1..=MAX_ATTEMPTS_CEILING).contains(&value) {
            return Err(EnginePolicyError::InvalidMaxAttempts { value });
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> EnginePolicy {
        EnginePolicy::default()
    }

    fn assert_within_documented_bounds(delay: Duration) {
        assert!(
            BACKOFF_FLOOR <= delay,
            "delay {delay:?} fell below documented floor {BACKOFF_FLOOR:?}",
        );
        assert!(
            delay <= BACKOFF_CEILING,
            "delay {delay:?} exceeded documented ceiling {BACKOFF_CEILING:?}",
        );
    }

    #[test]
    fn clean_exit_yields_one_second_continuation_retry() {
        // Requirement 5.5: clean exit on an active issue waits exactly 1s.
        for failures in [0u32, 1, 5, 100] {
            let delay = policy().next_launch_delay(WorkerOutcome::CleanExit, failures);
            assert_eq!(
                delay,
                Duration::from_secs(1),
                "clean-exit delay must be 1s regardless of consecutive_failures={failures}",
            );
        }
    }

    #[test]
    fn non_clean_exit_yields_bounded_exponential_backoff() {
        // Requirement 5.6: non-clean exits land in [10s, 5min] for any
        // consecutive_failures count.
        let outcome = WorkerOutcome::NonCleanExit { code: 1 };
        for failures in [0u32, 1, 2, 3, 10, 100, u32::MAX] {
            let delay = policy().next_launch_delay(outcome, failures);
            assert_within_documented_bounds(delay);
        }
    }

    #[test]
    fn turn_budget_exhausted_yields_bounded_backoff() {
        // Requirement 5.6: turn-budget exhaustion routes to backoff.
        for failures in [0u32, 1, 5, 50] {
            let delay = policy().next_launch_delay(WorkerOutcome::TurnBudgetExhausted, failures);
            assert_within_documented_bounds(delay);
        }
    }

    #[test]
    fn stall_yields_bounded_backoff() {
        // Requirement 5.3 + 5.6: stall outcome routes to backoff.
        let outcome = WorkerOutcome::Stalled {
            reason: StallReason::EventInactivity,
        };
        for failures in [0u32, 1, 5, 50] {
            let delay = policy().next_launch_delay(outcome, failures);
            assert_within_documented_bounds(delay);
        }
    }

    #[test]
    fn allow_continuation_returns_true_until_budget_exhausted() {
        // Requirement 5.4: continuation prompts stop once turn_budget is hit.
        let p = EnginePolicy {
            turn_budget: 3,
            ..EnginePolicy::default()
        };
        let mut state = WorkerState::fresh(Duration::ZERO);

        for expected_turn in 0..3 {
            assert_eq!(state.turns_consumed, expected_turn);
            assert!(
                p.allow_continuation(&state),
                "expected continuation allowed at turns_consumed={expected_turn}",
            );
            state.turns_consumed += 1;
        }

        assert_eq!(state.turns_consumed, 3);
        assert!(
            !p.allow_continuation(&state),
            "continuation must stop once turns_consumed >= turn_budget",
        );
        // And remains denied past the budget.
        state.turns_consumed = u32::MAX;
        assert!(!p.allow_continuation(&state));
    }

    #[test]
    fn detect_stall_returns_some_after_window_elapsed() {
        // Requirement 5.3: stall fires once event-inactivity exceeds window.
        let p = EnginePolicy {
            stall_window: Duration::from_secs(60),
            ..EnginePolicy::default()
        };
        let last = Duration::from_secs(100);
        let now = last + Duration::from_secs(61);

        assert_eq!(
            p.detect_stall(last, now),
            Some(StallReason::EventInactivity),
        );
    }

    #[test]
    fn detect_stall_returns_none_when_recent_activity() {
        let p = EnginePolicy {
            stall_window: Duration::from_secs(60),
            ..EnginePolicy::default()
        };
        let last = Duration::from_secs(100);

        // Equal to window → not yet stalled (strict comparison).
        assert_eq!(p.detect_stall(last, last + Duration::from_secs(60)), None);
        // Below window → not stalled.
        assert_eq!(p.detect_stall(last, last + Duration::from_secs(30)), None);
        // Identical timestamps → not stalled.
        assert_eq!(p.detect_stall(last, last), None);
        // "now" before "last" (clock skew) → saturating_sub yields zero, not
        // stalled.
        assert_eq!(p.detect_stall(last, last - Duration::from_secs(5)), None);
    }

    #[test]
    fn next_launch_delay_respects_floor_for_all_failure_outcomes() {
        // Even with a degenerate policy whose initial+max are below the
        // documented floor, the next-launch delay must never fall below 10s
        // for a non-CleanExit outcome (requirement 5.6).
        let p = EnginePolicy {
            backoff: BackoffPolicy {
                initial: Duration::from_millis(1),
                max: Duration::from_millis(1),
                multiplier: 2.0,
            },
            ..EnginePolicy::default()
        };
        let outcomes = [
            WorkerOutcome::NonCleanExit { code: 137 },
            WorkerOutcome::TurnBudgetExhausted,
            WorkerOutcome::Stalled {
                reason: StallReason::EventInactivity,
            },
        ];

        for outcome in outcomes {
            for failures in [0u32, 1, 100] {
                let delay = p.next_launch_delay(outcome, failures);
                assert_eq!(
                    delay, BACKOFF_FLOOR,
                    "outcome {outcome:?} with failures={failures} must clamp up to floor",
                );
            }
        }
    }

    #[test]
    fn next_launch_delay_respects_ceiling_for_all_failure_outcomes() {
        // With a runaway policy, the next-launch delay must never exceed
        // 5min for a non-CleanExit outcome (requirement 5.6).
        let p = EnginePolicy {
            backoff: BackoffPolicy {
                initial: Duration::from_secs(120),
                max: Duration::from_secs(86_400),
                multiplier: 10.0,
            },
            ..EnginePolicy::default()
        };
        let outcomes = [
            WorkerOutcome::NonCleanExit { code: 1 },
            WorkerOutcome::TurnBudgetExhausted,
            WorkerOutcome::Stalled {
                reason: StallReason::EventInactivity,
            },
        ];

        for outcome in outcomes {
            for failures in [0u32, 1, 5, 50, 1_000] {
                let delay = p.next_launch_delay(outcome, failures);
                assert!(
                    delay <= BACKOFF_CEILING,
                    "outcome {outcome:?} with failures={failures} produced \
                     {delay:?} above ceiling",
                );
            }
        }
    }

    #[test]
    fn outcome_matrix_stays_within_documented_delay_bounds() {
        // Observable-completion matrix from task 2.8: simulate each
        // WorkerOutcome and assert the resulting next-launch delay falls in
        // the documented bounds.
        let p = policy();
        let cases = [
            (
                WorkerOutcome::CleanExit,
                Duration::from_secs(1)..=Duration::from_secs(1),
            ),
            (
                WorkerOutcome::NonCleanExit { code: 2 },
                BACKOFF_FLOOR..=BACKOFF_CEILING,
            ),
            (
                WorkerOutcome::TurnBudgetExhausted,
                BACKOFF_FLOOR..=BACKOFF_CEILING,
            ),
            (
                WorkerOutcome::Stalled {
                    reason: StallReason::EventInactivity,
                },
                BACKOFF_FLOOR..=BACKOFF_CEILING,
            ),
        ];

        for (outcome, bounds) in cases {
            // Walk a representative range of consecutive_failures to confirm
            // monotonic clamping for the failure outcomes.
            for failures in [0u32, 1, 3, 10] {
                let delay = p.next_launch_delay(outcome, failures);
                assert!(
                    bounds.contains(&delay),
                    "outcome {outcome:?} with failures={failures} produced \
                     {delay:?} outside expected range {bounds:?}",
                );
            }
        }
    }

    #[test]
    fn validate_max_attempts_rejects_zero_and_above_ceiling() {
        // Task 3.7: the JSON-Schema bound is 1..=10. The runtime helper must
        // reject 0 and 11 (or higher) so the policy resolver fails closed if
        // the schema check is bypassed.
        assert_eq!(
            EnginePolicy::validate_max_attempts(0),
            Err(EnginePolicyError::InvalidMaxAttempts { value: 0 }),
            "max_attempts = 0 must be rejected (one shot is encoded as 1)",
        );
        assert_eq!(
            EnginePolicy::validate_max_attempts(MAX_ATTEMPTS_CEILING + 1),
            Err(EnginePolicyError::InvalidMaxAttempts {
                value: MAX_ATTEMPTS_CEILING + 1,
            }),
            "max_attempts = {} must be rejected (above documented ceiling)",
            MAX_ATTEMPTS_CEILING + 1,
        );
    }

    #[test]
    fn validate_max_attempts_accepts_documented_envelope() {
        // The lower and upper bounds are both inclusive.
        assert_eq!(EnginePolicy::validate_max_attempts(1), Ok(1));
        assert_eq!(
            EnginePolicy::validate_max_attempts(MAX_ATTEMPTS_CEILING),
            Ok(MAX_ATTEMPTS_CEILING),
        );
        // Default constant is itself inside the envelope.
        assert_eq!(
            EnginePolicy::validate_max_attempts(DEFAULT_MAX_ATTEMPTS),
            Ok(DEFAULT_MAX_ATTEMPTS),
        );
    }

    #[test]
    fn default_engine_policy_carries_documented_max_attempts() {
        // Sanity: the documented default is `3`, and `Default::default`
        // must agree.
        assert_eq!(EnginePolicy::default().max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(EnginePolicy::default().max_attempts, 3);
        assert_eq!(EnginePolicy::default().backoff_floor, BACKOFF_FLOOR);
    }

    #[test]
    fn compute_backoff_honours_per_policy_floor_field() {
        // Task 3.7: tests must be able to construct a policy with a
        // sub-second floor so retry-loop integration tests run deterministically
        // in well under one second. The field overrides the constant on the
        // per-policy `backoff_floor`, while the absolute `BACKOFF_CEILING`
        // remains in effect.
        let p = EnginePolicy {
            backoff: BackoffPolicy {
                initial: Duration::from_millis(10),
                max: Duration::from_secs(60),
                multiplier: 2.0,
            },
            backoff_floor: Duration::from_millis(50),
            ..EnginePolicy::default()
        };

        // First failure at the floor — backoff_floor wins because the raw
        // computed delay (10ms) is below the configured floor (50ms).
        let delay = p.next_launch_delay(WorkerOutcome::NonCleanExit { code: 1 }, 0);
        assert_eq!(
            delay,
            Duration::from_millis(50),
            "floor field must override the BACKOFF_FLOOR constant for sub-second test policies",
        );

        // CleanExit still uses the dedicated 1s continuation delay regardless.
        assert_eq!(
            p.next_launch_delay(WorkerOutcome::CleanExit, 0),
            CLEAN_EXIT_RETRY_DELAY,
        );
    }

    #[test]
    fn backoff_grows_monotonically_until_clamped() {
        // Sanity: with the default policy, the unclamped growth should be
        // monotonically non-decreasing in consecutive_failures.
        let p = policy();
        let outcome = WorkerOutcome::NonCleanExit { code: 1 };
        let mut previous = Duration::ZERO;
        for failures in 0u32..10 {
            let delay = p.next_launch_delay(outcome, failures);
            assert!(
                previous <= delay,
                "delay regressed at failures={failures}: previous={previous:?} now={delay:?}",
            );
            previous = delay;
        }
        // Eventually the ceiling clamp kicks in.
        assert_eq!(
            p.next_launch_delay(outcome, 100),
            BACKOFF_CEILING,
            "high failure counts must saturate at the documented ceiling",
        );
    }
}
