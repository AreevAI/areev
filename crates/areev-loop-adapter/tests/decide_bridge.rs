//! `LoopDecider`: an `areev_core::decide::DecisionBackend` seen through the
//! loop's wire-JSON `DecideBackend` seam, and driven by the real engine.

use areev_core::decide::{Answer, DecideError, DecideRequest, Decision, DecisionBackend, Question};
use areev_loop::DecideBackend;
use areev_loop_adapter::LoopDecider;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Answers every noul with `p`, every choice with `timeout` at `p`; logs requests.
struct Fake {
    p: f32,
    calibrated: bool,
    fail: bool,
    seen: Mutex<Vec<DecideRequest>>,
}

impl Fake {
    fn new(p: f32) -> Self {
        Fake { p, calibrated: true, fail: false, seen: Mutex::new(Vec::new()) }
    }
}

impl DecisionBackend for Fake {
    fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError> {
        self.seen.lock().unwrap().push(req.clone());
        if self.fail {
            return Err(DecideError::Deadline("scripted".into()));
        }
        let answers = req
            .questions
            .iter()
            .map(|(id, q)| {
                let a = match q {
                    Question::Noul { .. } => Answer::Noul { p: self.p },
                    Question::Choice { criteria, .. } => {
                        let rest = (1.0 - self.p) / (criteria.len() as f32 - 1.0);
                        let probabilities: BTreeMap<String, f32> = criteria
                            .keys()
                            .map(|k| (k.clone(), if k == "timeout" { self.p } else { rest }))
                            .collect();
                        Answer::Choice { choice: "timeout".into(), probabilities, confidence: 0.5 }
                    }
                    Question::Score { .. } => unreachable!("the loop asks no scores"),
                };
                (id.clone(), a)
            })
            .collect();
        Ok(Decision {
            answers,
            model: "fake-1".into(),
            provider: "fake".into(),
            calibrated: self.calibrated,
            input_tokens: None,
            output_tokens: None,
            usd_micros: None,
            latency_ms: 4,
        })
    }
    fn calibrated(&self) -> bool {
        self.calibrated
    }
    fn describe(&self) -> String {
        "fake:fake-1".into()
    }
}

#[test]
fn the_bridge_round_trips_the_wire_shape() {
    let fake = Arc::new(Fake::new(0.875));
    let bridge = LoopDecider(fake.clone());
    assert!(bridge.calibrated());
    assert_eq!(bridge.describe(), "fake:fake-1");
    let raw = bridge
        .decide(
            r#"{"state":{"pairs":{"p0":{"a":"x","b":"y"}}},
                "questions":{"p0":{"type":"noul","instructions":"same claim?"},
                             "c0":{"type":"choice","instructions":"why?",
                                   "criteria":{"timeout":"slow","unknown":"other"}}}}"#,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["answers"]["p0"]["noul"], 0.875);
    assert_eq!(v["answers"]["c0"]["probabilities"]["timeout"], 0.875);
    assert_eq!((v["provider"].as_str(), v["model"].as_str()), (Some("fake"), Some("fake-1")));
    assert_eq!(v["calibrated"], true);
    assert_eq!(v["latency_ms"], 4);
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen[0].state["pairs"]["p0"]["a"], "x");
    assert_eq!(seen[0].questions.len(), 2);
}

#[test]
fn every_bridge_failure_is_lop_e051() {
    let mut failing = Fake::new(0.5);
    failing.fail = true;
    let bridge = LoopDecider(Arc::new(failing));
    let ok_req = r#"{"state":"s","questions":{"q":{"type":"noul","instructions":"?"}}}"#;
    for req in [
        ok_req,                                                        // backend DEC-E004
        "not json",                                                    // unparseable
        r#"{"state":"s","questions":{}}"#,                             // no questions (DEC-E006)
        r#"{"state":"s","questions":{"q":{"type":"noul","instructions":""}}}"#,
    ] {
        let e = bridge.decide(req).unwrap_err();
        assert_eq!(e.code(), "LOP-E051", "{req}: {e}");
    }
}

/// The loop's copy of the tool-cause vocabulary must be exactly the core's
/// `FailureCause` set (the loop cannot import it; this pins the mirror).
#[test]
fn the_loops_tool_cause_vocabulary_matches_core() {
    use areev_core::types::FailureCause;
    let all = [
        FailureCause::Timeout,
        FailureCause::ExecutorError,
        FailureCause::SchemaValidationFailed,
        FailureCause::UserAborted,
        FailureCause::Unknown,
        FailureCause::ContextOverflow,
    ];
    let mut core: Vec<&str> = all.iter().map(|c| c.as_str()).collect();
    let mut looped: Vec<&str> = areev_loop::decide::TOOL_CAUSES.iter().map(|(k, _)| *k).collect();
    core.sort();
    looped.sort();
    assert_eq!(core, looped);
    for k in looped {
        assert!(FailureCause::parse(k).is_some());
    }
}

#[test]
fn the_engine_runs_a_sweep_through_the_bridge() {
    use areev_loop::{Engine, ReferenceSubstrate, RunOptions};
    let mut sub = ReferenceSubstrate::new();
    let e = Engine::with_builtins().with_decider(Box::new(LoopDecider(Arc::new(Fake::new(0.1)))));
    let r = e.run(&mut sub, &RunOptions::default(), 1_000).unwrap();
    let rep = r.decider.expect("reported");
    assert_eq!(rep.backend, "fake:fake-1");
    assert!(rep.calibrated);
}
