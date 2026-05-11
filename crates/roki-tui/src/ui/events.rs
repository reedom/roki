use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::model::AppModel;

pub fn draw(frame: &mut Frame, area: Rect, model: &AppModel) {
    let items: Vec<ListItem> = model
        .events
        .rows
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .map(|e| {
            ListItem::new(format!(
                "{} #{} {} {}{}",
                e.ts,
                e.seq,
                e.event,
                e.ticket_id.clone().unwrap_or_default(),
                e.cycle_id
                    .map(|u| format!(" cycle={u}"))
                    .unwrap_or_default(),
            ))
        })
        .collect();
    let title = if model.events.gap_pending {
        "Events (gap pending — consult roki events --file <log>)"
    } else {
        "Events"
    };
    let list = List::new(items).block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(list, area);
}
