//! Scored hybrid recall (`recall_hybrid_scored` / `recall_hybrid_scoped_scored`)
//! and the `CommandRerank` shell-out reranker. The scored call must return
//! EXACTLY the unscored call's order (it is the same body), the best hit must
//! score exactly 1.0, and fusion scores must be non-increasing; with a
//! reranker installed the scores are the reranker's, min-max normalized.

use areev_core::error::Result;
use areev_core::format::deserialize::DeserializedGrain;
use areev_core::types::{Fact, Grain};
use areev_store::{Areev, CommandRerank, RecallTuning, RerankBackend};
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str, ts: i64) -> Fact {
    let mut f = Fact::new(s, r, o).created_at(ts);
    f.common.namespace = Some(ns.to_string());
    f
}

fn hashes(g: &[DeserializedGrain]) -> Vec<areev_core::error::Hash> {
    g.iter().map(|g| g.hash).collect()
}

fn seed(m: &mut Areev) {
    let base = 1_700_000_000_000;
    m.add(&fact("work", "proj", "task", "design homepage", base)).unwrap();
    m.add(&fact("work", "proj", "task", "write the docs about the bug tracker", base + 1)).unwrap();
    m.add(&fact("work", "proj", "task", "urgent fix login", base + 2)).unwrap();
    m.add(&fact("work", "proj", "task", "small bug in footer", base + 3)).unwrap();
    m.add(&fact("work", "ops", "task", "bug triage rota", base + 4)).unwrap();
}

fn assert_fusion_shape(scores: &[f32]) {
    assert!(!scores.is_empty());
    assert_eq!(scores[0], 1.0, "best-fused hit scores exactly 1.0: {scores:?}");
    for w in scores.windows(2) {
        assert!(w[0] >= w[1], "fusion scores must be non-increasing: {scores:?}");
    }
    assert!(scores.iter().all(|s| *s > 0.0 && *s <= 1.0), "in (0,1]: {scores:?}");
}

#[test]
fn scored_recall_matches_unscored_order_and_normalizes() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    seed(&mut m);

    for (subject, query) in [(Some("proj"), Some("bug")), (None, Some("bug")), (Some("proj"), None)] {
        let plain = m
            .recall_hybrid_tuned("work", subject, None, query, 8, None, RecallTuning::default())
            .unwrap();
        let scored = m
            .recall_hybrid_scored("work", subject, None, query, 8, None, RecallTuning::default())
            .unwrap();
        let (grains, scores): (Vec<_>, Vec<_>) = scored.into_iter().unzip();
        assert_eq!(hashes(&plain), hashes(&grains), "order unchanged for {subject:?}/{query:?}");
        assert_fusion_shape(&scores);
    }

    // A hit found by two legs outranks one found by one — and the score says
    // so strictly (not a tie).
    let scored = m
        .recall_hybrid_scored("work", Some("proj"), None, Some("bug"), 8, None, RecallTuning::default())
        .unwrap();
    let last = scored.last().unwrap().1;
    assert!(last < 1.0, "a single-leg hit scores below the two-leg top: {last}");

    // The resolved-list path shares the body.
    let list = vec!["work".to_string()];
    let a = m
        .recall_hybrid_scoped(&list, None, None, Some("bug"), 8, None, RecallTuning::default())
        .unwrap();
    let b = m
        .recall_hybrid_scoped_scored(&list, None, None, Some("bug"), 8, None, RecallTuning::default())
        .unwrap();
    assert_eq!(hashes(&a), b.iter().map(|(g, _)| g.hash).collect::<Vec<_>>());
    assert_fusion_shape(&b.iter().map(|(_, s)| *s).collect::<Vec<_>>());
}

#[test]
fn scored_recall_empty_is_empty() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    seed(&mut m);
    let got = m
        .recall_hybrid_scored("work", None, None, Some("zanthrofel"), 8, None, RecallTuning::default())
        .unwrap();
    assert!(got.is_empty());
}

/// Raw scores on an arbitrary scale: "urgent" 10, "bug" 4, everything else -2.
struct ScaledRerank;
impl RerankBackend for ScaledRerank {
    fn rerank(&self, _q: &str, docs: &[&str]) -> Result<Vec<f32>> {
        Ok(docs
            .iter()
            .map(|d| if d.contains("urgent") { 10.0 } else if d.contains("bug") { 4.0 } else { -2.0 })
            .collect())
    }
}

/// All-equal scores: every hit normalizes to 1.0.
struct FlatRerank;
impl RerankBackend for FlatRerank {
    fn rerank(&self, _q: &str, docs: &[&str]) -> Result<Vec<f32>> {
        Ok(vec![3.5; docs.len()])
    }
}

/// Wrong length: fail open to fusion order AND fusion scores.
struct ShortRerank;
impl RerankBackend for ShortRerank {
    fn rerank(&self, _q: &str, _docs: &[&str]) -> Result<Vec<f32>> {
        Ok(vec![1.0])
    }
}

