//! Namespace prefix scoping (`"org.*"`) at the store layer: the `ns_reg`
//! registry lifecycle, scope expansion, the scoped recall paths (structural /
//! BM25 / vector / recent), and the exact-namespace guards on writes, point
//! reads and destruction.
//!
//! Every filter test asserts what a scope EXCLUDES — a prefix that is
//! silently ignored fails open into an over-broad answer, which is the worst
//! failure shape recall has (see areev-testing).

use areev_core::types::{Event, Fact, Grain};
use areev_store::{Areev, EmbedBackend, RecallTuning};
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

/// Fixed-timestamp fact so orderings are deterministic where they matter.
fn fact_at(ns: &str, s: &str, r: &str, o: &str, at: i64) -> Fact {
    let mut f = fact(ns, s, r, o);
    f.common.created_at = Some(at);
    f
}

fn objects(grains: &[areev_core::format::deserialize::DeserializedGrain]) -> Vec<String> {
    grains
        .iter()
        .map(|g| g.get_str("object").unwrap_or("").to_string())
        .collect()
}

fn open(d: &TempDir) -> Areev {
    Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap()
}

/// The namespace tree every scoping test uses. `organization`, `orgs` and
/// `org:x` are the lookalike traps a raw prefix match would wrongly include.
fn seed_tree(m: &mut Areev) {
    for (i, (ns, o)) in [
        ("org", "root-value"),
        ("org.sales", "sales-value"),
        ("org.sales.emea", "emea-value"),
        ("org.ops", "ops-value"),
        ("organization", "lookalike-value"),
        ("orgs", "plural-value"),
        ("org:x", "colon-value"),
        ("personal", "personal-value"),
    ]
    .iter()
    .enumerate()
    {
        m.add(&fact_at(ns, "john", "prefers", o, 1_700_000_000_000 + i as i64))
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// Structural recall scoping
// ---------------------------------------------------------------------------

#[test]
fn prefix_recall_selects_parent_and_descendants_only() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    seed_tree(&mut m);

    let got = objects(&m.recall("org.*", "john", None, 16).unwrap());
    // Exactly the tree — newest-first by insertion (seq desc).
    assert_eq!(
        got,
        vec!["ops-value", "emea-value", "sales-value", "root-value"],
        "org.* must select org + '.'-descendants, nothing else"
    );
    // The exclusions are the test: a dropped scope fails open.
    for excluded in ["lookalike-value", "plural-value", "colon-value", "personal-value"] {
        assert!(!got.iter().any(|o| o == excluded), "{excluded} leaked into org.*");
    }
}

#[test]
fn deeper_prefix_narrows_and_exact_stays_exact() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    seed_tree(&mut m);

    let sales = objects(&m.recall("org.sales.*", "john", None, 16).unwrap());
    assert_eq!(sales, vec!["emea-value", "sales-value"]);

    // Exact "org" is only the parent — descendants must NOT ride along.
    let exact = objects(&m.recall("org", "john", None, 16).unwrap());
    assert_eq!(exact, vec!["root-value"]);
}

#[test]
fn colon_hierarchies_scope_with_their_own_separator() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    for (ns, o) in [("agent", "a"), ("agent:authz", "b"), ("agent.dot", "c"), ("agents", "d")] {
        m.add(&fact(ns, "s", "r", o)).unwrap();
    }
    let got = objects(&m.recall("agent:*", "s", None, 16).unwrap());
    assert_eq!(got, vec!["b", "a"], "agent:* = agent + ':'-descendants; '.'-tree and 'agents' stay out");
}

#[test]
fn prefix_recall_respects_k_across_namespaces() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    for i in 0..6 {
        let ns = if i % 2 == 0 { "org.a" } else { "org.b" };
        m.add(&fact_at(ns, "john", "said", &format!("v{i}"), 1_700_000_000_000 + i)).unwrap();
    }
    let got = objects(&m.recall("org.*", "john", None, 3).unwrap());
    // Global newest-first across the set, bounded by k — not k per namespace.
    assert_eq!(got, vec!["v5", "v4", "v3"]);
}

#[test]
fn malformed_patterns_error_instead_of_matching_nothing() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    seed_tree(&mut m);
    for bad in ["*", "org*", "*.org", "org.*x"] {
        let err = m.recall(bad, "john", None, 8).unwrap_err().to_string();
        assert!(err.starts_with("VAL-E001"), "{bad}: {err}");
    }
}

