//! `DecisionRerank` — the decision-backend reranker (proposal §4 A1):
//! batching boundaries, the state-size split, the in-process cache, score
//! mapping, and whole-call error propagation. Keyless: a fake in-process
//! `DecisionBackend` records every request it receives.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use areev_core::decide::{Answer, DecideError, DecideRequest, Decision, DecisionBackend};
use areev_store::{DecisionRerank, RerankBackend};

/// Answers every `c<n>` with the level named by the candidate text's
/// leading `L<k>` tag (else level 0). Fails on the call numbers in `fail_on`.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<DecideRequest>>,
    fail_on: Vec<usize>,
    wrong_type: bool,
    uncalibrated: bool,
}

impl Fake {
    fn calls(&self) -> Vec<DecideRequest> {
        self.calls.lock().unwrap().clone()
    }
    fn sizes(&self) -> Vec<usize> {
        self.calls().iter().map(|r| r.questions.len()).collect()
    }
}

impl DecisionBackend for Fake {
    fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError> {
        // Every real adapter validates first (the trait's contract).
        req.validate()?;
        let n = {
            let mut c = self.calls.lock().unwrap();
            c.push(req.clone());
            c.len()
        };
        if self.fail_on.contains(&n) {
            return Err(DecideError::Provider {
                provider: "fake".into(),
                status: Some(503),
                message: "down".into(),
                retryable: true,
            });
        }
        let cands = req.state["candidates"].as_array().unwrap();
        let mut answers = BTreeMap::new();
        for (id, q) in &req.questions {
            let i: usize = id[1..].parse().unwrap();
            assert_eq!(cands[i]["i"], i, "question c{i} must point at candidates[{i}]");
            let text = cands[i]["text"].as_str().unwrap();
            let levels = match q {
                areev_core::decide::Question::Score { levels, .. } => levels.len(),
                _ => panic!("rerank asks score questions only"),
            };
            let lvl = text
                .strip_prefix('L')
                .and_then(|t| t.chars().next())
                .and_then(|c| c.to_digit(10))
                .map(|d| (d as usize).min(levels - 1))
                .unwrap_or(0);
            let a = if self.wrong_type {
                Answer::Noul { p: 0.5 }
            } else {
                Answer::Score {
                    score: lvl as f32,
                    probabilities: (0..levels)
                        .map(|k| (k.to_string(), if k == lvl { 1.0 } else { 0.0 }))
                        .collect(),
                    confidence: 1.0,
                    legend: BTreeMap::new(),
                }
            };
            answers.insert(id.clone(), a);
        }
        Ok(Decision {
            answers,
            model: "fake-1".into(),
            provider: "fake".into(),
            calibrated: !self.uncalibrated,
            input_tokens: Some(10),
            output_tokens: Some(2),
            usd_micros: Some(16),
            latency_ms: 1,
        })
    }
    fn calibrated(&self) -> bool {
        !self.uncalibrated
    }
    fn describe(&self) -> String {
        "fake:fake-1".into()
    }
}

fn docs(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("L{} doc number {i}", i % 4)).collect()
}

fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

#[test]
fn scores_follow_the_answer_and_map_into_unit_range() {
    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone());
    let d = docs(4);
    let s = r.rerank("q", &refs(&d)).unwrap();
    // Default 4 levels: level k → k / 3.
    assert_eq!(s, vec![0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]);
    assert_eq!(fake.sizes(), vec![4], "one request for a pool that fits");
    let req = &fake.calls()[0];
    assert_eq!(req.state["query"], "q");
    assert_eq!(req.deadline, None, "no deadline on the rerank seam: the backend default applies");
    assert_eq!(r.model(), "fake:fake-1");
    assert!(r.calibrated());
    let st = r.stats();
    assert_eq!((st.requests(), st.candidates_sent(), st.input_tokens(), st.output_tokens()), (1, 4, 10, 2));
    assert_eq!(st.usd_micros(), 16, "provider-reported cost is summed");
    assert_eq!(st.served(), Some(("fake".into(), "fake-1".into(), true)));
}

#[test]
fn custom_levels_change_the_scale() {
    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone()).with_levels(vec!["no".into(), "yes".into()]);
    let d = vec!["L0 a".to_string(), "L3 b".to_string()]; // L3 clamps to the top of 2 levels
    assert_eq!(r.rerank("q", &refs(&d)).unwrap(), vec![0.0, 1.0]);
}

#[test]
fn a_whole_refine_pool_is_one_request_by_default() {
    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone());
    let d = docs(64);
    r.rerank("q", &refs(&d)).unwrap();
    assert_eq!(fake.sizes(), vec![64]);
}

#[test]
fn batch_boundaries_split_by_count() {
    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone()).with_batch(4);
    let d = docs(10);
    let s = r.rerank("q", &refs(&d)).unwrap();
    assert_eq!(fake.sizes(), vec![4, 4, 2]);
    // Positional alignment survives the split.
    let want: Vec<f32> = (0..10).map(|i| (i % 4) as f32 / 3.0).collect();
    assert_eq!(s, want);
    // Exactly-full batches do not emit an empty tail request.
    let fake = Arc::new(Fake::default());
    DecisionRerank::new(fake.clone()).with_batch(5).rerank("q", &refs(&docs(10))).unwrap();
    assert_eq!(fake.sizes(), vec![5, 5]);
}

