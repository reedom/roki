//! `roki log` — read per-ticket subprocess captures.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use thiserror::Error;

use crate::cli::shared::{
    config_resolve::{
        ResolveError, enforce_same_ticket, resolve_session_root, resolve_ticket_and_cycle,
    },
    tail::{tail_bytes, tail_lines},
    visit_lookup::{list_visits, resolve_iter_for_state, visit_dir},
};

#[derive(Debug, Default, Args)]
pub struct LogArgs {
    #[arg(long = "ticket", value_name = "ID")]
    pub ticket: Option<String>,
    #[arg(long = "cycle", value_name = "UUID")]
    pub cycle: Option<String>,
    #[arg(long = "state", value_name = "STATE_ID")]
    pub state: Option<String>,
    #[arg(long = "iter", value_name = "N", allow_negative_numbers = true)]
    pub iter: Option<i32>,
    #[arg(long = "stream", value_enum)]
    pub stream: Option<Stream>,
    #[arg(long = "tail", value_name = "N", conflicts_with = "bytes")]
    pub tail: Option<usize>,
    #[arg(long = "bytes", value_name = "N")]
    pub bytes: Option<u64>,
    #[arg(long = "list-visits")]
    pub list_visits: bool,
    #[arg(long = "meta")]
    pub meta: bool,
    #[arg(long = "follow")]
    pub follow: bool,
    #[arg(
        long = "follow-poll-ms",
        value_name = "MS",
        default_value_t = 200,
        hide = true
    )]
    pub follow_poll_ms: u64,
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
    Events,
    Terminal,
    Directive,
    ExitCode,
}

impl Stream {
    fn file_suffix(self) -> &'static str {
        match self {
            Stream::Stdout => ".stdout",
            Stream::Stderr => ".stderr",
            Stream::Events => ".events.jsonl",
            Stream::Terminal => ".terminal.json",
            Stream::Directive => ".directive.json",
            Stream::ExitCode => ".exit_code",
        }
    }
}

#[derive(Debug, Error)]
pub enum LogError {
    #[error("roki log: cannot resolve session_root (set --config or run from a state subprocess)")]
    NoSessionRoot,
    #[error("roki log: {0}")]
    Resolve(String),
    #[error("roki log: cross-ticket read refused")]
    CrossTicket,
    #[error("roki log: {0:?} not found")]
    NotFound(PathBuf),
    #[error("roki log: io: {0}")]
    Io(#[from] std::io::Error),
    #[error("roki log: {0}")]
    Other(String),
    /// User-input error (missing required arg, mutually-exclusive misuse).
    /// Mapped to exit code 2.
    #[error("roki log: {0}")]
    Usage(String),
}

/// Map a `ResolveError` to the matching `LogError` so the dispatcher's
/// exit-code table keeps working — `CrossTicketRefused` and the
/// "missing" variants are usage errors (exit 2), the rest are runtime.
fn resolve_to_log_err(err: ResolveError) -> LogError {
    match err {
        ResolveError::CrossTicketRefused => LogError::CrossTicket,
        ResolveError::NoSessionRoot => LogError::NoSessionRoot,
        ResolveError::NoApiUrl => LogError::Resolve(err.to_string()),
        ResolveError::MissingTicket | ResolveError::MissingCycle => {
            LogError::Usage(err.to_string())
        }
        ResolveError::LoadConfig(e) => LogError::Other(format!("config: {e}")),
    }
}

pub async fn run(args: LogArgs) -> ExitCode {
    if args.follow {
        match run_follow_streaming(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err @ (LogError::CrossTicket | LogError::Usage(_))) => {
                eprintln!("{err}");
                ExitCode::from(2)
            }
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(1)
            }
        }
    } else {
        match run_capture_inner(args).await {
            Ok(bytes) => {
                use std::io::Write;
                let _ = std::io::stdout().write_all(&bytes);
                ExitCode::SUCCESS
            }
            Err(err @ (LogError::CrossTicket | LogError::Usage(_))) => {
                eprintln!("{err}");
                ExitCode::from(2)
            }
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(1)
            }
        }
    }
}

