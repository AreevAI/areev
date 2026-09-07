//! The external-embedding seam: caller-supplied vectors in, vector search out.
//!
//! Hosts that own their embedding pipeline (a framework adapter — CrewAI hands
//! its storage backend a pre-computed `query_embedding`) must search with
//! *their* model's vectors; re-embedding query text with a different local
//! model would compare vectors from two spaces and return silently wrong
//! similarity. `add_with_embedding` / `set_grain_embedding` are the write
//! half, `nearest_vector` the read half — no embedder installed anywhere in
//! this file, deliberately.

use areev_core::types::*;
use areev_store::Areev;
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let d = TempDir::new().unwrap();
    let m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    (m, d)
}

fn fact(s: &str, r: &str, o: &str, at: i64) -> Fact {
    Fact::new(s, r, o).created_at(at).namespace("kb")
}

#[test]
fn nearest_vector_ranks_by_cosine_without_an_embedder() {
    let (mut m, _d) = open_mem();
    let a = m
        .add_with_embedding(&fact("cats", "are", "mammals", 1), &[1.0, 0.0, 0.0])
        .unwrap();
    let b = m
        .add_with_embedding(&fact("rust", "is", "compiled", 2), &[0.0, 1.0, 0.0])
        .unwrap();
    let c = m
        .add_with_embedding(&fact("dogs", "are", "mammals", 3), &[0.9, 0.1, 0.0])
        .unwrap();

    let hits = m.nearest_vector("kb", None, None, &[1.0, 0.0, 0.0], 3).unwrap();
    let order: Vec<_> = hits.iter().map(|(h, _)| *h).collect();
    assert_eq!(order[0], a, "exact match first");
    assert_eq!(order[1], c, "near vector second");
    assert_eq!(order[2], b, "orthogonal vector last");
    assert!(hits[0].1 > hits[1].1 && hits[1].1 > hits[2].1);

    // (subject, relation) scoping composes: only the mammal facts remain.
    let scoped = m
        .nearest_vector("kb", None, Some("are"), &[1.0, 0.0, 0.0], 5)
        .unwrap();
    assert_eq!(scoped.len(), 2);
    assert!(scoped.iter().all(|(h, _)| *h == a || *h == c));
}

/// The first external vector stamps the file's embedding provenance, and from
/// then on a wrong-dimension vector is refused — on write and on read.
/// Asserting what a mismatch EXCLUDES: it errors, it never scores garbage.
#[test]
fn dimension_is_declared_once_and_enforced_everywhere() {
    let (mut m, _d) = open_mem();
    assert_eq!(m.declared_embedding(), None);
    m.add_with_embedding(&fact("a", "b", "c", 1), &[0.1, 0.2, 0.3])
        .unwrap();
    assert_eq!(m.declared_embedding(), Some(("external", 3)));

    let write_err = m
        .add_with_embedding(&fact("d", "e", "f", 2), &[0.1, 0.2])
        .unwrap_err();
    assert!(write_err.to_string().contains("dimension mismatch"), "{write_err}");

    let read_err = m
        .nearest_vector("kb", None, None, &[0.1, 0.2, 0.3, 0.4], 3)
        .unwrap_err();
    assert!(read_err.to_string().contains("dimension mismatch"), "{read_err}");

    let empty_err = m.nearest_vector("kb", None, None, &[], 3).unwrap_err();
    assert!(empty_err.to_string().contains("non-empty"), "{empty_err}");
}

#[test]
fn set_grain_embedding_backfills_replaces_and_rejects_unknown_hashes() {
    let (mut m, _d) = open_mem();
    // Backfill: the grain was added without a vector.
    let h = m.add(&fact("late", "gets", "vector", 1)).unwrap();
    assert!(m.nearest_vector("kb", None, None, &[1.0, 0.0], 3).unwrap().is_empty());
    m.set_grain_embedding(&h, &[1.0, 0.0]).unwrap();
    let hits = m.nearest_vector("kb", None, None, &[1.0, 0.0], 3).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].1 > 0.99);

    // Replace: the newest vector wins (DELETE + INSERT, not append).
    m.set_grain_embedding(&h, &[0.0, 1.0]).unwrap();
    let hits = m.nearest_vector("kb", None, None, &[1.0, 0.0], 3).unwrap();
    assert_eq!(hits.len(), 1, "one row per grain, not two");
    assert!(hits[0].1 < 0.01, "the old vector must be gone");

    // Unknown hash: a loud error, not a silent no-op.
    let missing = areev_core::error::Hash::from_hex(&"0".repeat(64)).unwrap();
    assert!(m.set_grain_embedding(&missing, &[1.0, 0.0]).is_err());
}

