//! `roki events` — read the structured event stream (online HTTP or offline file).
//!
//! Slice 11 lands this command in two tasks: this file implements the
//! offline JSON-Lines reader path (Task 8). The online HTTP client lands
//! in Task 9 and replaces the stub in `run_online_dispatch`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use roki_api_types::ApiEvent;
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::cli::shared::events_format::format_human;

#[derive(Debug, Default, Args)]
pub struct EventsArgs {
    #[arg(long = "tail")]
    pub tail: bool,
    #[arg(long = "since", value_name = "SEQ_OR_RFC3339")]
    pub since: Option<String>,
    #[arg(long = "kind", value_name = "EVENT")]
    pub kind: Option<String>,
    #[arg(long = "ticket", value_name = "ID")]
    pub ticket: Option<String>,
    #[arg(long = "cycle", value_name = "UUID")]
    pub cycle: Option<String>,
    #[arg(long = "format", value_enum, default_value_t = Format::Json)]
    pub format: Format,
    #[arg(long = "api", value_name = "URL")]
    pub api: Option<String>,
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[arg(long = "offline")]
    pub offline: bool,
    #[arg(long = "file", value_name = "PATH")]
    pub file: Option<PathBuf>,
    #[arg(
        long = "cadence-ms",
        value_name = "MS",
        default_value_t = 1000,
        hide = true
    )]
    pub cadence_ms: u64,
}

#[derive(Debug, Default, Clone, Copy, ValueEnum)]
pub enum Format {
    #[default]
    Json,
    Human,
}

#[derive(Debug, Error)]
pub enum EventsError {
    #[error("roki events: {0}")]
    Resolve(String),
    #[error("roki events: --offline requires --file")]
    NoFile,
    #[error("roki events: --tail not supported with --offline")]
    OfflineTail,
    #[error("roki events: io: {0}")]
    Io(#[from] std::io::Error),
    #[error("roki events: http: {0}")]
    Http(String),
    #[error("roki events: bad event line: {0}")]
    BadLine(String),
}

pub async fn run(args: EventsArgs) -> ExitCode {
    let format = args.format;
    if args.offline {
        if args.tail {
            eprintln!("{}", EventsError::OfflineTail);
            return ExitCode::from(2);
        }
        return run_offline_dispatch(args, format).await;
    }
    run_online_dispatch(args, format).await
}

async fn run_offline_dispatch(args: EventsArgs, format: Format) -> ExitCode {
    match run_offline_capture(args).await {
        Ok(text) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(text.as_bytes());
            let _ = std::io::stdout().write_all(b"\n");
            let _ = format;
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(1)
        }
    }
}

async fn run_online_dispatch(_args: EventsArgs, _format: Format) -> ExitCode {
    // Wired in Task 9.
    eprintln!("roki events: online mode not yet implemented (slice 11 Task 9)");
    ExitCode::from(70)
}

#[cfg(test)]
pub(crate) async fn run_capture(args: EventsArgs) -> Result<String, EventsError> {
    if args.offline {
        return run_offline_capture(args).await;
    }
    Err(EventsError::Resolve("test path is offline-only".into()))
}

async fn run_offline_capture(args: EventsArgs) -> Result<String, EventsError> {
    let file = args.file.clone().ok_or(EventsError::NoFile)?;
    let raw = std::fs::read_to_string(&file)?;
    let filter = Filter::from_args(&args)?;
    let mut out = String::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ev: ApiEvent = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => {
                eprintln!("# roki events: skipping malformed line");
                continue;
            }
        };
        if !filter.accept(&ev) {
            continue;
        }
        match args.format {
            Format::Json => {
                out.push_str(line);
                out.push('\n');
            }
            Format::Human => {
                out.push_str(&format_human(&ev));
                out.push('\n');
            }
        }
    }
    // Trim trailing newline so tests can `out.lines()` exactly.
    if out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

struct Filter {
    since_seq: Option<u64>,
    since_ts: Option<OffsetDateTime>,
    kind: Option<String>,
    ticket: Option<String>,
    cycle: Option<Uuid>,
}

