//! Contradiction sweep (T0). Flags subjects holding two or more live objects
//! under a *functional* relation (one that should be single-valued). Ships with
//! a seeded functional-relation list so it fires on day one; the from-file
//! learner (single-valued for ≥80% of subjects) is deferred. Resolving a
//! contradiction is a judgment call, so it never auto-applies.
//!
//! **With a decision backend** (`Engine::with_decider`, proposal row E2): for
//! every (namespace, subject, relation) OUTSIDE the functional set holding
//! two or more distinct live objects, each pair of distinct values is asked
//! "can both of these be true at the same time?" — batched, at most
//! `pair_cap` pairs per run. When a CALIBRATED `1 − p ≥ DECIDE_MIN_P` for any
//! pair, the same supersede-older draft is proposed over the values in the
//! conflicting pairs (the newest of them wins), under the
//! `contradiction.judged` summary that says the relation was not seeded and
//! with the probabilities on `judged_by`. It carries no recurrence metric: a
//! relation nobody declared single-valued may legitimately gain values later,
//! and counting them would read as a regression. Uncalibrated → no proposals;
//! a failed request → none from that batch.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::decide::{Ask, DECIDE_MIN_P, QUESTIONS_PER_REQUEST};
use crate::analyzers::bound_evidence;
use crate::cal;
use crate::error::Result;
use crate::manifest::*;
use crate::model::{normalize_ident, ActionKind, GrainRecord, Severity};
use crate::recommendation::{MetricSnapshot, Proposal, RecDraft, Summary};
use serde_json::{json, Map, Value};
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
        // The rest are kept aside for the decision backend (E2), if any.
        let mut groups: BTreeMap<(String, String, String), Vec<GrainRecord>> = BTreeMap::new();
        let mut other: BTreeMap<(String, String, String), Vec<GrainRecord>> = BTreeMap::new();
        for f in facts {
            let (Some(s), Some(r)) = (f.fact_subject(), f.fact_relation()) else {
                continue;
            };
            let key = (
                normalize_ident(&f.namespace),
                normalize_ident(s),
                normalize_ident(r),
            );
            if !functional.contains(&key.2) {
                other.entry(key).or_default().push(f);
                continue;
            }
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
                    checkpoints: Vec::new(),
                    // A count of excess live values: fewer is better.
                    higher_is_better: false,
                }),
            );
        }
        drafts.extend(judged(ctx, other));
        drafts.sort_by(|a, b| a.target_ref.cmp(&b.target_ref));
        Ok(drafts)
    }
}

/// The supersede-older CAL: every member but `latest` is superseded with
/// `latest`'s value (namespace carried, or the winner would migrate to the
/// store default namespace).
fn supersede_older(members: &[GrainRecord], latest: &GrainRecord) -> String {
    let mut latest_fields = Map::new();
    latest_fields.insert("subject".into(), json!(latest.fact_subject().unwrap_or("")));
    latest_fields.insert("relation".into(), json!(latest.fact_relation().unwrap_or("")));
    latest_fields.insert("object".into(), json!(latest.fact_object().unwrap_or("")));
    if !latest.namespace.is_empty() {
        latest_fields.insert("namespace".into(), json!(latest.namespace));
    }
    let statements: Vec<String> = members
        .iter()
        .filter(|m| m.hash != latest.hash)
        .map(|older| cal::supersede(&older.hash, "fact", &latest_fields))
        .collect();
    cal::batch(&statements)
}

fn fact_text(f: &GrainRecord) -> String {
    format!(
        "{} {} {}",
        f.fact_subject().unwrap_or(""),
        f.fact_relation().unwrap_or(""),
        f.fact_object().unwrap_or("")
    )
}

fn oldest_first(a: &GrainRecord, b: &GrainRecord) -> std::cmp::Ordering {
    a.created_at_ms.cmp(&b.created_at_ms).then(a.hash.cmp(&b.hash))
}

