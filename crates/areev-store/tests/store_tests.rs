//! areev-store integration tests.
//!
//! The first test IS the M1 exit criterion: the vaais operation profile
//! (create memory, structural recall, batch-add, supersede, forget)
//! running in-process.

use areev_core::types::{Event, Fact, Grain, GrainType};
use areev_store::{Axis, Direction, Areev, OP_ADD, OP_FORGET, OP_SUPERSEDE};
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("caller.db");
    let m = Areev::open(path.to_str().unwrap()).unwrap();
    (m, dir)
}

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9).source_type("user_explicit");
    f.common.namespace = Some(ns.to_string());
    f
}

#[test]
fn vaais_operation_profile() {
    let (mut m, _d) = open_mem();

    // 1. create memory + add
    let h1 = m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    let _h2 = m.add(&fact("caller", "alice", "lives_in", "Berlin")).unwrap();

    // 2. structural recall
    let got = m.recall("caller", "alice", None, 16).unwrap();
    assert_eq!(got.len(), 2);
    let got = m.recall("caller", "alice", Some("prefers"), 16).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].get_str("object"), Some("window seat"));

    // 3. batch-add
    let f1 = fact("caller", "alice", "allergic_to", "peanuts");
    let f2 = fact("caller", "alice", "speaks", "German");
    let f3 = fact("caller", "bob", "prefers", "aisle seat");
    let hashes = m.add_batch(&[&f1, &f2, &f3]).unwrap();
    assert_eq!(hashes.len(), 3);
    assert_eq!(m.recall("caller", "alice", None, 16).unwrap().len(), 4);

    // 4. supersede — the user moved cities
    let mut newer = fact("caller", "alice", "lives_in", "Munich");
    let h_new = m.supersede(&_h2, &mut newer).unwrap();
    let head = m.latest("caller", "alice", "lives_in").unwrap().unwrap();
    assert_eq!(head.get_str("object"), Some("Munich"));
    assert_eq!(head.hash, h_new);
    // old version excluded from current recall
    let cur = m.recall("caller", "alice", Some("lives_in"), 16).unwrap();
    assert_eq!(cur.len(), 1);
    assert_eq!(cur[0].get_str("object"), Some("Munich"));
    // old blob still readable by hash (immutability), marked via provenance
    let old = m.get(&_h2).unwrap();
    assert_eq!(old.get_str("object"), Some("Berlin"));
    assert_eq!(m.get(&h_new).unwrap().get_str("derived_from"), Some(_h2.to_hex().as_str()));

    // 5. forget
    m.forget(&h1).unwrap();
    assert!(m.get(&h1).is_err());
    let after = m.recall("caller", "alice", Some("prefers"), 16).unwrap();
    assert_eq!(after.len(), 0);

    // op-log recorded everything with tombstone
    let ops = m.changes_since(0, 100).unwrap();
    let kinds: Vec<i64> = ops.iter().map(|o| o.op).collect();
    assert!(kinds.contains(&OP_ADD));
    assert!(kinds.contains(&OP_SUPERSEDE));
    assert!(kinds.contains(&OP_FORGET));
    // HLCs strictly increase
    assert!(ops.windows(2).all(|w| w[0].hlc < w[1].hlc));
}

#[test]
fn double_supersede_conflicts() {
    let (mut m, _d) = open_mem();
    let h = m.add(&fact("ns", "x", "state", "v1")).unwrap();
    let mut a = fact("ns", "x", "state", "v2");
    m.supersede(&h, &mut a).unwrap();
    let mut b = fact("ns", "x", "state", "v3");
    assert!(m.supersede(&h, &mut b).is_err());
}

