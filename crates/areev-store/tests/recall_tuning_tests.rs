//! Tier-1 (query expansion, MMR diversity) and Tier-2 (cross-encoder rerank)
//! recall refinements on `recall_hybrid_tuned`. Each stage is opt-in and
//! fail-open; these tests pin the reorder behavior with deterministic,
//! host-supplied backends (no real ML model needed).

use areev_core::error::Result;
use areev_core::types::{Fact, Grain};
use areev_store::{
    Areev, AreevOptions, EmbedBackend, EnglishExpander, QueryExpander, RecallTuning,
    RerankBackend,
};
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

fn objects(grains: &[areev_core::format::deserialize::DeserializedGrain]) -> Vec<String> {
    grains
        .iter()
        .map(|g| g.get_str("object").unwrap_or("").to_string())
        .collect()
}

fn pos(grains: &[areev_core::format::deserialize::DeserializedGrain], needle: &str) -> Option<usize> {
    grains
        .iter()
        .position(|g| g.get_str("object").unwrap_or("").contains(needle))
}

// ---------------------------------------------------------------------------
// Tier-1: rule-based query expansion (no embedder — the edge/BM25-only profile)
// ---------------------------------------------------------------------------

#[test]
fn query_expansion_bridges_a_synonym_gap() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    // Stored vocabulary says "mobile"; the caller will ask for "cell".
    m.add(&fact("caller", "alice", "has", "mobile 5551234")).unwrap();

    // Without expansion: BM25 on "cell" finds nothing (no vector leg installed).
    let plain = m
        .recall_hybrid_tuned("caller", None, Some("has"), Some("cell"), 8, None, RecallTuning::default())
        .unwrap();
    assert!(plain.is_empty(), "‘cell’ must not match ‘mobile’ without expansion");

    // With expansion: the EnglishExpander emits "mobile"/"phone" variants,
    // whose BM25 legs match — RRF-fused into the result.
    let expanded = m
        .recall_hybrid_tuned(
            "caller",
            None,
            Some("has"),
            Some("cell"),
            8,
            None,
            RecallTuning { query_expansion: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!(expanded.len(), 1, "expansion should surface the ‘mobile’ fact");
    assert_eq!(expanded[0].get_str("object"), Some("mobile 5551234"));
}

#[test]
fn english_expander_produces_bounded_variants() {
    let ex = EnglishExpander::default();
    let v = ex.expand("cell");
    assert!(v.contains(&"mobile".to_string()));
    assert!(v.contains(&"phone".to_string()));
    assert!(!v.contains(&"cell".to_string()), "variants exclude the original");
    assert!(v.len() <= 4, "default cap is 4 variants");
    // Multi-word: synonym substitution keeps the other tokens in place.
    let v2 = ex.expand("my cell number");
    assert!(v2.iter().any(|s| s == "my mobile number"));
    // No synonyms/stems → no variants.
    assert!(ex.expand("xyzzy").is_empty());
}

#[test]
fn custom_query_expander_overrides_the_builtin() {
    struct FixedExpander;
    impl QueryExpander for FixedExpander {
        fn expand(&self, _q: &str) -> Vec<String> {
            vec!["widget".to_string()]
        }
    }
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    m.set_query_expander(Box::new(FixedExpander));
    m.add(&fact("s", "acme", "sells", "widget assortment")).unwrap();

    let got = m
        .recall_hybrid_tuned(
            "s",
            None,
            Some("sells"),
            Some("gizmo"), // no built-in synonyms; custom expander maps to "widget"
            8,
            None,
            RecallTuning { query_expansion: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].get_str("object"), Some("widget assortment"));
}

// ---------------------------------------------------------------------------
// Tier-1: MMR diversity reranking (needs an embedder + stored vectors)
// ---------------------------------------------------------------------------

/// Deterministic 3-d embedder keyed on topic words so cosine geometry is
/// under test control: coffee≈espresso (near-duplicates), tea partially
/// related to a coffee query.
struct KeyedEmbed;
impl EmbedBackend for KeyedEmbed {
    fn dim(&self) -> usize {
        3
    }
    fn model(&self) -> &str {
        "keyed-test"
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let t = text.to_lowercase();
        let v: [f32; 3] = if t.contains("espresso") {
            [0.96, 0.28, 0.0]
        } else if t.contains("coffee") {
            [1.0, 0.0, 0.0]
        } else if t.contains("tea") {
            [0.6, 0.8, 0.0]
        } else {
            [0.0, 0.0, 1.0]
        };
        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-9);
        Ok(vec![v[0] / n, v[1] / n, v[2] / n])
    }
}