#[test]
fn large_candidates_split_under_the_state_budget_and_oversize_is_truncated() {
    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone());
    let big = |k: usize| format!("L{k}{}", "x".repeat(40_000));
    let d = vec![big(1), big(2), big(3)];
    let s = r.rerank("q", &refs(&d)).unwrap();
    assert_eq!(fake.sizes(), vec![2, 1], "~28k-token budget holds two 40k-char docs, not three");
    assert_eq!(s, vec![1.0 / 3.0, 2.0 / 3.0, 1.0]);
    for req in fake.calls() {
        assert!(req.state.to_string().len() / 4 < 28_500, "state stays under the budget");
    }

    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone());
    let huge = format!("L2{}", "é".repeat(100_000)); // multi-byte: truncation must hit a char boundary
    let s = r.rerank("q", &[huge.as_str()]).unwrap();
    assert_eq!(s, vec![2.0 / 3.0]);
    let sent = fake.calls()[0].state["candidates"][0]["text"].as_str().unwrap().len();
    assert!(sent < huge.len() && sent <= 28_000 * 4, "sent {sent} bytes");
}

#[test]
fn cached_candidates_are_never_resent() {
    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone());
    let d = docs(6);
    let first = r.rerank("q", &refs(&d)).unwrap();
    let again = r.rerank("q", &refs(&d)).unwrap();
    assert_eq!(first, again);
    assert_eq!(fake.sizes(), vec![6], "the second call is all cache hits");
    assert_eq!(r.stats().cache_hits(), 6);

    // Overlap: only the new candidate goes out.
    let mut more = d.clone();
    more.push("L3 fresh".into());
    r.rerank("q", &refs(&more)).unwrap();
    assert_eq!(fake.sizes(), vec![6, 1]);

    // The key includes the query: a different query re-asks.
    r.rerank("another q", &refs(&d)).unwrap();
    assert_eq!(fake.sizes(), vec![6, 1, 6]);

    // ...and the levels.
    let r2 = DecisionRerank::new(fake.clone()).with_levels(vec!["a".into(), "b".into(), "c".into()]);
    r2.rerank("q", &refs(&d[..1])).unwrap();
    assert_eq!(fake.sizes(), vec![6, 1, 6, 1]);
}

#[test]
fn duplicate_docs_within_one_call_are_sent_once() {
    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone());
    let s = r.rerank("q", &["L2 same", "L1 other", "L2 same"]).unwrap();
    assert_eq!(fake.sizes(), vec![2]);
    assert_eq!(s, vec![2.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0]);
}

#[test]
fn the_cache_evicts_and_can_be_disabled() {
    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone()).with_cache_entries(2);
    r.rerank("q", &["L1 a", "L2 b", "L3 c"]).unwrap();
    r.rerank("q", &["L1 a", "L2 b", "L3 c"]).unwrap();
    assert!(fake.sizes()[1] >= 1, "a 2-entry cache cannot hold 3 candidates: {:?}", fake.sizes());

    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake.clone()).with_cache_entries(0);
    r.rerank("q", &["L1 a"]).unwrap();
    r.rerank("q", &["L1 a"]).unwrap();
    assert_eq!(fake.sizes(), vec![1, 1]);
}

#[test]
fn a_backend_error_is_an_error_for_the_whole_call_never_partial() {
    let fake = Arc::new(Fake { fail_on: vec![2], ..Default::default() });
    let r = DecisionRerank::new(fake.clone()).with_batch(2);
    let d = docs(5);
    let err = r.rerank("q", &refs(&d)).unwrap_err().to_string();
    assert!(err.contains("DEC-E002") && err.contains("fake:fake-1"), "{err}");
    assert_eq!(fake.sizes(), vec![2, 2], "stops at the failing batch");
    assert_eq!(r.stats().failures(), 1);
    assert_eq!(r.stats().failure_codes().get("DEC-E002"), Some(&1));
    // The batch that did answer is cached: a retry re-sends only the rest.
    r.rerank("q", &refs(&d)).unwrap();
    assert_eq!(fake.sizes(), vec![2, 2, 2, 1]);
}

#[test]
fn a_wrong_answer_type_is_an_error() {
    let fake = Arc::new(Fake { wrong_type: true, ..Default::default() });
    let err = DecisionRerank::new(fake).rerank("q", &["L1 a"]).unwrap_err().to_string();
    assert!(err.contains("expected a score answer"), "{err}");
}

#[test]
fn invalid_levels_are_refused_by_the_backend_not_guessed() {
    // One level is not askable: the backend refuses (DEC-E006) and the call
    // is an error, so recall keeps fusion order.
    let fake = Arc::new(Fake::default());
    let r = DecisionRerank::new(fake).with_levels(vec!["only".into()]);
    let err = r.rerank("q", &["L0 a"]).unwrap_err().to_string();
    assert!(err.contains("DEC-E006"), "{err}");
}

#[test]
fn calibrated_forwards() {
    let r = DecisionRerank::new(Arc::new(Fake { uncalibrated: true, ..Default::default() }));
    assert!(!r.calibrated());
}

#[test]
fn an_empty_pool_sends_nothing() {
    let fake = Arc::new(Fake::default());
    assert!(DecisionRerank::new(fake.clone()).rerank("q", &[]).unwrap().is_empty());
    assert!(fake.sizes().is_empty());
}