#[test]
fn prefix_with_no_matching_namespace_is_empty_not_an_error() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    seed_tree(&mut m);
    assert!(m.recall("nothing-here.*", "john", None, 8).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Hybrid recall scoping (BM25 + structural + list API)
// ---------------------------------------------------------------------------

#[test]
fn hybrid_free_text_scopes_the_bm25_leg() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    m.add(&fact("org.sales", "note1", "records", "the quarterly zebra forecast")).unwrap();
    m.add(&fact("org.ops", "note2", "records", "zebra maintenance schedule")).unwrap();
    m.add(&fact("personal", "note3", "records", "my pet zebra photos")).unwrap();

    let got = objects(
        &m.recall_hybrid("org.*", None, None, Some("zebra"), 8, None).unwrap(),
    );
    assert_eq!(got.len(), 2, "exactly the two org.* zebra grains: {got:?}");
    assert!(!got.iter().any(|o| o.contains("pet")), "personal grain leaked into org.*");
}

#[test]
fn scoped_list_api_unions_exactly_and_refuses_patterns_inside() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    for (ns, o) in [("a", "va"), ("b", "vb"), ("c", "vc")] {
        m.add(&fact(ns, "john", "prefers", o)).unwrap();
    }
    let list = vec!["a".to_string(), "c".to_string()];
    let got = objects(
        &m.recall_hybrid_scoped(&list, Some("john"), None, None, 8, None, RecallTuning::default())
            .unwrap(),
    );
    assert_eq!(got, vec!["vc", "va"], "a ∪ c, newest first — and b must be gone");

    // An empty resolved list selects nothing (fail closed, like empty IN).
    let empty: Vec<String> = Vec::new();
    assert!(m
        .recall_hybrid_scoped(&empty, Some("john"), None, None, 8, None, RecallTuning::default())
        .unwrap()
        .is_empty());

    // A pattern inside a RESOLVED list is a caller bug, refused loudly.
    let bad = vec!["a".to_string(), "b.*".to_string()];
    let err = m
        .recall_hybrid_scoped(&bad, Some("john"), None, None, 8, None, RecallTuning::default())
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("VAL-E001"), "{err}");
}

#[test]
fn scoped_recall_with_superseded_labels_the_chain_per_namespace() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    let h1 = m.add(&fact_at("org.sales", "kim", "status", "junior", 1_700_000_000_000)).unwrap();
    let mut v2 = fact_at("org.sales", "kim", "status", "senior", 1_700_000_000_001);
    v2.common.derived_from = Some(h1.to_hex());
    m.supersede(&h1, &mut v2).unwrap();
    m.add(&fact_at("org.ops", "kim", "status", "oncall", 1_700_000_000_002)).unwrap();

    // Heads only: the superseded value must NOT come back.
    let heads = objects(&m.recall("org.*", "kim", None, 8).unwrap());
    assert_eq!(heads, vec!["oncall", "senior"]);
    assert!(!heads.iter().any(|o| o == "junior"), "superseded value resurfaced");

    // Widened: the chain comes back across the scope.
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
    assert!(all.contains(&"junior".to_string()), "widened scope must include history: {all:?}");
    assert!(all.contains(&"senior".to_string()));
    assert!(all.contains(&"oncall".to_string()));
}

// ---------------------------------------------------------------------------
// Vector leg scoping
// ---------------------------------------------------------------------------

/// Deterministic toy embedder: 8-dim char-bucket histogram (no ML).
struct BucketEmbed;
impl EmbedBackend for BucketEmbed {
    fn model(&self) -> &str {
        "bucket-test"
    }
    fn dim(&self) -> usize {
        8
    }
    fn embed(&self, text: &str) -> areev_core::error::Result<Vec<f32>> {
        let mut v = vec![0.0f32; 8];
        for b in text.bytes() {
            v[(b % 8) as usize] += 1.0;
        }
        Ok(v)
    }
}

#[test]
fn vector_leg_stays_inside_the_scope() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    m.set_embedder(Box::new(BucketEmbed));
    m.add(&fact("org.sales", "doc1", "says", "alpha beta gamma")).unwrap();
    m.add(&fact("personal", "doc2", "says", "alpha beta gamma")).unwrap();

    let hits = m.search_vector("org.*", "alpha beta gamma", 8).unwrap();
    assert_eq!(hits.len(), 1, "identical text outside the scope must not match");
    let all = m.search_vector("*", "alpha beta gamma", 8);
    assert!(all.is_err(), "bare * is not a recall scope");
}

