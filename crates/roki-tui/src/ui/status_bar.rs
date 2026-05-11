use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::model::{AppModel, View};

pub fn tab_strip(frame: &mut Frame, area: Rect, model: &AppModel) {
    let tabs = [
        (View::Tickets, "[1]Tickets"),
        (View::TicketDetail, "[2]Detail"),
        (View::Events, "[3]Events"),
        (View::Escalations, "[4]Escalations"),
    ];
    let line: String = tabs
        .iter()
        .map(|(v, label)| {
            if *v == model.focus {
                format!("<{label}>")
            } else {
                format!(" {label} ")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    frame.render_widget(
        Paragraph::new(line).style(Style::default().add_modifier(Modifier::BOLD)),
        area,
    );
}

pub fn status_line(frame: &mut Frame, area: Rect, model: &AppModel) {
    frame.render_widget(Paragraph::new(model.status.text().to_string()), area);
}
