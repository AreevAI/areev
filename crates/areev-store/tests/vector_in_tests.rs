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
