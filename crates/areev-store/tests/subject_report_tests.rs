//! The read-only DSAR surface (`subject_report` / `subject_bundle_with`) —
//! GDPR Art. 15/20. The load-bearing contract: the report and the erasure
//! run the SAME selector, so "show me everything, then delete it" is two
//! calls over one selection.

use areev_core::types::{Fact, Grain};
use areev_store::{Areev, AreevOptions, ErasureOptions};
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

/// Pin `created_at` so two writes in one tick stay two grains (see the
/// fork-merge tests for why).
fn fact_at(ns: &str, s: &str, r: &str, o: &str, at: i64) -> Fact {
    let mut f = fact(ns, s, r, o);
    f.common.created_at = Some(at);
    f
}

#[test]
fn report_matches_erasure_selection_exactly() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();

    // pat: exact subject, a partition key, an object-position reference,
    // and a superseded value (history must be in the report).
    m.add(&fact_at("caller", "pat", "prefers", "tea", 1_000)).unwrap();
    m.add(&fact_at("caller", "pat#visit1", "note", "arrived late", 2_000)).unwrap();
    m.add(&fact_at("caller", "dr_lee", "treats", "pat", 3_000)).unwrap();
    let old = m.add(&fact_at("caller", "pat", "lives_in", "Berlin", 4_000)).unwrap();
    let mut moved = fact_at("caller", "pat", "lives_in", "Munich", 5_000);
    m.supersede(&old, &mut moved).unwrap();
    // Not pat: a longer word sharing the prefix, and an unrelated subject.
    m.add(&fact_at("caller", "patricia", "prefers", "coffee", 6_000)).unwrap();
    m.add(&fact_at("caller", "mary", "prefers", "juice", 7_000)).unwrap();

    let report = m.subject_report("caller", "pat").unwrap();
    assert_eq!(report.grains.len(), 5, "exact + partition + object-ref + full history");
    assert!(report.identity_names.contains(&"pat".to_string()));
    assert!(report.identity_names.contains(&"pat#visit1".to_string()));
    assert!(
        !report.identity_names.iter().any(|n| n == "patricia"),
        "a longer word is never the identity"
    );
    // The filter must EXCLUDE: neither patricia's nor mary's grains.
    for g in &report.grains {
        let s = g.fields.get("subject").and_then(|v| v.as_str()).unwrap_or("");
        let o = g.fields.get("object").and_then(|v| v.as_str()).unwrap_or("");
        assert_ne!(s, "patricia", "patricia must not appear in pat's report");
        assert_ne!(s, "mary");
        assert!(s.starts_with("pat") || o == "pat", "unexpected grain in report: {s} {o}");
    }

    // THE contract: erasure removes exactly what the report showed.
    let erased = m.forget_subject("caller", "pat").unwrap();
    assert_eq!(
        erased.grains_erased,
        report.grains.len(),
        "report and erasure must be one selection"
    );
    assert!(m.subject_report("caller", "pat").unwrap().grains.is_empty());
    // The excluded grains survive.
    assert_eq!(m.subject_report("caller", "patricia").unwrap().grains.len(), 1);
}

#[test]
fn report_text_mentions_reaches_prose_and_guards_like_erasure() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    m.add(&fact_at("caller", "pat", "prefers", "tea", 1_000)).unwrap();
    m.add(&fact_at("caller", "note-1", "says", "call pat about the visit", 2_000)).unwrap();
    m.add(&fact_at("caller", "note-2", "says", "water the plants", 3_000)).unwrap();

    let plain = m.subject_report("caller", "pat").unwrap();
    assert_eq!(plain.grains.len(), 1, "structural only without text_mentions");
    let wide = m
        .subject_report_with("caller", "pat", ErasureOptions { text_mentions: true })
        .unwrap();
    assert_eq!(wide.grains.len(), 2, "prose mention joins the selection");

    // Same hard guard as erasure when the index is off: never a silent
    // partial answer.
    let dir2 = TempDir::new().unwrap();
    let mut voice = Areev::open_with(
        dir2.path().join("v.db").to_str().unwrap(),
        AreevOptions { index_text: false, ..Default::default() },
    )
    .unwrap();
    voice.add(&fact("caller", "pat", "prefers", "tea")).unwrap();
    let denied =
        voice.subject_report_with("caller", "pat", ErasureOptions { text_mentions: true });
    assert!(denied.is_err(), "text_mentions without the index must hard-error");
}

#[test]
fn report_edge_cases_refuse_or_empty() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    m.add(&fact("caller", "pat", "prefers", "tea")).unwrap();
    assert!(m.subject_report("caller", "  ").is_err(), "empty subject is a caller bug");
    assert!(
        m.subject_report("no-such-ns", "pat").unwrap().grains.is_empty(),
        "unknown namespace is an empty report, same contract as erasure"
    );
}

#[test]
fn subject_bundle_is_portable_and_scoped() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    m.add(&fact_at("caller", "pat", "prefers", "tea", 1_000)).unwrap();
    let old = m.add(&fact_at("caller", "pat", "lives_in", "Berlin", 2_000)).unwrap();
    let mut moved = fact_at("caller", "pat", "lives_in", "Munich", 3_000);
    let new = m.supersede(&old, &mut moved).unwrap();
    m.add(&fact_at("caller", "mary", "prefers", "juice", 4_000)).unwrap();

    let path = dir.path().join("pat.mgb");
    let stats = m
        .subject_bundle_with("caller", "pat", ErasureOptions::default(), path.to_str().unwrap())
        .unwrap();
    // Three grains → four records: the superseding grain exports its ADD
    // and its SUPERSEDE marker (both ride the same hash). No tombstones.
    assert_eq!(stats.ops, 4, "adds + the supersession pair, no tombstones");

    // Portable: a fresh store imports it and reconstructs the chain.
    let dir2 = TempDir::new().unwrap();
    let mut peer = Areev::open(dir2.path().join("p.db").to_str().unwrap()).unwrap();
    peer.import_bundle(path.to_str().unwrap()).unwrap();
    assert_eq!(peer.count().unwrap(), 3, "pat's grains only — mary is not in the bundle");
    assert!(peer.get(&new).is_ok());
    let head = peer.latest("caller", "pat", "lives_in").unwrap().expect("head");
    assert_eq!(head.get_str("object"), Some("Munich"), "supersession replays causally");
}
