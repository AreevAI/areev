//! Legal holds, the namespace-scoped change feed, and the scoped graph and
//! as-of reads (1.9.0 — #278, #279, #303, #307).
//!
//! All four are STORE semantics, so they must hold identically on the
//! embedded backend and on Postgres. The hold cases matter most: the guard
//! runs INSIDE the erasure transaction after `reserve_write` precisely
//! because a hold placed concurrently on Postgres must not lose the race,
//! and a contract asserted on only one backend would not have caught that.

use crate::{fact, Backend};
use areev_store::{Direction, ErasureOptions, HoldOverride, OP_FORGET};

/// A hold refuses EVERY deletion path — not only the age-based sweep it
/// used to guard — and a refusal erases nothing.
pub fn a_hold_refuses_every_deletion_path(b: &dyn Backend) {
    let mut m = b.open_named("hold_paths");
    let h1 = m.add(&fact("held", "acme", "owes", "4200")).unwrap();
    m.add(&fact("held", "acme", "invoiced", "2026-01-04")).unwrap();
    let free = m.add(&fact("open", "zeta", "owes", "10")).unwrap();
    m.place_hold("held", "litigation 2026-114", "user:counsel", 1_000).unwrap();

    // 1. FORGET by hash.
    let e = m.forget(&h1).unwrap_err();
    assert!(e.to_string().starts_with("STO-E009"), "forget by hash: {e}");
    // 2. Identity erasure.
    let e = m.forget_subject("held", "acme").unwrap_err();
    assert!(e.to_string().starts_with("STO-E009"), "forget_subject: {e}");
    // 3. The age-based sweep, scoped at the held namespace.
    let e = m.forget_older_than(Some("held"), i64::MAX, None).unwrap_err();
    assert!(e.to_string().starts_with("STO-E009"), "age sweep: {e}");

    // A refusal erases NOTHING — not even a partial pass (REQ-ERASE-4
    // applied to the deferral).
    assert_eq!(m.recall("held", "acme", None, 16).unwrap().len(), 2);
    assert!(m.get(&h1).is_ok(), "a refused forget must leave the grain readable");

    // An unheld namespace in the same memory is unaffected: a hold is
    // per-namespace, and over-holding a whole file would be its own bug.
    m.forget(&free).unwrap();
    assert!(m.get(&free).is_err());

    // Releasing the hold restores every path.
    m.release_hold("held").unwrap();
    m.forget(&h1).unwrap();
    assert_eq!(m.forget_subject("held", "acme").unwrap().grains_erased, 1);
}

/// The D10 override destroys anyway and NAMES the hold it overrode, so the
/// audit record can say what was overridden rather than merely that
/// something was.
pub fn a_hold_override_destroys_and_names_the_hold(b: &dyn Backend) {
    let mut m = b.open_named("hold_override");
    let h = m.add(&fact("held", "acme", "owes", "4200")).unwrap();
    m.add(&fact("held", "acme", "invoiced", "2026-01-04")).unwrap();
    m.place_hold("held", "litigation 2026-114", "user:counsel", 1_000).unwrap();

    let over = HoldOverride::new("user:dpo", "regulator ordered destruction").unwrap();
    let named = m.forget_overriding(&h, &over).unwrap().expect("the hold must be reported");
    assert_eq!(named.ns, "held");
    assert_eq!(named.because, "litigation 2026-114");
    assert_eq!(named.placed_by, "user:counsel");
    assert!(m.get(&h).is_err());

    let (rep, named) = m
        .forget_subject_overriding("held", "acme", ErasureOptions::default(), &over)
        .unwrap();
    assert_eq!(rep.grains_erased, 1, "the remaining grain about acme");
    assert_eq!(named.expect("hold reported").because, "litigation 2026-114");

    // The override is not sticky: the hold is still there afterwards, so the
    // next destruction refuses again.
    assert_eq!(m.holds().unwrap().len(), 1, "an override must not release the hold");
    let h2 = m.add(&fact("held", "acme", "owes", "0")).unwrap();
    assert!(m.forget(&h2).unwrap_err().to_string().starts_with("STO-E009"));

    // An override with no stated ground is refused at construction — it is
    // indistinguishable from a mistake.
    assert!(HoldOverride::new("user:dpo", "  ").is_err());
    assert!(HoldOverride::new("", "a ground").is_err());
}