/// Vector search answers from current heads only — a superseded grain's
/// vector must not resurface (`svt IS NULL`, same rule as text recall).
#[test]
fn superseded_grains_leave_vector_search() {
    let (mut m, _d) = open_mem();
    let old = m
        .add_with_embedding(&fact("policy", "window", "14 days", 1), &[1.0, 0.0])
        .unwrap();
    let mut newer = fact("policy", "window", "30 days", 2);
    let new_hash = m.supersede(&old, &mut newer).unwrap();
    m.set_grain_embedding(&new_hash, &[0.9, 0.1]).unwrap();

    let hits = m.nearest_vector("kb", None, None, &[1.0, 0.0], 5).unwrap();
    let hashes: Vec<_> = hits.iter().map(|(h, _)| *h).collect();
    assert!(!hashes.contains(&old), "superseded vector resurfaced");
    assert_eq!(hashes, vec![new_hash]);
}

/// `nearest_vector` used to `require_exact_ns`, which made it and
/// `nearest_semantic` the only plural reads that refused a `"org.*"` scope —
/// `search_text` and `search_vector` have always accepted one. The cost was
/// not a missing convenience: a corpus-wide semantic search had no way to
/// spell itself except by falling through to prefix-scoped `recall_hybrid`,
/// paying a BM25 leg and a structural leg to answer a purely vector question.
///
/// The scope has to be exercised for what it EXCLUDES as well as what it
/// includes — a scope that silently matched everything would pass a
/// containment-only assertion while failing open, which is the same bug the
/// relation-only arm had.
#[test]
fn nearest_vector_accepts_a_prefix_scope() {
    let (mut m, _d) = open_mem();
    let mk = |s: &str, ns: &str, at: i64| Fact::new(s, "is", "x").created_at(at).namespace(ns);

    let health = m
        .add_with_embedding(&mk("h1", "deal.health.1", 1), &[1.0, 0.0, 0.0])
        .unwrap();
    let health2 = m
        .add_with_embedding(&mk("h2", "deal.health.2", 2), &[0.98, 0.02, 0.0])
        .unwrap();
    let indus = m
        .add_with_embedding(&mk("i1", "deal.industrial.1", 3), &[0.96, 0.04, 0.0])
        .unwrap();
    // Deliberately the CLOSEST vector of all, in a namespace no scope below
    // selects: if scoping failed open, this would rank first every time.
    let outside = m
        .add_with_embedding(&mk("o1", "other.1", 4), &[1.0, 0.0, 0.0])
        .unwrap();

    // Exact namespace: unchanged behaviour, one namespace only.
    let hits = m.nearest_vector("deal.health.1", None, None, &[1.0, 0.0, 0.0], 10).unwrap();
    let got: Vec<_> = hits.iter().map(|(h, _)| *h).collect();
    assert_eq!(got, vec![health], "exact scope stays exact");

    // Sub-tree scope: both health deals, neither the industrial one nor the
    // nearer outsider.
    let hits = m.nearest_vector("deal.health.*", None, None, &[1.0, 0.0, 0.0], 10).unwrap();
    let got: Vec<_> = hits.iter().map(|(h, _)| *h).collect();
    assert_eq!(got.len(), 2, "sector scope selects its own sub-tree: {got:?}");
    assert!(got.contains(&health) && got.contains(&health2));
    assert!(!got.contains(&indus), "a sibling sector must not leak in");
    assert!(!got.contains(&outside), "the nearest vector outside the scope must stay out");

    // Whole-tree scope: every deal, still not the outsider.
    let hits = m.nearest_vector("deal.*", None, None, &[1.0, 0.0, 0.0], 10).unwrap();
    let got: Vec<_> = hits.iter().map(|(h, _)| *h).collect();
    assert_eq!(got.len(), 3, "tree scope selects every descendant: {got:?}");
    assert!(!got.contains(&outside), "scoping must not fail open to the whole file");
    assert_eq!(got[0], health, "ranking is still by cosine within the scope");

    // A prefix scope matching nothing is empty, not everything.
    let hits = m.nearest_vector("nosuch.*", None, None, &[1.0, 0.0, 0.0], 10).unwrap();
    assert!(hits.is_empty(), "an unmatched scope returns nothing, never the whole file");
}

/// The subject filter has to keep working *through* a prefix scope — that arm
/// takes a different SQL path (inlined namespace ids, shifted parameter
/// numbering) than the single-namespace fast path, so it needs its own pin.
#[test]
fn a_prefix_scope_still_honours_the_subject_filter() {
    let (mut m, _d) = open_mem();
    let mk = |s: &str, ns: &str, at: i64| Fact::new(s, "is", "x").created_at(at).namespace(ns);
    let want = m.add_with_embedding(&mk("acme", "deal.a", 1), &[1.0, 0.0, 0.0]).unwrap();
    let _other_subject =
        m.add_with_embedding(&mk("beta", "deal.b", 2), &[1.0, 0.0, 0.0]).unwrap();

    let hits = m.nearest_vector("deal.*", Some("acme"), None, &[1.0, 0.0, 0.0], 10).unwrap();
    let got: Vec<_> = hits.iter().map(|(h, _)| *h).collect();
    assert_eq!(got, vec![want], "subject filter applies across the scope, not just within one ns");

    let none = m.nearest_vector("deal.*", Some("ghost"), None, &[1.0, 0.0, 0.0], 10).unwrap();
    assert!(none.is_empty(), "an uninterned subject short-circuits instead of scanning");
}

