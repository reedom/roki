// Walking-skeleton tasks land in dependency order: this capture layout
// (task 4.3) precedes the runner (4.4) and runtime wiring (4.5) that call
// `create` per cycle. Until those land, the function and `CaptureLayout`
// struct are exercised only by the unit tests below, which triggers
// `dead_code` for the leaf API. Allow it module-locally instead of leaking
// the relaxation crate-wide, matching the pattern in `admission`,
// `config::workflow`, and `linear::ticket`.
#![allow(dead_code)]

//! Per-cycle capture layout. Synchronous filesystem.
//!
//! Creates `<session_root>/cycle-<uuid>/` and opens the stdout/stderr file
//! handles inside it for the runner to redirect into. The skeleton path
//! intentionally omits the canonical `<ticket-id>/cycle-<uuid>/iter-<n>/...`
//! layout — that landing is deferred to `roki-runtime-capture-layout`
//! (see roki-skeleton design.md "Logical Data Model").

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::CaptureError;

/// Per-cycle capture artifact: the directory plus the open stdout/stderr
/// file handles ready for `Stdio::from(File)` redirection by the runner.
#[derive(Debug)]
pub struct CaptureLayout {
    pub dir: PathBuf,
    pub stdout: File,
    pub stderr: File,
}

/// Create the per-cycle capture directory under `session_root` and open the
/// stdout / stderr file handles inside it.
///
/// Layout: `<session_root>/cycle-<uuid>/{stdout,stderr}`.
///
/// Errors carry the offending path so the `tracing::error!` line can identify
/// the cause from the error alone (Req 7.3).
pub fn create(session_root: &Path, _ticket_id: &str) -> Result<CaptureLayout, CaptureError> {
    let cycle_id = Uuid::new_v4();
    let dir = session_root.join(format!("cycle-{cycle_id}"));

    fs::create_dir_all(&dir).map_err(|source| CaptureError::CreateDir {
        path: dir.clone(),
        source,
    })?;

    let stdout_path = dir.join("stdout");
    let stderr_path = dir.join("stderr");

    let stdout = File::create(&stdout_path).map_err(|source| CaptureError::OpenFile {
        path: stdout_path,
        source,
    })?;
    let stderr = File::create(&stderr_path).map_err(|source| CaptureError::OpenFile {
        path: stderr_path,
        source,
    })?;

    Ok(CaptureLayout {
        dir,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn happy_path_creates_dir_and_opens_files() {
        let root = TempDir::new().unwrap();
        let layout = create(root.path(), "ENG-1").expect("layout should be created");

        assert!(layout.dir.exists(), "cycle dir must exist on disk");
        assert!(layout.dir.is_dir(), "cycle dir must be a directory");

        let dir_name = layout
            .dir
            .file_name()
            .and_then(|n| n.to_str())
            .expect("cycle dir name must be utf-8");
        assert!(
            dir_name.starts_with("cycle-"),
            "expected cycle-<uuid> prefix, got {dir_name}"
        );

        // The skeleton layout is exactly stdout + stderr inside the cycle dir.
        assert!(layout.dir.join("stdout").is_file());
        assert!(layout.dir.join("stderr").is_file());
    }

    #[test]
    fn capture_files_receive_writes() {
        let root = TempDir::new().unwrap();
        let mut layout = create(root.path(), "ENG-1").unwrap();

        layout.stdout.write_all(b"out").unwrap();
        layout.stderr.write_all(b"err").unwrap();
        layout.stdout.sync_all().unwrap();
        layout.stderr.sync_all().unwrap();

        // Read back via the directory path; the runner will do the same for
        // the smoke test's stdout / stderr assertions.
        let stdout_bytes = fs::read(layout.dir.join("stdout")).unwrap();
        let stderr_bytes = fs::read(layout.dir.join("stderr")).unwrap();
        assert_eq!(stdout_bytes, b"out");
        assert_eq!(stderr_bytes, b"err");
    }

    #[test]
    fn unwritable_session_root_returns_create_dir_error() {
        // Use a path that descends through a regular file. `create_dir_all`
        // cannot create a directory under a non-directory parent on either
        // unix or windows, so this is portable without relying on chmod.
        let tmp = TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, b"i am a file").unwrap();
        let bad_root = blocker.join("subdir");

        match create(&bad_root, "ENG-1") {
            Err(CaptureError::CreateDir { path, .. }) => {
                assert!(
                    path.starts_with(&bad_root),
                    "error path {path:?} must point inside the offending session root {bad_root:?}"
                );
            }
            other => panic!("expected CaptureError::CreateDir, got {other:?}"),
        }
    }
}
