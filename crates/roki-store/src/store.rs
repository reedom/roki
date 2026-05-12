use crate::Result;
use crate::models::{
    Cycle, CycleOutcome, Escalation, Event, NewCycle, NewEscalation, NewEvent, StateVisit,
    SubprocessRun, Ticket, UnixMillis,
};

/// Object-safe seam over the embedded SQLite control-plane store.
///
/// All implementations MUST run admission / cycle / event mutations inside a
/// single transaction so an FSM transition (cycle state + state_visits bump +
/// event insert) cannot be observed half-applied across a daemon crash.
/// Concrete implementations document their own write-serialization story; the
/// SQLite implementation relies on the single-daemon single-writer invariant
/// from fr:01.
pub trait Store: Send + Sync {
    // --- tickets / admission cache --------------------------------------

    fn admit_ticket(&self, id: &str, repo: &str, at: UnixMillis) -> Result<()>;
    fn evict_ticket(&self, id: &str, at: UnixMillis) -> Result<()>;
    fn list_admitted(&self) -> Result<Vec<Ticket>>;
    fn get_ticket(&self, id: &str) -> Result<Option<Ticket>>;

    // --- cycles + FSM ---------------------------------------------------

    fn open_cycle(&self, c: NewCycle) -> Result<Cycle>;
    fn set_current_state(&self, cycle_id: i64, state_id: &str, iter: u32) -> Result<()>;
    fn bump_visit(&self, cycle_id: i64, state_id: &str) -> Result<u32>;
    fn close_cycle(
        &self,
        cycle_id: i64,
        outcome: CycleOutcome,
        ended_at: UnixMillis,
    ) -> Result<()>;
    fn get_cycle(&self, cycle_id: i64) -> Result<Option<Cycle>>;
    fn list_inflight_cycles(&self) -> Result<Vec<Cycle>>;
    fn visits_for_cycle(&self, cycle_id: i64) -> Result<Vec<StateVisit>>;

    // --- events ---------------------------------------------------------

    fn append_event(&self, e: NewEvent) -> Result<Event>;
    /// Replay events for a ticket starting *after* `since_seq` (exclusive),
    /// up to `limit` rows. Returns rows in ascending `seq` order.
    fn events_since(
        &self,
        ticket_id: &str,
        since_seq: i64,
        limit: usize,
    ) -> Result<Vec<Event>>;
    fn latest_event_seq(&self, ticket_id: &str) -> Result<Option<i64>>;

    // --- subprocess registry (capture_dir pointers) ---------------------

    fn register_subprocess(&self, run: SubprocessRun) -> Result<()>;
    fn finish_subprocess(
        &self,
        cycle_id: i64,
        state_id: &str,
        visit: u32,
        exit_code: i32,
        ended_at: UnixMillis,
    ) -> Result<()>;
    fn list_subprocesses(&self, cycle_id: i64) -> Result<Vec<SubprocessRun>>;

    // --- escalations ----------------------------------------------------

    fn enqueue_escalation(&self, e: NewEscalation) -> Result<Escalation>;
    fn ack_escalation(&self, id: i64, at: UnixMillis) -> Result<()>;
    fn list_open_escalations(&self) -> Result<Vec<Escalation>>;
}
