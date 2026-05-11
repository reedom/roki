use std::collections::HashSet;

use roki_api_types::ApiEscalation;
use uuid::Uuid;

#[derive(Debug, Default, Clone)]
pub struct EscalationsView {
    pub rows: Vec<ApiEscalation>,
    pub acked: HashSet<AckKey>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AckKey {
    pub marker: String,
    pub ticket_id: Option<String>,
    pub cycle_id: Option<Uuid>,
    pub kind: String,
    pub state_id: Option<String>,
    pub visit_n: Option<u32>,
}

impl AckKey {
    pub fn from(e: &ApiEscalation) -> Self {
        Self {
            marker: e.marker.clone(),
            ticket_id: e.ticket_id.clone(),
            cycle_id: e.cycle_id,
            kind: e.kind.clone(),
            state_id: e.state_id.clone(),
            visit_n: e.visit_n,
        }
    }
}

impl EscalationsView {
    pub fn apply(&mut self, rows: Vec<ApiEscalation>) {
        let new_keys: HashSet<_> = rows.iter().map(AckKey::from).collect();
        self.acked.retain(|k| new_keys.contains(k));
        let len = rows.len();
        self.rows = rows;
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    pub fn toggle_ack(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            let k = AckKey::from(row);
            if !self.acked.remove(&k) {
                self.acked.insert(k);
            }
        }
    }

    pub fn is_acked(&self, row: &ApiEscalation) -> bool {
        self.acked.contains(&AckKey::from(row))
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

    fn e(kind: &str, marker: &str) -> ApiEscalation {
        ApiEscalation {
            ticket_id: Some("ENG-1".into()),
            cycle_id: None,
            kind: kind.into(),
            state_id: None,
            visit_n: None,
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            error_text: "boom".into(),
            marker: marker.into(),
        }
    }

    #[test]
    fn toggle_ack_round_trip() {
        let mut v = EscalationsView::default();
        v.apply(vec![e("recursion_bound", "recursion_bound")]);
        assert!(!v.is_acked(&v.rows[0]));
        v.toggle_ack();
        assert!(v.is_acked(&v.rows[0]));
        v.toggle_ack();
        assert!(!v.is_acked(&v.rows[0]));
    }

    #[test]
    fn ack_clears_when_entry_disappears() {
        let mut v = EscalationsView::default();
        v.apply(vec![e("recursion_bound", "recursion_bound")]);
        v.toggle_ack();
        assert_eq!(v.acked.len(), 1);
        v.apply(vec![e("cleanup_fs", "cleanup_fs")]);
        assert!(v.acked.is_empty());
    }

    #[test]
    fn ack_persists_when_entry_persists() {
        let mut v = EscalationsView::default();
        v.apply(vec![e("recursion_bound", "recursion_bound")]);
        v.toggle_ack();
        v.apply(vec![e("recursion_bound", "recursion_bound")]);
        assert_eq!(v.acked.len(), 1);
    }
}
