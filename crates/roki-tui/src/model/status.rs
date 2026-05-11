use std::time::Instant;

#[derive(Debug, Default, Clone)]
pub struct StatusLine {
    text: String,
}

impl StatusLine {
    pub fn set(&mut self, msg: impl Into<String>) {
        self.text = msg.into();
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone, Copy)]
pub enum RefreshState {
    Idle,
    InFlight,
    DebouncedUntil(Instant),
}
