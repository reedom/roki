//! Shared helpers for the section-12 integration test files.
//!
//! Centralizes the `fake_claude` example-binary build + path resolution and
//! the small ceremony for writing the per-CWD `.fake_claude_mode` file. Each
//! integration test file imports the helpers it needs.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Build (once per process) the `fake_claude` example binary and return its
/// path under `target/debug/examples`.
pub fn fake_claude_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let status = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "--quiet",
                "--example",
                "fake_claude",
                "-p",
                "roki-daemon",
            ])
            .status()
            .expect("invoke cargo build --example fake_claude");
        assert!(status.success(), "fake_claude example build failed");
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .ancestors()
            .nth(2)
            .expect("workspace root is two levels above the daemon manifest")
            .to_path_buf();
        workspace_root
            .join("target")
            .join("debug")
            .join("examples")
            .join("fake_claude")
    })
    .as_path()
}

/// Write the `.fake_claude_mode` selector file the harness reads from CWD.
pub fn write_mode(dir: &Path, mode: &str) {
    std::fs::write(dir.join(".fake_claude_mode"), mode).expect("write fake_claude_mode");
}
