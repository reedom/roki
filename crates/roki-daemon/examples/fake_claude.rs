//! Test harness binary stubbing the `claude` CLI surface used by the
//! orchestrator-session adapter (and, in the future, the phase subprocess
//! adapter).
//!
//! Mode selection is driven by a `.fake_claude_mode` file in CWD. The
//! adapter sets CWD to the per-session tempdir so each test can write a
//! distinct mode without colliding with concurrent tests.
//!
//! The harness deliberately ignores `--settings` / `--input-format` /
//! `--output-format` flags; it only models the behaviors the adapter tests
//! need. Stdout is line-buffered via explicit flushes so the parser
//! observes turn boundaries on the fast path.

use std::io::{BufRead, BufReader, Read, Write};

fn main() {
    let mode = std::fs::read_to_string(".fake_claude_mode")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();

    // The adapter writes the system prompt as the first stdin line — read
    // it so the test can assert delivery via the marker the harness echoes
    // back into its first action's `reason` field.
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut first_line = String::new();
    let _ = reader.read_line(&mut first_line);
    let prompt_marker = extract_system_prompt_marker(&first_line);

    match mode.as_str() {
        "single_action" => {
            emit_run_phase("implement", &prompt_marker);
            // Drain stdin so EOF on the parent's drop unblocks `read_line`.
            drain_stdin(&mut reader);
        }
        "echo_phase_complete" => {
            emit_run_phase("implement", &prompt_marker);
            // Wait for one event line on stdin, then emit a follow-up.
            let mut event_line = String::new();
            let _ = reader.read_line(&mut event_line);
            emit_run_phase("open_pr", "after_phase_complete");
            drain_stdin(&mut reader);
        }
        "wait_for_stdin_close" => {
            // No stdout; just wait for stdin to close (parent shutdown).
            drain_stdin(&mut reader);
        }
        "stderr_then_action" => {
            eprintln!("ROKI-STDERR-MARKER: warning from fake_claude");
            let _ = std::io::stderr().flush();
            emit_run_phase("implement", &prompt_marker);
            drain_stdin(&mut reader);
        }
        "phase_success" => {
            // Drain whatever the adapter pushed into stdin (it may write
            // the rendered template body for template-form invocations).
            drain_stdin(&mut reader);
            emit_stream_result("success");
        }
        "phase_success_capture_stdin" => {
            // Like `phase_success`, but captures the full stdin (including
            // the system-prompt envelope first line we already read above)
            // verbatim into `<cwd>/.fake_claude_stdin_capture` so tests can
            // assert the rendered template body — including
            // `additional_context` — actually reached the subprocess.
            // Gated on this dedicated mode to keep the existing
            // `phase_success` callers unaffected.
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink);
            let mut combined = first_line.clone().into_bytes();
            combined.extend_from_slice(&sink);
            let _ = std::fs::write(".fake_claude_stdin_capture", &combined);
            emit_stream_result("success");
        }
        "phase_error_max_turns" => {
            drain_stdin(&mut reader);
            emit_stream_result("error_max_turns");
        }
        "phase_error_during_execution" => {
            drain_stdin(&mut reader);
            emit_stream_result("error_during_execution");
        }
        "phase_unknown_subtype" => {
            drain_stdin(&mut reader);
            emit_stream_result("error_future_unknown_signal");
        }
        "phase_nonzero_no_result" => {
            drain_stdin(&mut reader);
            // Exit non-zero without a terminal `result` event. The adapter
            // must classify this as `phase_nonclean(NonZero)`.
            std::process::exit(7);
        }
        "phase_stall" => {
            // Emit nothing on stdout; let the adapter's per-phase stall
            // detector SIGTERM the child.
            drain_stdin(&mut reader);
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        "phase_stderr_then_success" => {
            eprintln!("ROKI-PHASE-STDERR-MARKER: phase warning");
            let _ = std::io::stderr().flush();
            drain_stdin(&mut reader);
            emit_stream_result("success");
        }
        _ => {
            eprintln!("fake_claude: unknown mode `{mode}`");
            std::process::exit(2);
        }
    }
}

fn emit_run_phase(phase: &str, reason_extra: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let payload = serde_json::json!({
        "action": "run_phase",
        "phase": phase,
        "reason": format!("nominate {phase} {reason_extra}"),
    });
    let _ = writeln!(out, "{payload}");
    let _ = out.flush();
}

/// Emit a stream-json terminal `result` event with the given subtype. Used
/// by the phase-subprocess adapter integration tests.
fn emit_stream_result(subtype: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let payload = serde_json::json!({
        "type": "result",
        "subtype": subtype,
        "total_cost": 0.0,
    });
    let _ = writeln!(out, "{payload}");
    let _ = out.flush();
}

/// Pull the marker text out of the system-prompt envelope so it round-trips
/// into the harness's first action and the test can assert delivery.
fn extract_system_prompt_marker(first_line: &str) -> String {
    let trimmed = first_line.trim();
    if trimmed.is_empty() {
        return "<no-prompt>".to_owned();
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return "<unparseable-prompt>".to_owned(),
    };
    value
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("<missing-system-prompt-field>")
        .to_owned()
}

fn drain_stdin<R: Read>(reader: &mut R) {
    let mut sink = Vec::new();
    let _ = reader.read_to_end(&mut sink);
}
