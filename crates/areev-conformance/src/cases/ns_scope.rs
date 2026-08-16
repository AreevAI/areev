//! Namespace prefix scoping (`"org.*"`): the registry lifecycle, scope
//! expansion, scoped recall across every leg, and the exact-namespace guards
//! — all of which must hold IDENTICALLY on every backend, because a scope
//! that silently widens (or a registry that drifts) on one backend is a
//! cross-backend divergence in what a model gets to see.

use crate::{fact, Backend};
use areev_store::RecallTuning;

fn objects(grains: &[areev_core::format::deserialize::DeserializedGrain]) -> Vec<String> {
    grains
        .iter()
        .map(|g| g.get_str("object").unwrap_or("").to_string())
        .collect()
}

fn seed_tree(m: &mut areev_store::Areev) {
    for (ns, o) in [
        ("org", "root-value"),
        ("org.sales", "sales-value"),
        ("org.sales.emea", "emea-value"),
        ("org.ops", "ops-value"),
        ("organization", "lookalike-value"),
        ("orgs", "plural-value"),
        ("org:x", "colon-value"),
        ("personal", "personal-value"),
    ] {
        m.add(&fact(ns, "john", "prefers", o)).unwrap();
    }
}

/// `org.*` = parent + `.`-descendants, nothing else — on every backend, and
/// on every leg (structural anchored, hybrid free-text, recent scan).
pub fn ns_prefix_scope_selects_tree_exactly(b: &dyn Backend) {
    let mut m = b.open();
    seed_tree(&mut m);

    let mut got = objects(&m.recall("org.*", "john", None, 16).unwrap());
    got.sort();
    assert_eq!(
        got,
        vec!["emea-value", "ops-value", "root-value", "sales-value"],
        "[{}] org.* must select org + '.'-descendants only",
        b.name()
    );
    for excluded in ["lookalike-value", "plural-value", "colon-value", "personal-value"] {
        assert!(!got.iter().any(|o| o == excluded), "[{}] {excluded} leaked", b.name());
    }

    // The recent scan honors the same scope.
    let recent = objects(&m.recent("org.sales.*", None, 16).unwrap());
    assert_eq!(recent.len(), 2, "[{}] recent over org.sales.*: {recent:?}", b.name());

    // Free-text (BM25) leg stays inside the scope.
    let hits = objects(
        &m.recall_hybrid("org.*", None, None, Some("prefers john"), 16, None)
            .unwrap_or_default(),
    );
    assert!(
        !hits.iter().any(|o| o == "personal-value"),
        "[{}] BM25 leg leaked outside the scope: {hits:?}",
        b.name()
    );

    // Malformed patterns refuse; unknown prefixes answer empty.
    assert!(m.recall("org*", "john", None, 8).is_err(), "[{}] org* must refuse", b.name());
    assert!(m.recall("nothing.*", "john", None, 8).unwrap().is_empty());
}

/// The registry is count-maintained through add / supersede / forget /
/// erasure, and an emptied namespace stops matching prefix scopes.
pub fn ns_registry_tracks_lifecycle(b: &dyn Backend) {
    let mut m = b.open();
    let h1 = m.add(&fact("org.a", "s1", "r", "o1")).unwrap();
    m.add(&fact("org.a", "s2", "r", "o2")).unwrap();
    m.add(&fact("org.b", "s3", "r", "o3")).unwrap();
    assert_eq!(
        m.namespaces().unwrap(),
        vec![("org.a".to_string(), 2), ("org.b".to_string(), 1)],
        "[{}]",
        b.name()
    );

    // Supersession ADDS a grain (history is kept): the count rises.
    let mut v2 = fact("org.a", "s1", "r", "o1-v2");
    m.supersede(&h1, &mut v2).unwrap();
    assert_eq!(m.namespaces().unwrap()[0], ("org.a".to_string(), 3), "[{}]", b.name());

    // Erasing the only subject in org.b empties it: the namespace must
    // leave the registry AND stop matching scopes.
    m.forget_subject("org.b", "s3").unwrap();
    assert_eq!(m.namespaces().unwrap().len(), 1, "[{}]", b.name());
    assert!(m.recall("org.*", "s3", None, 8).unwrap().is_empty(), "[{}]", b.name());
    let scope = areev_core::ns::NsScope::parse("org.*").unwrap();
    assert_eq!(
        m.namespaces_in_scope(&scope).unwrap(),
        vec!["org.a".to_string()],
        "[{}] an emptied namespace must not appear in an expansion",
        b.name()
    );
}

