//! `trg:` evaluation state and the `meta_cas` arbitration primitive.
//!
//! The claim protocol's correctness rests on two properties: exactly one caller
//! wins a contested claim, and a holder whose lease expired cannot write back
//! behind the new owner. Both are exercised here, including under real threads.

use areev_store::{Areev, TriggerState};
use std::sync::{Arc, Barrier, Mutex};

fn store(dir: &tempfile::TempDir, name: &str) -> Areev {
    Areev::open(dir.path().join(name).to_str().unwrap()).unwrap()
}

#[test]
fn claim_if_absent_admits_exactly_one() {
    let dir = tempfile::TempDir::new().unwrap();
    let m = store(&dir, "a.db");

    assert!(m.meta_cas("trg:x", None, "first").unwrap(), "the first claim wins");
    assert!(!m.meta_cas("trg:x", None, "second").unwrap(), "a claim on a held row loses");
    assert_eq!(m.meta_get("trg:x").unwrap().as_deref(), Some("first"));
}

#[test]
fn swap_requires_the_exact_prior_value() {
    let dir = tempfile::TempDir::new().unwrap();
    let m = store(&dir, "b.db");
    m.meta_cas("trg:x", None, "v1").unwrap();

    assert!(!m.meta_cas("trg:x", Some("wrong"), "v2").unwrap(), "a stale expectation loses");
    assert_eq!(m.meta_get("trg:x").unwrap().as_deref(), Some("v1"), "and changes nothing");

    assert!(m.meta_cas("trg:x", Some("v1"), "v2").unwrap());
    assert_eq!(m.meta_get("trg:x").unwrap().as_deref(), Some("v2"));
}

#[test]
fn a_zombie_cannot_write_behind_the_new_owner() {
    // The scenario the fence exists for: node A claims, stalls past its lease,
    // node B takes over, and then A wakes up and tries to release. A's write
    // must not land — and it must not land *without* a fence column, because
    // the fence rides inside the compared value.
    let dir = tempfile::TempDir::new().unwrap();
    let m = store(&dir, "c.db");

    let mut st = TriggerState { fence: 1, claimed_by: Some("A".into()), ..Default::default() };
    m.put_trigger_state("t1", None, &st).unwrap();
    let (_, a_saw) = m.trigger_state("t1").unwrap().unwrap();

    // B takes over: reads, bumps the fence, swaps against what it read.
    let (mut b_state, b_saw) = m.trigger_state("t1").unwrap().unwrap();
    b_state.fence = 2;
    b_state.claimed_by = Some("B".into());
    assert!(m.put_trigger_state("t1", Some(&b_saw), &b_state).unwrap(), "B takes the lease");

    // A wakes and releases against the row it remembers.
    st.claimed_by = None;
    st.last_fired_at = Some(999);
    assert!(
        !m.put_trigger_state("t1", Some(&a_saw), &st).unwrap(),
        "the zombie's release must be refused"
    );

    let (after, _) = m.trigger_state("t1").unwrap().unwrap();
    assert_eq!(after.fence, 2, "B's fence stands");
    assert_eq!(after.claimed_by.as_deref(), Some("B"));
    assert_eq!(after.last_fired_at, None, "A wrote nothing");
}

#[test]
fn concurrent_claims_produce_exactly_one_winner() {
    // Eight threads on one handle. On the embedded backend the single writer
    // makes this trivially safe, which is the point: the same code path is
    // correct on both tiers rather than there being two implementations.
    let dir = tempfile::TempDir::new().unwrap();
    let m = Arc::new(Mutex::new(store(&dir, "d.db")));
    let barrier = Arc::new(Barrier::new(8));
    let wins = Arc::new(Mutex::new(0u32));

    std::thread::scope(|s| {
        for node in 0..8 {
            let (m, barrier, wins) = (Arc::clone(&m), Arc::clone(&barrier), Arc::clone(&wins));
            s.spawn(move || {
                barrier.wait();
                let won = {
                    let guard = m.lock().unwrap();
                    guard.meta_cas("trg:race", None, &format!("node-{node}")).unwrap()
                };
                if won {
                    *wins.lock().unwrap() += 1;
                }
            });
        }
    });

    assert_eq!(*wins.lock().unwrap(), 1, "exactly one claim may succeed");
}

#[test]
fn state_round_trips_and_omits_its_defaults() {
    let dir = tempfile::TempDir::new().unwrap();
    let m = store(&dir, "e.db");

    let st = TriggerState {
        next_due_at: Some(1_755_600_000_000),
        cursor: Some("1802611".into()),
        fence: 7,
        consecutive_failures: 2,
        last_error: Some("connector exited 1".into()),
        ..Default::default()
    };
    m.put_trigger_state("t2", None, &st).unwrap();

    let (back, raw) = m.trigger_state("t2").unwrap().unwrap();
    assert_eq!(back, st);
    assert!(!raw.contains("last_fired_at"), "absent options stay off the row");
    assert!(!raw.contains("claimed_by"));
}

#[test]
fn an_unreadable_state_row_refuses_rather_than_reading_as_never_fired() {
    // Reading a corrupt row as "no state" would re-fire everything, and for a
    // polling trigger that means reprocessing the entire backlog.
    let dir = tempfile::TempDir::new().unwrap();
    let m = store(&dir, "f.db");
    m.meta_put("trg:broken", "{not json").unwrap();

    assert!(m.trigger_state("broken").is_err());
    assert!(m.trigger_states().is_err());
}

#[test]
fn state_alone_answers_only_half_the_due_question() {
    // `permits_firing` is the half that needs no declaration. Whether an absent
    // `next_due_at` means "fire now" depends on the trigger's kind, which is
    // why the complete check lives in `areev_trigger::schedule::is_due` — an
    // interval fires at once, a cron waits for its boundary.
    let fresh = TriggerState::default();
    assert!(fresh.permits_firing(0));
    assert!(!fresh.leased(0));

    let paused = TriggerState { paused: true, ..Default::default() };
    assert!(!paused.permits_firing(i64::MAX));

    // A one-shot past its instant. Without this flag, "no next instant" and
    // "no baseline yet" are the same value and the trigger re-fires forever.
    let spent = TriggerState { exhausted: true, ..Default::default() };
    assert!(!spent.permits_firing(i64::MAX), "an exhausted trigger never fires again");

    let later = TriggerState { next_due_at: Some(100), ..Default::default() };
    assert!(!later.permits_firing(99));
    assert!(later.permits_firing(100), "due is inclusive at the instant");
}

#[test]
fn trigger_state_never_rides_a_bundle() {
    // The declaration replicates; the cursor must not. A dev memory restored
    // from prod that inherited prod's Gmail cursor would silently skip real
    // mail while reporting success.
    let dir = tempfile::TempDir::new().unwrap();
    let mut src = store(&dir, "src.db");
    src.put_trigger_state(
        "t3",
        None,
        &TriggerState { cursor: Some("prod-cursor".into()), ..Default::default() },
    )
    .unwrap();
    src.meta_put("qry:shared", r#"{"body":"RECALL facts","updated_at":5}"#).unwrap();

    let bundle = dir.path().join("b.mgb");
    src.bundle_since(0, bundle.to_str().unwrap()).unwrap();

    let mut dst = store(&dir, "dst.db");
    dst.import_bundle(bundle.to_str().unwrap()).unwrap();

    assert!(
        dst.trigger_state("t3").unwrap().is_none(),
        "trg: rows must not replicate"
    );
    assert!(
        dst.meta_get("qry:shared").is_ok_and(|v| v.is_some()),
        "…while the registry rows that are supposed to replicate still do"
    );
}
