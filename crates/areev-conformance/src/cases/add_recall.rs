//! Basic write/read contract: add, recall, latest, count, reopen.

use crate::{fact, Backend};

pub fn add_recall_roundtrip(b: &dyn Backend) {
    let mut m = b.open();
    let h = m.add(&fact("caller", "john", "prefers", "tea")).unwrap();
    assert!(m.get(&h).is_ok());
    assert!(m.has(&h).unwrap());
    let got = m.recall("caller", "john", Some("prefers"), 8).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].get_str("object"), Some("tea"));
    // relation-less recall finds it too
    assert_eq!(m.recall("caller", "john", None, 8).unwrap().len(), 1);
    // the µs point read agrees
    let head = m.latest("caller", "john", "prefers").unwrap().unwrap();
    assert_eq!(head.get_str("object"), Some("tea"));
    assert_eq!(m.count().unwrap(), 1);
}

pub fn unknown_terms_short_circuit_empty(b: &dyn Backend) {
    let mut m = b.open();
    m.add(&fact("caller", "john", "prefers", "tea")).unwrap();
    // Unknown ns / subject / relation are empty results, never errors.
    assert!(m.recall("nope", "john", None, 8).unwrap().is_empty());
    assert!(m.recall("caller", "nobody", None, 8).unwrap().is_empty());
    assert!(m.recall("caller", "john", Some("unknown_rel"), 8).unwrap().is_empty());
    assert!(m.latest("caller", "nobody", "prefers").unwrap().is_none());
}

pub fn recall_orders_newest_first_and_honors_k(b: &dyn Backend) {
    let mut m = b.open();
    for i in 0..5 {
        m.add(&fact("ns", "john", &format!("rel{i}"), &format!("v{i}"))).unwrap();
    }
    let got = m.recall("ns", "john", None, 3).unwrap();
    assert_eq!(got.len(), 3, "k caps the result");
    // newest first (insertion order is the seq order here)
    let objs: Vec<_> = got.iter().filter_map(|g| g.get_str("object")).collect();
    assert_eq!(objs, vec!["v4", "v3", "v2"]);
}

pub fn reopen_preserves_state_and_counters(b: &dyn Backend) {
    let h1;
    {
        let mut m = b.open_named("re");
        h1 = m.add(&fact("ns", "a", "r", "one")).unwrap();
        m.add(&fact("ns", "b", "r", "two")).unwrap();
    } // dropped: single writer per memory
    let mut m = b.open_named("re");
    assert_eq!(m.count().unwrap(), 2, "state survives reopen");
    assert!(m.get(&h1).is_ok());
    // counters re-seed correctly: a fresh write must not collide with old seqs
    let h3 = m.add(&fact("ns", "c", "r", "three")).unwrap();
    assert_eq!(m.count().unwrap(), 3);
    assert!(m.get(&h3).is_ok());
    assert_eq!(m.recall("ns", "c", Some("r"), 4).unwrap().len(), 1);
    // op-log grew monotonically across the reopen
    let ops = m.changes_since(0, 100).unwrap();
    assert_eq!(ops.len(), 3);
    let seqs: Vec<i64> = ops.iter().map(|o| o.op_seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seqs, sorted, "op_seq strictly increasing, no reuse");
}

/// A grain may carry a `subject` without asserting a relation or object (an
/// Event about a message id, an Observation about an entity). That subject has
/// to reach the structural index on every backend: it is both the retrieval key
/// AND — because `forget_subject`/`subject_report` select through the same
/// index — the difference between erasing an identity and silently keeping it.
pub fn subject_without_relation_is_indexed(b: &dyn Backend) {
    let mut m = b.open();
    let mut e = areev_core::types::Event::new("call transcript").subject("pat");
    e.common.namespace = Some("ns".into());
    m.add(&e).unwrap();
    m.add(&fact("ns", "pat", "lives_in", "Berlin")).unwrap();
    m.add(&fact("ns", "mara", "prefers", "tea")).unwrap();

    // Retrieval leg.
    assert_eq!(
        m.recall("ns", "pat", None, 8).unwrap().len(),
        2,
        "[{}] subject-only grain must join the structural recall",
        b.name()
    );
    // What it must exclude: the grain asserts no relation, so no relation
    // filter may match it, and an unknown subject stays empty.
    assert_eq!(
        m.recall("ns", "pat", Some("lives_in"), 8).unwrap().len(),
        1,
        "[{}] a relation filter must not pick up the subject-only grain",
        b.name()
    );
    assert!(m.recall("ns", "nobody", None, 8).unwrap().is_empty(), "[{}]", b.name());

    // Disclosure and erasure legs — the pair that shares one selector.
    assert_eq!(
        m.subject_report("ns", "pat").unwrap().grains.len(),
        2,
        "[{}] DSAR must disclose the subject-only grain",
        b.name()
    );
    assert_eq!(
        m.forget_subject("ns", "pat").unwrap().grains_erased,
        2,
        "[{}] erasure must reach the subject-only grain",
        b.name()
    );
    assert_eq!(m.recent("ns", None, 8).unwrap().len(), 1, "[{}] unrelated grain survives", b.name());
}
