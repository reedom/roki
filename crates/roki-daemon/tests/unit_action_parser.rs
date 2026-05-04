//! Integration smoke tests for the orchestrator response parser.
//!
//! In-file unit tests in `engine::orchestrator_session::action_parser::tests`
//! cover schema validation and individual drift transitions; this file drives
//! the public `ActionParser::parse_turn` surface end-to-end with realistic
//! multi-line turn fixtures (advisory progress + final action) and the
//! consecutive-drift -> TerminalDrift transition.

use roki_daemon::engine::orchestrator_session::action_parser::{
    ActionKind, ActionParser, Outcome, ParseTurnOutcome, PhaseName,
};

fn line(s: &str) -> String {
    s.to_owned()
}

fn run_phase_json(phase: &str, reason: &str) -> String {
    format!(r#"{{"action":"run_phase","phase":"{phase}","reason":"{reason}"}}"#)
}

#[test]
fn advisory_progress_then_final_run_phase_yields_only_the_final_action() {
    // Realistic turn: advisory `system/progress` + extended-thinking +
    // human-readable prose, terminated by the canonical action JSON.
    let mut parser = ActionParser::new();
    let lines = vec![
        line(r#"{"type":"system","subtype":"progress","note":"warming up"}"#),
        line(r#"{"type":"thinking","content":"weighing options"}"#),
        line("plain prose advisory line — orchestrator is thinking out loud"),
        line(&run_phase_json("implement", "advance")),
    ];
    match parser.parse_turn(&lines) {
        ParseTurnOutcome::Action(action) => {
            assert_eq!(action.action, ActionKind::RunPhase);
            assert_eq!(action.phase, Some(PhaseName::Implement));
            assert_eq!(action.reason.as_str(), "advance");
        }
        other => panic!("expected Action, got {other:?}"),
    }
    assert_eq!(parser.drift_count(), 0);
}

#[test]
fn stop_with_outcome_is_emitted_as_typed_action_after_advisory_lines() {
    let mut parser = ActionParser::new();
    let lines = vec![
        line(r#"{"type":"system","subtype":"progress","note":"finalizing"}"#),
        line(r#"{"action":"stop","outcome":"success","reason":"done"}"#),
    ];
    match parser.parse_turn(&lines) {
        ParseTurnOutcome::Action(action) => {
            assert_eq!(action.action, ActionKind::Stop);
            assert_eq!(action.outcome, Some(Outcome::Success));
        }
        other => panic!("expected Stop Action, got {other:?}"),
    }
}

#[test]
fn two_consecutive_drift_turns_produce_terminal_drift() {
    let mut parser = ActionParser::new();
    // First drift: no parseable JSON object on this turn.
    match parser.parse_turn(&[line("oops, no JSON here")]) {
        ParseTurnOutcome::Drift { reprompt_payload } => {
            assert!(reprompt_payload.contains("OrchestratorAction"));
        }
        other => panic!("expected Drift, got {other:?}"),
    }
    assert_eq!(parser.drift_count(), 1);

    // Second drift: still no parseable action -> terminal drift surfaces the
    // last raw stdout for operator diagnosis.
    match parser.parse_turn(&[line("still nothing"), line("final raw line")]) {
        ParseTurnOutcome::TerminalDrift { last_raw_stdout } => {
            assert_eq!(last_raw_stdout, "final raw line");
        }
        other => panic!("expected TerminalDrift, got {other:?}"),
    }
}
