//! Op-log / bundle replication contract: multi-hop convergence, tombstone
//! replay, idempotency, and HLC-bounded point-in-time restore.

use crate::{fact, Backend};
use areev_store::{OP_ADD, OP_FORGET, OP_SUPERSEDE};

pub fn supersede_two_hop_replication_converges(b: &dyn Backend) {
    let mut a = b.open_named("hop_a");
    let h = a.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
    let mut munich = fact("ns", "alice", "lives_in", "Munich");
    a.supersede(&h, &mut munich).unwrap();
    let ab = b.scratch().join("hop_ab.mgb");
    a.bundle_since(0, ab.to_str().unwrap()).unwrap();
    let mut mid = b.open_named("hop_b");
    mid.import_bundle(ab.to_str().unwrap()).unwrap();
    assert_eq!(mid.recall("ns", "alice", Some("lives_in"), 16).unwrap().len(), 1);
    // B's op-log must carry the supersession for onward replication.
    assert!(
        mid.changes_since(0, 100).unwrap().iter().any(|o| o.op == OP_SUPERSEDE),
        "replica op-log missing OP_SUPERSEDE"
    );
    let bc = b.scratch().join("hop_bc.mgb");
    mid.bundle_since(0, bc.to_str().unwrap()).unwrap();
    let mut c = b.open_named("hop_c");
    c.import_bundle(bc.to_str().unwrap()).unwrap();
    assert_eq!(
        c.recall("ns", "alice", Some("lives_in"), 16).unwrap().len(),
        1,
        "two-hop supersede must not fork on C"
    );
    assert_eq!(c.latest("ns", "alice", "lives_in").unwrap().unwrap().get_str("object"), Some("Munich"));
}

pub fn merge_replicates_as_fork_closure(b: &dyn Backend) {
    let (mut a, _h2a, _h2b) = super::heads_forks::build_two_tip_fork(b, false);
    let mut v3 = fact("ns", "john", "plan", "enterprise (merged)");
    a.merge_heads("ns", "john", "plan", &mut v3).unwrap();
    let full = b.scratch().join("closure_full.mgb");
    a.bundle_since(0, full.to_str().unwrap()).unwrap();
    let mut peer = b.open_named("closure_peer");
    peer.import_bundle(full.to_str().unwrap()).unwrap();
    assert_eq!(peer.heads("ns", "john", "plan").unwrap().len(), 1, "merge must close the fork on the peer");
    assert_eq!(peer.recall("ns", "john", Some("plan"), 8).unwrap().len(), 1);
}

pub fn merge_heads_closure_logged(b: &dyn Backend) {
    let (mut a, _h2a, _h2b) = super::heads_forks::build_two_tip_fork(b, false);
    let before = a.changes_since(0, 10_000).unwrap().len();
    let mut v3 = fact("ns", "john", "plan", "enterprise (merged)");
    a.merge_heads("ns", "john", "plan", &mut v3).unwrap();
    let after = a.changes_since(0, 10_000).unwrap();
    let new_ops: Vec<i64> = after[before..].iter().map(|o| o.op).collect();
    assert!(new_ops.contains(&OP_SUPERSEDE), "merge must log OP_SUPERSEDE: {new_ops:?}");
}

pub fn forget_replicates_as_tombstone(b: &dyn Backend) {
    // Path 1 — live replica: the peer already has the grain, then receives
    // the tombstone as a delta. The forget applies and is re-logged for
    // onward (2-hop) replication.
    let mut a = b.open_named("tomb_a");
    let keep = a.add(&fact("ns", "kim", "keeps", "this")).unwrap();
    let gone = a.add(&fact("ns", "kim", "drops", "this")).unwrap();
    let b1 = b.scratch().join("tomb_adds.mgb");
    let st = a.bundle_since(0, b1.to_str().unwrap()).unwrap();
    let mut peer = b.open_named("tomb_b");
    peer.import_bundle(b1.to_str().unwrap()).unwrap();
    assert_eq!(peer.count().unwrap(), 2);
    a.forget(&gone).unwrap();
    let b2 = b.scratch().join("tomb_delta.mgb");
    a.bundle_since(st.last_op_seq, b2.to_str().unwrap()).unwrap();
    peer.import_bundle(b2.to_str().unwrap()).unwrap();
    assert!(peer.get(&keep).is_ok());
    assert!(peer.get(&gone).is_err(), "tombstone must replay — forgotten grain absent on peer");
    assert_eq!(peer.count().unwrap(), 1);
    let second = peer.import_bundle(b2.to_str().unwrap()).unwrap();
    assert_eq!(second.applied, 0, "tombstone replay is idempotent");
    assert!(
        peer.changes_since(0, 100).unwrap().iter().any(|o| o.op == OP_FORGET),
        "peer op-log must carry the tombstone onward"
    );

    // Path 2 — cold replica: a full bundle where the forgotten grain's ADD
    // exports with an empty blob and the tombstone follows. Net-equivalent:
    // the grain simply never materializes.
    let full = b.scratch().join("tomb_full.mgb");
    a.bundle_since(0, full.to_str().unwrap()).unwrap();
    let mut cold = b.open_named("tomb_cold");
    cold.import_bundle(full.to_str().unwrap()).unwrap();
    assert!(cold.get(&keep).is_ok());
    assert!(cold.get(&gone).is_err());
    assert_eq!(cold.count().unwrap(), 1);
}

