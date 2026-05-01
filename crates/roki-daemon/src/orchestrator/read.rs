//! Read-only projection surface for orchestrator state.
//!
//! This module publishes the [`OrchestratorRead`] trait — the additive
//! consumer-facing API pinned by requirement 13.1. Downstream specs (notably
//! roki-observability) build observability and TUI surfaces on top of this
//! trait, and they MUST NOT be granted any path to state mutation. To make
//! that contract explicit:
//!
//! * Every method on [`OrchestratorRead`] takes `&self`. There is no
//!   `&mut self` method, no `set_*` method, and no method that returns an
//!   owning handle to internal state.
//! * The returned types ([`SnapshotResponse`], [`IssueState`]) are pure data
//!   projections cloned out of the orchestrator's in-memory state.
//!
//! ## What ships in 2.1a (and 7.1b's key collapse)
//!
//! Only the trait shape and the projection structs. The orchestrator's
//! concrete `OrchestratorRead` implementation that reads live worker state
//! lives behind the worker actor (task 3.x). For 2.1a, downstream specs and
//! tests can implement [`OrchestratorRead`] themselves against a fixture map.
//!
//! Task 7.1b collapsed the state-machine key from `(repo, issue)` to
//! `(issue,)`. The projection therefore identifies a worker by issue alone;
//! repo association moves onto the (yet-to-land in 7.1d) `WorktreeRegistry`,
//! which is per-worker rather than per-state.
//!
//! ## Serialization
//!
//! [`SnapshotResponse`] and [`IssueState`] derive `Debug` and `Clone` and
//! implement [`serde::Serialize`] manually so downstream observability can
//! emit JSON without forcing serde derives onto the foundational
//! [`super::state`] types (which are intentionally serde-free for now). The
//! manual impls use the public string accessors on [`IssueId`] and stable
//! lower-kebab-case names for [`WorkerState`].

use std::time::SystemTime;

use serde::{Serialize, Serializer, ser::SerializeStruct};

use super::state::{CorrelationId, IssueId, WorkerState};

/// Read-only projection surface for orchestrator state.
///
/// # Stability and contract
///
/// This trait is the consumer-facing read API published by the orchestrator
/// for additive specs (see requirement 13.1). It is intentionally narrow:
///
/// * Every method takes `&self` only — there are no mutators.
/// * Returned types are owned clones of internal projections; consumers
///   cannot reach back into orchestrator state through them.
/// * Implementations MUST NOT panic on unknown `IssueId` keys; they
///   return [`Option::None`] from [`OrchestratorRead::issue`] and skip the
///   key from [`SnapshotResponse::issues`].
///
/// Implementations are expected to be cheap to call — observability dashboards
/// poll [`OrchestratorRead::snapshot`] frequently — so the underlying state
/// store is expected to be an in-memory map guarded by a `RwLock` (or
/// equivalent), with the read path on the lock's read side only.
pub trait OrchestratorRead: Send + Sync {
    /// Snapshot the current per-issue state for every tracked worker. The
    /// returned [`SnapshotResponse`] is a self-contained owned value safe to
    /// JSON-serialize and ship over a wire.
    fn snapshot(&self) -> SnapshotResponse;

    /// Look up a single issue projection. Returns [`None`] if no worker for
    /// that issue is being tracked.
    fn issue(&self, issue: &IssueId) -> Option<IssueState>;
}

/// Stable wire version for [`SnapshotResponse`]. Bumped only on a breaking
/// change to the JSON shape; additive fields do not bump the version.
pub const SNAPSHOT_RESPONSE_VERSION: &str = "v1";

/// Top-level projection returned by [`OrchestratorRead::snapshot`].
///
/// Shape (JSON):
///
/// ```json
/// {
///   "version": "v1",
///   "issues": [ <IssueState>, ... ]
/// }
/// ```
#[derive(Debug, Clone)]
pub struct SnapshotResponse {
    /// Wire version — currently always [`SNAPSHOT_RESPONSE_VERSION`].
    pub version: String,
    /// Projection of every tracked worker, in implementation order. Consumers
    /// must not assume any particular ordering beyond stable repetition
    /// between back-to-back snapshots.
    pub issues: Vec<IssueState>,
}

impl SnapshotResponse {
    /// Build a v1 snapshot response from a collection of [`IssueState`]
    /// projections.
    pub fn new(issues: Vec<IssueState>) -> Self {
        Self {
            version: SNAPSHOT_RESPONSE_VERSION.to_string(),
            issues,
        }
    }
}

