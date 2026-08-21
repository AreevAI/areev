//! Adapter intake (T0) — the tuning seam's propose leg. `areev tune`
//! registers a host-trained adapter as an `mg:adapter` Fact in
//! `agent:harness` (the registry tuple, its Rule E1 evalset pin, and the
//! corpus-manifest lineage all embedded in the object JSON); this analyzer
//! turns each unpromoted candidate into an `adapter_revision`
//! recommendation. The engine's gates do the rest: apply is refused without
//! a clean recorded run of the pinned evalset, the promotion is an
//! immutable `(model:X, mg:adapter_promotion)` Fact hosts re-resolve from,
//! and rollback retracts it.
//!
//! Lifecycle (deliberate, dedup-shaped): **one candidate per served model**.
//! A subject with a live promotion is skipped entirely — replacing a
//! promoted adapter means rolling the promotion back first, which frees the
//! dedup key; the next pass then proposes the newest unpromoted candidate.
//! A rolled-back candidate is re-proposed while its registry grain stays
//! live ("the situation returned"); retiring the `mg:adapter` grain is how
//! a host silences it.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, GrainRecord, Severity};
use crate::recommendation::{MetricSnapshot, Proposal, RecDraft, Summary};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The namespace `areev tune` registers adapters into.
const HARNESS_NS: &str = "agent:harness";

pub struct AdapterIntake {
    manifest: AnalyzerManifest,
}

impl AdapterIntake {
    pub fn new() -> Self {
        AdapterIntake {
            manifest: AnalyzerManifest {
                id: "loop.adapter_intake/1".into(),
                title: "Adapter intake".into(),
                description:
                    "Proposes promoting host-trained adapters registered by `areev tune` \
                     — one candidate per served model, gated on the pinned evalset, \
                     never auto-applied."
                        .into(),
                tier: Tier::T0,
                cadence: CadenceClass::Slow,
                requires: vec![],
                target_classes: vec![TargetClass::Host],
                auto_apply: AutoApplyClass::Never,
                trust_class: TrustClass::Builtin,
                params: vec![],
                default_on: true,
            },
        }
    }
}

impl Default for AdapterIntake {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for AdapterIntake {
    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let harness = match ctx.grains_in("fact", HARNESS_NS) {
            Ok(rows) => rows,
            // No harness namespace / no read grant: nothing tuned — degrade
            // to nothing, never fabricate.
            Err(_) => return Ok(Vec::new()),
        };
        // Subjects with a live promotion are settled: one candidate per
        // served model, and replacement starts with a rollback (which
        // retracts the promotion Fact, emptying this set for the subject).
        let promoted: BTreeSet<String> = match ctx.grains_in("fact", crate::engine::LOOP_NS) {
            Ok(rows) => rows
                .iter()
                .filter(|g| g.is_live() && g.str_field("relation") == Some("mg:adapter_promotion"))
                .filter_map(|g| g.str_field("subject").map(str::to_string))
                .collect(),
            Err(_) => BTreeSet::new(),
        };

        // Newest live candidate per subject (created_at, hash tiebreak) —
        // the engine's in-pass dedup would otherwise keep whichever came
        // first, which after a rollback is the OLD adapter.
        let mut newest: BTreeMap<String, &GrainRecord> = BTreeMap::new();
        for g in &harness {
            if !g.is_live() || g.str_field("relation") != Some("mg:adapter") {
                continue;
            }
            let Some(subject) = g.str_field("subject") else {
                continue;
            };
            if promoted.contains(subject) {
                continue;
            }
            let newer = match newest.get(subject) {
                Some(cur) => (g.created_at_ms, g.hash.as_str()) > (cur.created_at_ms, cur.hash.as_str()),
                None => true,
            };
            if newer {
                newest.insert(subject.to_string(), g);
            }
        }

        let mut drafts = Vec::new();
        for (subject, grain) in newest {
            // The registry tuple: everything the promotion needs rides in the
            // object JSON `areev tune` validated at record time. A grain that
            // does not parse, or that carries no pin, is not a candidate —
            // Rule E1 would refuse the draft at the door anyway; skipping it
            // here keeps the queue free of dead-on-arrival entries.
            let Some(tuple) = grain
                .str_field("object")
                .and_then(|o| serde_json::from_str::<Map<String, Value>>(o).ok())
            else {
                continue;
            };
            let Some(pin) = tuple
                .get("evalset_hash")
                .and_then(Value::as_str)
                .filter(|h| !h.trim().is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let base_model = tuple
                .get("base_model")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let mut evidence = vec![grain.hash.clone()];
            if let Some(manifest) = tuple.get("corpus_manifest").and_then(Value::as_str) {
                evidence.push(manifest.to_string());
            }

            let mut args = Map::new();
            args.insert("model".into(), json!(subject));
            args.insert("base_model".into(), json!(base_model));

            let mut payload = tuple.clone();
            payload.insert("adapter_grain".into(), json!(grain.hash));

            // The verify leg: after promotion the host re-runs the pinned
            // evalset against the served adapter; a recorded run with more
            // failures than the gating baseline (0 — apply refuses a failing
            // gate) is a regression and outcome_review proposes the revert.
            // No recorded baseline run yet → no metric: honestly unmeasured,
            // never fabricated.
            let eval_subject = format!("evalset:{pin}");
            let baseline_run = harness
                .iter()
                .filter(|g| {
                    g.str_field("relation") == Some("mg:eval_run")
                        && g.str_field("subject") == Some(eval_subject.as_str())
                })
                .max_by(|a, b| {
                    (a.created_at_ms, a.hash.as_str()).cmp(&(b.created_at_ms, b.hash.as_str()))
                });

            let mut draft = RecDraft::new(
                subject.clone(),
                ActionKind::AdapterRevision,
                Summary::new("adapter.candidate", args),
                Proposal::Data { data: payload },
            )
            .severity(Severity::Medium)
            .evidence(evidence)
            .evalset_hash(pin.clone());

            if baseline_run.is_some() {
                draft = draft.metric(MetricSnapshot {
                    metric: format!("evalset:{pin}:failed"),
                    // Apply refuses a failing gate, so the promoted state's
                    // baseline is zero failures; any post-apply failure is a
                    // regression.
                    baseline: 0.0,
                    unit: "count".into(),
                    n: 1,
                    window: "per-run".into(),
                    subject: Some(subject.clone()),
                    namespace: None,
                    relation: None,
                    query: format!(
                        "RECALL facts WHERE subject = \"evalset:{pin}\" AND relation = \"mg:eval_run\""
                    ),
                    review_after_ms: 86_400_000,
                    // 1 day, 1 week, 1 month — a late regression is caught by
                    // the schedule.
                    horizons_ms: vec![86_400_000, 7 * 86_400_000, 30 * 86_400_000],
                    // Failure count: fewer is better (the default).
                    higher_is_better: false,
                });
            }
            drafts.push(draft);
        }
        Ok(drafts)
    }

    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TargetRef;
    use crate::testkit::TestSubstrate;

