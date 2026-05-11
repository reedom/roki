//! Stable wire-schema types for the roki observability HTTP API.
//!
//! Imported by `roki-daemon`'s `api/` module and (slice 10) `roki-tui`. No
//! runtime dependencies beyond `serde` / `serde_json` / `time` / `uuid`.

pub mod escalations;
pub mod events;
pub mod healthz;
pub mod refresh;
pub mod tickets;

pub use escalations::ApiEscalation;
pub use events::{ApiEvent, EventsPage};
pub use healthz::Healthz;
pub use refresh::RefreshAck;
pub use tickets::{CycleKind, CycleSummary, CycleTrigger, TicketDetail, TicketSummary};