#[test]
fn mmr_promotes_a_diverse_result_over_a_near_duplicate() {
    let d = TempDir::new().unwrap();
    // BM25 off so the ranking is purely the vector leg — isolates MMR.
    let mut m = Areev::open_with(
        d.path().join("m.db").to_str().unwrap(),
        AreevOptions { index_text: false, ..AreevOptions::default() },
    )
    .unwrap();
    m.set_embedder(Box::new(KeyedEmbed));
    m.add(&fact("caller", "alice", "likes", "coffee")).unwrap();
    m.add(&fact("caller", "alice", "likes", "espresso drinks")).unwrap();
    m.add(&fact("caller", "alice", "likes", "green tea")).unwrap();

    // Plain fusion = pure relevance to a "coffee" query:
    // coffee (1.00) > espresso (0.96) > tea (0.60).
    let plain = m
        .recall_hybrid_tuned("caller", None, Some("likes"), Some("coffee"), 3, None, RecallTuning::default())
        .unwrap();
    let (p_esp, p_tea) = (pos(&plain, "espresso").unwrap(), pos(&plain, "tea").unwrap());
    assert!(p_esp < p_tea, "without MMR the near-duplicate outranks tea: {:?}", objects(&plain));

    // MMR with a diversity-leaning lambda: after picking coffee, tea (distinct)
    // beats espresso (a near-duplicate of coffee) despite lower raw relevance.
    let diverse = m
        .recall_hybrid_tuned(
            "caller",
            None,
            Some("likes"),
            Some("coffee"),
            3,
            None,
            RecallTuning { diversity_lambda: Some(0.3), ..Default::default() },
        )
        .unwrap();
    assert_eq!(pos(&diverse, "coffee"), Some(0), "most relevant stays first");
    let (d_esp, d_tea) = (pos(&diverse, "espresso").unwrap(), pos(&diverse, "tea").unwrap());
    assert!(d_tea < d_esp, "MMR must promote the diverse result: {:?}", objects(&diverse));
}

#[test]
fn mmr_without_embedder_is_a_noop() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    m.add(&fact("caller", "alice", "likes", "coffee")).unwrap();
    // Diversity requested but no embedder installed → falls back to fusion order.
    let got = m
        .recall_hybrid_tuned(
            "caller",
            None,
            Some("likes"),
            Some("coffee"),
            3,
            None,
            RecallTuning { diversity_lambda: Some(0.3), ..Default::default() },
        )
        .unwrap();
    assert_eq!(got.len(), 1);
}

// ---------------------------------------------------------------------------
// Tier-2: cross-encoder rerank (host-supplied RerankBackend seam)
// ---------------------------------------------------------------------------

