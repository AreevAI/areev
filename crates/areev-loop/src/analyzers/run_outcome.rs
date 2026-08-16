//! Run outcomes (T0) — whole-run health for `areev run` workflows (§8 Wave
//! 4). The datasource is the compact `run_outcome` Observation the driver
//! writes at every terminal run (outcome label + spent figures + plan
//! hash). Two signals, both advisory (Flag — what to do about a failing or
//! expensive workflow is a human/host decision, never auto-applied):
//!
//! - **Failure clusters per workflow**: the same plan hash finishing
//!   `failed`/`stalled`/`budget_exhausted` repeatedly.
//! - **Cost attribution**: aggregate USD spend per workflow — the run-side
//!   `budget_pressure` signal (assembly-budget pressure has its own
//!   analyzer; this one watches what runs actually spend).

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};
use std::collections::BTreeMap;

/// The namespace `areev run` journals into.
const HARNESS_NS: &str = "agent:harness";

pub struct RunOutcome {
    manifest: AnalyzerManifest,
}

impl RunOutcome {
    pub fn new() -> Self {
        RunOutcome {
            manifest: AnalyzerManifest {
                id: "loop.run_outcome/1".into(),
                title: "Run outcomes".into(),
                description:
                    "Flags workflows whose runs keep failing, stalling, or exhausting \
                     budgets, and attributes run spend per workflow."
                        .into(),
                tier: Tier::T0,
                cadence: CadenceClass::Slow,
                requires: vec![],
                target_classes: vec![TargetClass::Host],
                auto_apply: AutoApplyClass::Never,
                trust_class: TrustClass::Builtin,
                params: vec![
                    ParamSpec::Int {
                        name: "min_runs".into(),
                        default: 3,
                        min: 1,
                        max: 1_000_000,
                        description: "Minimum terminal runs of one workflow before its \
                                      failure rate is meaningful."
                            .into(),
                    },
                    ParamSpec::Float {
                        name: "min_failure_ratio".into(),
                        default: 0.5,
                        min: 0.0,
                        max: 1.0,
                        description: "Non-completed fraction at or above which the \
                                      workflow is flagged."
                            .into(),
                    },
                    ParamSpec::Int {
                        name: "min_usd_micros".into(),
                        default: 5_000_000, // $5
                        min: 0,
                        max: i64::MAX,
                        description: "Aggregate spend (micro-USD) at or above which a \
                                      workflow's cost is surfaced."
                            .into(),
                    },
                ],
                default_on: true,
            },
        }
    }
}

impl Default for RunOutcome {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct PlanStats {
    runs: i64,
    failed: i64,
    usd_micros: i64,
    last_detail: String,
}

impl Analyzer for RunOutcome {
    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let obs = match ctx.grains_in("observation", HARNESS_NS) {
            Ok(rows) => rows,
            // No harness namespace / no read grant: no runs to analyze —
            // degrade to nothing, never fabricate.
            Err(_) => return Ok(Vec::new()),
        };
        let min_runs = ctx.params().get_int("min_runs");
        let min_failure_ratio = ctx.params().get_float("min_failure_ratio");
        let min_usd = ctx.params().get_int("min_usd_micros");

        let mut by_plan: BTreeMap<String, PlanStats> = BTreeMap::new();
        let mut evidence: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for g in &obs {
            if g.str_field("observation_kind") != Some("run_outcome") {
                continue;
            }
            let Some(plan) = g.str_field("plan_hash") else { continue };
            let outcome = g.str_field("object").unwrap_or_default();
            let entry = by_plan.entry(plan.to_string()).or_default();
            entry.runs += 1;
            if outcome != "completed" && outcome != "canceled" {
                entry.failed += 1;
                if let Some(d) = g.str_field("outcome_detail") {
                    entry.last_detail = d.to_string();
                } else {
                    entry.last_detail = outcome.to_string();
                }
            }
            entry.usd_micros += g
                .fields
                .get("spent_usd_micros")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            evidence.entry(plan.to_string()).or_default().push(g.hash.clone());
        }

