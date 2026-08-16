//! End-to-end: CAL `WITH` options → executor → AreevFacade → the store's
//! `recall_hybrid_tuned` Tier-1/Tier-2 refinements. The grammar for these
//! options; these tests prove they *execute*.

use areev_cal::executor::CalResultPayload;
use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
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

fn objects(payload: &CalResultPayload) -> Vec<String> {
    match payload {
        CalResultPayload::Grains { grains, .. } => grains
            .iter()
            .map(|g| {
                serde_json::to_value(g).unwrap()["fields"]["object"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            })
            .collect(),
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn cal_with_query_expansion_bridges_synonyms() {
    let (ex, facade, _d) = setup();
    add(&ex, &facade, "alice", "has", "mobile 5551234");

    // Plain LIKE "cell" finds nothing — stored vocabulary says "mobile".
    let plain = ex.execute(r#"RECALL facts LIKE "cell""#, &facade).unwrap();
    assert_eq!(objects(&plain.result).len(), 0);

    // WITH query_expansion → the engine also searches "mobile"/"phone".
    let expanded = ex
        .execute(r#"RECALL facts LIKE "cell" WITH query_expansion"#, &facade)
        .unwrap();
    let objs = objects(&expanded.result);
    assert_eq!(objs, vec!["mobile 5551234".to_string()]);
}

/// Stub cross-encoder: scores "urgent" docs far above the rest.
struct UrgentRerank;
impl RerankBackend for UrgentRerank {
    fn rerank(&self, _query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        Ok(docs
            .iter()
            .map(|d| if d.to_lowercase().contains("urgent") { 10.0 } else { 1.0 })
            .collect())
    }
}

#[test]
fn cal_with_rerank_reorders_via_installed_backend() {
    let (ex, facade, _d) = setup();
    // Install the host-supplied reranker on the store behind the facade.
    facade.with_store(|m| m.set_reranker(Box::new(UrgentRerank)));

    add(&ex, &facade, "proj", "task", "design homepage");
    add(&ex, &facade, "proj", "task", "write the docs");
    add(&ex, &facade, "proj", "task", "urgent fix login");
    add(&ex, &facade, "proj", "task", "small bug in footer");

    // "task" is in every fact's indexed text, so all four are candidates;
    // WITH rerank pulls the urgent one to the top.
    let out = ex
        .execute(r#"RECALL facts LIKE "task" WITH rerank"#, &facade)
        .unwrap();
    let objs = objects(&out.result);
    assert!(!objs.is_empty(), "rerank recall returned nothing");
    assert!(
        objs[0].contains("urgent"),
        "rerank should surface the urgent task first: {objs:?}"
    );
}

#[test]
fn cal_llm_dependent_options_error_honestly() {
    let (ex, facade, _d) = setup();
    // LLM-needing refinements are not silently ignored — they raise a clear,
    // actionable CAL-E116 pointing at the feature-request tracker.
    for opt in ["hyde", "llm_rerank"] {
        let cal = format!(r#"RECALL facts LIKE "anything" WITH {opt}"#);
        let err = ex.execute(&cal, &facade).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("CAL-E116"), "{opt} should be CAL-E116: {msg}");
        assert!(msg.contains(opt), "{opt} should name the option: {msg}");
        assert!(msg.contains("external LLM"), "{opt} should explain why: {msg}");
        assert!(msg.contains("feature request"), "{opt} should invite an FR: {msg}");
    }

    // The Tier-1/Tier-2 options remain available (no LLM) — sanity that we
    // only gated the LLM-dependent ones.
    assert!(ex.execute(r#"RECALL facts LIKE "x" WITH query_expansion"#, &facade).is_ok());
}
