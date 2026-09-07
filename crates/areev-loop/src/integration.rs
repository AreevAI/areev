//! Engine end-to-end tests over the reference substrate: the full
//! propose → review → apply → rollback loop, gating, dedup idempotency, the
//! destructive gate, scopes, and the self-approval block. Compiled only under
//! `cfg(test)`.

use crate::engine::{Decision, Engine, RunOptions, RunOutcome, Scope, ScopeSet, SkipReason, LOOP_NS};
use crate::error::Error;
use crate::policy::Policy;
use crate::recommendation::{ObserverType, RecStatus, Recommendation};
use crate::testkit::TestSubstrate;

struct CountingSubstrate<'a> {
    inner: &'a mut crate::reference::ReferenceSubstrate,
    effect_calls: usize,
    unpinned_specs: usize,
}

impl CountingSubstrate<'_> {
    fn note_spec(&mut self, spec: &crate::substrate::GrainSpec) {
        self.effect_calls += 1;
        if !spec.fields.contains_key("created_at_ms") && !spec.fields.contains_key("at_ms") {
            self.unpinned_specs += 1;
        }
    }
}

impl crate::substrate::SubstrateRead for CountingSubstrate<'_> {
    fn capabilities(&self) -> crate::substrate::Capabilities {
        self.inner.capabilities()
    }

    fn grains_of_type(
        &self,
        grain_type: &str,
        namespace: Option<&str>,
        opts: crate::substrate::ReadOpts,
    ) -> crate::error::Result<Vec<crate::model::GrainRecord>> {
        self.inner.grains_of_type(grain_type, namespace, opts)
    }

    fn grain(&self, hash: &str) -> crate::error::Result<Option<crate::model::GrainRecord>> {
        self.inner.grain(hash)
    }

    fn heads(
        &self,
        namespace: Option<&str>,
    ) -> crate::error::Result<Vec<crate::substrate::HeadGroup>> {
        self.inner.heads(namespace)
    }

    fn telemetry(
        &self,
        namespace: Option<&str>,
    ) -> crate::error::Result<Option<crate::substrate::TelemetryView>> {
        self.inner.telemetry(namespace)
    }
}

impl crate::substrate::OmsSubstrate for CountingSubstrate<'_> {
    fn put_grain(
        &mut self,
        spec: &crate::substrate::GrainSpec,
    ) -> crate::error::Result<String> {
        self.note_spec(spec);
        self.inner.put_grain(spec)
    }

    fn supersede(
        &mut self,
        target_hash: &str,
        spec: &crate::substrate::GrainSpec,
        justification: &str,
    ) -> crate::error::Result<String> {
        self.note_spec(spec);
        self.inner.supersede(target_hash, spec, justification)
    }

    fn retract(&mut self, hash: &str, reason: &str) -> crate::error::Result<()> {
        self.effect_calls += 1;
        self.inner.retract(hash, reason)
    }

    fn execute_cal(&mut self, cal: &str) -> crate::error::Result<Vec<serde_json::Value>> {
        self.effect_calls += 1;
        self.inner.execute_cal(cal)
    }

    fn validate_cal(&self, cal: &str) -> crate::error::Result<()> {
        self.inner.validate_cal(cal)
    }

    fn load_state(&self) -> crate::error::Result<serde_json::Value> {
        self.inner.load_state()
    }

    fn store_state(&mut self, state: &serde_json::Value) -> crate::error::Result<()> {
        self.effect_calls += 1;
        self.inner.store_state(state)
    }
}

fn seed_all(sub: &mut TestSubstrate) {
    // exact-duplicate facts
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise");
    // contradiction under a functional relation
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    // an expired grain
    sub.add_fact_valid_to("promo", "active", "true", 500);
    // a dominant tool-failure cluster
    for _ in 0..5 {
        sub.add_tool_call("stripe_refund", true, "rate_limited 429");
    }
    sub.add_tool_call("stripe_refund", false, "ok");
}

#[test]
fn run_proposes_across_analyzers_and_is_idempotent() {
    let mut sub = TestSubstrate::new();
    seed_all(&mut sub);
    let e = Engine::with_builtins();

    let r1 = e
        .run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    assert!(r1.ran());
    assert!(
        r1.stored >= 4,
        "expected duplicate + contradiction + staleness + tool-failure, got {}",
        r1.stored
    );

    // Same findings on a second run collapse to nothing new (dedup).
    let r2 = e
        .run(&mut sub.inner, &RunOptions::default(), 20_000)
        .unwrap();
    assert!(r2.ran());
    assert_eq!(r2.stored, 0, "no re-proposals");
}

#[test]
fn analyze_only_is_side_effect_free_and_matches_production_decisions() {
    use crate::substrate::{OmsSubstrate, ReadOpts, SubstrateRead};
    use std::collections::BTreeMap;

    let mut sub = TestSubstrate::new();
    seed_all(&mut sub);
    let e = Engine::with_builtins();
    let opts = RunOptions::default();
    let now = 10_000;
    let mut counted = CountingSubstrate {
        inner: &mut sub.inner,
        effect_calls: 0,
        unpinned_specs: 0,
    };
    let state_before = counted.load_state().unwrap();
    let count_before: usize = [
        "fact",
        "event",
        "tool",
        "observation",
        "recommendation",
        "audit",
    ]
    .iter()
    .map(|ty| {
        counted
            .grains_of_type(
                ty,
                None,
                ReadOpts {
                    live_only: false,
                    since_ms: None,
                },
            )
            .unwrap()
            .len()
    })
    .sum();

    let replay = e
        .analyze_only(&counted, &opts, &BTreeMap::new(), now)
        .unwrap();
    assert_eq!(counted.effect_calls, 0, "replay must invoke no effect executor");
    assert_eq!(counted.load_state().unwrap(), state_before, "state is immutable");
    let count_after: usize = [
        "fact",
        "event",
        "tool",
        "observation",
        "recommendation",
        "audit",
    ]
    .iter()
    .map(|ty| {
        counted
            .grains_of_type(
                ty,
                None,
                ReadOpts {
                    live_only: false,
                    since_ms: None,
                },
            )
            .unwrap()
            .len()
    })
    .sum();
    assert_eq!(count_after, count_before, "analysis must append no grains or audit rows");

    e.run(&mut counted, &opts, now).unwrap();
    assert!(counted.effect_calls > 0, "production control must exercise effects");
    assert_eq!(counted.unpinned_specs, 0, "engine writes must carry an injected clock");
    let stored = counted
        .grains_of_type(
            crate::model::grain_type::RECOMMENDATION,
            Some(LOOP_NS),
            ReadOpts {
                live_only: false,
                since_ms: None,
            },
        )
        .unwrap()
        .into_iter()
        .map(|g| Recommendation::from_fields(&g.hash, &g.fields).unwrap())
        .collect::<Vec<_>>();
    let replay_values: Vec<_> = replay
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect();
    let stored_values: Vec<_> = stored
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect();
    assert_eq!(replay_values, stored_values, "replay preserves production decision order");
}

#[test]
fn analyze_only_overrides_are_validated_and_applied() {
    use std::collections::BTreeMap;

    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("stripe_refund", true, "rate_limited 429");
    }
    let e = Engine::with_builtins();
    let baseline = e
        .analyze_only(&sub.inner, &RunOptions::default(), &BTreeMap::new(), 10_000)
        .unwrap();
    assert!(baseline.iter().any(|r| r.analyzer.starts_with("loop.tool_failure")));

    let mut params = serde_json::Map::new();
    params.insert("min_count".into(), serde_json::json!(10));
    let overrides = BTreeMap::from([("loop.tool_failure/1".to_string(), params)]);
    let replay = e
        .analyze_only(&sub.inner, &RunOptions::default(), &overrides, 10_000)
        .unwrap();
    assert!(replay.iter().all(|r| !r.analyzer.starts_with("loop.tool_failure")));
}

/// A canned LLM backend keyed by op (discover / ground / verify / enrich) so a
/// test can drive the whole PROPOSE → GROUND → VERIFY → ENRICH pipeline.
struct MockLlm {
    discover: String,
    ground: String,
    verify: String,
    enrich: String,
}
impl crate::llm::LlmBackend for MockLlm {
    fn model(&self) -> &str {
        "mock-llm"
    }
    fn complete(&self, request: &str) -> crate::error::Result<String> {
        Ok(if request.contains("\"op\":\"discover\"") {
            self.discover.clone()
        } else if request.contains("\"op\":\"ground\"") {
            self.ground.clone()
        } else if request.contains("\"op\":\"verify\"") {
            self.verify.clone()
        } else {
            self.enrich.clone()
        })
    }
}

/// A draft may cite bundled evidence by the short id the bundle labels it
/// with, not only by the full hash — and a citation naming nothing in the
/// bundle is still a fabrication that drops the draft.
#[test]
fn llm_drafts_may_cite_evidence_by_bundle_id() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"by id","target":"entity:test/acme","evidence":["e1"],"confidence":0.9}},
          {{"summary":"by hash","target":"grain:{h2}","evidence":["{h2}"],"confidence":0.9}},
          {{"summary":"fabricated","target":"entity:test/acme","evidence":["e99","not-a-hash"],"confidence":0.9}}
        ]}}"#
    );
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
        discover,
        ground: r#"{"results":[{"id":0,"supported":true,"reason":"ok"},{"id":1,"supported":true,"reason":"ok"}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9,"reason":"ok"},{"id":1,"keep":true,"confidence":0.9,"reason":"ok"}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    }));
    let out = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let funnel = out.llm_funnel.expect("an llm run reports its funnel");
    assert_eq!(funnel.proposed, 3);
    assert_eq!(funnel.dropped_uncited, 1, "the fabricated citation is the only drop");
    let llm: Vec<Recommendation> = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r.origin, Origin::Llm { .. }))
        .collect();
    // Distinct targets, so dedup (family ⟂ target ⟂ action) keeps both.
    assert_eq!(llm.len(), 2, "id- and hash-cited drafts both survive");
    for r in &llm {
        assert!(
            r.evidence.iter().all(|h| h == &h1 || h == &h2),
            "stored evidence is the resolved grain hash, never the label: {:?}",
            r.evidence
        );
    }
    assert!(llm.iter().all(|r| r.summary.render() != "fabricated"));
}

/// The three citation shapes, pinned on the resolver itself (the reference
/// substrate's hashes are not hex, so the prefix rule needs a direct test).
#[test]
fn citation_resolves_by_hash_id_or_unambiguous_hex_prefix() {
    use crate::engine::resolve_citation;
    use std::collections::{BTreeMap, BTreeSet};
    let a = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90".to_string();
    let b = "a1b2c3d4e5f6ffffffffffffffffffffffffffffffffffffffffffffffffffff".to_string();
    let bundle: BTreeSet<String> = [a.clone(), b.clone()].into_iter().collect();
    let ids: BTreeMap<&str, &str> = [("e1", a.as_str()), ("e2", b.as_str())].into_iter().collect();
    assert_eq!(resolve_citation(&a, &bundle, &ids).as_deref(), Some(a.as_str()));
    assert_eq!(resolve_citation("e2", &bundle, &ids).as_deref(), Some(b.as_str()));
    assert_eq!(resolve_citation(" e1 ", &bundle, &ids).as_deref(), Some(a.as_str()), "whitespace-tolerant");
    assert_eq!(resolve_citation(&a[..16], &bundle, &ids).as_deref(), Some(a.as_str()), "unambiguous prefix");
    assert_eq!(resolve_citation(&a[..16].to_uppercase(), &bundle, &ids).as_deref(), Some(a.as_str()));
    assert_eq!(resolve_citation(&a[..12], &bundle, &ids), None, "shared 12-char prefix is ambiguous");
    assert_eq!(resolve_citation(&a[..8], &bundle, &ids), None, "too short to count as a citation");
    assert_eq!(resolve_citation("e3", &bundle, &ids), None);
    assert_eq!(resolve_citation("", &bundle, &ids), None);
}

/// Two lessons on one entity that both regress get TWO reverts. A revert's
/// dedup key includes the hash of what it reverts; keyed by target alone,
/// the second was dropped as a duplicate of the first (the learning-curve
/// study's seed 3: two regressed, one revert proposed).
#[test]
fn two_regressed_lessons_on_one_target_get_two_reverts() {
    use crate::model::Origin;
    let t = 5_000_000;
    let scopes = ScopeSet::all();
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("capture", "correction", "vendor missing");
    let journal = |sub: &mut TestSubstrate, run_id: &str, exact: u64, at: i64| {
        sub.add_fact_at(
            "agent:harness",
            "evalset:heldout1",
            "mg:eval_run",
            &format!(r#"{{"run_id":"{run_id}","passed":{exact},"failed":{},"exact":{exact}}}"#, 100 - exact),
            at,
        );
    };
    journal(&mut sub, "eval-before", 90, t - DAY);
    let llm = MockLlm {
        discover: format!(
            r#"{{"recommendations":[{{"summary":"dates as printed","target":"entity:test/capture","evidence":["{h1}"],"confidence":0.9,"proposal":{{"kind":"lesson","lesson":"Copy the file date exactly as printed."}}}},{{"summary":"names as printed","target":"entity:test/capture","evidence":["{h1}"],"confidence":0.9,"proposal":{{"kind":"lesson","lesson":"Copy the signer name exactly as printed."}}}}]}}"#
        ),
        ground: r#"{"results":[{"id":0,"supported":true,"reason":"ok"},{"id":1,"supported":true,"reason":"ok"}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9,"reason":"ok"},{"id":1,"keep":true,"confidence":0.9,"reason":"ok"}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    };
    let policy = Policy::from_json(
        r#"{"outcome_evalset": {"hash": "heldout1", "field": "exact", "higher_is_better": true}}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_llm(Box::new(llm)).with_policy(policy);
    e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    let lessons: Vec<Recommendation> = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r.origin, Origin::Llm { .. }))
        .collect();
    assert_eq!(lessons.len(), 2, "two different lessons on one entity both reach the queue");
    for (i, r) in lessons.iter().enumerate() {
        e.review(&mut sub.inner, &r.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "ok", t + 1 + i as i64)
            .unwrap();
        e.apply(&mut sub.inner, &r.hash, "user:a", ObserverType::Human, &scopes, "apply", false, t + 10 + i as i64)
            .unwrap();
    }
    journal(&mut sub, "eval-after", 60, t + DAY + 100);
    e.run(&mut sub.inner, &RunOptions::default(), t + DAY + 200).unwrap();
    let regressed: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.verdict == "regressed")
        .collect();
    assert_eq!(regressed.len(), 2, "both lessons measured regressed");
    let reverts: Vec<Recommendation> = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .filter(|r| r.analyzer.starts_with("loop.outcome_review"))
        .collect();
    assert_eq!(reverts.len(), 2, "one revert per regressed lesson, not one per target");
    let mut keys: Vec<_> = reverts.iter().map(|r| r.dedup_key.clone()).collect();
    keys.dedup();
    assert_eq!(keys.len(), 2, "distinct dedup keys");
}

/// A verdict compares against the state of the world at the APPLY, not at the
/// proposal. A deployment that journals its evalset once, on day one, and
/// then approves rule after rule would otherwise measure its twentieth rule
/// against day one — and a rule that cost twenty points reads as `held`
/// against a baseline the first nineteen rules had long since left behind.
/// Found on a real corpus (`crates/areev-bench/CURVE.md`, seed 1: 86% → 66%,
/// measured as held against 26%). With nothing journaled between the
/// proposal and the apply, the baseline is the proposal's own run, so the
/// test right below this one is unchanged.
#[test]
fn a_run_journaled_before_the_apply_is_the_lessons_baseline() {
    use crate::model::Origin;
    let t = 5_000_000;
    let scopes = ScopeSet::all();
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("capture", "correction", "vendor missing");
    let journal = |sub: &mut TestSubstrate, run_id: &str, exact: u64, at: i64| {
        sub.add_fact_at(
            "agent:harness",
            "evalset:heldout1",
            "mg:eval_run",
            &format!(r#"{{"run_id":"{run_id}","passed":{exact},"failed":{},"exact":{exact}}}"#, 100 - exact),
            at,
        );
    };
    // Day one: 26 of 100. Then the deployment's earlier rules take it to 86.
    journal(&mut sub, "eval-day-one", 26, t - 3 * DAY);
    let llm = MockLlm {
        discover: format!(
            r#"{{"recommendations":[{{"summary":"dates are being standardised","target":"entity:test/capture","evidence":["{h1}"],"confidence":0.9,"proposal":{{"kind":"lesson","lesson":"Copy the file date exactly as printed, in its original format."}}}}]}}"#
        ),
        ground: r#"{"results":[{"id":0,"supported":true,"reason":"ok"}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9,"reason":"ok"}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    };
    let policy = Policy::from_json(
        r#"{"outcome_evalset": {"hash": "heldout1", "field": "exact", "higher_is_better": true}}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_llm(Box::new(llm)).with_policy(policy);
    e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .expect("the lesson is proposed");
    assert_eq!(rec.metric.as_ref().unwrap().baseline, 26.0, "the proposal froze day one");
    // Measured again before the apply: 86. THIS is what the rule is judged against.
    journal(&mut sub, "eval-before-apply", 86, t + 1);
    e.review(&mut sub.inner, &rec.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "reads fine", t + 2)
        .unwrap();
    e.apply(&mut sub.inner, &rec.hash, "user:a", ObserverType::Human, &scopes, "apply the lesson", false, t + 3)
        .unwrap();
    // After the apply: 66. Better than day one; twenty points worse than the apply.
    // (The 1d checkpoint is due a day after the apply at t + 3.)
    journal(&mut sub, "eval-after", 66, t + DAY + 10);
    e.run(&mut sub.inner, &RunOptions::default(), t + DAY + 20).unwrap();
    let verdicts: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.rec_hash == rec.hash)
        .collect();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(
        (verdicts[0].baseline, verdicts[0].current, verdicts[0].verdict.as_str()),
        (86.0, 66.0, "regressed"),
        "judged against the run before the apply, not the day-one snapshot"
    );
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("loop.outcome_review")),
        "and the revert is proposed"
    );
}

