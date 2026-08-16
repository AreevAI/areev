//! M4 vector leg + multilingual recall (English / Arabic / Mandarin).
//!
//! The test embedder is a deterministic character-trigram bag — crude but
//! script-agnostic (no tokenization), so it exercises exactly what matters:
//! UTF-8 flows unmangled, CJK works without whitespace, and same-language
//! content lands nearest. Real deployments plug a multilingual model
//! (bge-m3 / multilingual-e5) into the same trait.

use areev_core::types::{Event, Fact, Grain};
use areev_store::{EmbedBackend, Areev};
use tempfile::TempDir;

struct TrigramEmbed;
impl EmbedBackend for TrigramEmbed {
    fn dim(&self) -> usize {
        64
    }
    fn embed(&self, text: &str) -> areev_core::error::Result<Vec<f32>> {
        let chars: Vec<char> = text.chars().collect();
        let mut v = vec![0f32; 64];
        for w in chars.windows(3) {
            let mut h: u64 = 1469598103934665603;
            for c in w {
                h ^= *c as u64;
                h = h.wrapping_mul(1099511628211);
            }
            v[(h % 64) as usize] += 1.0;
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        Ok(v.into_iter().map(|x| x / n).collect())
    }
}

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

fn seed(m: &mut Areev) {
    m.add(&fact("caller", "john", "prefers", "green tea in the morning")).unwrap();
    m.add(&fact("caller", "جون", "يفضل", "الشاي الأخضر في الصباح")).unwrap();
    let mut e = Event::new("约翰早上喜欢喝绿茶，还问了花生过敏的政策");
    e.common.namespace = Some("caller".to_string());
    e.session_id = Some("call-9".to_string());
    m.add(&e).unwrap();
}

#[test]
fn structural_recall_is_script_agnostic() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    seed(&mut m);
    // exact-term structural recall on the Arabic subject
    let got = m.recall("caller", "جون", None, 8).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].get_str("object"), Some("الشاي الأخضر في الصباح"));
    // roundtrip integrity across scripts
    assert_eq!(m.verify().unwrap().hash_mismatches, 0);
}

#[test]
fn vector_leg_serves_each_language() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    m.set_embedder(Box::new(TrigramEmbed));
    seed(&mut m);

    // Mandarin query → Mandarin event is the top hit (no whitespace
    // tokenization involved — this is the leg BM25 can't cover for CJK)
    let hits = m.recall_hybrid("caller", None, None, Some("绿茶 花生过敏"), 3, None).unwrap();
    assert!(!hits.is_empty());
    assert!(hits[0].get_str("content").is_some_and(|c| c.contains("绿茶")), "{:?}", hits[0].fields);

    // Arabic query → Arabic fact ranks first
    let hits = m.recall_hybrid("caller", None, None, Some("الشاي الأخضر"), 3, None).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].get_str("subject"), Some("جون"));

    // English query → English fact ranks first
    let hits = m.recall_hybrid("caller", None, None, Some("green tea morning"), 3, None).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].get_str("subject"), Some("john"));
}

#[test]
fn forget_removes_embedding_row() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    m.set_embedder(Box::new(TrigramEmbed));
    let h = m.add(&fact("caller", "john", "prefers", "unique-zanzibar-token")).unwrap();
    assert!(!m.search_vector("caller", "unique-zanzibar-token", 4).unwrap().is_empty());
    m.forget(&h).unwrap();
    let hits = m.recall_hybrid("caller", None, None, Some("unique-zanzibar-token"), 4, None).unwrap();
    assert!(hits.is_empty(), "forgotten grain must leave the vector leg");
}

#[test]
fn nearest_semantic_ranks_paraphrase_and_needs_embedder() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    m.set_embedder(Box::new(TrigramEmbed));
    // Two lessons under the same (subject, relation); only the first is about
    // tempdir isolation.
    m.add(&fact("agent", "flaky", "lesson", "isolate the shared tempdir per test"))
        .unwrap();
    m.add(&fact("agent", "flaky", "lesson", "prefer table-driven cases for coverage"))
        .unwrap();

    // A paraphrase of the first lesson must rank it first, above the unrelated one.
    let hits = m
        .nearest_semantic(
            "agent",
            Some("flaky"),
            Some("lesson"),
            "isolate the tempdir for each test",
            5,
        )
        .unwrap();
    assert!(!hits.is_empty(), "advise mode returns near neighbours");
    let top = m.get(&hits[0].0).unwrap();
    assert_eq!(
        top.get_str("object"),
        Some("isolate the shared tempdir per test"),
        "the paraphrase's nearest neighbour is the tempdir lesson"
    );
    if hits.len() > 1 {
        assert!(hits[0].1 >= hits[1].1, "results are sorted by similarity desc");
    }

    // Novelty is a vector op: without an embedder it errors loudly rather than
    // silently returning nothing.
    let mut bare = Areev::open(d.path().join("bare.db").to_str().unwrap()).unwrap();
    assert!(bare
        .nearest_semantic("agent", None, None, "anything", 3)
        .is_err());
}

/// Regression: relation-only scoping fell through the match to the unscoped
/// arm, so `nearest_semantic(ns, None, Some(rel), …)` silently ignored the
/// filter and failed OPEN — neighbours from every relation, no error. A
/// filter test must assert what the scope EXCLUDES, or it passes against a
/// filter that is dropped entirely.
#[test]
fn nearest_semantic_relation_only_scope_is_applied() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    m.set_embedder(Box::new(TrigramEmbed));
    m.add(&fact("agent", "a", "lesson", "tempdir isolation matters")).unwrap();
    m.add(&fact("agent", "b", "lesson", "tempdir cleanup matters")).unwrap();
    m.add(&fact("agent", "c", "note", "tempdir trivia unrelated")).unwrap();

    let hits = m
        .nearest_semantic("agent", None, Some("lesson"), "tempdir", 10)
        .unwrap();
    assert_eq!(hits.len(), 2, "the 'note' grain must be excluded, not ranked");
    for (h, _) in &hits {
        let g = m.get(h).unwrap();
        assert_eq!(g.get_str("relation"), Some("lesson"));
    }
}
