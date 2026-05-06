// Walking-skeleton tasks land in dependency order: this filter (task 4.1)
// precedes the runtime wiring that calls `accept` per cycle. Until that
// wiring lands, the function is exercised only by the unit tests below,
// which triggers `dead_code` for the leaf API. Allow it module-locally
// instead of leaking the relaxation crate-wide, matching the pattern in
// `config::workflow` and `linear::ticket`.
#![allow(dead_code)]

//! Admission filter for the walking-skeleton daemon.
//!
//! Pure function over a `NormalizedTicket`, the loaded `WorkflowConfig`,
//! and the runtime-resolved viewer id (`MeId`). Two checks:
//!
//! - The ticket's `assignee_id` must equal `WorkflowConfig::admission::assignee`.
//!   When the configured value is the literal `"me"`, the comparison uses the
//!   resolved viewer id (Req 4.1, 4.2).
//! - The target repository is resolved as the first `[[admission.repos]]`
//!   entry's `ghq` only; absence of any entry is fatal (Req 4.3, 4.4).

use crate::config::workflow::WorkflowConfig;
use crate::error::AdmissionError;
use crate::linear::client::MeId;
use crate::linear::ticket::NormalizedTicket;

/// Outcome of a successful admission. Carries the ticket forward together
/// with the resolved repository identifier the runner will operate on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedTicket {
    pub ticket: NormalizedTicket,
    pub ghq: String,
}

/// Run the admission gate.
///
/// Returns `AdmittedTicket` when the ticket is accepted; otherwise an
/// `AdmissionError` describing the rejection cause. Errors are pure values:
/// the runtime decides log level and exit code (Req 4.5).
pub fn accept(
    ticket: &NormalizedTicket,
    workflow: &WorkflowConfig,
    me_id: &MeId,
) -> Result<AdmittedTicket, AdmissionError> {
    // `me` is resolved to the viewer id at runtime; any other value is
    // compared verbatim (Req 4.2).
    let expected = if workflow.admission.assignee == "me" {
        me_id.0.clone()
    } else {
        workflow.admission.assignee.clone()
    };

    let got = ticket.assignee_id.clone();
    if got.as_deref() != Some(expected.as_str()) {
        return Err(AdmissionError::AssigneeMismatch {
            ticket_id: ticket.id.clone(),
            expected,
            got,
        });
    }

    // First `[[admission.repos]]` entry only; per-entry `when.*` matchers
    // are not consulted at the skeleton layer (Req 4.3).
    let repo = workflow.repo.as_ref().ok_or(AdmissionError::NoRepos)?;

    Ok(AdmittedTicket {
        ticket: ticket.clone(),
        ghq: repo.ghq.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::workflow::{AdmissionRepo, AdmissionSection, WorkflowConfig};

    fn ticket(assignee: Option<&str>) -> NormalizedTicket {
        NormalizedTicket::new(
            "tid-1".to_string(),
            assignee.map(String::from),
            "in_progress".to_string(),
            vec!["bug".to_string()],
        )
    }

    fn workflow_with(assignee: &str, repo: Option<&str>) -> WorkflowConfig {
        WorkflowConfig {
            admission: AdmissionSection {
                assignee: assignee.to_string(),
            },
            repo: repo.map(|g| AdmissionRepo {
                ghq: g.to_string(),
            }),
            rules: Vec::new(),
        }
    }

    #[test]
    fn accepts_when_assignee_id_matches_literal() {
        let t = ticket(Some("u1"));
        let wf = workflow_with("u1", Some("github.com/owner/repo"));
        let me = MeId("does-not-matter".into());

        let admitted = accept(&t, &wf, &me).expect("literal id match should accept");
        assert_eq!(admitted.ghq, "github.com/owner/repo");
        assert_eq!(admitted.ticket.id, "tid-1");
    }

    #[test]
    fn accepts_when_me_resolves_to_viewer_id() {
        // "me" in config + viewer id matching the ticket's assignee_id is
        // the canonical Req 4.2 path.
        let t = ticket(Some("u-viewer"));
        let wf = workflow_with("me", Some("github.com/owner/repo"));
        let me = MeId("u-viewer".into());

        let admitted = accept(&t, &wf, &me).expect("me-resolved id match should accept");
        assert_eq!(admitted.ghq, "github.com/owner/repo");
    }

    #[test]
    fn rejects_when_assignee_mismatches() {
        let t = ticket(Some("u-other"));
        let wf = workflow_with("u1", Some("github.com/owner/repo"));
        let me = MeId("u-other".into());

        match accept(&t, &wf, &me) {
            Err(AdmissionError::AssigneeMismatch {
                ticket_id,
                expected,
                got,
            }) => {
                assert_eq!(ticket_id, "tid-1");
                assert_eq!(expected, "u1");
                assert_eq!(got.as_deref(), Some("u-other"));
            }
            other => panic!("expected AssigneeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unassigned_ticket_against_literal_assignee() {
        let t = ticket(None);
        let wf = workflow_with("u1", Some("github.com/owner/repo"));
        let me = MeId("u1".into());

        match accept(&t, &wf, &me) {
            Err(AdmissionError::AssigneeMismatch { got, expected, .. }) => {
                assert!(got.is_none());
                assert_eq!(expected, "u1");
            }
            other => panic!("expected AssigneeMismatch for unassigned, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unassigned_ticket_against_me() {
        // "me" expands to the viewer id; an unassigned ticket can never
        // satisfy it.
        let t = ticket(None);
        let wf = workflow_with("me", Some("github.com/owner/repo"));
        let me = MeId("u-viewer".into());

        match accept(&t, &wf, &me) {
            Err(AdmissionError::AssigneeMismatch { got, expected, .. }) => {
                assert!(got.is_none());
                assert_eq!(expected, "u-viewer");
            }
            other => panic!("expected AssigneeMismatch for unassigned, got {other:?}"),
        }
    }

    #[test]
    fn returns_no_repos_when_workflow_has_none() {
        // Assignee match path runs first, so configure a matching ticket
        // and confirm NoRepos is the rejection cause when `repo` is None
        // (Req 4.4).
        let t = ticket(Some("u1"));
        let wf = workflow_with("u1", None);
        let me = MeId("u1".into());

        match accept(&t, &wf, &me) {
            Err(AdmissionError::NoRepos) => {}
            other => panic!("expected NoRepos, got {other:?}"),
        }
    }
}
