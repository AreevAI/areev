//! Contradiction sweep (T0). Flags subjects holding two or more live objects
//! under a *functional* relation (one that should be single-valued). Ships with
//! a seeded functional-relation list so it fires on day one; the from-file
//! learner (single-valued for ≥80% of subjects) is deferred. Resolving a
//! contradiction is a judgment call, so it never auto-applies.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::analyzers::bound_evidence;
use crate::cal;
use crate::error::Result;
use crate::manifest::*;
use crate::model::{normalize_ident, ActionKind, GrainRecord, Severity};
use crate::recommendation::{MetricSnapshot, Proposal, RecDraft, Summary};
use serde_json::{json, Map};
use std::collections::BTreeMap;

/// Relations that are single-valued by convention (a subset of the built-in
/// `mg:` vocabulary plus common agent relations).
const SEEDED_FUNCTIONAL: &[&str] = &[
    "deploy_target",
    "lives_in",
    "reports_to",
    "status",
    "tier",
    "owner",
    "region",
    "assigned_to",
    "primary_email",
    "current_plan",
];

pub struct ContradictionSweep {
    manifest: AnalyzerManifest,
}

impl ContradictionSweep {
    pub fn new() -> Self {
        ContradictionSweep {
            manifest: AnalyzerManifest {
                id: "loop.contradiction_sweep/1".into(),
                title: "Contradiction sweep".into(),
                description: "Flags conflicting live values under functional relations.".into(),
                tier: Tier::T0,
                cadence: CadenceClass::Fast,
                requires: vec![],
                target_classes: vec![TargetClass::Memory],
                auto_apply: AutoApplyClass::Never,
                trust_class: TrustClass::Builtin,
                params: vec![ParamSpec::Str {
                    name: "extra_relations".into(),
                    default: String::new(),
                    max_len: 2000,
                    description: "Additional functional (single-valued) relations to check, \
                                  comma-separated — e.g. a healthcare deployment adds \
                                  \"insurance_plan,prior_auth,next_appt\"."
                        .into(),
                }],
                default_on: true,
            },
        }
    }
}

impl Default for ContradictionSweep {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for ContradictionSweep {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        // Seeded functional relations + any host-supplied domain relations.
        let mut functional: std::collections::BTreeSet<String> =
            SEEDED_FUNCTIONAL.iter().map(|s| s.to_string()).collect();
        for r in ctx.params().get_str("extra_relations").split(',') {
            let r = normalize_ident(r);
            if !r.is_empty() {
                functional.insert(r);
            }
        }

        let facts = ctx.facts()?;
        // (ns, subject, relation) → live facts, only for functional relations.
        let mut groups: BTreeMap<(String, String, String), Vec<GrainRecord>> = BTreeMap::new();
        for f in facts {
            let (Some(s), Some(r)) = (f.fact_subject(), f.fact_relation()) else {
                continue;
            };
            if !functional.contains(&normalize_ident(r)) {
                continue;
            }
            let key = (
                normalize_ident(&f.namespace),
                normalize_ident(s),
                normalize_ident(r),
            );
            groups.entry(key).or_default().push(f);
        }

        let mut drafts = Vec::new();
        for ((ns, subject, relation), mut members) in groups {
            // Distinct live objects?
            let distinct: std::collections::BTreeSet<String> = members
                .iter()
                .filter_map(|m| m.fact_object().map(normalize_ident))
                .collect();
            if distinct.len() < 2 {
                continue;
            }
            // Resolve-to-latest: keep the newest, supersede the older values.
            members.sort_by(|a, b| {
                a.created_at_ms
                    .cmp(&b.created_at_ms)
                    .then(a.hash.cmp(&b.hash))
            });
            let latest = members.last().unwrap().clone();
            let mut latest_fields = Map::new();
            latest_fields.insert("subject".into(), json!(latest.fact_subject().unwrap_or("")));
            latest_fields.insert(
                "relation".into(),
                json!(latest.fact_relation().unwrap_or("")),
            );
            latest_fields.insert("object".into(), json!(latest.fact_object().unwrap_or("")));
            // The resolution supersedes older values with a NEW grain built
            // from these fields — carry the namespace or the winning value
            // would migrate to the store default namespace.
            if !latest.namespace.is_empty() {
                latest_fields.insert("namespace".into(), json!(latest.namespace));
            }

            let mut statements = Vec::new();
            for older in &members[..members.len() - 1] {
                statements.push(cal::supersede(&older.hash, "fact", &latest_fields));
            }
            let evidence = bound_evidence(members.iter().map(|m| m.hash.clone()).collect());

            let mut args = Map::new();
            args.insert("subject".into(), json!(subject));
            args.insert("relation".into(), json!(relation));
            args.insert("count".into(), json!(distinct.len()));

            drafts.push(
                RecDraft::new(
                    format!("entity:{ns}/{subject}"),
                    ActionKind::FlagContradiction,
                    Summary::new("contradiction.functional", args),
                    Proposal::Cal {
                        cal: cal::batch(&statements),
                    },
                )
                .severity(Severity::Medium)
                .evidence(evidence)
                .metric(MetricSnapshot {
                    // After resolving to the latest value, does the subject
                    // again hold ≥2 live values under this functional
                    // relation? Baseline 0 = one live value; any excess at a
                    // checkpoint is a regression → outcome review proposes a
                    // revert for human judgment.
                    metric: "contradiction_recurrence".into(),
                    baseline: 0.0,
                    unit: "count".into(),
                    n: members.len() as u64,
                    window: "live".into(),
                    subject: Some(subject.clone()),
                    namespace: (!ns.is_empty()).then(|| ns.clone()),
                    relation: Some(relation.clone()),
                    query: format!(
                        "RECALL facts WHERE subject = \"{subject}\" AND relation = \"{relation}\" | COUNT DISTINCT object > 1"
                    ),
                    review_after_ms: 86_400_000,
                    horizons_ms: vec![86_400_000, 7 * 86_400_000, 30 * 86_400_000],
                }),
            );
        }
        drafts.sort_by(|a, b| a.target_ref.cmp(&b.target_ref));
        Ok(drafts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    #[test]
    fn flags_two_live_deploy_targets() {
        let mut sub = TestSubstrate::new();
        sub.add_fact("acme", "deploy_target", "us-east-1");
        sub.add_fact("acme", "deploy_target", "eu-west-1");
        let drafts = sub.analyze(&ContradictionSweep::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].action_kind, ActionKind::FlagContradiction);
    }

    #[test]
    fn extra_relations_extend_the_functional_set() {
        let mut sub = TestSubstrate::new();
        sub.add_fact("bob", "insurance_plan", "aetna");
        sub.add_fact("bob", "insurance_plan", "cigna"); // not in the seeded list
        // Without the param, insurance_plan isn't treated as functional.
        assert!(sub.analyze(&ContradictionSweep::new(), 10_000).is_empty());
        // A healthcare deployment adds it.
        let drafts = sub.analyze_with(
            &ContradictionSweep::new(),
            10_000,
            &[("extra_relations", serde_json::json!("insurance_plan,prior_auth"))],
        );
        assert_eq!(drafts.len(), 1, "the custom functional relation is now checked");
    }

    #[test]
    fn ignores_non_functional_relations() {
        let mut sub = TestSubstrate::new();
        sub.add_fact("acme", "likes", "pizza");
        sub.add_fact("acme", "likes", "sushi"); // multi-valued relation — fine
        assert!(sub.analyze(&ContradictionSweep::new(), 10_000).is_empty());
    }
}
