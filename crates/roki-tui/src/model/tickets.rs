use roki_api_types::TicketSummary;

#[derive(Debug, Default, Clone)]
pub struct TicketsView {
    pub rows: Vec<TicketSummary>,
    pub selected: usize,
}

impl TicketsView {
    /// Replace `rows`, sorting by last_event_at descending. Keeps the previous
    /// selection clamped to the new row count.
    pub fn apply(&mut self, mut rows: Vec<TicketSummary>) {
        rows.sort_by(|a, b| b.last_event_at.cmp(&a.last_event_at));
        let len = rows.len();
        self.rows = rows;
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn move_down(&mut self) {
        if !self.rows.is_empty() && self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn t(id: &str, ts: i64) -> TicketSummary {
        TicketSummary {
            ticket_id: id.into(),
            repo: "github.com/x/y".into(),
            status: "open".into(),
            labels: vec![],
            assignee: "u".into(),
            in_flight_cycle_id: None,
            last_event_at: OffsetDateTime::from_unix_timestamp(ts).unwrap(),
        }
    }

    #[test]
    fn sorts_descending_by_last_event() {
        let mut v = TicketsView::default();
        v.apply(vec![t("A", 1), t("B", 3), t("C", 2)]);
        let ids: Vec<_> = v.rows.iter().map(|t| t.ticket_id.as_str()).collect();
        assert_eq!(ids, vec!["B", "C", "A"]);
    }

    #[test]
    fn move_clamps_at_bounds() {
        let mut v = TicketsView::default();
        v.apply(vec![t("A", 1), t("B", 2)]);
        v.move_down();
        v.move_down();
        assert_eq!(v.selected, 1);
        v.move_up();
        v.move_up();
        assert_eq!(v.selected, 0);
    }
}