#[test]
fn add_if_novel_collapses_repeat_value() {
    let (mut m, _d) = open_mem();
    // First add of a value is novel and writes a grain.
    let (h1, ins1) = m.add_if_novel(&fact("ns", "x", "prefers", "tea")).unwrap();
    assert!(ins1, "first add is novel");
    // Re-adding the exact same value is a no-op: existing hash, nothing written.
    let (h2, ins2) = m.add_if_novel(&fact("ns", "x", "prefers", "tea")).unwrap();
    assert!(!ins2, "re-add of the current value collapses");
    assert_eq!(h1, h2, "returns the existing head's hash");
    assert_eq!(
        m.recall("ns", "x", Some("prefers"), 10).unwrap().len(),
        1,
        "no duplicate grain was written"
    );

    // A different object is a genuine new value and inserts.
    let (_h3, ins3) = m.add_if_novel(&fact("ns", "x", "prefers", "coffee")).unwrap();
    assert!(ins3, "a different object is novel");

    // Scope: idempotency keys on the *current* head only. Once the head moved
    // to "coffee", re-adding "tea" is novel again (it is not the current value).
    let (_h4, ins4) = m.add_if_novel(&fact("ns", "x", "prefers", "tea")).unwrap();
    assert!(ins4, "an old value is novel once it is no longer the head");
}

#[test]
fn grains_derived_from_finds_reverse_provenance() {
    let (mut m, _d) = open_mem();
    // An experience grain, then two lessons distilled from it and one unrelated.
    let mut obs = Event::new("session 41: flaky test fixed by isolating tempdir");
    obs.common.namespace = Some("agent".to_string());
    let src = m.add(&obs).unwrap();

    let mut l1 = fact("agent", "fix_flaky_tests", "lesson", "isolate the tempdir per test");
    l1.common.derived_from = Some(src.to_hex());
    let h1 = m.add(&l1).unwrap();
    let mut l2 = fact("agent", "fix_flaky_tests", "lesson", "rerunning alone never fixes it");
    l2.common.derived_from = Some(src.to_hex());
    m.add(&l2).unwrap();
    // Unrelated lesson, no derived_from → must not match.
    m.add(&fact("agent", "unrelated", "lesson", "something else")).unwrap();

    let kids = m.grains_derived_from(&src).unwrap();
    assert_eq!(kids.len(), 2, "exactly the two lessons distilled from the source");
    for g in &kids {
        assert_eq!(g.get_str("derived_from"), Some(src.to_hex().as_str()));
    }
    // A grain with no children returns empty, not an error.
    let none = m.grains_derived_from(&h1).unwrap();
    assert!(none.is_empty());
}

#[test]
fn thread_tail_returns_transcript_order() {
    let (mut m, _d) = open_mem();
    for i in 0..30 {
        let mut e = Event::new(&format!("turn {i}"));
        e.session_id = Some("call-42".to_string());
        e.common.namespace = Some("caller".to_string());
        m.add(&e).unwrap();
    }
    let tail = m.thread_tail("caller", "call-42", 20).unwrap();
    assert_eq!(tail.len(), 20);
    assert_eq!(tail[0].get_str("content"), Some("turn 10"));
    assert_eq!(tail[19].get_str("content"), Some("turn 29"));
}

#[test]
fn recent_in_session_beats_the_truncated_window() {
    // The defect this read exists to fix (#49): the session's turns are OLDER
    // than the page a namespace scan returns, so a post-filter over `recent`
    // finds none of them while the index-backed read finds all of them.
    let (mut m, _d) = open_mem();
    for i in 0..5 {
        let mut e = Event::new(&format!("call-7 turn {i}"));
        e.session_id = Some("call-7".to_string());
        e.common.namespace = Some("caller".to_string());
        m.add(&e).unwrap();
    }
    // 200 newer events on other sessions bury it.
    for i in 0..200 {
        let mut e = Event::new(&format!("noise {i}"));
        e.session_id = Some(format!("other-{i}"));
        e.common.namespace = Some("caller".to_string());
        m.add(&e).unwrap();
    }

    // What a post-filter over a 50-row page would have seen: nothing.
    let page = m.recent("caller", Some(GrainType::Event), 50).unwrap();
    let in_page = page
        .iter()
        .filter(|g| g.get_str("session_id") == Some("call-7"))
        .count();
    assert_eq!(in_page, 0, "the session must be outside the page for this test to mean anything");

    // The index-backed read returns the whole session regardless of page.
    let got = m
        .recent_in_session("caller", "call-7", Some(GrainType::Event), 20, true)
        .unwrap();
    assert_eq!(got.len(), 5);
    // Newest first, matching `recent`'s contract (thread_tail is the
    // oldest-first transcript view).
    assert_eq!(got[0].get_str("content"), Some("call-7 turn 4"));
    assert_eq!(got[4].get_str("content"), Some("call-7 turn 0"));
}

