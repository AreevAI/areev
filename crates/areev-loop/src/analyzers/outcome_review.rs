//! Outcome review (T0). For applied recommendations past their `review_after`,
//! the engine re-runs the stored metric query (it owns the `&mut` substrate)
//! and hands the measured values in as `OutcomeInput`s; this analyzer makes the
//! deterministic changed/regressed decision and proposes a revert on
//! regression. Closes the honesty loop — makes approve and auto-apply
//! accountable to measured history.
//!
//! Our built-in metrics are lower-is-better (e.g. `tool_error_rate`), so a
//! regression is `current > baseline` beyond a small epsilon.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};

/// Minimum relative worsening to call a regression (avoids noise at n=1).
const REGRESSION_EPSILON: f64 = 1e-9;

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
            let regressed = input.current > input.baseline + REGRESSION_EPSILON;
            if !regressed {
                continue;
            }
            let mut args = Map::new();
            args.insert("metric".into(), json!(input.metric));
            args.insert("baseline".into(), json!(round4(input.baseline)));
            args.insert("current".into(), json!(round4(input.current)));

            let mut data = Map::new();
            data.insert("revert_of".into(), json!(input.rec_hash));
            data.insert("metric".into(), json!(input.metric));

            drafts.push(
                RecDraft::new(
                    input.target_ref.clone(),
                    ActionKind::Revert,
                    Summary::new("outcome.regression", args),
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
}
