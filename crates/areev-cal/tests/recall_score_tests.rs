//! Real recall scores through the facade (decision-backend phase 0).
//!
//! A RECALL with a free-text leg now carries the store's normalized fused
//! score in `score` (top hit = 1.0) instead of the constant 1.0, so
//! `WITH min_score` on a single RECALL filters on something real; the
//! facade's `set_reranker` / `set_recall_deadline` are reachable and
//! load-bearing.

use areev_cal::executor::CalResultPayload;
use areev_cal::{AreevFacade, CalExecutor, CalExecutorConfig};
use areev_core::error::Result;
use areev_store::{Areev, RerankBackend};
use tempfile::TempDir;

fn setup() -> (CalExecutor, AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn add(ex: &CalExecutor, facade: &AreevFacade, subject: &str, relation: &str, object: &str) {
    let cal = format!(
        r#"ADD fact SET subject = "{subject}" SET relation = "{relation}" SET object = "{object}" SET namespace = "caller" REASON "seed""#
    );
    ex.execute(&cal, facade).unwrap();
}

/// `(object, score)` per returned grain, in result order.
fn scored(payload: &CalResultPayload) -> Vec<(String, f64)> {
    match payload {
        CalResultPayload::Grains { grains, .. } => grains
            .iter()
            .map(|g| {
                let v = serde_json::to_value(g).unwrap();
                (v["fields"]["object"].as_str().unwrap_or("").to_string(), v["score"].as_f64().unwrap())
            })
            .collect(),
        other => panic!("expected Grains, got {other:?}"),
    }
}

fn seed(ex: &CalExecutor, facade: &AreevFacade) {
    // Three facts mention "tea" at different term frequencies, so BM25
    // separates them; "coffee" never matches.
    add(ex, facade, "tea", "note", "green tea with jasmine tea leaves");
    add(ex, facade, "alice", "likes", "tea");
    add(ex, facade, "bob", "likes", "tea and biscuits in the tea room");
    add(ex, facade, "carol", "likes", "coffee");
}

#[test]
fn about_recall_scores_are_normalized_and_non_increasing() {
    let (ex, facade, _d) = setup();
    seed(&ex, &facade);
    let hits = scored(&ex.execute(r#"RECALL facts ABOUT "tea""#, &facade).unwrap().result);
    assert!(hits.len() >= 2, "{hits:?}");
    assert_eq!(hits[0].1, 1.0, "top hit is exactly 1.0: {hits:?}");
    for w in hits.windows(2) {
        assert!(w[0].1 >= w[1].1, "non-increasing: {hits:?}");
    }
    assert!(hits.iter().all(|(_, s)| *s > 0.0 && *s <= 1.0), "{hits:?}");
    assert!(hits.last().unwrap().1 < 1.0, "scores are no longer a constant: {hits:?}");
}

#[test]
fn structural_recall_keeps_the_sentinel() {
    let (ex, facade, _d) = setup();
    seed(&ex, &facade);
    add(&ex, &facade, "alice", "likes", "scones");
    let hits = scored(&ex.execute(r#"RECALL facts WHERE subject = "alice""#, &facade).unwrap().result);
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|(_, s)| *s == 1.0), "no free-text leg → sentinel 1.0: {hits:?}");
}

#[test]
fn min_score_on_recall_filters_real_scores() {
    let (ex, facade, _d) = setup();
    seed(&ex, &facade);
    let all = scored(&ex.execute(r#"RECALL facts ABOUT "tea""#, &facade).unwrap().result);
    let floor = all.last().unwrap().1;
    assert!(floor < 1.0);
    // A floor just above the weakest hit's score drops it — and only it
    // (and anything tied with it).
    let q = format!(r#"RECALL facts ABOUT "tea" WITH min_score({})"#, floor + 1e-4);
    let out = ex.execute(&q, &facade).unwrap();
    let kept = scored(&out.result);
    assert_eq!(
        kept,
        all.iter().filter(|(_, s)| *s > floor).cloned().collect::<Vec<_>>(),
        "min_score drops exactly the hits below the floor"
    );
    assert!(kept.len() < all.len());
    assert!(out.warnings.iter().all(|w| !w.starts_with("CAL-W014")), "{:?}", out.warnings);

    // min_score(1.0) keeps only the top-scored hit(s).
    let top = scored(&ex.execute(r#"RECALL facts ABOUT "tea" WITH min_score(1.0)"#, &facade).unwrap().result);
    assert!(!top.is_empty() && top.iter().all(|(_, s)| *s == 1.0), "{top:?}");

    // No ABOUT: structural sentinel, exempt — the option is inert and says so.
    let out = ex
        .execute(r#"RECALL facts WHERE subject = "alice" WITH min_score(0.99)"#, &facade)
        .unwrap();
    assert_eq!(scored(&out.result).len(), 1);
    assert!(
        out.warnings.iter().any(|w| w.starts_with("CAL-W014") && w.contains("min_score")),
        "{:?}",
        out.warnings
    );
}

/// "biscuits" 5, everything else 1 — so the min-max floor is 0.0.
struct BiscuitRerank;
impl RerankBackend for BiscuitRerank {
    fn rerank(&self, _q: &str, docs: &[&str]) -> Result<Vec<f32>> {
        Ok(docs.iter().map(|d| if d.contains("biscuits") { 5.0 } else { 1.0 }).collect())
    }
}

#[test]
fn facade_set_reranker_supplies_the_scores() {
    let (ex, mut facade, _d) = setup();
    seed(&ex, &facade);
    facade.set_reranker(Box::new(BiscuitRerank));
    let hits = scored(&ex.execute(r#"RECALL facts ABOUT "tea" WITH rerank"#, &facade).unwrap().result);
    assert_eq!(hits[0].0, "tea and biscuits in the tea room", "{hits:?}");
    assert_eq!(hits[0].1, 1.0);
    assert!(hits[1..].iter().all(|(_, s)| *s == 0.0), "min-max: pool minimum is 0.0: {hits:?}");
    // Without WITH rerank the installed reranker does not run.
    let plain = scored(&ex.execute(r#"RECALL facts ABOUT "tea""#, &facade).unwrap().result);
    assert_ne!(plain, hits);
}

#[test]
fn facade_recall_deadline_is_threaded_into_hybrid_recall() {
    let (ex, mut facade, _d) = setup();
    seed(&ex, &facade);
    assert_eq!(facade.recall_deadline(), None);
    let q = r#"RECALL facts ABOUT "tea""#;
    assert!(!scored(&ex.execute(q, &facade).unwrap().result).is_empty());

    // A spent budget skips the free-text leg (fail-open: empty, not an error).
    facade.set_recall_deadline(Some(std::time::Duration::ZERO));
    let spent = ex.execute(q, &facade).unwrap();
    assert!(scored(&spent.result).is_empty(), "the deadline reached the store");
    assert!(spent.warnings.iter().all(|w| !w.contains("error")), "fail-open, not an error");

    facade.set_recall_deadline(None);
    assert!(!scored(&ex.execute(q, &facade).unwrap().result).is_empty());
}
