use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};

use crate::model::AppModel;

pub fn draw(frame: &mut Frame, area: Rect, model: &AppModel) {
    let header = Row::new(vec!["Ack", "Kind", "StateId", "Ticket", "Cycle", "Error"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = model.escalations.rows.iter().enumerate().map(|(i, e)| {
        let glyph = if model.escalations.is_acked(e) {
            "[*]"
        } else {
            "[ ]"
        };
        let mut style = Style::default();
        if i == model.escalations.selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if model.escalations.is_acked(e) {
            style = style.add_modifier(Modifier::DIM);
        }
        Row::new(vec![
            Cell::from(glyph),
            Cell::from(e.kind.clone()),
            Cell::from(e.state_id.clone().unwrap_or_default()),
            Cell::from(e.ticket_id.clone().unwrap_or_default()),
            Cell::from(e.cycle_id.map(|u| u.to_string()).unwrap_or_default()),
            Cell::from(e.error_text.clone()),
        ])
        .style(style)
    });
    let widths = [
        Constraint::Length(3),
        Constraint::Length(18),
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(38),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title("Escalations").borders(Borders::ALL));
    frame.render_widget(table, area);
}

use ratatui::layout::Constraint;