/// Stub cross-encoder: scores any doc containing "urgent" far above the rest.
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
fn rerank_reorders_the_candidate_pool() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    // Add order matters for the structural leg (newest = rank 0). The "bug"
    // doc is added last so BM25("bug") + structural put it first in fusion.
    m.add(&fact("work", "proj", "task", "design homepage")).unwrap();
    m.add(&fact("work", "proj", "task", "write the docs")).unwrap();
    m.add(&fact("work", "proj", "task", "urgent fix login")).unwrap(); // no query term
    m.add(&fact("work", "proj", "task", "small bug in footer")).unwrap(); // query term

    // No rerank: fusion puts the query-matching "bug" doc first, not "urgent".
    let plain = m
        .recall_hybrid_tuned("work", Some("proj"), Some("task"), Some("bug"), 4, None, RecallTuning::default())
        .unwrap();
    assert_ne!(pos(&plain, "urgent"), Some(0), "urgent is not first without rerank: {:?}", objects(&plain));

    // With a reranker installed + requested: "urgent" is pulled to the top.
    m.set_reranker(Box::new(UrgentRerank));
    assert!(m.has_reranker());
    let reranked = m
        .recall_hybrid_tuned(
            "work",
            Some("proj"),
            Some("task"),
            Some("bug"),
            4,
            None,
            RecallTuning { rerank: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!(pos(&reranked, "urgent"), Some(0), "rerank must surface urgent: {:?}", objects(&reranked));
}

#[test]
fn rerank_requested_without_backend_keeps_fusion_order() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    m.add(&fact("work", "proj", "task", "small bug in footer")).unwrap();
    m.add(&fact("work", "proj", "task", "urgent fix login")).unwrap();
    // rerank=true but no reranker installed → no-op, still returns results.
    let got = m
        .recall_hybrid_tuned(
            "work",
            Some("proj"),
            Some("task"),
            Some("bug"),
            4,
            None,
            RecallTuning { rerank: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!(got.len(), 2);
}

// ---------------------------------------------------------------------------
// include_superseded: widen every leg from the heads to the whole chain
// ---------------------------------------------------------------------------

/// tea → coffee → matcha, one (subject, relation) cell, two supersessions.
fn chain(m: &mut Areev) -> Result<()> {
    let h1 = m.add(&fact("caller", "kim", "prefers", "tea leaves"))?;
    let mut g2 = fact("caller", "kim", "prefers", "coffee beans");
    let h2 = m.supersede(&h1, &mut g2)?;
    let mut g3 = fact("caller", "kim", "prefers", "matcha powder");
    m.supersede(&h2, &mut g3)?;
    Ok(())
}

#[test]
fn structural_leg_widens_to_the_whole_chain() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    chain(&mut m).unwrap();

    // Default: the head only. Stale values must not reach a model's context
    // unless the caller asked for history.
    let heads = m
        .recall_hybrid_tuned("caller", Some("kim"), Some("prefers"), None, 8, None, RecallTuning::default())
        .unwrap();
    assert_eq!(objects(&heads), vec!["matcha powder"]);

    let all = m
        .recall_hybrid_tuned(
            "caller",
            Some("kim"),
            Some("prefers"),
            None,
            8,
            None,
            RecallTuning { include_superseded: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!(all.len(), 3, "the whole chain: {:?}", objects(&all));
    assert!(pos(&all, "tea").is_some(), "the oldest version must be reachable");
}

#[test]
fn bm25_leg_finds_text_that_only_exists_in_history() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    chain(&mut m).unwrap();

    // "tea" survives only in a superseded version. Heads-only recall cannot see
    // it at all — that word is unfindable, which is the gap this flag closes.
    assert!(m.search_text("caller", "tea", 8).unwrap().is_empty());
    assert_eq!(m.search_text_all("caller", "tea", 8).unwrap().len(), 1);

    let all = m
        .recall_hybrid_tuned(
            "caller",
            None,
            None,
            Some("tea"),
            8,
            None,
            RecallTuning { include_superseded: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!(objects(&all), vec!["tea leaves"]);
}

#[test]
fn forgotten_grains_do_not_come_back_through_the_widened_scan() {
    // The safety property behind relaxing `cur=1`: supersede only flips index
    // state, but forget DELETEs the rows outright. A tombstone must stay a
    // tombstone on every path, including the one that asks for history.
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    let h1 = m.add(&fact("caller", "kim", "prefers", "tea leaves")).unwrap();
    let mut g2 = fact("caller", "kim", "prefers", "coffee beans");
    m.supersede(&h1, &mut g2).unwrap();
    m.forget(&h1).unwrap();

    let widened = RecallTuning { include_superseded: true, ..Default::default() };
    let structural = m
        .recall_hybrid_tuned("caller", Some("kim"), Some("prefers"), None, 8, None, widened)
        .unwrap();
    assert_eq!(objects(&structural), vec!["coffee beans"], "forgotten grain resurfaced");
    let text = m
        .recall_hybrid_tuned("caller", None, None, Some("tea"), 8, None, widened)
        .unwrap();
    assert!(text.is_empty(), "forgotten text stayed findable: {:?}", objects(&text));
}

#[test]
fn supersession_map_labels_only_the_stale_versions() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    chain(&mut m).unwrap();

    let all = m
        .recall_hybrid_tuned(
            "caller",
            Some("kim"),
            Some("prefers"),
            None,
            8,
            None,
            RecallTuning { include_superseded: true, ..Default::default() },
        )
        .unwrap();
    let hashes: Vec<_> = all.iter().map(|g| g.hash).collect();
    let map = m.supersession_map(&hashes).unwrap();
    assert_eq!(map.len(), 2, "both stale versions carry a superseder");

    // The head is absent from the map, and every superseder is itself a grain
    // in the chain — the labels reconstruct the lineage, not just a flag.
    let head = all.iter().find(|g| g.get_str("object") == Some("matcha powder")).unwrap();
    assert!(!map.contains_key(&head.hash));
    for sup in map.values() {
        assert!(hashes.contains(sup), "superseder must be a real grain");
    }
}
