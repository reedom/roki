//! Projection: `DiffCache` + `EventRing` → `TicketSummary` / `TicketDetail`.

use std::sync::Arc;

use roki_api_types::{ApiEvent, TicketDetail, TicketSummary};

use crate::api::sanitize::clean_text;
use crate::daemon::cache::{CacheEntry, DiffCache};
use crate::observability::EventRing;

/// Snapshot every tracked ticket, sorted newest `last_event_at` first.
pub async fn list_tickets(cache: &DiffCache) -> Vec<TicketSummary> {
    let mut entries = cache.snapshot_all().await;
    entries.sort_by(|a, b| b.1.last_event_at.cmp(&a.1.last_event_at));
    entries
        .into_iter()
        .map(|(id, entry)| into_summary(id, entry))
        .collect()
}

/// Render a single ticket plus its `window` most-recent ring events.
pub async fn detail(
    cache: &DiffCache,
    ring: &Arc<EventRing>,
    ticket_id: &str,
    window: usize,
) -> Option<TicketDetail> {
    let entry = cache.snapshot(ticket_id).await?;
    // Pull window+1 so we can detect truncation without a second call.
    let recent: Vec<ApiEvent> = ring
        .page(None, None, Some(ticket_id), None, window + 1)
        .events;
    let truncated = recent.len() > window;
    let recent_events = recent.into_iter().take(window).collect();
    Some(TicketDetail {
        summary: into_summary(ticket_id.to_string(), entry),
        recent_events,
        truncated,
    })
}

fn into_summary(ticket_id: String, entry: CacheEntry) -> TicketSummary {
    TicketSummary {
        ticket_id,
        repo: clean_text(&entry.repo),
        status: clean_text(&entry.status),
        labels: entry.labels.iter().map(|l| clean_text(l)).collect(),
        assignee: clean_text(&entry.assignee),
        in_flight_cycle_id: entry.cycle_id,
        last_event_at: entry.last_event_at,
    }
}
