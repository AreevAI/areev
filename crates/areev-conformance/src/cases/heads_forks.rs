//! The grains-as-git model: fork creation via import, deterministic
//! provisional-head election, merge, and re-election after tip removal.
//! Every election assertion pins `created_at` (determinism rule #2).

use crate::{fact, fact_at, Backend};
use areev_core::error::Hash;
use areev_core::types::Grain;
use areev_store::Areev;

/// Two-tip fork on (ns, john, plan) built by concurrent supersedes on two
/// peers of the same backend: tip A "pro" (older), tip B "enterprise"
/// (newer, provisional winner). Returns (store_a, tip_a, tip_b).
pub(crate) fn build_two_tip_fork(b: &dyn Backend, link_tip_a: bool) -> (Areev, Hash, Hash) {
    let bv1 = b.scratch().join("fork_v1.mgb");
    let bb = b.scratch().join("fork_b.mgb");
    let mut a = b.open_named("fork_a");
    let anchor = a.add(&fact("ns", "acct", "kind", "profile")).unwrap();
    let v1 = a.add(&fact("ns", "john", "plan", "basic")).unwrap();
    let st = a.bundle_since(0, bv1.to_str().unwrap()).unwrap();
    let mut peer = b.open_named("fork_b");
    peer.import_bundle(bv1.to_str().unwrap()).unwrap();
    let mut v2a = fact_at("ns", "john", "plan", "pro", 1_800_000_000_000);
    if link_tip_a {
        v2a = v2a.related_to_link(&anchor.to_hex(), "mentions", 1.0);
    }
    let h2a = a.supersede(&v1, &mut v2a).unwrap();
    let mut v2b = fact_at("ns", "john", "plan", "enterprise", 1_800_000_000_500);
    let h2b = peer.supersede(&v1, &mut v2b).unwrap();
    peer.bundle_since(st.last_op_seq, bb.to_str().unwrap()).unwrap();
    a.import_bundle(bb.to_str().unwrap()).unwrap();
    assert_eq!(a.heads("ns", "john", "plan").unwrap().len(), 2, "setup: fork exists");
    (a, h2a, h2b)
}

pub fn concurrent_supersede_forks_then_merges(b: &dyn Backend) {
    let (mut a, h2a, h2b) = build_two_tip_fork(b, false);
    // both tips alive — nothing lost
    assert!(a.get(&h2a).is_ok() && a.get(&h2b).is_ok());
    // provisional head is deterministic: later created_at wins
    let head = a.latest("ns", "john", "plan").unwrap().unwrap();
    assert_eq!(head.get_str("object"), Some("enterprise"));
    // contested state visible: recall shows both current tips
    assert_eq!(a.recall("ns", "john", Some("plan"), 8).unwrap().len(), 2);

    // explicit merge closes the fork with both parents recorded
    let mut v3 = fact("ns", "john", "plan", "enterprise (migrated)");
    let h3 = a.merge_heads("ns", "john", "plan", &mut v3).unwrap();
    let heads = a.heads("ns", "john", "plan").unwrap();
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].0, h3);
    assert_eq!(a.recall("ns", "john", Some("plan"), 8).unwrap().len(), 1);
    let merged = a.get(&h3).unwrap();
    let parents = serde_json::to_string(&merged.fields["context"]["merge_parents"]).unwrap();
    assert!(
        parents.contains(&h2a.to_hex()) && parents.contains(&h2b.to_hex()),
        "merge records both parents: {parents}"
    );
}

pub fn provisional_head_election_is_deterministic(b: &dyn Backend) {
    // Same-created_at tips: the HASH tiebreak decides, identically everywhere.
    let bv1 = b.scratch().join("tie_v1.mgb");
    let bb = b.scratch().join("tie_b.mgb");
    let mut a = b.open_named("tie_a");
    let v1 = a.add(&fact("ns", "kai", "status", "s0")).unwrap();
    let st = a.bundle_since(0, bv1.to_str().unwrap()).unwrap();
    let mut peer = b.open_named("tie_b");
    peer.import_bundle(bv1.to_str().unwrap()).unwrap();
    let t = 1_800_000_100_000;
    let mut va = fact_at("ns", "kai", "status", "sa", t);
    let ha = a.supersede(&v1, &mut va).unwrap();
    let mut vb = fact_at("ns", "kai", "status", "sb", t);
    let hb = peer.supersede(&v1, &mut vb).unwrap();
    peer.bundle_since(st.last_op_seq, bb.to_str().unwrap()).unwrap();
    a.import_bundle(bb.to_str().unwrap()).unwrap();
    let expected = if ha.as_bytes() > hb.as_bytes() { ha } else { hb };
    let heads = a.heads("ns", "kai", "status").unwrap();
    assert_eq!(heads.len(), 2);
    assert_eq!(heads[0].0, expected, "provisional head = max (created_at, hash)");
}

