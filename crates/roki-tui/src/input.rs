//! crossterm key event → Action. Pure mapping; the App applies the Action.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::View;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Focus(View),
    Up,
    Down,
    Enter,
    Refresh,
    ToggleAck,
    PrintLogCmd,
    None,
}

pub fn classify(ev: KeyEvent) -> Action {
    if ev.modifiers.contains(KeyModifiers::CONTROL) && matches!(ev.code, KeyCode::Char('c')) {
        return Action::Quit;
    }
    match ev.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('1') => Action::Focus(View::Tickets),
        KeyCode::Char('2') => Action::Focus(View::TicketDetail),
        KeyCode::Char('3') => Action::Focus(View::Events),
        KeyCode::Char('4') => Action::Focus(View::Escalations),
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Enter => Action::Enter,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('a') => Action::ToggleAck,
        KeyCode::Char('c') => Action::PrintLogCmd,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn maps_focus_keys() {
        assert_eq!(classify(k('1')), Action::Focus(View::Tickets));
        assert_eq!(classify(k('4')), Action::Focus(View::Escalations));
    }

    #[test]
    fn maps_ctrl_c_to_quit() {
        let ev = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(classify(ev), Action::Quit);
    }

    #[test]
    fn maps_arrow_and_vi_motion() {
        assert_eq!(classify(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)), Action::Up);
        assert_eq!(classify(k('j')), Action::Down);
    }

    #[test]
    fn unknown_keys_are_none() {
        assert_eq!(classify(k('x')), Action::None);
    }
}