#[test]
fn recent_in_session_filters_type_and_unknown_session_is_empty() {
    let (mut m, _d) = open_mem();
    let mut e = Event::new("an event");
    e.session_id = Some("s1".to_string());
    e.common.namespace = Some("caller".to_string());
    m.add(&e).unwrap();

    // Narrowing to a type the session has none of returns empty, not the event.
    let none = m
        .recent_in_session("caller", "s1", Some(GrainType::Fact), 10, true)
        .unwrap();
    assert!(none.is_empty());
    // No type filter: the event comes back.
    let all = m.recent_in_session("caller", "s1", None, 10, true).unwrap();
    assert_eq!(all.len(), 1);
    // A session term nothing ever carried is an empty answer, not an error.
    let unknown = m
        .recent_in_session("caller", "never-used", None, 10, true)
        .unwrap();
    assert!(unknown.is_empty());
}

#[test]
fn graph_related_and_path() {
    let (mut m, _d) = open_mem();
    m.add(&fact("org", "alice", "reports_to", "bob")).unwrap();
    m.add(&fact("org", "bob", "reports_to", "carol")).unwrap();
    m.add(&fact("org", "carol", "reports_to", "dana")).unwrap();
    m.add(&fact("org", "alice", "prefers", "tea")).unwrap();

    let up2 = m
        .related("org", "alice", &["reports_to"], Direction::Out, 2, 100)
        .unwrap();
    assert_eq!(up2, vec!["bob".to_string(), "carol".to_string()]);

    // reverse traversal via selective OSP: who reports (transitively) to carol?
    let down2 = m
        .related("org", "carol", &["reports_to"], Direction::In, 2, 100)
        .unwrap();
    assert_eq!(down2, vec!["bob".to_string(), "alice".to_string()]);

    let p = m
        .path("org", "alice", "dana", &["reports_to"], 4)
        .unwrap()
        .unwrap();
    assert_eq!(p, vec!["alice", "bob", "carol", "dana"]);

    assert!(m.path("org", "dana", "alice", &["reports_to"], 4).unwrap().is_none());
}

#[test]
fn entity_at_knowledge_axis_walks_chain() {
    let (mut m, _d) = open_mem();
    // The sleeps below are load-bearing, but NOT for HLC ordering: `next_hlc`'s
    // in-memory counter already guarantees strictly-increasing HLCs (see
    // `vaais_operation_profile`, which asserts that with no sleep). Here the
    // Knowledge-axis `entity_at` walk compares each version's wall-clock
    // `created_at`/`svf` against the `now_ms_test()` as-of probes, so we must
    // space the versions and probes onto distinct milliseconds — otherwise all
    // three versions could share one ms and the `<= t` boundary is ambiguous.
    let h1 = m.add(&fact("ns", "acct", "balance", "100")).unwrap();
    let t_after_v1 = now_ms_test();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut v2 = fact("ns", "acct", "balance", "80");
    let h2 = m.supersede(&h1, &mut v2).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let t_after_v2 = now_ms_test();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut v3 = fact("ns", "acct", "balance", "230");
    let _h3 = m.supersede(&h2, &mut v3).unwrap();

    // now → v3
    let now_g = m.latest("ns", "acct", "balance").unwrap().unwrap();
    assert_eq!(now_g.get_str("object"), Some("230"));
    // knowledge as-of between v1 and v2 → v1
    let g = m
        .entity_at("ns", "acct", "balance", t_after_v1, Axis::Knowledge)
        .unwrap()
        .unwrap();
    assert_eq!(g.get_str("object"), Some("100"));
    // knowledge as-of after v2, before v3 → v2
    let g = m
        .entity_at("ns", "acct", "balance", t_after_v2, Axis::Knowledge)
        .unwrap()
        .unwrap();
    assert_eq!(g.get_str("object"), Some("80"));
}

