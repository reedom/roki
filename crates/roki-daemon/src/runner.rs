// Walking-skeleton tasks land in dependency order: this command-form runner
// (task 4.4) precedes runtime wiring (4.5). Until that lands, the function is
// only exercised by the unit tests below, which triggers `dead_code` for the
// leaf API. Allow it module-locally instead of leaking the relaxation
// crate-wide, matching the pattern in `admission`, `capture`,
// `config::workflow`, and `linear::ticket`.
#![allow(dead_code)]

//! Command-form subprocess runner.
//!
//! Spawns `sh -c "<cmd>"` with stdout / stderr redirected into the
//! per-cycle [`CaptureLayout`] file handles, awaits exit, and returns the
//! resulting [`ExitStatus`] inside [`RunOutcome`].
//!
//! Scope (skeleton):
//! - One subprocess per cycle. No `pre` / `post` phases (Req 6.3).
//! - `cmd` is passed verbatim. No Liquid rendering (Req 6.4).
//! - The handles inside `CaptureLayout` are duplicated (`File::try_clone`)
//!   before being handed to `tokio::process::Command::stdout/stderr` so the
//!   layout retains its own handles for the runtime to flush after exit.

use std::process::{ExitStatus, Stdio};

use tokio::process::Command;

use crate::capture::CaptureLayout;
use crate::error::PhaseInfraError as RunnerError;

/// Result of spawning and awaiting a single `run.cmd` subprocess.
///
/// Holds the subprocess's [`ExitStatus`] (Req 6.5). The runtime maps this
/// onto the cycle outcome — the daemon exits 0 on a clean cycle regardless
/// of the child's exit code (Req 8.2), and the captured stdout / stderr
/// files satisfy the operator-inspection contract (Req 7.2).
#[derive(Debug)]
pub struct RunOutcome {
    pub exit_status: ExitStatus,
}

/// Spawn `sh -c "<cmd>"` with stdout / stderr redirected to the capture
/// files inside `layout`, await exit, and return the exit status.
///
/// The function takes `&CaptureLayout` rather than consuming it because the
/// runtime keeps the layout for post-exit flushing of the capture files.
/// `Stdio::from(File)` requires owning the underlying file descriptor, so
/// each handle is duplicated via `File::try_clone` before being moved into
/// the child's stdio. The duplicated descriptors are closed by the child's
/// drop, leaving the layout's originals intact.
///
/// Errors carry the offending `cmd` so the `tracing::error!` line can
/// identify the cause from the error alone, matching the Req 7.3 / 8.3
/// pattern shared with `CaptureError`.
pub async fn spawn(cmd: &str, layout: &CaptureLayout) -> Result<RunOutcome, RunnerError> {
    let stdout_handle = layout
        .stdout
        .try_clone()
        .map_err(|source| RunnerError::Spawn {
            cmd: cmd.to_string(),
            source,
        })?;
    let stderr_handle = layout
        .stderr
        .try_clone()
        .map_err(|source| RunnerError::Spawn {
            cmd: cmd.to_string(),
            source,
        })?;

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_handle))
        .stderr(Stdio::from(stderr_handle))
        .spawn()
        .map_err(|source| RunnerError::Spawn {
            cmd: cmd.to_string(),
            source,
        })?;

    let exit_status = child.wait().await.map_err(|source| RunnerError::Wait {
        cmd: cmd.to_string(),
        source,
    })?;

    Ok(RunOutcome { exit_status })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn spawn_captures_stdout_stderr_and_exit_code() {
        // Mirrors the design unit-test contract:
        //   `runner::spawn` against `sh -c "echo hi; echo err >&2; exit 7"`
        //   with a temp `CaptureLayout`: stdout and stderr files contain
        //   the expected bytes; `RunOutcome::exit_status` is 7
        //   (Req 6.5, 7.2).
        let root = TempDir::new().unwrap();
        let layout = capture::create(root.path(), "ENG-1").unwrap();
        let dir = layout.dir.clone();

        let outcome = spawn("echo hi; echo err >&2; exit 7", &layout)
            .await
            .expect("spawn should succeed for a well-formed sh -c command");

        // Drop the layout so the original file handles flush before we read.
        drop(layout);

        assert_eq!(
            outcome.exit_status.code(),
            Some(7),
            "Req 6.5: subprocess exit status must be recorded as the cycle outcome"
        );

        let stdout_bytes = fs::read_to_string(dir.join("stdout")).unwrap();
        let stderr_bytes = fs::read_to_string(dir.join("stderr")).unwrap();
        assert!(
            stdout_bytes.contains("hi"),
            "Req 7.2: stdout capture file must contain subprocess stdout, got {stdout_bytes:?}"
        );
        assert!(
            stderr_bytes.contains("err"),
            "Req 7.2: stderr capture file must contain subprocess stderr, got {stderr_bytes:?}"
        );
    }

    #[tokio::test]
    async fn spawn_records_zero_exit_for_successful_command() {
        // Boundary: confirm the happy-zero path is wired through the same
        // `ExitStatus` pipeline (Req 6.5).
        let root = TempDir::new().unwrap();
        let layout = capture::create(root.path(), "ENG-1").unwrap();

        let outcome = spawn("true", &layout)
            .await
            .expect("spawn should succeed for `true`");

        assert_eq!(outcome.exit_status.code(), Some(0));
    }

    #[tokio::test]
    async fn spawn_passes_cmd_verbatim_without_template_rendering() {
        // Req 6.4: the daemon must NOT perform Liquid rendering of `run.cmd`
        // during the skeleton phase. If `cmd` were rendered, the literal
        // `{{ ticket.id }}` would either be substituted or rejected; under
        // `sh -c` it is a literal string the shell echoes unchanged.
        let root = TempDir::new().unwrap();
        let layout = capture::create(root.path(), "ENG-1").unwrap();
        let dir = layout.dir.clone();

        let outcome = spawn("printf '{{ ticket.id }}'", &layout)
            .await
            .expect("spawn should succeed for printf");

        drop(layout);

        assert_eq!(outcome.exit_status.code(), Some(0));
        let stdout_bytes = fs::read_to_string(dir.join("stdout")).unwrap();
        assert_eq!(
            stdout_bytes, "{{ ticket.id }}",
            "Req 6.4: cmd must reach `sh -c` verbatim, no template rendering"
        );
    }
}
