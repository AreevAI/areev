//! Decision-backend phase 3 (row A2) on CAL's multi-source `ASSEMBLE`.
//!
//! With `AreevFacade::set_decider`, each non-pinned source that has a query
//! text (`RECALL … ABOUT "…"`) has its hits judged; the budget
//! trim then spends on the most relevant first instead of cutting the tail,
//! and a CALIBRATED backend also drops off-topic hits before the budget —
//! announced as `CAL-W019`. An uncalibrated backend reorders the trim only.
//! Every failure falls back to today's tail-first trim. The backend here is a
//! scripted in-process fake: deterministic, keyless.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use areev_cal::executor::CalResultPayload;
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig, CalStoreFacade};
use areev_core::decide::{Answer, DecideError, DecideRequest, Decision, DecisionBackend};
use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

/// `ANSWER` → level 3, `RELEVANT` → 2, `OFFTOPIC` → 0, else 1.
struct Fake {
    calibrated: bool,
    fail: bool,
    seen: Mutex<Vec<DecideRequest>>,
}

impl Fake {
    fn new(calibrated: bool) -> Arc<Self> {
        Arc::new(Fake { calibrated, fail: false, seen: Mutex::new(Vec::new()) })
    }
    fn failing() -> Arc<Self> {
        Arc::new(Fake { calibrated: true, fail: true, seen: Mutex::new(Vec::new()) })
    }
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl DecisionBackend for Fake {
    fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError> {
        req.validate()?;
        self.seen.lock().unwrap().push(req.clone());
        if self.fail {
            return Err(DecideError::Deadline("scripted".into()));
        }
        let mut answers = BTreeMap::new();
        for (n, c) in req.state["candidates"].as_array().unwrap().iter().enumerate() {
            let text = c["text"].as_str().unwrap();
            let level = if text.contains("ANSWER") {
                3
            } else if text.contains("RELEVANT") {
                2
            } else if text.contains("OFFTOPIC") {
                0
            } else {
                1
            };
            let probabilities = (0..4).map(|i| (i.to_string(), if i == level { 1.0 } else { 0.0 })).collect();
            let legend = (0..4).map(|i| (i.to_string(), format!("l{i}"))).collect();
            answers.insert(
                format!("rel_{n}"),
                Answer::Score { score: level as f32, probabilities, confidence: 1.0, legend },
            );
            answers.insert(format!("verbatim_{n}"), Answer::Noul { p: 0.1 });
        }
        Ok(Decision {
            answers,
            model: "fake-1".into(),
            provider: "fake".into(),
            calibrated: self.calibrated,
            input_tokens: None,
            output_tokens: None,
            usd_micros: None,
            latency_ms: 1,
        })
    }
    fn calibrated(&self) -> bool {
        self.calibrated
    }
    fn describe(&self) -> String {
        "fake:fake-1".into()
    }
}

/// `markers[i]` becomes grain i's object; every object mentions "badge" so
/// one ABOUT matches them all.
fn seeded(dir: &TempDir, markers: &[&str]) -> AreevFacade {
    let mut m = Areev::open(dir.path().join("d.db").to_str().unwrap()).unwrap();
    for (i, mk) in markers.iter().enumerate() {
        let object = format!(
            "{mk} badge note number {i} padded so that a small budget binds on these grains"
        );
        let mut f = Fact::new(&format!("s{i}"), "note", &object).confidence(0.9);
        f.common.namespace = Some("b".to_string());
        f.common.created_at = Some(1_740_000_000_000 + i as i64 * 1000);
        m.add(&f).unwrap();
    }
    AreevFacade::with_session(m, Some("b".to_string()), None)
}

struct Out {
    objects: Vec<String>,
    omitted: Vec<String>,
    total_available: Option<usize>,
    warnings: Vec<String>,
}

fn run(facade: &AreevFacade, src: &str) -> Out {
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let res = ex.execute(src, facade).unwrap();
    let obj = |g: &areev_cal::executor::CalGrainResult| {
        g.fields["object"].as_str().or(g.fields["content"].as_str()).unwrap_or("").to_string()
    };
    match res.result {
        CalResultPayload::Assembled { grains, sources, total_available, .. } => Out {
            objects: grains.iter().map(obj).collect(),
            omitted: sources.iter().flat_map(|s| s.omitted.iter().map(obj)).collect(),
            total_available,
            warnings: res.warnings,
        },
        other => panic!("expected Assembled, got {other:?}"),
    }
}

fn count(xs: &[String], marker: &str) -> usize {
    xs.iter().filter(|o| o.contains(marker)).count()
}

const ABOUT: &str =
    r#"ASSEMBLE "e" FROM e: (RECALL facts ABOUT "badge" WHERE namespace = "b" LIMIT 50) BUDGET 16000 tokens"#;

#[test]
fn a_calibrated_backend_drops_off_topic_hits_and_says_so() {
    let d = TempDir::new().unwrap();
    let markers = ["OFFTOPIC", "ANSWER", "OFFTOPIC", "RELEVANT", "ANSWER", "OFFTOPIC", "plain"];
    let mut facade = seeded(&d, &markers);

    let today = run(&facade, ABOUT);
    assert_eq!(today.objects.len(), 7, "positive control: the ceiling holds all seven");
    assert!(!today.warnings.iter().any(|w| w.starts_with("CAL-W019")));

    let fake = Fake::new(true);
    facade.set_decider(fake.clone());
    let out = run(&facade, ABOUT);
    assert_eq!(count(&out.objects, "OFFTOPIC"), 0, "{:?}", out.objects);
    assert_eq!(out.objects.len(), 4);
    // The dropped grains stay accountable: ELEMENT_OMIT sees them and
    // total_available is still the pre-judgment count.
    assert_eq!(count(&out.omitted, "OFFTOPIC"), 3);
    assert_eq!(out.total_available, Some(7));
    let w = out.warnings.iter().find(|w| w.starts_with("CAL-W019")).unwrap_or_else(|| {
        panic!("no CAL-W019 among {:?}", out.warnings)
    });
    assert!(w.contains("fake") && w.contains("3 grain(s)") && w.contains("[e]"), "{w}");
    // Nothing was cut by the BUDGET, so W017 stays silent.
    assert!(!out.warnings.iter().any(|w| w.starts_with("CAL-W017")), "{:?}", out.warnings);
    assert_eq!(fake.calls(), 1);
}

#[test]
fn judged_relevance_decides_what_the_budget_trims() {
    // Recall order has the ANSWERs last; a tail-first trim cuts them.
    let d = TempDir::new().unwrap();
    let markers: Vec<&str> = std::iter::repeat_n("plain", 30).chain(std::iter::repeat_n("ANSWER", 4)).collect();
    let mut facade = seeded(&d, &markers);
    let q = r#"ASSEMBLE "e" FROM e: (RECALL facts ABOUT "badge" WHERE namespace = "b" ORDER BY created_at ASC LIMIT 50) BUDGET 300 tokens"#;

    let today = run(&facade, q);
    assert!(today.objects.len() < 34, "fixture must bind the budget");
    assert_eq!(count(&today.objects, "ANSWER"), 0, "positive control: tail-first loses them");

    // Uncalibrated: order only — the ANSWERs win the budget, nothing is
    // dropped by the backend, and survivors keep their recall order.
    facade.set_decider(Fake::new(false));
    let out = run(&facade, q);
    assert_eq!(count(&out.objects, "ANSWER"), 4, "{:?}", out.objects);
    assert_eq!(out.objects.len(), today.objects.len(), "same budget, same count");
    assert!(!out.warnings.iter().any(|w| w.starts_with("CAL-W019")));
    assert!(out.warnings.iter().any(|w| w.starts_with("CAL-W017")), "the budget still bound");
    let nums: Vec<usize> = out
        .objects
        .iter()
        .map(|o| o.split("number ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap())
        .collect();
    let mut sorted = nums.clone();
    sorted.sort();
    assert_eq!(nums, sorted, "kept grains stay in recall order");
}

#[test]
fn an_uncalibrated_backend_never_drops() {
    let d = TempDir::new().unwrap();
    let mut facade = seeded(&d, &["OFFTOPIC", "ANSWER", "OFFTOPIC"]);
    facade.set_decider(Fake::new(false));
    let out = run(&facade, ABOUT);
    assert_eq!(out.objects.len(), 3);
    assert!(!out.warnings.iter().any(|w| w.starts_with("CAL-W019")));
}

#[test]
fn a_failing_backend_falls_back_to_the_tail_first_trim() {
    let d = TempDir::new().unwrap();
    let markers: Vec<&str> = std::iter::repeat_n("plain", 30).chain(std::iter::repeat_n("ANSWER", 4)).collect();
    let mut facade = seeded(&d, &markers);
    let q = r#"ASSEMBLE "e" FROM e: (RECALL facts ABOUT "badge" WHERE namespace = "b" ORDER BY created_at ASC LIMIT 50) BUDGET 300 tokens"#;
    let today = run(&facade, q);
    let fake = Fake::failing();
    facade.set_decider(fake.clone());
    let out = run(&facade, q);
    assert_eq!(out.objects, today.objects);
    assert_eq!(out.omitted, today.omitted);
    assert_eq!(out.warnings, today.warnings);
    assert_eq!(fake.calls(), 1, "it was asked, and failed open");
}

#[test]
fn pins_literals_and_query_less_sources_are_never_judged() {
    let d = TempDir::new().unwrap();
    let mut facade = seeded(&d, &["OFFTOPIC", "ANSWER", "OFFTOPIC"]);
    let fake = Fake::new(true);
    facade.set_decider(fake.clone());
    let q = r#"ASSEMBLE "e" FROM
        rule: PIN LITERAL "OFFTOPIC literal text",
        pinned: PIN (RECALL facts ABOUT "badge" WHERE namespace = "b" LIMIT 50),
        plain: (RECALL facts WHERE namespace = "b" LIMIT 50)
        BUDGET 16000 tokens"#;
    let out = run(&facade, q);
    assert_eq!(fake.calls(), 0, "a pin is non-degradable, and the others have no query");
    assert_eq!(count(&out.objects, "OFFTOPIC"), 3, "{:?}", out.objects);
}

#[test]
fn the_decider_is_host_config_a_principal_session_shares() {
    let d = TempDir::new().unwrap();
    let mut facade = seeded(&d, &["x"]);
    assert!(facade.decider().is_none());
    assert!(CalStoreFacade::decider(&facade).is_none());
    facade.set_decider(Fake::new(true));
    {
        let session = facade.principal_session("user:local").unwrap();
        assert!(CalStoreFacade::decider(&session).is_some());
    }
    facade.clear_decider();
    assert!(facade.decider().is_none());
}
