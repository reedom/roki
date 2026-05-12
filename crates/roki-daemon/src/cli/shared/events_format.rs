//! Human-readable single-line formatter for `ApiEvent`.

use roki_api_types::ApiEvent;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use crate::cli::shared::sanitize::strip_for_terminal;

pub fn format_human(ev: &ApiEvent) -> String {
    let ts = ev
        .ts
        .format(&Rfc3339)
        .unwrap_or_else(|_| "ts-format-error".into());
    let ticket = ev.ticket_id.as_deref().unwrap_or("-");
    let cycle = ev
        .cycle_id
        .map(|u| {
            let s = u.to_string();
            s.split('-').next().unwrap_or(&s).to_string()
        })
        .unwrap_or_else(|| "-".into());
    let mut line = format!(
        "{seq}  {ts}  {event}  ticket={ticket}  cycle={cycle}",
        seq = ev.seq,
        event = strip_for_terminal(&ev.event),
    );
    if let Value::Object(map) = &ev.payload {
        for (k, v) in map {
            match v {
                Value::String(s) => {
                    line.push_str(&format!("  {k}={}", strip_for_terminal(s)));
                }
                Value::Number(n) => line.push_str(&format!("  {k}={n}")),
                Value::Bool(b) => line.push_str(&format!("  {k}={b}")),
                _ => {}
            }
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use roki_api_types::ApiEvent;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn ev() -> ApiEvent {
        ApiEvent {
            seq: 42,
            ts: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            event: "webhook_received".into(),
            ticket_id: Some("ENG-1".into()),
            cycle_id: Some(Uuid::parse_str("12345678-1234-1234-1234-1234567890ab").unwrap()),
            payload: serde_json::json!({
                "title": "\x1b[31mhello\x1b[0m",
                "count": 3,
                "ok": true,
                "nested": {"a": 1}
            }),
        }
    }

    #[test]
    fn human_line_has_fixed_prefix_columns() {
        let line = format_human(&ev());
        assert!(line.starts_with("42  "));
        assert!(line.contains("webhook_received"));
        assert!(line.contains("ticket=ENG-1"));
        assert!(line.contains("cycle=12345678"));
    }

    #[test]
    fn human_line_strips_ansi_from_payload_strings() {
        let line = format_human(&ev());
        assert!(line.contains("title=hello"));
        assert!(!line.contains("\x1b"));
    }

    #[test]
    fn human_line_skips_object_and_array_payload_fields() {
        let line = format_human(&ev());
        assert!(!line.contains("nested"));
    }
}
