//! View dispatch + chrome (tab strip, status bar). Each submodule renders one
//! View into a Frame.

pub mod escalations;
pub mod events;
pub mod status_bar;
pub mod ticket_detail;
pub mod tickets;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::model::{AppModel, View};

pub fn draw(frame: &mut Frame, model: &AppModel) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    status_bar::tab_strip(frame, layout[0], model);
    match model.focus {
        View::Tickets => tickets::draw(frame, layout[1], model),
        View::TicketDetail => ticket_detail::draw(frame, layout[1], model),
        View::Events => events::draw(frame, layout[1], model),
        View::Escalations => escalations::draw(frame, layout[1], model),
    }
    status_bar::status_line(frame, layout[2], model);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::model::AppModel;
    use crate::palette::Palette;
    use crate::ui::draw;

    #[test]
    fn draws_empty_tickets_view_without_panic() {
        let backend = TestBackend::new(120, 30);
        let mut term = Terminal::new(backend).unwrap();
        let model = AppModel::new(Palette::IndexedAnsi16);
        term.draw(|f| draw(f, &model)).unwrap();
        let buf = term.backend().buffer();
        let cell = &buf[(0, 0)];
        assert!(!cell.symbol().is_empty());
    }
}
