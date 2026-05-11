use std::collections::VecDeque;

use roki_api_types::{ApiEvent, EventsPage};

const MAX_ROWS: usize = 1000;

#[derive(Debug, Default, Clone)]
pub struct EventsView {
    pub rows: VecDeque<ApiEvent>,
    pub last_seq: Option<u64>,
    pub gap_pending: bool,
}

impl EventsView {
    pub fn merge_page(&mut self, page: EventsPage, _requested_since: Option<u64>) {
        if page.gap {
            self.gap_pending = true;
        }
        for ev in page.events {
            self.last_seq = Some(self.last_seq.map_or(ev.seq, |s| s.max(ev.seq)));
            self.rows.push_back(ev);
            if self.rows.len() > MAX_ROWS {
                self.rows.pop_front();
            }
        }
        if !page.gap && page.next_since.is_some() {
            self.gap_pending = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn ev(seq: u64) -> ApiEvent {
        ApiEvent {
            seq,
            ts: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            event: "x".into(),
            ticket_id: None,
            cycle_id: None,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn merges_in_order_and_tracks_seq() {
        let mut v = EventsView::default();
        v.merge_page(
            EventsPage { events: vec![ev(1), ev(2)], gap: false, next_since: Some(2) },
            None,
        );
        v.merge_page(
            EventsPage { events: vec![ev(3)], gap: false, next_since: Some(3) },
            Some(2),
        );
        assert_eq!(v.last_seq, Some(3));
        assert_eq!(v.rows.len(), 3);
        assert!(!v.gap_pending);
    }

    #[test]
    fn gap_flag_sticks_until_clean_page_arrives() {
        let mut v = EventsView::default();
        v.merge_page(
            EventsPage { events: vec![ev(10)], gap: true, next_since: Some(10) },
            Some(0),
        );
        assert!(v.gap_pending);
        v.merge_page(
            EventsPage { events: vec![ev(11)], gap: false, next_since: Some(11) },
            Some(10),
        );
        assert!(!v.gap_pending);
    }

    #[test]
    fn caps_rows_at_max() {
        let mut v = EventsView::default();
        let big: Vec<_> = (0..(MAX_ROWS as u64 + 5)).map(ev).collect();
        v.merge_page(
            EventsPage { events: big, gap: false, next_since: Some(MAX_ROWS as u64 + 4) },
            None,
        );
        assert_eq!(v.rows.len(), MAX_ROWS);
        assert_eq!(v.rows.front().unwrap().seq, 5);
    }
}
