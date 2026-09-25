//! The optional decision backend (`docs/decision-model-proposal.md` rows
//! E1–E3) end to end over the reference substrate, with a scripted,
//! deterministic [`FakeDecider`]. Compiled only under `cfg(test)`.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::analyzers::{
    contradiction_sweep::ContradictionSweep, duplicate_sweep::DuplicateSweep,
    tool_failure::ToolFailureClustering,
};
use crate::decide::{Decider, DECIDE_MIN_P};
use crate::engine::{Engine, RunOptions};
use crate::error::Result;
use crate::manifest::AnalyzerManifest;
use crate::model::Origin;
use crate::policy::Policy;
use crate::recommendation::{RecDraft, RecStatus, Recommendation};
use crate::testkit::{FakeDecider, TestSubstrate};
use serde_json::{json, Value};

fn decider(f: FakeDecider) -> Decider {
    Decider::new(Box::new(f))
}

/// Every question id across every logged request.
fn asked(log: &std::sync::Mutex<Vec<Value>>) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .flat_map(|r| r["questions"].as_object().unwrap().keys().cloned().collect::<Vec<_>>())
        .collect()
}

// ---- the engine slot --------------------------------------------------------

#[test]
fn the_engine_slot_records_the_backend_on_the_run() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "gold");
    let without = Engine::with_builtins()
        .run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    assert!(without.decider.is_none());
    assert!(
        !serde_json::to_string(&without).unwrap().contains("decider"),
        "a run without a backend serializes exactly as before"
    );

    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "gold");
    let e = Engine::with_builtins().with_decider(Box::new(FakeDecider::constant(0.1)));
    assert_eq!(e.decider().unwrap().describe(), "fake:fake-1");
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let rep = r.decider.expect("the run reports its backend");
    assert_eq!(rep.backend, "fake:fake-1");
    assert!(rep.calibrated);
    assert_eq!(rep.failed_calls, 0);
    assert!(rep.last_error.is_none());
}

#[test]
fn the_pair_cap_is_carried_across_a_later_with_decider() {
    let e = Engine::with_builtins()
        .with_decider(Box::new(FakeDecider::constant(0.1)))
        .with_decider_pair_cap(7);
    assert_eq!(e.decider().unwrap().pair_cap(), 7);
    let e = e.with_decider(Box::new(FakeDecider::constant(0.2)));
    assert_eq!(e.decider().unwrap().pair_cap(), 7);
    assert_eq!(
        Engine::with_builtins()
            .with_decider(Box::new(FakeDecider::constant(0.1)))
            .decider()
            .unwrap()
            .pair_cap(),
        crate::decide::DEFAULT_PAIR_CAP
    );
}

// ---- E1: GROUND / VERIFY ------------------------------------------------------

/// An LLM keyed by op that logs every request.
struct OpLlm {
    ground: String,
    verify: String,
    log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl crate::llm::LlmBackend for OpLlm {
    fn model(&self) -> &str {
        "op-llm"
    }
    fn complete(&self, request: &str) -> Result<String> {
        self.log.lock().unwrap().push(request.to_string());
        Ok(if request.contains(r#""op":"discover""#) {
            r#"{"recommendations":[{"summary":"acme is deployed to two regions at once",
                "target":"entity:test/acme","evidence":["e1","e2"],"confidence":0.9}]}"#
                .into()
        } else if request.contains(r#""op":"ground""#) {
            self.ground.clone()
        } else if request.contains(r#""op":"verify""#) {
            self.verify.clone()
        } else {
            r#"{"notes":[]}"#.into()
        })
    }
}

const GROUND_NO: &str = r#"{"results":[{"id":0,"supported":false,"reason":"no"}]}"#;
const GROUND_YES: &str = r#"{"results":[{"id":0,"supported":true,"reason":"ok"}]}"#;

