//! E2E: `roki repo` against overridden ghq base + worktree root.
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_roki")
}

#[test]
fn repo_returns_ghq_base_when_no_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let wt_root = tmp.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();
    let ghq_base = tmp.path().join("ghq-base");
    std::fs::create_dir_all(&ghq_base).unwrap();
    let out = Command::new(bin())
        .env("ROKI_GHQ_BASE_OVERRIDE", &ghq_base)
        .env("ROKI_WT_ROOT_OVERRIDE", &wt_root)
        .args(["repo", "github.com/x/y", "--ticket", "OPS-11"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8(out.stdout).unwrap();
    assert_eq!(s.trim(), ghq_base.to_string_lossy());
}

#[test]
fn repo_worktree_flag_errors_when_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let wt_root = tmp.path().join("wts");
    std::fs::create_dir_all(&wt_root).unwrap();
    let ghq_base = tmp.path().join("ghq-base");
    std::fs::create_dir_all(&ghq_base).unwrap();
    let out = Command::new(bin())
        .env("ROKI_GHQ_BASE_OVERRIDE", &ghq_base)
        .env("ROKI_WT_ROOT_OVERRIDE", &wt_root)
        .args(["repo", "github.com/x/y", "--ticket", "OPS-11", "--worktree"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let s = String::from_utf8(out.stderr).unwrap();
    assert!(s.contains("worktree not yet materialized"));
}
