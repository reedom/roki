//! Test-only fake `claude` binary used by `tests/engine_claude.rs` to drive
//! the subprocess supervisor through its observable-completion matrix
//! (task 2.10): clean exit, non-clean exit, stall, and prelude capture.
//!
//! Behaviour is selected at runtime by reading two files inside the
//! supervisor-supplied workspace cwd:
//!
//! * `.fake_claude_mode` — selects the scenario (`clean_exit`,
//!   `non_clean_exit`, `stall`, or `capture_prelude`).
//! * `.fake_claude_capture` — when present, holds an absolute path that the
//!   binary writes the captured stdin (the prelude envelope) to.
//!
//! File-based driving avoids env-var mutation in tests, which is unsafe
//! under the workspace-wide `unsafe_code = "forbid"` lint.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

fn main() -> ExitCode {
    let mode = fs::read_to_string(".fake_claude_mode")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "clean_exit".to_owned());

    // Always drain stdin so the supervisor's `write_all(...)` never blocks on
    // a full pipe. Optionally persist the captured input so the prelude
    // capture test can assert on it.
    let mut stdin_buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin_buf);
    if let Ok(capture_path) = fs::read_to_string(".fake_claude_capture") {
        let target = PathBuf::from(capture_path.trim());
        let _ = fs::write(target, stdin_buf.as_bytes());
    }

    match mode.as_str() {
        "clean_exit" => clean_exit(),
        "non_clean_exit" => non_clean_exit(),
        "stall" => stall(),
        "capture_prelude" => ExitCode::from(0),
        _ => {
            eprintln!("fake_claude: unknown FAKE_CLAUDE_MODE `{mode}`");
            ExitCode::from(2)
        }
    }
}

/// Emit a representative stream-json sequence (system init + assistant
/// message + result) and exit cleanly. Drives the `engine_clean_exit`
/// integration scenario.
fn clean_exit() -> ExitCode {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let lines = [
        r#"{"type":"system","subtype":"init","session_id":"fake-clean","model":"claude-test","tools":[]}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#,
    ];
    for line in lines {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
    ExitCode::from(0)
}

/// Emit a partial stream then exit non-zero. Drives the
/// `engine_non_clean_exit` integration scenario.
fn non_clean_exit() -> ExitCode {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(
        out,
        r#"{{"type":"system","subtype":"init","session_id":"fake-failing"}}"#
    );
    let _ = out.flush();
    ExitCode::from(7)
}

/// Open stdout and write nothing. The supervisor's stall watchdog detects
/// event-inactivity beyond the configured stall window, kills the
/// subprocess, and reports `WorkerOutcome::Stalled`. Drives the
/// `engine_stall` integration scenario.
fn stall() -> ExitCode {
    // Sleep well past any reasonable stall window the test will configure.
    // The supervisor will SIGKILL us before this returns; the explicit
    // duration is only a safety net in case the test harness fails to
    // terminate the child.
    thread::sleep(Duration::from_secs(60));
    ExitCode::from(0)
}
