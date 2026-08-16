//! Duplicate sweep (T0/T1). Exact triple duplicates (NFC + case-fold) among
//! Facts, and near-duplicate Observations by token-set Jaccard. Consolidation
//! keeps the earliest member canonical and supersedes the rest — structural,
//! non-destructive. (Exact duplicates are auto-apply *eligible*; near-dups fail
//! the engine's exact-equality shape check and stay pending — §6.3.)

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::analyzers::bound_evidence;
use crate::cal;
use crate::error::Result;
use crate::manifest::*;
use crate::model::{normalize_ident, ActionKind, GrainRecord, Severity};
use crate::recommendation::{Proposal, RecDraft, Summary};
use serde_json::{json, Map};
use std::collections::BTreeMap;

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
            let canonical = &tokenized[cluster[0]].0;
            let mut canonical_fields = Map::new();
            canonical_fields.insert("body".into(), json!(obs_text(canonical).unwrap_or("")));
            // Keep the cluster's namespace on the replacement (clusters never
            // cross namespaces — see the filter above).
            if !canonical.namespace.is_empty() {
                canonical_fields.insert("namespace".into(), json!(canonical.namespace));
            }

            let mut statements = Vec::new();
            for &k in &cluster[1..] {
                statements.push(cal::supersede(
                    &tokenized[k].0.hash,
                    "observation",
                    &canonical_fields,
                ));
            }
            let evidence = bound_evidence(
                cluster
                    .iter()
                    .map(|&k| tokenized[k].0.hash.clone())
                    .collect(),
            );

            let mut args = Map::new();
            args.insert("count".into(), json!(cluster.len()));
            args.insert("threshold".into(), json!(threshold));

            drafts.push(
                RecDraft::new(
                    format!("grain:{}", canonical.hash),
                    ActionKind::Consolidate,
                    Summary::new("duplicate.near", args),
                    Proposal::Cal {
                        cal: cal::batch(&statements),
                    },
                )
                .severity(Severity::Info)
                .evidence(evidence),
            );
        }
        Ok(drafts)
    }
}

fn obs_text(o: &GrainRecord) -> Option<&str> {
    o.str_field("body")
        .or_else(|| o.str_field("content"))
        .or_else(|| o.str_field("text"))
}

fn tokenize(text: &str) -> std::collections::BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn jaccard(a: &std::collections::BTreeSet<String>, b: &std::collections::BTreeSet<String>) -> f64 {
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