/// `hold:` rows ride a bundle AND apply on a point-in-time import, so a
/// restored or synced memory comes back HELD. Every other meta row is
/// skipped by PITR; a hold is the deliberate exception, because it is a
/// present-day stop on destruction rather than a historical fact.
pub fn holds_replicate_and_survive_a_pitr_restore(b: &dyn Backend) {
    let mut leader = b.open_named("hold_leader");
    leader.add(&fact("held", "acme", "owes", "4200")).unwrap();
    leader.place_hold("held", "litigation 2026-114", "user:counsel", 1_000).unwrap();
    let path = b.scratch().join("hold_replicate.mgb");
    leader.bundle_since(0, path.to_str().unwrap()).unwrap();

    let mut follower = b.open_named("hold_follower");
    let stats = follower.import_bundle(path.to_str().unwrap()).unwrap();
    assert!(stats.meta_applied >= 1, "the hold row must ride the bundle: {stats:?}");
    assert_eq!(
        follower.holds().unwrap(),
        vec![(
            "held".to_string(),
            "litigation 2026-114".to_string(),
            "user:counsel".to_string()
        )]
    );
    // And it BINDS on the replica — a hold that replicated as a note would
    // be worse than none.
    let h = follower.recall("held", "acme", None, 8).unwrap()[0].hash;
    assert!(follower.forget(&h).unwrap_err().to_string().starts_with("STO-E009"));

    // A point-in-time restore comes back held too, though it skips every
    // other meta row.
    let mut restored = b.open_named("hold_pitr");
    restored.import_bundle_until(path.to_str().unwrap(), Some(0)).unwrap();
    assert_eq!(restored.holds().unwrap().len(), 1, "a PITR restore must come back HELD");
}

/// Replication carries the decision; it does not re-take it. A follower
/// under its own hold still applies the leader's tombstone — aborting would
/// diverge it from its leader permanently — and the import counts it, so the
/// operator can see it happened.
pub fn a_replicated_tombstone_applies_under_a_local_hold(b: &dyn Backend) {
    let mut leader = b.open_named("tomb_leader");
    let h = leader.add(&fact("shared", "acme", "owes", "4200")).unwrap();
    leader.add(&fact("shared", "zeta", "owes", "9")).unwrap();
    let seed = b.scratch().join("tomb_seed.mgb");
    leader.bundle_since(0, seed.to_str().unwrap()).unwrap();

    let mut follower = b.open_named("tomb_follower");
    follower.import_bundle(seed.to_str().unwrap()).unwrap();
    // The follower places its OWN hold, then the leader decides to destroy.
    follower.place_hold("shared", "local matter", "user:counsel", 2_000).unwrap();
    let after = leader.changes_since(0, 1_000).unwrap().last().unwrap().op_seq;
    leader.forget(&h).unwrap();
    let tomb = b.scratch().join("tomb_delta.mgb");
    leader.bundle_since(after, tomb.to_str().unwrap()).unwrap();

    let stats = follower.import_bundle(tomb.to_str().unwrap()).unwrap();
    assert_eq!(stats.forgets_under_hold, 1, "the deletion under hold must be counted: {stats:?}");
    assert!(follower.get(&h).is_err(), "a follower must converge, not diverge");
    // A LOCAL destruction in the same namespace still refuses: the hold is
    // intact and binds the decisions this memory makes.
    let local = follower.recall("shared", "zeta", None, 8).unwrap()[0].hash;
    assert!(follower.forget(&local).unwrap_err().to_string().starts_with("STO-E009"));
}

/// The change feed is namespace-scoped, and a TOMBSTONE is attributable
/// (#307) — which resolving its hash cannot do, because the grain is gone.
/// `op_seq` stays the memory-wide sequence, so a scoped cursor and an
/// unscoped one remain comparable.
pub fn the_change_feed_is_namespace_scoped_including_tombstones(b: &dyn Backend) {
    let mut m = b.open_named("scoped_feed");
    let a1 = m.add(&fact("n1", "alice", "owes", "1")).unwrap();
    m.add(&fact("n2", "bob", "owes", "2")).unwrap();
    m.add(&fact("n1", "carol", "owes", "3")).unwrap();
    m.forget(&a1).unwrap();

    let all = m.changes_since(0, 100).unwrap();
    assert_eq!(all.len(), 4, "unscoped feed sees every op");
    // Every row is attributed, the tombstone included.
    assert!(all.iter().all(|o| o.ns.is_some()), "every op must carry its namespace: {all:?}");
    let tomb = all.iter().find(|o| o.op == OP_FORGET).expect("tombstone row");
    assert_eq!(tomb.ns.as_deref(), Some("n1"), "a tombstone must stay attributable");

    let n1 = m.changes_since_scoped(0, &["n1".to_string()], 100).unwrap();
    assert_eq!(n1.len(), 3, "two adds and the tombstone: {n1:?}");
    assert!(n1.iter().all(|o| o.ns.as_deref() == Some("n1")));
    let n2 = m.changes_since_scoped(0, &["n2".to_string()], 100).unwrap();
    assert_eq!(n2.len(), 1);

    // op_seq is the memory-wide sequence: a scoped cursor is comparable with
    // an unscoped one, and paging from it skips nothing in scope.
    let both = m.changes_since_scoped(0, &["n1".to_string(), "n2".to_string()], 100).unwrap();
    assert_eq!(
        both.iter().map(|o| o.op_seq).collect::<Vec<_>>(),
        all.iter().map(|o| o.op_seq).collect::<Vec<_>>(),
    );
    let tail = m.changes_since_scoped(n1[0].op_seq, &["n1".to_string()], 100).unwrap();
    assert_eq!(tail.len(), 2, "paging from a scoped cursor: {tail:?}");
}