impl Serialize for SnapshotResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SnapshotResponse", 2)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("issues", &self.issues)?;
        state.end()
    }
}

/// Per-issue projection of orchestrator state.
///
/// This is the unit returned by [`OrchestratorRead::issue`] and held inside
/// [`SnapshotResponse::issues`]. It is intentionally a flat data record:
/// downstream specs may map it into their own UI or transport types.
///
/// `last_event_at` is reported as a [`SystemTime`] (rather than `Instant`,
/// which is monotonic but not absolute) so observability can render it
/// against a wall clock; serialized form is whole seconds since UNIX_EPOCH
/// via the manual [`Serialize`] impl. Repo association is handled by the
/// (post-7.1d) `WorktreeRegistry` per opened worktree, not by this
/// per-state projection.
#[derive(Debug, Clone)]
pub struct IssueState {
    pub issue: IssueId,
    pub state: WorkerState,
    /// Wall-clock timestamp of the most recent transition observed for this
    /// issue, or [`None`] if the worker has not transitioned since being
    /// observed.
    pub last_event_at: Option<SystemTime>,
    /// Correlation id associated with the most recent worker invocation for
    /// this issue, or [`None`] if no invocation has been minted.
    pub last_correlation_id: Option<CorrelationId>,
}

impl Serialize for IssueState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("IssueState", 4)?;
        state.serialize_field("issue", self.issue.as_str())?;
        state.serialize_field("state", worker_state_wire_name(self.state))?;
        // SystemTime serializes as a struct in serde's default impl; render as
        // whole seconds-since-UNIX_EPOCH for a stable, language-neutral wire
        // shape that downstream observability can parse.
        let last_event_at = self.last_event_at.map(system_time_to_unix_seconds);
        state.serialize_field("last_event_at_unix_seconds", &last_event_at)?;
        let last_correlation_id = self
            .last_correlation_id
            .map(|cid| cid.as_uuid().to_string());
        state.serialize_field("last_correlation_id", &last_correlation_id)?;
        state.end()
    }
}

/// Stable lower-kebab-case wire names for [`WorkerState`]. Kept in this
/// module (rather than as a `Display` impl on [`WorkerState`]) so the wire
/// shape is owned by the read-projection surface, not the foundational state
/// type.
const fn worker_state_wire_name(state: WorkerState) -> &'static str {
    match state {
        WorkerState::Discovered => "discovered",
        WorkerState::Queued => "queued",
        WorkerState::Active => "active",
        WorkerState::AwaitingReview => "awaiting-review",
        WorkerState::Backoff => "backoff",
        WorkerState::Stalled => "stalled",
        WorkerState::TerminalSuccess => "terminal-success",
        WorkerState::Cleaning => "cleaning",
        WorkerState::TerminalFailure => "terminal-failure",
    }
}

