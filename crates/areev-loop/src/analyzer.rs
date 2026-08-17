//! The `Analyzer` trait and `AnalyzeCtx` — the SDK seam. `AnalyzeCtx` is a
//! struct (not a trait) so the engine can add methods without breaking
//! implementors. It exposes only read-only substrate access plus resolved
//! params, the watermark, and `now` — analyzers cannot write (trust floor),
//! enforced by holding `&dyn SubstrateRead`.

use crate::error::Result;
use crate::manifest::{AnalyzerManifest, Params};
use crate::model::GrainRecord;
use crate::recommendation::RecDraft;
use crate::substrate::{HeadGroup, ReadOpts, SubstrateRead, TelemetryView};

/// One applied recommendation due for outcome review, with its metric already
/// re-measured by the engine (which owns the `&mut` substrate). The outcome
/// analyzer makes the deterministic changed/regressed decision over this — I/O
/// in the engine, judgment in the analyzer.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeInput {
    pub rec_hash: String,
    pub target_ref: String,
    pub metric: String,
    pub baseline: f64,
    pub current: f64,
    pub unit: String,
    /// Carried from the metric snapshot so the analyzer applies the SAME
    /// direction the engine did — see `recommendation::is_regression`.
    pub higher_is_better: bool,
}

/// The context handed to `analyze`. Read-only by construction.
pub struct AnalyzeCtx<'a> {
    reader: &'a dyn SubstrateRead,
    params: &'a Params,
    namespaces: &'a [String],
    watermark_ms: Option<i64>,
    now_ms: i64,
    outcome_inputs: &'a [OutcomeInput],
}

impl<'a> AnalyzeCtx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reader: &'a dyn SubstrateRead,
        params: &'a Params,
        namespaces: &'a [String],
        watermark_ms: Option<i64>,
        now_ms: i64,
        outcome_inputs: &'a [OutcomeInput],
    ) -> Self {
        AnalyzeCtx {
            reader,
            params,
            namespaces,
            watermark_ms,
            now_ms,
            outcome_inputs,
        }
    }

    pub fn params(&self) -> &Params {
        self.params
    }
    pub fn now_ms(&self) -> i64 {
        self.now_ms
    }
    pub fn watermark_ms(&self) -> Option<i64> {
        self.watermark_ms
    }
    pub fn capabilities(&self) -> crate::substrate::Capabilities {
        self.reader.capabilities()
    }
    pub fn outcome_inputs(&self) -> &[OutcomeInput] {
        self.outcome_inputs
    }

    /// All grains of a type across the configured namespaces (or all namespaces
    /// when none are configured).
    pub fn grains_of_type(&self, grain_type: &str, opts: ReadOpts) -> Result<Vec<GrainRecord>> {
        if self.namespaces.is_empty() {
            self.reader.grains_of_type(grain_type, None, opts)
        } else {
            let mut out = Vec::new();
            for ns in self.namespaces {
                out.extend(self.reader.grains_of_type(grain_type, Some(ns), opts)?);
            }
            Ok(out)
        }
    }

    /// Live facts across configured namespaces.
    pub fn facts(&self) -> Result<Vec<GrainRecord>> {
        self.grains_of_type(crate::model::grain_type::FACT, ReadOpts::default())
    }

    /// Live observations across configured namespaces.
    pub fn observations(&self) -> Result<Vec<GrainRecord>> {
        self.grains_of_type(crate::model::grain_type::OBSERVATION, ReadOpts::default())
    }

    /// Live grains of one type in ONE explicit namespace — for analyzers
    /// whose datasource lives in a well-known system namespace (the run
    /// journals in `agent:harness`) regardless of the sweep's configured
    /// scope.
    pub fn grains_in(&self, grain_type: &str, namespace: &str) -> Result<Vec<GrainRecord>> {
        self.reader
            .grains_of_type(grain_type, Some(namespace), ReadOpts::default())
    }

    /// Live Skill grains across configured namespaces.
    pub fn skills(&self) -> Result<Vec<GrainRecord>> {
        self.grains_of_type(crate::model::grain_type::SKILL, ReadOpts::default())
    }

    /// Live Goal grains across configured namespaces.
    pub fn goals(&self) -> Result<Vec<GrainRecord>> {
        self.grains_of_type(crate::model::grain_type::GOAL, ReadOpts::default())
    }

    /// Tool grains (captured tool calls), optionally windowed by a `since`
    /// watermark, live only. The flagship analyzer's input.
    pub fn tools_since(&self, since_ms: Option<i64>) -> Result<Vec<GrainRecord>> {
        self.grains_of_type(
            crate::model::grain_type::TOOL,
            ReadOpts {
                live_only: true,
                since_ms,
            },
        )
    }

    /// Entities with more than one live head (requires the forks capability).
    pub fn heads(&self) -> Result<Vec<HeadGroup>> {
        let ns = if self.namespaces.len() == 1 {
            Some(self.namespaces[0].as_str())
        } else {
            None
        };
        self.reader.heads(ns)
    }

    /// A snapshot of the recall-telemetry rollups (requires the `telemetry`
    /// capability). `None` when the substrate has no sidecar — a telemetry-fed
    /// analyzer then degrades to an activation-ladder entry.
    pub fn telemetry(&self) -> Result<Option<TelemetryView>> {
        let ns = if self.namespaces.len() == 1 {
            Some(self.namespaces[0].as_str())
        } else {
            None
        };
        self.reader.telemetry(ns)
    }
}

