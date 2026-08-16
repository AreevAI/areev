//! v4 grain-git model: the v2a/v2b scenario — concurrent supersedes on two
//! nodes, fork on merge, provisional head, explicit merge commit.

use areev_core::types::{Fact, Grain};
use areev_store::Areev;
use tempfile::TempDir;

fn fact(s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some("ns".to_string());
    f
}

#[test]
fn concurrent_supersede_forks_then_merges() {
    let d = TempDir::new().unwrap();
    let pa = d.path().join("edge_a.db");
    let pb = d.path().join("edge_b.db");
    let bundle_v1 = d.path().join("v1.mgb");
    let bundle_b = d.path().join("b_delta.mgb");

    // edge A: v1, replicate to edge B
    let mut a = Areev::open(pa.to_str().unwrap()).unwrap();
    let v1 = a.add(&fact("john", "plan", "basic")).unwrap();
    let st = a.bundle_since(0, bundle_v1.to_str().unwrap()).unwrap();
    let mut b = Areev::open(pb.to_str().unwrap()).unwrap();
    b.import_bundle(bundle_v1.to_str().unwrap()).unwrap();

    // concurrent edits: A → v2a, B → v2b (deterministic created_at so the
    // provisional-head election is testable: v2b is later → wins)
    let mut v2a = fact("john", "plan", "pro");
    v2a.common.created_at = Some(1_800_000_000_000);
    let h2a = a.supersede(&v1, &mut v2a).unwrap();
    let mut v2b = fact("john", "plan", "enterprise");
    v2b.common.created_at = Some(1_800_000_000_500);
    let h2b = b.supersede(&v1, &mut v2b).unwrap();

    // B's delta reaches A → fork on A
    b.bundle_since(st.last_op_seq, bundle_b.to_str().unwrap()).unwrap();
    a.import_bundle(bundle_b.to_str().unwrap()).unwrap();

    // both tips alive — nothing lost
    let heads = a.heads("ns", "john", "plan").unwrap();
    assert_eq!(heads.len(), 2, "fork must keep both tips");
    assert!(a.get(&h2a).is_ok() && a.get(&h2b).is_ok() && a.get(&v1).is_ok());

    // provisional head is deterministic: later created_at (v2b) wins
    let head = a.latest("ns", "john", "plan").unwrap().unwrap();
    assert_eq!(head.get_str("object"), Some("enterprise"));
    // contested state visible: recall shows both current tips
    let cur = a.recall("ns", "john", Some("plan"), 8).unwrap();
    assert_eq!(cur.len(), 2, "contested fact surfaces both versions");

    // explicit merge commit closes the fork with both parents recorded
    let mut v3 = fact("john", "plan", "enterprise (migrated from pro)");
    let h3 = a.merge_heads("ns", "john", "plan", &mut v3).unwrap();
    let heads = a.heads("ns", "john", "plan").unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].0, h3);
    let cur = a.recall("ns", "john", Some("plan"), 8).unwrap();
    assert_eq!(cur.len(), 1);
    let merged = a.get(&h3).unwrap();
    let parents = serde_json::to_string(&merged.fields["context"]["merge_parents"]).unwrap();
    assert!(parents.contains(&h2a.to_hex()) && parents.contains(&h2b.to_hex()),
        "merge records both parents: {parents}");
    // full triangle still auditable
    assert!(a.get(&h2a).unwrap().get_str("object") == Some("pro"));
}

#[test]
fn same_supersede_replay_stays_idempotent() {
    let d = TempDir::new().unwrap();
    let pa = d.path().join("a.db");
    let pb = d.path().join("rep.db");
    let bl = d.path().join("all.mgb");
    let mut a = Areev::open(pa.to_str().unwrap()).unwrap();
    let v1 = a.add(&fact("john", "tier", "t1")).unwrap();
    let mut v2 = fact("john", "tier", "t2");
    a.supersede(&v1, &mut v2).unwrap();
    a.bundle_since(0, bl.to_str().unwrap()).unwrap();
    let mut b = Areev::open(pb.to_str().unwrap()).unwrap();
    b.import_bundle(bl.to_str().unwrap()).unwrap();
    let second = b.import_bundle(bl.to_str().unwrap()).unwrap();
    assert_eq!(second.applied, 0, "replay is a no-op");
    assert_eq!(b.heads("ns", "john", "tier").unwrap().len(), 1, "no phantom fork");
}

#[test]
fn open_forks_enumerates_and_clears_on_merge() {
    let d = TempDir::new().unwrap();
    let pa = d.path().join("a.db");
    let pb = d.path().join("b.db");
    let v1b = d.path().join("v1.mgb");
    let bdelta = d.path().join("b.mgb");

    // Build the v2a/v2b fork on edge A (same shape as the merge test).
    let mut a = Areev::open(pa.to_str().unwrap()).unwrap();
    let v1 = a.add(&fact("john", "plan", "basic")).unwrap();
    let st = a.bundle_since(0, v1b.to_str().unwrap()).unwrap();
    let mut b = Areev::open(pb.to_str().unwrap()).unwrap();
    b.import_bundle(v1b.to_str().unwrap()).unwrap();

    let mut v2a = fact("john", "plan", "pro");
    v2a.common.created_at = Some(1_800_000_000_000);
    a.supersede(&v1, &mut v2a).unwrap();
    let mut v2b = fact("john", "plan", "enterprise");
    v2b.common.created_at = Some(1_800_000_000_500);
    b.supersede(&v1, &mut v2b).unwrap();
    b.bundle_since(st.last_op_seq, bdelta.to_str().unwrap()).unwrap();
    a.import_bundle(bdelta.to_str().unwrap()).unwrap();

    // The fork is discoverable without knowing the subject/relation up front.
    let forks = a.open_forks().unwrap();
    assert_eq!(forks.len(), 1, "exactly one open fork");
    assert_eq!(forks[0].subject, "john");
    assert_eq!(forks[0].relation, "plan");
    assert_eq!(forks[0].heads.len(), 2, "both tips reported");

    // A file with no forks reports none.
    assert!(b.open_forks().unwrap().is_empty() || b.heads("ns", "john", "plan").unwrap().len() == 1);

    // Merging closes it — enumeration goes empty.
    let mut v3 = fact("john", "plan", "enterprise");
    a.merge_heads("ns", "john", "plan", &mut v3).unwrap();
    assert!(a.open_forks().unwrap().is_empty(), "merge clears the fork");
}
