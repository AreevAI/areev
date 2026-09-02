//! Recall must not mutate belief: ranking by how often a memory is retrieved
//! makes a popular error indistinguishable from a settled fact. A `last_seen`
//! stamp, an access counter, or a confidence nudge added later fails here.

use crate::{fact, Backend};
use areev_store::Direction;

fn snapshot(m: &mut areev_store::Areev, ns: &str, subject: &str) -> Vec<String> {
    let mut rows: Vec<String> = m
        .recall(ns, subject, None, 256)
        .unwrap()
        .iter()
        .map(|g| {
            format!(
                "{}|{}|{}|{}|{}|{}|{}",
                g.hash.to_hex(),
                g.get_f64("confidence").map(f64::to_bits).unwrap_or(0),
                g.get_str("verification_status").unwrap_or("-"),
                g.get_i64("success_count").unwrap_or(-1),
                g.get_i64("failure_count").unwrap_or(-1),
                g.get_i64("valid_from").unwrap_or(-1),
                g.get_i64("valid_to").unwrap_or(-1),
            )
        })
        .collect();
    rows.sort();
    rows
}

pub fn recall_never_mutates_belief(b: &dyn Backend) {
    let mut m = b.open_named("purity");
    m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    m.add(&fact("caller", "alice", "lives_in", "Berlin")).unwrap();
    m.add(&fact("caller", "bob", "prefers", "aisle seat")).unwrap();

    let before = snapshot(&mut m, "caller", "alice");
    let ops_before = m.changes_since(0, usize::MAX / 2).unwrap().len();
    assert!(!before.is_empty(), "[{}] fixture must produce grains", b.name());

    for _ in 0..8 {
        let _ = m.recall("caller", "alice", None, 64).unwrap();
        let _ = m.recall_hybrid("caller", Some("alice"), None, None, 64, None).unwrap();
        let _ = m.search_text("caller", "window", 32);
        let _ = m.related("caller", "alice", &[], Direction::Both, 2, 32);
        let _ = m.history("caller", "alice", "prefers");
        let _ = m.subject_report("caller", "alice");
        let _ = m.latest("caller", "alice", "prefers");
    }

    let after = snapshot(&mut m, "caller", "alice");
    let ops_after = m.changes_since(0, usize::MAX / 2).unwrap().len();

    assert_eq!(
        before,
        after,
        "[{}] a read path mutated a grain field — recall must not change belief",
        b.name()
    );
    assert_eq!(
        ops_before,
        ops_after,
        "[{}] a read path appended to the op log — reads must write nothing",
        b.name()
    );
}

/// A mutation buffered in memory and flushed on drop passes the check above and
/// still corrupts the file, so the snapshot has to survive a round trip.
pub fn recall_writes_nothing_that_survives_reopen(b: &dyn Backend) {
    let before = {
        let mut m = b.open_named("purity_reopen");
        m.add(&fact("caller", "carol", "prefers", "quiet room")).unwrap();
        m.add(&fact("caller", "carol", "speaks", "German")).unwrap();
        let snap = snapshot(&mut m, "caller", "carol");
        for _ in 0..8 {
            let _ = m.recall("caller", "carol", None, 64).unwrap();
            let _ = m.recall_hybrid("caller", Some("carol"), None, None, 64, None).unwrap();
        }
        snap
    };

    let mut m = b.open_named("purity_reopen");
    let after = snapshot(&mut m, "caller", "carol");
    assert_eq!(
        before,
        after,
        "[{}] a read-time write survived close/reopen",
        b.name()
    );
}

/// `subject_report` shares one selector with erasure, so filtering retraction
/// inside the store would make a DSAR under-disclose. Withholding belongs to
/// `areev-context`; the store keeps returning retracted grains.
pub fn store_recall_still_returns_retracted_grains(b: &dyn Backend) {
    let mut m = b.open_named("retracted_store");
    let mut f = fact("caller", "alice", "prefers", "window seat");
    f.common.verification_status = Some("retracted".to_string());
    m.add(&f).unwrap();
    m.add(&fact("caller", "alice", "lives_in", "Berlin")).unwrap();

    let got = m.recall("caller", "alice", None, 16).unwrap();
    assert_eq!(
        got.len(),
        2,
        "[{}] store recall must not filter retracted grains — the DSAR selector depends on it",
        b.name()
    );
    assert!(
        got.iter().any(|g| g.get_str("verification_status") == Some("retracted")),
        "[{}] the retracted grain must be reachable from the store",
        b.name()
    );

    let report = m.subject_report("caller", "alice").unwrap();
    assert!(
        format!("{report:?}").contains("retracted"),
        "[{}] a DSAR must disclose a retracted grain, not hide it",
        b.name()
    );
}
