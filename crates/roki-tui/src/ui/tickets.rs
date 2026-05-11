use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};

use crate::model::AppModel;

pub fn draw(frame: &mut Frame, area: Rect, model: &AppModel) {
    let header = Row::new(vec![
        "TicketId",
        "Repo",
        "Status",
        "Labels",
        "Assignee",
        "InFlight",
        "LastEvent",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = model.tickets.rows.iter().enumerate().map(|(i, t)| {
        let style = if i == model.tickets.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(t.ticket_id.clone()),
            Cell::from(t.repo.clone()),
            Cell::from(t.status.clone()),
            Cell::from(t.labels.join(",")),
            Cell::from(t.assignee.clone()),
            Cell::from(
                t.in_flight_cycle_id
                    .map(|u| u.to_string())
                    .unwrap_or_default(),
            ),
            Cell::from(t.last_event_at.to_string()),
        ])
        .style(style)
    });
    let widths = [
        Constraint::Length(14),
        Constraint::Length(28),
        Constraint::Length(14),
        Constraint::Length(20),
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(28),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title("Tickets").borders(Borders::ALL));
    frame.render_widget(table, area);
}

use ratatui::layout::Constraint;