/// An LLM-authored lesson carries no recurrence metric — nothing errors when
/// a lesson is merely useless — so `Policy::outcome_evalset` gives every
/// applicable authored proposal the host's evalset as its metric: baseline
/// from the newest run journaled before the proposal, current from runs
/// journaled after the apply. A worse run after the apply is a regression,
/// the gate proposes the revert, applying it retracts the lesson and puts it
/// on cooldown. Without a baseline run there is no metric at all.
#[test]
fn an_authored_lesson_is_measured_against_the_policy_evalset_and_reverted_on_regression() {
    use crate::model::Origin;
    use crate::substrate::OmsSubstrate;
    let t = 5_000_000;
    let scopes = ScopeSet::all();
    let lesson_llm = |h1: &str| MockLlm {
        discover: format!(
            r#"{{"recommendations":[{{"summary":"the agent keeps skipping the vendor","target":"entity:test/capture","evidence":["{h1}"],"confidence":0.9,"proposal":{{"kind":"lesson","lesson":"Record the vendor name on every receipt."}}}}]}}"#
        ),
        ground: r#"{"results":[{"id":0,"supported":true,"reason":"ok"}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9,"reason":"ok"}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    };
    let policy = || {
        Policy::from_json(
            r#"{"outcome_evalset": {"hash": "heldout1", "field": "exact", "higher_is_better": true}}"#,
        )
        .unwrap()
    };
    let journal = |sub: &mut TestSubstrate, run_id: &str, exact: u64, at: i64| {
        sub.add_fact_at(
            "agent:harness",
            "evalset:heldout1",
            "mg:eval_run",
            &format!(r#"{{"run_id":"{run_id}","passed":{exact},"failed":{},"exact":{exact}}}"#, 60 - exact),
            at,
        );
    };
    let llm_pending = |e: &Engine, sub: &TestSubstrate| -> Vec<Recommendation> {
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .into_iter()
            .filter(|r| matches!(r.origin, Origin::Llm { .. }))
            .collect()
    };

    // No baseline run journaled → the lesson is stored without a metric.
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("capture", "correction", "vendor missing");
    let e = Engine::with_builtins().with_llm(Box::new(lesson_llm(&h1))).with_policy(policy());
    e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    let recs = llm_pending(&e, &sub);
    assert_eq!(recs.len(), 1);
    assert!(recs[0].metric.is_none(), "no journaled run → honestly unmeasured");

    // With a baseline run: the metric names the evalset and carries its value.
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("capture", "correction", "vendor missing");
    journal(&mut sub, "eval-baseline", 20, t - 3_600_000);
    let e = Engine::with_builtins().with_llm(Box::new(lesson_llm(&h1))).with_policy(policy());
    e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    let recs = llm_pending(&e, &sub);
    assert_eq!(recs.len(), 1);
    let m = recs[0].metric.as_ref().expect("an applicable authored lesson carries the evalset metric");
    assert_eq!(m.metric, "evalset:heldout1:exact");
    assert_eq!(m.baseline, 20.0);
    assert!(m.higher_is_better);
    assert_eq!(m.horizons_ms, vec![DAY, 7 * DAY, 30 * DAY]);
    let hash = recs[0].hash.clone();
    let dk = recs[0].dedup_key.clone();

    e.review(&mut sub.inner, &hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "try it", t + 1)
        .unwrap();
    e.apply(&mut sub.inner, &hash, "user:a", ObserverType::Human, &scopes, "apply the lesson", false, t + 2)
        .unwrap();

    // A run journaled BEFORE the apply is not evidence: with none after it,
    // the 1d checkpoint stays due and no verdict is recorded.
    e.run(&mut sub.inner, &RunOptions::default(), t + 2 * DAY).unwrap();
    assert!(
        e.outcomes(&sub.inner).unwrap().iter().all(|o| o.rec_hash != hash),
        "no run since the apply → not yet measurable, never scored against the baseline"
    );

    // A worse run after the apply → regressed at the 1d checkpoint → revert.
    journal(&mut sub, "eval-after", 12, t + 2 * DAY + 1);
    e.run(&mut sub.inner, &RunOptions::default(), t + 2 * DAY + 2).unwrap();
    let verdicts: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.rec_hash == hash)
        .collect();
    assert_eq!(verdicts.len(), 1);
    assert_eq!((verdicts[0].baseline, verdicts[0].current, verdicts[0].verdict.as_str()), (20.0, 12.0, "regressed"));
    let revert = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.analyzer.starts_with("loop.outcome_review"))
        .expect("the regression proposed a revert");
    e.review(&mut sub.inner, &revert.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "it hurt", t + 2 * DAY + 3)
        .unwrap();
    e.apply(&mut sub.inner, &revert.hash, "user:a", ObserverType::Human, &scopes, "revert", false, t + 2 * DAY + 4)
        .unwrap();
    assert_eq!(status_of(&e, &sub, &hash), RecStatus::RolledBack);
    let cooled = crate::config::LoopPersisted::from_value(sub.inner.load_state().unwrap())
        .unwrap()
        .cooldowns
        .get(&dk)
        .copied();
    assert_eq!(cooled, Some(t + 2 * DAY + 4 + 7 * DAY), "the reverted lesson is on cooldown");

    // The same evidence, the same model, the next pass: not re-proposed.
    e.run(&mut sub.inner, &RunOptions::default(), t + 2 * DAY + 5).unwrap();
    assert!(
        llm_pending(&e, &sub).iter().all(|r| r.dedup_key != dk),
        "a lesson the gate just retracted is not re-proposed on the next pass"
    );

    // A better run after the apply → held.
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("capture", "correction", "vendor missing");
    journal(&mut sub, "eval-baseline", 20, t - 3_600_000);
    let e = Engine::with_builtins().with_llm(Box::new(lesson_llm(&h1))).with_policy(policy());
    e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    let hash = llm_pending(&e, &sub)[0].hash.clone();
    e.review(&mut sub.inner, &hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "try it", t + 1)
        .unwrap();
    e.apply(&mut sub.inner, &hash, "user:a", ObserverType::Human, &scopes, "apply", false, t + 2)
        .unwrap();
    journal(&mut sub, "eval-after", 33, t + 3);
    // The 1d checkpoint is due one day after the APPLY (t + 2), not after t.
    e.run(&mut sub.inner, &RunOptions::default(), t + 2 + DAY).unwrap();
    let verdicts: Vec<_> = e.outcomes(&sub.inner).unwrap().into_iter().filter(|o| o.rec_hash == hash).collect();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].verdict, "held");
    assert!(
        !e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("loop.outcome_review")),
        "an improvement proposes no revert"
    );
}

/// Two different authored lessons on ONE entity are two findings — both
/// reach the queue, and the second is not a duplicate of the first even
/// while the first is applied. The same lesson re-authored (at a different
/// confidence, with different spacing) IS a duplicate. An advisory flag
/// keeps the analyzer-style key: one open flag per target.
#[test]
fn authored_lessons_dedup_on_content_not_only_on_target() {
    use crate::model::Origin;
    let scopes = ScopeSet::all();
    let llm = |h1: &str, lessons: &[&str]| {
        let recs: Vec<String> = lessons
            .iter()
            .map(|l| format!(
                r#"{{"summary":"{l}","target":"entity:test/capture","evidence":["{h1}"],"confidence":0.9,"proposal":{{"kind":"lesson","lesson":"{l}"}}}}"#
            ))
            .collect();
        let n = lessons.len();
        MockLlm {
            discover: format!(r#"{{"recommendations":[{}]}}"#, recs.join(",")),
            ground: format!(
                r#"{{"results":[{}]}}"#,
                (0..n).map(|i| format!(r#"{{"id":{i},"supported":true,"reason":"ok"}}"#)).collect::<Vec<_>>().join(",")
            ),
            verify: format!(
                r#"{{"results":[{}]}}"#,
                (0..n).map(|i| format!(r#"{{"id":{i},"keep":true,"confidence":0.9,"reason":"ok"}}"#)).collect::<Vec<_>>().join(",")
            ),
            enrich: r#"{"notes":[]}"#.into(),
        }
    };
    let llm_recs = |e: &Engine, sub: &TestSubstrate, status: Option<RecStatus>| -> Vec<Recommendation> {
        e.recommendations(&sub.inner, status)
            .unwrap()
            .into_iter()
            .filter(|r| matches!(r.origin, Origin::Llm { .. }))
            .collect()
    };

    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("capture", "correction", "vendor and amount missing");
    let e = Engine::with_builtins().with_llm(Box::new(llm(
        &h1,
        &["Record the vendor name on every receipt.", "Record the amount on every receipt."],
    )));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = llm_recs(&e, &sub, Some(RecStatus::Pending));
    assert_eq!(recs.len(), 2, "two different lessons on one entity both reach the queue");
    assert_ne!(recs[0].dedup_key, recs[1].dedup_key);
    let vendor = recs.iter().find(|r| r.summary.render().contains("vendor")).unwrap().clone();
    e.review(&mut sub.inner, &vendor.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "yes", 10_001)
        .unwrap();
    e.apply(&mut sub.inner, &vendor.hash, "user:a", ObserverType::Human, &scopes, "apply", false, 10_002)
        .unwrap();

    // The vendor lesson again (reworded only in spacing/case) plus a THIRD
    // lesson: the repeat is deduped against the applied one, the new one is
    // admitted.
    let e = Engine::with_builtins().with_llm(Box::new(llm(
        &h1,
        &["record  the VENDOR name on every receipt", "Copy the address exactly as printed."],
    )));
    sub.add_fact("capture", "correction", "address missing");
    // A full sweep, so the bundle holds the evidence the drafts cite
    // regardless of the first pass's watermark.
    let sweep = RunOptions { full_sweep: true, ..RunOptions::default() };
    e.run(&mut sub.inner, &sweep, 20_000).unwrap();
    let all = llm_recs(&e, &sub, None);
    assert_eq!(all.len(), 3, "vendor (applied), amount (pending), address (pending) — the repeat was deduped");
    let pending = llm_recs(&e, &sub, Some(RecStatus::Pending));
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().any(|r| r.summary.render().contains("address")));
    assert!(
        !pending.iter().any(|r| r.summary.render().to_lowercase().contains("vendor")),
        "the applied lesson is not re-queued under a cosmetic rewording"
    );

    // Advisory drafts (no proposal) on one target still collapse to one.
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("capture", "correction", "x");
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
        discover: format!(
            r#"{{"recommendations":[
              {{"summary":"look at this","target":"entity:test/capture","evidence":["{h1}"],"confidence":0.9}},
              {{"summary":"and at this","target":"entity:test/capture","evidence":["{h1}"],"confidence":0.9}}
            ]}}"#
        ),
        ground: r#"{"results":[{"id":0,"supported":true},{"id":1,"supported":true}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9},{"id":1,"keep":true,"confidence":0.9}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(llm_recs(&e, &sub, Some(RecStatus::Pending)).len(), 1, "one open advisory flag per target");
}

/// An Observation reaches the model with its observer named. A bare
/// sentence is ambiguous about direction in the way that matters: a
/// person's correction reads identically to the agent being told something
/// it asked for, and a model given unattributed corrections concluded the
/// agent had been doing the asking. A Fact still renders as its triple, and
/// an Observation with no recorded observer still renders as its bare text.
#[test]
fn an_observation_reaches_the_model_with_its_observer_named() {
    use std::sync::{Arc, Mutex};
    struct Capturing(Arc<Mutex<Vec<String>>>);
    impl crate::llm::LlmBackend for Capturing {
        fn model(&self) -> &str {
            "capture"
        }
        fn complete(&self, request: &str) -> crate::error::Result<String> {
            self.0.lock().unwrap().push(request.to_string());
            Ok(r#"{"recommendations":[]}"#.into())
        }
    }
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    sub.add_human_note("test", "capture", "user:accountant", "Vendor Name is ACME LTD.");
    sub.add_observation("test", "an unattributed note");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let e = Engine::with_builtins().with_llm(Box::new(Capturing(seen.clone())));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let reqs = seen.lock().unwrap();
    let req = reqs
        .iter()
        .find(|r| r.contains("\"op\":\"discover\""))
        .expect("a discover call was made");
    let v: serde_json::Value = serde_json::from_str(req).unwrap();
    let texts: Vec<String> = v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["text"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        texts.iter().any(|t| t == "user:accountant (a person) said of capture: Vendor Name is ACME LTD."),
        "a human observation names its observer: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "an unattributed note"),
        "an observation with no observer still renders as its bare text: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "acme deploy_target us-east-1"),
        "a fact still renders as its triple: {texts:?}"
    );
}

/// `evidence_attribution: anonymous` restores the pre-2026-09-04 projection
/// exactly — the bare text, no observer. It is host policy because an
/// observer id can be a person's name, and because it is the one variable
/// the receipts ablation turns.
#[test]
fn attribution_can_be_turned_off_by_host_policy() {
    use std::sync::{Arc, Mutex};
    struct Capturing(Arc<Mutex<Vec<String>>>);
    impl crate::llm::LlmBackend for Capturing {
        fn model(&self) -> &str {
            "capture"
        }
        fn complete(&self, request: &str) -> crate::error::Result<String> {
            self.0.lock().unwrap().push(request.to_string());
            Ok(r#"{"recommendations":[]}"#.into())
        }
    }
    let texts_under = |policy: Option<Policy>| -> Vec<String> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut sub = TestSubstrate::new();
        sub.add_fact("acme", "deploy_target", "us-east-1");
        sub.add_fact("acme", "deploy_target", "eu-west-1");
        sub.add_human_note("test", "capture", "user:accountant", "Vendor Name is ACME LTD.");
        let mut e = Engine::with_builtins().with_llm(Box::new(Capturing(seen.clone())));
        if let Some(p) = policy {
            e = e.with_policy(p);
        }
        e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
        let reqs = seen.lock().unwrap();
        let req = reqs
            .iter()
            .find(|r| r.contains(r#""op":"discover""#))
            .expect("a discover call");
        serde_json::from_str::<serde_json::Value>(req).unwrap()["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["text"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let named = texts_under(None);
    let anon = texts_under(Some(
        Policy::from_json(r#"{"evidence_attribution": "anonymous"}"#).unwrap(),
    ));
    assert!(named.iter().any(|t| t.contains("user:accountant (a person) said")), "{named:?}");
    assert!(anon.iter().any(|t| t == "Vendor Name is ACME LTD."), "{anon:?}");
    assert!(!anon.iter().any(|t| t.contains("(a person) said")),
            "anonymous attributes nothing: {anon:?}");
    // Only the observation's rendering changes; facts are untouched.
    let facts = |v: &Vec<String>| -> Vec<String> {
        v.iter().filter(|t| t.starts_with("acme ")).cloned().collect()
    };
    assert_eq!(facts(&named), facts(&anon));
}

/// The DISCOVER objective is host policy: the default keeps the review-queue
/// rule byte-for-byte, and `learner` swaps exactly the scoring paragraph.
#[test]
fn discover_objective_is_selected_by_host_policy() {
    use std::sync::{Arc, Mutex};
    struct Capturing(Arc<Mutex<Vec<String>>>);
    impl crate::llm::LlmBackend for Capturing {
        fn model(&self) -> &str {
            "capture"
        }
        fn complete(&self, request: &str) -> crate::error::Result<String> {
            self.0.lock().unwrap().push(request.to_string());
            Ok(r#"{"recommendations":[]}"#.into())
        }
    }
    let discover_instructions = |policy: Option<Policy>| -> String {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut sub = TestSubstrate::new();
        sub.add_fact("acme", "deploy_target", "us-east-1");
        sub.add_fact("acme", "deploy_target", "eu-west-1");
        let mut e = Engine::with_builtins().with_llm(Box::new(Capturing(seen.clone())));
        if let Some(p) = policy {
            e = e.with_policy(p);
        }
        e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
        let reqs = seen.lock().unwrap();
        let req = reqs
            .iter()
            .find(|r| r.contains("\"op\":\"discover\""))
            .expect("a discover call was made");
        serde_json::from_str::<serde_json::Value>(req).unwrap()["instructions"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let default = discover_instructions(None);
    let learner = discover_instructions(Some(
        Policy::from_json(r#"{"discover_objective": "learner"}"#).unwrap(),
    ));
    assert!(default.contains("returning nothing earns 0"), "review-queue rule by default");
    assert!(!default.contains("learning stage"));
    assert!(learner.contains("learning stage of a deployed agent"));
    assert!(learner.contains("ALSO penalized"));
    assert!(!learner.contains("returning nothing earns 0"));
    // Everything but the scoring paragraph is shared verbatim.
    for shared in [
        "propose ADDITIONAL findings",
        "Require at least two rejected outcomes",
        "MUST cite one or more evidence items from the bundle by their 'id'",
        "\"kind\":\"lesson\"",
        "Propose nothing you cannot ground in the evidence.",
    ] {
        assert!(default.contains(shared) && learner.contains(shared), "{shared}");
    }
}

#[test]
fn llm_discover_verified_rec_is_stamped_with_confidence_and_enrich_adds_guidance() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    // A contradiction gives a deterministic finding whose cited evidence seeds
    // the DISCOVER bundle.
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");

    // One draft cites a real evidence hash (kept); one cites a bogus hash
    // (dropped as uncited before the verifier even runs).
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"prod region is ambiguous","target":"entity:test/acme","guidance":"pick one","evidence":["{h1}"],"confidence":0.9}},
          {{"summary":"uncited nonsense","target":"entity:test/acme","evidence":["deadbeef"],"confidence":0.9}}
        ]}}"#
    );
    // After validation only the cited draft remains → verifier id 0.
    let ground = r#"{"results":[{"id":0,"supported":true,"reason":"entailed"}]}"#.to_string();
    let verify =
        r#"{"results":[{"id":0,"keep":true,"confidence":0.88,"reason":"novel and real"}]}"#.to_string();
    let enrich =
        r#"{"notes":[{"target":"entity:test/acme","guidance":"resolve to latest"}]}"#.to_string();

    let e = Engine::with_builtins().with_llm(Box::new(MockLlm { discover, ground, verify, enrich }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();

    // Exactly one llm-origin rec — cited, grounded, verified.
    let llm: Vec<_> = recs
        .iter()
        .filter(|r| matches!(r.origin, Origin::Llm { .. }))
        .collect();
    assert_eq!(llm.len(), 1, "only the cited+grounded+verified draft survives");
    assert!(llm[0].summary.render().contains("ambiguous"));
    assert_eq!(llm[0].evidence, vec![h1.clone()]);
    // The verifier's calibrated confidence is stamped (not a hardcoded default).
    assert!((llm[0].confidence - 0.88).abs() < 1e-9, "conf {}", llm[0].confidence);
    assert!(!llm[0].destructive);
    assert_eq!(llm[0].status, RecStatus::Pending);

    // ENRICH added a whitelisted guidance note to the deterministic finding
    // without touching its templated summary.
    let det = recs
        .iter()
        .find(|r| r.analyzer.starts_with("loop.contradiction"))
        .expect("a contradiction recommendation");
    assert_eq!(det.guidance.as_deref(), Some("resolve to latest"));
    assert!(det.summary.render().contains("deploy_target"));

    // §6b approval-rate metric: one surfaced llm proposal, still undecided.
    let m = e.llm_metrics(&sub.inner).unwrap();
    assert_eq!(m.proposed, 1);
    assert_eq!(m.pending, 1);
    assert_eq!(m.approval_rate, None);
}

#[test]
fn verifier_drops_ungrounded_and_low_confidence_drafts() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    // Two cited drafts pass validation → verifier ids 0 and 1.
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"grounded but the verifier is unsure","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.9}},
          {{"summary":"an ungrounded claim","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.9}}
        ]}}"#
    );
    // Grounding: id 0 supported, id 1 NOT — id 1 never reaches verify.
    let ground =
        r#"{"results":[{"id":0,"supported":true},{"id":1,"supported":false}]}"#.to_string();
    // Verify (only id 0): kept, but confidence 0.5 is below the 0.75 floor → dropped.
    let verify = r#"{"results":[{"id":0,"keep":true,"confidence":0.5}]}"#.to_string();
    let enrich = r#"{"notes":[]}"#.to_string();

    let e = Engine::with_builtins().with_llm(Box::new(MockLlm { discover, ground, verify, enrich }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    assert!(
        recs.iter().all(|r| !matches!(r.origin, Origin::Llm { .. })),
        "ungrounded (id 1) and below-floor (id 0) drafts never reach the queue"
    );
}

#[test]
fn separate_ground_backend_is_consulted_for_grounding() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"grounded per the main model","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.9}}
        ]}}"#
    );
    // The MAIN backend would ground (supported:true) and keep the draft.
    let main = MockLlm {
        discover,
        ground: r#"{"results":[{"id":0,"supported":true}]}"#.to_string(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9}]}"#.to_string(),
        enrich: r#"{"notes":[]}"#.to_string(),
    };
    // The SEPARATE ground backend REJECTS (supported:false). If it — not the main
    // backend — is the one consulted for GROUND, the draft dies before verify.
    let ground = MockLlm {
        discover: String::new(),
        ground: r#"{"results":[{"id":0,"supported":false}]}"#.to_string(),
        verify: String::new(),
        enrich: String::new(),
    };
    let e = Engine::with_builtins()
        .with_llm(Box::new(main))
        .with_ground_llm(Box::new(ground));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    assert!(
        recs.iter().all(|r| !matches!(r.origin, Origin::Llm { .. })),
        "the separate ground backend's rejection gates the draft"
    );
}

