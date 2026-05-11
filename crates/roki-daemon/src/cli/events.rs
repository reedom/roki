//! `roki events` — read the structured event stream (online HTTP or offline file).
//!
//! Slice 11 lands this command in two tasks: this file implements the
//! offline JSON-Lines reader path (Task 8). The online HTTP client lands
//! in Task 9 and replaces the stub in `run_online_dispatch`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use roki_api_types::{ApiEvent, EventsPage};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::cli::shared::config_resolve::resolve_api_url;
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

async fn run_online_dispatch(args: EventsArgs, _format: Format) -> ExitCode {
    let base = match resolve_api_url(args.api.as_deref(), args.config.as_deref()) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("roki events: {e}");
            return ExitCode::from(1);
        }
    };
    let mut sink = StdoutSink;
    match run_online(args, base, &mut sink).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(1)
        }
    }
}

/// Output sink. Lets tests capture lines without touching real stdout.
trait Sink: Send {
    fn write_line(&mut self, line: &str) -> Result<(), EventsError>;
}

struct StdoutSink;

impl Sink for StdoutSink {
    fn write_line(&mut self, line: &str) -> Result<(), EventsError> {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        lock.write_all(line.as_bytes())?;
        lock.write_all(b"\n")?;
        Ok(())
    }
}

#[cfg(test)]
#[derive(Default)]
struct StringSink(String);

#[cfg(test)]
impl Sink for StringSink {
    fn write_line(&mut self, line: &str) -> Result<(), EventsError> {
        self.0.push_str(line);
        self.0.push('\n');
        Ok(())
    }
}

