//! Single JSON-line emitted to TUI's own stderr at startup (fr:11 §Logging).

use std::io::Write;

use crate::config::PollingSection;
use crate::palette::Palette;

pub fn emit<W: Write>(
    mut out: W,
    api_url: &str,
    polling: &PollingSection,
    palette: Palette,
) -> std::io::Result<()> {
    let value = serde_json::json!({
        "event": "roki_tui_started",
        "ts": now_rfc3339(),
        "api_url": api_url,
        "polling": {
            "tickets_seconds": polling.tickets_seconds,
            "events_seconds": polling.events_seconds,
            "escalations_seconds": polling.escalations_seconds,
        },
        "palette": palette.as_str(),
    });
    writeln!(out, "{value}")
}

pub fn emit_decode_error<W: Write>(mut out: W, endpoint: &str, error: &str) -> std::io::Result<()> {
    let value = serde_json::json!({
        "event": "roki_tui_decode_error",
        "ts": now_rfc3339(),
        "endpoint": endpoint,
        "error": error,
    });
    writeln!(out, "{value}")
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_writes_one_json_line() {
        let mut buf = Vec::new();
        let cfg = PollingSection::default();
        emit(
            &mut buf,
            "http://127.0.0.1:8080",
            &cfg,
            Palette::IndexedAnsi16,
        )
        .unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert!(line.contains("\"event\":\"roki_tui_started\""));
        assert!(line.contains("\"palette\":\"indexed_ansi16\""));
        assert!(line.ends_with('\n'));
        // Must parse as a single JSON value.
        let trimmed = line.trim_end_matches('\n');
        let _: serde_json::Value = serde_json::from_str(trimmed).unwrap();
    }
}