#[test]
fn llm_authored_lesson_is_applicable_and_rolls_back() {
    use crate::model::{ActionKind, Origin};
    use crate::recommendation::Proposal;
    use crate::substrate::SubstrateRead;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");

    // Three lesson-bearing drafts: (0) an entity target — becomes applicable
    // (the embedded control chars must be collapsed, never stored); (1) a
    // query target — a lesson Fact has no subject there, stays advisory;
    // (2) an over-long lesson — capped at MAX_LESSON_LEN.
    let long_lesson = "y".repeat(400);
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"the region conflict keeps recurring","target":"entity:test/acme","guidance":"","evidence":["{h1}"],"confidence":0.9,"lesson":"Confirm the deploy region\nbefore writing it"}},
          {{"summary":"query needs a tighter filter","target":"query:cleanup","evidence":["{h1}"],"confidence":0.9,"lesson":"Scope the cleanup query"}},
          {{"summary":"long-winded advice","target":"entity:test/acme2","evidence":["{h1}"],"confidence":0.9,"lesson":"{long_lesson}"}}
        ]}}"#
    );
    let ground = r#"{"results":[{"id":0,"supported":true},{"id":1,"supported":true},{"id":2,"supported":true}]}"#.to_string();
    let verify = r#"{"results":[{"id":0,"keep":true,"confidence":0.9},{"id":1,"keep":true,"confidence":0.9},{"id":2,"keep":true,"confidence":0.9}]}"#.to_string();
    let enrich = r#"{"notes":[]}"#.to_string();

    let e = Engine::with_builtins().with_llm(Box::new(MockLlm { discover, ground, verify, enrich }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();

    // Draft 0: applicable — an ADD fact of the sanitized lesson, in the
    // evidence's namespace, reviewable with the lesson in the summary.
    let lesson_rec = recs
        .iter()
        .find(|r| {
            matches!(r.origin, Origin::Llm { .. }) && r.target_ref == "entity:test/acme"
        })
        .expect("the entity-target lesson rec");
    assert_eq!(lesson_rec.action_kind, ActionKind::ClusterFailure);
    assert!(lesson_rec.rollbackable && !lesson_rec.destructive);
    assert_eq!(lesson_rec.status, RecStatus::Pending);
    let Proposal::Cal { cal } = &lesson_rec.proposal else {
        panic!("authored lesson must be a CAL proposal, got {:?}", lesson_rec.proposal);
    };
    assert!(cal.starts_with("ADD fact "), "{cal}");
    assert!(cal.contains(r#""relation":"lesson""#), "{cal}");
    assert!(
        cal.contains("Confirm the deploy region before writing it"),
        "control chars collapse to spaces: {cal}"
    );
    assert!(cal.contains(r#""subject":"acme""#), "{cal}");
    assert!(
        cal.contains(r#""namespace":"test""#),
        "lesson lands in the evidence's namespace: {cal}"
    );
    assert!(
        lesson_rec.summary.render().contains("Confirm the deploy region"),
        "the reviewer sees the exact line an apply would record"
    );

    // Draft 1: lesson on a query target stays an advisory flag.
    let advisory = recs
        .iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }) && r.target_ref == "query:cleanup")
        .expect("the query-target rec");
    assert_eq!(advisory.action_kind, ActionKind::Flag);
    assert!(!advisory.rollbackable);
    assert!(matches!(advisory.proposal, Proposal::Data { .. }));

    // Draft 2: the lesson text is capped.
    let capped = recs
        .iter()
        .find(|r| {
            matches!(r.origin, Origin::Llm { .. }) && r.target_ref == "entity:test/acme2"
        })
        .expect("the over-long lesson rec");
    let Proposal::Cal { cal } = &capped.proposal else { panic!("expected CAL") };
    assert!(
        !cal.contains(&"y".repeat(crate::llm::MAX_LESSON_LEN + 1)),
        "lesson capped at MAX_LESSON_LEN"
    );

    // The governed round trip: approve with a BECAUSE, apply, roll back —
    // the exact machinery deterministic lessons use.
    e.review(
        &mut sub.inner,
        &lesson_rec.hash,
        Decision::Approve,
        "user:reviewer",
        ObserverType::Human,
        &ScopeSet::all(),
        "grounded and useful",
        11_000,
    )
    .unwrap();
    let applied = e
        .apply(
            &mut sub.inner,
            &lesson_rec.hash,
            "user:reviewer",
            ObserverType::Human,
            &ScopeSet::all(),
            "recording the lesson",
            false, // an authored lesson never needs the destructive override
            12_000,
        )
        .unwrap();
    assert_eq!(applied.created_hashes.len(), 1, "one lesson Fact created");
    let g = sub.inner.grain(&applied.created_hashes[0]).unwrap().expect("lesson grain");
    assert_eq!(g.fact_relation(), Some("lesson"));
    assert_eq!(g.fact_subject(), Some("acme"));
    assert!(g.is_live());

    e.rollback(
        &mut sub.inner,
        &lesson_rec.hash,
        "user:reviewer",
        ObserverType::Human,
        &ScopeSet::all(),
        "measured regression",
        13_000,
    )
    .unwrap();
    let g = sub.inner.grain(&applied.created_hashes[0]).unwrap();
    assert!(
        g.is_none_or(|g| !g.is_live()),
        "rollback retracts the authored lesson"
    );
}

/// A human's note must reach the model even when routine records outnumber
/// it hundreds to one.
///
/// This is the shape that matters: a supervisor states a rule ONCE, and the
/// desk then generates thousands of ordinary grains. Seeding the evidence
/// bundle by recency or frequency buries exactly the rarest and most
/// valuable signal, and the symptom is indistinguishable from a model that
/// had nothing to say.
#[test]
fn a_lone_human_observation_survives_a_flood_of_routine_facts() {
    use std::sync::{Arc, Mutex};
    let mut sub = TestSubstrate::new();
    // The note, written early — then buried.
    sub.add_observation("test", "Note from the billing lead: from now on any refund over $500 must ALSO be logged as a case with priority high.");
    for i in 0..300 {
        sub.add_fact(&format!("task-{i:04}"), "episode", "{\"outcome\":\"accepted\"}");
    }
    // Enough of one failure shape to give the analyzers something to cite.
    for _ in 0..8 {
        sub.add_tool_call("refund", true, "{\"error\":{\"code\":\"rate_limited\"}}");
    }
    sub.add_tool_call("refund", false, "{\"refund_id\":\"re_1\"}");

    let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    struct Capture(Arc<Mutex<String>>);
    impl crate::llm::LlmBackend for Capture {
        fn model(&self) -> &str { "capture" }
        fn complete(&self, request: &str) -> crate::error::Result<String> {
            if request.contains("\"op\":\"discover\"") {
                *self.0.lock().unwrap() = request.to_string();
            }
            Ok(r#"{"recommendations":[]}"#.to_string())
        }
    }
    let e = Engine::with_builtins().with_llm(Box::new(Capture(seen.clone())));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();

    let req = seen.lock().unwrap().clone();
    assert!(!req.is_empty(), "DISCOVER was never called");
    assert!(
        req.contains("billing lead"),
        "the human note never reached the evidence bundle — 300 routine facts          crowded out the one grain a person wrote"
    );
}

/// The funnel has to tell apart the ways "the model contributed nothing"
/// can happen, because they call for opposite responses and every one of
/// them renders as an empty ledger.
#[test]
fn the_llm_funnel_separates_abstention_from_each_gate() {
    let draft = |h: &str| format!(
        r#"{{"recommendations":[{{"summary":"s","target":"entity:test/acme","evidence":["{h}"],"confidence":0.9}}]}}"#
    );
    // The cite-check drops any draft naming a hash the bundle does not hold,
    // so a test of the LATER gates has to cite a real one.
    let real_hash = {
        let mut sub = TestSubstrate::new();
        let h = sub.add_fact("acme", "deploy_target", "us-east-1");
        sub.add_fact("acme", "deploy_target", "eu-west-1");
        h
    };
    let run = |discover: String, ground: &str, verify: &str| {
        let mut sub = TestSubstrate::new();
        sub.add_fact("acme", "deploy_target", "us-east-1");
        sub.add_fact("acme", "deploy_target", "eu-west-1");
        let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
            discover,
            ground: ground.to_string(),
            verify: verify.to_string(),
            enrich: r#"{"notes":[]}"#.to_string(),
        }));
        let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
        r.llm_funnel.expect("a funnel whenever a backend is attached")
    };

    // The model abstained: evidence was offered, nothing came back.
    let f = run(r#"{"recommendations":[]}"#.to_string(), "", "");
    assert!(f.evidence > 0, "evidence was offered");
    assert_eq!((f.proposed, f.stored), (0, 0), "abstention: nothing proposed");

    // Proposed, but uncited — dropped before any model call.
    let f = run(draft("deadbeef"), "", "");
    assert_eq!((f.proposed, f.cited), (1, 0), "uncited drafts die at the cite-check");

    // Cited, then refused by GROUND.
    let f = run(
        draft(&real_hash),
        r#"{"results":[{"id":0,"supported":false}]}"#,
        r#"{"results":[{"id":0,"keep":true,"confidence":0.9}]}"#,
    );
    assert_eq!(f.grounded, 0, "GROUND rejection is visible as its own stage");

    // Grounded, then killed by VERIFY.
    let f = run(
        draft(&real_hash),
        r#"{"results":[{"id":0,"supported":true}]}"#,
        r#"{"results":[{"id":0,"keep":false,"confidence":0.9}]}"#,
    );
    assert_eq!((f.grounded, f.kept, f.stored), (1, 0, 0), "VERIFY kill is distinct");

    // Kept, but under the confidence floor.
    let f = run(
        draft(&real_hash),
        r#"{"results":[{"id":0,"supported":true}]}"#,
        r#"{"results":[{"id":0,"keep":true,"confidence":0.10}]}"#,
    );
    assert_eq!((f.kept, f.stored), (1, 0), "the floor is its own stage");
}

#[test]
fn llm_authored_fact_records_a_model_chosen_relation() {
    use crate::model::{ActionKind, Origin};
    use crate::recommendation::Proposal;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "invoice_vendor", "Cobolt Cloud");
    let _h2 = sub.add_fact("acme", "invoice_vendor", "Cobalt Cloud");

    // (0) a well-formed fact; (1) a relation that is prose, not an identifier
    // — it would become an unfindable predicate, so the draft goes advisory;
    // (2) an empty object — nothing to record.
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"the same misspelling keeps arriving","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"fact","relation":"alias_of","object":"Cobalt Cloud"}}}},
          {{"summary":"prose relation","target":"entity:test/acme2","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"fact","relation":"is usually spelled","object":"Cobalt Cloud"}}}},
          {{"summary":"empty object","target":"entity:test/acme3","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"fact","relation":"alias_of","object":"   "}}}}
        ]}}"#
    );
    let ground = r#"{"results":[{"id":0,"supported":true},{"id":1,"supported":true},{"id":2,"supported":true}]}"#.to_string();
    let verify = r#"{"results":[{"id":0,"keep":true,"confidence":0.9},{"id":1,"keep":true,"confidence":0.9},{"id":2,"keep":true,"confidence":0.9}]}"#.to_string();
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
        discover,
        ground,
        verify,
        enrich: r#"{"notes":[]}"#.to_string(),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();

    let fact = recs
        .iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }) && r.target_ref == "entity:test/acme")
        .expect("the fact rec");
    assert_eq!(fact.action_kind, ActionKind::Record);
    assert!(fact.rollbackable && !fact.destructive);
    let Proposal::Cal { cal } = &fact.proposal else {
        panic!("a fact proposal is CAL, got {:?}", fact.proposal)
    };
    assert!(cal.starts_with("ADD fact "), "{cal}");
    assert!(cal.contains(r#""relation":"alias_of""#), "{cal}");
    assert!(cal.contains(r#""object":"Cobalt Cloud""#), "{cal}");
    // Subject from the TARGET and namespace from the EVIDENCE — the model
    // named neither.
    assert!(cal.contains(r#""subject":"acme""#), "{cal}");
    assert!(cal.contains(r#""namespace":"test""#), "{cal}");
    // The grain carries the VERIFIER's confidence, not the proposer's 0.9.
    assert!(cal.contains(r#""confidence":0.9"#), "{cal}");
    assert!(
        fact.summary.render().contains("alias_of"),
        "the reviewer sees the relation an apply would write: {}",
        fact.summary.render()
    );

    for (target, why) in [
        ("entity:test/acme2", "a prose relation is not a predicate"),
        ("entity:test/acme3", "an empty object records nothing"),
    ] {
        let r = recs
            .iter()
            .find(|r| matches!(r.origin, Origin::Llm { .. }) && r.target_ref == target)
            .unwrap_or_else(|| panic!("expected an advisory rec for {target}"));
        assert_eq!(r.action_kind, ActionKind::Flag, "{why}");
        assert!(matches!(r.proposal, Proposal::Data { .. }), "{why}");
    }
}

#[test]
fn llm_query_revision_is_applicable_only_with_a_recorded_inverse() {
    use crate::model::{ActionKind, Origin};
    use crate::recommendation::Proposal;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");

    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"the briefing query misses the lessons","target":"query:desk_pulse","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"query_revision","body":"RECALL facts WHERE relation = \"lesson\" LIMIT 20"}}}},
          {{"summary":"empty body","target":"query:empty","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"query_revision","body":"  "}}}}
        ]}}"#
    );
    let ground = r#"{"results":[{"id":0,"supported":true},{"id":1,"supported":true}]}"#.to_string();
    let verify = r#"{"results":[{"id":0,"keep":true,"confidence":0.9},{"id":1,"keep":true,"confidence":0.9}]}"#.to_string();
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
        discover,
        ground,
        verify,
        enrich: r#"{"notes":[]}"#.to_string(),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();

    let rev = recs
        .iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }) && r.target_ref == "query:desk_pulse")
        .expect("the query revision rec");
    assert_eq!(rev.action_kind, ActionKind::Revise);
    assert!(rev.rollbackable && !rev.destructive);
    let Proposal::Cal { cal } = &rev.proposal else {
        panic!("a query revision is CAL, got {:?}", rev.proposal)
    };
    // The NAME comes from the target; only the body came from the model.
    assert!(cal.starts_with(r#"DEFINE QUERY "desk_pulse" AS { "#), "{cal}");
    assert!(cal.contains("relation = \"lesson\""), "{cal}");
    assert!(!cal.contains('\n'), "a definition is one statement: {cal}");

    // An empty body is nothing to define.
    let empty = recs
        .iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }) && r.target_ref == "query:empty")
        .expect("the empty-body rec");
    assert_eq!(empty.action_kind, ActionKind::Flag);

    // Governed apply: the DEFINE runs, and the recorded inverse is what makes
    // it applicable at all.
    e.review(
        &mut sub.inner,
        &rev.hash,
        Decision::Approve,
        "user:reviewer",
        ObserverType::Human,
        &ScopeSet::all(),
        "the briefing should carry the lessons",
        11_000,
    )
    .unwrap();
    e.apply(
        &mut sub.inner,
        &rev.hash,
        "user:reviewer",
        ObserverType::Human,
        &ScopeSet::all(),
        "applying the tighter briefing",
        false,
        12_000,
    )
    .unwrap();
    e.rollback(
        &mut sub.inner,
        &rev.hash,
        "user:reviewer",
        ObserverType::Human,
        &ScopeSet::all(),
        "reverting",
        13_000,
    )
    .unwrap();
}

#[test]
fn llm_plan_revision_edits_fields_and_refuses_topology_and_staleness() {
    use crate::model::{ActionKind, Origin};
    use crate::recommendation::Proposal;
    let mut sub = TestSubstrate::new();
    let plan = sub.add_workflow();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");

    // (0) two legal edits; (1) a topology rewrite; (2) a stale `from`;
    // (3) a no-op. Only (0) may become applicable.
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"the review cycle is too tight","target":"grain:{plan}","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"plan_revision","edits":[
              {{"path":"edges.1.max_cycles","from":2,"to":4}},
              {{"path":"retries.fetch","from":1,"to":3}}]}}}},
          {{"summary":"rewire it","target":"grain:{plan}","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"plan_revision","edits":[{{"path":"edges.0.dst","from":"review","to":"post"}}]}}}},
          {{"summary":"authored against an older plan","target":"grain:{plan}","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"plan_revision","edits":[{{"path":"edges.1.max_cycles","from":9,"to":4}}]}}}},
          {{"summary":"changes nothing","target":"grain:{plan}","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"plan_revision","edits":[{{"path":"retries.fetch","from":1,"to":1}}]}}}}
        ]}}"#
    );
    let ground = r#"{"results":[{"id":0,"supported":true},{"id":1,"supported":true},{"id":2,"supported":true},{"id":3,"supported":true}]}"#.to_string();
    let verify = r#"{"results":[{"id":0,"keep":true,"confidence":0.9},{"id":1,"keep":true,"confidence":0.9},{"id":2,"keep":true,"confidence":0.9},{"id":3,"keep":true,"confidence":0.9}]}"#.to_string();
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
        discover,
        ground,
        verify,
        enrich: r#"{"notes":[]}"#.to_string(),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();

    // All four share one target, so dedup keeps ONE rec per (family, target,
    // action) — the applicable Revise is the one that resolved.
    let plan_recs: Vec<_> = recs
        .iter()
        .filter(|r| matches!(r.origin, Origin::Llm { .. }) && r.target_ref == format!("grain:{plan}"))
        .collect();
    let revise = plan_recs
        .iter()
        .find(|r| r.action_kind == ActionKind::Revise)
        .expect("the legal plan revision");
    let Proposal::Cal { cal } = &revise.proposal else {
        panic!("a plan revision is CAL, got {:?}", revise.proposal)
    };
    assert!(cal.starts_with(&format!("SUPERSEDE {plan} WITH workflow ")), "{cal}");
    assert!(cal.contains(r#""max_cycles":4"#), "the edit landed: {cal}");
    assert!(cal.contains(r#""fetch":3"#), "the retry edit landed: {cal}");
    // Untouched topology travels through verbatim.
    assert!(cal.contains(r#""dst":"review""#), "topology preserved: {cal}");
    assert!(
        revise.summary.render().contains("edges.1.max_cycles: 2 -> 4"),
        "the reviewer sees the deltas, not a re-drawn graph: {}",
        revise.summary.render()
    );

    // The topology, stale and no-op drafts all failed to resolve — they are
    // advisory, and none of them minted a second executable plan proposal.
    assert_eq!(
        plan_recs.iter().filter(|r| r.action_kind == ActionKind::Revise).count(),
        1,
        "only the legal edit set is executable"
    );
}

#[test]
fn llm_code_revision_pins_the_tools_declared_evalset() {
    use crate::model::{ActionKind, Origin};
    use crate::recommendation::Proposal;
    let mut sub = TestSubstrate::new();
    let evalset = sub.add_fact("evalset:screen", "mg:evalset", r#"{"cases":[]}"#);
    sub.add_tool_def("screen_payment", Some(&evalset));
    sub.add_tool_def("unpinned_tool", None);
    let h1 = sub.add_tool_call("screen_payment", true, "missed an exact match");
    let _h2 = sub.add_tool_call("screen_payment", true, "missed an exact match");

    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"the matcher misses exact hits","target":"tool:screen_payment","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"code_revision","source":"def screen(p):\n    return exact_match(p)"}}}},
          {{"summary":"same for the unpinned one","target":"tool:unpinned_tool","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"code_revision","source":"def f(): pass"}}}},
          {{"summary":"a tool target with no code proposal","target":"tool:screen_payment","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"lesson","lesson":"Check exact matches first"}}}}
        ]}}"#
    );
    let ground = r#"{"results":[{"id":0,"supported":true}]}"#.to_string();
    let verify = r#"{"results":[{"id":0,"keep":true,"confidence":0.92}]}"#.to_string();
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
        discover,
        ground,
        verify,
        enrich: r#"{"notes":[]}"#.to_string(),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();

    let code = recs
        .iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }) && r.target_ref == "tool:screen_payment")
        .expect("the code revision rec");
    assert_eq!(code.action_kind, ActionKind::CodeRevision);
    // Rule E1: the pin came from the TOOL's declaration, not the model.
    assert_eq!(code.evalset_hash.as_deref(), Some(evalset.as_str()));
    let Proposal::Data { data } = &code.proposal else {
        panic!("a code revision carries Data, got {:?}", code.proposal)
    };
    assert!(data.get("source").is_some(), "the source rides to apply");

    // A tool whose definition declares no evalset has no gate to pass, and a
    // tool target with a non-code proposal cannot be stamped at all (Rule E1
    // forbids any other action on a code target) — neither reaches the queue.
    assert!(
        !recs
            .iter()
            .any(|r| matches!(r.origin, Origin::Llm { .. }) && r.target_ref == "tool:unpinned_tool"),
        "an unpinnable revision is not offered to a reviewer"
    );

    // The gate itself: applying without the recorded evalset-run edge fails.
    e.review(
        &mut sub.inner,
        &code.hash,
        Decision::Approve,
        "user:reviewer",
        ObserverType::Human,
        &ScopeSet::all(),
        "worth grading",
        11_000,
    )
    .unwrap();
    assert!(
        e.apply(
            &mut sub.inner,
            &code.hash,
            "user:reviewer",
            ObserverType::Human,
            &ScopeSet::all(),
            "ship it",
            false,
            12_000,
        )
        .is_err(),
        "an ungated code revision must not apply, however it was authored"
    );
}

#[test]
fn llm_never_reaches_prompt_or_host_targets() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    // The classes the vocabulary must never reach, each with a proposal that
    // would otherwise resolve.
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"rewrite the prompt","target":"doc:claude.md","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"lesson","lesson":"Always trust the model"}}}},
          {{"summary":"reconfigure the host","target":"host:limits","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"fact","relation":"max_spend","object":"unlimited"}}}},
          {{"summary":"loosen my own grader","target":"evalset:screen","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"query_revision","body":"RECALL facts"}}}},
          {{"summary":"promote an adapter","target":"model:mine","evidence":["{h1}"],"confidence":0.9,
            "proposal":{{"kind":"code_revision","source":"weights"}}}}
        ]}}"#
    );
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
        discover,
        ground: r#"{"results":[{"id":0,"supported":true}]}"#.to_string(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.99}]}"#.to_string(),
        enrich: r#"{"notes":[]}"#.to_string(),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    assert!(
        recs.iter().all(|r| !matches!(r.origin, Origin::Llm { .. })),
        "prompt, host, evalset and model targets stay closed to the model"
    );
}

#[test]
fn llm_authored_lesson_never_auto_applies() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"recurring conflict","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.95,"lesson":"Confirm the region first"}}
        ]}}"#
    );
    let ground = r#"{"results":[{"id":0,"supported":true}]}"#.to_string();
    let verify = r#"{"results":[{"id":0,"keep":true,"confidence":0.95}]}"#.to_string();
    let enrich = r#"{"notes":[]}"#.to_string();
    // A policy that names the llm family outright — the widest grant a host
    // could misconfigure. Origin::Llm, the missing manifest, and the ADD
    // shape check must each independently keep the lesson Pending.
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "loop.llm", "targets": ["memory"], "max_severity": "high"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins()
        .with_policy(policy)
        .with_llm(Box::new(MockLlm { discover, ground, verify, enrich }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    let rec = recs
        .iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .expect("the lesson rec");
    assert_eq!(
        rec.status,
        RecStatus::Pending,
        "an authored lesson never auto-applies, whatever the policy grants"
    );
}

#[test]
fn ground_and_verify_judge_the_lesson_text_not_just_the_summary() {
    use std::sync::{Arc, Mutex};
    struct RecordingLlm {
        inner: MockLlm,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl crate::llm::LlmBackend for RecordingLlm {
        fn model(&self) -> &str {
            "recording-mock"
        }
        fn complete(&self, request: &str) -> crate::error::Result<String> {
            self.seen.lock().unwrap().push(request.to_string());
            self.inner.complete(request)
        }
    }
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"recurring conflict","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.9,"lesson":"Confirm the region first"}}
        ]}}"#
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let e = Engine::with_builtins().with_llm(Box::new(RecordingLlm {
        inner: MockLlm {
            discover,
            ground: r#"{"results":[{"id":0,"supported":true}]}"#.to_string(),
            verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9}]}"#.to_string(),
            enrich: r#"{"notes":[]}"#.to_string(),
        },
        seen: Arc::clone(&seen),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let seen = seen.lock().unwrap();
    for op in ["\"op\":\"ground\"", "\"op\":\"verify\""] {
        let req = seen
            .iter()
            .find(|r| r.contains(op))
            .unwrap_or_else(|| panic!("no {op} request recorded"));
        assert!(
            req.contains("Proposed lesson to record") && req.contains("Confirm the region first"),
            "{op} must see the authored lesson, got: {req}"
        );
    }
}

