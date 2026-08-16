//! M4: FTS leg + RRF hybrid recall + deadline fail-open.

use areev_core::types::{Event, Fact, Grain};
use areev_store::Areev;
use std::time::Duration;
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

fn seed(m: &mut Areev) {
    m.add(&fact("caller", "john", "allergic_to", "peanuts")).unwrap();
    m.add(&fact("caller", "john", "prefers", "window seat")).unwrap();
    m.add(&fact("caller", "mary", "prefers", "aisle seat")).unwrap();
    let mut e = Event::new("john asked about the peanut allergy policy on flights");
    e.common.namespace = Some("caller".to_string());
    e.session_id = Some("call-7".to_string());
    m.add(&e).unwrap();
}

#[test]
fn fts_finds_by_text() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    seed(&mut m);
    let hits = m.search_text("caller", "peanuts", 10).unwrap();
    assert!(!hits.is_empty(), "BM25 should match the fact text");
    // free-text only hybrid (no subject) — the lifted facade path
    let grains = m.recall_hybrid("caller", None, None, Some("allergy"), 10, None).unwrap();
    assert!(grains.iter().any(|g| g.get_str("content").is_some_and(|c| c.contains("allergy"))));
}

#[test]
fn hybrid_rrf_ranks_intersection_first() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    seed(&mut m);
    // subject=john + query "peanuts": the allergy fact is in BOTH legs →
    // highest RRF score; mary's fact must not appear at all.
    let grains = m
        .recall_hybrid("caller", Some("john"), None, Some("peanuts"), 10, None)
        .unwrap();
    assert!(!grains.is_empty());
    assert_eq!(grains[0].get_str("object"), Some("peanuts"));
    assert!(grains.iter().all(|g| g.get_str("subject") != Some("mary")));
}

#[test]
fn superseded_versions_leave_the_fts_leg() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    let h = m.add(&fact("caller", "john", "lives_in", "Berlin")).unwrap();
    let mut v2 = fact("caller", "john", "lives_in", "Munich");
    m.supersede(&h, &mut v2).unwrap();
    let hits = m.recall_hybrid("caller", None, None, Some("Berlin"), 10, None).unwrap();
    assert!(hits.is_empty(), "superseded text must not surface");
    let hits = m.recall_hybrid("caller", None, None, Some("Munich"), 10, None).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn zero_deadline_fails_open() {
    let d = TempDir::new().unwrap();
    let mut m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    seed(&mut m);
    // Deadline already spent: FTS leg skipped, fetch loop yields nothing —
    // but no error (fail-open contract).
    let grains = m
        .recall_hybrid("caller", Some("john"), None, Some("peanuts"), 10, Some(Duration::ZERO))
        .unwrap();
    assert!(grains.len() <= 2, "partial-or-empty, never an error");
}
