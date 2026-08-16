//! Goal stagnation (T0) — a Goal grain (0x07) that is still `active`, has made
//! little progress, and is old. Computed from the grain's own `goal_state` +
//! `progress` fields (the field-based form the review asked about — not the
//! weaker "no progress events" form). Advisory only; never auto-applies.
//!
//! **Default-OFF (opt-in).** "Low progress on an old active goal" is genuinely
//! ambiguous — legitimate long-running work looks identical to a stall — so
//! this ships opt-in rather than default-on until field precision is validated
//! on real corpora. Enable it per file by setting `enabled: true` for
//! `loop.goal_stagnation/1` via the console's per-analyzer config
//! (`POST /api/loop/config`) or the bindings' `set_analyzer_config`.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};

pub struct GoalStagnation {
    manifest: AnalyzerManifest,
}

impl GoalStagnation {
    pub fn new() -> Self {
        GoalStagnation {
            manifest: AnalyzerManifest {
                id: "loop.goal_stagnation/1".into(),
                title: "Goal stagnation".into(),
                description: "Flags active goals with little progress that have gone stale.".into(),
                tier: Tier::T0,
                cadence: CadenceClass::Fast,
                requires: vec![],
                target_classes: vec![TargetClass::Memory],
                auto_apply: AutoApplyClass::Never,
                trust_class: TrustClass::Builtin,
                params: vec![
                    ParamSpec::Float {
                        name: "max_progress".into(),
                        default: 0.1,
                        min: 0.0,
                        max: 1.0,
                        description: "Progress at or below this counts as no progress.".into(),
                    },
                    ParamSpec::Int {
                        name: "min_age_days".into(),
                        default: 30,
                        min: 1,
                        max: 3650,
                        description: "Days an active low-progress goal must be old to flag.".into(),
                    },
                ],
                // Opt-in: "stalled" is ambiguous; validate field precision first.
                default_on: false,
            },
        }
    }
}

impl Default for GoalStagnation {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for GoalStagnation {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let max_progress = ctx.params().get_float("max_progress");
        let min_age_ms = ctx.params().get_int("min_age_days") * 86_400_000;
        let cutoff = ctx.now_ms() - min_age_ms;

        let mut drafts = Vec::new();
        for g in ctx.goals()? {
            if g.goal_state() != Some("active") {
                continue;
            }
            if g.goal_progress() > max_progress || g.created_at_ms > cutoff {
                continue;
            }
            let age_days = ((ctx.now_ms() - g.created_at_ms).max(0)) / 86_400_000;
            let label = g
                .str_field("subject")
                .or_else(|| g.str_field("description"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| short_hash(&g.hash));

            let mut args = Map::new();
            args.insert("goal".into(), json!(label));
            args.insert("age_days".into(), json!(age_days));
            args.insert("progress".into(), json!(g.goal_progress()));

            let mut data = Map::new();
            data.insert("goal".into(), json!(label));
            data.insert("progress".into(), json!(g.goal_progress()));
            data.insert("age_days".into(), json!(age_days));

            drafts.push(
                RecDraft::new(
                    format!("entity:goals/{label}"),
                    ActionKind::Flag,
                    Summary::new("goal.stagnation", args),
                    Proposal::Data { data },
                )
                .severity(Severity::Low)
                .evidence(vec![g.hash.clone()]),
            );
        }
        drafts.sort_by(|a, b| a.target_ref.cmp(&b.target_ref));
        Ok(drafts)
    }
}

fn short_hash(h: &str) -> String {
    h.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    #[test]
    fn flags_old_active_no_progress_goal() {
        let mut sub = TestSubstrate::new();
        let day = 86_400_000;
        let now = 100 * day;
        sub.add_goal("ship_v2", "active", 0.05, now - 40 * day); // old, no progress
        sub.add_goal("done_thing", "satisfied", 1.0, now - 40 * day); // not active
        sub.add_goal("fresh", "active", 0.0, now - 5 * day); // too young
        sub.add_goal("moving", "active", 0.6, now - 40 * day); // progressing
        let drafts = sub.analyze(&GoalStagnation::new(), now);
        assert_eq!(drafts.len(), 1);
        assert!(drafts[0].summary.render().contains("ship_v2"));
    }
}