#[test]
fn entity_at_world_axis_filters_validity() {
    let (mut m, _d) = open_mem();
    let mut f = fact("ns", "alice", "employer", "Acme");
    f.common.valid_from = Some(1_000);
    f.common.valid_to = Some(2_000);
    m.add(&f).unwrap();
    let mut g = fact("ns", "alice", "employer", "Globex");
    g.common.valid_from = Some(2_000);
    m.add(&g).unwrap();

    let at_1500 = m
        .entity_at("ns", "alice", "employer", 1_500, Axis::World)
        .unwrap()
        .unwrap();
    assert_eq!(at_1500.get_str("object"), Some("Acme"));
    let at_3000 = m
        .entity_at("ns", "alice", "employer", 3_000, Axis::World)
        .unwrap()
        .unwrap();
    assert_eq!(at_3000.get_str("object"), Some("Globex"));
    assert!(m
        .entity_at("ns", "alice", "employer", 500, Axis::World)
        .unwrap()
        .is_none());
}

#[test]
fn reopen_preserves_state_and_counters() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("mem.db");
    let path = path.to_str().unwrap();
    let h;
    {
        let mut m = Areev::open(path).unwrap();
        h = m.add(&fact("ns", "alice", "prefers", "tea")).unwrap();
        m.add(&fact("ns", "alice", "speaks", "German")).unwrap();
    }
    {
        let mut m = Areev::open(path).unwrap();
        assert_eq!(m.get(&h).unwrap().get_str("object"), Some("tea"));
        assert_eq!(m.recall("ns", "alice", None, 16).unwrap().len(), 2);
        // counters continue, no collisions
        let h3 = m.add(&fact("ns", "alice", "likes", "coffee")).unwrap();
        assert!(m.get(&h3).is_ok());
        let ops = m.changes_since(0, 100).unwrap();
        assert_eq!(ops.len(), 3);
        assert!(ops.windows(2).all(|w| w[0].op_seq < w[1].op_seq));
    }
}

fn now_ms_test() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ── Duplicate adds (#40) ────────────────────────────────────────────────────

/// Re-adding a byte-identical grain is a no-op, not `UNIQUE constraint failed:
/// grains.hash`.
///
/// `created_at` has millisecond resolution, so two identical events in the same
/// millisecond serialize to the same bytes and therefore the same content
/// address. An agent retrying a failing tool in a tight loop is exactly that
/// workload — and `record_tool_call` is the flagship analyzer's ingest path, so
/// the workaround was to corrupt the payload (jitter the result string) to
/// satisfy the store.
#[test]
fn adding_the_same_grain_twice_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();

    // Fixed created_at makes the two grains byte-identical without racing a
    // clock — same thing the same-millisecond case produces, deterministically.
    let grain = || {
        Fact::new("crm_lookup", "returned", "timeout after 30s")
            .namespace("caller")
            .created_at(1_700_000_000_000)
    };

    let first = m.add(&grain()).unwrap();
    let second = m.add(&grain()).expect("re-adding a stored grain must not error");
    assert_eq!(first, second, "the same content is the same address");
    assert_eq!(m.count().unwrap(), 1, "and it is stored exactly once");

    // A skipped duplicate must not consume a sequence number or emit an op-log
    // row — nothing changed, so nothing replicates.
    let ops = m.changes_since(0, 100).unwrap();
    assert_eq!(ops.len(), 1, "one add, one op-log record: {ops:?}");

    // Five in a row, the shape of the retry loop in the report.
    for _ in 0..5 {
        assert_eq!(m.add(&grain()).unwrap(), first);
    }
    assert_eq!(m.count().unwrap(), 1);

    // A genuinely different grain still writes.
    let other = m
        .add(&Fact::new("crm_lookup", "returned", "ok").namespace("caller").created_at(1_700_000_000_000))
        .unwrap();
    assert_ne!(other, first);
    assert_eq!(m.count().unwrap(), 2);
}