#[cfg(test)]
pub(crate) async fn run_capture(args: LogArgs) -> Result<String, LogError> {
    if args.follow {
        let bytes = follow_file_for_test(args).await?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let bytes = run_capture_inner(args).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Output sink for the follow loop. Production writes chunks directly to
/// stdout; tests accumulate into a `Vec<u8>` so the bytes can be asserted.
trait FollowSink: Send {
    fn write_chunk<'a>(
        &'a mut self,
        bytes: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>>;
}

struct StdoutFollowSink {
    stdout: tokio::io::Stdout,
}

impl FollowSink for StdoutFollowSink {
    fn write_chunk<'a>(
        &'a mut self,
        bytes: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            use tokio::io::AsyncWriteExt;
            self.stdout.write_all(bytes).await?;
            self.stdout.flush().await
        })
    }
}

#[cfg(test)]
struct CapturingFollowSink(Vec<u8>);

#[cfg(test)]
impl FollowSink for CapturingFollowSink {
    fn write_chunk<'a>(
        &'a mut self,
        bytes: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>> {
        let owned = bytes.to_vec();
        Box::pin(async move {
            self.0.extend_from_slice(&owned);
            Ok(())
        })
    }
}

/// Common --follow prelude shared between production and tests. Resolves
/// every CLI input down to the capture file path and the state id (used
/// to build the exit-code sentinel).
fn resolve_follow_target(args: &LogArgs) -> Result<(std::path::PathBuf, String, u64), LogError> {
    enforce_same_ticket(args.ticket.as_deref()).map_err(resolve_to_log_err)?;
    let session_root = resolve_session_root(args.config.as_deref()).map_err(resolve_to_log_err)?;
    let (ticket, cycle) = resolve_ticket_and_cycle(args.ticket.as_deref(), args.cycle.as_deref())
        .map_err(resolve_to_log_err)?;
    let cycle_dir = session_root.join(&ticket).join(format!("cycle-{cycle}"));
    let stream = args
        .stream
        .ok_or_else(|| LogError::Usage("--stream required for --follow".into()))?;
    if !matches!(stream, Stream::Stdout | Stream::Stderr) {
        return Err(LogError::Usage(
            "--follow supported only with --stream stdout|stderr".into(),
        ));
    }
    let state = args
        .state
        .clone()
        .ok_or_else(|| LogError::Usage("--state required for --follow".into()))?;
    let visit = resolve_iter_for_state(&cycle_dir, args.iter, Some(&state))
        .map_err(|e| LogError::Other(format!("{e}")))?;
    let file = visit_dir(&cycle_dir, visit).join(format!("{state}{}", stream.file_suffix()));
    if !file.exists() {
        return Err(LogError::NotFound(file));
    }
    Ok((file, state, args.follow_poll_ms))
}

/// Single follow loop body shared by production and tests. Polls the
/// capture file for new bytes, writes them to `sink`, terminates after
/// draining any final bytes once the visit's `<state>.exit_code`
/// sentinel appears on disk.
async fn follow_loop(
    file: &std::path::Path,
    state: &str,
    poll_ms: u64,
    sink: &mut dyn FollowSink,
) -> Result<(), LogError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(file).await.map_err(LogError::Io)?;
    let mut offset: u64 = 0;
    let exit_sentinel = file.with_file_name(format!("{state}.exit_code"));
    loop {
        let len = f.metadata().await.map_err(LogError::Io)?.len();
        if len > offset {
            let mut buf = vec![0u8; (len - offset) as usize];
            f.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(LogError::Io)?;
            f.read_exact(&mut buf).await.map_err(LogError::Io)?;
            sink.write_chunk(&buf).await.map_err(LogError::Io)?;
            offset = len;
        }
        if exit_sentinel.exists() {
            // Drain any bytes appended between the last poll and the
            // sentinel write so the consumer sees the full capture.
            let len = f.metadata().await.map_err(LogError::Io)?.len();
            if len > offset {
                let mut buf = vec![0u8; (len - offset) as usize];
                f.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(LogError::Io)?;
                f.read_exact(&mut buf).await.map_err(LogError::Io)?;
                sink.write_chunk(&buf).await.map_err(LogError::Io)?;
            }
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
    }
}

