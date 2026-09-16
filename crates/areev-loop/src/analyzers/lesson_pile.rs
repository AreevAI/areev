//! Lesson pile (T0, default-off) — an active-lesson budget per entity.
//!
//! Every approved lesson is one more rule in the agent's prompt, forever, and
//! the loop's own evidence says that trajectory ends in a worse agent than no
//! lessons at all: on the ad-buy corpus (`crates/areev-bench/ADBUY.md`, seed
//! 3) ten approved rules stated four distinct facts, each true and approvable
//! alone, and the agent stopped emitting the very fields the rules most
//! insistently named. Dream-RSI (arXiv 2609.14858 §5.1) measured the same
//! shape: prompt guidance distilled from history underperformed no guidance
//! under equal budgets. A reviewer judging one card at a time cannot see the
//! pile; this analyzer counts it.
//!
//! Above `max_active` live lessons on one entity it emits an advisory Flag
//! listing the pile with each member's latest Verify-gate verdict, so a
//! reviewer can retire the ones that measured `regressed` or `drifted`. With
//! an LLM attached the finding is also DISCOVER's cue to draft ONE
//! consolidating lesson (`kind: consolidation`) through the ordinary
//! GROUND → VERIFY path — gate-judged, human-applied, never auto-applied.
//! Default-off: a budget is a policy a host states, not one the engine
//! infers.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, GrainRecord, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};
use std::collections::BTreeMap;

pub struct LessonPile {
    manifest: AnalyzerManifest,
}

impl LessonPile {
    pub fn new() -> Self {
        LessonPile {
            manifest: AnalyzerManifest {
                id: "loop.lesson_pile/1".into(),
                title: "Lesson pile".into(),
                description: "Flags an entity carrying more live lessons than its budget, \
                              with each member's latest outcome verdict, and cues one \
                              consolidating lesson when a model is attached."
                    .into(),
                tier: Tier::T0,
                cadence: CadenceClass::Slow,
                requires: vec![],
                target_classes: vec![TargetClass::Memory],
                auto_apply: AutoApplyClass::Never,
                trust_class: TrustClass::Builtin,
                params: vec![ParamSpec::Int {
                    name: "max_active".into(),
                    default: 8,
                    min: 1,
                    max: 10_000,
                    description: "Live lessons one entity may carry before the pile is \
                                  flagged for consolidation."
                        .into(),
                }],
                default_on: false,
            },
        }
    }
}

impl Default for LessonPile {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for LessonPile {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let max_active = ctx.params().get_int("max_active").max(1) as usize;
        // Live only: a retracted or superseded lesson is not in the prompt
        // and does not count. Grouped per entity — (namespace, subject) —
        // because that is the unit a prompt renders lessons for.
        let mut piles: BTreeMap<(String, String), Vec<GrainRecord>> = BTreeMap::new();
        for f in ctx.facts()? {
            if f.fact_relation() != Some("lesson") {
                continue;
            }
            let Some(subject) = f.fact_subject() else { continue };
            piles
                .entry((f.namespace.clone(), subject.to_string()))
                .or_default()
                .push(f);
        }
        let mut drafts = Vec::new();
        for ((ns, subject), mut members) in piles {
            if members.len() <= max_active {
                continue;
            }
            members.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms).then(a.hash.cmp(&b.hash)));
            let hashes: Vec<String> = members.iter().map(|m| m.hash.clone()).collect();
            let mut verdicts = Map::new();
            let rendered: Vec<String> = members
                .iter()
                .map(|m| {
                    let v = ctx.verdict_for(&m.hash).unwrap_or("not applied by the loop");
                    verdicts.insert(m.hash.clone(), json!(v));
                    format!("{}… ({v})", m.hash.chars().take(8).collect::<String>())
                })
                .collect();
            let target = if ns.is_empty() {
                format!("entity:{subject}")
            } else {
                format!("entity:{ns}/{subject}")
            };
            let mut args = Map::new();
            args.insert("count".into(), json!(members.len()));
            args.insert("subject".into(), json!(subject));
            args.insert("max_active".into(), json!(max_active));
            args.insert("members".into(), json!(rendered.join(", ")));
            let mut data = Map::new();
            data.insert("pile".into(), json!(hashes));
            data.insert("verdicts".into(), json!(verdicts));
            data.insert("max_active".into(), json!(max_active));
            drafts.push(
                RecDraft::new(target, ActionKind::Flag, Summary::new("lesson.pile", args), Proposal::Data { data })
                    .severity(Severity::Medium)
                    .evidence(hashes),
            );
        }
        Ok(drafts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    fn lessons(sub: &mut TestSubstrate, subject: &str, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| sub.add_fact(subject, "lesson", &format!("Rule number {i} for {subject}.")))
            .collect()
    }

    #[test]
    fn manifest_pinned_default_off() {
        let a = LessonPile::new();
        assert_eq!(a.manifest().id, "loop.lesson_pile/1");
        assert!(!a.manifest().default_on, "a budget is stated, never inferred");
        assert_eq!(a.manifest().auto_apply, AutoApplyClass::Never);
    }

    /// Nine live lessons on one entity under a budget of eight → one finding
    /// citing all nine, each with its latest verdict; eight → nothing.
    #[test]
    fn flags_a_pile_over_budget_with_each_members_verdict() {
        let mut sub = TestSubstrate::new();
        let nine = lessons(&mut sub, "capture", 9);
        sub.set_verdict(&nine[0], "held");
        sub.set_verdict(&nine[3], "regressed");
        let drafts = sub.analyze(&LessonPile::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        let d = &drafts[0];
        assert_eq!(d.action_kind, ActionKind::Flag);
        assert_eq!(d.evidence, nine, "cites every member, oldest first");
        let text = d.summary.render();
        assert!(text.contains("9 live lessons") && text.contains("budget of 8"), "{text}");
        assert!(text.contains("(held)") && text.contains("(regressed)") && text.contains("(not applied by the loop)"), "{text}");
        match &d.proposal {
            Proposal::Data { data } => {
                assert_eq!(data["pile"].as_array().unwrap().len(), 9);
                assert_eq!(data["verdicts"][&nine[3]], "regressed");
            }
            other => panic!("advisory data expected, got {other:?}"),
        }

        let mut sub = TestSubstrate::new();
        lessons(&mut sub, "capture", 8);
        assert!(sub.analyze(&LessonPile::new(), 10_000).is_empty(), "eight is within budget");
    }

    /// A retracted lesson is not in the prompt and does not count.
    #[test]
    fn a_retracted_lesson_does_not_count() {
        use crate::substrate::OmsSubstrate;
        let mut sub = TestSubstrate::new();
        let nine = lessons(&mut sub, "capture", 9);
        sub.inner.retract(&nine[8], "reviewer retired it").unwrap();
        assert!(sub.analyze(&LessonPile::new(), 10_000).is_empty());
        // And the budget is a parameter.
        let drafts = sub.analyze_with(&LessonPile::new(), 10_000, &[("max_active", json!(3))]);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].evidence.len(), 8);
    }

    /// Piles are per entity: two entities under budget are not one pile.
    #[test]
    fn piles_are_per_entity() {
        let mut sub = TestSubstrate::new();
        lessons(&mut sub, "capture", 5);
        lessons(&mut sub, "refund", 5);
        assert!(sub.analyze(&LessonPile::new(), 10_000).is_empty());
    }
}