#[test]
fn human_note_evidence_reaches_the_llm_with_text() {
    use std::sync::{Arc, Mutex};
    struct RecordingLlm {
        inner: MockLlm,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl crate::llm::LlmBackend for RecordingLlm {
        fn model(&self) -> &str {
            "recording-mock"
        }
        fn complete(&self, request: &str) -> crate::error::Result<String> {
            self.seen.lock().unwrap().push(request.to_string());
            self.inner.complete(request)
        }
    }
    let mut sub = TestSubstrate::new();
    // A human note is stored as subject + object with NO relation, so it
    // misses grain_brief's fact-triple branch. It must still render.
    sub.add_human_note(
        "expense",
        "expense_capture",
        "user:billing-lead",
        "I also need the vendor, the amount and the currency on every one.",
    );
    for _ in 0..5 {
        sub.add_tool_call("stripe_refund", true, "rate_limited 429");
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let e = Engine::with_builtins().with_llm(Box::new(RecordingLlm {
        inner: MockLlm {
            discover: r#"{"recommendations":[]}"#.to_string(),
            ground: r#"{"results":[]}"#.to_string(),
            verify: r#"{"results":[]}"#.to_string(),
            enrich: r#"{"notes":[]}"#.to_string(),
        },
        seen: Arc::clone(&seen),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let seen = seen.lock().unwrap();
    let discover = seen
        .iter()
        .find(|r| r.contains("\"op\":\"discover\""))
        .expect("a discover request");
    let v: serde_json::Value = serde_json::from_str(discover).unwrap();
    let notes: Vec<_> = v["evidence"]
        .as_array()
        .expect("evidence array")
        .iter()
        .filter(|i| i["grain_type"] == "observation")
        .collect();
    assert!(!notes.is_empty(), "the human note reaches the bundle");
    for item in notes {
        let text = item["text"].as_str().unwrap_or("");
        // Found live against a real memory: every human Observation reached
        // the model as "" — the highest-value evidence a memory holds was the
        // one shape that rendered to nothing, so an explicit instruction from
        // a person could never become a lesson.
        assert!(
            text.contains("vendor") && text.contains("currency"),
            "human-note evidence must carry its text, got: {text:?}"
        );
    }
}

#[test]
fn tool_grain_evidence_reaches_the_llm_with_text() {
    use std::sync::{Arc, Mutex};
    struct RecordingLlm {
        inner: MockLlm,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl crate::llm::LlmBackend for RecordingLlm {
        fn model(&self) -> &str {
            "recording-mock"
        }
        fn complete(&self, request: &str) -> crate::error::Result<String> {
            self.seen.lock().unwrap().push(request.to_string());
            self.inner.complete(request)
        }
    }
    let mut sub = TestSubstrate::new();
    // A dominant failure cluster: the tool_failure candidate cites these tool
    // grains, which seeds them into the DISCOVER evidence bundle.
    for _ in 0..5 {
        sub.add_tool_call("stripe_refund", true, "rate_limited 429");
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let e = Engine::with_builtins().with_llm(Box::new(RecordingLlm {
        inner: MockLlm {
            discover: r#"{"recommendations":[]}"#.to_string(),
            ground: r#"{"results":[]}"#.to_string(),
            verify: r#"{"results":[]}"#.to_string(),
            enrich: r#"{"notes":[]}"#.to_string(),
        },
        seen: Arc::clone(&seen),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let seen = seen.lock().unwrap();
    let discover = seen
        .iter()
        .find(|r| r.contains("\"op\":\"discover\""))
        .expect("a discover request");
    let v: serde_json::Value = serde_json::from_str(discover).unwrap();
    let items = v["evidence"].as_array().expect("evidence array");
    let tools: Vec<_> =
        items.iter().filter(|i| i["grain_type"] == "tool").collect();
    assert!(!tools.is_empty(), "tool grains reach the bundle");
    for item in tools {
        let text = item["text"].as_str().unwrap_or("");
        // Found live: tool grains rendered as EMPTY text, so GROUND rightly
        // refused to ground a correct lesson against evidence it couldn't
        // see. The brief must carry the tool name and the failure body.
        assert!(
            text.contains("stripe_refund") && text.contains("rate_limited"),
            "tool evidence must carry name + outcome, got: {text:?}"
        );
    }
}

#[test]
fn llm_sees_tool_failures_no_analyzer_flagged() {
    use std::sync::{Arc, Mutex};
    struct RecordingLlm {
        inner: MockLlm,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl crate::llm::LlmBackend for RecordingLlm {
        fn model(&self) -> &str {
            "recording-mock"
        }
        fn complete(&self, request: &str) -> crate::error::Result<String> {
            self.seen.lock().unwrap().push(request.to_string());
            self.inner.complete(request)
        }
    }
    let mut sub = TestSubstrate::new();
    // TWO failures against many successes: far under tool_failure's
    // threshold, so NO analyzer flags anything and `candidates` is empty.
    // The LLM must still see the failures — that is the whole point of a
    // reflection pass, and before the tool-grain seeding it saw nothing
    // here and abstained on an empty bundle.
    for _ in 0..2 {
        sub.add_tool_call("stripe_refund", true, "cancelled_before_refund");
    }
    for _ in 0..20 {
        sub.add_tool_call("stripe_refund", false, "{\"ok\":true}");
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let e = Engine::with_builtins().with_llm(Box::new(RecordingLlm {
        inner: MockLlm {
            discover: r#"{"recommendations":[]}"#.to_string(),
            ground: r#"{"results":[]}"#.to_string(),
            verify: r#"{"results":[]}"#.to_string(),
            enrich: r#"{"notes":[]}"#.to_string(),
        },
        seen: Arc::clone(&seen),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let seen = seen.lock().unwrap();
    let discover = seen
        .iter()
        .find(|r| r.contains("\"op\":\"discover\""))
        .expect("DISCOVER runs even with no deterministic finding to elaborate");
    let v: serde_json::Value = serde_json::from_str(discover).unwrap();
    assert!(
        v["findings"].as_array().is_none_or(|f| f.is_empty()),
        "precondition: no analyzer flagged anything"
    );
    let tools: Vec<_> = v["evidence"]
        .as_array()
        .expect("evidence")
        .iter()
        .filter(|i| i["grain_type"] == "tool")
        .collect();
    // Under the default policy the proposer may author skills, so successful
    // calls share the tool reserve too — AFTER the failures, which keep first
    // claim on it. Both failures are in the bundle regardless.
    let failures: Vec<_> = tools.iter().filter(|t| t["text"].as_str().unwrap_or("").contains(" error:")).collect();
    assert_eq!(failures.len(), 2, "both unflagged failures reach the bundle: {tools:?}");
    assert!(tools.len() > 2, "and, with skill authoring on, so do successes");
    for t in failures {
        let text = t["text"].as_str().unwrap_or("");
        assert!(
            text.contains("cancelled_before_refund"),
            "the failure body is what makes it actionable, got {text:?}"
        );
    }
    // With skill authoring off the bundle is exactly what it was before
    // skills existed: the failures and nothing else from the tool share.
    let seen2 = Arc::new(Mutex::new(Vec::new()));
    let e2 = Engine::with_builtins()
        .with_llm(Box::new(RecordingLlm {
            inner: MockLlm {
                discover: r#"{"recommendations":[]}"#.to_string(),
                ground: r#"{"results":[]}"#.to_string(),
                verify: r#"{"results":[]}"#.to_string(),
                enrich: r#"{"notes":[]}"#.to_string(),
            },
            seen: Arc::clone(&seen2),
        }))
        .with_policy(Policy::from_json(r#"{"skills": {"enabled": false}, "plans": {"enabled": false}}"#).unwrap());
    e2.run(&mut sub.inner, &RunOptions { full_sweep: true, ..Default::default() }, 10_001).unwrap();
    let seen2 = seen2.lock().unwrap();
    let d2: serde_json::Value =
        serde_json::from_str(seen2.iter().find(|r| r.contains("\"op\":\"discover\"")).unwrap()).unwrap();
    let tools2 = d2["evidence"].as_array().unwrap().iter().filter(|i| i["grain_type"] == "tool").count();
    assert_eq!(tools2, 2, "skills and plans off: only the failures, as before");
    for t in v["evidence"].as_array().unwrap().iter().filter(|i| i["grain_type"] == "tool").filter(|t| t["text"].as_str().unwrap_or("").contains(" error:")) {
        let text = t["text"].as_str().unwrap_or("");
        assert!(text.contains("cancelled_before_refund"));
    }

}

#[cfg(unix)]
#[test]
fn external_command_analyzer_surfaces_advisory_findings() {
    use crate::analyzer::Analyzer; // for .manifest()
    use std::os::unix::fs::PermissionsExt;
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "country", "germany");

    // A tiny analyzer: consume stdin, always emit one finding. One fixed body
    // serves both the probe (reads `id`) and analyze (reads `findings`), since
    // each reply type ignores the other's fields. Written to a space-free temp
    // path (argv is whitespace-split, like --llm-cmd).
    let script = std::env::temp_dir().join(format!("loop_ext_{}.sh", std::process::id()));
    std::fs::write(
        &script,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"id\":\"acme.ext/1\",\"title\":\"ext\",\
         \"findings\":[{\"target\":\"entity:test/acme\",\"summary\":\"external flags acme\",\
         \"severity\":\"medium\",\"evidence\":[\"deadbeef\"]}]}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let analyzer = crate::external::CommandAnalyzer::new(script.to_str().unwrap()).unwrap();
    assert_eq!(analyzer.manifest().id, "acme.ext/1");
    assert_eq!(analyzer.manifest().trust_class, crate::manifest::TrustClass::Command);
    assert_eq!(analyzer.manifest().auto_apply, crate::manifest::AutoApplyClass::Never);

    let mut e = Engine::with_builtins();
    e.register(Box::new(analyzer));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();

    let ext: Vec<_> = recs.iter().filter(|r| r.analyzer == "acme.ext/1").collect();
    assert_eq!(ext.len(), 1, "the external finding is surfaced");
    assert_eq!(ext[0].summary.render(), "external flags acme");
    assert_eq!(ext[0].severity, crate::model::Severity::Medium);
    assert!(!ext[0].destructive, "advisory flag, not a mutation");

    std::fs::remove_file(&script).ok();
}

#[test]
fn config_edit_toggles_analyzer_and_is_admin_gated() {
    use crate::config::AnalyzerConfigUpdate;
    let mut sub = TestSubstrate::new();
    // A contradiction under a functional relation → the contradiction sweep fires.
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    let e = Engine::with_builtins();

    // The analyzer id, via the read-side settings (no trait import needed).
    let cid = e
        .analyzer_settings(&sub.inner)
        .unwrap()
        .into_iter()
        .find(|s| s.id.starts_with("loop.contradiction"))
        .expect("contradiction analyzer present")
        .id;

    // Non-admin scope is denied.
    let denied = e.set_analyzer_config(
        &mut sub.inner,
        &cid,
        AnalyzerConfigUpdate { enabled: Some(false), ..Default::default() },
        &ScopeSet::of(&[Scope::Review]),
    );
    assert!(matches!(denied, Err(Error::ScopeDenied(_))), "config edit needs admin");

    // Unknown analyzer id is rejected (fail-closed).
    assert!(e
        .set_analyzer_config(
            &mut sub.inner,
            "nope.x/1",
            AnalyzerConfigUpdate::default(),
            &ScopeSet::all(),
        )
        .is_err());

    // Admin disables it; the setting flips and a run no longer surfaces it.
    e.set_analyzer_config(
        &mut sub.inner,
        &cid,
        AnalyzerConfigUpdate { enabled: Some(false), ..Default::default() },
        &ScopeSet::all(),
    )
    .unwrap();
    assert!(
        !e.analyzer_settings(&sub.inner).unwrap().iter().find(|s| s.id == cid).unwrap().enabled,
        "disabled in the effective settings"
    );
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    assert!(
        recs.iter().all(|r| !r.analyzer.starts_with("loop.contradiction")),
        "the disabled analyzer produced no findings"
    );
}

#[test]
fn full_sweep_reconsiders_grains_before_the_watermark() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "country", "germany"); // created_at 1000

    // A plain run (no llm) advances the watermark to 10_000 — past the fact.
    Engine::with_builtins()
        .run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();

    // Attach an llm that WOULD discover on that (now pre-watermark) fact.
    let discover = format!(
        r#"{{"recommendations":[{{"summary":"semantic issue on acme","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.9}}]}}"#
    );
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
        discover,
        ground: r#"{"results":[{"id":0,"supported":true}]}"#.to_string(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9}]}"#.to_string(),
        enrich: r#"{"notes":[]}"#.to_string(),
    }));

    // Incremental: discover seeds only grains since the watermark, so the old
    // fact is not in the bundle → no llm finding.
    e.run(&mut sub.inner, &RunOptions::default(), 20_000).unwrap();
    let incremental = e
        .recommendations(&sub.inner, None)
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r.origin, Origin::Llm { .. }))
        .count();
    assert_eq!(incremental, 0, "an incremental run skips pre-watermark grains");

    // Full sweep: re-seeds the whole memory → the old fact is reconsidered.
    let sweep = RunOptions { full_sweep: true, ..Default::default() };
    e.run(&mut sub.inner, &sweep, 30_000).unwrap();
    let swept = e
        .recommendations(&sub.inner, None)
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r.origin, Origin::Llm { .. }))
        .count();
    assert_eq!(swept, 1, "a full sweep reconsiders pre-watermark grains");
}

#[test]
fn no_llm_backend_is_the_identity() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    let e = Engine::with_builtins(); // no LLM attached
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    assert!(
        recs.iter().all(|r| !matches!(r.origin, Origin::Llm { .. })),
        "no llm-origin recs without a backend"
    );
}

#[test]
fn review_apply_rollback_on_nondestructive() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("stripe_refund", true, "rate_limited 429");
    }
    sub.add_tool_call("stripe_refund", false, "ok");
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();

    let recs = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap();
    let tf = recs
        .iter()
        .find(|r| r.analyzer.starts_with("loop.tool_failure"))
        .expect("a tool-failure recommendation");
    let hash = tf.hash.clone();
    assert!(!tf.destructive);

    let scopes = ScopeSet::all();
    e.review(
        &mut sub.inner,
        &hash,
        Decision::Approve,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "retries belong in the client",
        11_000,
    )
    .unwrap();
    let applied = e
        .apply(
            &mut sub.inner,
            &hash,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "applying the lesson",
            false,
            12_000,
        )
        .unwrap();
    assert!(applied.rollbackable);
    assert_eq!(
        applied.created_hashes.len(),
        1,
        "the ADD created one lesson grain"
    );
    assert_eq!(status_of(&e, &sub, &hash), RecStatus::Applied);

    e.rollback(
        &mut sub.inner,
        &hash,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "undo",
        13_000,
    )
    .unwrap();
    assert_eq!(status_of(&e, &sub, &hash), RecStatus::RolledBack);
}

#[test]
fn destructive_apply_requires_admin_and_flag() {
    let mut sub = TestSubstrate::new();
    sub.add_fact_valid_to("promo", "active", "true", 500);
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();

    let recs = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap();
    let st = recs
        .iter()
        .find(|r| r.analyzer.starts_with("loop.staleness"))
        .expect("a staleness recommendation");
    let hash = st.hash.clone();
    assert!(st.destructive);

    let scopes = ScopeSet::all();
    e.review(
        &mut sub.inner,
        &hash,
        Decision::Approve,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "expired",
        11_000,
    )
    .unwrap();

    // Without allow_destructive → gated even with admin scope.
    let denied = e.apply(
        &mut sub.inner,
        &hash,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "apply",
        false,
        12_000,
    );
    assert!(matches!(denied, Err(Error::DestructiveGated(_))));

    // With allow_destructive → applies, and is non-rollbackable.
    let ok = e
        .apply(
            &mut sub.inner,
            &hash,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "apply",
            true,
            12_000,
        )
        .unwrap();
    assert!(!ok.rollbackable, "FORGET has no inverse");
}

/// A PURGE in a proposal must stamp `destructive` (and so hit the
/// admin + allow_destructive apply gate) exactly like a FORGET — proposals
/// from LLM enrichment or `--analyzer-cmd` are arbitrary CAL text, and a
/// FORGET-only check would let bulk erasure ride through as "rollbackable".
#[test]
fn purge_proposal_is_stamped_destructive_and_gated() {
    use crate::analyzer::{AnalyzeCtx, Analyzer};
    use crate::manifest::{
        AnalyzerManifest, AutoApplyClass, CadenceClass, TargetClass, Tier, TrustClass,
    };
    use crate::model::ActionKind;
    use crate::recommendation::{Proposal, RecDraft, Summary};

    struct PurgeProposer {
        manifest: AnalyzerManifest,
    }
    impl Analyzer for PurgeProposer {
        fn manifest(&self) -> &AnalyzerManifest {
            &self.manifest
        }
        fn analyze(&self, _ctx: &AnalyzeCtx) -> crate::error::Result<Vec<RecDraft>> {
            Ok(vec![RecDraft::new(
                "grain:deadbeef",
                ActionKind::Expire,
                Summary::new("test.purge", serde_json::Map::new()),
                Proposal::Cal {
                    cal: r#"PURGE OLDER THAN 90d BECAUSE "retention""#.into(),
                },
            )])
        }
    }

    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    let mut e = Engine::with_builtins();
    e.register(Box::new(PurgeProposer {
        manifest: AnalyzerManifest {
            id: "test.purge/1".into(),
            title: "Purge proposer".into(),
            description: "test-only".into(),
            tier: Tier::T0,
            cadence: CadenceClass::Fast,
            requires: vec![],
            target_classes: vec![TargetClass::Memory],
            auto_apply: AutoApplyClass::Never,
            trust_class: TrustClass::Builtin,
            params: vec![],
            default_on: true,
        },
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();

    let recs = e.recommendations(&sub.inner, None).unwrap();
    let purge = recs
        .iter()
        .find(|r| r.analyzer == "test.purge/1")
        .expect("the purge recommendation");
    assert!(purge.destructive, "PURGE must stamp destructive");
    assert!(!purge.rollbackable, "bulk erasure has no inverse");

    let scopes = ScopeSet::all();
    let hash = purge.hash.clone();
    e.review(
        &mut sub.inner,
        &hash,
        Decision::Approve,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "retention",
        11_000,
    )
    .unwrap();
    let denied = e.apply(
        &mut sub.inner,
        &hash,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "apply",
        false,
        12_000,
    );
    assert!(
        matches!(denied, Err(Error::DestructiveGated(_))),
        "destructive gate must hold for PURGE, got {denied:?}"
    );
}

#[test]
fn apply_on_pending_is_rejected() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()[0]
        .hash
        .clone();

    // pending → applied is policy-only; a human must approve first.
    let res = e.apply(
        &mut sub.inner,
        &hash,
        "user:alice",
        ObserverType::Human,
        &ScopeSet::all(),
        "x",
        false,
        11_000,
    );
    assert!(matches!(res, Err(Error::LifecycleViolation(_))));
}

#[test]
fn self_approval_blocked_against_creator() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()[0]
        .clone();
    let creator = format!("engine:{}", rec.analyzer);

    let blocked = e.review(
        &mut sub.inner,
        &rec.hash,
        Decision::Approve,
        &creator,
        ObserverType::System,
        &ScopeSet::all(),
        "self",
        11_000,
    );
    assert!(matches!(blocked, Err(Error::SelfApproval(_))));

    // A different actor approves fine.
    assert!(e
        .review(
            &mut sub.inner,
            &rec.hash,
            Decision::Approve,
            "user:alice",
            ObserverType::Human,
            &ScopeSet::all(),
            "ok",
            11_000
        )
        .is_ok());
}

#[test]
fn llm_rec_blocks_approval_by_triggering_actor() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"prod region is ambiguous","target":"entity:test/acme","guidance":"pick one","evidence":["{h1}"],"confidence":0.9}}
        ]}}"#
    );
    let ground = r#"{"results":[{"id":0,"supported":true,"reason":"entailed"}]}"#.to_string();
    let verify =
        r#"{"results":[{"id":0,"keep":true,"confidence":0.88,"reason":"real"}]}"#.to_string();
    let enrich = r#"{"notes":[]}"#.to_string();
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm { discover, ground, verify, enrich }));

    let opts = RunOptions {
        triggering_actor: Some("user:sam".into()),
        ..Default::default()
    };
    e.run(&mut sub.inner, &opts, 10_000).unwrap();

    let recs = e.recommendations(&sub.inner, Some(RecStatus::Pending)).unwrap();
    let llm = recs
        .iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .expect("an llm-origin recommendation");
    let det = recs
        .iter()
        .find(|r| matches!(r.origin, Origin::Builtin))
        .expect("a deterministic recommendation");

    // The trigger of the LLM run cannot approve the model's own output —
    // the creator string is engine:loop.llm/1, so before co_creators this
    // approval sailed through.
    let blocked = e.review(
        &mut sub.inner,
        &llm.hash,
        Decision::Approve,
        "user:sam",
        ObserverType::Human,
        &ScopeSet::all(),
        "looks right to me",
        11_000,
    );
    assert!(matches!(blocked, Err(Error::SelfApproval(_))));

    // A different reviewer can.
    assert!(e
        .review(
            &mut sub.inner,
            &llm.hash,
            Decision::Approve,
            "user:review",
            ObserverType::Human,
            &ScopeSet::all(),
            "verified against the fork",
            11_000
        )
        .is_ok());

    // The deterministic finding stays approvable by the trigger — it is
    // computed, not authored, so its creator remains the engine.
    assert!(e
        .review(
            &mut sub.inner,
            &det.hash,
            Decision::Approve,
            "user:sam",
            ObserverType::Human,
            &ScopeSet::all(),
            "resolve to latest",
            11_000
        )
        .is_ok());
}

