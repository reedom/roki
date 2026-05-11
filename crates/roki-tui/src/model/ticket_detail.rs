use roki_api_types::{CycleSummary, TicketDetail};

#[derive(Debug, Default, Clone)]
pub struct TicketDetailView {
    pub ticket_id: Option<String>,
    pub detail: Option<TicketDetail>,
    pub cycles: Vec<CycleSummary>,
    pub selected_cycle: usize,
    pub tail_text: Option<String>,
    pub tail_visit_n: Option<u32>,
}

impl TicketDetailView {
    pub fn focus_ticket(&mut self, id: String) {
        if self.ticket_id.as_deref() != Some(id.as_str()) {
            self.ticket_id = Some(id);
            self.detail = None;
            self.cycles.clear();
            self.selected_cycle = 0;
            self.tail_text = None;
            self.tail_visit_n = None;
        }
    }

    pub fn selected_cycle(&self) -> Option<&CycleSummary> {
        self.cycles.get(self.selected_cycle)
    }

    pub fn move_cycle_down(&mut self) {
        if !self.cycles.is_empty() && self.selected_cycle + 1 < self.cycles.len() {
            self.selected_cycle += 1;
        }
    }

    pub fn move_cycle_up(&mut self) {
        self.selected_cycle = self.selected_cycle.saturating_sub(1);
    }
}
