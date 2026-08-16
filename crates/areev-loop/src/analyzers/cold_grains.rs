//! Cold grains (T0; requires `telemetry`) — the first *utility* analyzer, not
//! a consistency one. A fact that has sat in memory past a grace window and has
//! **never been surfaced by recall** is memory that isn't earning its place:
//! it costs storage and assembly budget without ever informing an answer. This
//! is exactly the signal deterministic consistency checks can't see — it needs
//! the recall-telemetry sidecar (§8). Advisory only: cold ≠ wrong (a rarely-hit
//! but critical fact is legitimately cold), so it flags a retire *candidate*
//! for human judgment and never auto-applies.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};
use std::collections::HashSet;

const DAY_MS: i64 = 24 * 3600 * 1000;

pub struct ColdGrains {
    manifest: AnalyzerManifest,
}

impl ColdGrains {
    pub fn new() -> Self {
        ColdGrains {
            manifest: AnalyzerManifest {
                id: "loop.cold_grains/1".into(),
                title: "Cold grains".into(),
                description: "Flags facts that have never been recalled past a grace window."
                    .into(),
                tier: Tier::T0,
                cadence: CadenceClass::Slow,
                requires: vec![Capability::Telemetry],
                target_classes: vec![TargetClass::Memory],
                auto_apply: AutoApplyClass::Never, // cold ≠ wrong — human decides
                trust_class: TrustClass::Builtin,
                params: vec![
                    ParamSpec::Int {
                        name: "min_age_days".into(),
                        default: 30,
                        min: 0,
                        max: 3650,
                        description: "A grain younger than this is too new to call cold.".into(),
                    },
                    ParamSpec::Int {
                        name: "max_recalls".into(),
                        default: 0,
                        min: 0,
                        max: 1_000_000,
                        description: "A grain recalled at most this many times counts as cold."
                            .into(),
                    },
                ],
                default_on: true,
            },
        }
    }
}

impl Default for ColdGrains {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for ColdGrains {
    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        // Graceful degradation: no sidecar → no signal, not a false "all cold".
        let Some(tel) = ctx.telemetry()? else {
            return Ok(Vec::new());
        };
        let min_age_days = ctx.params().get_int("min_age_days");
        let max_recalls = ctx.params().get_int("max_recalls");
        let min_age_ms = min_age_days.saturating_mul(DAY_MS);
        let now = ctx.now_ms();

        // The "warm" set: grains recalled often enough not to be cold.
        let warm: HashSet<&str> = tel
            .access
            .iter()
            .filter(|a| a.recall_count > max_recalls)
            .map(|a| a.hash.as_str())
            .collect();

        let mut drafts = Vec::new();
        for f in ctx.facts()? {
            let age = now - f.created_at_ms;
            if age < min_age_ms || warm.contains(f.hash.as_str()) {
                continue;
            }
            let subject = f.fact_subject().unwrap_or("").to_string();
            let age_days = age / DAY_MS;

            let mut args = Map::new();
            args.insert("subject".into(), json!(subject));
            args.insert("age_days".into(), json!(age_days));

            let mut data = Map::new();
            data.insert("hash".into(), json!(f.hash));
            data.insert("subject".into(), json!(subject));
            data.insert("age_days".into(), json!(age_days));

            drafts.push(
                RecDraft::new(
                    format!("entity:cold/{}", f.hash),
                    ActionKind::Flag,
                    Summary::new("cold.grain", args),
                    Proposal::Data { data },
                )
                .severity(Severity::Low)
                .evidence(vec![f.hash.clone()]),
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
    fn flags_never_recalled_but_not_the_warm_one() {
        let mut sub = TestSubstrate::new();
        let cold = sub.add_fact("acme", "tier", "gold"); // never recalled
        let warm = sub.add_fact("beta", "tier", "silver"); // recalled a lot
        sub.telemetry_recall(&warm, 5);
        let _ = cold;

        let drafts = sub.analyze_with(&ColdGrains::new(), 10_000_000, &[("min_age_days", json!(0))]);
        assert_eq!(drafts.len(), 1, "only the never-recalled grain is cold");
        assert_eq!(drafts[0].action_kind, ActionKind::Flag);
        assert!(drafts[0].summary.render().contains("acme"));
    }

    #[test]
    fn no_telemetry_capability_means_no_findings() {
        let mut sub = TestSubstrate::new();
        sub.add_fact("acme", "tier", "gold");
        // No telemetry injected → capability off → degrade to nothing.
        let drafts = sub.analyze_with(&ColdGrains::new(), 10_000_000, &[("min_age_days", json!(0))]);
        assert!(drafts.is_empty());
    }

    #[test]
    fn young_grain_is_not_cold() {
        let mut sub = TestSubstrate::new();
        sub.add_fact("acme", "tier", "gold");
        sub.telemetry_budget(1, 0); // turn telemetry on without recalling the grain
        // Default 30-day grace: a just-created grain is too new to be cold.
        let drafts = sub.analyze(&ColdGrains::new(), 10_000);
        assert!(drafts.is_empty());
    }
}