/// An analysis unit. Object-safe so `builtin_analyzers()` yields trait objects.
pub trait Analyzer: Send + Sync {
    fn manifest(&self) -> &AnalyzerManifest;

    /// Produce recommendation drafts. `dedup_key`, `origin`, and the params
    /// snapshot are stamped by the engine afterward, not here. Returning an
    /// error drops *this* analyzer's findings for the run; other analyzers are
    /// unaffected.
    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>>;
}

/// The default-registered built-in analyzers. Count is test-pinned (§11).
pub fn builtin_analyzers() -> Vec<Box<dyn Analyzer>> {
    vec![
        Box::new(crate::analyzers::tool_failure::ToolFailureClustering::new()),
        Box::new(crate::analyzers::duplicate_sweep::DuplicateSweep::new()),
        Box::new(crate::analyzers::contradiction_sweep::ContradictionSweep::new()),
        Box::new(crate::analyzers::fork_surfacing::ForkSurfacing::new()),
        Box::new(crate::analyzers::staleness::Staleness::new()),
        Box::new(crate::analyzers::skill_stall::SkillStall::new()),
        Box::new(crate::analyzers::goal_stagnation::GoalStagnation::new()),
        Box::new(crate::analyzers::cold_grains::ColdGrains::new()),
        Box::new(crate::analyzers::coverage_gap::CoverageGap::new()),
        Box::new(crate::analyzers::budget_pressure::BudgetPressure::new()),
        Box::new(crate::analyzers::outcome_review::OutcomeReview::new()),
        Box::new(crate::analyzers::retention_sweep::RetentionSweep::new()),
        Box::new(crate::analyzers::run_outcome::RunOutcome::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_have_unique_ids() {
        let a = builtin_analyzers();
        assert_eq!(
            a.len(),
            13,
            "6 hygiene + skill/goal trajectory + 3 telemetry-fed (cold/coverage/budget) \
             + retention (default-off) + run_outcome"
        );
        let mut ids: Vec<&str> = a.iter().map(|x| x.manifest().id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 13, "analyzer ids must be unique");
    }

    #[test]
    fn all_builtins_are_trust_class_builtin() {
        for a in builtin_analyzers() {
            assert_eq!(
                a.manifest().trust_class,
                crate::manifest::TrustClass::Builtin,
                "{} must be builtin trust class",
                a.manifest().id
            );
        }
    }
}
