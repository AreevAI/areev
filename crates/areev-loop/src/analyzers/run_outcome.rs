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
                     budgets, attributes run spend per workflow, and surfaces those \
                     whose transcripts keep outgrowing the model's window."
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
                        name: "min_folds_per_run".into(),
                        // Folding once in a while is the mechanism working. A
                        // workflow averaging one fold PER RUN is one whose
                        // nodes no longer fit, which is a design signal.
                        default: 1,
                        min: 1,
                        max: 1_000_000,
                        description: "Average transcript folds per run at or above which \
                                      a workflow's context pressure is surfaced."
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
    folds: i64,
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
        let min_folds_per_run = ctx.params().get_int("min_folds_per_run");

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
            // Absent on every run that never folded, which is most of them.
            entry.folds +=
                g.fields.get("folds").and_then(serde_json::Value::as_i64).unwrap_or(0);
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
            // Context pressure. A fold is the runtime keeping a long agent
            // alive, so one is not a problem — a workflow that needs one on
            // EVERY run is telling you its nodes no longer fit the window, and
            // that is a plan-shape decision a person makes: split the node,
            // bound its tool results, or accept the summaries. Advisory only;
            // there is nothing here for an apply to do automatically.
            if s.runs > 0 && s.folds > 0 && s.folds >= min_folds_per_run * s.runs {
                let mut args = Map::new();
                args.insert("workflow".into(), json!(short));
                args.insert("runs".into(), json!(s.runs));
                args.insert("folds".into(), json!(s.folds));
                args.insert(
                    "avg_folds".into(),
                    json!(format!("{:.1}", s.folds as f64 / s.runs as f64)),
                );
                let mut data = Map::new();
                data.insert("plan_hash".into(), json!(plan));
                data.insert("folds".into(), json!(s.folds));
                data.insert("runs".into(), json!(s.runs));
                out.push(
                    RecDraft::new(
                        format!("host:workflow-context/{plan}"),
                        ActionKind::Flag,
                        Summary::new("run.context_pressure", args),
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

    /// A completed run that had to summarize itself `folds` times.
    fn folded_run(sub: &mut TestSubstrate, plan: &str, folds: i64) {
        sub.put_observation(
            HARNESS_NS,
            &[
                ("observation_kind", json!("run_outcome")),
                ("plan_hash", json!(plan)),
                ("object", json!("completed")),
                ("spent_usd_micros", json!(0)),
                ("folds", json!(folds)),
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

    /// A workflow that folds on every run has outgrown the window. That is a
    /// plan-shape decision (split the node, bound its tool results, or accept
    /// the summaries) so it surfaces as an advisory Flag with nothing to
    /// auto-apply — the same posture as the cost signal beside it.
    #[test]
    fn a_workflow_that_folds_every_run_is_surfaced() {
        let mut sub = TestSubstrate::new();
        for _ in 0..3 {
            folded_run(&mut sub, "c0ffee11", 2);
        }
        let drafts = sub.analyze(&RunOutcome::new(), 10_000);
        let pressure: Vec<_> = drafts
            .iter()
            .filter(|d| d.summary.render().contains("summarized its own transcript"))
            .collect();
        assert_eq!(pressure.len(), 1, "{drafts:?}");
        let text = pressure[0].summary.render();
        assert!(text.contains("6 time(s) across 3 runs"), "{text}");
        assert!(text.contains("avg 2.0/run"), "{text}");
        assert_eq!(pressure[0].action_kind, ActionKind::Flag);
    }

    /// Folding is the mechanism WORKING. An occasional fold must not nag: the
    /// signal is a workflow that needs one every time, not one that ever needed
    /// one at all.
    #[test]
    fn an_occasional_fold_is_not_a_finding() {
        let mut sub = TestSubstrate::new();
        folded_run(&mut sub, "c0ffee11", 1);
        for _ in 0..5 {
            outcome(&mut sub, "c0ffee11", "completed", 0);
        }
        let drafts = sub.analyze(&RunOutcome::new(), 10_000);
        assert!(
            !drafts.iter().any(|d| d.summary.render().contains("summarized its own")),
            "one fold in six runs is healthy: {drafts:?}"
        );
    }

    /// The field is absent on every run that never folded, and an absent field
    /// must read as zero rather than as anything at all.
    #[test]
    fn runs_that_never_folded_carry_no_pressure_signal() {
        let mut sub = TestSubstrate::new();
        for _ in 0..4 {
            outcome(&mut sub, "c0ffee11", "completed", 0);
        }
        let drafts = sub.analyze(&RunOutcome::new(), 10_000);
        assert!(
            !drafts.iter().any(|d| d.summary.render().contains("summarized its own")),
            "{drafts:?}"
        );
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
