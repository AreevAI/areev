//! Budget pressure (T0; requires `telemetry`) — when context assembly keeps
//! overflowing its token budget, recall is being forced to drop material it
//! selected: the memory has outgrown the window it's rendered into. The signal
//! is the assembly-budget rollup in the telemetry sidecar (§8). Advisory and
//! global (one finding, not per-entity): the remedy — raise the budget, tighten
//! selection, or curate — is a human/host decision, so it never auto-applies.
//!
//! **Default-on.** The datasource is wired: the ASSEMBLE budget allocator
//! (`areev-cal`) records one sample per assembly via
//! `CalStoreFacade::note_assembly_budget` → `Areev::telemetry_note_budget`
//! (overflow = the token budget forced grains to be dropped). With no telemetry
//! sidecar the `requires: [telemetry]` gate degrades it cleanly to nothing.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};

pub struct BudgetPressure {
    manifest: AnalyzerManifest,
}

impl BudgetPressure {
    pub fn new() -> Self {
        BudgetPressure {
            manifest: AnalyzerManifest {
                id: "loop.budget_pressure/1".into(),
                title: "Budget pressure".into(),
                description: "Flags context assembly that keeps overflowing its token budget."
                    .into(),
                tier: Tier::T0,
                cadence: CadenceClass::Slow,
                requires: vec![Capability::Telemetry],
                target_classes: vec![TargetClass::Memory],
                auto_apply: AutoApplyClass::Never, // raising a budget is a host decision
                trust_class: TrustClass::Builtin,
                params: vec![
                    ParamSpec::Int {
                        name: "min_samples".into(),
                        default: 20,
                        min: 1,
                        max: 10_000_000,
                        description: "Minimum assembly samples before overflow rate is meaningful."
                            .into(),
                    },
                    ParamSpec::Float {
                        name: "min_overflow_ratio".into(),
                        default: 0.5,
                        min: 0.0,
                        max: 1.0,
                        description: "Overflow fraction at or above which pressure is flagged."
                            .into(),
                    },
                ],
                default_on: true,
            },
        }
    }
}

impl Default for BudgetPressure {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for BudgetPressure {
    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let Some(tel) = ctx.telemetry()? else {
            return Ok(Vec::new());
        };
        let min_samples = ctx.params().get_int("min_samples");
        let min_overflow_ratio = ctx.params().get_float("min_overflow_ratio");

        let b = &tel.budget;
        if b.sample_count < min_samples || b.sample_count <= 0 {
            return Ok(Vec::new());
        }
        let ratio = b.overflow_count as f64 / b.sample_count as f64;
        if ratio < min_overflow_ratio {
            return Ok(Vec::new());
        }
        let overflow_rate = (ratio * 100.0).round() as i64;

        let mut args = Map::new();
        args.insert("overflow_rate".into(), json!(overflow_rate));
        args.insert("samples".into(), json!(b.sample_count));

        let mut data = Map::new();
        data.insert("sample_count".into(), json!(b.sample_count));
        data.insert("overflow_count".into(), json!(b.overflow_count));

        Ok(vec![RecDraft::new(
            "entity:budget/assembly".to_string(),
            ActionKind::Flag,
            Summary::new("budget.pressure", args),
            Proposal::Data { data },
        )
        .severity(Severity::Medium)])
    }

    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    #[test]
    fn flags_sustained_overflow() {
        let mut sub = TestSubstrate::new();
        sub.telemetry_budget(40, 30); // 75% ≥ 50%, enough samples
        let drafts = sub.analyze(&BudgetPressure::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].action_kind, ActionKind::Flag);
        assert!(drafts[0].summary.render().contains('%'));
    }

    #[test]
    fn low_overflow_is_fine() {
        let mut sub = TestSubstrate::new();
        sub.telemetry_budget(40, 5); // 12.5% < 50%
        assert!(sub.analyze(&BudgetPressure::new(), 10_000).is_empty());
    }

    #[test]
    fn too_few_samples_no_finding() {
        let mut sub = TestSubstrate::new();
        sub.telemetry_budget(5, 5); // 100% but only 5 samples < 20
        assert!(sub.analyze(&BudgetPressure::new(), 10_000).is_empty());
    }
}
