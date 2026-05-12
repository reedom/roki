//! E2E: `roki log --follow` picks up late writes.
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_roki")
}

/// Poll the child's stdout (line-buffered via BufReader) until `marker`
/// appears, with a deadline. Returns the collected lines so the caller
/// can assert on the prefix as well as the marker line.
fn read_until(
    reader: &mut BufReader<std::process::ChildStdout>,
    marker: &str,
    timeout: Duration,
) -> Vec<String> {
    let start = Instant::now();
    let mut out: Vec<String> = Vec::new();
    while start.elapsed() < timeout {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return out, // EOF
            Ok(_) => {
                let saw_marker = line.contains(marker);
                out.push(line);
                if saw_marker {
                    return out;
                }
            }
            Err(_) => return out,
        }
    }
    out
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

    let child_stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(child_stdout);

    // Drain `first\n` before appending so the late-write ordering is
    // observable rather than timing-coupled.
    let initial = read_until(&mut reader, "first", Duration::from_secs(2));
    assert!(
        initial.iter().any(|l| l.contains("first")),
        "missing initial: {initial:?}"
    );

    // Append `second\n`, then wait for the follower to emit it before
    // writing the exit-code sentinel. This is the de-flake: the test no
    // longer races a fixed sleep against the polling cadence.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&stdout)
        .unwrap();
    f.write_all(b"second\n").unwrap();
    drop(f);

    let after = read_until(&mut reader, "second", Duration::from_secs(2));
    assert!(
        after.iter().any(|l| l.contains("second")),
        "missing late append: {after:?}"
    );

    // Now signal end-of-visit. Any stragglers between this and the next
    // poll get drained in `follow_loop`'s post-sentinel drain branch.
    std::fs::write(vd.join("impl.exit_code"), "0\n").unwrap();

    let waited = child.wait_with_output().unwrap();
    assert!(waited.status.success(), "follower exited non-zero");
}