fn verify_keep(conf: f64) -> String {
    format!(r#"{{"results":[{{"id":0,"keep":true,"confidence":{conf},"reason":"sound"}}]}}"#)
}

struct E1 {
    stored: Vec<Recommendation>,
    llm_log: Vec<String>,
    decide_log: Vec<Value>,
    run: crate::engine::RunResult,
}

/// One run of the DISCOVER → GROUND → VERIFY pipeline over two cited facts.
fn e1(ground: &str, verify: String, fake: Option<FakeDecider>) -> E1 {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    let llm_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut e = Engine::empty().with_llm(Box::new(OpLlm {
        ground: ground.into(),
        verify,
        log: llm_log.clone(),
    }));
    let mut decide_log = None;
    if let Some(f) = fake {
        decide_log = Some(f.calls());
        e = e.with_decider(Box::new(f));
    }
    let run = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let stored = e
        .recommendations(&sub.inner, None)
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r.origin, Origin::Llm { .. }))
        .collect();
    let llm_log = llm_log.lock().unwrap().clone();
    E1 {
        stored,
        llm_log,
        decide_log: decide_log.map(|l| l.lock().unwrap().clone()).unwrap_or_default(),
        run,
    }
}

/// A fake answering the E1 questions: `ev_*` with `ground`, `sound` with `sound`.
fn e1_fake(ground: f64, sound: f64) -> FakeDecider {
    FakeDecider::by(move |id, _| {
        let p = if id == "sound" { sound } else { ground };
        json!({"type": "noul", "noul": p})
    })
}

