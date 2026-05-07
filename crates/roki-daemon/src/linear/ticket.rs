// Walking-skeleton tasks land in dependency order: this value object (task 3.1)
// precedes admission, rule evaluation, and `linear::webhook::normalize`, which
// will consume `NormalizedTicket` in later tasks. Until those land, the
// constructor and fields are exercised only by the unit tests below, which
// triggers `dead_code` for the leaf API. Allow it module-locally instead of
// leaking the relaxation crate-wide, matching the pattern in `config::workflow`.
#![allow(dead_code)]

//! Internal value object handed to admission and rule evaluation.
//!
//! Carries the minimum field set the skeleton's downstream modules consult
//! (per design.md `linear::ticket`). Construction is restricted to crate
//! internals so only `linear::webhook::normalize` can build instances; the
//! Linear webhook envelope shape never leaks past this boundary.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedTicket {
    pub id: String,
    pub assignee_id: Option<String>,
    pub status: String,
    pub labels: Vec<String>,
    pub title: String,
    pub body: String,
}

impl NormalizedTicket {
    /// Build a `NormalizedTicket`.
    ///
    /// Crate-internal so only `linear::webhook::normalize` constructs instances;
    /// admission and rule evaluation read the public fields without depending
    /// on the Linear webhook envelope.
    pub(crate) fn new(
        id: String,
        assignee_id: Option<String>,
        status: String,
        labels: Vec<String>,
        title: String,
        body: String,
    ) -> Self {
        Self {
            id,
            assignee_id,
            status,
            labels,
            title,
            body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_builds_ticket_with_all_fields() {
        let ticket = NormalizedTicket::new(
            "tid-1".to_string(),
            Some("u1".to_string()),
            "in_progress".to_string(),
            vec!["bug".to_string(), "p0".to_string()],
            "Title".to_string(),
            "Body".to_string(),
        );
        assert_eq!(ticket.id, "tid-1");
        assert_eq!(ticket.assignee_id, Some("u1".to_string()));
        assert_eq!(ticket.status, "in_progress");
        assert_eq!(ticket.labels, vec!["bug".to_string(), "p0".to_string()]);
        assert_eq!(ticket.title, "Title");
        assert_eq!(ticket.body, "Body");
    }

    #[test]
    fn constructor_accepts_unassigned_ticket() {
        let ticket = NormalizedTicket::new(
            "t".to_string(),
            None,
            "todo".to_string(),
            Vec::new(),
            String::new(),
            String::new(),
        );
        assert!(ticket.assignee_id.is_none());
        assert_eq!(ticket.id, "t");
        assert_eq!(ticket.status, "todo");
        assert!(ticket.labels.is_empty());
        assert!(ticket.title.is_empty());
        assert!(ticket.body.is_empty());
    }

    #[test]
    fn ticket_is_clonable_and_comparable() {
        let ticket = NormalizedTicket::new(
            "tid-2".to_string(),
            Some("u2".to_string()),
            "review".to_string(),
            vec!["feature".to_string()],
            "T".to_string(),
            "B".to_string(),
        );
        let clone = ticket.clone();
        assert_eq!(ticket, clone);
    }

    #[test]
    fn constructor_accepts_title_and_body() {
        let ticket = NormalizedTicket::new(
            "tid-3".to_string(),
            Some("u1".to_string()),
            "review".to_string(),
            vec!["needs-impl".to_string()],
            "Implement widget".to_string(),
            "Body paragraph one.\n\nBody paragraph two.".to_string(),
        );
        assert_eq!(ticket.title, "Implement widget");
        assert!(ticket.body.contains("paragraph two"));
    }
}
