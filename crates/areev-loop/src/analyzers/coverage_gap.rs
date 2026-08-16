//! Coverage gap (T0; requires `telemetry`) — recurring *questions the memory
//! can't answer*. When the same recall query keeps coming back empty, the agent
//! is repeatedly reaching for knowledge that was never stored: a gap the memory
//! should be filled to close. Deterministic consistency checks are blind to
//! this — it lives entirely in the recall-telemetry query rollups (§8).
//! Advisory: the fix is to *add* the missing memory (a human/host act), so it
//! flags the gap and never auto-applies.
//!
//! A T1 refinement (embedding-clustered near-duplicate questions) is a natural
//! extension; v1 keys on the exact recurring query, which is precise and needs
//! no embedder.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};

pub struct CoverageGap {
    manifest: AnalyzerManifest,
}

impl CoverageGap {
    pub fn new() -> Self {
        CoverageGap {
            manifest: AnalyzerManifest {
                id: "loop.coverage_gap/1".into(),
                title: "Coverage gap".into(),
                description: "Flags recurring recall questions that keep returning nothing.".into(),
                tier: Tier::T0,
                cadence: CadenceClass::Slow,
                requires: vec![Capability::Telemetry],
                target_classes: vec![TargetClass::Memory],
                auto_apply: AutoApplyClass::Never, // the fix is to ADD memory — human act
                trust_class: TrustClass::Builtin,
                params: vec![
                    ParamSpec::Int {
                        name: "min_runs".into(),
                        default: 3,
                        min: 1,
                        max: 1_000_000,
                        description: "How many times a question must recur before it's a gap."
                            .into(),
                    },
                    ParamSpec::Float {
                        name: "min_empty_ratio".into(),
                        default: 0.8,
                        min: 0.0,
                        max: 1.0,
                        description: "Fraction of runs that returned nothing to count as a gap."
                            .into(),
                    },
                ],
                default_on: true,
            },
        }
    }
}

impl Default for CoverageGap {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for CoverageGap {
    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let Some(tel) = ctx.telemetry()? else {
            return Ok(Vec::new());
        };
        let min_runs = ctx.params().get_int("min_runs");
        let min_empty_ratio = ctx.params().get_float("min_empty_ratio");

        let mut drafts = Vec::new();
        for q in &tel.queries {
            if q.run_count < min_runs || q.run_count <= 0 {
                continue;
            }
            let empty_ratio = q.empty_count as f64 / q.run_count as f64;
            if empty_ratio < min_empty_ratio {
                continue;
            }
            let empty_rate = (empty_ratio * 100.0).round() as i64;

            let mut args = Map::new();
            args.insert("query".into(), json!(q.sample));
            args.insert("count".into(), json!(q.run_count));
            args.insert("empty_rate".into(), json!(empty_rate));

            let mut data = Map::new();
            data.insert("query".into(), json!(q.sample));
            data.insert("run_count".into(), json!(q.run_count));
            data.insert("empty_count".into(), json!(q.empty_count));

            drafts.push(
                RecDraft::new(
                    format!("entity:coverage/{}", q.sample),
                    ActionKind::Flag,
                    Summary::new("coverage.gap", args),
                    Proposal::Data { data },
                )
                .severity(Severity::Medium),
            );
        }
        drafts.sort_by(|a, b| a.target_ref.cmp(&b.target_ref));
        Ok(drafts)
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
    fn flags_recurring_empty_question_only() {
        let mut sub = TestSubstrate::new();
        sub.telemetry_query("refund policy for EU", 5, 5); // recurs, always empty → gap
        sub.telemetry_query("shipping time", 4, 0); // recurs, always answered → fine
        sub.telemetry_query("one-off typo", 1, 1); // below min_runs → ignore

        let drafts = sub.analyze(&CoverageGap::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].action_kind, ActionKind::Flag);
        assert!(drafts[0].summary.render().contains("refund policy"));
    }

    #[test]
    fn no_telemetry_means_no_findings() {
        let sub = TestSubstrate::new();
        assert!(sub.analyze(&CoverageGap::new(), 10_000).is_empty());
    }
}