/// A walk over a namespace SET shares ONE frontier, `seen`, depth and cap —
/// which is exactly why it cannot be composed from per-namespace calls. The
/// as-of read answers each namespace independently, because "the value at T"
/// is a per-namespace question.
pub fn scoped_graph_and_as_of_reads_span_a_namespace_set(b: &dyn Backend) {
    let mut opts = areev_store::AreevOptions::default();
    opts.entity_relations.insert("advises".to_string());
    let mut m = b.open_named_with("scoped_graph", opts);
    m.add(&fact("n1", "a", "advises", "b")).unwrap();
    m.add(&fact("n2", "b", "advises", "c")).unwrap();
    m.add(&fact("n3", "c", "advises", "d")).unwrap();

    // Neither namespace alone reaches past its own edge…
    assert_eq!(m.related("n1", "a", &["advises"], Direction::Out, 4, 64).unwrap(), vec!["b"]);
    assert!(m.related("n2", "a", &["advises"], Direction::Out, 4, 64).unwrap().is_empty());
    // …and the union of two separate answers is still not the walk: only a
    // shared frontier crosses the n1→n2 edge and goes on.
    let two = m
        .related_scoped(
            &["n1".to_string(), "n2".to_string()],
            "a",
            &["advises"],
            Direction::Out,
            4,
            64,
        )
        .unwrap();
    assert_eq!(two, vec!["b".to_string(), "c".to_string()], "one shared frontier");
    // An ungranted namespace simply not in the set bounds the walk.
    let three = m
        .related_scoped(
            &["n1".to_string(), "n2".to_string(), "n3".to_string()],
            "a",
            &["advises"],
            Direction::Out,
            4,
            64,
        )
        .unwrap();
    assert_eq!(three.len(), 3);
    // The cap is the SHARED one, not per namespace.
    let capped = m
        .related_scoped(
            &["n1".to_string(), "n2".to_string(), "n3".to_string()],
            "a",
            &["advises"],
            Direction::Out,
            4,
            2,
        )
        .unwrap();
    assert_eq!(capped.len(), 2, "the cap bounds the whole walk: {capped:?}");

    // The as-of read answers each namespace as itself, labelled — the same
    // subject can legitimately hold different values in two tenancies.
    let mut e1 = fact("n1", "deal", "stage", "LOI");
    e1.common.valid_from = Some(1_000);
    m.add(&e1).unwrap();
    let mut e2 = fact("n2", "deal", "stage", "Closed");
    e2.common.valid_from = Some(1_000);
    m.add(&e2).unwrap();
    let at = m
        .entity_at_scoped(
            &["n1".to_string(), "n2".to_string()],
            "deal",
            "stage",
            5_000,
            areev_store::Axis::World,
        )
        .unwrap();
    assert_eq!(at.len(), 2, "one answer per namespace");
    let mut seen: Vec<(&str, &str)> =
        at.iter().map(|(ns, g)| (ns.as_str(), g.get_str("object").unwrap())).collect();
    seen.sort();
    assert_eq!(seen, vec![("n1", "LOI"), ("n2", "Closed")]);

    // A repeated namespace is answered once, not twice.
    let dup = m
        .entity_at_scoped(
            &["n1".to_string(), "n1".to_string()],
            "deal",
            "stage",
            5_000,
            areev_store::Axis::World,
        )
        .unwrap();
    assert_eq!(dup.len(), 1);
}

/// Among the windows that contain T, the one that took effect most recently
/// in WORLD time wins — write order only breaks an exact tie (#305). Two
/// OPEN-ENDED windows both contain every instant after the later start, so
/// ordering by write sequence answered a world-time question with a
/// system-time tie-break, and an out-of-order backfill read wrong forever.
pub fn the_world_axis_is_write_order_independent(b: &dyn Backend) {
    let mut m = b.open_named("world_axis_order");
    // The LATER state is written FIRST — what a CRM field history in arrival
    // order produces.
    let mut loi = fact("ns", "deal", "stage", "LOI");
    loi.common.valid_from = Some(2_000);
    m.add(&loi).unwrap();
    let mut eval = fact("ns", "deal", "stage", "Evaluating");
    eval.common.valid_from = Some(1_000);
    m.add(&eval).unwrap();

    let at = |m: &mut areev_store::Areev, t: i64| {
        m.entity_at("ns", "deal", "stage", t, areev_store::Axis::World)
            .unwrap()
            .unwrap()
            .get_str("object")
            .unwrap()
            .to_string()
    };
    assert_eq!(at(&mut m, 1_500), "Evaluating");
    assert_eq!(at(&mut m, 3_000), "LOI");

    // The mirror, written in date order, must give the same two answers —
    // that is what "write order independent" means.
    let mut m2 = b.open_named("world_axis_order_2");
    let mut eval = fact("ns", "deal", "stage", "Evaluating");
    eval.common.valid_from = Some(1_000);
    m2.add(&eval).unwrap();
    let mut loi = fact("ns", "deal", "stage", "LOI");
    loi.common.valid_from = Some(2_000);
    m2.add(&loi).unwrap();
    assert_eq!(at(&mut m2, 1_500), "Evaluating");
    assert_eq!(at(&mut m2, 3_000), "LOI");
}