        let mut out = Vec::new();
        for (plan, s) in &by_plan {
            let short = &plan[..plan.len().min(12)];
            if s.runs >= min_runs
                && (s.failed as f64 / s.runs as f64) >= min_failure_ratio
            {
                let mut args = Map::new();
                args.insert("workflow".into(), json!(short));
                args.insert("failed".into(), json!(s.failed));
                args.insert("runs".into(), json!(s.runs));
                args.insert(
                    "rate".into(),
                    json!(((s.failed as f64 / s.runs as f64) * 100.0).round() as i64),
                );
                args.insert("last_error".into(), json!(s.last_detail));
                let mut data = Map::new();
                data.insert("plan_hash".into(), json!(plan));
                data.insert("failed".into(), json!(s.failed));
                data.insert("runs".into(), json!(s.runs));
                out.push(
                    RecDraft::new(
                        format!("host:workflow/{plan}"),
                        ActionKind::Flag,
                        Summary::new("run.failures", args),
                        Proposal::Data { data },
                    )
                    .severity(Severity::High)
                    .evidence(evidence.get(plan).cloned().unwrap_or_default()),
                );
            }
            if min_usd > 0 && s.usd_micros >= min_usd && s.runs > 0 {
                let mut args = Map::new();
                args.insert("workflow".into(), json!(short));
                args.insert("runs".into(), json!(s.runs));
                args.insert(
                    "usd".into(),
                    json!(format!("{:.2}", s.usd_micros as f64 / 1e6)),
                );
                args.insert(
                    "avg_usd".into(),
                    json!(format!("{:.2}", s.usd_micros as f64 / 1e6 / s.runs as f64)),
                );
                let mut data = Map::new();
                data.insert("plan_hash".into(), json!(plan));
                data.insert("usd_micros".into(), json!(s.usd_micros));
                out.push(
                    RecDraft::new(
                        format!("host:workflow-cost/{plan}"),
                        ActionKind::Flag,
                        Summary::new("run.cost", args),
                        Proposal::Data { data },
                    )
                    .severity(Severity::Medium)
                    .evidence(evidence.get(plan).cloned().unwrap_or_default()),
                );
            }
        }
        Ok(out)
    }

    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    fn outcome(sub: &mut TestSubstrate, plan: &str, outcome: &str, usd: i64) {
        sub.put_observation(
            HARNESS_NS,
            &[
                ("observation_kind", json!("run_outcome")),
                ("plan_hash", json!(plan)),
                ("object", json!(outcome)),
                ("spent_usd_micros", json!(usd)),
                ("outcome_detail", json!("greet: ExecutorError: down")),
            ],
        );
    }

    #[test]
    fn flags_failing_workflow() {
        let mut sub = TestSubstrate::new();
        for _ in 0..2 {
            outcome(&mut sub, "abcd1234", "failed", 0);
        }
        outcome(&mut sub, "abcd1234", "completed", 0);
        let drafts = sub.analyze(&RunOutcome::new(), 10_000);
        assert_eq!(drafts.len(), 1, "{drafts:?}");
        let text = drafts[0].summary.render();
        assert!(text.contains("failed 2/3"), "{text}");
        assert_eq!(drafts[0].action_kind, ActionKind::Flag);
    }

    #[test]
    fn healthy_workflow_stays_quiet() {
        let mut sub = TestSubstrate::new();
        for _ in 0..5 {
            outcome(&mut sub, "abcd1234", "completed", 100);
        }
        assert!(sub.analyze(&RunOutcome::new(), 10_000).is_empty());
    }

    #[test]
    fn cost_attribution_surfaces_expensive_workflows() {
        let mut sub = TestSubstrate::new();
        for _ in 0..4 {
            outcome(&mut sub, "eeff5566", "completed", 2_000_000); // $2 each
        }
        let drafts = sub.analyze(&RunOutcome::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        let text = drafts[0].summary.render();
        assert!(text.contains("8.00"), "aggregate spend rendered: {text}");
    }
}
