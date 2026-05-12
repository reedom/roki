//! E2E: drive `roki log` against a synthetic visit directory.
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_roki")
}

#[test]
fn stream_stdout_reads_visit_capture() {
    let tmp = tempfile::tempdir().unwrap();
    let cycle = "00000000-0000-0000-0000-00000000000a";
    let vd = tmp
        .path()
        .join("ENG-9")
        .join(format!("cycle-{cycle}"))
        .join("visit-001");
    std::fs::create_dir_all(&vd).unwrap();
    std::fs::write(vd.join("impl.stdout"), b"hello\nworld\n").unwrap();
    std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();

    let out = Command::new(bin())
        .env("ROKI_CONFIG_SESSION_ROOT", tmp.path())
        .env("ROKI_TICKET_ID", "ENG-9")
        .env("ROKI_CYCLE_ID", cycle)
        .args(["log", "--state", "impl", "--stream", "stdout"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"hello\nworld\n");
}

#[test]
fn list_visits_emits_per_visit_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    let cycle = "00000000-0000-0000-0000-00000000000b";
    let cycle_dir = tmp.path().join("ENG-9").join(format!("cycle-{cycle}"));
    for n in 1..=3u32 {
        let vd = cycle_dir.join(format!("visit-{n:03}"));
        std::fs::create_dir_all(&vd).unwrap();
        std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();
    }
    let out = Command::new(bin())
        .env("ROKI_CONFIG_SESSION_ROOT", tmp.path())
        .env("ROKI_TICKET_ID", "ENG-9")
        .env("ROKI_CYCLE_ID", cycle)
        .args(["log", "--list-visits"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8(out.stdout).unwrap();
    assert_eq!(s.lines().count(), 3);
}
