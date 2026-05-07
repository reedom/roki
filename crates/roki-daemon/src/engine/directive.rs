#![allow(dead_code)]

//! Scan stdout for the last top-level JSON object and validate the contained
//! `directive` value against the phase's legal set.

use serde_json::Value;

use super::outcome::{FailureKind, PostDirective, PreDirective};

/// Walk top-level values in `stdout` and return the **last** parsed
/// `Value::Object`. Bytes between objects are ignored. Items that fail to
/// parse are dropped. Non-object top-level values (string, number, array,
/// null) are ignored. Returns `None` if no top-level object parsed
/// successfully.
pub fn scan_last_json_object(stdout: &[u8]) -> Option<Value> {
    use serde_json::Deserializer;

    let mut last: Option<Value> = None;
    let mut offset = 0usize;

    // Re-create the stream after each parse error so non-JSON bytes (advisory
    // text, log lines, partial trailing objects) between JSON values don't halt
    // the scan. `byte_offset()` on the stream tells us where the last attempt
    // stopped; we advance one byte past that and retry.
    while offset < stdout.len() {
        let slice = &stdout[offset..];
        let mut stream = Deserializer::from_slice(slice).into_iter::<Value>();
        let mut got_error = false;
        for item in stream.by_ref() {
            match item {
                Ok(value @ Value::Object(_)) => last = Some(value),
                Ok(_) => {}
                Err(_) => {
                    // Advance past the point where the parse failed and retry.
                    let err_offset = stream.byte_offset();
                    offset += err_offset + 1;
                    got_error = true;
                    break;
                }
            }
        }
        if !got_error {
            // Stream exhausted cleanly (or was empty); we're done.
            break;
        }
    }
    last
}

/// Pre-phase result: the parsed directive plus the full payload, or a
/// `FailureKind` describing the parse / validation failure.
pub enum PreParse {
    Ok {
        directive: PreDirective,
        payload: Value,
    },
    Failed(FailureKind),
}

/// Parse a Pre stdout slice into `PreParse`.
///
/// `exit_status_success`: whether the subprocess returned exit code 0. Used
/// to disambiguate Unparseable (zero exit + no JSON) from ProcessCrash
/// (non-zero exit + no JSON).
pub fn parse_pre_directive(stdout: &[u8], exit_status_success: bool) -> PreParse {
    let Some(value) = scan_last_json_object(stdout) else {
        return if exit_status_success {
            PreParse::Failed(FailureKind::Unparseable)
        } else {
            PreParse::Failed(FailureKind::ProcessCrash)
        };
    };
    let Some(directive_str) = value.get("directive").and_then(Value::as_str) else {
        return PreParse::Failed(FailureKind::Unparseable);
    };
    match PreDirective::try_from_str(directive_str) {
        Some(directive) => PreParse::Ok {
            directive,
            payload: value,
        },
        None => PreParse::Failed(FailureKind::SchemaDrift),
    }
}

/// Post-phase analogue of `parse_pre_directive`.
pub enum PostParse {
    Ok {
        directive: PostDirective,
        payload: Value,
    },
    Failed(FailureKind),
}

