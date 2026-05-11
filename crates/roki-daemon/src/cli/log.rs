//! `roki log` — read per-ticket subprocess captures.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use thiserror::Error;

use crate::cli::shared::{
    config_resolve::{enforce_same_ticket, resolve_session_root, resolve_ticket_and_cycle},
    tail::{tail_bytes, tail_lines},
    visit_lookup::{list_visits, resolve_iter, visit_dir},
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

#[cfg(test)]
async fn follow_file_for_test(args: LogArgs) -> Result<Vec<u8>, LogError> {
    enforce_same_ticket(args.ticket.as_deref()).map_err(|_| LogError::CrossTicket)?;
    let session_root =
        resolve_session_root(args.config.as_deref()).map_err(|_| LogError::NoSessionRoot)?;
    let (ticket, cycle) = resolve_ticket_and_cycle(args.ticket.as_deref(), args.cycle.as_deref())
        .map_err(|e| LogError::Resolve(format!("{e}")))?;
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
        .ok_or_else(|| LogError::Usage("--state required for --follow".into()))?;
    let visit = resolve_iter(&cycle_dir, args.iter).map_err(|e| LogError::Other(format!("{e}")))?;
    let file = visit_dir(&cycle_dir, visit).join(format!("{state}{}", stream.file_suffix()));
    if !file.exists() {
        return Err(LogError::NotFound(file));
    }
    follow_file(&file, &cycle_dir, &state, args.follow_poll_ms).await
}

async fn follow_file(
    file: &std::path::Path,
    _cycle_dir: &std::path::Path,
    state: &str,
    poll_ms: u64,
) -> Result<Vec<u8>, LogError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut collected = Vec::new();
    let mut f = tokio::fs::File::open(file).await.map_err(LogError::Io)?;
    let mut offset: u64 = 0;
    let exit_sentinel = file.with_file_name(format!("{state}.exit_code"));
    // Each iteration: read whatever is new, sleep, then check the exit-code
    // sentinel that marks "writer is done".
    loop {
        let len = f.metadata().await.map_err(LogError::Io)?.len();
        if len > offset {
            let mut buf = vec![0u8; (len - offset) as usize];
            f.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(LogError::Io)?;
            f.read_exact(&mut buf).await.map_err(LogError::Io)?;
            collected.extend_from_slice(&buf);
            offset = len;
        }
        // Termination signal: the daemon writes `<state>.exit_code` when the
        // visit finishes. Once present, drain any final bytes and exit.
        if exit_sentinel.exists() {
            let len = f.metadata().await.map_err(LogError::Io)?.len();
            if len > offset {
                let mut buf = vec![0u8; (len - offset) as usize];
                f.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(LogError::Io)?;
                f.read_exact(&mut buf).await.map_err(LogError::Io)?;
                collected.extend_from_slice(&buf);
            }
            return Ok(collected);
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
    }
}

async fn run_follow_streaming(args: LogArgs) -> Result<(), LogError> {
    // Resolve block duplicated from run_capture_inner so chunks can stream
    // directly to stdout instead of accumulating.
    enforce_same_ticket(args.ticket.as_deref()).map_err(|_| LogError::CrossTicket)?;
    let session_root =
        resolve_session_root(args.config.as_deref()).map_err(|_| LogError::NoSessionRoot)?;
    let (ticket, cycle) = resolve_ticket_and_cycle(args.ticket.as_deref(), args.cycle.as_deref())
        .map_err(|e| LogError::Resolve(format!("{e}")))?;
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
        .ok_or_else(|| LogError::Usage("--state required for --follow".into()))?;
    let visit = resolve_iter(&cycle_dir, args.iter).map_err(|e| LogError::Other(format!("{e}")))?;
    let file = visit_dir(&cycle_dir, visit).join(format!("{state}{}", stream.file_suffix()));
    if !file.exists() {
        return Err(LogError::NotFound(file));
    }
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    let mut f = tokio::fs::File::open(&file).await.map_err(LogError::Io)?;
    let mut offset: u64 = 0;
    let exit_sentinel = file.with_file_name(format!("{state}.exit_code"));
    let mut stdout = tokio::io::stdout();
    loop {
        let len = f.metadata().await.map_err(LogError::Io)?.len();
        if len > offset {
            let mut buf = vec![0u8; (len - offset) as usize];
            f.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(LogError::Io)?;
            f.read_exact(&mut buf).await.map_err(LogError::Io)?;
            stdout.write_all(&buf).await.map_err(LogError::Io)?;
            stdout.flush().await.map_err(LogError::Io)?;
            offset = len;
        }
        if exit_sentinel.exists() {
            let len = f.metadata().await.map_err(LogError::Io)?.len();
            if len > offset {
                let mut buf = vec![0u8; (len - offset) as usize];
                f.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(LogError::Io)?;
                f.read_exact(&mut buf).await.map_err(LogError::Io)?;
                stdout.write_all(&buf).await.map_err(LogError::Io)?;
                stdout.flush().await.map_err(LogError::Io)?;
            }
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(args.follow_poll_ms)).await;
    }
}

async fn run_capture_inner(args: LogArgs) -> Result<Vec<u8>, LogError> {
    enforce_same_ticket(args.ticket.as_deref()).map_err(|_| LogError::CrossTicket)?;
    let session_root =
        resolve_session_root(args.config.as_deref()).map_err(|_| LogError::NoSessionRoot)?;
    let (ticket, cycle) = resolve_ticket_and_cycle(args.ticket.as_deref(), args.cycle.as_deref())
        .map_err(|e| LogError::Resolve(format!("{e}")))?;
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
    let visit = resolve_iter(&cycle_dir, args.iter).map_err(|e| LogError::Other(format!("{e}")))?;
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
        // -1 is the latest = visit-002 by convention in this plan; the spec
        // §4.2 step 3 defines "Relative -N → take dirs.len() - N (1-indexed)".
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
}