#[test]
fn review_requires_review_scope() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()[0]
        .hash
        .clone();

    let write_only = ScopeSet::of(&[Scope::Read, Scope::Write]);
    let res = e.review(
        &mut sub.inner,
        &hash,
        Decision::Approve,
        "user:bob",
        ObserverType::Human,
        &write_only,
        "x",
        11_000,
    );
    assert!(matches!(res, Err(Error::ScopeDenied(_))), "write ⊉ review");
}

#[test]
fn empty_because_is_rejected() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()[0]
        .hash
        .clone();

    let res = e.review(
        &mut sub.inner,
        &hash,
        Decision::Approve,
        "user:bob",
        ObserverType::Human,
        &ScopeSet::all(),
        "   ",
        11_000,
    );
    assert!(
        matches!(res, Err(Error::InvalidProposal(_))),
        "BECAUSE is mandatory"
    );
}

#[test]
fn gating_min_new_skips_but_stale_runs_first() {
    // min_new gate on a thin file skips cleanly.
    let mut sub = TestSubstrate::new();
    sub.add_fact("a", "b", "c");
    let e = Engine::with_builtins();
    let opts = RunOptions {
        min_new: Some(100),
        ..Default::default()
    };
    let r = e.run(&mut sub.inner, &opts, 10_000).unwrap();
    assert_eq!(r.outcome, RunOutcome::Skipped);
    assert_eq!(r.skip_reason, Some(SkipReason::MinNewNotMet));

    // if_stale on a never-run file runs (last_run is None).
    let mut sub2 = TestSubstrate::new();
    sub2.add_fact("a", "b", "c");
    let stale = RunOptions {
        if_stale_ms: Some(3_600_000),
        ..Default::default()
    };
    assert!(e.run(&mut sub2.inner, &stale, 10_000).unwrap().ran());
}

#[test]
fn min_new_errors_wakes_a_run() {
    let mut sub = TestSubstrate::new();
    // Two prior runs establish a watermark; then add only error events.
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 1_000)
        .unwrap();
    for _ in 0..4 {
        sub.add_tool_call("s", true, "boom 1");
    }
    // min_new very high (won't trip) but min_new_errors low (will).
    let opts = RunOptions {
        min_new: Some(1000),
        min_new_errors: Some(3),
        ..Default::default()
    };
    assert!(
        e.run(&mut sub.inner, &opts, 2_000).unwrap().ran(),
        "error gate wakes the run"
    );
}

#[test]
fn default_policy_auto_applies_nothing() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise");
    let e = Engine::with_builtins();
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 0, "a closed policy applies nothing");
    assert!(e
        .recommendations(&sub.inner, None)
        .unwrap()
        .iter()
        .all(|x| x.status == RecStatus::Pending));
}

#[test]
fn policy_grant_auto_applies_structural_consolidation() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise"); // exact dup → SUPERSEDE-only proposal
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 1, "the consolidation is auto-applied");
    let applied = e.recommendations(&sub.inner, Some(RecStatus::Applied)).unwrap();
    assert_eq!(applied.len(), 1);
    assert!(applied[0].analyzer.starts_with("loop.duplicate_sweep"));
}

#[test]
fn fork_merge_never_auto_applies_even_when_granted() {
    let mut sub = TestSubstrate::new();
    sub.add_fork("caller/john", &["ref-a", "ref-b"]);
    // A merge is SUPERSEDE-only (passes the shape check), but it is lossy, so
    // its manifest is Never — even an explicit policy grant cannot auto-apply it.
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "loop.fork_surfacing", "targets": ["memory"], "max_severity": "high"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 0, "a lossy fork merge is never auto-applied");
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|x| x.analyzer.starts_with("loop.fork_surfacing")),
        "it is proposed for human review instead"
    );
}

#[test]
fn auto_apply_never_touches_free_text_add() {
    // tool-failure proposes an ADD carrying an evidence-derived signature —
    // shape verification rejects it even when the policy names it.
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "loop.tool_failure", "targets": ["memory"], "max_severity": "high"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 0, "an ADD-with-text proposal never auto-applies");
}

/// The exact-equality half of the shape check (§6.3): a near-duplicate
/// consolidation rewrites an observation body, so it must stay pending even
/// under a policy grant — while an exact (value-identical) consolidation in
/// the same run auto-applies. This is the module-doc promise of
/// `duplicate_sweep`, enforced engine-side.
#[test]
fn near_duplicate_consolidation_never_auto_applies() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "enterprise"); // exact (case variant)
    sub.add_observation("caller", "user asked about pricing tiers refunds billing invoices today");
    sub.add_observation(
        "caller",
        "user asked about pricing tiers refunds billing invoices today please", // near, not exact
    );
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 1, "only the value-identical consolidation auto-applies");
    let recs = e.recommendations(&sub.inner, None).unwrap();
    let near = recs
        .iter()
        .find(|x| x.summary.template_id == "duplicate.near")
        .expect("the near-dup consolidation is proposed");
    assert_eq!(
        near.status,
        RecStatus::Pending,
        "a body-rewriting consolidation waits for a human"
    );
}

/// An auto-applied consolidation must keep the fact in its namespace — the
/// replacement grain carries `namespace`, so ns-scoped recall still finds the
/// value afterwards.
#[test]
fn auto_applied_consolidation_preserves_namespace() {
    use crate::substrate::{ReadOpts, SubstrateRead};
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise");
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 1);
    let live = sub
        .inner
        .grains_of_type("fact", None, ReadOpts { live_only: true, since_ms: None })
        .unwrap();
    let acme: Vec<_> = live
        .iter()
        .filter(|g| g.fact_subject() == Some("acme"))
        .collect();
    assert!(!acme.is_empty());
    assert!(
        acme.iter().all(|g| g.namespace == "test"),
        "no replacement grain escaped to the store default namespace"
    );
}

/// An applied tool-failure lesson lands in the namespace of its evidence, so
/// the ns-scoped recall the agent actually runs can surface it.
#[test]
fn applied_lesson_lands_in_evidence_namespace() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("stripe_refund", true, "rate_limited 429");
    }
    sub.add_tool_call("stripe_refund", false, "ok");
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 1_000_000).unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.analyzer.starts_with("loop.tool_failure"))
        .expect("a tool-failure lesson")
        .hash;
    let scopes = ScopeSet::all();
    e.review(&mut sub.inner, &hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "codify", 1_000_100).unwrap();
    let applied = e
        .apply(&mut sub.inner, &hash, "user:a", ObserverType::Human, &scopes, "apply", false, 1_000_200)
        .unwrap();
    let created = applied.created_hashes.first().expect("the lesson grain");
    use crate::substrate::SubstrateRead;
    let grain = sub.inner.grain(created).unwrap().expect("stored");
    assert_eq!(
        grain.namespace, "test",
        "the lesson inherits the evidence tool calls' namespace"
    );
}

#[test]
fn policy_deny_disables_an_analyzer() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise");
    let policy = Policy::from_json(r#"{"deny": ["loop.duplicate_sweep"]}"#).unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert!(
        e.recommendations(&sub.inner, None)
            .unwrap()
            .iter()
            .all(|x| !x.analyzer.starts_with("loop.duplicate_sweep")),
        "a denied analyzer produces nothing"
    );
}

const DAY: i64 = 86_400_000;

/// Apply a tool-failure lesson; return (engine, sub, rec_hash) at apply time T.
fn apply_lesson(now: i64) -> (Engine, TestSubstrate, String) {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call_at("stripe_refund", true, "rate_limited 429", 1_000);
    }
    sub.add_tool_call_at("stripe_refund", false, "ok", 1_100);
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 1_000_000).unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.analyzer.starts_with("loop.tool_failure"))
        .expect("a tool-failure lesson")
        .hash;
    let scopes = ScopeSet::all();
    e.review(&mut sub.inner, &hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "codify", now).unwrap();
    e.apply(&mut sub.inner, &hash, "user:a", ObserverType::Human, &scopes, "apply the rule", false, now).unwrap();
    (e, sub, hash)
}

/// The multi-horizon Verify gate: a LATE recurrence that early checkpoints miss
/// is caught at a later one — held at 1d, held at 7d, regressed at 30d.
#[test]
fn outcome_time_series_catches_a_late_regression() {
    let t = 2_000_000;
    let (e, mut sub, hash) = apply_lesson(t);

    // Measure the 1d and 7d checkpoints — no recurrence yet.
    e.run(&mut sub.inner, &RunOptions::default(), t + 2 * DAY).unwrap();
    e.run(&mut sub.inner, &RunOptions::default(), t + 8 * DAY).unwrap();

    // The failure recurs at day 20 — after the early checkpoints.
    for _ in 0..2 {
        sub.add_tool_call_at("stripe_refund", true, "rate_limited 429", t + 20 * DAY);
    }

    // Measure the 30d checkpoint.
    e.run(&mut sub.inner, &RunOptions::default(), t + 31 * DAY).unwrap();

    let series: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.rec_hash == hash)
        .collect();
    let verdict_at = |h: i64| series.iter().find(|o| o.horizon_ms == h).map(|o| o.verdict.as_str());
    assert_eq!(verdict_at(DAY), Some("held"), "no recurrence at day 1");
    assert_eq!(verdict_at(7 * DAY), Some("held"), "still held at day 7");
    assert_eq!(verdict_at(30 * DAY), Some("regressed"), "the late recurrence is caught at day 30");
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("loop.outcome_review")),
        "a revert is proposed once the regression appears"
    );

    // The regression recommendation is not merely an audit transition: apply
    // it and prove the original lesson grain is retracted through the same
    // rollback path as a direct human rollback.
    use crate::substrate::{ReadOpts, SubstrateRead};
    let lesson = sub
        .inner
        .grains_of_type("fact", Some("test"), ReadOpts::default())
        .unwrap()
        .into_iter()
        .find(|g| {
            g.fields.get("subject").and_then(serde_json::Value::as_str)
                == Some("stripe_refund")
                && g.fields.get("relation").and_then(serde_json::Value::as_str)
                    == Some("fails_with")
        })
        .expect("applied lesson grain");
    let revert = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.analyzer.starts_with("loop.outcome_review"))
        .expect("revert recommendation");
    let scopes = ScopeSet::all();
    e.review(
        &mut sub.inner,
        &revert.hash,
        Decision::Approve,
        "user:a",
        ObserverType::Human,
        &scopes,
        "regression confirmed",
        t + 31 * DAY + 1,
    )
    .unwrap();
    e.apply(
        &mut sub.inner,
        &revert.hash,
        "user:a",
        ObserverType::Human,
        &scopes,
        "revert regressed lesson",
        false,
        t + 31 * DAY + 2,
    )
    .unwrap();
    assert_eq!(status_of(&e, &sub, &hash), RecStatus::RolledBack);
    assert_eq!(status_of(&e, &sub, &revert.hash), RecStatus::Applied);
    assert!(
        !sub.inner.grain(&lesson.hash).unwrap().unwrap().is_live(),
        "the lesson created by the original apply must be retracted"
    );
}

/// A revert the Verify gate proposed is a verdict on the FINDING: once a
/// reviewer applies it, the reverted lesson goes on the rejection cooldown
/// and the next pass does not re-propose it — even though the failure
/// cluster that produced it is still there. An operator's own rollback earns
/// no cooldown, so the same cluster re-proposes the same lesson at once.
/// Both arms share every input up to the rollback mechanism, which is what
/// makes the difference attributable to it.
#[test]
fn a_measured_revert_cools_down_the_reverted_finding_but_a_manual_rollback_does_not() {
    use crate::substrate::OmsSubstrate;
    let t = 2_000_000;
    let scopes = ScopeSet::all();
    let cooldown_of = |sub: &TestSubstrate, dk: &str| -> Option<i64> {
        crate::config::LoopPersisted::from_value(sub.inner.load_state().unwrap())
            .unwrap()
            .cooldowns
            .get(dk)
            .copied()
    };
    let pending_lesson = |e: &Engine, sub: &TestSubstrate, dk: &str| -> bool {
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("loop.tool_failure") && r.dedup_key == dk)
    };
    // Shared prefix: apply the lesson, pass the early checkpoints, then the
    // failure comes back in force — enough for the analyzer to fire again on
    // its own, not only for the recurrence metric to regress.
    let regress = |now: i64| -> (Engine, TestSubstrate, String, String) {
        let (e, mut sub, hash) = apply_lesson(now);
        let dk = load_dedup_key(&e, &sub, &hash);
        e.run(&mut sub.inner, &RunOptions::default(), now + 2 * DAY).unwrap();
        e.run(&mut sub.inner, &RunOptions::default(), now + 8 * DAY).unwrap();
        for _ in 0..5 {
            sub.add_tool_call_at("stripe_refund", true, "rate_limited 429", now + 30 * DAY);
        }
        sub.add_tool_call_at("stripe_refund", false, "ok", now + 30 * DAY + 1);
        e.run(&mut sub.inner, &RunOptions::default(), now + 31 * DAY).unwrap();
        (e, sub, hash, dk)
    };

    // Arm 1 — the Verify gate's revert, approved and applied.
    let (e, mut sub, hash, dk) = regress(t);
    let revert = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.analyzer.starts_with("loop.outcome_review"))
        .expect("the regression proposed a revert");
    assert!(cooldown_of(&sub, &dk).is_none(), "no cooldown before the revert");
    e.review(&mut sub.inner, &revert.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "regression confirmed", t + 31 * DAY + 1)
        .unwrap();
    let applied_at = t + 31 * DAY + 2;
    e.apply(&mut sub.inner, &revert.hash, "user:a", ObserverType::Human, &scopes, "revert regressed lesson", false, applied_at)
        .unwrap();
    assert_eq!(status_of(&e, &sub, &hash), RecStatus::RolledBack);
    assert_eq!(
        cooldown_of(&sub, &dk),
        Some(applied_at + 7 * DAY),
        "a measured revert earns the reverted finding a first-strike (7d) cooldown"
    );
    e.run(&mut sub.inner, &RunOptions::default(), applied_at + 1).unwrap();
    assert!(
        !pending_lesson(&e, &sub, &dk),
        "the lesson the gate just retracted must not be re-proposed on the next pass"
    );
    // ...and once the cooldown lapses the situation is judged afresh.
    e.run(&mut sub.inner, &RunOptions::default(), applied_at + 7 * DAY + 1).unwrap();
    assert!(
        pending_lesson(&e, &sub, &dk),
        "after the cooldown the still-present cluster re-proposes the lesson"
    );

    // Arm 2 — the same state, rolled back by an operator instead.
    let (e, mut sub, hash, dk) = regress(t);
    let rolled_at = t + 31 * DAY + 2;
    e.rollback(&mut sub.inner, &hash, "user:a", ObserverType::Human, &scopes, "retract it by hand", rolled_at)
        .unwrap();
    assert_eq!(status_of(&e, &sub, &hash), RecStatus::RolledBack);
    assert!(cooldown_of(&sub, &dk).is_none(), "an operator rollback earns no cooldown");
    e.run(&mut sub.inner, &RunOptions::default(), rolled_at + 1).unwrap();
    assert!(
        pending_lesson(&e, &sub, &dk),
        "after a manual rollback the still-present cluster re-proposes the lesson at once"
    );
}

fn load_dedup_key(e: &Engine, sub: &TestSubstrate, hash: &str) -> String {
    e.recommendations(&sub.inner, None)
        .unwrap()
        .into_iter()
        .find(|r| r.hash == hash)
        .expect("the applied recommendation is listed")
        .dedup_key
}

/// No recurrence at any checkpoint → the fix held across the whole series, no
/// revert ever proposed.
#[test]
fn outcome_time_series_holds_when_fix_works() {
    let t = 2_000_000;
    let (e, mut sub, hash) = apply_lesson(t);
    sub.add_tool_call_at("stripe_refund", false, "ok", t + 10 * DAY); // only a success
    e.run(&mut sub.inner, &RunOptions::default(), t + 31 * DAY).unwrap();

    let series: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.rec_hash == hash)
        .collect();
    assert_eq!(series.len(), 3, "all three checkpoints measured");
    assert!(series.iter().all(|o| o.verdict == "held"), "held throughout");
    assert!(
        !e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("loop.outcome_review")),
        "no revert when the fix held"
    );
}

/// Apply a contradiction resolution; return (engine, sub, rec_hash).
fn apply_resolution(now: i64) -> (Engine, TestSubstrate, String) {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 1_000_000).unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.analyzer.starts_with("loop.contradiction_sweep"))
        .expect("a contradiction resolution")
        .hash;
    let scopes = ScopeSet::all();
    e.review(&mut sub.inner, &hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "latest wins", now).unwrap();
    e.apply(&mut sub.inner, &hash, "user:a", ObserverType::Human, &scopes, "resolve", false, now).unwrap();
    (e, sub, hash)
}

/// The contradiction-recurrence metric: held while the subject keeps one live
/// value; a NEW conflicting value after apply regresses a later checkpoint and
/// a revert is proposed — the Verify gate now covers resolutions, not just
/// tool lessons.
#[test]
fn contradiction_outcome_regresses_when_conflict_returns() {
    let t = 2_000_000;
    let (e, mut sub, hash) = apply_resolution(t);

    // 1d checkpoint: resolved — one live value → held.
    e.run(&mut sub.inner, &RunOptions::default(), t + 2 * DAY).unwrap();

    // A new conflicting value arrives after the early checkpoint.
    sub.add_fact("acme", "deploy_target", "ap-south-1");

    // 7d checkpoint: two live values again → regressed.
    e.run(&mut sub.inner, &RunOptions::default(), t + 8 * DAY).unwrap();

    let series: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.rec_hash == hash)
        .collect();
    let verdict_at = |h: i64| series.iter().find(|o| o.horizon_ms == h).map(|o| o.verdict.as_str());
    assert_eq!(verdict_at(DAY), Some("held"), "one live value at day 1");
    assert_eq!(verdict_at(7 * DAY), Some("regressed"), "the returned conflict is caught at day 7");
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("loop.outcome_review")),
        "a revert is proposed for the regressed resolution"
    );
}

#[test]
fn contradiction_outcome_holds_when_resolution_sticks() {
    let t = 2_000_000;
    let (e, mut sub, hash) = apply_resolution(t);
    e.run(&mut sub.inner, &RunOptions::default(), t + 31 * DAY).unwrap();
    let series: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.rec_hash == hash)
        .collect();
    assert_eq!(series.len(), 3, "all three checkpoints measured");
    assert!(series.iter().all(|o| o.verdict == "held"), "held throughout");
}

fn status_of(e: &Engine, sub: &TestSubstrate, hash: &str) -> RecStatus {
    e.recommendations(&sub.inner, None)
        .unwrap()
        .into_iter()
        .find(|r| r.hash == hash)
        .unwrap()
        .status
}

// ── Regression: mechanism-traced findings fixed 2026-07-25 ──────────────────

/// #A6F1 — a tool lesson's outcome metric is scoped to its failure signature;
/// an unrelated later failure of the SAME tool must not read as a regression.
#[test]
fn tool_lesson_holds_on_unrelated_same_tool_failure() {
    let t = 2_000_000;
    let (e, mut sub, hash) = apply_lesson(t); // lesson signature = "rate_limited #"
    // A different-signature failure of the same tool, after the apply.
    for _ in 0..3 {
        sub.add_tool_call_at("stripe_refund", true, "insufficient_funds 402", t + 2 * DAY);
    }
    e.run(&mut sub.inner, &RunOptions::default(), t + 31 * DAY).unwrap();
    let series: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.rec_hash == hash)
        .collect();
    assert!(!series.is_empty(), "the metric was measured");
    assert!(
        series.iter().all(|o| o.verdict == "held"),
        "an unrelated-signature failure must not regress the lesson: {:?}",
        series.iter().map(|o| (o.horizon_ms, o.verdict.clone())).collect::<Vec<_>>()
    );
    assert!(
        !e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("loop.outcome_review")),
        "no revert proposed for an unrelated failure"
    );
}

