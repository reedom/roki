//! AppModel holds every piece of UI state. Reducers in the submodules are
//! pure functions over snapshots so unit tests do not need any I/O.

pub mod escalations;
pub mod events;
pub mod status;
pub mod ticket_detail;
pub mod tickets;

use std::time::Instant;

use crossterm::event::KeyEvent;
use roki_api_types::{
    ApiEscalation, CycleSummary, EventsPage, RefreshAck, TicketDetail, TicketSummary,
};

use crate::palette::Palette;

pub use escalations::{AckKey, EscalationsView};
pub use events::EventsView;
pub use status::{RefreshState, StatusLine};
pub use ticket_detail::TicketDetailView;
pub use tickets::TicketsView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Tickets,
    TicketDetail,
    Events,
    Escalations,
}

#[derive(Debug)]
pub enum Update {
    Tickets(Vec<TicketSummary>),
    TicketDetail(TicketDetail),
    Cycles(Vec<CycleSummary>),
    Tail {
        visit_n: u32,
        body: String,
    },
    Events {
        page: EventsPage,
        requested_since: Option<u64>,
    },
    Escalations(Vec<ApiEscalation>),
    RefreshAck(RefreshAck),
    Input(KeyEvent),
    PollError {
        source: PollSource,
        message: String,
    },
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollSource {
    Tickets,
    Events,
    Escalations,
    TicketDetail,
    Cycles,
    Tail,
    Refresh,
    Input,
}

pub struct AppModel {
    pub focus: View,
    pub tickets: TicketsView,
    pub ticket_detail: TicketDetailView,
    pub events: EventsView,
    pub escalations: EscalationsView,
    pub status: StatusLine,
    pub refresh: RefreshState,
    pub palette: Palette,
    pub started_at: Instant,
}

impl AppModel {
    pub fn new(palette: Palette) -> Self {
        Self {
            focus: View::Tickets,
            tickets: TicketsView::default(),
            ticket_detail: TicketDetailView::default(),
            events: EventsView::default(),
            escalations: EscalationsView::default(),
            status: StatusLine::default(),
            refresh: RefreshState::Idle,
            palette,
            started_at: Instant::now(),
        }
    }

    pub fn focus_view(&mut self, v: View) {
        self.focus = v;
    }

    pub fn selected_ticket_id(&self) -> Option<&str> {
        self.tickets
            .rows
            .get(self.tickets.selected)
            .map(|t| t.ticket_id.as_str())
    }

    pub fn apply_ticket_detail(&mut self, detail: TicketDetail) {
        self.ticket_detail.detail = Some(detail);
    }

    pub fn apply_cycles(&mut self, cycles: Vec<CycleSummary>) {
        let prev_selected = self.ticket_detail.selected_cycle;
        self.ticket_detail.cycles = cycles;
        self.ticket_detail.selected_cycle =
            prev_selected.min(self.ticket_detail.cycles.len().saturating_sub(1));
    }

    pub fn apply_tail(&mut self, visit_n: u32, body: String) {
        self.ticket_detail.tail_visit_n = Some(visit_n);
        self.ticket_detail.tail_text = Some(body);
    }

    pub fn apply_refresh_ack(&mut self, ack: RefreshAck) {
        let parts = [
            format!("coalesced={}", ack.coalesced),
            format!("backoff_active={}", ack.backoff_active),
            match ack.earliest_fire_at {
                Some(t) => format!("fire_at={t}"),
                None => "fire_at=now".into(),
            },
        ];
        self.status.set(format!("refresh: {}", parts.join(" ")));
        self.refresh =
            RefreshState::DebouncedUntil(Instant::now() + std::time::Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    #[test]
    fn selected_ticket_id_returns_none_when_empty() {
        let m = AppModel::new(Palette::IndexedAnsi16);
        assert!(m.selected_ticket_id().is_none());
    }

    #[test]
    fn selected_ticket_id_returns_first() {
        let mut m = AppModel::new(Palette::IndexedAnsi16);
        m.tickets.rows = vec![TicketSummary {
            ticket_id: "ENG-1".into(),
            repo: "github.com/x/y".into(),
            status: "open".into(),
            labels: vec![],
            assignee: "u".into(),
            in_flight_cycle_id: None,
            last_event_at: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        }];
        assert_eq!(m.selected_ticket_id(), Some("ENG-1"));
    }
}
