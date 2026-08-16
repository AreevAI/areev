//! Retention FLOORS and LEGAL HOLDS (governed-agents §5.4): the ceiling
//! machinery (`retention:<ns>`, `PURGE OLDER THAN`) could destroy the very
//! Art. 12-mandated lifecycle logs the compliance story cites — with a
//! clean audit trail of having done so. Floors refuse any cutoff younger
//! than `min_days`; holds refuse ALL age-based destruction in a namespace;
//! sweeps skip a refused namespace WITH the refusal on record, never
//! silently, never aborting the pass.

use areev_core::types::{Fact, Grain};
use areev_store::{Areev, RetentionOutcome, RetentionPolicy};
use tempfile::TempDir;

fn open_mem() -> (Areev, TempDir) {
    let d = TempDir::new().unwrap();
    let m = Areev::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    (m, d)
}

fn old_fact(m: &mut Areev, ns: &str, s: &str, at: i64) {
    m.add(&Fact::new(s, "logged", "x").namespace(ns).created_at(at))
        .unwrap();
}

const DAY: i64 = 86_400_000;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn a_floor_refuses_young_cutoffs_and_admits_old_ones() {
    let (mut m, _d) = open_mem();
    let t = now();
    old_fact(&mut m, "runs", "r1", t - 400 * DAY);
    old_fact(&mut m, "runs", "r2", t - 10 * DAY);
    // The eu-ai-act posture: run logs live at least ~183 days.
    m.set_retention_floor("runs", 183.0, "AI Act Art. 12 — 6-month minimum")
        .unwrap();

    // A 30-day purge would destroy mandated logs: refused, naming the floor.
    let err = m
        .forget_older_than(Some("runs"), t - 30 * DAY, None)
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("retention floor") && msg.contains("Art. 12"), "{msg}");

    // Cutoffs older than the floor still work: r1 (400d) goes, r2 stays.
    let rep = m
        .forget_older_than(Some("runs"), t - 200 * DAY, None)
        .unwrap();
    assert_eq!(rep.grains_erased, 1);

    // Clearing the floor restores the old behavior.
    m.clear_retention_floor("runs").unwrap();
    assert!(m.forget_older_than(Some("runs"), t - 5 * DAY, None).is_ok());
}

#[test]
fn a_hold_stops_all_age_destruction_and_global_sweeps_name_it() {
    let (mut m, _d) = open_mem();
    let t = now();
    old_fact(&mut m, "cases", "c1", t - 500 * DAY);
    old_fact(&mut m, "other", "o1", t - 500 * DAY);
    m.place_hold("cases", "litigation ACME v. Us", "counsel:jane", t)
        .unwrap();

    // Any cutoff, however old: refused while held.
    let err = m
        .forget_older_than(Some("cases"), t - 490 * DAY, None)
        .unwrap_err();
    assert!(err.to_string().contains("legal hold"), "{err}");
    assert!(err.to_string().contains("counsel:jane"), "the hold has an owner: {err}");

    // A GLOBAL sweep is refused while any hold is live — scoping around a
    // held namespace is the operator's explicit act, never the default.
    let err = m.forget_older_than(None, t - 490 * DAY, None).unwrap_err();
    assert!(err.to_string().contains("scope the sweep"), "{err}");

    // A scoped sweep of an unheld namespace still works.
    let rep = m
        .forget_older_than(Some("other"), t - 490 * DAY, None)
        .unwrap();
    assert_eq!(rep.grains_erased, 1);

    // Release restores.
    m.release_hold("cases").unwrap();
    let rep = m
        .forget_older_than(Some("cases"), t - 490 * DAY, None)
        .unwrap();
    assert_eq!(rep.grains_erased, 1);
}

/// The cron path: a held namespace SKIPS with the refusal recorded in the
/// sweep's own output — the pass continues, nothing is silent.
#[test]
fn sweep_retention_skips_held_namespaces_with_the_refusal_on_record() {
    let (mut m, _d) = open_mem();
    let t = now();
    old_fact(&mut m, "held", "h1", t - 100 * DAY);
    old_fact(&mut m, "free", "f1", t - 100 * DAY);
    for ns in ["held", "free"] {
        m.set_retention_policy(
            ns,
            &RetentionPolicy { days: 30.0, grain_type: None, because: Some("test".into()) },
        )
        .unwrap();
    }
    m.place_hold("held", "audit in progress", "counsel:jane", t).unwrap();

    let results = m.sweep_retention(t).unwrap();
    assert_eq!(results.len(), 2, "every policy appears in the output");
    let held = results.iter().find(|r| r.ns == "held").unwrap();
    let free = results.iter().find(|r| r.ns == "free").unwrap();
    match &held.outcome {
        RetentionOutcome::Skipped { why } => {
            assert!(why.contains("legal hold"), "{why}")
        }
        other => panic!("held ns must skip, got {other:?}"),
    }
    match &free.outcome {
        RetentionOutcome::Swept(rep) => assert_eq!(rep.grains_erased, 1),
        other => panic!("free ns must sweep, got {other:?}"),
    }
    // The held grain survived.
    assert_eq!(m.recall("held", "h1", None, 5).unwrap().len(), 1);
}

/// Floors and holds are file-truths: they survive a reopen (a copy, a sync,
/// a restore on another host keeps refusing).
#[test]
fn floors_and_holds_survive_reopen() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    let path = path.to_str().unwrap();
    {
        let m = Areev::open(path).unwrap();
        m.set_retention_floor("runs", 183.0, "Art. 12").unwrap();
        m.place_hold("cases", "lit", "counsel:jane", 1_000).unwrap();
    }
    let m = Areev::open(path).unwrap();
    assert_eq!(m.retention_floors().unwrap().len(), 1);
    assert_eq!(m.holds().unwrap().len(), 1);
}

#[test]
fn declarations_validate_their_inputs() {
    let (m, _d) = open_mem();
    assert!(m.set_retention_floor("", 10.0, "x").is_err());
    assert!(m.set_retention_floor("ns", 0.0, "x").is_err());
    assert!(m.set_retention_floor("ns", -1.0, "x").is_err());
    assert!(m.set_retention_floor("ns", f64::NAN, "x").is_err());
    assert!(m.set_retention_floor("ns", 10.0, "  ").is_err(), "because is mandatory");
    assert!(m.place_hold("ns", "", "who", 0).is_err());
    assert!(m.place_hold("ns", "why", " ", 0).is_err());
}