/// #A6F2 — the value-identical auto-apply gate fails closed when the superseded
/// grain carries information the {s,r,o,ns} replacement can't preserve.
#[test]
fn auto_apply_blocks_dropped_expiry() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise"); // canonical (earliest)
    sub.add_fact_valid_to("acme", "tier", "Enterprise", 999_999); // dup WITH an expiry
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 0, "a dup carrying a valid_to must not auto-apply (expiry would be lost)");
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("loop.duplicate_sweep")),
        "it stays pending for human review"
    );
}

/// A bare-value duplicate (only s/r/o/ns, no expiry) still auto-applies — the
/// narrowed A6F2 guard must not block legitimate consolidation.
#[test]
fn auto_apply_still_consolidates_plain_duplicates() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise");
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 1, "plain value-identical duplicates still consolidate");
}

/// #A6F4 — an empty/whitespace error signature yields no (unappliable) lesson.
#[test]
fn empty_signature_cluster_not_proposed() {
    let mut sub = TestSubstrate::new();
    for _ in 0..6 {
        sub.add_tool_call("flaky", true, "   "); // whitespace-only error body → sig ""
    }
    let drafts = sub.analyze(&crate::analyzers::tool_failure::ToolFailureClustering::new(), 10_000);
    assert!(drafts.is_empty(), "an empty-signature cluster must not be proposed, got {}", drafts.len());
}

/// #A6F3 — rejection cooldown backs off exponentially (7d → 14d), not a flat 7d.
#[test]
fn rejection_cooldown_doubles() {
    let e = Engine::with_builtins();
    let scopes = ScopeSet::all();
    let reject_at = |sub: &mut TestSubstrate, now: i64| -> String {
        e.run(&mut sub.inner, &RunOptions::default(), now).unwrap();
        let rec = e
            .recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .into_iter()
            .find(|r| r.analyzer.starts_with("loop.contradiction"))
            .expect("a contradiction recommendation");
        let dk = rec.dedup_key.clone();
        e.review(&mut sub.inner, &rec.hash, Decision::Reject, "user:a", ObserverType::Human, &scopes, "no", now)
            .unwrap();
        dk
    };
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1"); // contradiction
    let t0 = 10_000;
    let dk = reject_at(&mut sub, t0);
    let cooldown = |sub: &TestSubstrate, dk: &str| -> i64 {
        use crate::substrate::OmsSubstrate;
        crate::config::LoopPersisted::from_value(sub.inner.load_state().unwrap())
            .unwrap()
            .cooldowns
            .get(dk)
            .copied()
            .unwrap()
    };
    assert_eq!(cooldown(&sub, &dk) - t0, 7 * DAY, "first rejection = 7d");
    // Re-propose after the first cooldown elapses, reject again.
    let t1 = cooldown(&sub, &dk) + 1;
    reject_at(&mut sub, t1);
    assert_eq!(cooldown(&sub, &dk) - t1, 14 * DAY, "second rejection doubles to 14d");
}

#[test]
fn advisory_findings_are_refused_by_preflight_not_after_approval() {
    use crate::engine::ensure_executable;
    use crate::model::Origin;
    use crate::recommendation::Proposal;
    use serde_json::{json, Map};

    // The shared gate, pinned directly: exactly one Data shape is executable.
    use crate::model::ActionKind as AK;
    assert!(ensure_executable(AK::Flag, &Proposal::Cal { cal: "ADD fact …".into() }).is_ok());
    assert!(matches!(
        ensure_executable(AK::Flag, &Proposal::Edit {
            format: "md".into(),
            base_digest: "d".into(),
            diff: "-a\n+b".into(),
        }),
        Err(Error::InvalidProposal(_))
    ));
    let mut advisory = Map::new();
    advisory.insert("note".into(), json!("go look at this"));
    assert!(matches!(
        ensure_executable(AK::Flag, &Proposal::Data { data: advisory }),
        Err(Error::InvalidProposal(_))
    ));
    let mut revert = Map::new();
    revert.insert("revert_of".into(), json!("abc123"));
    assert!(ensure_executable(AK::Revert, &Proposal::Data { data: revert }).is_ok());
    // The gated revisions are executable Data shapes — the promotion write.
    assert!(ensure_executable(AK::CodeRevision, &Proposal::Data { data: Map::new() }).is_ok());
    assert!(ensure_executable(AK::AdapterRevision, &Proposal::Data { data: Map::new() }).is_ok());

    // End to end: an LLM finding is advisory, and preflight refuses it *before*
    // the fused approve-and-apply path commits an approval it cannot undo.
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"prod region is ambiguous","target":"entity:test/acme","guidance":"pick one","evidence":["{h1}"],"confidence":0.9}}
        ]}}"#
    );
    let e = Engine::with_builtins().with_llm(Box::new(MockLlm {
        discover,
        ground: r#"{"results":[{"id":0,"supported":true,"reason":"entailed"}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.88,"reason":"real"}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();

    let llm = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .expect("an llm-origin recommendation");

    let refused = e.preflight_apply(&sub.inner, &llm.hash, &ScopeSet::all(), true, false);
    assert!(
        matches!(refused, Err(Error::InvalidProposal(_))),
        "preflight must refuse an advisory finding, got {refused:?}"
    );
    assert_eq!(
        status_of(&e, &sub, &llm.hash),
        RecStatus::Pending,
        "a refused preflight must leave the finding dismissible"
    );

    // Approving it stays legal — a Flag is acknowledged, not executed.
    e.review(
        &mut sub.inner,
        &llm.hash,
        Decision::Approve,
        "user:reviewer",
        ObserverType::Human,
        &ScopeSet::all(),
        "real issue, handling it in the host",
        11_000,
    )
    .expect("acknowledging an advisory finding is legal");

    // But applying it still refuses, with guidance rather than a bare error.
    match e.apply(
        &mut sub.inner,
        &llm.hash,
        "user:reviewer",
        ObserverType::Human,
        &ScopeSet::all(),
        "try to apply",
        true,
        12_000,
    ) {
        Err(Error::InvalidProposal(msg)) => {
            assert!(msg.contains("advisory"), "message should explain, got {msg:?}")
        }
        other => panic!("expected an advisory refusal, got {other:?}"),
    }
}

/// §7.4 end-to-end (the Wave-4 gate): a code revision proposed by an
/// analyzer, reviewed by a human, REFUSED without the evalset-run edge,
/// refused with mismatched/stale/failing gates, applied only through
/// `apply_gated` with the pin live and the gate clean — and the audit
/// Observation carries the recorded edge.
#[test]
fn governed_code_change_applies_only_through_the_gate() {
    use crate::analyzer::{AnalyzeCtx, Analyzer};
    use crate::manifest::{
        AnalyzerManifest, AutoApplyClass, CadenceClass, TargetClass, Tier, TrustClass,
    };
    use crate::model::ActionKind;
    use crate::recommendation::{GatingEvidence, Proposal, RecDraft, Summary};

    struct CodeProposer {
        manifest: AnalyzerManifest,
        evalset: String,
    }
    impl Analyzer for CodeProposer {
        fn manifest(&self) -> &AnalyzerManifest {
            &self.manifest
        }
        fn analyze(&self, _ctx: &AnalyzeCtx) -> crate::error::Result<Vec<RecDraft>> {
            let mut args = serde_json::Map::new();
            args.insert("text".into(), serde_json::Value::from("faster retry backoff"));
            let mut data = serde_json::Map::new();
            data.insert("code_blob".into(), serde_json::Value::from("cas://sha256:feed"));
            Ok(vec![RecDraft::new(
                "tool:cafe0123",
                ActionKind::CodeRevision,
                Summary::new("command.finding", args),
                Proposal::Data { data },
            )
            .evalset_hash(self.evalset.clone())])
        }
    }

    let mut sub = TestSubstrate::new();
    // The evalset is itself a grain — the pin's liveness is checked at apply.
    let evalset_hash = sub.add_fact("evalset:retry", "mg:evalset", "{\"cases\":[]}");
    let mut e = Engine::with_builtins();
    e.register(Box::new(CodeProposer {
        manifest: AnalyzerManifest {
            id: "test.codegen/1".into(),
            title: "Code proposer".into(),
            description: "test-only".into(),
            tier: Tier::T0,
            cadence: CadenceClass::Fast,
            requires: vec![],
            target_classes: vec![TargetClass::Host],
            auto_apply: AutoApplyClass::Never,
            trust_class: TrustClass::Builtin,
            params: vec![],
            default_on: true,
        },
        evalset: evalset_hash.clone(),
    }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();

    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.action_kind == ActionKind::CodeRevision)
        .expect("the code revision");
    assert_eq!(rec.evalset_hash.as_deref(), Some(evalset_hash.as_str()));
    let hash = rec.hash.clone();
    let scopes = ScopeSet::all();
    e.review(
        &mut sub.inner, &hash, Decision::Approve, "user:reviewer",
        ObserverType::Human, &scopes, "diff reviewed", 11_000,
    )
    .unwrap();

    // 1. Plain apply — no gating edge — REFUSED.
    match e.apply(&mut sub.inner, &hash, "user:reviewer", ObserverType::Human, &scopes, "ship it", false, 12_000) {
        Err(Error::InvalidProposal(m)) => assert!(m.contains("gating run"), "{m}"),
        other => panic!("ungated apply must refuse, got {other:?}"),
    }
    // 2. Gated against the WRONG evalset — refused (Rule E1's pin).
    let wrong = GatingEvidence { evalset_hash: "not-the-pin".into(), run_id: "eval-1".into(), passed: 3, failed: 0 };
    match e.apply_gated(&mut sub.inner, &hash, "user:reviewer", ObserverType::Human, &scopes, "ship", false, &wrong, 12_100) {
        Err(Error::InvalidProposal(m)) => assert!(m.contains("pinned"), "{m}"),
        other => panic!("wrong-evalset apply must refuse, got {other:?}"),
    }
    // 3. A FAILING gate admits nothing.
    let failing = GatingEvidence { evalset_hash: evalset_hash.clone(), run_id: "eval-2".into(), passed: 2, failed: 1 };
    match e.apply_gated(&mut sub.inner, &hash, "user:reviewer", ObserverType::Human, &scopes, "ship", false, &failing, 12_200) {
        Err(Error::InvalidProposal(m)) => assert!(m.contains("failing gate"), "{m}"),
        other => panic!("failing-gate apply must refuse, got {other:?}"),
    }
    // 4. Clean gate, live pin: the ONLY path that applies — and the audit
    // Observation records the edge.
    let clean = GatingEvidence { evalset_hash: evalset_hash.clone(), run_id: "eval-3".into(), passed: 3, failed: 0 };
    e.apply_gated(&mut sub.inner, &hash, "user:reviewer", ObserverType::Human, &scopes, "gated and green", false, &clean, 12_300)
        .unwrap();
    let audits = {
        use crate::substrate::SubstrateRead;
        sub.inner
            .grains_of_type("observation", Some("areev-loop"), Default::default())
            .unwrap()
    };
    let applied_audit = audits
        .iter()
        .find(|a| {
            a.str_field("rec_hash") == Some(hash.as_str())
                && a.str_field("to_status") == Some("applied")
        })
        .expect("the applied audit record");
    assert_eq!(applied_audit.str_field("gating_evalset"), Some(evalset_hash.as_str()));
    assert_eq!(applied_audit.str_field("gating_run_id"), Some("eval-3"));

    // 5. Superseding the evalset AFTER apply invalidates FUTURE pins: a
    // second code rec pinned to the old evalset re-gates.
    e.run(&mut sub.inner, &RunOptions::default(), 20_000).unwrap(); // dedup: no new rec
    {
        use crate::substrate::{GrainSpec, OmsSubstrate};
        let spec = GrainSpec::new("fact", "test")
            .with_field("subject", "evalset:retry")
            .with_field("relation", "mg:evalset")
            .with_field("object", "{\"cases\":[1]}");
        sub.inner
            .supersede(&evalset_hash, &spec, "evalset v2")
            .unwrap();
    }
    // Fresh engine+proposer so a new pending rec pinned to the OLD hash exists.
    let mut e2 = Engine::with_builtins();
    e2.register(Box::new(CodeProposer {
        manifest: AnalyzerManifest {
            id: "test.codegen2/1".into(),
            title: "Code proposer 2".into(),
            description: "test-only".into(),
            tier: Tier::T0,
            cadence: CadenceClass::Fast,
            requires: vec![],
            target_classes: vec![TargetClass::Host],
            auto_apply: AutoApplyClass::Never,
            trust_class: TrustClass::Builtin,
            params: vec![],
            default_on: true,
        },
        evalset: evalset_hash.clone(),
    }));
    e2.run(&mut sub.inner, &RunOptions::default(), 21_000).unwrap();
    let rec2 = e2
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.action_kind == ActionKind::CodeRevision && r.analyzer.starts_with("test.codegen2"))
        .expect("second code revision");
    e2.review(&mut sub.inner, &rec2.hash, Decision::Approve, "user:reviewer", ObserverType::Human, &scopes, "ok", 22_000).unwrap();
    let stale = GatingEvidence { evalset_hash: evalset_hash.clone(), run_id: "eval-4".into(), passed: 3, failed: 0 };
    match e2.apply_gated(&mut sub.inner, &rec2.hash, "user:reviewer", ObserverType::Human, &scopes, "ship", false, &stale, 23_000) {
        Err(Error::InvalidProposal(m)) => assert!(m.contains("superseded"), "{m}"),
        other => panic!("stale-pin apply must refuse (re-gate), got {other:?}"),
    }
}

/// The tuning seam end-to-end: an `mg:adapter` registry grain (what
/// `areev tune` writes) is picked up by the builtin `adapter_intake`
/// analyzer, gated exactly like a code revision (refused ungated /
/// mismatched / failing), applied only through a clean recorded run of the
/// pinned evalset — writing the `mg:adapter_promotion` Fact hosts re-resolve
/// from — and rolled back by retracting it, after which the candidate is
/// re-proposed (the situation returned) until its registry grain is retired.
#[test]
fn governed_adapter_promotion_applies_only_through_the_gate() {
    use crate::model::ActionKind;
    use crate::recommendation::GatingEvidence;

    let mut sub = TestSubstrate::new();
    let evalset_hash = sub.add_fact("evalset:support", "mg:evalset", "{\"cases\":[]}");
    let tuple = serde_json::json!({
        "adapter": {"uri": "file:///adapters/a.safetensors", "sha256": "feed"},
        "base_model": "qwen3-4b",
        "quantization": "bf16",
        "serving_runtime": "vllm",
        "serves_as": "acme-support",
        "evalset_hash": evalset_hash,
        "corpus_manifest": "cafe0123",
    })
    .to_string();
    let adapter_grain =
        sub.add_fact_at("agent:harness", "model:acme-support", "mg:adapter", &tuple, 9_000);

    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();

    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.action_kind == ActionKind::AdapterRevision)
        .expect("the adapter revision");
    assert_eq!(rec.evalset_hash.as_deref(), Some(evalset_hash.as_str()));
    assert_eq!(rec.target_ref, "model:acme-support");
    assert!(rec.evidence.contains(&adapter_grain));
    // Auto-apply is structurally impossible: the run left it Pending even
    // though the engine ran with its default (and any) policy — the class is
    // excluded by name and the analyzer is AutoApplyClass::Never.
    assert_eq!(rec.status, RecStatus::Pending);
    let hash = rec.hash.clone();
    let scopes = ScopeSet::all();
    e.review(
        &mut sub.inner, &hash, Decision::Approve, "user:reviewer",
        ObserverType::Human, &scopes, "corpus + lineage reviewed", 11_000,
    )
    .unwrap();

    // 1. Ungated apply — refused.
    match e.apply(&mut sub.inner, &hash, "user:reviewer", ObserverType::Human, &scopes, "ship it", false, 12_000) {
        Err(Error::InvalidProposal(m)) => assert!(m.contains("gating run"), "{m}"),
        other => panic!("ungated adapter apply must refuse, got {other:?}"),
    }
    // 2. Wrong evalset — refused (Rule E1's pin).
    let wrong = GatingEvidence { evalset_hash: "not-the-pin".into(), run_id: "eval-1".into(), passed: 3, failed: 0 };
    match e.apply_gated(&mut sub.inner, &hash, "user:reviewer", ObserverType::Human, &scopes, "ship", false, &wrong, 12_100) {
        Err(Error::InvalidProposal(m)) => assert!(m.contains("pinned"), "{m}"),
        other => panic!("wrong-evalset apply must refuse, got {other:?}"),
    }
    // 3. A failing gate admits nothing.
    let failing = GatingEvidence { evalset_hash: evalset_hash.clone(), run_id: "eval-2".into(), passed: 2, failed: 1 };
    match e.apply_gated(&mut sub.inner, &hash, "user:reviewer", ObserverType::Human, &scopes, "ship", false, &failing, 12_200) {
        Err(Error::InvalidProposal(m)) => assert!(m.contains("failing gate"), "{m}"),
        other => panic!("failing-gate apply must refuse, got {other:?}"),
    }
    // 4. Clean gate, live pin: applies, writing the promotion Fact.
    let clean = GatingEvidence { evalset_hash: evalset_hash.clone(), run_id: "eval-3".into(), passed: 3, failed: 0 };
    e.apply_gated(&mut sub.inner, &hash, "user:reviewer", ObserverType::Human, &scopes, "gated and green", false, &clean, 12_300)
        .unwrap();
    let promotion = {
        use crate::substrate::SubstrateRead;
        sub.inner
            .grains_of_type("fact", Some("areev-loop"), Default::default())
            .unwrap()
            .into_iter()
            .find(|g| g.str_field("relation") == Some("mg:adapter_promotion") && g.is_live())
            .expect("the promotion grain")
    };
    assert_eq!(promotion.str_field("subject"), Some("model:acme-support"));
    assert_eq!(promotion.str_field("gating_evalset"), Some(evalset_hash.as_str()));
    assert_eq!(promotion.str_field("gating_run_id"), Some("eval-3"));

    // 5. One candidate per served model: while the promotion is live, the
    // analyzer proposes nothing new for this subject.
    e.run(&mut sub.inner, &RunOptions::default(), 13_000).unwrap();
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .all(|r| r.action_kind != ActionKind::AdapterRevision),
        "a promoted model must not re-propose"
    );

    // 6. Rollback retracts the promotion (the recorded inverse) …
    e.rollback(&mut sub.inner, &hash, "user:reviewer", ObserverType::Human, &scopes, "regressed in prod", 14_000)
        .unwrap();
    {
        use crate::substrate::SubstrateRead;
        let live = sub.inner
            .grains_of_type("fact", Some("areev-loop"), Default::default())
            .unwrap()
            .into_iter()
            .filter(|g| g.str_field("relation") == Some("mg:adapter_promotion") && g.is_live())
            .count();
        assert_eq!(live, 0, "rollback must retract the promotion");
    }
    // … and the still-live candidate is re-proposed — the situation
    // returned. Retiring the mg:adapter grain is how a host silences it.
    e.run(&mut sub.inner, &RunOptions::default(), 15_000).unwrap();
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.action_kind == ActionKind::AdapterRevision),
        "post-rollback the candidate must be re-proposed"
    );
}

// ── Issue #29: evalset-backed outcome metric ──────────────────────────────
// `areev loop outcomes` could only close the receipt on internal recurrence
// counts. An evalset run is ALSO internal, bounded and attributable, so it can
// carry external correctness without breaking the honesty rule — provided the
// measurement never scores against a run that predates the apply.
mod evalset_outcome {
    use super::*;
    use crate::recommendation::MetricSnapshot;

    const EVALSET: &str = "abc123";

    fn snapshot(field: &str, baseline: f64, higher_is_better: bool) -> MetricSnapshot {
        MetricSnapshot {
            metric: format!("evalset:{EVALSET}:{field}"),
            baseline,
            unit: "ratio".into(),
            n: 184,
            window: "evalset".into(),
            subject: None,
            namespace: None,
            relation: None,
            query: String::new(),
            review_after_ms: 86_400_000,
            horizons_ms: vec![],
            checkpoints: Vec::new(),
            higher_is_better,
        }
    }

