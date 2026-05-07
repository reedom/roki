//! Per-iter capture layout.
//!
//! Layout: `<session_root>/<ticket-id>/cycle-<uuid>/iter-<n>/{pre,run,post}.{stdout,stderr}`
//! plus parsed-derivative files (`pre.response.json`, `run.exit_code`,
//! `post.response.json`). The skeleton's flat `cycle-<uuid>/{stdout,stderr}`
//! layout is gone.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::engine::outcome::PhaseKind;
use crate::error::CaptureError;

/// Sanitise a ticket id for filesystem use. Keeps `[A-Za-z0-9_-]`; replaces
/// every other byte (slashes, spaces, unicode) with `_`. Linear-style ids
/// like `ENG-123` survive verbatim.
pub fn sanitize_ticket_id(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => b as char,
            _ => '_',
        })
        .collect()
}

/// Create `<session_root>/<sanitised_ticket>/cycle-<uuid>/iter-<n>/` and
/// return its path. The directory is empty until `open_phase_files` is
/// called for each phase.
pub fn create_iter_dir(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
    iter: u32,
) -> Result<PathBuf, CaptureError> {
    let safe_ticket = sanitize_ticket_id(ticket_id);
    let path = session_root
        .join(safe_ticket)
        .join(format!("cycle-{cycle_id}"))
        .join(format!("iter-{iter}"));
    fs::create_dir_all(&path).map_err(|source| CaptureError::CreateDir {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Open `<phase>.stdout` and `<phase>.stderr` inside `iter_dir`. Returns the
/// pair `(stdout, stderr)` ready for `Stdio::from(File)` redirection.
pub fn open_phase_files(
    iter_dir: &Path,
    phase: PhaseKind,
) -> Result<(File, File), CaptureError> {
    let stdout_path = iter_dir.join(format!("{}.stdout", phase.as_str()));
    let stderr_path = iter_dir.join(format!("{}.stderr", phase.as_str()));
    let stdout = File::create(&stdout_path).map_err(|source| CaptureError::OpenFile {
        path: stdout_path,
        source,
    })?;
    let stderr = File::create(&stderr_path).map_err(|source| CaptureError::OpenFile {
        path: stderr_path,
        source,
    })?;
    Ok((stdout, stderr))
}

/// Write `<phase>.response.json` (pretty-printed) inside `iter_dir`. Used
/// after a successful Pre or Post directive parse.
pub fn write_response_json(
    iter_dir: &Path,
    phase: PhaseKind,
    value: &serde_json::Value,
) -> Result<(), CaptureError> {
    let path = iter_dir.join(format!("{}.response.json", phase.as_str()));
    let pretty = serde_json::to_vec_pretty(value).map_err(|err| CaptureError::Write {
        path: path.clone(),
        source: std::io::Error::other(err),
    })?;
    let mut file = File::create(&path).map_err(|source| CaptureError::OpenFile {
        path: path.clone(),
        source,
    })?;
    file.write_all(&pretty).map_err(|source| CaptureError::Write {
        path,
        source,
    })
}

/// Write `run.exit_code` inside `iter_dir`. The text contents are
/// `"<exit>\n"`.
pub fn write_run_exit_code(iter_dir: &Path, exit_code: i32) -> Result<(), CaptureError> {
    let path = iter_dir.join("run.exit_code");
    let mut file = File::create(&path).map_err(|source| CaptureError::OpenFile {
        path: path.clone(),
        source,
    })?;
    let body = format!("{exit_code}\n");
    file.write_all(body.as_bytes()).map_err(|source| CaptureError::Write {
        path,
        source,
    })
}

/// Files opened for one session-shape phase turn. The supervisor opens these
/// at the start of `run_turn(kind, ...)` and rotates them when the next turn
/// starts.
#[allow(dead_code)]
pub struct SessionPhaseFiles {
    pub stdout: File,
    pub stderr: File,
    pub events: File,
}

/// Open `<phase>.stdout`, `<phase>.stderr`, and `<phase>.events.jsonl`
/// inside `iter_dir`. All three files are truncated on open per slice-1
/// `open_phase_files` semantics — the supervisor writes for the duration
/// of one turn and never reopens the same triple twice.
#[allow(dead_code)]
pub fn open_session_phase_files(
    iter_dir: &Path,
    phase: PhaseKind,
) -> Result<SessionPhaseFiles, CaptureError> {
    let stdout_path = iter_dir.join(format!("{}.stdout", phase.as_str()));
    let stderr_path = iter_dir.join(format!("{}.stderr", phase.as_str()));
    let events_path = iter_dir.join(format!("{}.events.jsonl", phase.as_str()));
    let stdout = File::create(&stdout_path).map_err(|source| CaptureError::OpenFile {
        path: stdout_path,
        source,
    })?;
    let stderr = File::create(&stderr_path).map_err(|source| CaptureError::OpenFile {
        path: stderr_path,
        source,
    })?;
    let events = File::create(&events_path).map_err(|source| CaptureError::OpenFile {
        path: events_path,
        source,
    })?;
    Ok(SessionPhaseFiles {
        stdout,
        stderr,
        events,
    })
}

/// Write `run.terminal.json` (pretty-printed) inside `iter_dir`. Used when
/// the run-phase tee scanner spots a claude/codex `result` event mid-stream.
#[allow(dead_code)]
pub fn write_run_terminal_json(
    iter_dir: &Path,
    value: &serde_json::Value,
) -> Result<(), CaptureError> {
    let path = iter_dir.join("run.terminal.json");
    let pretty = serde_json::to_vec_pretty(value).map_err(|err| CaptureError::Write {
        path: path.clone(),
        source: std::io::Error::other(err),
    })?;
    let mut file = File::create(&path).map_err(|source| CaptureError::OpenFile {
        path: path.clone(),
        source,
    })?;
    file.write_all(&pretty).map_err(|source| CaptureError::Write {
        path,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_iter_dir_builds_full_path() {
        let tmp = TempDir::new().unwrap();
        let path = create_iter_dir(tmp.path(), "ENG-1", Uuid::nil(), 3).unwrap();
        assert!(path.exists());
        let s = path.to_string_lossy();
        assert!(s.contains("ENG-1"));
        assert!(s.contains(&format!("cycle-{}", Uuid::nil())));
        assert!(s.ends_with("iter-3"));
    }

    #[test]
    fn sanitiser_keeps_safe_chars_replaces_others() {
        assert_eq!(sanitize_ticket_id("ENG-123"), "ENG-123");
        assert_eq!(sanitize_ticket_id("a/b c"), "a_b_c");
        assert_eq!(sanitize_ticket_id("x_y-z"), "x_y-z");
    }

    #[test]
    fn open_phase_files_creates_stdout_and_stderr() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let (out, err) = open_phase_files(&dir, PhaseKind::Run).unwrap();
        drop(out);
        drop(err);
        assert!(dir.join("run.stdout").is_file());
        assert!(dir.join("run.stderr").is_file());
    }

    #[test]
    fn write_response_json_writes_pretty_payload() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let value = serde_json::json!({"directive":"run","note":"hi"});
        write_response_json(&dir, PhaseKind::Pre, &value).unwrap();
        let body = std::fs::read_to_string(dir.join("pre.response.json")).unwrap();
        assert!(body.contains("\"directive\""));
        assert!(body.contains("\"hi\""));
    }

    #[test]
    fn write_run_exit_code_writes_text() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        write_run_exit_code(&dir, 7).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("run.exit_code")).unwrap(),
            "7\n"
        );
    }

    #[test]
    fn open_session_phase_files_creates_three_files() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "ENG-1", Uuid::nil(), 1).unwrap();
        let files = open_session_phase_files(&dir, PhaseKind::Pre).unwrap();
        drop(files);
        assert!(dir.join("pre.stdout").is_file());
        assert!(dir.join("pre.stderr").is_file());
        assert!(dir.join("pre.events.jsonl").is_file());
    }

    #[test]
    fn write_run_terminal_json_writes_pretty_payload() {
        let tmp = TempDir::new().unwrap();
        let dir = create_iter_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let value = serde_json::json!({"type":"result","is_error":false});
        write_run_terminal_json(&dir, &value).unwrap();
        let body = std::fs::read_to_string(dir.join("run.terminal.json")).unwrap();
        assert!(body.contains("\"is_error\""));
        assert!(body.contains("false"));
    }

    #[test]
    fn unwritable_session_root_returns_create_dir_error() {
        let tmp = TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let bad_root = blocker.join("subdir");
        match create_iter_dir(&bad_root, "X", Uuid::nil(), 1) {
            Err(CaptureError::CreateDir { .. }) => {}
            other => panic!("expected CreateDir error, got {other:?}"),
        }
    }
}