/// E2: pairs of distinct values under relations nobody declared functional,
/// asked of a CALIBRATED decision backend.
fn judged(
    ctx: &AnalyzeCtx,
    other: BTreeMap<(String, String, String), Vec<GrainRecord>>,
) -> Vec<RecDraft> {
    let Some(d) = ctx.decider() else {
        return Vec::new();
    };
    if !d.calibrated() {
        return Vec::new();
    }
    // One representative grain per distinct normalized object (the newest),
    // then every unordered pair of them, in deterministic order.
    struct Pair {
        group: usize,
        a: GrainRecord,
        b: GrainRecord,
    }
    let groups: Vec<((String, String, String), Vec<GrainRecord>)> = other.into_iter().collect();
    let mut pairs: Vec<Pair> = Vec::new();
    'groups: for (g, (_, members)) in groups.iter().enumerate() {
        let mut by_object: BTreeMap<String, GrainRecord> = BTreeMap::new();
        for m in members {
            let Some(o) = m.fact_object() else { continue };
            let slot = by_object.entry(normalize_ident(o)).or_insert_with(|| m.clone());
            if oldest_first(slot, m).is_lt() {
                *slot = m.clone();
            }
        }
        if by_object.len() < 2 {
            continue;
        }
        let reps: Vec<&GrainRecord> = by_object.values().collect();
        for i in 0..reps.len() {
            for j in (i + 1)..reps.len() {
                if pairs.len() >= d.pair_cap() {
                    break 'groups;
                }
                pairs.push(Pair { group: g, a: reps[i].clone(), b: reps[j].clone() });
            }
        }
    }
    // group → (conflicting grains by hash, p(cannot both be true) per pair,
    // the provenance of the first answer that found one).
    let backend = d.describe();
    type Conflict = (BTreeMap<String, GrainRecord>, BTreeMap<String, f64>, crate::decide::Answered);
    let mut conflicts: BTreeMap<usize, Conflict> = BTreeMap::new();
    for chunk in pairs.chunks(QUESTIONS_PER_REQUEST) {
        let mut state = Map::new();
        let mut asks = Vec::new();
        for (n, p) in chunk.iter().enumerate() {
            let id = format!("p{n}");
            state.insert(id.clone(), json!({"a": fact_text(&p.a), "b": fact_text(&p.b)}));
            asks.push(Ask::Noul {
                instructions: format!(
                    "Can statements \"a\" and \"b\" of pair \"{id}\" (in state.pairs) both be true at the same time?"
                ),
                id,
            });
        }
        // Fail-soft: a failed or uncalibrated answer contributes nothing.
        let Ok(a) = d.ask(json!({ "pairs": Value::Object(state) }), &asks) else {
            continue;
        };
        if !a.calibrated {
            continue;
        }
        for (n, p) in chunk.iter().enumerate() {
            let Some(&both) = a.noul.get(&format!("p{n}")) else { continue };
            let p_no = 1.0 - both;
            if p_no < DECIDE_MIN_P {
                continue;
            }
            let entry = conflicts
                .entry(p.group)
                .or_insert_with(|| (BTreeMap::new(), BTreeMap::new(), a.clone()));
            entry.0.insert(p.a.hash.clone(), p.a.clone());
            entry.0.insert(p.b.hash.clone(), p.b.clone());
            // Keyed by the pair itself: "not_both:<a>|<b>" → p(cannot both be true).
            entry.1.insert(
                format!(
                    "not_both:{}|{}",
                    p.a.fact_object().unwrap_or(""),
                    p.b.fact_object().unwrap_or("")
                ),
                p_no,
            );
        }
    }

    let mut drafts = Vec::new();
    for (g, (involved, ps, answered)) in conflicts {
        let ((ns, subject, relation), _) = &groups[g];
        let mut members: Vec<GrainRecord> = involved.into_values().collect();
        members.sort_by(oldest_first);
        let latest = members.last().expect("a conflicting pair has two members").clone();
        let evidence = bound_evidence(members.iter().map(|m| m.hash.clone()).collect());
        let p_max = ps.values().copied().fold(0.0_f64, f64::max);
        let mut args = Map::new();
        args.insert("subject".into(), json!(subject));
        args.insert("relation".into(), json!(relation));
        args.insert("count".into(), json!(members.len()));
        args.insert("p".into(), json!((p_max * 1000.0).round() / 1000.0));
        drafts.push(
            RecDraft::new(
                format!("entity:{ns}/{subject}"),
                ActionKind::FlagContradiction,
                Summary::new("contradiction.judged", args),
                Proposal::Cal {
                    cal: supersede_older(&members, &latest),
                },
            )
            .severity(Severity::Medium)
            .evidence(evidence)
            .judged_by(answered.judged_by(&backend, "contradiction", ps)),
        );
    }
    drafts
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