    /// Journal a summary the way `areev eval run` does.
    fn journal_run(sub: &mut TestSubstrate, run_id: &str, at_ms: i64, extra: serde_json::Value) {
        let mut summary = serde_json::json!({"run_id": run_id, "passed": 1, "failed": 0});
        if let (Some(o), Some(e)) = (summary.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        sub.add_fact_at(
            "agent:harness",
            &format!("evalset:{EVALSET}"),
            "mg:eval_run",
            &summary.to_string(),
            at_ms,
        );
    }

    fn measure(sub: &TestSubstrate, m: &MetricSnapshot, since: i64) -> Option<f64> {
        crate::engine::measure_metric(&sub.inner, m, since).unwrap()
    }

    #[test]
    fn resolves_a_host_field_from_the_newest_run() {
        let mut sub = TestSubstrate::new();
        journal_run(&mut sub, "eval-1", 1_000, serde_json::json!({"category_accuracy": 0.71}));
        journal_run(&mut sub, "eval-2", 2_000, serde_json::json!({"category_accuracy": 0.92}));
        let m = snapshot("category_accuracy", 0.71, true);
        assert_eq!(measure(&sub, &m, 500), Some(0.92), "the newest run is the current value");
    }

    #[test]
    fn a_run_that_predates_the_apply_is_not_evidence() {
        // The failure this prevents is a FABRICATED receipt: scoring the
        // baseline run against itself reports "held" forever, which reads as
        // proof the change worked.
        let mut sub = TestSubstrate::new();
        journal_run(&mut sub, "eval-1", 1_000, serde_json::json!({"category_accuracy": 0.71}));
        let m = snapshot("category_accuracy", 0.71, true);
        assert_eq!(
            measure(&sub, &m, 5_000),
            None,
            "no run since the apply means NOT YET MEASURABLE, not 'held'"
        );
        // Once a fresh run lands, it measures.
        journal_run(&mut sub, "eval-2", 6_000, serde_json::json!({"category_accuracy": 0.92}));
        assert_eq!(measure(&sub, &m, 5_000), Some(0.92));
    }

    #[test]
    fn promoted_fields_work_without_the_host_adding_any() {
        let mut sub = TestSubstrate::new();
        sub.add_fact_at(
            "agent:harness",
            &format!("evalset:{EVALSET}"),
            "mg:eval_run",
            &serde_json::json!({"run_id": "eval-1", "passed": 150, "failed": 34}).to_string(),
            1_000,
        );
        assert_eq!(measure(&sub, &snapshot("failed", 0.0, false), 0), Some(34.0));
        assert_eq!(measure(&sub, &snapshot("passed", 0.0, true), 0), Some(150.0));
        assert_eq!(measure(&sub, &snapshot("total", 0.0, false), 0), Some(184.0));
        let rate = measure(&sub, &snapshot("error_rate", 0.0, false), 0).unwrap();
        assert!((rate - 34.0 / 184.0).abs() < 1e-9, "error_rate = failed/total, got {rate}");
    }

    #[test]
    fn degenerate_and_malformed_inputs_measure_nothing_rather_than_guessing() {
        let mut sub = TestSubstrate::new();
        // An empty evalset must not divide by zero into NaN.
        sub.add_fact_at(
            "agent:harness",
            &format!("evalset:{EVALSET}"),
            "mg:eval_run",
            &serde_json::json!({"run_id": "eval-0", "passed": 0, "failed": 0}).to_string(),
            1_000,
        );
        assert_eq!(measure(&sub, &snapshot("error_rate", 0.0, false), 0), None);
        // A field the summary never recorded is unmeasurable, not zero.
        assert_eq!(measure(&sub, &snapshot("category_accuracy", 0.0, true), 0), None);
        // A malformed metric string resolves to nothing.
        let mut bad = snapshot("x", 0.0, false);
        bad.metric = "evalset:onlyhash".into();
        assert_eq!(measure(&sub, &bad, 0), None);
        // A different evalset's runs never leak in.
        let mut other = snapshot("failed", 0.0, false);
        other.metric = "evalset:deadbeef:failed".into();
        assert_eq!(measure(&sub, &other, 0), None);
    }

    #[test]
    fn a_summary_missing_its_counts_is_dropped_not_defaulted() {
        // Fail-closed: at the apply gate an absent `failed` must never read as
        // "zero failures".
        let mut sub = TestSubstrate::new();
        sub.add_fact_at(
            "agent:harness",
            &format!("evalset:{EVALSET}"),
            "mg:eval_run",
            &serde_json::json!({"run_id": "eval-x", "category_accuracy": 0.99}).to_string(),
            1_000,
        );
        assert_eq!(measure(&sub, &snapshot("category_accuracy", 0.5, true), 0), None);
        assert!(crate::eval::eval_runs(&sub.inner, EVALSET, None).unwrap().is_empty());
    }

    #[test]
    fn the_gating_and_outcome_edges_read_the_same_runs() {
        let mut sub = TestSubstrate::new();
        journal_run(&mut sub, "eval-1", 1_000, serde_json::json!({}));
        journal_run(&mut sub, "eval-2", 2_000, serde_json::json!({}));
        let by_id = crate::eval::eval_run_by_id(&sub.inner, EVALSET, "eval-1")
            .unwrap()
            .expect("gating edge finds the named run");
        assert_eq!(by_id.run_id, "eval-1");
        let newest = crate::eval::newest_eval_run(&sub.inner, EVALSET, None)
            .unwrap()
            .expect("outcome edge finds the newest run");
        assert_eq!(newest.run_id, "eval-2");
        assert!(crate::eval::eval_run_by_id(&sub.inner, EVALSET, "eval-nope").unwrap().is_none());
    }
}


// ---- checkpoints in the deployment's unit ----------------------------------

/// The MockLlm + policy scaffolding every checkpoint test shares: one lesson
/// proposed over one cited fact, an evalset the policy names, and a journal
/// helper. `checkpoints` is the policy's schedule.
fn lesson_under_evalset(checkpoints: &str) -> (TestSubstrate, Engine, String) {
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("capture", "correction", "vendor missing");
    let llm = MockLlm {
        discover: format!(
            r#"{{"recommendations":[{{"summary":"dates are being standardised","target":"entity:test/capture","evidence":["{h1}"],"confidence":0.9,"proposal":{{"kind":"lesson","lesson":"Copy the file date exactly as printed."}}}}]}}"#
        ),
        ground: r#"{"results":[{"id":0,"supported":true,"reason":"ok"}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9,"reason":"ok"}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    };
    let policy = Policy::from_json(&format!(
        r#"{{"outcome_evalset": {{"hash": "heldout1", "field": "exact", "higher_is_better": true, "checkpoints": {checkpoints}}}}}"#
    ))
    .unwrap();
    let e = Engine::with_builtins().with_llm(Box::new(llm)).with_policy(policy);
    (sub, e, h1)
}

fn journal_exact(sub: &mut TestSubstrate, run_id: &str, exact: u64, at: i64) {
    sub.add_fact_at(
        "agent:harness",
        "evalset:heldout1",
        "mg:eval_run",
        &format!(r#"{{"run_id":"{run_id}","passed":{exact},"failed":{},"exact":{exact}}}"#, 100 - exact),
        at,
    );
}

/// A benchmark journals one graded run per episode and finishes a family in
/// minutes; with `after_runs: 1` the verdict lands at the pass after the next
/// run, with the clock barely moved — the schedule that was measured to fire
/// zero verdicts across 78 governed runs under the day-long default.
#[test]
fn a_run_checkpoint_comes_due_at_the_next_graded_run_not_a_day_later() {
    use crate::model::Origin;
    use crate::recommendation::Checkpoint;
    let t = 5_000_000;
    let scopes = ScopeSet::all();
    let (mut sub, e, _) = lesson_under_evalset(r#"[{"after_runs": 1}]"#);
    journal_exact(&mut sub, "eval-0", 40, t - 1_000);
    e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .expect("the lesson is proposed");
    let m = rec.metric.as_ref().expect("the policy attached a metric");
    assert_eq!(m.schedule(), vec![Checkpoint::AfterRuns(1)], "the snapshot carries the host's unit");
    assert!(m.horizons_ms.is_empty(), "no time schedule is invented beside it");
    e.review(&mut sub.inner, &rec.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "ok", t + 1).unwrap();
    e.apply(&mut sub.inner, &rec.hash, "user:a", ObserverType::Human, &scopes, "apply", false, t + 2).unwrap();

    // No run since the apply: not due, no matter how much time passes.
    e.run(&mut sub.inner, &RunOptions::default(), t + 3 * DAY).unwrap();
    assert!(e.outcomes(&sub.inner).unwrap().is_empty(), "a run checkpoint never fires on the clock alone");

    // One graded run after the apply, seconds later: due at the very next pass.
    journal_exact(&mut sub, "eval-1", 20, t + 3 * DAY + 10);
    e.run(&mut sub.inner, &RunOptions::default(), t + 3 * DAY + 20).unwrap();
    let v: Vec<_> = e.outcomes(&sub.inner).unwrap().into_iter().filter(|o| o.rec_hash == rec.hash).collect();
    assert_eq!(v.len(), 1, "measured exactly once");
    assert_eq!((v[0].baseline, v[0].current, v[0].verdict.as_str()), (40.0, 20.0, "regressed"));
    assert_eq!(v[0].checkpoint, Some(Checkpoint::AfterRuns(1)), "the record says which unit fired");
    assert_eq!(v[0].horizon_ms, 0, "and does not pretend to be a time checkpoint");
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("loop.outcome_review")),
        "the regression proposes the revert — the half of governance the default horizon had switched off"
    );
    // Another run does not re-measure a checkpoint already taken.
    journal_exact(&mut sub, "eval-2", 10, t + 3 * DAY + 30);
    e.run(&mut sub.inner, &RunOptions::default(), t + 3 * DAY + 40).unwrap();
    assert_eq!(e.outcomes(&sub.inner).unwrap().iter().filter(|o| o.rec_hash == rec.hash).count(), 1);
}

/// A chat deployment counts activity: `after_grains` fires once enough has
/// been written since the apply, and the measurement still reads only runs
/// journaled after it.
#[test]
fn a_grain_checkpoint_counts_activity_since_the_apply() {
    use crate::model::Origin;
    use crate::recommendation::Checkpoint;
    let t = 5_000_000;
    let scopes = ScopeSet::all();
    let (mut sub, e, _) = lesson_under_evalset(r#"[{"after_grains": 3}]"#);
    journal_exact(&mut sub, "eval-0", 40, t - 1_000);
    e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .unwrap();
    e.review(&mut sub.inner, &rec.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "ok", t + 1).unwrap();
    e.apply(&mut sub.inner, &rec.hash, "user:a", ObserverType::Human, &scopes, "apply", false, t + 2).unwrap();
    // Two grains since the apply (one of them the run): not yet.
    journal_exact(&mut sub, "eval-1", 45, t + 10);
    sub.add_fact_at("test", "x", "y", "z", t + 11);
    e.run(&mut sub.inner, &RunOptions::default(), t + 12).unwrap();
    assert!(e.outcomes(&sub.inner).unwrap().is_empty());
    // A third: due. Current is the newest run after the apply (45 ≥ 40: held).
    sub.add_fact_at("test", "x", "y", "w", t + 13);
    e.run(&mut sub.inner, &RunOptions::default(), t + 14).unwrap();
    let v = e.outcomes(&sub.inner).unwrap();
    assert_eq!(v.len(), 1);
    assert_eq!((v[0].verdict.as_str(), v[0].checkpoint), ("held", Some(Checkpoint::AfterGrains(3))));
}

/// The pre-checkpoint spelling still means what it meant: a state blob with
/// `measured: [86400000]` and a policy with only `horizons_ms` read as time.
#[test]
fn checkpoint_serde_keeps_bare_integers_as_milliseconds() {
    use crate::recommendation::Checkpoint;
    let v: Vec<Checkpoint> = serde_json::from_str(r#"[86400000, {"after_runs": 2}, {"after_grains": 7}, {"after_ms": 5}]"#).unwrap();
    assert_eq!(
        v,
        vec![Checkpoint::AfterMs(86_400_000), Checkpoint::AfterRuns(2), Checkpoint::AfterGrains(7), Checkpoint::AfterMs(5)]
    );
    // An all-time schedule serializes exactly as the old Vec<i64> did.
    assert_eq!(serde_json::to_string(&vec![Checkpoint::AfterMs(1), Checkpoint::AfterMs(2)]).unwrap(), "[1,2]");
    assert_eq!(serde_json::to_string(&Checkpoint::AfterRuns(3)).unwrap(), r#"{"after_runs":3}"#);
    assert_eq!(Checkpoint::AfterMs(86_400_000).label(), "1d");
    assert_eq!(Checkpoint::AfterMs(7_200_000).label(), "2h");
    assert_eq!(Checkpoint::AfterRuns(1).label(), "1 run");
    assert_eq!(Checkpoint::AfterGrains(50).label(), "50 grains");
    for bad in [r#"-1"#, r#""1d""#, r#"{"after_turns":1}"#, r#"{"after_runs":1,"after_ms":1}"#, r#"{"after_runs":-1}"#] {
        assert!(serde_json::from_str::<Checkpoint>(bad).is_err(), "{bad}");
    }
    // A legacy state blob decodes into the typed map.
    let p = crate::config::LoopPersisted::from_value(serde_json::json!({"measured": {"h": [86400000]}})).unwrap();
    assert_eq!(p.measured["h"], vec![Checkpoint::AfterMs(86_400_000)]);
}

// ---- cadence ----------------------------------------------------------------

fn add_event(sub: &mut TestSubstrate, session: &str, at: i64) {
    let mut fields = serde_json::Map::new();
    fields.insert("content".into(), serde_json::json!("hello"));
    fields.insert("role".into(), serde_json::json!("user"));
    fields.insert("session_id".into(), serde_json::json!(session));
    fields.insert("namespace".into(), serde_json::json!("test"));
    sub.inner.insert(crate::model::GrainRecord {
        hash: String::new(),
        grain_type: "event".into(),
        namespace: "test".into(),
        created_at_ms: at,
        valid_to_ms: None,
        superseded_by: None,
        fields,
    });
}

#[test]
fn cadence_counts_turns_and_sessions_and_yields_to_flags_and_sweeps() {
    let t = 10_000;
    // every_events: 3 — two turns is not a tick, three is.
    let p = Policy::from_json(r#"{"cadence": {"every_events": 3}}"#).unwrap();
    let e = Engine::with_builtins().with_policy(p);
    let mut sub = TestSubstrate::new();
    add_event(&mut sub, "s1", t + 1);
    add_event(&mut sub, "s1", t + 2);
    let r = e.run(&mut sub.inner, &RunOptions::default(), t + 10).unwrap();
    assert_eq!((r.outcome, r.skip_reason), (RunOutcome::Skipped, Some(SkipReason::CadenceNotDue)));
    add_event(&mut sub, "s1", t + 3);
    let r = e.run(&mut sub.inner, &RunOptions::default(), t + 11).unwrap();
    assert_eq!(r.outcome, RunOutcome::Ran, "the third turn makes the pass due");
    // The watermark advanced: the same three turns do not fire it again.
    let r = e.run(&mut sub.inner, &RunOptions::default(), t + 12).unwrap();
    assert_eq!(r.skip_reason, Some(SkipReason::CadenceNotDue));

    // every_sessions: 1 — reflect once per conversation. Ten turns in one
    // session is one session.
    let p = Policy::from_json(r#"{"cadence": {"every_sessions": 2}}"#).unwrap();
    let e = Engine::with_builtins().with_policy(p);
    let mut sub = TestSubstrate::new();
    for i in 0..10 {
        add_event(&mut sub, "only", t + i);
    }
    assert_eq!(e.run(&mut sub.inner, &RunOptions::default(), t + 20).unwrap().skip_reason, Some(SkipReason::CadenceNotDue));
    add_event(&mut sub, "another", t + 15);
    assert_eq!(e.run(&mut sub.inner, &RunOptions::default(), t + 21).unwrap().outcome, RunOutcome::Ran);

    // every_ms: time since the last run; OR with every_grains — whichever first.
    let p = Policy::from_json(r#"{"cadence": {"every_ms": 3600000, "every_grains": 2}}"#).unwrap();
    let e = Engine::with_builtins().with_policy(p);
    let mut sub = TestSubstrate::new();
    sub.add_fact_at("test", "a", "b", "c", t);
    assert_eq!(e.run(&mut sub.inner, &RunOptions::default(), t + 1).unwrap().outcome, RunOutcome::Ran, "never ran → due");
    sub.add_fact_at("test", "a", "b", "d", t + 2);
    assert_eq!(e.run(&mut sub.inner, &RunOptions::default(), t + 3).unwrap().skip_reason, Some(SkipReason::CadenceNotDue));
    assert_eq!(e.run(&mut sub.inner, &RunOptions::default(), t + 3_600_005).unwrap().outcome, RunOutcome::Ran, "an hour later: time fires");

    // Flags override the block (host CLI flags > policy file): --min-new 1 on a
    // file the cadence would skip runs; and a sweep is a command, not a tick.
    let p = Policy::from_json(r#"{"cadence": {"every_grains": 100}}"#).unwrap();
    let e = Engine::with_builtins().with_policy(p);
    let mut sub = TestSubstrate::new();
    sub.add_fact("a", "b", "c");
    assert_eq!(e.run(&mut sub.inner, &RunOptions::default(), t).unwrap().skip_reason, Some(SkipReason::CadenceNotDue));
    let flags = RunOptions { min_new: Some(1), ..Default::default() };
    assert_eq!(e.run(&mut sub.inner, &flags, t + 1).unwrap().outcome, RunOutcome::Ran);
    let sweep = RunOptions { full_sweep: true, ..Default::default() };
    assert_eq!(e.run(&mut sub.inner, &sweep, t + 2).unwrap().outcome, RunOutcome::Ran);
    // And an unset cadence changes nothing: always due.
    let e = Engine::with_builtins();
    let mut sub = TestSubstrate::new();
    assert_eq!(e.run(&mut sub.inner, &RunOptions::default(), t).unwrap().outcome, RunOutcome::Ran);
}

// ---- the evidence floor -------------------------------------------------------

/// Under `min_evidence: 2` a lesson citing one grain is stored as a finding a
/// reviewer can read, but carries nothing they could apply; the funnel says
/// why. Under the default it is applicable, as before.
#[test]
fn a_thin_draft_stays_advisory_under_the_evidence_floor() {
    use crate::model::Origin;
    let t = 5_000_000;
    let mk = |min: u32| {
        let mut sub = TestSubstrate::new();
        let h1 = sub.add_fact("capture", "correction", "vendor missing");
        let llm = MockLlm {
            discover: format!(
                r#"{{"recommendations":[{{"summary":"a pattern","target":"entity:test/capture","evidence":["{h1}"],"confidence":0.9,"proposal":{{"kind":"lesson","lesson":"Record the vendor on every invoice."}}}}]}}"#
            ),
            ground: r#"{"results":[{"id":0,"supported":true,"reason":"ok"}]}"#.into(),
            verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9,"reason":"ok"}]}"#.into(),
            enrich: r#"{"notes":[]}"#.into(),
        };
        let policy = Policy::from_json(&format!(r#"{{"min_evidence": {min}}}"#)).unwrap();
        (sub, Engine::with_builtins().with_llm(Box::new(llm)).with_policy(policy))
    };
    let (mut sub, e) = mk(2);
    let r = e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    assert_eq!(r.llm_funnel.as_ref().map(|f| f.advisory_thin_evidence), Some(1));
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .expect("stored — a person may still want to see it");
    assert_eq!(rec.action_kind, crate::model::ActionKind::Flag, "but nothing to apply");
    let (mut sub, e) = mk(1);
    let r = e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    assert_eq!(r.llm_funnel.as_ref().map(|f| f.advisory_thin_evidence), Some(0));
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .unwrap();
    assert_eq!(rec.action_kind, crate::model::ActionKind::ClusterFailure, "the default: one instance may become a rule");
}


// ---- skill authoring ------------------------------------------------------------

/// The proposer sees a successful trajectory — tool calls with their inputs —
/// and authors a Skill: name from the target, ordered steps as instructions,
/// `when_to_use` as the routing cue. Applied, it is a live Skill grain in the
/// evidence's namespace; proposed again under the same name, it supersedes
/// rather than duplicating.
#[test]
fn the_proposer_authors_a_skill_from_a_successful_trajectory_and_patches_it_by_name() {
    use crate::model::Origin;
    use crate::substrate::{ReadOpts, SubstrateRead};
    let t = 5_000_000;
    let scopes = ScopeSet::all();
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_tool_call("helpdesk_list_tickets", false, "TK-1 open, TK-2 open");
    let h2 = sub.add_tool_call("helpdesk_update_ticket", false, "TK-2 updated: priority=high tags=[deploy-child]");
    let draft_citing = |cites: &str, steps: &str| {
        format!(
            r#"{{"recommendations":[{{"summary":"a repeatable triage","target":"entity:test/group-deploy-children","evidence":[{cites}],"confidence":0.9,"proposal":{{"kind":"skill","description":"Group child failures under their deploy event","when_to_use":"open tickets share a release_hash with a deploy-event ticket","steps":{steps}}}}}]}}"#
        )
    };
    let draft = |steps: &str| draft_citing(&format!(r#""{h1}","{h2}""#), steps);
    let mk = |discover: String| MockLlm {
        discover,
        ground: r#"{"results":[{"id":0,"supported":true,"reason":"ok"}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9,"reason":"ok"}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    };
    let e = Engine::with_builtins().with_llm(Box::new(mk(draft(
        r#"["List open tickets with helpdesk_list_tickets","For each child-failure sharing the deploy event's release_hash, set priority=high and tags=[deploy-child, <release_hash>]","Do not update the root deploy-event ticket"]"#,
    ))));
    let r = e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    assert_eq!(r.llm_funnel.as_ref().map(|f| f.stored), Some(1), "the skill reached the queue");
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .unwrap();
    let text = rec.summary.render();
    assert!(text.contains("record skill: \"group-deploy-children\" (3 steps)"), "{text}");
    assert!(rec.rollbackable);
    e.review(&mut sub.inner, &rec.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "reusable", t + 1).unwrap();
    e.apply(&mut sub.inner, &rec.hash, "user:a", ObserverType::Human, &scopes, "apply", false, t + 2).unwrap();
    let skills = sub
        .inner
        .grains_of_type(crate::model::grain_type::SKILL, None, ReadOpts { live_only: true, since_ms: None })
        .unwrap();
    assert_eq!(skills.len(), 1);
    let sk = &skills[0];
    assert_eq!(sk.skill_name(), Some("group-deploy-children"));
    assert_eq!(sk.namespace, "test", "the evidence's namespace, never the model's");
    let instr = sk.str_field("instructions").unwrap();
    assert!(instr.starts_with("1. List open tickets"), "{instr}");
    assert!(instr.contains("\n3. Do not update"), "ordered, numbered: {instr}");
    assert!(sk.str_field("when_to_use").unwrap().contains("release_hash"));
    let first_hash = sk.hash.clone();

    // The same name again, with a changed procedure: a SUPERSEDE of the live
    // skill, so the memory holds one skill of that name, not two.
    let e2 = Engine::with_builtins().with_llm(Box::new(mk(draft(
        r#"["List open tickets with helpdesk_list_tickets","Set priority=critical on a 503 child failure, else high"]"#,
    ))));
    e2.run(&mut sub.inner, &RunOptions { full_sweep: true, ..Default::default() }, t + DAY).unwrap();
    let rec2 = e2
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }) && r.hash != rec.hash)
        .expect("a second, different skill proposal");
    match &rec2.proposal {
        crate::recommendation::Proposal::Cal { cal } => {
            assert!(cal.starts_with(&format!("SUPERSEDE {first_hash} WITH skill ")), "patched by name: {cal}")
        }
        other => panic!("{other:?}"),
    }
    e2.review(&mut sub.inner, &rec2.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "changed", t + DAY + 1).unwrap();
    e2.apply(&mut sub.inner, &rec2.hash, "user:a", ObserverType::Human, &scopes, "apply", false, t + DAY + 2).unwrap();
    let live = sub
        .inner
        .grains_of_type(crate::model::grain_type::SKILL, None, ReadOpts { live_only: true, since_ms: None })
        .unwrap();
    assert_eq!(live.len(), 1, "one live skill of that name");
    assert!(live[0].str_field("instructions").unwrap().contains("critical"));

    // Too thin to be a procedure, or disabled by policy: advisory, not a change.
    let mut sub3 = TestSubstrate::new();
    let a = sub3.add_tool_call("x", false, "ok");
    let thin = Engine::with_builtins().with_llm(Box::new(mk(draft_citing(&format!(r#""{a}""#), r#"["Just one step"]"#))));
    let r = thin.run(&mut sub3.inner, &RunOptions::default(), t).unwrap();
    assert_eq!(r.llm_funnel.as_ref().map(|f| f.evidence), Some(1), "a lone successful call is evidence under skill authoring: {:?}", r.llm_funnel);
    let only = thin.recommendations(&sub3.inner, Some(RecStatus::Pending)).unwrap().into_iter().find(|r| matches!(r.origin, Origin::Llm { .. })).unwrap();
    assert_eq!(only.action_kind, crate::model::ActionKind::Flag, "one step is not a procedure: stored as a finding, applies as nothing");
    // Skills off: a lone success is not evidence (the pre-skill bundle), and
    // even a skill the model returns anyway, over a cited failure, applies
    // as nothing.
    let mut sub4 = TestSubstrate::new();
    let b = sub4.add_tool_call("x", true, "boom");
    let off = Engine::with_builtins()
        .with_llm(Box::new(mk(draft_citing(&format!(r#""{b}""#), r#"["a","b","c"]"#))))
        .with_policy(Policy::from_json(r#"{"skills": {"enabled": false}}"#).unwrap());
    off.run(&mut sub4.inner, &RunOptions::default(), t).unwrap();
    let only = off.recommendations(&sub4.inner, Some(RecStatus::Pending)).unwrap().into_iter().find(|r| matches!(r.origin, Origin::Llm { .. })).unwrap();
    assert_eq!(only.action_kind, crate::model::ActionKind::Flag, "not offered by policy: even a returned skill applies as nothing");
}

/// The skill paragraph is in the instructions only when the host allows it,
/// so the vocabulary the model sees is the vocabulary that can apply.
#[test]
fn the_skill_kind_is_offered_only_under_policy() {
    use std::sync::{Arc, Mutex};
    struct Capture(Arc<Mutex<Vec<String>>>);
    impl crate::llm::LlmBackend for Capture {
        fn model(&self) -> &str {
            "capture"
        }
        fn complete(&self, request: &str) -> crate::error::Result<String> {
            self.0.lock().unwrap().push(request.to_string());
            Ok(r#"{"recommendations":[]}"#.into())
        }
    }
    let run = |policy: &str| -> bool {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut sub = TestSubstrate::new();
        // A FAILURE, so the model is reached under either policy — with
        // skills off a lone success is not evidence at all, which the
        // tool-failure evidence test pins separately.
        sub.add_tool_call("x", true, "boom");
        let e = Engine::with_builtins()
            .with_llm(Box::new(Capture(seen.clone())))
            .with_policy(Policy::from_json(policy).unwrap());
        let r = e.run(&mut sub.inner, &RunOptions::default(), 5_000_000).unwrap();
        let reqs = seen.lock().unwrap();
        let discover = reqs
            .iter()
            .find(|r| r.contains("\"op\":\"discover\""))
            .unwrap_or_else(|| panic!("discover was called; outcome {:?} funnel {:?}", r.outcome, r.llm_funnel));
        // `<skill-name>` occurs in the skill paragraph and nowhere else.
        discover.contains("<skill-name>")
    };
    assert!(run("{}"), "default: offered");
    assert!(!run(r#"{"skills": {"enabled": false}}"#), "disabled: not offered");
}


// ---- plan authoring -----------------------------------------------------------

/// A procedure with a branch is authored as a PLAN: a Workflow the runtime
/// validated (steps bound to tools the evidence shows were called, an edge
/// with a condition in the frozen grammar) and a Skill of the same name that
/// carries the prose. One batch, one review. The same name again supersedes
/// both — the patch `PC02_sop_patch` is about.
#[test]
fn the_proposer_authors_a_plan_as_a_validated_workflow_beside_its_skill() {
    use crate::model::Origin;
    use crate::substrate::{ReadOpts, SubstrateRead};
    let t = 5_000_000;
    let scopes = ScopeSet::all();
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_tool_call("helpdesk_list_tickets", false, "TK-1 open, TK-2 open, TK-3 open");
    let h2 = sub.add_tool_call("helpdesk_update_ticket", false, "TK-2 tagged shared-incident");
    let draft = |nodes: &str, edges: &str| {
        format!(
            r#"{{"recommendations":[{{"summary":"a repeatable triage with a branch","target":"entity:test/shared-incident-triage","evidence":["{h1}","{h2}"],"confidence":0.9,"proposal":{{"kind":"plan","description":"Triage a batch for a shared incident","when_to_use":"several open tickets mention one component","nodes":{nodes},"edges":{edges}}}}}]}}"#
        )
    };
    let mk = |discover: String| MockLlm {
        discover,
        ground: r#"{"results":[{"id":0,"supported":true,"reason":"ok"}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9,"reason":"ok"}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    };
    let nodes = r#"[{"id":"list_open","tool":"helpdesk_list_tickets","step":"List every open ticket"},{"id":"tag_shared","tool":"helpdesk_update_ticket","step":"Tag each ticket sharing the component with shared-incident, priority high"},{"id":"leave","tool":"helpdesk_update_ticket","step":"Leave resolution to on-call: do not close"}]"#;
    let edges = r#"[{"src":"list_open","dst":"tag_shared","cond":"shared_component == true"},{"src":"tag_shared","dst":"leave"}]"#;
    let e = Engine::with_builtins().with_llm(Box::new(mk(draft(nodes, edges))));
    let r = e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    assert_eq!(r.llm_funnel.as_ref().map(|f| f.stored), Some(1));
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .unwrap();
    let text = rec.summary.render();
    assert!(text.contains("record plan: \"shared-incident-triage\" (3 steps, 2 edges)"), "{text}");
    match &rec.proposal {
        crate::recommendation::Proposal::Cal { cal } => {
            assert!(cal.starts_with("ADD skill "), "{cal}");
            assert!(cal.contains("\nADD workflow "), "one batch, two grains: {cal}");
        }
        other => panic!("{other:?}"),
    }
    e.review(&mut sub.inner, &rec.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "reusable", t + 1).unwrap();
    e.apply(&mut sub.inner, &rec.hash, "user:a", ObserverType::Human, &scopes, "apply", false, t + 2).unwrap();
    let live = |s: &TestSubstrate, gt: &str| s.inner.grains_of_type(gt, None, ReadOpts { live_only: true, since_ms: None }).unwrap();
    let skills = live(&sub, crate::model::grain_type::SKILL);
    let plans = live(&sub, crate::model::grain_type::WORKFLOW);
    assert_eq!((skills.len(), plans.len()), (1, 1));
    let sk = &skills[0];
    assert_eq!(sk.skill_name(), Some("shared-incident-triage"));
    let instr = sk.str_field("instructions").unwrap();
    assert!(instr.starts_with("1. list_open [helpdesk_list_tickets]: List every open ticket"), "{instr}");
    assert!(instr.contains("Flow:\n- list_open → tag_shared if shared_component == true\n- tag_shared → leave"), "{instr}");
    let wf = &plans[0];
    assert_eq!(wf.str_field("name"), Some("shared-incident-triage"));
    assert_eq!(wf.namespace, "test");
    assert_eq!(wf.fields["nodes"], serde_json::json!(["list_open", "tag_shared", "leave"]));
    assert_eq!(wf.fields["edges"][0]["cond"], serde_json::json!("shared_component == true"));
    let (skill_hash, plan_hash) = (sk.hash.clone(), wf.hash.clone());

    // Patched by name: a second plan of the same name supersedes both grains.
    let e2 = Engine::with_builtins().with_llm(Box::new(mk(draft(
        r#"[{"id":"list_open","tool":"helpdesk_list_tickets","step":"List open tickets"},{"id":"tag_shared","tool":"helpdesk_update_ticket","step":"Tag with shared-incident AND zone-correlated"}]"#,
        r#"[{"src":"list_open","dst":"tag_shared"}]"#,
    ))));
    e2.run(&mut sub.inner, &RunOptions { full_sweep: true, ..Default::default() }, t + DAY).unwrap();
    let rec2 = e2
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }) && r.hash != rec.hash)
        .unwrap();
    assert_eq!(rec2.action_kind, crate::model::ActionKind::Revise);
    match &rec2.proposal {
        crate::recommendation::Proposal::Cal { cal } => {
            assert!(cal.contains(&format!("SUPERSEDE {skill_hash} WITH skill ")), "{cal}");
            assert!(cal.contains(&format!("SUPERSEDE {plan_hash} WITH workflow ")), "{cal}");
        }
        other => panic!("{other:?}"),
    }
    e2.review(&mut sub.inner, &rec2.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "v2", t + DAY + 1).unwrap();
    e2.apply(&mut sub.inner, &rec2.hash, "user:a", ObserverType::Human, &scopes, "apply", false, t + DAY + 2).unwrap();
    assert_eq!((live(&sub, crate::model::grain_type::SKILL).len(), live(&sub, crate::model::grain_type::WORKFLOW).len()), (1, 1), "one live pair");
    assert!(live(&sub, crate::model::grain_type::SKILL)[0].str_field("instructions").unwrap().contains("zone-correlated"));

    // Not grounded, not runnable, or not allowed: advisory, never a change.
    let advisory = |sub: &mut TestSubstrate, e: &Engine| {
        e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .into_iter()
            .find(|r| matches!(r.origin, Origin::Llm { .. }))
            .map(|r| r.action_kind)
    };
    // a tool the evidence never shows
    let mut s3 = TestSubstrate::new();
    let a = s3.add_tool_call("helpdesk_list_tickets", false, "ok");
    let bad_tool = format!(
        r#"{{"recommendations":[{{"summary":"s","target":"entity:test/p","evidence":["{a}"],"confidence":0.9,"proposal":{{"kind":"plan","description":"d","when_to_use":"w","nodes":[{{"id":"x","tool":"helpdesk_list_tickets","step":"s"}},{{"id":"y","tool":"delete_everything","step":"s"}}],"edges":[]}}}}]}}"#
    );
    assert_eq!(advisory(&mut s3, &Engine::with_builtins().with_llm(Box::new(mk(bad_tool)))), Some(crate::model::ActionKind::Flag));
    // an edge to a step that does not exist
    let mut s4 = TestSubstrate::new();
    let a = s4.add_tool_call("helpdesk_list_tickets", false, "ok");
    let bad_edge = format!(
        r#"{{"recommendations":[{{"summary":"s","target":"entity:test/p","evidence":["{a}"],"confidence":0.9,"proposal":{{"kind":"plan","description":"d","when_to_use":"w","nodes":[{{"id":"x","tool":"helpdesk_list_tickets","step":"s"}},{{"id":"y","tool":"helpdesk_list_tickets","step":"s"}}],"edges":[{{"src":"x","dst":"nowhere"}}]}}}}]}}"#
    );
    assert_eq!(advisory(&mut s4, &Engine::with_builtins().with_llm(Box::new(mk(bad_edge)))), Some(crate::model::ActionKind::Flag));
    // plans off by policy
    let mut s5 = TestSubstrate::new();
    let a = s5.add_tool_call("helpdesk_list_tickets", true, "boom");
    let fine = format!(
        r#"{{"recommendations":[{{"summary":"s","target":"entity:test/p","evidence":["{a}"],"confidence":0.9,"proposal":{{"kind":"plan","description":"d","when_to_use":"w","nodes":[{{"id":"x","tool":"helpdesk_list_tickets","step":"s"}},{{"id":"y","tool":"helpdesk_list_tickets","step":"s"}}],"edges":[{{"src":"x","dst":"y"}}]}}}}]}}"#
    );
    let off = Engine::with_builtins()
        .with_llm(Box::new(mk(fine)))
        .with_policy(Policy::from_json(r#"{"plans": {"enabled": false}}"#).unwrap());
    assert_eq!(advisory(&mut s5, &off), Some(crate::model::ActionKind::Flag));
}

// ---- the Verify gate's second question: premise drift ---------------------------

fn applied_lesson_over(sub: &mut TestSubstrate, e: &Engine, t: i64) -> Recommendation {
    use crate::model::Origin;
    let scopes = ScopeSet::all();
    e.run(&mut sub.inner, &RunOptions::default(), t).unwrap();
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| matches!(r.origin, Origin::Llm { .. }))
        .expect("the lesson is proposed");
    e.review(&mut sub.inner, &rec.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "ok", t + 1).unwrap();
    e.apply(&mut sub.inner, &rec.hash, "user:a", ObserverType::Human, &scopes, "apply", false, t + 2).unwrap();
    rec
}

fn lesson_llm(h1: &str) -> MockLlm {
    MockLlm {
        discover: format!(
            r#"{{"recommendations":[{{"summary":"the readiness rule is flagged stale","target":"entity:test/release","evidence":["{h1}"],"confidence":0.9,"proposal":{{"kind":"lesson","lesson":"Skip the Release readiness review when it is flagged as a stale target."}}}}]}}"#
        ),
        ground: r#"{"results":[{"id":0,"supported":true,"reason":"ok"}]}"#.into(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9,"reason":"ok"}]}"#.into(),
        enrich: r#"{"notes":[]}"#.into(),
    }
}

/// A lesson learned from a rule that is later REPLACED has lost its premise:
/// the gate records `drifted` and proposes the revert; applying it rolls the
/// lesson back. A value-identical supersession (consolidation) is not drift,
/// and the check is a policy switch.
#[test]
fn a_lesson_whose_cited_evidence_was_superseded_by_a_different_value_is_reverted() {
    use crate::substrate::OmsSubstrate;
    let t = 5_000_000;
    let scopes = ScopeSet::all();
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("release", "readiness_rule", "flagged: stale target for migration");
    let e = Engine::with_builtins().with_llm(Box::new(lesson_llm(&h1)));
    let rec = applied_lesson_over(&mut sub, &e, t);
    // Nothing moved yet: no drift, no revert.
    e.run(&mut sub.inner, &RunOptions::default(), t + 10).unwrap();
    assert!(e.outcomes(&sub.inner).unwrap().is_empty());

    // The rule the lesson was learned from is replaced by a DIFFERENT one.
    sub.inner
        .execute_cal(&format!(
            r#"SUPERSEDE {h1} WITH fact {{"subject":"release","relation":"readiness_rule","object":"REL-GAMMA: review before every deploy","namespace":"test"}}"#
        ))
        .unwrap();
    e.run(&mut sub.inner, &RunOptions::default(), t + 20).unwrap();
    let v: Vec<_> = e.outcomes(&sub.inner).unwrap().into_iter().filter(|o| o.rec_hash == rec.hash).collect();
    assert_eq!(v.len(), 1);
    assert_eq!((v[0].metric.as_str(), v[0].verdict.as_str(), v[0].current), ("premise_drift", "drifted", 1.0));
    let revert = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.analyzer.starts_with("loop.outcome_review"))
        .expect("a revert is proposed");
    assert!(revert.summary.render().contains("premise moved"), "{}", revert.summary.render());
    // A further pass does not re-record the same drift.
    e.run(&mut sub.inner, &RunOptions::default(), t + 30).unwrap();
    assert_eq!(e.outcomes(&sub.inner).unwrap().iter().filter(|o| o.rec_hash == rec.hash).count(), 1);
    // Applying the revert rolls the lesson back.
    e.review(&mut sub.inner, &revert.hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "its premise changed", t + 31).unwrap();
    e.apply(&mut sub.inner, &revert.hash, "user:a", ObserverType::Human, &scopes, "revert", false, t + 32).unwrap();
    let statuses: std::collections::BTreeMap<_, _> = e
        .recommendations(&sub.inner, None)
        .unwrap()
        .into_iter()
        .map(|r| (r.hash, r.status))
        .collect();
    assert_eq!(statuses[&rec.hash], RecStatus::RolledBack);

    // Value-identical supersession is not drift.
    let mut sub2 = TestSubstrate::new();
    let h = sub2.add_fact("release", "readiness_rule", "flagged: stale target for migration");
    let e2 = Engine::with_builtins().with_llm(Box::new(lesson_llm(&h)));
    let rec2 = applied_lesson_over(&mut sub2, &e2, t);
    sub2.inner
        .execute_cal(&format!(
            r#"SUPERSEDE {h} WITH fact {{"subject":"release","relation":"readiness_rule","object":"Flagged: stale target for migration ","namespace":"test","confidence":0.99}}"#
        ))
        .unwrap();
    e2.run(&mut sub2.inner, &RunOptions::default(), t + 20).unwrap();
    assert!(e2.outcomes(&sub2.inner).unwrap().iter().all(|o| o.rec_hash != rec2.hash), "same value, different confidence: the premise stands");

    // A retracted premise IS drift.
    let mut sub3 = TestSubstrate::new();
    let h = sub3.add_fact("release", "readiness_rule", "flagged: stale target for migration");
    let e3 = Engine::with_builtins().with_llm(Box::new(lesson_llm(&h)));
    let rec3 = applied_lesson_over(&mut sub3, &e3, t);
    sub3.inner.execute_cal(&format!("FORGET {h}")).unwrap();
    e3.run(&mut sub3.inner, &RunOptions::default(), t + 20).unwrap();
    assert!(e3.outcomes(&sub3.inner).unwrap().iter().any(|o| o.rec_hash == rec3.hash && o.verdict == "drifted"));

    // Switched off by policy: nothing.
    let mut sub4 = TestSubstrate::new();
    let h = sub4.add_fact("release", "readiness_rule", "flagged: stale target for migration");
    let e4 = Engine::with_builtins()
        .with_llm(Box::new(lesson_llm(&h)))
        .with_policy(Policy::from_json(r#"{"premise_drift": false}"#).unwrap());
    let rec4 = applied_lesson_over(&mut sub4, &e4, t);
    sub4.inner
        .execute_cal(&format!(r#"SUPERSEDE {h} WITH fact {{"subject":"release","relation":"readiness_rule","object":"something else","namespace":"test"}}"#))
        .unwrap();
    e4.run(&mut sub4.inner, &RunOptions::default(), t + 20).unwrap();
    assert!(e4.outcomes(&sub4.inner).unwrap().iter().all(|o| o.rec_hash != rec4.hash));
}