pub fn changes_since_cursor_pages_in_order(b: &dyn Backend) {
    let mut m = b.open();
    for i in 0..5 {
        m.add(&fact("ns", "s", &format!("r{i}"), "o")).unwrap();
    }
    let all = m.changes_since(0, 100).unwrap();
    assert_eq!(all.len(), 5);
    assert!(all.iter().all(|o| o.op == OP_ADD));
    // strictly ascending op_seq
    for w in all.windows(2) {
        assert!(w[0].op_seq < w[1].op_seq);
    }
    // cursor resumes exactly after the given op_seq
    let tail = m.changes_since(all[2].op_seq, 100).unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].op_seq, all[3].op_seq);
    // limit caps the page
    assert_eq!(m.changes_since(0, 2).unwrap().len(), 2);
}

pub fn pitr_max_hlc_cutoff_is_inclusive(b: &dyn Backend) {
    let mut a = b.open_named("pitr_a");
    let h1 = a.add(&fact("ns", "doc", "rev", "one")).unwrap();
    let mut v2 = fact("ns", "doc", "rev", "two");
    a.supersede(&h1, &mut v2).unwrap();
    let ops = a.changes_since(0, 100).unwrap();
    assert_eq!(ops.len(), 3, "ADD + (ADD, SUPERSEDE)");
    let bl = b.scratch().join("pitr.mgb");
    a.bundle_since(0, bl.to_str().unwrap()).unwrap();

    // Cut at the first op's HLC: only v1 exists, and the cutoff op itself
    // is INCLUDED (the filter drops hlc > max_hlc, not >=).
    let mut early = b.open_named("pitr_early");
    early.import_bundle_until(bl.to_str().unwrap(), Some(ops[0].hlc)).unwrap();
    assert_eq!(early.count().unwrap(), 1);
    assert_eq!(early.latest("ns", "doc", "rev").unwrap().unwrap().get_str("object"), Some("one"));

    // Cut at the last op's HLC: full state.
    let mut full = b.open_named("pitr_full");
    full.import_bundle_until(bl.to_str().unwrap(), Some(ops.last().unwrap().hlc)).unwrap();
    assert_eq!(full.count().unwrap(), 2);
    assert_eq!(full.latest("ns", "doc", "rev").unwrap().unwrap().get_str("object"), Some("two"));
}

/// An imported grain must be free-text recallable on the importing handle,
/// without a reopen. Every other import case reopens before searching, which is
/// how the missing BM25 postings stayed hidden behind `finish_open`'s self-heal.
pub fn imported_grains_are_text_searchable_without_reopen(b: &dyn Backend) {
    let bundle = {
        let mut src = b.open_named("txt_src");
        src.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
        src.add(&fact("caller", "bob", "prefers", "aisle seat")).unwrap();
        let path = std::env::temp_dir().join(format!("conf_txt_{}.mgb", std::process::id()));
        src.bundle_since(0, path.to_str().unwrap()).unwrap();
        path
    };

    let mut dst = b.open_named("txt_dst");
    let applied = dst.import_bundle(bundle.to_str().unwrap()).unwrap().applied;
    assert!(applied > 0, "[{}] fixture must import something", b.name());

    let hits = dst.search_text("caller", "window", 8).unwrap();
    assert!(
        !hits.is_empty(),
        "[{}] an imported grain was not text-searchable on the importing handle — \
         BM25 postings were not written by the import path",
        b.name()
    );

    let scoped = dst.recall_hybrid("caller", None, None, Some("window seat"), 8, None).unwrap();
    assert!(
        !scoped.is_empty(),
        "[{}] hybrid recall found nothing for imported text",
        b.name()
    );

    let _ = std::fs::remove_file(&bundle);
}
