//! Engine type vocabulary for slice 8.
//!
//! Slice 8 dropped the legacy pre/run/post `PhaseKind`, the session/command
//! `PhaseShape`, the stdout-scan `Pre/PostDirective` types, and the
//! `IterExhausted` failure kind. The state machine now expresses control
//! flow; this module keeps only what the failure-routing layer needs.

#![allow(dead_code)]

/// Which list a cycle was dispatched from. Surfaced as `cycle.kind`
/// / `ROKI_CYCLE_KIND` per fr:01 §Cycle kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleKind {
    Rule,
    Cleanup,
    Failure,
}

impl CycleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CycleKind::Rule => "rule",
            CycleKind::Cleanup => "cleanup",
            CycleKind::Failure => "failure",
        }
    }
}

/// Daemon-detected failure kinds. Routed to `on_failure:` rules and
/// surfaced through the `failure.*` Liquid namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Sentinel file present but JSON parse failed or `directive` field
    /// missing.
    Unparseable,
    /// Sentinel `directive` value not in `state.directives` ∪ built-in
    /// defaults.
    SchemaDrift,
    /// Subprocess killed by signal without sentinel write.
    ProcessCrash,
    /// Liquid render of cli line, body, or `if:` condition failed before
    /// launch.
    TemplateError,
    /// State visited more than `state.max_visits` times.
    RecursionBound,
    /// Stdout silent for the configured stall window; daemon SIGTERMed
    /// the subprocess.
    Stall,
    /// Filesystem error creating session_tempdir / sentinel-dir / worktree
    /// before subprocess launch.
    FsPoison,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::Unparseable => "unparseable",
            FailureKind::SchemaDrift => "schema_drift",
            FailureKind::ProcessCrash => "process_crash",
            FailureKind::TemplateError => "template_error",
            FailureKind::RecursionBound => "recursion_bound",
            FailureKind::Stall => "stall",
            FailureKind::FsPoison => "fs_poison",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_kind_str_round_trip() {
        assert_eq!(CycleKind::Rule.as_str(), "rule");
        assert_eq!(CycleKind::Cleanup.as_str(), "cleanup");
        assert_eq!(CycleKind::Failure.as_str(), "failure");
    }

    #[test]
    fn failure_kind_str_round_trip() {
        assert_eq!(FailureKind::Stall.as_str(), "stall");
        assert_eq!(FailureKind::FsPoison.as_str(), "fs_poison");
        assert_eq!(FailureKind::RecursionBound.as_str(), "recursion_bound");
    }
}
