//! Skill stall (T0) — the on-theme analyzer for a "self-improving agents"
//! product. A Skill grain (0x0B) carries `proficiency` (aliases `confidence`)
//! and `practice_count`. A skill practiced many times whose proficiency stays
//! low is one the agent keeps *doing* but isn't getting *better* at — a
//! genuine "stop and rethink the strategy" signal, computed from the grain's
//! own fields (no chain traversal, no telemetry). Advisory only: it surfaces
//! the skill for human attention; there is no automatic fix, so it never
//! auto-applies.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};

pub struct SkillStall {
    manifest: AnalyzerManifest,
}

impl SkillStall {
    pub fn new() -> Self {
        SkillStall {
            manifest: AnalyzerManifest {
                id: "loop.skill_stall/1".into(),
                title: "Skill stall".into(),
                description: "Flags skills practiced repeatedly without proficiency gain.".into(),
                tier: Tier::T0,
                cadence: CadenceClass::Fast,
                requires: vec![],
                target_classes: vec![TargetClass::Memory],
                auto_apply: AutoApplyClass::Never, // advisory; no automatic fix
                trust_class: TrustClass::Builtin,
                params: vec![
                    ParamSpec::Int {
                        name: "min_practice".into(),
                        default: 5,
                        min: 1,
                        max: 100_000,
                        description: "Minimum practice_count before a low proficiency counts as a stall."
                            .into(),
                    },
                    ParamSpec::Float {
                        name: "max_proficiency".into(),
                        default: 0.4,
                        min: 0.0,
                        max: 1.0,
                        description: "Proficiency at or below this (despite practice) is a stall.".into(),
                    },
                ],
                default_on: true,
            },
        }
    }
}

impl Default for SkillStall {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for SkillStall {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let min_practice = ctx.params().get_int("min_practice");
        let max_proficiency = ctx.params().get_float("max_proficiency");

        let mut drafts = Vec::new();
        for s in ctx.skills()? {
            let practice = s.skill_practice_count();
            let Some(proficiency) = s.skill_proficiency() else { continue };
            if practice < min_practice || proficiency > max_proficiency {
                continue;
            }
            let name = s.skill_name().unwrap_or("").to_string();

            let mut args = Map::new();
            args.insert("skill".into(), json!(name));
            args.insert("practice_count".into(), json!(practice));
            args.insert("proficiency".into(), json!(round2(proficiency)));

            let mut data = Map::new();
            data.insert("skill".into(), json!(name));
            data.insert("proficiency".into(), json!(round2(proficiency)));
            data.insert("practice_count".into(), json!(practice));

            drafts.push(
                RecDraft::new(
                    format!("entity:skills/{name}"),
                    ActionKind::Flag,
                    Summary::new("skill.stall", args),
                    Proposal::Data { data },
                )
                .severity(Severity::Medium)
                .evidence(vec![s.hash.clone()]),
            );
        }
        drafts.sort_by(|a, b| a.target_ref.cmp(&b.target_ref));
        Ok(drafts)
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    #[test]
    fn flags_practiced_but_unimproved_skill() {
        let mut sub = TestSubstrate::new();
        sub.add_skill("parse_invoices", 0.25, 12); // practiced a lot, still bad
        sub.add_skill("write_sql", 0.9, 8); // proficient — fine
        sub.add_skill("new_thing", 0.2, 1); // barely practiced — not yet a stall
        let drafts = sub.analyze(&SkillStall::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].action_kind, ActionKind::Flag);
        assert!(drafts[0].summary.render().contains("parse_invoices"));
    }
}
