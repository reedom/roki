//! Cycle trigger enum surfaced through `cycle.trigger` Liquid + the
//! `ROKI_CYCLE_TRIGGER` env var. Slice 8 dropped the legacy `PhaseContext`
//! aggregator; per-cycle Liquid globals are now assembled by
//! `daemon::real_runner::build_cycle_context` and consumed via
//! `engine::state_runtime::CycleContext`.

#![allow(dead_code)]

/// Identifies why a cycle was started. Slices 1-5 hardcoded `"runtime"`;
/// slice 6 introduced the cold-start path which uses `ColdStart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleTrigger {
    Runtime,
    ColdStart,
}

impl CycleTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            CycleTrigger::Runtime => "runtime",
            CycleTrigger::ColdStart => "cold_start",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_trigger_str_round_trip() {
        assert_eq!(CycleTrigger::Runtime.as_str(), "runtime");
        assert_eq!(CycleTrigger::ColdStart.as_str(), "cold_start");
    }
}
