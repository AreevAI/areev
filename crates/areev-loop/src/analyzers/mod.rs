//! The built-in analyzers (proposal §8). Each computes over
//! declared grain semantics — never raw prose — so the deterministic layer
//! works with zero models. All produce *pending* drafts; the engine stamps
//! identity/origin and runs the governance gates.

pub mod budget_pressure;
pub mod cold_grains;
pub mod contradiction_sweep;
pub mod coverage_gap;
pub mod duplicate_sweep;
pub mod fork_surfacing;
pub mod goal_stagnation;
pub mod outcome_review;
pub mod retention_sweep;
pub mod run_outcome;
pub mod skill_stall;
pub mod staleness;
pub mod tool_failure;

use crate::recommendation::MAX_EVIDENCE;

/// Bound an evidence list to the representative cap (§7.1). Deterministic:
/// callers pass an already-sorted list.
pub(crate) fn bound_evidence(mut hashes: Vec<String>) -> Vec<String> {
    hashes.truncate(MAX_EVIDENCE);
    hashes
}