pub fn same_supersede_replay_stays_idempotent(b: &dyn Backend) {
    let bl = b.scratch().join("replay.mgb");
    let mut a = b.open_named("replay_a");
    let v1 = a.add(&fact("ns", "john", "tier", "t1")).unwrap();
    let mut v2 = fact("ns", "john", "tier", "t2");
    a.supersede(&v1, &mut v2).unwrap();
    a.bundle_since(0, bl.to_str().unwrap()).unwrap();
    let mut rep = b.open_named("replay_b");
    rep.import_bundle(bl.to_str().unwrap()).unwrap();
    let second = rep.import_bundle(bl.to_str().unwrap()).unwrap();
    assert_eq!(second.applied, 0, "replay is a no-op");
    assert_eq!(rep.heads("ns", "john", "tier").unwrap().len(), 1, "no phantom fork");
}

pub fn open_forks_enumerates_and_clears_on_merge(b: &dyn Backend) {
    let (mut a, _h2a, _h2b) = build_two_tip_fork(b, false);
    let forks = a.open_forks().unwrap();
    assert_eq!(forks.len(), 1, "exactly one open fork");
    assert_eq!(forks[0].subject, "john");
    assert_eq!(forks[0].relation, "plan");
    assert_eq!(forks[0].heads.len(), 2, "both tips reported");
    let mut v3 = fact("ns", "john", "plan", "enterprise");
    a.merge_heads("ns", "john", "plan", &mut v3).unwrap();
    assert!(a.open_forks().unwrap().is_empty(), "merge clears the fork");
}

pub fn fork_then_forget_one_tip_resolves_fork(b: &dyn Backend) {
    let (mut a, _h2a, h2b) = build_two_tip_fork(b, false);
    a.forget(&h2b).unwrap();
    assert_eq!(a.recall("ns", "john", Some("plan"), 8).unwrap().len(), 1);
    assert_eq!(a.heads("ns", "john", "plan").unwrap().len(), 1, "heads drop to 1");
    assert!(a.open_forks().unwrap().is_empty(), "fork resolved");
    let (hh, _) = a.heads("ns", "john", "plan").unwrap()[0];
    assert!(a.get(&hh).is_ok());
}

pub fn merge_requires_an_open_fork(b: &dyn Backend) {
    let mut m = b.open();
    m.add(&fact("ns", "solo", "state", "v1")).unwrap();
    let mut v2 = fact("ns", "solo", "state", "merged");
    assert!(
        m.merge_heads("ns", "solo", "state", &mut v2).is_err(),
        "merge with a single head must error"
    );
}

pub fn forget_tip_reelection_ignores_link_rows(b: &dyn Backend) {
    let (mut a, h2a, h2b) = build_two_tip_fork(b, true);
    a.forget(&h2b).unwrap();
    assert_eq!(a.latest("ns", "john", "plan").unwrap().unwrap().get_str("object"), Some("pro"));
    let (h, inserted) = a.add_if_novel(&fact("ns", "john", "plan", "pro")).unwrap();
    assert!(!inserted, "value probe must match the re-elected head's true object");
    assert_eq!(h, h2a);
}

pub fn supersede_changed_key_reelection_ignores_link_rows(b: &dyn Backend) {
    let (mut a, h2a, h2b) = build_two_tip_fork(b, true);
    let mut moved = fact("ns", "john", "tier", "gold");
    a.supersede(&h2b, &mut moved).unwrap();
    assert_eq!(a.latest("ns", "john", "plan").unwrap().unwrap().get_str("object"), Some("pro"));
    let (h, inserted) = a.add_if_novel(&fact("ns", "john", "plan", "pro")).unwrap();
    assert!(!inserted, "value probe must match the re-elected head's true object");
    assert_eq!(h, h2a);
}

pub fn supersede_changed_relation_reconciles_old_key(b: &dyn Backend) {
    let mut m = b.open();
    let h1 = m.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
    let mut newg = fact("ns", "alice", "mood", "happy");
    m.supersede(&h1, &mut newg).unwrap();
    assert!(m.recall("ns", "alice", Some("lives_in"), 8).unwrap().is_empty());
    assert!(m.latest("ns", "alice", "lives_in").unwrap().is_none(), "old key must not surface the superseded grain");
    assert!(m.heads("ns", "alice", "lives_in").unwrap().is_empty(), "old key must have no open head");
    assert_eq!(m.latest("ns", "alice", "mood").unwrap().unwrap().get_str("object"), Some("happy"));
}
