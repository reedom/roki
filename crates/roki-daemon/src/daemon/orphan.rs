#![allow(dead_code)]

//! Session-tempdir orphan reconcile (fr:07 §Cold start step 5).
//!
//! Walks `<session_root>/`, deletes every directory whose name is not in
//! `keep_ids`, and emits one `session_tempdir_deleted { reason: "orphan" }`
//! per deletion. The reserved `_daemon/` directory is skipped.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::events::{Event, EventWriter, SessionTempdirDeleteReason, now_rfc3339};

pub struct OrphanScan<'a> {
    pub session_root: &'a Path,
    pub keep_ids: &'a HashSet<String>,
}

#[derive(Debug, Default)]
pub struct OrphanReport {
    pub deleted: Vec<String>,
    pub fs_errors: Vec<(String, std::io::Error)>,
}

pub async fn reconcile(scan: OrphanScan<'_>, writer: Arc<Mutex<EventWriter>>) -> OrphanReport {
    let mut report = OrphanReport::default();

    let read_dir = match tokio::fs::read_dir(scan.session_root).await {
        Ok(d) => d,
        Err(_) => return report,
    };
    tokio::pin!(read_dir);

    let mut entries = read_dir;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let file_name = entry.file_name();
        let name = match file_name.to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        if name == "_daemon" || name.starts_with("_daemon.") {
            continue;
        }

        let ft = match entry.file_type().await {
            Ok(ft) => ft,
            Err(e) => {
                report.fs_errors.push((name, e));
                continue;
            }
        };
        if !ft.is_dir() {
            continue;
        }

        if scan.keep_ids.contains(&name) {
            continue;
        }

        let path = entry.path();
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {
                {
                    let mut w = writer.lock().await;
                    let _ = w.emit(&Event::SessionTempdirDeleted {
                        ts: now_rfc3339(),
                        ticket_id: name.clone(),
                        path: path.display().to_string(),
                        reason: SessionTempdirDeleteReason::Orphan,
                    });
                }
                report.deleted.push(name);
            }
            Err(e) => {
                report.fs_errors.push((name, e));
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn writer_in(root: &Path) -> EventWriter {
        EventWriter::open(root, "_daemon").expect("open writer")
    }

    fn keep(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn arc_writer(root: &Path) -> Arc<Mutex<EventWriter>> {
        Arc::new(Mutex::new(writer_in(root)))
    }

    #[tokio::test]
    async fn empty_session_root_is_noop() {
        let tmp = TempDir::new().unwrap();
        let w = arc_writer(tmp.path());
        let report = reconcile(
            OrphanScan {
                session_root: tmp.path(),
                keep_ids: &keep(&[]),
            },
            w,
        )
        .await;
        assert!(report.deleted.is_empty());
        assert!(report.fs_errors.is_empty());
    }

    #[tokio::test]
    async fn orphans_deleted_kept_preserved() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("ticket-keep")).unwrap();
        fs::create_dir_all(tmp.path().join("ticket-orphan")).unwrap();
        let w = arc_writer(tmp.path());

        let report = reconcile(
            OrphanScan {
                session_root: tmp.path(),
                keep_ids: &keep(&["ticket-keep"]),
            },
            w,
        )
        .await;

        assert_eq!(report.deleted, vec!["ticket-orphan".to_string()]);
        assert!(tmp.path().join("ticket-keep").is_dir());
        assert!(!tmp.path().join("ticket-orphan").exists());
    }

    #[tokio::test]
    async fn daemon_directory_is_skipped() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("_daemon")).unwrap();
        let w = arc_writer(tmp.path());

        let report = reconcile(
            OrphanScan {
                session_root: tmp.path(),
                keep_ids: &keep(&[]),
            },
            w,
        )
        .await;
        assert!(report.deleted.is_empty());
        assert!(tmp.path().join("_daemon").exists());
    }

    #[tokio::test]
    async fn non_directory_entries_are_skipped() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("loose.jsonl"), b"").unwrap();
        let w = arc_writer(tmp.path());

        let report = reconcile(
            OrphanScan {
                session_root: tmp.path(),
                keep_ids: &keep(&[]),
            },
            w,
        )
        .await;
        assert!(report.deleted.is_empty());
        assert!(tmp.path().join("loose.jsonl").exists());
    }
}