// ---------------------------------------------------------------------------
// recent / recent_scoped
// ---------------------------------------------------------------------------

#[test]
fn recent_accepts_patterns_and_scoped_list_merges() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    for (i, ns) in ["org.a", "org.b", "elsewhere"].iter().enumerate() {
        let mut e = Event::new(&format!("event-{ns}")).session("sess".to_string());
        e.common.namespace = Some(ns.to_string());
        e.common.created_at = Some(1_700_000_000_000 + i as i64);
        m.add(&e).unwrap();
    }
    let got = m.recent("org.*", None, 10).unwrap();
    let texts: Vec<_> = got.iter().map(|g| g.get_str("content").unwrap_or("").to_string()).collect();
    assert_eq!(texts, vec!["event-org.b", "event-org.a"]);
    assert!(!texts.iter().any(|t| t.contains("elsewhere")));

    let list = vec!["org.a".to_string(), "elsewhere".to_string()];
    let scoped = m.recent_scoped(&list, None, 10).unwrap();
    assert_eq!(scoped.len(), 2);
}

// ---------------------------------------------------------------------------
// Write / point-read / destruction guards
// ---------------------------------------------------------------------------

#[test]
fn writes_refuse_wildcard_namespaces() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    let err = m.add(&fact("org.*", "s", "r", "o")).unwrap_err().to_string();
    assert!(err.starts_with("VAL-E001"), "{err}");
    assert!(err.contains("exact namespace"), "{err}");
    // Nothing was stored, no namespace registered.
    assert!(m.namespaces().unwrap().is_empty());
}

#[test]
fn point_reads_policy_and_destruction_refuse_patterns() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    seed_tree(&mut m);

    assert!(m.latest("org.*", "john", "prefers").is_err(), "latest");
    assert!(m.thread_tail("org.*", "sess", 5).is_err(), "thread_tail");
    assert!(m.heads("org.*", "john", "prefers").is_err(), "heads");
    assert!(m.forget_subject("org.*", "john").is_err(), "forget_subject");
    assert!(m.subject_report("org.*", "john").is_err(), "subject_report");
    assert!(
        m.forget_older_than(Some("org.*"), 1_900_000_000_000, None).is_err(),
        "forget_older_than"
    );
    assert!(
        m.set_retention_policy("org.*", &areev_store::RetentionPolicy { days: 30.0, grain_type: None, because: None })
            .is_err(),
        "set_retention_policy"
    );
    assert!(m.place_hold("org.*", "why", "who", 0).is_err(), "place_hold");
    assert!(m.set_retention_floor("org.*", 1.0, "why").is_err(), "set_retention_floor");

    // And the recall the guards protect still works right afterwards.
    assert_eq!(m.recall("org.*", "john", None, 16).unwrap().len(), 4);
}

// ---------------------------------------------------------------------------
// Registry lifecycle
// ---------------------------------------------------------------------------

#[test]
fn registry_counts_track_add_forget_and_erasure() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    let h1 = m.add(&fact("org.a", "s1", "r", "o1")).unwrap();
    m.add(&fact("org.a", "s2", "r", "o2")).unwrap();
    m.add(&fact("org.b", "s3", "r", "o3")).unwrap();
    assert_eq!(
        m.namespaces().unwrap(),
        vec![("org.a".to_string(), 2), ("org.b".to_string(), 1)]
    );

    // A forget decrements; the last grain's forget removes the namespace.
    m.forget(&h1).unwrap();
    assert_eq!(
        m.namespaces().unwrap(),
        vec![("org.a".to_string(), 1), ("org.b".to_string(), 1)]
    );

    // Identity erasure that empties a namespace removes it from the registry
    // — an emptied namespace must stop matching prefix scopes.
    m.forget_subject("org.b", "s3").unwrap();
    assert_eq!(m.namespaces().unwrap(), vec![("org.a".to_string(), 1)]);
    assert!(m.recall("org.*", "s3", None, 8).unwrap().is_empty());
    // Supersession adds a grain (the old version stays): count goes UP.
    let h2 = m.add(&fact("org.a", "kim", "status", "junior")).unwrap();
    let mut v2 = fact("org.a", "kim", "status", "senior");
    v2.common.derived_from = Some(h2.to_hex());
    m.supersede(&h2, &mut v2).unwrap();
    assert_eq!(m.namespaces().unwrap(), vec![("org.a".to_string(), 3)]);
}