/// Convert a [`SystemTime`] to whole seconds since `UNIX_EPOCH`. Times before
/// the epoch (clock skew, tests with hand-built `SystemTime`s) clamp to `0`
/// rather than panic, mirroring observability's "render best-effort" stance.
fn system_time_to_unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use uuid::Uuid;

    /// A hand-crafted [`OrchestratorRead`] backed by a fixed `Vec` of
    /// projections. Used by these tests to assert the trait shape and the
    /// snapshot projection without depending on the worker actor (which
    /// lands in task 3.x).
    struct FixtureRead {
        issues: Vec<IssueState>,
    }

    impl OrchestratorRead for FixtureRead {
        fn snapshot(&self) -> SnapshotResponse {
            SnapshotResponse::new(self.issues.clone())
        }

        fn issue(&self, issue: &IssueId) -> Option<IssueState> {
            self.issues
                .iter()
                .find(|projection| &projection.issue == issue)
                .cloned()
        }
    }

    fn seed_issues() -> Vec<IssueState> {
        vec![
            IssueState {
                issue: IssueId::new("ENG-1"),
                state: WorkerState::Active,
                last_event_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
                last_correlation_id: Some(CorrelationId::from_uuid(Uuid::nil())),
            },
            IssueState {
                issue: IssueId::new("ENG-2"),
                state: WorkerState::AwaitingReview,
                last_event_at: None,
                last_correlation_id: None,
            },
            IssueState {
                issue: IssueId::new("OPS-9"),
                state: WorkerState::Cleaning,
                last_event_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_500)),
                last_correlation_id: Some(CorrelationId::new()),
            },
        ]
    }

    /// Locks in the trait shape: `OrchestratorRead` exposes ONLY the two
    /// `&self` methods (`snapshot` and `issue`). Adding any `&mut self` or
    /// `set_*` method would break this test through compiler-enforced
    /// `dyn`-coercion (the immutable `&dyn OrchestratorRead` reference is
    /// the maximum power downstream specs are allowed to hold).
    #[test]
    fn orchestrator_read_exposes_no_mutation_methods() {
        let fixture = FixtureRead {
            issues: seed_issues(),
        };
        // Coerce through an immutable trait object: any mutator on the trait
        // would force this binding to be `&mut dyn OrchestratorRead`, which
        // would not compile from the immutable `&fixture` borrow above.
        let read: &dyn OrchestratorRead = &fixture;

        // Both methods compile against `&dyn OrchestratorRead`, confirming
        // the trait grants no mutation rights.
        let _snapshot: SnapshotResponse = read.snapshot();
        let _issue: Option<IssueState> = read.issue(&IssueId::new("ENG-1"));
    }

    #[test]
    fn snapshot_returns_expected_projection_for_seeded_keys() {
        let seeded = seed_issues();
        let fixture = FixtureRead {
            issues: seeded.clone(),
        };

        let response = fixture.snapshot();
        assert_eq!(response.version, SNAPSHOT_RESPONSE_VERSION);
        assert_eq!(response.issues.len(), seeded.len());

        for (returned, expected) in response.issues.iter().zip(seeded.iter()) {
            assert_eq!(returned.issue, expected.issue);
            assert_eq!(returned.state, expected.state);
            assert_eq!(returned.last_event_at, expected.last_event_at);
            assert_eq!(returned.last_correlation_id, expected.last_correlation_id);
        }
    }

    #[test]
    fn issue_lookup_returns_some_for_seeded_key_and_none_otherwise() {
        let fixture = FixtureRead {
            issues: seed_issues(),
        };

        let hit = fixture
            .issue(&IssueId::new("ENG-1"))
            .expect("seeded key must resolve");
        assert_eq!(hit.state, WorkerState::Active);

        let miss = fixture.issue(&IssueId::new("DOES-NOT-EXIST"));
        assert!(miss.is_none(), "unknown key must return None, not panic");
    }

    #[test]
    fn snapshot_response_serializes_to_stable_v1_shape() {
        let fixture = FixtureRead {
            issues: vec![IssueState {
                issue: IssueId::new("ENG-1"),
                state: WorkerState::Cleaning,
                last_event_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
                last_correlation_id: Some(CorrelationId::from_uuid(Uuid::nil())),
            }],
        };

        let json = serde_json::to_value(fixture.snapshot()).expect("serialize to JSON value");
        assert_eq!(json["version"], "v1");
        let issues = json["issues"].as_array().expect("issues is an array");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["issue"], "ENG-1");
        assert_eq!(issues[0]["state"], "cleaning");
        assert_eq!(issues[0]["last_event_at_unix_seconds"], 1_700_000_000_i64);
        assert_eq!(
            issues[0]["last_correlation_id"],
            "00000000-0000-0000-0000-000000000000",
        );
    }

    #[test]
    fn worker_state_wire_names_are_stable_lower_kebab_case() {
        // Lock the wire spelling for every variant — observability dashboards
        // depend on these strings being byte-for-byte stable.
        assert_eq!(
            worker_state_wire_name(WorkerState::Discovered),
            "discovered",
        );
        assert_eq!(worker_state_wire_name(WorkerState::Queued), "queued");
        assert_eq!(worker_state_wire_name(WorkerState::Active), "active");
        assert_eq!(
            worker_state_wire_name(WorkerState::AwaitingReview),
            "awaiting-review",
        );
        assert_eq!(worker_state_wire_name(WorkerState::Backoff), "backoff");
        assert_eq!(worker_state_wire_name(WorkerState::Stalled), "stalled");
        assert_eq!(
            worker_state_wire_name(WorkerState::TerminalSuccess),
            "terminal-success",
        );
        assert_eq!(worker_state_wire_name(WorkerState::Cleaning), "cleaning");
        assert_eq!(
            worker_state_wire_name(WorkerState::TerminalFailure),
            "terminal-failure",
        );
    }
}
