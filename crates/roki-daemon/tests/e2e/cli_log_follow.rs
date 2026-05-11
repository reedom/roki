//! E2E: `roki log --follow` picks up late writes.
use std::process::{Command, Stdio};
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_roki")
}

#[test]
fn follow_streams_late_appends_then_exits_on_exit_code() {
    let tmp = tempfile::tempdir().unwrap();
    let cycle = "00000000-0000-0000-0000-00000000000c";
    let vd = tmp
        .path()
        .join("ENG-9")
        .join(format!("cycle-{cycle}"))
        .join("visit-001");
    std::fs::create_dir_all(&vd).unwrap();
    let stdout = vd.join("impl.stdout");
    std::fs::write(&stdout, b"first\n").unwrap();

    let mut child = Command::new(bin())
        .env("ROKI_CONFIG_SESSION_ROOT", tmp.path())
        .env("ROKI_TICKET_ID", "ENG-9")
        .env("ROKI_CYCLE_ID", cycle)
        .args([
            "log",
            "--state",
            "impl",
            "--stream",
            "stdout",
            "--follow",
            "--follow-poll-ms",
            "50",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(100));
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&stdout)
        .unwrap();
    f.write_all(b"second\n").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();

    let waited = child.wait_with_output().unwrap();
    assert!(waited.status.success());
    let s = String::from_utf8(waited.stdout).unwrap();
    assert!(s.contains("first"));
    assert!(s.contains("second"));
}
