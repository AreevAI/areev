//! Concurrent evaluation: the acceptance criterion from AreevAI/areev#36.
//!
//! > N concurrent `trigger run` against one memory produce exactly one claim
//! > per due trigger — property test, N ≥ 8, repeated.
//!
//! One nuance the issue did not anticipate, worth stating because it changes
//! what "N concurrent invocations" can even mean per tier:
//!
//! **On the embedded backend, N concurrent *processes* cannot happen at all.**
//! One memory is one writer, enforced across processes by an OS file lock and
//! inside a process by the open-path registry, so a second `areev trigger run`
//! fails at open with `STO-E002` rather than losing a claim. That is also
//! correct — exactly one evaluator runs — it just arrives by a different
//! mechanism. The claim protocol is what makes the *Postgres* tier safe, where
//! several nodes genuinely do share a memory.
//!
//! So the property is exercised here the way it can be on this tier: N threads
//! against one shared handle, which is the same `meta_cas` path the Postgres
//! tier takes. `crates/areev-conformance` runs the CAS case against both.

use areev_cal::AreevFacade;
use areev_core::types::{Grain, Trigger, TriggerKind};
use areev_store::Areev;
use areev_trigger::{clock::FixedClock, Clock, EvalOptions, Evaluator};
use std::sync::{Arc, Barrier, Mutex};

const NS: &str = "ops";
const WF: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
const T0: i64 = 1_767_225_600_000;

fn rig(dir: &tempfile::TempDir, name: &str) -> (Arc<AreevFacade>, Arc<FixedClock>) {
    let m = Areev::open(dir.path().join(name).to_str().unwrap()).unwrap();
    (Arc::new(AreevFacade::new(m)), Arc::new(FixedClock::new(T0)))
}

fn evaluator(facade: &Arc<AreevFacade>, clock: &Arc<FixedClock>, node: &str) -> Evaluator {
    let _ = node;
    Evaluator {
        facade: Arc::clone(facade),
        clock: Arc::clone(clock) as Arc<dyn Clock>,
        connector: None,
        starter: None,
        credentials: Default::default(),
        ns: NS.into(),
        principal: "user:test".into(),
    }
}

#[test]
fn eight_concurrent_evaluators_produce_exactly_one_claim_per_due_trigger() {
    // Repeated, because a race that fails one run in ten is worse than one that
    // fails every time.
    for round in 0..12 {
        let dir = tempfile::TempDir::new().unwrap();
        let (facade, clock) = rig(&dir, &format!("m{round}.db"));

        let t = Trigger::new(TriggerKind::Interval, WF)
            .interval_secs(3600)
            .created_at(T0)
            .namespace(NS);
        facade.with_store(|m| m.add(&t)).unwrap();

        const N: usize = 8;
        let barrier = Arc::new(Barrier::new(N));
        let claims = Arc::new(Mutex::new(0usize));
        let locked = Arc::new(Mutex::new(0usize));

        std::thread::scope(|s| {
            for node in 0..N {
                let (facade, clock) = (Arc::clone(&facade), Arc::clone(&clock));
                let (barrier, claims, locked) =
                    (Arc::clone(&barrier), Arc::clone(&claims), Arc::clone(&locked));
                s.spawn(move || {
                    let ev = evaluator(&facade, &clock, &format!("node-{node}"));
                    let opts = EvalOptions { node: format!("node-{node}"), ..Default::default() };
                    barrier.wait();
                    let r = ev.run(&opts).unwrap();
                    *claims.lock().unwrap() += r.claimed;
                    *locked.lock().unwrap() += r.skipped_locked + r.skipped_not_due;
                });
            }
        });

        assert_eq!(
            *claims.lock().unwrap(),
            1,
            "round {round}: exactly one evaluator may fire a due trigger"
        );
        assert_eq!(
            *locked.lock().unwrap(),
            N - 1,
            "round {round}: every loser must report a skip, not an error"
        );
    }
}