/// The same rule inside one batch, where the `has` probe cannot help: neither
/// copy is committed yet.
#[test]
fn a_batch_containing_the_same_grain_twice_writes_it_once() {
    let dir = TempDir::new().unwrap();
    let mut m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();

    let a = Fact::new("john", "prefers", "tea").namespace("caller").created_at(1_700_000_000_000);
    let dup = Fact::new("john", "prefers", "tea").namespace("caller").created_at(1_700_000_000_000);
    let b = Fact::new("jane", "prefers", "coffee").namespace("caller").created_at(1_700_000_000_000);

    let hashes = m.add_batch(&[&a as &dyn areev_store::AddableDyn, &dup, &b]).unwrap();
    assert_eq!(hashes.len(), 3, "every input gets an address back, in order");
    assert_eq!(hashes[0], hashes[1], "the duplicate resolves to the same address");
    assert_eq!(m.count().unwrap(), 2, "but only two grains are stored");
    // Sequence numbers are not burned by the skip: the next add must land.
    let c = m.add(&Fact::new("bob", "prefers", "water").namespace("caller")).unwrap();
    assert_ne!(c, hashes[0]);
    assert_eq!(m.count().unwrap(), 3);
}

// ── One handle per file per process (#50) ───────────────────────────────────

/// A second handle on one file, in one process, used to open silently and then
/// poison the FIRST handle's writes.
///
/// Both handles load `next_seq`/`next_term` into memory at open and allocate
/// from them independently, so they drift until a write collides — surfacing as
/// `UNIQUE constraint failed: terms.id` on a handle that did nothing wrong,
/// long after the mistake. The cross-process path refuses this correctly at
/// open; inside one process the OS lock is already held, so nothing caught it.
#[test]
fn a_second_handle_on_one_file_is_refused_at_open() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let p = path.to_str().unwrap();

    let mut a = Areev::open(p).unwrap();
    a.add(&Fact::new("a1", "rel", "v").namespace("caller")).unwrap();

    let err = match Areev::open(p) {
        Ok(_) => panic!("the second handle must be refused"),
        Err(e) => e,
    };
    assert_eq!(err.code(), "STO-E002", "unexpected error: {err}");
    let msg = err.to_string();
    assert!(
        msg.contains("already open in this process") && msg.contains("share the handle"),
        "the message must name the cause and the fix: {msg}"
    );

    // The first handle keeps working — the whole point is that the refusal
    // lands on the newcomer, not on the incumbent's next write.
    a.add(&Fact::new("a2", "rel", "v").namespace("caller")).unwrap();
    assert_eq!(a.count().unwrap(), 2);

    // Dropping it releases the claim, so a genuine reopen still works.
    drop(a);
    let mut b = Areev::open(p).expect("reopen after close must succeed");
    b.add(&Fact::new("b1", "rel", "v").namespace("caller")).unwrap();
    assert_eq!(b.count().unwrap(), 3);
}

/// Different files are independent — the guard keys on the path, not on "a
/// memory is open".
#[test]
fn two_handles_on_different_files_coexist() {
    let dir = TempDir::new().unwrap();
    let mut a = Areev::open(dir.path().join("a.db").to_str().unwrap()).unwrap();
    let mut b = Areev::open(dir.path().join("b.db").to_str().unwrap()).unwrap();
    a.add(&Fact::new("x", "r", "v").namespace("caller")).unwrap();
    b.add(&Fact::new("y", "r", "v").namespace("caller")).unwrap();
    assert_eq!(a.count().unwrap(), 1);
    assert_eq!(b.count().unwrap(), 1);
}
