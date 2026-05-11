//! Projection: `EventRing` page → sanitized `EventsPage` (fr:10 §/events).

use std::sync::Arc;

use roki_api_types::EventsPage;
use uuid::Uuid;

use crate::observability::EventRing;

pub struct EventsQuery<'a> {
    pub since: Option<u64>,
    pub kind: Option<&'a str>,
    pub ticket: Option<&'a str>,
    pub cycle: Option<Uuid>,
    pub limit: usize,
}

/// Page the ring and scrub every payload string leaf + event/ticket fields
/// before handing the result back to the HTTP layer.
pub fn page(ring: &Arc<EventRing>, q: EventsQuery<'_>) -> EventsPage {
    let mut p = ring.page(q.since, q.kind, q.ticket, q.cycle, q.limit);
    for ev in &mut p.events {
        crate::api::sanitize::clean_json(&mut ev.payload);
        ev.event = crate::api::sanitize::clean_text(&ev.event);
        if let Some(t) = &mut ev.ticket_id {
            *t = crate::api::sanitize::clean_text(t);
        }
    }
    p
}
