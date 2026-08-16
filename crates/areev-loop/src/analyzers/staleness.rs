//! Staleness (T0): grains whose declared `valid_to` has elapsed. The honest
//! framing — "expiry you declared" — only; the soft never-recalled tier is
//! deferred (§8). One recommendation per grain (single-grain FORGET), so each
//! dedups on its own target.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::cal;
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};

pub struct Staleness {
    manifest: AnalyzerManifest,
}

impl Staleness {
    pub fn new() -> Self {
        Staleness {
            manifest: AnalyzerManifest {
                id: "loop.staleness/1".into(),
                title: "Staleness".into(),
                description: "Proposes tombstoning grains past their declared valid_to.".into(),
                tier: Tier::T0,
                cadence: CadenceClass::Fast,
                requires: vec![],
                target_classes: vec![TargetClass::Memory],
                // FORGET has no inverse: never auto-apply (also destructive-gated).
                auto_apply: AutoApplyClass::Never,
                trust_class: TrustClass::Builtin,
                params: vec![ParamSpec::Int {
                    name: "grace_days".into(),
                    default: 0,
                    min: 0,
                    max: 3650,
                    description: "Days past valid_to before proposing expiry.".into(),
                }],
                default_on: true,
            },
        }
    }
}

impl Default for Staleness {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for Staleness {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let grace_ms = ctx.params().get_int("grace_days") * 86_400_000;
        let cutoff = ctx.now_ms() - grace_ms;

        // valid_to may appear on facts or observations.
        let mut grains = ctx.facts()?;
        grains.extend(ctx.observations()?);

        let mut drafts = Vec::new();
        for g in grains {
            let Some(valid_to) = g.valid_to_ms else {
                continue;
            };
            if valid_to >= cutoff {
                continue;
            }
            let age_days = ((ctx.now_ms() - valid_to).max(0)) / 86_400_000;
            let subject = g
                .fact_subject()
                .map(|s| s.to_string())
                .unwrap_or_else(|| short_hash(&g.hash));

            let mut args = Map::new();
            args.insert("subject".into(), json!(subject));
            args.insert("age_days".into(), json!(age_days));

            drafts.push(
                RecDraft::new(
                    format!("grain:{}", g.hash),
                    ActionKind::Expire,
                    Summary::new("staleness.expired", args),
                    Proposal::Cal {
                        cal: cal::forget(&g.hash),
                    },
                )
                .severity(if age_days > 90 {
                    Severity::Medium
                } else {
                    Severity::Low
                })
                .evidence(vec![g.hash.clone()]),
            );
        }
        // Deterministic ordering by target.
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
    fn proposes_expiry_for_elapsed_valid_to() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_valid_to("caller", "promo", "active", 1_000); // expired at t=1000
        sub.add_fact("caller", "name", "John"); // no valid_to
        let drafts = sub.analyze(&Staleness::new(), 10_000);
        assert_eq!(drafts.len(), 1, "only the elapsed grain");
        assert_eq!(drafts[0].action_kind, ActionKind::Expire);
        assert!(matches!(&drafts[0].proposal, Proposal::Cal { cal } if cal.starts_with("FORGET")));
    }

    #[test]
    fn respects_grace_period() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_valid_to("caller", "promo", "active", 9_000);
        // grace_days default 0 → expired; set grace to keep it alive.
        let drafts = sub.analyze_with(
            &Staleness::new(),
            10_000,
            &[("grace_days", serde_json::json!(1))],
        );
        assert!(drafts.is_empty(), "within grace window");
    }
}