pub fn parse_post_directive(stdout: &[u8], exit_status_success: bool) -> PostParse {
    let Some(value) = scan_last_json_object(stdout) else {
        return if exit_status_success {
            PostParse::Failed(FailureKind::Unparseable)
        } else {
            PostParse::Failed(FailureKind::ProcessCrash)
        };
    };
    let Some(directive_str) = value.get("directive").and_then(Value::as_str) else {
        return PostParse::Failed(FailureKind::Unparseable);
    };
    match PostDirective::try_from_str(directive_str) {
        Some(directive) => PostParse::Ok {
            directive,
            payload: value,
        },
        None => PostParse::Failed(FailureKind::SchemaDrift),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_returns_last_object_when_multiple_present() {
        let stdout = br#"
        {"directive":"run","note":"first"}
        advisory text
        {"directive":"end","note":"second"}
        "#;
        let v = scan_last_json_object(stdout).expect("must find last object");
        assert_eq!(v["note"], "second");
    }

    #[test]
    fn scan_ignores_non_object_top_level_values() {
        let stdout = br#"42 "string-value" {"directive":"run"} [1,2,3]"#;
        let v = scan_last_json_object(stdout).expect("must find the object");
        assert_eq!(v["directive"], "run");
    }

    #[test]
    fn scan_returns_none_when_no_object_present() {
        let stdout = b"plain text with no JSON whatsoever";
        assert!(scan_last_json_object(stdout).is_none());
    }

    #[test]
    fn scan_tolerates_trailing_partial_object() {
        let stdout = br#"{"directive":"run"} {"directive":"en"#; // truncated
        let v = scan_last_json_object(stdout).expect("must keep the parsed first object");
        assert_eq!(v["directive"], "run");
    }

    #[test]
    fn parse_pre_run_succeeds_with_payload() {
        let bytes = br#"{"directive":"run","extra":1}"#;
        match parse_pre_directive(bytes, true) {
            PreParse::Ok { directive, payload } => {
                assert_eq!(directive, PreDirective::Run);
                assert_eq!(payload["extra"], 1);
            }
            PreParse::Failed(k) => panic!("unexpected failure {k:?}"),
        }
    }

    #[test]
    fn parse_pre_end_succeeds() {
        let bytes = br#"{"directive":"end"}"#;
        match parse_pre_directive(bytes, true) {
            PreParse::Ok { directive, .. } => assert_eq!(directive, PreDirective::End),
            PreParse::Failed(k) => panic!("unexpected failure {k:?}"),
        }
    }

    #[test]
    fn parse_pre_rejects_pre_directive() {
        // `pre` is illegal as a Pre directive (legal set is run/end).
        let bytes = br#"{"directive":"pre"}"#;
        match parse_pre_directive(bytes, true) {
            PreParse::Failed(FailureKind::SchemaDrift) => {}
            other => panic!("expected SchemaDrift, got {other:?}"),
        }
    }

    #[test]
    fn parse_pre_no_json_zero_exit_is_unparseable() {
        match parse_pre_directive(b"plain text", true) {
            PreParse::Failed(FailureKind::Unparseable) => {}
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    fn parse_pre_no_json_nonzero_exit_is_process_crash() {
        match parse_pre_directive(b"plain text", false) {
            PreParse::Failed(FailureKind::ProcessCrash) => {}
            other => panic!("expected ProcessCrash, got {other:?}"),
        }
    }

    #[test]
    fn parse_pre_object_missing_directive_field_is_unparseable() {
        let bytes = br#"{"foo":"bar"}"#;
        match parse_pre_directive(bytes, true) {
            PreParse::Failed(FailureKind::Unparseable) => {}
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[test]
    fn parse_post_pre_run_end_all_legal() {
        let cases = [("pre", PostDirective::Pre), ("run", PostDirective::Run), ("end", PostDirective::End)];
        for (s, expected) in cases {
            let body = format!(r#"{{"directive":"{}"}}"#, s);
            match parse_post_directive(body.as_bytes(), true) {
                PostParse::Ok { directive, .. } => assert_eq!(directive, expected),
                PostParse::Failed(k) => panic!("{s} should be legal, got {k:?}"),
            }
        }
    }

    #[test]
    fn parse_post_rejects_unknown_value() {
        let bytes = br#"{"directive":"halt"}"#;
        match parse_post_directive(bytes, true) {
            PostParse::Failed(FailureKind::SchemaDrift) => {}
            other => panic!("expected SchemaDrift, got {other:?}"),
        }
    }

    // Implement Debug manually for use in panic! formatting in tests.
    impl std::fmt::Debug for PreParse {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                PreParse::Ok { directive, .. } => write!(f, "Ok({directive:?})"),
                PreParse::Failed(k) => write!(f, "Failed({k:?})"),
            }
        }
    }

    impl std::fmt::Debug for PostParse {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                PostParse::Ok { directive, .. } => write!(f, "Ok({directive:?})"),
                PostParse::Failed(k) => write!(f, "Failed({k:?})"),
            }
        }
    }
}