async fn run_online(
    args: EventsArgs,
    base: String,
    sink: &mut dyn Sink,
) -> Result<(), EventsError> {
    let client = reqwest::Client::new();
    let filter = Filter::from_args(&args)?;
    let mut since: u64 = filter.since_seq.unwrap_or(0);
    let url = format!("{}/api/events", base.trim_end_matches('/'));
    // Surface ring gaps to stderr at most once per contiguous run so tail
    // mode doesn't spam when the server keeps returning gap=true.
    let mut gap_reported = false;
    loop {
        let mut req = client.get(&url).query(&[("since", since.to_string())]);
        if let Some(k) = &args.kind {
            req = req.query(&[("kind", k.as_str())]);
        }
        if let Some(t) = &args.ticket {
            req = req.query(&[("ticket", t.as_str())]);
        }
        if let Some(c) = &args.cycle {
            req = req.query(&[("cycle", c.as_str())]);
        }
        let page: EventsPage = match fetch_page(req).await {
            Ok(p) => p,
            Err(err) if args.tail => {
                // Transient errors in --tail mode are reported and retried.
                // Non-tail callers still get the failure as exit 1.
                eprintln!("# roki events: transient http error: {err}; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(args.cadence_ms)).await;
                continue;
            }
            Err(err) => return Err(err),
        };
        if page.gap {
            if !gap_reported {
                eprintln!("# roki events: ring gap detected; consult [log].file_path");
                gap_reported = true;
            }
            // Also emit a structured marker into the output stream so JSON
            // consumers piping events downstream see the discontinuity.
            sink.write_line(&gap_marker_line(args.format, since))?;
        } else {
            gap_reported = false;
        }
        for ev in &page.events {
            if !filter.accept_after_seq_cursor(ev) {
                continue;
            }
            match args.format {
                Format::Json => {
                    let line = serde_json::to_string(ev)
                        .map_err(|e| EventsError::BadLine(format!("{e}")))?;
                    sink.write_line(&line)?;
                }
                Format::Human => {
                    sink.write_line(&format_human(ev))?;
                }
            }
        }
        // Pagination + tail semantics:
        //   - non-tail: keep paging until next_since is None (drain the ring once).
        //   - tail: never break; sleep when next_since is None and re-poll for new events.
        match page.next_since {
            Some(n) => since = n,
            None if !args.tail => break,
            None => {}
        }
        if args.tail {
            tokio::time::sleep(std::time::Duration::from_millis(args.cadence_ms)).await;
        }
    }
    Ok(())
}

async fn fetch_page(req: reqwest::RequestBuilder) -> Result<EventsPage, EventsError> {
    req.send()
        .await
        .map_err(|e| EventsError::Http(format!("{e}")))?
        .error_for_status()
        .map_err(|e| EventsError::Http(format!("{e}")))?
        .json::<EventsPage>()
        .await
        .map_err(|e| EventsError::Http(format!("{e}")))
}

fn gap_marker_line(format: Format, since: u64) -> String {
    match format {
        Format::Json => format!(r#"{{"event":"__gap__","since":{since},"detail":"ring overrun"}}"#),
        Format::Human => format!("-  -  __gap__  since={since}  detail=ring overrun"),
    }
}

#[cfg(test)]
pub(crate) async fn run_capture(args: EventsArgs) -> Result<String, EventsError> {
    if args.offline {
        return run_offline_capture(args).await;
    }
    run_capture_online(args).await
}

#[cfg(test)]
pub(crate) async fn run_capture_online(args: EventsArgs) -> Result<String, EventsError> {
    let base = resolve_api_url(args.api.as_deref(), args.config.as_deref())
        .map_err(|e| EventsError::Resolve(format!("{e}")))?;
    let mut sink = StringSink::default();
    run_online(args, base, &mut sink).await?;
    // Trim trailing newline so callers can `out.lines()` cleanly.
    let mut out = sink.0;
    if out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

async fn run_offline_capture(args: EventsArgs) -> Result<String, EventsError> {
    use std::io::BufRead;
    let file_path = args.file.clone().ok_or(EventsError::NoFile)?;
    let file = std::fs::File::open(&file_path)?;
    let reader = std::io::BufReader::new(file);
    let filter = Filter::from_args(&args)?;
    let mut out = String::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let ev: ApiEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(err) => {
                eprintln!(
                    "# roki events: {}:{}: malformed line skipped: {err}",
                    file_path.display(),
                    idx + 1
                );
                continue;
            }
        };
        if !filter.accept(&ev) {
            continue;
        }
        match args.format {
            Format::Json => {
                out.push_str(&line);
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
    // skip them here.
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn online_dump_against_wiremock() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let page1 = serde_json::json!({
            "events": [{
                "seq": 1,
                "ts": "2026-05-11T10:00:00Z",
                "event": "webhook_received",
                "ticket_id": "ENG-1",
                "payload": {"foo": "bar"}
            }],
            "gap": false,
            "next_since": 2,
        });
        let page2 = serde_json::json!({
            "events": [],
            "gap": false,
        });
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .and(query_param("since", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .and(query_param("since", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page2))
            .mount(&server)
            .await;

        let out = run_capture_online(EventsArgs {
            api: Some(server.uri()),
            format: Format::Json,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(out.contains("\"seq\":1"), "out was: {out}");
        assert!(out.contains("webhook_received"), "out was: {out}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn online_emits_structured_gap_marker_when_server_reports_gap() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let page1 = serde_json::json!({
            "events": [{
                "seq": 5,
                "ts": "2026-05-11T10:00:00Z",
                "event": "cycle_started",
                "ticket_id": "ENG-9",
                "payload": {}
            }],
            "gap": true,
            "next_since": 6,
        });
        let page2 = serde_json::json!({ "events": [], "gap": false });
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .and(query_param("since", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .and(query_param("since", "6"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page2))
            .mount(&server)
            .await;

        let out = run_capture_online(EventsArgs {
            api: Some(server.uri()),
            format: Format::Json,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(out.contains("__gap__"), "expected gap marker in out: {out}");
        // Loop continued past the gap and consumed page2.
        assert!(out.contains("\"seq\":5"), "expected event after gap: {out}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn online_tail_retries_after_transient_http_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // First match returns 503 (transient); a second `up_to_n_times` lets the
        // retry observe a 200. wiremock matches in registration order; mount the
        // failure first with `up_to_n_times(1)`, then the success with
        // `up_to_n_times(1)`, then a final empty drain page.
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let page = serde_json::json!({
            "events": [{
                "seq": 1,
                "ts": "2026-05-11T10:00:00Z",
                "event": "webhook_received",
                "ticket_id": "ENG-1",
                "payload": {}
            }],
            "gap": false,
        });
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // After the success page, tail keeps polling — return drain pages so the
        // test can observe the first success without hanging.
        let drain = serde_json::json!({ "events": [], "gap": false });
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(drain))
            .mount(&server)
            .await;

        // Drive the loop in a background task so we can stop it once the
        // success page lands in the sink.
        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        struct ChanSink(tokio::sync::mpsc::UnboundedSender<String>);
        impl Sink for ChanSink {
            fn write_line(&mut self, line: &str) -> Result<(), EventsError> {
                let _ = self.0.send(line.to_string());
                Ok(())
            }
        }
        let args = EventsArgs {
            api: Some(server.uri()),
            tail: true,
            cadence_ms: 50,
            ..Default::default()
        };
        let base = resolve_api_url(args.api.as_deref(), None).unwrap();
        let task = tokio::spawn(async move {
            let mut sink = ChanSink(sink_tx);
            let _ = run_online(args, base, &mut sink).await;
        });
        let line = tokio::time::timeout(std::time::Duration::from_secs(2), sink_rx.recv())
            .await
            .expect("event delivered after 503 retry")
            .expect("sink received line");
        assert!(line.contains("\"seq\":1"), "got: {line}");
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn online_since_rfc3339_filters_strictly_older() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = serde_json::json!({
            "events": [
                {
                    "seq": 1,
                    "ts": "2026-05-11T10:00:00Z",
                    "event": "webhook_received",
                    "ticket_id": "ENG-1",
                    "payload": {}
                },
                {
                    "seq": 2,
                    "ts": "2026-05-11T10:00:05Z",
                    "event": "cycle_started",
                    "ticket_id": "ENG-1",
                    "payload": {}
                }
            ],
            "gap": false,
        });
        Mock::given(method("GET"))
            .and(path("/api/events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let out = run_capture_online(EventsArgs {
            api: Some(server.uri()),
            since: Some("2026-05-11T10:00:05Z".into()),
            format: Format::Json,
            ..Default::default()
        })
        .await
        .unwrap();
        // Only seq=2 (ts == cutoff) survives the client-side rfc3339 filter.
        assert!(!out.contains("\"seq\":1"), "older event leaked: {out}");
        assert!(out.contains("\"seq\":2"), "expected event missing: {out}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn online_resolve_api_url_errors_with_no_source() {
        let err = temp_env::async_with_vars([("ROKI_API_URL", None::<&str>)], async {
            run_capture_online(EventsArgs::default()).await.unwrap_err()
        })
        .await;
        assert!(
            format!("{err}").contains("cannot resolve API URL"),
            "err was: {err}"
        );
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
