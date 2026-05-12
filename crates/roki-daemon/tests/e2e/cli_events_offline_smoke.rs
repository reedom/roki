//! E2E: `roki events --offline --file <p>` JSON Lines reader.
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_roki")
}

#[test]
fn offline_kind_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("daemon.jsonl");
    std::fs::write(
        &file,
        concat!(
            r#"{"seq":1,"ts":"2026-05-11T10:00:00Z","event":"webhook_received","ticket_id":"ENG-1","payload":{}}"#,
            "\n",
            r#"{"seq":2,"ts":"2026-05-11T10:00:01Z","event":"cycle_started","ticket_id":"ENG-1","payload":{}}"#,
            "\n",
        ),
    )
    .unwrap();
    let out = Command::new(bin())
        .args([
            "events",
            "--offline",
            "--file",
            file.to_str().unwrap(),
            "--kind",
            "cycle_started",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("cycle_started"));
    assert!(!s.contains("webhook_received"));
}
