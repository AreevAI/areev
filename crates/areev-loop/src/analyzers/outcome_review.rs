//! Outcome review (T0). For applied recommendations past their `review_after`,
//! the engine re-runs the stored metric query (it owns the `&mut` substrate)
//! and hands the measured values in as `OutcomeInput`s; this analyzer makes the
//! deterministic changed/regressed decision and proposes a revert on
//! regression. Closes the honesty loop — makes approve and auto-apply
//! accountable to measured history.
//!
//! Our built-in metrics are lower-is-better (e.g. `tool_error_rate`), so a
//! regression is `current > baseline` beyond a small epsilon. An evalset
//! metric can be higher-is-better and inverts that, which is why the direction
//! travels on the input and the comparison lives in ONE place
//! (`recommendation::is_regression`).

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};

pub struct OutcomeReview {
    manifest: AnalyzerManifest,
}

impl OutcomeReview {
    pub fn new() -> Self {
        OutcomeReview {
            manifest: AnalyzerManifest {
                id: "loop.outcome_review/1".into(),
                title: "Outcome review".into(),
                description:
                    "Re-measures applied recommendations and proposes revert on regression.".into(),
                tier: Tier::T0,
                cadence: CadenceClass::Fast,
                requires: vec![],
                target_classes: vec![TargetClass::Memory, TargetClass::Query],
                auto_apply: AutoApplyClass::Never,
                trust_class: TrustClass::Builtin,
                params: vec![],
                default_on: true,
            },
        }
    }
}

impl Default for OutcomeReview {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for OutcomeReview {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let mut drafts = Vec::new();
        for input in ctx.outcome_inputs() {
            let regressed = crate::recommendation::is_regression(
                input.baseline,
                input.current,
                input.higher_is_better,
                input.tolerance,
            );
            if !regressed {
                continue;
            }
            let mut args = Map::new();
            args.insert("metric".into(), json!(input.metric));
            args.insert("baseline".into(), json!(round4(input.baseline)));
            args.insert("current".into(), json!(round4(input.current)));
            if let Some(run) = &input.baseline_run_id {
                args.insert("baseline_run".into(), json!(run));
            }
            if let Some(best) = input.best_before {
                args.insert("best_before".into(), json!(round4(best)));
            }
            if input.tolerance > 0.0 {
                args.insert("tolerance".into(), json!(round4(input.tolerance)));
            }

            let mut data = Map::new();
            data.insert("revert_of".into(), json!(input.rec_hash));
            data.insert("metric".into(), json!(input.metric));

            // The gate asks two questions of an applied recommendation: did
            // its metric hold, and does its premise still stand. The engine
            // feeds both here as inputs; the summary says which one failed.
            let key = if input.metric == crate::engine::PREMISE_DRIFT_METRIC {
                "outcome.premise_drift"
            } else if input.baseline_kind == "high_water" {
                // Drafted against the peak: the summary names both figures so
                // the reviewer judges whether this rule owns the whole fall.
                "outcome.regression_high_water"
            } else {
                "outcome.regression"
            };
            drafts.push(
                RecDraft::new(
                    input.target_ref.clone(),
                    ActionKind::Revert,
                    Summary::new(key, args),
                    Proposal::Data { data },
                )
                .severity(Severity::High)
                .evidence(vec![input.rec_hash.clone()]),
            );
        }
        drafts.sort_by(|a, b| a.evidence.cmp(&b.evidence));
        Ok(drafts)
    }
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::OutcomeInput;
    use crate::testkit::TestSubstrate;

    fn input(baseline: f64, current: f64) -> OutcomeInput {
        OutcomeInput {
            rec_hash: "ref-1".into(),
            target_ref: "entity:lessons/stripe_refund".into(),
            metric: "tool_error_rate".into(),
            baseline,
            current,
            unit: "ratio".into(),
            higher_is_better: false,
            baseline_kind: "snapshot".into(),
            baseline_run_id: None,
            best_before: None,
            tolerance: 0.0,
        }
    }