async fn run_follow_streaming(args: LogArgs) -> Result<(), LogError> {
    let (file, state, poll_ms) = resolve_follow_target(&args)?;
    let mut sink = StdoutFollowSink {
        stdout: tokio::io::stdout(),
    };
    follow_loop(&file, &state, poll_ms, &mut sink).await
}

#[cfg(test)]
async fn follow_file_for_test(args: LogArgs) -> Result<Vec<u8>, LogError> {
    let (file, state, poll_ms) = resolve_follow_target(&args)?;
    let mut sink = CapturingFollowSink(Vec::new());
    follow_loop(&file, &state, poll_ms, &mut sink).await?;
    Ok(sink.0)
}

async fn run_capture_inner(args: LogArgs) -> Result<Vec<u8>, LogError> {
    enforce_same_ticket(args.ticket.as_deref()).map_err(resolve_to_log_err)?;
    let session_root = resolve_session_root(args.config.as_deref()).map_err(resolve_to_log_err)?;
    let (ticket, cycle) = resolve_ticket_and_cycle(args.ticket.as_deref(), args.cycle.as_deref())
        .map_err(resolve_to_log_err)?;
    let cycle_dir = session_root.join(&ticket).join(format!("cycle-{cycle}"));

    if args.list_visits {
        return list_visits_jsonl(&cycle_dir);
    }
    if args.meta {
        let p = cycle_dir.join("cycle.json");
        return std::fs::read(&p).map_err(|_| LogError::NotFound(p));
    }
    // Stream read.
    let stream = args.stream.ok_or_else(|| {
        LogError::Usage("--stream required (or pass --list-visits / --meta)".into())
    })?;
    let state = args
        .state
        .ok_or_else(|| LogError::Usage("--state required for stream reads".into()))?;
    let visit = resolve_iter_for_state(&cycle_dir, args.iter, Some(&state))
        .map_err(|e| LogError::Other(format!("{e}")))?;
    let file = visit_dir(&cycle_dir, visit).join(format!("{state}{}", stream.file_suffix()));
    if !file.exists() {
        return Err(LogError::NotFound(file));
    }
    if let Some(n) = args.tail {
        return tail_lines(&file, n).map_err(LogError::Io);
    }
    if let Some(n) = args.bytes {
        return tail_bytes(&file, n).map_err(LogError::Io);
    }
    std::fs::read(&file).map_err(|_| LogError::NotFound(file))
}

fn list_visits_jsonl(cycle_dir: &std::path::Path) -> Result<Vec<u8>, LogError> {
    let visits = list_visits(cycle_dir).map_err(|e| LogError::Other(format!("{e}")))?;
    let mut out = String::new();
    for n in visits {
        let vd = visit_dir(cycle_dir, n);
        let (state_id, exit_code) = pick_state_and_exit(&vd);
        out.push_str(&match exit_code {
            Some(code) => {
                format!("{{\"visit_n\":{n},\"state_id\":\"{state_id}\",\"exit_code\":{code}}}\n")
            }
            None => format!("{{\"visit_n\":{n},\"state_id\":\"{state_id}\"}}\n"),
        });
    }
    Ok(out.into_bytes())
}

