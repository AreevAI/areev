//! supersede × forget interactions (the combination checklist's first row).

use crate::{fact, Backend};
use areev_core::error::AreevError;

pub fn supersede_returns_only_head(b: &dyn Backend) {
    let mut m = b.open();
    let h1 = m.add(&fact("ns", "x", "state", "v1")).unwrap();
    let mut v2 = fact("ns", "x", "state", "v2");
    let h2 = m.supersede(&h1, &mut v2).unwrap();
    let got = m.recall("ns", "x", Some("state"), 8).unwrap();
    assert_eq!(got.len(), 1, "recall returns heads only");
    assert_eq!(got[0].get_str("object"), Some("v2"));
    assert_eq!(m.latest("ns", "x", "state").unwrap().unwrap().get_str("object"), Some("v2"));
    // both blobs remain readable (immutability)
    assert!(m.get(&h1).is_ok() && m.get(&h2).is_ok());
    assert_eq!(m.count().unwrap(), 2);
    // history walks new -> old
    let hist = m.history("ns", "x", "state").unwrap();
    assert_eq!(hist.len(), 2);
}

pub fn forget_clears_head_row(b: &dyn Backend) {
    let mut m = b.open();
    let h = m.add(&fact("caller", "alice", "prefers", "tea")).unwrap();
    assert_eq!(m.heads("caller", "alice", "prefers").unwrap().len(), 1);
    m.forget(&h).unwrap();
    assert!(m.heads("caller", "alice", "prefers").unwrap().is_empty(), "forgotten head must not linger");
    assert!(m.open_forks().unwrap().is_empty());
    assert!(m.recall("caller", "alice", Some("prefers"), 8).unwrap().is_empty());
    assert!(m.get(&h).is_err(), "forgotten grain is gone");
}

pub fn forget_new_head_does_not_resurrect_old(b: &dyn Backend) {
    let mut m = b.open();
    let h1 = m.add(&fact("ns", "x", "state", "v1")).unwrap();
    let mut v2 = fact("ns", "x", "state", "v2");
    let h2 = m.supersede(&h1, &mut v2).unwrap();
    m.forget(&h2).unwrap();
    assert!(m.heads("ns", "x", "state").unwrap().is_empty(), "heads must not list the forgotten new head");
    // the superseded old version stays superseded — no silent resurrection
    assert!(m.recall("ns", "x", Some("state"), 8).unwrap().is_empty());
    assert!(m.latest("ns", "x", "state").unwrap().is_none());
    assert!(m.get(&h1).is_ok(), "old blob is still readable, just not live");
}

pub fn forget_superseded_old_keeps_new_head(b: &dyn Backend) {
    let mut m = b.open();
    let h1 = m.add(&fact("ns", "x", "state", "v1")).unwrap();
    let mut v2 = fact("ns", "x", "state", "v2");
    let h2 = m.supersede(&h1, &mut v2).unwrap();
    m.forget(&h1).unwrap();
    assert!(m.get(&h1).is_err());
    assert_eq!(m.latest("ns", "x", "state").unwrap().unwrap().get_str("object"), Some("v2"));
    assert_eq!(m.heads("ns", "x", "state").unwrap().len(), 1);
    assert_eq!(m.heads("ns", "x", "state").unwrap()[0].0, h2);
    // chain integrity: history tolerates the missing link (stops, no error)
    let hist = m.history("ns", "x", "state").unwrap();
    assert_eq!(hist.len(), 1, "history stops at the tombstoned link");
}

pub fn double_supersede_is_a_local_conflict(b: &dyn Backend) {
    let mut m = b.open();
    let h1 = m.add(&fact("ns", "x", "state", "v1")).unwrap();
    let mut v2 = fact("ns", "x", "state", "v2");
    m.supersede(&h1, &mut v2).unwrap();
    let mut v3 = fact("ns", "x", "state", "v3");
    let err = m.supersede(&h1, &mut v3).unwrap_err();
    assert!(
        matches!(err, AreevError::SupersessionConflict(_)),
        "second local supersede of the same head must conflict, got {err:?}"
    );
}

pub fn forget_missing_grain_is_not_found(b: &dyn Backend) {
    let mut m = b.open();
    let h = m.add(&fact("ns", "x", "state", "v1")).unwrap();
    m.forget(&h).unwrap();
    let err = m.forget(&h).unwrap_err();
    assert!(matches!(err, AreevError::NotFound(_)), "double forget is NotFound, got {err:?}");
}

pub fn add_if_novel_dedupes_current_value(b: &dyn Backend) {
    let mut m = b.open();
    let h1 = m.add(&fact("ns", "john", "plan", "pro")).unwrap();
    let (h, inserted) = m.add_if_novel(&fact("ns", "john", "plan", "pro")).unwrap();
    assert!(!inserted, "identical current value must dedupe");
    assert_eq!(h, h1);
    assert_eq!(m.count().unwrap(), 1);
    // a different value for the same key IS novel
    let (_h2, inserted2) = m.add_if_novel(&fact("ns", "john", "plan", "max")).unwrap();
    assert!(inserted2);
}

/// `supersession_chain` — the #128 trigger-state-keying primitive — must
/// walk identically on both backends: head-first, root-last, a single-hop
/// chain for a never-superseded grain (the case the fix's no-op guarantee
/// rests on).
pub fn supersession_chain_walks_to_the_first_grain(b: &dyn Backend) {
    let mut m = b.open();
    let h1 = m.add(&fact("ns", "x", "state", "v1")).unwrap();
    assert_eq!(m.supersession_chain(&h1).unwrap(), vec![h1], "never-superseded: chain is itself alone");

    let mut v2 = fact("ns", "x", "state", "v2");
    let h2 = m.supersede(&h1, &mut v2).unwrap();
    let mut v3 = fact("ns", "x", "state", "v3");
    let h3 = m.supersede(&h2, &mut v3).unwrap();

    assert_eq!(m.supersession_chain(&h3).unwrap(), vec![h3, h2, h1], "head-first, root-last");
    assert_eq!(m.supersession_chain(&h2).unwrap(), vec![h2, h1]);
    assert_eq!(m.supersession_chain(&h1).unwrap(), vec![h1]);
}

pub fn reasserting_superseded_value_is_novel(b: &dyn Backend) {
    // Novelty is by CURRENT head value, not full history: re-asserting a
    // superseded old value counts as novel (documented, deliberate).
    let mut m = b.open();
    let h1 = m.add(&fact("ns", "john", "plan", "pro")).unwrap();
    let mut v2 = fact("ns", "john", "plan", "max");
    m.supersede(&h1, &mut v2).unwrap();
    let (_h, inserted) = m.add_if_novel(&fact("ns", "john", "plan", "pro")).unwrap();
    assert!(inserted, "a superseded old value re-asserted is novel again");
}