#[test]
fn a_calibrated_decider_replaces_the_llm_ground_call_batched_per_draft() {
    // The LLM GROUND would refuse the draft; the decision grounds it.
    let out = e1(GROUND_NO, verify_keep(0.8), Some(e1_fake(0.9, 0.9)));
    assert!(
        !out.llm_log.iter().any(|r| r.contains(r#""op":"ground""#)),
        "the LLM GROUND call is skipped when a calibrated decision answered"
    );
    assert!(out.llm_log.iter().any(|r| r.contains(r#""op":"verify""#)), "the LLM keep/kill still runs");
    assert_eq!(out.decide_log.len(), 1, "one request for the one draft");
    let req = &out.decide_log[0];
    let mut ids: Vec<&String> = req["questions"].as_object().unwrap().keys().collect();
    ids.sort();
    assert_eq!(ids, ["ev_e1", "ev_e2", "sound"], "every cited grain plus soundness, in ONE request");
    assert_eq!(
        req["questions"]["sound"]["instructions"],
        "Given only this evidence, is the recommendation sound?"
    );
    assert!(req["state"]["recommendation"]["summary"]
        .as_str()
        .unwrap()
        .contains("two regions"));
    assert_eq!(req["state"]["evidence"].as_array().unwrap().len(), 2);

    assert_eq!(out.stored.len(), 1);
    let r = &out.stored[0];
    assert_eq!(r.confidence, 0.9, "the routing number is the decision's p(sound)");
    assert_eq!(r.llm_confidence, Some(0.8), "the verifier's self-report is kept beside it");
    let j = r.judged_by.as_ref().expect("provenance recorded");
    assert_eq!(j.stage, "ground_verify");
    assert_eq!((j.provider.as_str(), j.model.as_str(), j.backend.as_str()), ("fake", "fake-1", "fake:fake-1"));
    assert!(j.calibrated);
    assert_eq!(j.latency_ms, 7);
    assert_eq!(j.answers.get("sound"), Some(&0.9));
    let funnel = out.run.llm_funnel.unwrap();
    assert_eq!((funnel.ground_verdicts, funnel.grounded, funnel.stored), (1, 1, 1));
}

#[test]
fn ground_holds_at_the_threshold_and_drops_below_it() {
    let at = e1(GROUND_YES, verify_keep(0.9), Some(e1_fake(DECIDE_MIN_P, 0.9)));
    assert_eq!(at.stored.len(), 1, "p = 0.75 grounds");
    let below = e1(GROUND_YES, verify_keep(0.9), Some(e1_fake(0.74, 0.9)));
    assert!(below.stored.is_empty(), "p = 0.74 does not — even though the LLM GROUND would have");
    let f = below.run.llm_funnel.unwrap();
    assert_eq!((f.ground_verdicts, f.grounded), (1, 0), "a refusal, not a failed call");
    assert!(!f.ground_call_failed);
}

#[test]
fn verify_routes_on_the_decision_not_the_self_report() {
    // The verifier is sure; the decision is not → dropped at the floor.
    let doubt = e1(GROUND_YES, verify_keep(0.99), Some(e1_fake(0.9, 0.5)));
    assert!(doubt.stored.is_empty());
    // The verifier is unsure; the decision is → stored, both numbers shown.
    let sure = e1(GROUND_YES, verify_keep(0.4), Some(e1_fake(0.9, 0.85)));
    assert_eq!(sure.stored.len(), 1);
    assert_eq!(sure.stored[0].confidence, 0.85);
    assert_eq!(sure.stored[0].llm_confidence, Some(0.4));
    // A kill is still a kill, whatever the decision says.
    let killed = e1(
        GROUND_YES,
        r#"{"results":[{"id":0,"keep":false,"confidence":0.9,"reason":"vague"}]}"#.into(),
        Some(e1_fake(0.99, 0.99)),
    );
    assert!(killed.stored.is_empty());
}

#[test]
fn an_uncalibrated_decider_never_grounds_or_routes() {
    // Rule 2: an uncalibrated probability may not drop (or admit) a draft —
    // GROUND and VERIFY run exactly as with no backend.
    let out = e1(GROUND_YES, verify_keep(0.8), Some(e1_fake(0.0, 0.0).uncalibrated()));
    assert!(out.decide_log.is_empty(), "not even asked");
    assert!(out.llm_log.iter().any(|r| r.contains(r#""op":"ground""#)));
    assert_eq!(out.stored.len(), 1);
    assert_eq!(out.stored[0].confidence, 0.8, "the self-report routes, as today");
    assert!(out.stored[0].judged_by.is_none());
    assert!(out.stored[0].llm_confidence.is_none());
    assert!(!out.run.decider.unwrap().calibrated);
}

#[test]
fn an_uncalibrated_response_from_a_calibrated_backend_falls_back() {
    struct Emulated;
    impl crate::decide::DecideBackend for Emulated {
        fn decide(&self, _: &str) -> Result<String> {
            Ok(json!({"answers": {
                "ev_e1": {"type": "noul", "noul": 0.0}, "ev_e2": {"type": "noul", "noul": 0.0},
                "sound": {"type": "noul", "noul": 0.0}},
                "provider": "llm", "model": "m", "calibrated": false, "latency_ms": 1})
            .to_string())
        }
        fn calibrated(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "chain".into()
        }
    }
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let e = Engine::empty()
        .with_llm(Box::new(OpLlm { ground: GROUND_YES.into(), verify: verify_keep(0.8), log: log.clone() }))
        .with_decider(Box::new(Emulated));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    assert_eq!(recs.len(), 1, "the zero probabilities did not drop the draft");
    assert!(recs[0].judged_by.is_none());
    assert!(log.lock().unwrap().iter().any(|r| r.contains(r#""op":"ground""#)));
}

#[test]
fn a_failing_decider_falls_back_to_the_llm_ground_and_the_run_continues() {
    let out = e1(GROUND_YES, verify_keep(0.8), Some(e1_fake(0.9, 0.9).failing()));
    assert!(out.run.ran());
    assert!(out.llm_log.iter().any(|r| r.contains(r#""op":"ground""#)), "today's rule");
    assert_eq!(out.stored.len(), 1);
    assert!(out.stored[0].judged_by.is_none());
    assert_eq!(out.stored[0].confidence, 0.8);
    let rep = out.run.decider.unwrap();
    assert_eq!(rep.failed_calls, 1);
    assert!(rep.last_error.unwrap().starts_with("LOP-E051 "));

    // Garbage is a failure too.
    struct Garbage;
    impl crate::decide::DecideBackend for Garbage {
        fn decide(&self, _: &str) -> Result<String> {
            Ok(r#"{"answers":{"sound":{"type":"noul","noul":7}}}"#.into())
        }
        fn calibrated(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "garbage".into()
        }
    }
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let e = Engine::empty()
        .with_llm(Box::new(OpLlm { ground: GROUND_YES.into(), verify: verify_keep(0.8), log }))
        .with_decider(Box::new(Garbage));
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.decider.unwrap().failed_calls, 1);
    assert_eq!(e.recommendations(&sub.inner, None).unwrap().len(), 1);
}

#[test]
fn a_decider_without_an_llm_changes_nothing_at_ground() {
    // No LLM → no DISCOVER → no GROUND; the decider is not consulted for E1.
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "deploy_target", "us-east-1");
    let f = e1_fake(0.9, 0.9);
    let log = f.calls();
    let e = Engine::empty().with_decider(Box::new(f));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert!(log.lock().unwrap().is_empty());
}

// ---- E2: duplicate sweep -------------------------------------------------------

const A: &str = "customer prefers email contact for billing questions";
const B: &str = "customer prefers email contact for invoices"; // Jaccard 5/8 = 0.625

#[test]
fn judged_duplicates_propose_the_same_supersede_draft() {
    let mut sub = TestSubstrate::new();
    let ha = sub.add_observation("test", A);
    let hb = sub.add_observation("test", B);
    sub.add_observation("test", "the server rebooted at midnight"); // Jaccard 0 → never asked

    assert!(sub.analyze(&DuplicateSweep::new(), 10_000).is_empty(), "below 0.9 with no backend");

    let f = FakeDecider::constant(0.9);
    let log = f.calls();
    let d = decider(f);
    let drafts = sub.analyze_decided(&DuplicateSweep::new(), 10_000, &d);
    assert_eq!(asked(&log), ["p0"], "only the in-band pair is asked");
    let req = &log.lock().unwrap()[0];
    assert_eq!(req["state"]["pairs"]["p0"], json!({"a": A, "b": B}));
    assert_eq!(
        req["questions"]["p0"]["instructions"],
        "Do texts \"a\" and \"b\" of pair \"p0\" (in state.pairs) state the same claim?"
    );
    assert_eq!(drafts.len(), 1);
    let dr = &drafts[0];
    assert_eq!(dr.target_ref, format!("grain:{ha}"), "the earliest is canonical");
    assert_eq!(dr.evidence, vec![ha, hb.clone()]);
    match &dr.proposal {
        crate::recommendation::Proposal::Cal { cal } => {
            assert!(cal.starts_with(&format!("SUPERSEDE {hb}")), "{cal}");
            assert!(cal.contains(A), "superseded with the canonical body: {cal}");
        }
        other => panic!("not a CAL proposal: {other:?}"),
    }
    assert_eq!(dr.summary.template_id, "duplicate.judged");
    assert!(dr.summary.render().contains("p = 0.9"), "{}", dr.summary.render());
    let j = dr.judged_by.as_ref().unwrap();
    assert_eq!((j.stage.as_str(), j.answers.get("same_claim")), ("duplicate", Some(&0.9)));
}

#[test]
fn judged_duplicates_need_a_calibrated_p_at_the_threshold() {
    let mut sub = TestSubstrate::new();
    sub.add_observation("test", A);
    sub.add_observation("test", B);
    assert!(sub.analyze_decided(&DuplicateSweep::new(), 10_000, &decider(FakeDecider::constant(0.74))).is_empty());
    assert_eq!(sub.analyze_decided(&DuplicateSweep::new(), 10_000, &decider(FakeDecider::constant(0.75))).len(), 1);

    let unc = FakeDecider::constant(0.99).uncalibrated();
    let log = unc.calls();
    assert!(sub.analyze_decided(&DuplicateSweep::new(), 10_000, &decider(unc)).is_empty());
    assert!(log.lock().unwrap().is_empty(), "an uncalibrated backend is not even asked");

    let failing = decider(FakeDecider::constant(0.99).failing());
    assert!(sub.analyze_decided(&DuplicateSweep::new(), 10_000, &failing).is_empty());
    assert_eq!(failing.report().failed_calls, 1);
}

#[test]
fn the_jaccard_path_is_unchanged_and_its_pairs_are_not_asked() {
    let mut sub = TestSubstrate::new();
    sub.add_observation("test", "user asked about pricing tiers refunds billing invoices today");
    sub.add_observation("test", "user asked about pricing tiers refunds billing invoices today please");
    let f = FakeDecider::constant(0.99);
    let log = f.calls();
    let drafts = sub.analyze_decided(&DuplicateSweep::new(), 10_000, &decider(f));
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].summary.template_id, "duplicate.near");
    assert!(drafts[0].judged_by.is_none());
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn the_duplicate_pair_cap_bounds_what_is_asked() {
    let mut sub = TestSubstrate::new();
    for x in ["one", "two", "three", "four", "five"] {
        // 4 shared tokens of 6 → Jaccard 0.667 for every pair: 10 pairs.
        sub.add_observation("test", &format!("alpha beta gamma delta {x}"));
    }
    let f = FakeDecider::constant(0.1);
    let log = f.calls();
    sub.analyze_decided(&DuplicateSweep::new(), 10_000, &decider(f).with_pair_cap(3));
    assert_eq!(asked(&log).len(), 3);

    let f = FakeDecider::constant(0.1);
    let log = f.calls();
    sub.analyze_decided(&DuplicateSweep::new(), 10_000, &decider(f));
    assert_eq!(asked(&log).len(), 10, "under the default cap every in-band pair is asked");
}

#[test]
fn duplicate_pairs_are_batched_per_request() {
    let mut sub = TestSubstrate::new();
    for i in 0..8 {
        sub.add_observation("test", &format!("alpha beta gamma delta epsilon zeta v{i}"));
    }
    // 28 pairs (Jaccard 6/8 = 0.75) → two requests of ≤16.
    let f = FakeDecider::constant(0.1);
    let log = f.calls();
    sub.analyze_decided(&DuplicateSweep::new(), 10_000, &decider(f));
    let sizes: Vec<usize> = log
        .lock()
        .unwrap()
        .iter()
        .map(|r| r["questions"].as_object().unwrap().len())
        .collect();
    assert_eq!(sizes, [16, 12]);
}

// ---- E2: contradiction sweep -----------------------------------------------------

#[test]
fn judged_contradictions_widen_past_the_seeded_relations() {
    let mut sub = TestSubstrate::new();
    let old = sub.add_fact("bob", "favorite_color", "red");
    let new = sub.add_fact("bob", "favorite_color", "blue");
    sub.add_fact("bob", "likes", "pizza");
    sub.add_fact("bob", "likes", "sushi");
    sub.add_fact("acme", "tier", "gold");
    sub.add_fact("acme", "tier", "silver"); // seeded → the deterministic path

    // p(both true): the colors cannot both be, the foods can.
    let f = FakeDecider::by(|id, req| {
        let a = req["state"]["pairs"][id]["a"].as_str().unwrap_or("");
        let p = if a.contains("favorite_color") { 0.1 } else { 0.95 };
        json!({"type": "noul", "noul": p})
    });
    let log = f.calls();
    let mut drafts = sub.analyze_decided(&ContradictionSweep::new(), 10_000, &decider(f));
    drafts.sort_by(|a, b| a.summary.template_id.cmp(&b.summary.template_id));
    assert_eq!(drafts.len(), 2, "{drafts:?}");
    assert_eq!(drafts[0].summary.template_id, "contradiction.functional", "seeded path unchanged");
    assert!(drafts[0].judged_by.is_none());

    assert_eq!(asked(&log).len(), 2, "one pair each for favorite_color and likes");
    let reqs = log.lock().unwrap();
    let texts: Vec<String> = reqs
        .iter()
        .flat_map(|r| r["state"]["pairs"].as_object().unwrap().values().map(|p| p["a"].to_string()).collect::<Vec<_>>())
        .collect();
    assert!(texts.iter().all(|t| !t.contains("tier")), "a seeded relation is never asked: {texts:?}");
    assert!(reqs[0]["questions"]["p0"]["instructions"]
        .as_str()
        .unwrap()
        .ends_with("both be true at the same time?"));
    drop(reqs);

    let j = &drafts[1];
    assert_eq!(j.summary.template_id, "contradiction.judged");
    let text = j.summary.render();
    assert!(text.contains("\"favorite_color\" is not a seeded functional relation"), "{text}");
    assert!(text.contains("p = 0.9"), "{text}");
    assert_eq!(j.target_ref, "entity:test/bob");
    assert_eq!(j.evidence, vec![old.clone(), new.clone()]);
    match &j.proposal {
        crate::recommendation::Proposal::Cal { cal } => {
            assert!(cal.starts_with(&format!("SUPERSEDE {old}")), "the OLDER value is superseded: {cal}");
            assert!(cal.contains("blue") && !cal.contains(&format!("SUPERSEDE {new}")));
        }
        other => panic!("{other:?}"),
    }
    assert!(j.metric.is_none(), "no recurrence metric on an undeclared relation");
    let jb = j.judged_by.as_ref().unwrap();
    assert_eq!(jb.stage, "contradiction");
    assert_eq!(jb.answers.values().copied().collect::<Vec<_>>(), [0.9]);
}

#[test]
fn judged_contradictions_need_a_calibrated_p_and_fail_soft() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("bob", "favorite_color", "red");
    sub.add_fact("bob", "favorite_color", "blue");
    let run = |f: FakeDecider| sub.analyze_decided(&ContradictionSweep::new(), 10_000, &decider(f));
    assert!(run(FakeDecider::constant(0.26)).is_empty(), "1 − 0.26 = 0.74 < 0.75");
    assert_eq!(run(FakeDecider::constant(0.25)).len(), 1, "1 − 0.25 = 0.75");
    let unc = FakeDecider::constant(0.0).uncalibrated();
    let log = unc.calls();
    assert!(run(unc).is_empty());
    assert!(log.lock().unwrap().is_empty());
    assert!(run(FakeDecider::constant(0.0).failing()).is_empty());
    assert!(sub.analyze(&ContradictionSweep::new(), 10_000).is_empty(), "no backend: unchanged");
}

#[test]
fn the_contradiction_pair_cap_bounds_what_is_asked() {
    let mut sub = TestSubstrate::new();
    for c in ["red", "blue", "green", "black", "white"] {
        sub.add_fact("bob", "favorite_color", c); // 10 pairs
    }
    let f = FakeDecider::constant(0.9);
    let log = f.calls();
    sub.analyze_decided(&ContradictionSweep::new(), 10_000, &decider(f).with_pair_cap(4));
    assert_eq!(asked(&log).len(), 4);
}

// ---- E3: tool failure cause ------------------------------------------------------

const SLOW: &str = "the upstream took far too long and the gateway gave up";

fn cause_fake(choice: &'static str, p: f64) -> FakeDecider {
    FakeDecider::by(move |_, _| {
        let rest = (1.0 - p) / 2.0;
        let mut probs = serde_json::Map::new();
        probs.insert(choice.into(), json!(p));
        for other in ["unknown", "executor_error", "timeout"] {
            if other != choice && probs.len() < 3 {
                probs.insert(other.into(), json!(rest));
            }
        }
        json!({"type": "choice", "choice": choice, "probabilities": probs})
    })
}

fn slow_tool(sub: &mut TestSubstrate, extra: &[(&str, &str)]) {
    for _ in 0..5 {
        sub.add_tool_error_with("refund", "gateway error 504", extra);
    }
    sub.add_tool_call("refund", false, "ok");
}

#[test]
fn a_free_text_cause_is_classified_once_and_named() {
    let mut sub = TestSubstrate::new();
    slow_tool(&mut sub, &[("failure_detail", SLOW)]);
    let f = cause_fake("timeout", 0.8);
    let log = f.calls();
    let d = decider(f);
    let drafts = sub.analyze_decided(&ToolFailureClustering::new(), 10_000, &d);
    assert_eq!(drafts.len(), 1);
    let dr = &drafts[0];
    assert_eq!(dr.summary.template_id, "tool_failure.cluster_cause");
    assert!(dr.summary.render().ends_with("(cause: timeout)"), "{}", dr.summary.render());
    let j = dr.judged_by.as_ref().unwrap();
    assert_eq!((j.stage.as_str(), j.answers.get("timeout")), ("tool_cause", Some(&0.8)));

    {
        let reqs = log.lock().unwrap();
        assert_eq!(reqs.len(), 1, "five grains, one distinct string, one question");
        assert_eq!(reqs[0]["state"]["failures"]["c0"], SLOW);
        let q = &reqs[0]["questions"]["c0"];
        assert_eq!(q["type"], "choice");
        let options: Vec<&String> = q["criteria"].as_object().unwrap().keys().collect();
        assert_eq!(options.len(), crate::decide::TOOL_CAUSES.len());
    }
    // Cached for the run: the same string is not asked again.
    sub.analyze_decided(&ToolFailureClustering::new(), 10_000, &d);
    assert_eq!(log.lock().unwrap().len(), 1, "cache hit");
    // …and the cache is per run.
    d.reset();
    sub.analyze_decided(&ToolFailureClustering::new(), 10_000, &d);
    assert_eq!(log.lock().unwrap().len(), 2);
}

#[test]
fn a_low_confidence_or_uncalibrated_classification_stays_unknown() {
    let mut sub = TestSubstrate::new();
    slow_tool(&mut sub, &[("failure_detail", SLOW)]);
    let cause = |f: FakeDecider| {
        let d = sub.analyze_decided(&ToolFailureClustering::new(), 10_000, &decider(f));
        (d[0].summary.args.get("cause").cloned(), d[0].judged_by.clone())
    };
    assert_eq!(cause(cause_fake("timeout", 0.59)), (Some(json!("unknown")), None), "below 0.6");
    assert_eq!(cause(cause_fake("timeout", 0.6)).0, Some(json!("timeout")));
    let unc = cause_fake("timeout", 0.99).uncalibrated();
    let log = unc.calls();
    assert_eq!(cause(unc), (Some(json!("unknown")), None));
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(cause(cause_fake("timeout", 0.99).failing()), (Some(json!("unknown")), None));
}

#[test]
fn a_known_cause_is_used_as_recorded_and_no_backend_changes_nothing() {
    let mut sub = TestSubstrate::new();
    slow_tool(&mut sub, &[("failure_cause", "context_overflow")]);
    let f = cause_fake("timeout", 0.99);
    let log = f.calls();
    let d = sub.analyze_decided(&ToolFailureClustering::new(), 10_000, &decider(f));
    assert_eq!(d[0].summary.args.get("cause"), Some(&json!("context_overflow")));
    assert!(d[0].judged_by.is_none());
    assert!(log.lock().unwrap().is_empty(), "nothing to classify");

    // An unknown `failure_cause` string is the free text (it wins over detail).
    let mut sub = TestSubstrate::new();
    slow_tool(&mut sub, &[("failure_cause", "gateway timed out"), ("failure_detail", "ignored")]);
    let f = cause_fake("timeout", 0.9);
    let log = f.calls();
    sub.analyze_decided(&ToolFailureClustering::new(), 10_000, &decider(f));
    assert_eq!(log.lock().unwrap()[0]["state"]["failures"]["c0"], "gateway timed out");

    // No backend: the draft is byte-for-byte today's.
    let plain = sub.analyze(&ToolFailureClustering::new(), 10_000);
    assert_eq!(plain[0].summary.template_id, "tool_failure.cluster");
    assert!(!plain[0].summary.args.contains_key("cause"));
}

#[test]
fn a_cluster_with_no_cause_signal_is_unchanged_under_a_backend() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("stripe_refund", true, "rate_limited 429");
    }
    let with = sub.analyze_decided(&ToolFailureClustering::new(), 10_000, &decider(FakeDecider::constant(0.9)));
    assert_eq!(with, sub.analyze(&ToolFailureClustering::new(), 10_000));
}

// ---- the gates are untouched ------------------------------------------------------

/// A structural-curation analyzer that emits exactly `duplicate_sweep`'s
/// value-identical consolidation — optionally stamped as judged.
struct Probe {
    manifest: AnalyzerManifest,
    judged: bool,
}

impl Probe {
    fn new(judged: bool) -> Self {
        let mut manifest = DuplicateSweep::new().manifest().clone();
        manifest.id = "loop.judged_probe/1".into();
        Probe { manifest, judged }
    }
}

impl Analyzer for Probe {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }
    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let drafts = DuplicateSweep::new().analyze(ctx)?;
        Ok(drafts
            .into_iter()
            .map(|d| {
                if self.judged {
                    d.judged_by(crate::decide::JudgedBy {
                        backend: "fake:fake-1".into(),
                        provider: "fake".into(),
                        model: "fake-1".into(),
                        calibrated: true,
                        latency_ms: 1,
                        stage: "duplicate".into(),
                        answers: [("same_claim".to_string(), 0.99)].into(),
                    })
                } else {
                    d
                }
            })
            .collect())
    }
}

#[test]
fn a_judged_recommendation_never_auto_applies_even_when_granted() {
    let policy = || {
        Policy::from_json(
            r#"{"auto_apply_enabled": true,
                "auto_apply": [
                  {"analyzer": "loop.judged_probe", "targets": ["memory"], "max_severity": "high"},
                  {"analyzer": "loop.llm", "targets": ["memory"], "max_severity": "high"}]}"#,
        )
        .unwrap()
    };
    let run = |judged: bool| {
        let mut sub = TestSubstrate::new();
        sub.add_fact("acme", "tier", "Enterprise");
        sub.add_fact("acme", "tier", "Enterprise");
        let mut e = Engine::empty().with_policy(policy());
        e.register(Box::new(Probe::new(judged)));
        let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
        let recs = e.recommendations(&sub.inner, None).unwrap();
        (r.auto_applied, recs)
    };
    // Positive control: the same payload, unjudged, DOES auto-apply — so the
    // zero below is the guard, not a policy that grants nothing.
    let (applied, _) = run(false);
    assert_eq!(applied, 1);
    let (applied, recs) = run(true);
    assert_eq!(applied, 0, "a model's probability never drives an apply");
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].status, RecStatus::Pending);
    assert_eq!(
        recs[0].judged_by.as_ref().map(|j| j.stage.as_str()),
        Some("duplicate"),
        "judged_by survives the grain round-trip"
    );
    // And model ORIGIN stays categorically ineligible.
    assert!(!Origin::Llm { model: "m".into() }.auto_apply_eligible());
}

#[test]
fn replay_never_consults_the_decider() {
    use std::collections::BTreeMap;
    let mut sub = TestSubstrate::new();
    sub.add_observation("test", A);
    sub.add_observation("test", B);
    let f = FakeDecider::constant(0.99);
    let log = f.calls();
    let e = Engine::with_builtins().with_decider(Box::new(f));
    let recs = e
        .analyze_only(&sub.inner, &RunOptions::default(), &BTreeMap::new(), 10_000)
        .unwrap();
    // analyze_only IS the production path (no persistence): it may ask.
    assert!(recs.iter().any(|r| r.judged_by.is_some()));
    let asked_live = log.lock().unwrap().len();
    assert!(asked_live > 0);
    let opts = crate::replay::ReplayOptions {
        since_ms: Some(0),
        until_ms: 10_000,
        step: crate::replay::ReplayStep::Stride(5_000),
        namespaces: Vec::new(),
    };
    e.replay(&sub.inner, &crate::replay::ReplayCandidate::default(), &opts).unwrap();
    assert_eq!(log.lock().unwrap().len(), asked_live, "a rehearsal asks nothing");
}