fn pick_state_and_exit(visit_dir: &std::path::Path) -> (String, Option<i32>) {
    let mut state_id = String::new();
    let mut exit_code: Option<i32> = None;
    if let Ok(read) = std::fs::read_dir(visit_dir) {
        for entry in read.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && let Some(rest) = name.strip_suffix(".exit_code")
            {
                state_id = rest.to_string();
                if let Ok(s) = std::fs::read_to_string(entry.path())
                    && let Ok(n) = s.trim().parse::<i32>()
                {
                    exit_code = Some(n);
                }
                break;
            }
        }
    }
    (state_id, exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(session_root: &std::path::Path, ticket: &str, cycle: &str) -> std::path::PathBuf {
        let cycle_dir = session_root.join(ticket).join(format!("cycle-{cycle}"));
        for n in 1..=2u32 {
            let vd = cycle_dir.join(format!("visit-{n:03}"));
            std::fs::create_dir_all(&vd).unwrap();
            std::fs::write(vd.join("impl.stdout"), format!("v{n} stdout\n")).unwrap();
            std::fs::write(vd.join("impl.stderr"), format!("v{n} stderr\n")).unwrap();
            std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();
        }
        cycle_dir
    }

    #[tokio::test]
    async fn list_visits_emits_jsonl_for_each_visit() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = fixture(tmp.path(), "ENG-1", "00000000-0000-0000-0000-000000000001");
        let env = [
            (
                "ROKI_CONFIG_SESSION_ROOT",
                Some(tmp.path().to_str().unwrap()),
            ),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            (
                "ROKI_CYCLE_ID",
                Some("00000000-0000-0000-0000-000000000001"),
            ),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                list_visits: true,
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"visit_n\":1"));
        assert!(lines[1].contains("\"visit_n\":2"));
        assert!(lines[0].contains("\"exit_code\":0"));
    }

    #[tokio::test]
    async fn stream_stdout_default_iter_is_latest() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = fixture(tmp.path(), "ENG-1", "00000000-0000-0000-0000-000000000001");
        let env = [
            (
                "ROKI_CONFIG_SESSION_ROOT",
                Some(tmp.path().to_str().unwrap()),
            ),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            (
                "ROKI_CYCLE_ID",
                Some("00000000-0000-0000-0000-000000000001"),
            ),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                state: Some("impl".into()),
                stream: Some(Stream::Stdout),
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, "v2 stdout\n");
    }

    #[tokio::test]
    async fn stream_stdout_relative_iter_minus_one_reads_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = fixture(tmp.path(), "ENG-1", "00000000-0000-0000-0000-000000000001");
        let env = [
            (
                "ROKI_CONFIG_SESSION_ROOT",
                Some(tmp.path().to_str().unwrap()),
            ),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            (
                "ROKI_CYCLE_ID",
                Some("00000000-0000-0000-0000-000000000001"),
            ),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                state: Some("impl".into()),
                stream: Some(Stream::Stdout),
                iter: Some(-1),
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, "v2 stdout\n");
    }

    #[tokio::test]
    async fn meta_emits_cycle_json_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let cycle_dir = fixture(tmp.path(), "ENG-1", "00000000-0000-0000-0000-000000000001");
        std::fs::write(cycle_dir.join("cycle.json"), r#"{"hello":"world"}"#).unwrap();
        let env = [
            (
                "ROKI_CONFIG_SESSION_ROOT",
                Some(tmp.path().to_str().unwrap()),
            ),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            (
                "ROKI_CYCLE_ID",
                Some("00000000-0000-0000-0000-000000000001"),
            ),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                meta: true,
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, r#"{"hello":"world"}"#);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn follow_picks_up_late_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let cycle = "00000000-0000-0000-0000-000000000002";
        let cycle_dir = tmp.path().join("ENG-2").join(format!("cycle-{cycle}"));
        let vd = cycle_dir.join("visit-001");
        std::fs::create_dir_all(&vd).unwrap();
        let stdout_path = vd.join("impl.stdout");
        std::fs::write(&stdout_path, b"first\n").unwrap();

        // Writer task appends after 100 ms.
        let path_clone = stdout_path.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path_clone)
                .unwrap();
            f.write_all(b"second\n").unwrap();
            // signal end-of-test by writing the sentinel file the follower watches
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            std::fs::write(path_clone.with_extension("exit_code"), "0\n").unwrap();
        });

        let env = [
            (
                "ROKI_CONFIG_SESSION_ROOT",
                Some(tmp.path().to_str().unwrap()),
            ),
            ("ROKI_TICKET_ID", Some("ENG-2")),
            ("ROKI_CYCLE_ID", Some(cycle)),
        ];
        let collected = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                state: Some("impl".into()),
                stream: Some(Stream::Stdout),
                follow: true,
                follow_poll_ms: 50,
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        writer.await.unwrap();
        assert!(collected.contains("first"));
        assert!(collected.contains("second"));
    }

    #[tokio::test]
    async fn cross_ticket_refused_when_env_set_and_flag_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = fixture(tmp.path(), "ABC-1", "00000000-0000-0000-0000-000000000001");
        let env = [
            (
                "ROKI_CONFIG_SESSION_ROOT",
                Some(tmp.path().to_str().unwrap()),
            ),
            ("ROKI_TICKET_ID", Some("ABC-1")),
            (
                "ROKI_CYCLE_ID",
                Some("00000000-0000-0000-0000-000000000001"),
            ),
        ];
        let err = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                ticket: Some("XYZ-9".into()),
                state: Some("impl".into()),
                stream: Some(Stream::Stdout),
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("cross-ticket read refused"));
    }

    #[tokio::test]
    async fn stream_tail_returns_last_n_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let cycle = "00000000-0000-0000-0000-00000000000d";
        let vd = tmp
            .path()
            .join("ENG-1")
            .join(format!("cycle-{cycle}"))
            .join("visit-001");
        std::fs::create_dir_all(&vd).unwrap();
        std::fs::write(vd.join("impl.stdout"), b"a\nb\nc\nd\ne\n").unwrap();
        std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();
        let env = [
            (
                "ROKI_CONFIG_SESSION_ROOT",
                Some(tmp.path().to_str().unwrap()),
            ),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            ("ROKI_CYCLE_ID", Some(cycle)),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                state: Some("impl".into()),
                stream: Some(Stream::Stdout),
                tail: Some(2),
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, "d\ne\n");
    }

    #[tokio::test]
    async fn stream_bytes_returns_byte_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let cycle = "00000000-0000-0000-0000-00000000000e";
        let vd = tmp
            .path()
            .join("ENG-1")
            .join(format!("cycle-{cycle}"))
            .join("visit-001");
        std::fs::create_dir_all(&vd).unwrap();
        std::fs::write(vd.join("impl.stdout"), b"abcdef").unwrap();
        std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();
        let env = [
            (
                "ROKI_CONFIG_SESSION_ROOT",
                Some(tmp.path().to_str().unwrap()),
            ),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            ("ROKI_CYCLE_ID", Some(cycle)),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                state: Some("impl".into()),
                stream: Some(Stream::Stdout),
                bytes: Some(3),
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        assert_eq!(out, "def");
    }

    #[tokio::test]
    async fn list_visits_omits_exit_code_for_in_flight_visit() {
        let tmp = tempfile::tempdir().unwrap();
        let cycle = "00000000-0000-0000-0000-00000000000f";
        let cycle_dir = tmp.path().join("ENG-1").join(format!("cycle-{cycle}"));
        // visit-001 finished (exit_code present); visit-002 still in flight.
        let v1 = cycle_dir.join("visit-001");
        std::fs::create_dir_all(&v1).unwrap();
        std::fs::write(v1.join("impl.stdout"), b"x").unwrap();
        std::fs::write(v1.join("impl.exit_code"), "0\n").unwrap();
        let v2 = cycle_dir.join("visit-002");
        std::fs::create_dir_all(&v2).unwrap();
        std::fs::write(v2.join("impl.stdout"), b"y").unwrap();
        // No exit_code file for visit-002.
        let env = [
            (
                "ROKI_CONFIG_SESSION_ROOT",
                Some(tmp.path().to_str().unwrap()),
            ),
            ("ROKI_TICKET_ID", Some("ENG-1")),
            ("ROKI_CYCLE_ID", Some(cycle)),
        ];
        let out = temp_env::async_with_vars(env, async {
            run_capture(LogArgs {
                list_visits: true,
                ..Default::default()
            })
            .await
        })
        .await
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"exit_code\":0"), "{}", lines[0]);
        assert!(!lines[1].contains("\"exit_code\""), "{}", lines[1]);
    }
}
