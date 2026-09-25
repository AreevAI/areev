//! Duplicate sweep (T0/T1). Exact triple duplicates (NFC + case-fold) among
//! Facts, and near-duplicate Observations by token-set Jaccard. Consolidation
//! keeps the earliest member canonical and supersedes the rest — structural,
//! non-destructive. (Exact duplicates are auto-apply *eligible*; near-dups fail
//! the engine's exact-equality shape check and stay pending — §6.3.)
//!
//! **With a decision backend** (`Engine::with_decider`, proposal row E2):
//! observation pairs the Jaccard rule cannot decide — similarity in
//! `[JUDGED_JACCARD_FLOOR, jaccard)`, same namespace, neither already
//! clustered — are asked "do these two state the same claim?" in batches, at
//! most `pair_cap` pairs per run. A CALIBRATED `p ≥ DECIDE_MIN_P` proposes
//! the same supersede draft the Jaccard path does, with the probability on
//! its `judged_by` record; an uncalibrated backend proposes nothing (a rank
//! means nothing here), and a failed request contributes nothing.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::decide::{Ask, DECIDE_MIN_P, QUESTIONS_PER_REQUEST};
use crate::analyzers::bound_evidence;
use crate::cal;
use crate::error::Result;
use crate::manifest::*;
use crate::model::{normalize_ident, ActionKind, GrainRecord, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The lowest token-set Jaccard at which a pair is worth asking a decision
/// backend about. Below it two observations share too little wording for a
/// "same claim" to be the likely reading, and asking would spend the pair cap
/// on noise.
pub const JUDGED_JACCARD_FLOOR: f64 = 0.5;

pub struct DuplicateSweep {
    manifest: AnalyzerManifest,
}

impl DuplicateSweep {
    pub fn new() -> Self {
        DuplicateSweep {
            manifest: AnalyzerManifest {
                id: "loop.duplicate_sweep/1".into(),
                title: "Duplicate sweep".into(),
                description: "Consolidates exact-duplicate facts and near-duplicate observations."
                    .into(),
                tier: Tier::T1,
                cadence: CadenceClass::Batch,
                requires: vec![],
                target_classes: vec![TargetClass::Memory],
                auto_apply: AutoApplyClass::StructuralCuration,
                trust_class: TrustClass::Builtin,
                params: vec![ParamSpec::Float {
                    name: "jaccard".into(),
                    default: 0.9,
                    min: 0.5,
                    max: 1.0,
                    description: "Near-duplicate token-set Jaccard threshold.".into(),
                }],
                default_on: true,
            },
        }
    }
}

impl Default for DuplicateSweep {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for DuplicateSweep {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let mut drafts = self.exact_facts(ctx)?;
        drafts.extend(self.near_observations(ctx)?);
        drafts.sort_by(|a, b| a.target_ref.cmp(&b.target_ref));
        Ok(drafts)
    }
}

impl DuplicateSweep {
    fn exact_facts(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let facts = ctx.facts()?;
        // key = (ns, subject, relation, object) normalized.
        let mut groups: BTreeMap<(String, String, String, String), Vec<GrainRecord>> =
            BTreeMap::new();
        for f in facts {
            let (Some(s), Some(r), Some(o)) =
                (f.fact_subject(), f.fact_relation(), f.fact_object())
            else {
                continue;
            };
            let key = (
                normalize_ident(&f.namespace),
                normalize_ident(s),
                normalize_ident(r),
                normalize_ident(o),
            );
            groups.entry(key).or_default().push(f);
        }

        let mut drafts = Vec::new();
        for ((_, subject, _, _), mut members) in groups {
            if members.len() < 2 {
                continue;
            }
            // Canonical = earliest; supersede the rest.
            members.sort_by(|a, b| {
                a.created_at_ms
                    .cmp(&b.created_at_ms)
                    .then(a.hash.cmp(&b.hash))
            });
            let canonical = members[0].clone();
            let mut canonical_fields = Map::new();
            canonical_fields.insert(
                "subject".into(),
                json!(canonical.fact_subject().unwrap_or("")),
            );
            canonical_fields.insert(
                "relation".into(),
                json!(canonical.fact_relation().unwrap_or("")),
            );
            canonical_fields.insert(
                "object".into(),
                json!(canonical.fact_object().unwrap_or("")),
            );
            // The replacement must carry the original's namespace — a
            // supersession builds a NEW grain from exactly these fields, so an
            // absent namespace would silently move the fact to the store
            // default namespace and out of every ns-scoped recall.
            if !canonical.namespace.is_empty() {
                canonical_fields.insert("namespace".into(), json!(canonical.namespace));
            }

            let mut statements = Vec::new();
            for extra in &members[1..] {
                statements.push(cal::supersede(&extra.hash, "fact", &canonical_fields));
            }
            let evidence =
                bound_evidence(members.iter().map(|m| m.hash.clone()).collect::<Vec<_>>());

            let mut args = Map::new();
            args.insert("count".into(), json!(members.len()));
            args.insert("subject".into(), json!(subject));

            // No recurrence metric here (unlike contradiction_sweep): a
            // supersession creates a NEW replacement grain, so post-apply the
            // canonical + its copy both stay live and a live-grain count
            // never drops — a grain-count metric would read "regressed" the
            // moment it was applied. Head-based recall (`latest`) already
            // returns one value; measuring duplicate recurrence honestly
            // needs a supersede-by-existing substrate primitive first.
            drafts.push(
                RecDraft::new(
                    format!(
                        "entity:{}/{}",
                        normalize_ident(&canonical.namespace),
                        subject
                    ),
                    ActionKind::Consolidate,
                    Summary::new("duplicate.exact", args),
                    Proposal::Cal {
                        cal: cal::batch(&statements),
                    },
                )
                .severity(Severity::Low)
                .evidence(evidence),
            );
        }
        Ok(drafts)
    }

    fn near_observations(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let threshold = ctx.params().get_float("jaccard");
        let obs = ctx.observations()?;

        // Greedy clustering within a namespace by token-set Jaccard.
        let mut tokenized: Vec<(GrainRecord, std::collections::BTreeSet<String>)> = obs
            .into_iter()
            .filter_map(|o| {
                let tokens = tokenize(obs_text(&o)?);
                Some((o, tokens))
            })
            .collect();
        tokenized.sort_by(|a, b| {
            a.0.created_at_ms
                .cmp(&b.0.created_at_ms)
                .then(a.0.hash.cmp(&b.0.hash))
        });

        let mut used = vec![false; tokenized.len()];
        let mut drafts = Vec::new();
        for i in 0..tokenized.len() {
            if used[i] {
                continue;
            }
            let mut cluster = vec![i];
            for j in (i + 1)..tokenized.len() {
                if used[j] || tokenized[i].0.namespace != tokenized[j].0.namespace {
                    continue;
                }
                if jaccard(&tokenized[i].1, &tokenized[j].1) >= threshold {
                    used[j] = true;
                    cluster.push(j);
                }
            }
            if cluster.len() < 2 {
                continue;
            }
            used[i] = true;
            let mut args = Map::new();
            args.insert("count".into(), json!(cluster.len()));
            args.insert("threshold".into(), json!(threshold));
            drafts.push(consolidate_draft(&tokenized, &cluster, Summary::new("duplicate.near", args)));
        }
        drafts.extend(judged_pairs(ctx, &tokenized, &mut used, threshold));
        Ok(drafts)
    }
}

/// The supersede-into-the-earliest draft over one cluster (indices into
/// `tokenized`, earliest first) — shared by the Jaccard and judged paths so
/// both propose exactly the same change.
fn consolidate_draft(
    tokenized: &[(GrainRecord, std::collections::BTreeSet<String>)],
    cluster: &[usize],
    summary: Summary,
) -> RecDraft {
    let canonical = &tokenized[cluster[0]].0;
    let mut canonical_fields = Map::new();
    canonical_fields.insert("body".into(), json!(obs_text(canonical).unwrap_or("")));
    // Keep the cluster's namespace on the replacement (clusters never cross
    // namespaces — both paths filter on it).
    if !canonical.namespace.is_empty() {
        canonical_fields.insert("namespace".into(), json!(canonical.namespace));
    }
    let mut statements = Vec::new();
    for &k in &cluster[1..] {
        statements.push(cal::supersede(&tokenized[k].0.hash, "observation", &canonical_fields));
    }
    let evidence = bound_evidence(cluster.iter().map(|&k| tokenized[k].0.hash.clone()).collect());
    RecDraft::new(
        format!("grain:{}", canonical.hash),
        ActionKind::Consolidate,
        summary,
        Proposal::Cal {
            cal: cal::batch(&statements),
        },
    )
    .severity(Severity::Info)
    .evidence(evidence)
}

/// E2: the pairs the Jaccard rule left undecided, asked of a CALIBRATED
/// decision backend. Pairs are taken in (earlier, later) creation order, so
/// the cap cuts deterministically. A judged pair whose members a previous
/// judged pair already consumed is skipped (greedy, like the Jaccard path).
fn judged_pairs(
    ctx: &AnalyzeCtx,
    tokenized: &[(GrainRecord, std::collections::BTreeSet<String>)],
    used: &mut [bool],
    threshold: f64,
) -> Vec<RecDraft> {
    let Some(d) = ctx.decider() else {
        return Vec::new();
    };
    if !d.calibrated() {
        return Vec::new();
    }
    let mut pairs: Vec<(usize, usize, f64)> = Vec::new();
    'outer: for i in 0..tokenized.len() {
        if used[i] {
            continue;
        }
        for j in (i + 1)..tokenized.len() {
            if used[j] || tokenized[i].0.namespace != tokenized[j].0.namespace {
                continue;
            }
            let sim = jaccard(&tokenized[i].1, &tokenized[j].1);
            if (JUDGED_JACCARD_FLOOR..threshold).contains(&sim) {
                if pairs.len() >= d.pair_cap() {
                    break 'outer;
                }
                pairs.push((i, j, sim));
            }
        }
    }
    let backend = d.describe();
    let mut drafts = Vec::new();
    for chunk in pairs.chunks(QUESTIONS_PER_REQUEST) {
        let mut state = Map::new();
        let mut asks = Vec::new();
        for (n, &(i, j, _)) in chunk.iter().enumerate() {
            let id = format!("p{n}");
            state.insert(
                id.clone(),
                json!({"a": obs_text(&tokenized[i].0).unwrap_or(""), "b": obs_text(&tokenized[j].0).unwrap_or("")}),
            );
            asks.push(Ask::Noul {
                instructions: format!(
                    "Do texts \"a\" and \"b\" of pair \"{id}\" (in state.pairs) state the same claim?"
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
        for (n, &(i, j, sim)) in chunk.iter().enumerate() {
            let id = format!("p{n}");
            let Some(&p) = a.noul.get(&id) else { continue };
            if p < DECIDE_MIN_P || used[i] || used[j] {
                continue;
            }
            used[i] = true;
            used[j] = true;
            let mut args = Map::new();
            args.insert("count".into(), json!(2));
            args.insert("p".into(), json!(round3(p)));
            args.insert("similarity".into(), json!(round3(sim)));
            let judged = a.judged_by(&backend, "duplicate", BTreeMap::from([("same_claim".to_string(), p)]));
            drafts.push(
                consolidate_draft(tokenized, &[i, j], Summary::new("duplicate.judged", args))
                    .judged_by(judged),
            );
        }
    }
    drafts
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn obs_text(o: &GrainRecord) -> Option<&str> {
    o.str_field("body")
        .or_else(|| o.str_field("content"))
        .or_else(|| o.str_field("text"))
}

pub(crate) fn tokenize(text: &str) -> std::collections::BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

pub(crate) fn jaccard(a: &std::collections::BTreeSet<String>, b: &std::collections::BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    #[test]
    fn consolidates_exact_duplicate_facts() {
        let mut sub = TestSubstrate::new();
        sub.add_fact("caller", "tier", "Enterprise");
        sub.add_fact("caller", "tier", "enterprise"); // case variant → same
        sub.add_fact("caller", "tier", "Enterprise");
        let drafts = sub.analyze(&DuplicateSweep::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].action_kind, ActionKind::Consolidate);
        assert_eq!(drafts[0].evidence.len(), 3);
    }

    #[test]
    fn near_duplicate_observations_cluster() {
        let mut sub = TestSubstrate::new();
        sub.add_observation(
            "caller",
            "user asked about pricing tiers refunds billing invoices today",
        );
        sub.add_observation(
            "caller",
            "user asked about pricing tiers refunds billing invoices today please",
        );
        // Superset differs by one token of eleven → Jaccard ≈ 0.91 ≥ 0.9.
        let drafts = sub.analyze(&DuplicateSweep::new(), 10_000);
        assert_eq!(drafts.len(), 1, "the two near-dup observations cluster");
    }

    #[test]
    fn distinct_facts_are_left_alone() {
        let mut sub = TestSubstrate::new();
        sub.add_fact("caller", "tier", "Enterprise");
        sub.add_fact("caller", "tier", "Free");
        assert!(sub.analyze(&DuplicateSweep::new(), 10_000).is_empty());
    }
}
