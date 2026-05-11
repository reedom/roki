//! Projection: per-visit capture streams (fr:10 §/visits/…).

use std::path::Path;

/// One stream emitted by a visit. The HTTP handler maps URL path segments to
/// this enum so the projection layer owns the file-name convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    Directive,
    Terminal,
    Events,
    ExitCode,
}

impl Stream {
    pub fn parse(s: &str) -> Option<Stream> {
        match s {
            "stdout" => Some(Stream::Stdout),
            "stderr" => Some(Stream::Stderr),
            "directive" => Some(Stream::Directive),
            "terminal" => Some(Stream::Terminal),
            "events" => Some(Stream::Events),
            "exit_code" => Some(Stream::ExitCode),
            _ => None,
        }
    }

    pub fn file_suffix(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::Directive => "directive.json",
            Stream::Terminal => "terminal.json",
            Stream::Events => "events.jsonl",
            Stream::ExitCode => "exit_code",
        }
    }
}

/// Read the captured stream for a single visit. Path layout:
/// `<session_root>/<ticket_id>/cycle-<uuid>/visit-<n>/<state_id>.<suffix>`.
pub fn read_stream(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: uuid::Uuid,
    visit_n: u32,
    state_id: &str,
    stream: Stream,
) -> std::io::Result<Vec<u8>> {
    let path = session_root
        .join(ticket_id)
        .join(format!("cycle-{cycle_id}"))
        .join(format!("visit-{visit_n}"))
        .join(format!("{state_id}.{}", stream.file_suffix()));
    std::fs::read(path)
}