    const T: i64 = 1_000_000;

    fn tuple(pin: &str, manifest: &str) -> String {
        json!({
            "adapter": {"uri": "file:///adapters/a.safetensors", "sha256": "feed"},
            "base_model": "qwen3-4b",
            "quantization": "bf16",
            "serving_runtime": "vllm",
            "serves_as": "acme-support",
            "evalset_hash": pin,
            "corpus_manifest": manifest,
        })
        .to_string()
    }

    #[test]
    fn manifest_is_never_auto_apply_and_the_class_is_ineligible() {
        let a = AdapterIntake::new();
        assert_eq!(a.manifest().id, "loop.adapter_intake/1");
        assert_eq!(a.manifest().auto_apply, AutoApplyClass::Never);
        assert!(a.manifest().default_on);
        // The third lock: the target class itself is auto-apply-ineligible.
        assert!(!TargetRef::parse("model:acme-support")
            .unwrap()
            .auto_apply_eligible_class());
    }

    #[test]
    fn drafts_the_candidate_with_pin_and_evidence() {
        let mut sub = TestSubstrate::new();
        let adapter =
            sub.add_fact_at("agent:harness", "model:acme-support", "mg:adapter", &tuple("pin1", "cafe"), T);
        let drafts = sub.analyze(&AdapterIntake::new(), T + 1);
        assert_eq!(drafts.len(), 1);
        let d = &drafts[0];
        assert_eq!(d.target_ref, "model:acme-support");
        assert_eq!(d.action_kind, ActionKind::AdapterRevision);
        assert_eq!(d.evalset_hash.as_deref(), Some("pin1"));
        assert_eq!(d.evidence, vec![adapter, "cafe".to_string()]);
        // No recorded eval run for the pin yet: honestly unmeasured.
        assert!(d.metric.is_none());
        match &d.proposal {
            Proposal::Data { data } => {
                assert_eq!(data.get("serves_as").and_then(Value::as_str), Some("acme-support"));
                assert!(data.get("adapter_grain").is_some());
            }
            other => panic!("expected Data proposal, got {other:?}"),
        }
    }

    #[test]
    fn attaches_the_evalset_metric_when_a_baseline_run_exists() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_at("agent:harness", "model:acme-support", "mg:adapter", &tuple("pin1", "cafe"), T);
        sub.add_fact_at(
            "agent:harness",
            "evalset:pin1",
            "mg:eval_run",
            "{\"run_id\":\"eval-1\",\"passed\":3,\"failed\":0}",
            T,
        );
        let drafts = sub.analyze(&AdapterIntake::new(), T + 1);
        let m = drafts[0].metric.as_ref().expect("the verify metric");
        assert_eq!(m.metric, "evalset:pin1:failed");
        assert!(!m.higher_is_better, "failure counts: fewer is better");
    }

    #[test]
    fn a_promoted_subject_is_silent_until_rollback_retracts_the_promotion() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_at("agent:harness", "model:acme-support", "mg:adapter", &tuple("pin1", "cafe"), T);
        // One candidate per served model: a live promotion settles the slot.
        sub.add_fact_at("areev-loop", "model:acme-support", "mg:adapter_promotion", "{}", T + 10);
        assert!(sub.analyze(&AdapterIntake::new(), T + 20).is_empty());
    }

    #[test]
    fn two_candidates_one_model_yields_only_the_newest() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_at("agent:harness", "model:acme-support", "mg:adapter", &tuple("pin1", "old"), T);
        let newer =
            sub.add_fact_at("agent:harness", "model:acme-support", "mg:adapter", &tuple("pin1", "new"), T + 100);
        let drafts = sub.analyze(&AdapterIntake::new(), T + 200);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].evidence[0], newer);
    }

    #[test]
    fn malformed_or_unpinned_registry_grains_are_skipped() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_at("agent:harness", "model:a", "mg:adapter", "not json", T);
        sub.add_fact_at(
            "agent:harness",
            "model:b",
            "mg:adapter",
            "{\"base_model\":\"x\",\"serves_as\":\"b\"}",
            T,
        );
        assert!(sub.analyze(&AdapterIntake::new(), T + 1).is_empty());
    }
}
