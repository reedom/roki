//! Projection: `EscalationQueue` snapshot → `Vec<ApiEscalation>` (fr:10 §/escalations).
//!
//! `EscalationEntry` carries `ticket_id` / `cycle_id` / `state_id` as
//! `Option`s because daemon-internal escalations (e.g. cold-start orphan
//! reconcile fs error) have no cycle context. The wire schema already
//! `skip_serializing_if = "Option::is_none"` for those fields, so the
//! projection is a straight one-to-one map.
//!
//! `visit_n` stays `None` here: the queue does not track per-visit identity
//! today. A future revision of `EscalationEntry` can populate it.

use std::sync::Arc;

use roki_api_types::ApiEscalation;

use crate::engine::outcome::FailureKind;
use crate::escalation::EscalationQueue;

pub async fn list(queue: &Arc<EscalationQueue>) -> Vec<ApiEscalation> {
    queue
        .snapshot()
        .await
        .into_iter()
        .map(|e| ApiEscalation {
            ticket_id: e.ticket_id,
            cycle_id: e.cycle_id,
            kind: failure_kind_str(e.failure_kind).to_string(),
            state_id: e.state_id,
            visit_n: None,
            timestamp: e.timestamp,
            error_text: crate::api::sanitize::clean_text(&e.error_text),
            marker: "none".into(),
        })
        .collect()
}

/// Mirror `FailureKind::as_str`. Kept local so this projection layer is the
/// single owner of the wire-format spelling and we get a compile error if
/// `FailureKind` grows a new variant.
fn failure_kind_str(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::ProcessCrash => "process_crash",
        FailureKind::Unparseable => "unparseable",
        FailureKind::SchemaDrift => "schema_drift",
        FailureKind::FsPoison => "fs_poison",
        FailureKind::Stall => "stall",
        FailureKind::RecursionBound => "recursion_bound",
        FailureKind::TemplateError => "template_error",
    }
}