#[test]
fn registry_survives_reopen_and_replicates_via_bundles() {
    let d = TempDir::new().unwrap();
    let path_a = d.path().join("a.db");
    let path_b = d.path().join("b.db");
    let bundle = d.path().join("a.bundle");
    {
        let mut a = Areev::open(path_a.to_str().unwrap()).unwrap();
        a.add(&fact("org.sales", "john", "prefers", "window")).unwrap();
        a.add(&fact("org.ops", "john", "prefers", "aisle")).unwrap();
        a.bundle_since(0, bundle.to_str().unwrap()).unwrap();
    }
    // Reopen: registry persisted (no self-heal path — the stamp is current).
    {
        let mut a = Areev::open(path_a.to_str().unwrap()).unwrap();
        assert_eq!(
            a.namespaces().unwrap(),
            vec![("org.ops".to_string(), 1), ("org.sales".to_string(), 1)]
        );
        assert!(
            !a.open_warnings().iter().any(|w| w.contains("namespace registry")),
            "a maintained file must not re-heal: {:?}",
            a.open_warnings()
        );
    }
    // Replica: import populates the registry through the same choke points.
    {
        let mut b = Areev::open(path_b.to_str().unwrap()).unwrap();
        b.import_bundle(bundle.to_str().unwrap()).unwrap();
        assert_eq!(
            b.namespaces().unwrap(),
            vec![("org.ops".to_string(), 1), ("org.sales".to_string(), 1)]
        );
        assert_eq!(objects(&b.recall("org.*", "john", None, 8).unwrap()).len(), 2);
    }
}

#[test]
fn scope_expansion_is_sorted_and_bounded_to_grain_bearing_namespaces() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    for ns in ["org.b", "org.a", "org", "personal"] {
        m.add(&fact(ns, "s", "r", "o")).unwrap();
    }
    let scope = areev_core::ns::NsScope::parse("org.*").unwrap();
    assert_eq!(
        m.namespaces_in_scope(&scope).unwrap(),
        vec!["org".to_string(), "org.a".to_string(), "org.b".to_string()]
    );
    // A retention policy alone does not create a namespace.
    m.set_retention_policy("org.empty", &areev_store::RetentionPolicy { days: 30.0, grain_type: None, because: None })
        .unwrap();
    assert!(!m
        .namespaces_in_scope(&scope)
        .unwrap()
        .contains(&"org.empty".to_string()));
}

// ---------------------------------------------------------------------------
// Anonymization × prefix scope: per-grain policy resolution
// ---------------------------------------------------------------------------

#[test]
fn scoped_recall_applies_each_namespace_own_anon_policy() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d);
    // org.medical is pseudonymized on egress; org.public is not. A scope
    // spanning both must transform each grain under ITS OWN namespace's
    // policy — a single-namespace hint would either leak the medical
    // identity or corrupt the public grain.
    let mut f1 = fact("org.medical", "walter house", "diagnosed_with", "flu");
    f1.common.source_type = Some("user_explicit".to_string());
    m.add(&f1).unwrap();
    let mut f2 = fact("org.public", "acme", "announced", "q3 results");
    f2.common.source_type = Some("user_explicit".to_string());
    m.add(&f2).unwrap();
    m.set_anon_policy("org.medical", r#"{"mode": "egress"}"#).unwrap();

    let got = m.recall_hybrid("org.*", None, None, Some("flu results"), 8, None).unwrap();
    let by_ns: std::collections::HashMap<String, String> = got
        .iter()
        .map(|g| {
            (
                g.get_str("namespace").unwrap_or("shared").to_string(),
                g.get_str("subject").unwrap_or("").to_string(),
            )
        })
        .collect();
    let medical = by_ns.get("org.medical").expect("medical grain recalled");
    assert!(
        medical.starts_with('[') && medical != "walter house",
        "the governed namespace's identity must leave pseudonymized: {medical}"
    );
    let public = by_ns.get("org.public").expect("public grain recalled");
    assert_eq!(public, "acme", "the ungoverned namespace must stay untouched");
}