    /// A higher-is-better metric (an evalset accuracy) must invert the verdict.
    /// Reading it the built-in way would propose reverting exactly the changes
    /// that worked.
    fn rising(baseline: f64, current: f64) -> OutcomeInput {
        OutcomeInput {
            metric: "evalset:abc123:category_accuracy".into(),
            higher_is_better: true,
            ..input(baseline, current)
        }
    }

    #[test]
    fn proposes_revert_on_regression() {
        let mut sub = TestSubstrate::new();
        sub.set_outcome_inputs(vec![input(0.2, 0.5)]);
        let drafts = sub.analyze(&OutcomeReview::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].action_kind, ActionKind::Revert);
    }

    #[test]
    fn silent_when_improved_or_unchanged() {
        let mut sub = TestSubstrate::new();
        sub.set_outcome_inputs(vec![input(0.5, 0.2), input(0.3, 0.3)]);
        assert!(sub.analyze(&OutcomeReview::new(), 10_000).is_empty());
    }

    #[test]
    fn a_higher_is_better_metric_regresses_when_it_falls() {
        let mut sub = TestSubstrate::new();
        // Accuracy dropped 0.92 -> 0.71: that IS the regression.
        sub.set_outcome_inputs(vec![rising(0.92, 0.71)]);
        let drafts = sub.analyze(&OutcomeReview::new(), 10_000);
        assert_eq!(drafts.len(), 1, "a fall in accuracy must propose a revert");
        assert_eq!(drafts[0].action_kind, ActionKind::Revert);
    }

    /// A verdict drafted against the high-water mark names the run it fell
    /// from: a reviewer deciding whether THIS rule owns the whole fall needs
    /// the peak beside the current value, not a bare pair of numbers.
    #[test]
    fn a_high_water_regression_names_the_peak_run() {
        let mut sub = TestSubstrate::new();
        sub.set_outcome_inputs(vec![OutcomeInput {
            baseline_kind: "high_water".into(),
            baseline_run_id: Some("eval-peak".into()),
            best_before: Some(238.0),
            ..rising(238.0, 133.0)
        }]);
        let drafts = sub.analyze(&OutcomeReview::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        let text = drafts[0].summary.render();
        assert!(text.contains("eval-peak") && text.contains("238") && text.contains("133"), "{text}");
        assert_eq!(drafts[0].summary.args["best_before"], 238.0);
    }

    /// The floor the engine judged under travels with the input: a dip inside
    /// it drafts nothing, one past it drafts the revert and records the floor.
    #[test]
    fn the_revert_draft_applies_the_same_floor_as_the_verdict() {
        let mut sub = TestSubstrate::new();
        sub.set_outcome_inputs(vec![
            OutcomeInput { tolerance: 5.0, ..rising(359.0, 355.0) },
            OutcomeInput { tolerance: 5.0, rec_hash: "ref-2".into(), ..rising(359.0, 353.0) },
        ]);
        let drafts = sub.analyze(&OutcomeReview::new(), 10_000);
        assert_eq!(drafts.len(), 1, "only the dip past the floor reverts");
        assert_eq!(drafts[0].evidence, vec!["ref-2".to_string()]);
        assert_eq!(drafts[0].summary.args["tolerance"], 5.0);
    }

    #[test]
    fn a_higher_is_better_metric_holds_when_it_rises() {
        let mut sub = TestSubstrate::new();
        // The failure this pins: reading accuracy with the lower-is-better
        // rule would revert the recommendation that improved it.
        sub.set_outcome_inputs(vec![rising(0.71, 0.92), rising(0.8, 0.8)]);
        assert!(
            sub.analyze(&OutcomeReview::new(), 10_000).is_empty(),
            "rising accuracy is the receipt, not a regression"
        );
    }
}