impl Filter {
    fn from_args(args: &EventsArgs) -> Result<Self, EventsError> {
        let (since_seq, since_ts) = match args.since.as_deref() {
            None => (None, None),
            Some(s) => {
                if let Ok(n) = s.parse::<u64>() {
                    (Some(n), None)
                } else {
                    let ts = OffsetDateTime::parse(s, &Rfc3339)
                        .map_err(|_| EventsError::Resolve(format!("invalid --since value: {s}")))?;
                    (None, Some(ts))
                }
            }
        };
        let cycle = args
            .cycle
            .as_deref()
            .map(|s| Uuid::parse_str(s).map_err(|e| EventsError::Resolve(format!("{e}"))))
            .transpose()?;
        Ok(Self {
            since_seq,
            since_ts,
            kind: args.kind.clone(),
            ticket: args.ticket.clone(),
            cycle,
        })
    }

    fn accept(&self, ev: &ApiEvent) -> bool {
        if let Some(seq) = self.since_seq
            && ev.seq < seq
        {
            return false;
        }
        if let Some(ts) = self.since_ts
            && ev.ts < ts
        {
            return false;
        }
        if let Some(k) = &self.kind
            && ev.event != *k
        {
            return false;
        }
        if let Some(t) = &self.ticket
            && ev.ticket_id.as_deref() != Some(t.as_str())
        {
            return false;
        }
        if let Some(c) = self.cycle
            && ev.cycle_id != Some(c)
        {
            return false;
        }
        true
    }

    // Server-side seq cursor is already applied via the `since` query
    // param. For RFC3339 cutoffs the client must drop strictly-older
    // events. Other filters are server-applied in online mode, so we
    // skip them here. Defined now so Task 9 lands without re-touching
    // this `impl` block.
    #[cfg_attr(not(test), allow(dead_code))]
    fn accept_after_seq_cursor(&self, ev: &ApiEvent) -> bool {
        if let Some(ts) = self.since_ts
            && ev.ts < ts
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &std::path::Path) {
        std::fs::write(
            path,
            concat!(
                r#"{"seq":1,"ts":"2026-05-11T10:00:00Z","event":"webhook_received","ticket_id":"ENG-1","cycle_id":null,"payload":{"foo":"bar"}}"#,
                "\n",
                r#"{"seq":2,"ts":"2026-05-11T10:00:01Z","event":"cycle_started","ticket_id":"ENG-1","cycle_id":"00000000-0000-0000-0000-000000000001","payload":{"kind":"rule"}}"#,
                "\n",
                r#"{"seq":3,"ts":"2026-05-11T10:00:02Z","event":"state_started","ticket_id":"ENG-2","cycle_id":null,"payload":{}}"#,
                "\n",
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn offline_filter_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.jsonl");
        fixture(&path);
        let out = run_capture(EventsArgs {
            offline: true,
            file: Some(path),
            kind: Some("cycle_started".into()),
            format: Format::Json,
            ..Default::default()
        })
        .await
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"event\":\"cycle_started\""));
    }

    #[tokio::test]
    async fn offline_filter_ticket() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.jsonl");
        fixture(&path);
        let out = run_capture(EventsArgs {
            offline: true,
            file: Some(path),
            ticket: Some("ENG-2".into()),
            format: Format::Json,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(out.lines().count(), 1);
    }

    #[tokio::test]
    async fn offline_human_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.jsonl");
        fixture(&path);
        let out = run_capture(EventsArgs {
            offline: true,
            file: Some(path),
            format: Format::Human,
            ..Default::default()
        })
        .await
        .unwrap();
        let first = out.lines().next().unwrap();
        assert!(first.starts_with("1  "));
        assert!(first.contains("ticket=ENG-1"));
    }

    #[tokio::test]
    async fn offline_since_rfc3339_drops_strictly_older() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.jsonl");
        fixture(&path);
        let out = run_capture(EventsArgs {
            offline: true,
            file: Some(path),
            since: Some("2026-05-11T10:00:01Z".into()),
            format: Format::Json,
            ..Default::default()
        })
        .await
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // ts==target is kept; ts<target is dropped.
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"seq\":2"));
        assert!(lines[1].contains("\"seq\":3"));
    }
}