#[test]
fn reranker_scores_are_min_max_normalized() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    seed(&mut m);
    let tuning = RecallTuning { rerank: true, ..Default::default() };

    m.set_reranker(Box::new(ScaledRerank));
    let got = m
        .recall_hybrid_scored("work", Some("proj"), Some("task"), Some("bug"), 8, None, tuning)
        .unwrap();
    let by_obj: Vec<(String, f32)> =
        got.iter().map(|(g, s)| (g.get_str("object").unwrap().to_string(), *s)).collect();
    assert_eq!(by_obj[0].0, "urgent fix login", "{by_obj:?}");
    assert_eq!(by_obj[0].1, 1.0);
    // (4 - -2) / (10 - -2) = 0.5 for the "bug" docs; -2 → 0.0.
    for (o, s) in &by_obj {
        let want = if o.contains("urgent") { 1.0 } else if o.contains("bug") { 0.5 } else { 0.0 };
        assert!((s - want).abs() < 1e-6, "{o}: {s} != {want}");
    }
    // Same order as the unscored call under the same tuning.
    let plain = m
        .recall_hybrid_tuned("work", Some("proj"), Some("task"), Some("bug"), 8, None, tuning)
        .unwrap();
    assert_eq!(hashes(&plain), got.iter().map(|(g, _)| g.hash).collect::<Vec<_>>());

    m.set_reranker(Box::new(FlatRerank));
    let flat = m
        .recall_hybrid_scored("work", Some("proj"), Some("task"), Some("bug"), 8, None, tuning)
        .unwrap();
    assert!(flat.iter().all(|(_, s)| *s == 1.0), "all-equal pool → 1.0");

    m.set_reranker(Box::new(ShortRerank));
    let fallback = m
        .recall_hybrid_scored("work", Some("proj"), Some("task"), Some("bug"), 8, None, tuning)
        .unwrap();
    let fused = m
        .recall_hybrid_scored("work", Some("proj"), Some("task"), Some("bug"), 8, None, RecallTuning::default())
        .unwrap();
    assert_eq!(
        fallback.iter().map(|(g, s)| (g.hash, *s)).collect::<Vec<_>>(),
        fused.iter().map(|(g, s)| (g.hash, *s)).collect::<Vec<_>>(),
        "a failed reranker falls back to fusion order and fusion scores"
    );
}

// ---------------------------------------------------------------------------
// CommandRerank — mirrors command_embed_tests.rs: a tiny Python script, skip
// (with a note) when no Python is on PATH.
// ---------------------------------------------------------------------------

fn find_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|c| {
        std::process::Command::new(c)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Scores each doc by how many query words it contains.
const RERANK_PY: &str = r#"
import sys, json
req = json.load(sys.stdin)
q = set(req["query"].lower().split())
print(json.dumps([float(len(q & set(d.lower().split()))) for d in req["docs"]]))
"#;

/// Always one score, whatever it was sent.
const SHORT_PY: &str = r#"
import sys, json
json.load(sys.stdin)
print("[1.0]")
"#;

#[test]
fn command_rerank_round_trips_and_powers_recall() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("rerank.py");
    std::fs::write(&script, RERANK_PY).unwrap();
    let cr = CommandRerank::new(&format!("{py} {}", script.display()), Some("overlap-toy")).unwrap();
    assert_eq!(cr.model(), "overlap-toy");
    let raw = cr.rerank("fix login", &["urgent fix login", "design homepage"]).unwrap();
    assert_eq!(raw, vec![2.0, 0.0]);
    assert!(cr.rerank("q", &[]).unwrap().is_empty());

    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    seed(&mut m);
    m.set_reranker(Box::new(cr));
    let got = m
        .recall_hybrid_scored(
            "work",
            Some("proj"),
            Some("task"),
            Some("fix login"),
            8,
            None,
            RecallTuning { rerank: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!(got[0].0.get_str("object"), Some("urgent fix login"));
    assert_eq!(got[0].1, 1.0);
    assert!(got[1..].iter().all(|(_, s)| *s == 0.0), "no overlap → pool minimum");
}

#[test]
fn command_rerank_rejects_wrong_length_and_broken_commands() {
    assert!(CommandRerank::new("", None).is_err());
    assert!(CommandRerank::new("   ", None).is_err());
    // No setup probe: construction succeeds, the call fails.
    let missing = CommandRerank::new("definitely-not-a-real-binary-xyz", None).unwrap();
    assert!(missing.rerank("q", &["a"]).is_err());

    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("short.py");
    std::fs::write(&script, SHORT_PY).unwrap();
    let cr = CommandRerank::new(&format!("{py} {}", script.display()), None).unwrap();
    let err = cr.rerank("q", &["a", "b"]).unwrap_err();
    assert!(err.to_string().contains("returned 1 scores, expected 2"), "{err}");
    assert!(err.to_string().starts_with("VAL-"), "a wrong length is a Validation error: {err}");
}