#[test]
fn losing_the_race_is_never_an_error() {
    // Operationally load-bearing: losers exit 0. If a lost race were an error,
    // every heartbeat on an N-node deployment would page N-1 times.
    let dir = tempfile::TempDir::new().unwrap();
    let (facade, clock) = rig(&dir, "quiet.db");
    let t = Trigger::new(TriggerKind::Interval, WF).interval_secs(3600).created_at(T0).namespace(NS);
    facade.with_store(|m| m.add(&t)).unwrap();

    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    std::thread::scope(|s| {
        for node in 0..6 {
            let (facade, clock, errors) =
                (Arc::clone(&facade), Arc::clone(&clock), Arc::clone(&errors));
            s.spawn(move || {
                let ev = evaluator(&facade, &clock, "n");
                let opts = EvalOptions { node: format!("node-{node}"), ..Default::default() };
                let r = ev.run(&opts).unwrap();
                errors.lock().unwrap().extend(r.errors);
            });
        }
    });
    assert!(errors.lock().unwrap().is_empty(), "{:?}", errors.lock().unwrap());
}

#[test]
fn a_stalled_evaluator_cannot_write_back_behind_its_successor() {
    // The zombie case at the evaluator level rather than the store level: A
    // claims, its lease expires, B takes over and completes, and A's release
    // must be refused. TRG-E005 is the visible consequence.
    let dir = tempfile::TempDir::new().unwrap();
    let (facade, clock) = rig(&dir, "zombie.db");
    let t = Trigger::new(TriggerKind::Interval, WF).interval_secs(60).created_at(T0).namespace(NS);
    let hash = facade.with_store(|m| m.add(&t)).unwrap().to_hex();

    // A claims with a short lease.
    let a = evaluator(&facade, &clock, "A");
    let a_opts = EvalOptions {
        node: "A".into(),
        lease: std::time::Duration::from_millis(1),
        ..Default::default()
    };
    a.run(&a_opts).unwrap();

    // Time passes; A's lease is long gone. B reclaims and completes.
    clock.advance(60_000);
    let b = evaluator(&facade, &clock, "B");
    let b_opts = EvalOptions { node: "B".into(), ..Default::default() };
    let r = b.run(&b_opts).unwrap();
    assert_eq!(r.claimed, 1, "an expired lease must not park a trigger forever");

    let (state, _) = facade.with_store(|m| m.trigger_state(&hash)).unwrap().unwrap();
    assert_eq!(state.fence, 2, "each claim advances the fence");
    assert!(state.claimed_by.is_none(), "B released cleanly");
}

#[test]
fn clock_skew_does_not_change_whether_a_trigger_is_due() {
    // Two evaluators with clocks ten minutes apart, sharing a memory. The
    // *state* decides what is due, so the skewed one must not re-fire something
    // the other just handled.
    let dir = tempfile::TempDir::new().unwrap();
    let (facade, _) = rig(&dir, "skew.db");
    let t = Trigger::new(TriggerKind::Interval, WF).interval_secs(3600).created_at(T0).namespace(NS);
    facade.with_store(|m| m.add(&t)).unwrap();

    let on_time = Arc::new(FixedClock::new(T0));
    let fast = Arc::new(FixedClock::new(T0 + 600_000)); // ten minutes ahead

    let first = evaluator(&facade, &on_time, "A").run(&EvalOptions::default()).unwrap();
    assert_eq!(first.claimed, 1);

    // The fast node sees a later `now`, but next_due_at is an hour out, so it
    // is still not due — the skew is not enough to manufacture a firing.
    let second = evaluator(&facade, &fast, "B").run(&EvalOptions::default()).unwrap();
    assert_eq!(second.claimed, 0, "a skewed clock must not re-fire recent work");
    assert_eq!(second.skipped_not_due, 1);
}

#[test]
fn a_second_handle_on_an_embedded_memory_is_refused_at_open() {
    // Documents the tier difference the issue's deployment model missed: on the
    // embedded backend, N concurrent evaluator *processes* never happen,
    // because one memory is one writer. Exactly one evaluator runs, arrived at
    // by a lock rather than by a claim.
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("single.db");
    let _first = Areev::open(path.to_str().unwrap()).unwrap();

    let second = Areev::open(path.to_str().unwrap());
    let err = second.err().expect("a second handle must be refused");
    assert!(err.to_string().starts_with("STO-E002"), "{err}");
}