/// The registry replicates through bundles by construction (import runs the
/// same choke points), including tombstone replay.
pub fn ns_registry_replicates_via_bundles(b: &dyn Backend) {
    let bundle = b.scratch().join("ns_scope.bundle");
    let bundle = bundle.to_str().unwrap();
    let h;
    {
        let mut a = b.open_named("ns_src");
        h = a.add(&fact("org.sales", "john", "prefers", "window")).unwrap();
        a.add(&fact("org.ops", "john", "prefers", "aisle")).unwrap();
        a.bundle_since(0, bundle).unwrap();
    }
    {
        let mut r = b.open_named("ns_replica");
        r.import_bundle(bundle).unwrap();
        assert_eq!(
            r.namespaces().unwrap(),
            vec![("org.ops".to_string(), 1), ("org.sales".to_string(), 1)],
            "[{}] import must register namespaces",
            b.name()
        );
        assert_eq!(r.recall("org.*", "john", None, 8).unwrap().len(), 2, "[{}]", b.name());
    }
    // A forget at the source replays as a tombstone and updates the
    // replica's registry too.
    {
        let mut a = b.open_named("ns_src");
        a.forget(&h).unwrap();
        a.bundle_since(0, bundle).unwrap();
    }
    {
        let mut r = b.open_named("ns_replica");
        r.import_bundle(bundle).unwrap();
        assert_eq!(
            r.namespaces().unwrap(),
            vec![("org.ops".to_string(), 1)],
            "[{}] tombstone replay must unregister the emptied namespace",
            b.name()
        );
    }
}

/// `*` is reserved: writes refuse it, and the exact-namespace surfaces
/// (point reads, destruction, policy) refuse patterns — identically on
/// every backend, because a backend that accepted one would let a wildcard
/// widen destruction there.
pub fn ns_scope_guards_hold(b: &dyn Backend) {
    let mut m = b.open();
    seed_tree(&mut m);

    let err = m.add(&fact("org.*", "s", "r", "o")).unwrap_err().to_string();
    assert!(err.starts_with("VAL-E001"), "[{}] {err}", b.name());

    assert!(m.latest("org.*", "john", "prefers").is_err(), "[{}] latest", b.name());
    assert!(m.forget_subject("org.*", "john").is_err(), "[{}] forget_subject", b.name());
    assert!(m.subject_report("org.*", "john").is_err(), "[{}] subject_report", b.name());
    assert!(
        m.forget_older_than(Some("org.*"), i64::MAX / 2, None).is_err(),
        "[{}] forget_older_than",
        b.name()
    );
    // The refusals destroyed nothing.
    assert_eq!(m.recall("org.*", "john", None, 16).unwrap().len(), 4, "[{}]", b.name());
}

/// Heads-only vs `include_superseded` across a scoped set: the default keeps
/// stale values out of context; the widened scan returns the chain — with
/// each namespace's own chain intact.
pub fn ns_scoped_recall_spans_supersession_chains(b: &dyn Backend) {
    let mut m = b.open();
    let h1 = m.add(&fact("org.sales", "kim", "status", "junior")).unwrap();
    let mut v2 = fact("org.sales", "kim", "status", "senior");
    m.supersede(&h1, &mut v2).unwrap();
    m.add(&fact("org.ops", "kim", "status", "oncall")).unwrap();

    let mut heads = objects(&m.recall("org.*", "kim", None, 8).unwrap());
    heads.sort();
    assert_eq!(heads, vec!["oncall", "senior"], "[{}] superseded value must stay out", b.name());

    let all = objects(
        &m.recall_hybrid_tuned(
            "org.*",
            Some("kim"),
            None,
            None,
            8,
            None,
            RecallTuning { include_superseded: true, ..Default::default() },
        )
        .unwrap(),
    );
    for expect in ["junior", "senior", "oncall"] {
        assert!(all.iter().any(|o| o == expect), "[{}] {expect} missing from chain: {all:?}", b.name());
    }
}
