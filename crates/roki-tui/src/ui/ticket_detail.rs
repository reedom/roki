use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

use crate::model::AppModel;

pub fn draw(frame: &mut Frame, area: Rect, model: &AppModel) {
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)])
        .split(area);

    let header = Row::new(vec![
        "CycleId", "Kind", "Trigger", "Started", "Ended", "Terminal", "Visits",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = model.ticket_detail.cycles.iter().enumerate().map(|(i, c)| {
        let style = if i == model.ticket_detail.selected_cycle {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(c.cycle_id.to_string()),
            Cell::from(format!("{:?}", c.kind).to_lowercase()),
            Cell::from(format!("{:?}", c.trigger).to_lowercase()),
            Cell::from(c.started_at.to_string()),
            Cell::from(c.ended_at.map(|t| t.to_string()).unwrap_or_default()),
            Cell::from(c.terminal_id.clone().unwrap_or_default()),
            Cell::from(c.total_visits.to_string()),
        ])
        .style(style)
    });
    let widths = [
        Constraint::Length(38),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(20),
        Constraint::Length(20),
        Constraint::Length(14),
        Constraint::Length(6),
    ];
    let cycles_table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title("Cycles").borders(Borders::ALL));
    frame.render_widget(cycles_table, split[0]);

    let body = model
        .ticket_detail
        .tail_text
        .clone()
        .unwrap_or_else(|| "(no tail available)".to_string());
    let tail = Paragraph::new(body).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(format!(
                "Stdout tail (visit {})",
                model
                    .ticket_detail
                    .tail_visit_n
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            ))
            .borders(Borders::ALL),
    );
    frame.render_widget(tail, split[1]);
}