/// #141: the bulk write lands many vectors in one transaction, across more
/// than one statement chunk, and reads back exactly like the per-grain path.
#[test]
fn set_grain_embeddings_writes_a_multi_chunk_batch_atomically() {
    let (mut m, _d) = open_mem();
    let n = areev_store::EMBEDDING_BATCH_CHUNK + 50; // forces a second chunk
    let mut items = Vec::with_capacity(n);
    for i in 0..n {
        let h = m.add(&fact(&format!("g{i}"), "is", "a grain", 100 + i as i64)).unwrap();
        // Distinct directions in a 4-dim space so nearest() can tell them apart.
        let a = i as f32;
        items.push((h, vec![a.cos(), a.sin(), (a * 0.5).cos(), (a * 0.5).sin()]));
    }
    assert_eq!(m.set_grain_embeddings(&items).unwrap(), n);
    assert_eq!(m.declared_embedding(), Some(("external", 4)));
    // Every vector is its own nearest neighbour.
    for (h, v) in items.iter().step_by(37) {
        let near = m.nearest_vector("kb", None, None, v, 1).unwrap();
        assert_eq!(near[0].0, *h);
        assert!(near[0].1 > 0.999, "{}", near[0].1);
    }

    // A repeated hash takes its LAST vector.
    let (h0, _) = items[0].clone();
    let twice = vec![(h0, vec![1.0, 0.0, 0.0, 0.0]), (h0, vec![0.0, 1.0, 0.0, 0.0])];
    assert_eq!(m.set_grain_embeddings(&twice).unwrap(), 1);
    let near = m.nearest_vector("kb", None, None, &[0.0, 1.0, 0.0, 0.0], 1).unwrap();
    assert_eq!(near[0].0, h0);

    // An empty batch is a no-op, not an error.
    assert_eq!(m.set_grain_embeddings(&[]).unwrap(), 0);
}

/// One bad entry refuses the WHOLE batch before anything is written —
/// whether the fault is a dimension or an unknown address.
#[test]
fn set_grain_embeddings_is_all_or_nothing() {
    let (mut m, _d) = open_mem();
    let a = m.add(&fact("a", "is", "x", 1)).unwrap();
    let b = m.add(&fact("b", "is", "y", 2)).unwrap();
    let missing = areev_core::error::Hash::from_hex(&"9".repeat(64)).unwrap();

    let err = m
        .set_grain_embeddings(&[(a, vec![1.0, 0.0]), (b, vec![0.0, 1.0, 0.0])])
        .unwrap_err();
    assert!(err.to_string().contains("dimensions"), "{err}");
    assert!(m.nearest_vector("kb", None, None, &[1.0, 0.0], 5).unwrap().is_empty(), "nothing written");
    assert!(m.declared_embedding().is_none(), "and no provenance stamped");

    let err = m
        .set_grain_embeddings(&[(a, vec![1.0, 0.0]), (missing, vec![0.0, 1.0])])
        .unwrap_err();
    assert!(err.to_string().contains("MEM-E") || err.to_string().contains("not found"), "{err}");
    assert!(m.nearest_vector("kb", None, None, &[1.0, 0.0], 5).unwrap().is_empty(), "nothing written");
}

/// The embedded engine has no ANN index, so its reads are exact by
/// construction: the check says so (`index: None`, recall 1.0) and runs no
/// queries, rather than pretending to grade an index that is not there.
#[test]
fn vector_recall_check_is_trivially_exact_without_an_index() {
    let (mut m, _d) = open_mem();
    let a = m.add_with_embedding(&fact("a", "is", "x", 1), &[1.0, 0.0]).unwrap();
    let _ = a;
    let report = m.vector_recall_check("kb", &[vec![1.0, 0.0], vec![0.0, 1.0]], 3).unwrap();
    assert_eq!((report.index, report.ef_search), (None, None));
    assert_eq!((report.queries, report.k), (0, 3));
    assert_eq!(report.recall, Some(1.0));
    assert!(m.vector_recall_check("kb", &[vec![1.0, 0.0]], 0).is_err(), "k = 0 is refused");
    assert!(m.vector_recall_check("kb", &[], 3).is_err(), "no queries is refused");
    assert!(
        m.vector_recall_check("kb", &[vec![1.0, 0.0, 0.0]], 3).is_err(),
        "a query of the wrong dimension is refused even when no index exists"
    );
    assert!(
        m.set_vector_ef_search(100).unwrap_err().to_string().starts_with("STO-E007"),
        "nothing to tune on the embedded engine"
    );
    assert!(
        m.ensure_vector_index(16, 64, 40).unwrap_err().to_string().starts_with("STO-E007"),
        "the embedded engine still refuses to build one"
    );
    assert_eq!(m.vector_index().unwrap(), None);
    // Dropping what was never built is a no-op, not a refusal: the point of
    // a drop is exact reads, and they already are.
    m.drop_vector_index().unwrap();
    assert_eq!(m.vector_index().unwrap(), None);
}
