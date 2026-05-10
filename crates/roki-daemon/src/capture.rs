//! Per-state capture layout.
//!
//! Layout: `<session_root>/<ticket-id>/cycle-<uuid>/visit-<n>/<state_id>.{stdout,stderr,exit_code,directive.json,terminal.json,events.jsonl}`
//! per fr:04 §Capture. Slice 8 dropped the legacy iter-N + phase-shaped
//! capture (`pre/run/post.{stdout,stderr,response.json}`) when the engine
//! switched to the state-machine model.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use uuid::Uuid;

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

/// Create `<session_root>/<sanitised_ticket>/cycle-<uuid>/visit-<n>/` per
/// fr:04 §Capture and return its path.
pub fn create_visit_dir(
    session_root: &Path,
    ticket_id: &str,
    cycle_id: Uuid,
    visit_n: u32,
) -> Result<PathBuf, CaptureError> {
    let safe_ticket = sanitize_ticket_id(ticket_id);
    let path = session_root
        .join(safe_ticket)
        .join(format!("cycle-{cycle_id}"))
        .join(format!("visit-{visit_n}"));
    fs::create_dir_all(&path).map_err(|source| CaptureError::CreateDir {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Open `<state_id>.stdout` and `<state_id>.stderr` inside `visit_dir`.
pub fn open_state_files(visit_dir: &Path, state_id: &str) -> Result<(File, File), CaptureError> {
    let stdout_path = visit_dir.join(format!("{state_id}.stdout"));
    let stderr_path = visit_dir.join(format!("{state_id}.stderr"));
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

/// Write `<state_id>.exit_code` inside `visit_dir`.
pub fn write_state_exit_code(
    visit_dir: &Path,
    state_id: &str,
    exit_code: i32,
) -> Result<(), CaptureError> {
    let path = visit_dir.join(format!("{state_id}.exit_code"));
    let mut file = File::create(&path).map_err(|source| CaptureError::OpenFile {
        path: path.clone(),
        source,
    })?;
    let body = format!("{exit_code}\n");
    file.write_all(body.as_bytes())
        .map_err(|source| CaptureError::Write { path, source })
}

/// Write `<state_id>.directive.json` (pretty-printed) inside `visit_dir`.
pub fn write_state_directive_json(
    visit_dir: &Path,
    state_id: &str,
    value: &serde_json::Value,
) -> Result<(), CaptureError> {
    let path = visit_dir.join(format!("{state_id}.directive.json"));
    let pretty = serde_json::to_vec_pretty(value).map_err(|err| CaptureError::Write {
        path: path.clone(),
        source: std::io::Error::other(err),
    })?;
    let mut file = File::create(&path).map_err(|source| CaptureError::OpenFile {
        path: path.clone(),
        source,
    })?;
    file.write_all(&pretty)
        .map_err(|source| CaptureError::Write { path, source })
}

/// Write `<state_id>.terminal.json` (pretty-printed) inside `visit_dir`.
pub fn write_state_terminal_json(
    visit_dir: &Path,
    state_id: &str,
    value: &serde_json::Value,
) -> Result<(), CaptureError> {
    let path = visit_dir.join(format!("{state_id}.terminal.json"));
    let pretty = serde_json::to_vec_pretty(value).map_err(|err| CaptureError::Write {
        path: path.clone(),
        source: std::io::Error::other(err),
    })?;
    let mut file = File::create(&path).map_err(|source| CaptureError::OpenFile {
        path: path.clone(),
        source,
    })?;
    file.write_all(&pretty)
        .map_err(|source| CaptureError::Write { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_visit_dir_builds_full_path() {
        let tmp = TempDir::new().unwrap();
        let path = create_visit_dir(tmp.path(), "ENG-1", Uuid::nil(), 3).unwrap();
        assert!(path.exists());
        let s = path.to_string_lossy();
        assert!(s.contains("ENG-1"));
        assert!(s.contains(&format!("cycle-{}", Uuid::nil())));
        assert!(s.ends_with("visit-3"));
    }

    #[test]
    fn sanitiser_keeps_safe_chars_replaces_others() {
        assert_eq!(sanitize_ticket_id("ENG-123"), "ENG-123");
        assert_eq!(sanitize_ticket_id("a/b c"), "a_b_c");
        assert_eq!(sanitize_ticket_id("x_y-z"), "x_y-z");
    }

    #[test]
    fn open_state_files_creates_stdout_and_stderr() {
        let tmp = TempDir::new().unwrap();
        let dir = create_visit_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let (out, err) = open_state_files(&dir, "judge").unwrap();
        drop(out);
        drop(err);
        assert!(dir.join("judge.stdout").is_file());
        assert!(dir.join("judge.stderr").is_file());
    }

    #[test]
    fn write_state_exit_code_writes_text() {
        let tmp = TempDir::new().unwrap();
        let dir = create_visit_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        write_state_exit_code(&dir, "impl", 7).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("impl.exit_code")).unwrap(),
            "7\n"
        );
    }

    #[test]
    fn write_state_directive_json_pretty() {
        let tmp = TempDir::new().unwrap();
        let dir = create_visit_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let value = serde_json::json!({"directive":"end"});
        write_state_directive_json(&dir, "judge", &value).unwrap();
        let body = std::fs::read_to_string(dir.join("judge.directive.json")).unwrap();
        assert!(body.contains("\"directive\""));
    }

    #[test]
    fn write_state_terminal_json_pretty() {
        let tmp = TempDir::new().unwrap();
        let dir = create_visit_dir(tmp.path(), "X", Uuid::nil(), 1).unwrap();
        let value = serde_json::json!({"type":"result","is_error":false});
        write_state_terminal_json(&dir, "impl", &value).unwrap();
        let body = std::fs::read_to_string(dir.join("impl.terminal.json")).unwrap();
        assert!(body.contains("is_error"));
    }

    #[test]
    fn unwritable_session_root_returns_create_dir_error() {
        let tmp = TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let bad_root = blocker.join("subdir");
        match create_visit_dir(&bad_root, "X", Uuid::nil(), 1) {
            Err(CaptureError::CreateDir { .. }) => {}
            other => panic!("expected CreateDir error, got {other:?}"),
        }
    }
}
