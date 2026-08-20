//! `areev hold` and `areev retention floor` through the real binary.
//!
//! Both are guards over **age-based destruction**, and neither had a test. They
//! are the two answers to different questions — "stop deleting anything in this
//! namespace, we are in litigation" (a hold) and "never delete anything younger
//! than N days, whatever a policy says" (a floor) — and the thing worth pinning
//! is not that the verbs run, but that a sweep actually **erases nothing** while
//! either is live. A guard that stores cleanly and does not guard is worse than
//! no guard, because it is believed.
//!
//! The sweep reports a guarded namespace as `SKIPPED` and still exits zero:
//! one held namespace must not fail a nightly cron that sweeps every other one.
//! So "it was blocked" is asserted as *skipped, named, and nothing erased* —
//! never as a non-zero exit.

use std::process::Command;
use tempfile::TempDir;

fn areev(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_areev")).args(args).output().expect("spawn areev");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Seed a memory with one fact and a retention policy that would sweep it.
fn seeded(dir: &TempDir) -> String {
    let db = dir.path().join("g.db").to_str().unwrap().to_string();
    let (ok, _out, err) =
        areev(&["add", "acme", "owes", "4200", "--db", &db, "--ns", "books"]);
    assert!(ok, "seed add failed: {err}");
    // Zero days: everything already qualifies, so a sweep has real work to do
    // and "nothing happened" cannot be mistaken for "the guard worked".
    let (ok, _out, err) =
        areev(&["retention", "set", "--days", "0", "--db", &db, "--ns", "books"]);
    assert!(ok, "retention set failed: {err}");
    db
}

#[test]
fn a_legal_hold_stops_an_age_based_sweep_until_it_is_released() {
    let dir = TempDir::new().unwrap();
    let db = seeded(&dir);

    // Nothing held yet.
    let (ok, out, err) = areev(&["hold", "list", "--db", &db]);
    assert!(ok, "hold list failed: {err}");
    assert!(out.contains("no legal holds"), "{out}");

    // A hold records WHO and WHY — an unattributed hold cannot be lifted with
    // confidence later, which is how holds outlive the matter that caused them.
    let (ok, _out, err) = areev(&["hold", "set", "--db", &db, "--ns", "books"]);
    assert!(!ok, "a hold with no reason must be refused");
    assert!(err.contains("usage:"), "{err}");

    let (ok, out, err) = areev(&[
        "hold", "set", "--db", &db, "--ns", "books",
        "--because", "litigation 24-CV-9", "--by", "user:counsel",
    ]);
    assert!(ok, "hold set failed: {err}");
    assert!(out.contains("user:counsel") && out.contains("litigation 24-CV-9"), "{out}");

    let (ok, out, _err) = areev(&["hold", "list", "--db", &db]);
    assert!(ok);
    assert!(out.contains("books") && out.contains("user:counsel"), "{out}");

    // The point of the whole feature. Note the sweep still EXITS ZERO: a held
    // namespace is a skip, not a command failure, so one hold cannot break a
    // nightly cron sweeping every other namespace. What must be true is that
    // it erased nothing and said exactly why.
    let (ok, out, err) =
        areev(&["retention", "sweep", "--yes", "--db", &db, "--ns", "books"]);
    assert!(ok, "a hold should skip the namespace, not fail the sweep: {err}");
    assert!(out.contains("SKIPPED"), "the held namespace must be reported skipped: {out}");
    assert!(out.contains("legal hold"), "the skip must name the cause: {out}");
    assert!(out.contains("user:counsel"), "…and who placed it: {out}");
    assert!(out.contains("0 grains erased"), "nothing may be erased under a hold: {out}");

    // The fact is still there — the refusal was not cosmetic.
    let (ok, out, _err) = areev(&["recall", "acme", "--db", &db, "--ns", "books"]);
    assert!(ok);
    assert!(out.contains("4200"), "the held grain must survive the refused sweep: {out}");

    // Released, the same sweep proceeds.
    let (ok, out, err) = areev(&["hold", "release", "--db", &db, "--ns", "books"]);
    assert!(ok, "hold release failed: {err}");
    assert!(out.contains("released"), "{out}");

    let (ok, out, err) =
        areev(&["retention", "sweep", "--yes", "--db", &db, "--ns", "books"]);
    assert!(ok, "a sweep must proceed once the hold is released: {err}");
    assert!(!out.contains("legal hold"), "the released hold must stop blocking: {out}");
    assert!(out.contains("1 grains erased"), "the sweep must now do its work: {out}");

    // And the grain is actually gone — the released sweep was not cosmetic
    // either.
    let (ok, out, _err) = areev(&["recall", "acme", "--db", &db, "--ns", "books"]);
    assert!(ok);
    assert!(!out.contains("4200"), "the swept grain must be gone: {out}");
}

#[test]
fn a_retention_floor_outranks_a_shorter_policy_and_needs_a_reason() {
    let dir = TempDir::new().unwrap();
    let db = seeded(&dir);

    let (ok, out, err) = areev(&["retention", "floors", "--db", &db]);
    assert!(ok, "floors failed: {err}");
    assert!(out.contains("no retention floors"), "{out}");

    // A floor without a rationale is not auditable, so it is refused.
    let (ok, _out, err) =
        areev(&["retention", "floor", "--min-days", "30", "--db", &db, "--ns", "books"]);
    assert!(!ok, "a floor with no reason must be refused");
    assert!(err.contains("because"), "{err}");

    // …and so is one with no number.
    let (ok, _out, err) = areev(&[
        "retention", "floor", "--because", "tax law", "--db", &db, "--ns", "books",
    ]);
    assert!(!ok, "a floor with no --min-days must be refused");
    assert!(err.contains("usage:"), "{err}");

    let (ok, out, err) = areev(&[
        "retention", "floor", "--min-days", "2555", "--because", "tax records: 7 years",
        "--db", &db, "--ns", "books",
    ]);
    assert!(ok, "floor failed: {err}");
    assert!(out.contains("2555") && out.contains("tax records"), "{out}");

    let (ok, out, _err) = areev(&["retention", "floors", "--db", &db]);
    assert!(ok);
    assert!(out.contains("books") && out.contains("2555"), "{out}");

    // The seeded policy says "delete everything older than 0 days"; the floor
    // says "nothing younger than 7 years". The floor must win — a policy that
    // could quietly out-rank a floor makes the floor decorative.
    let (ok, out, err) =
        areev(&["retention", "sweep", "--yes", "--db", &db, "--ns", "books"]);
    assert!(ok, "a floor should skip the namespace, not fail the sweep: {err}");
    assert!(out.contains("SKIPPED"), "{out}");
    assert!(out.contains("2555-day retention floor"), "the skip must name the floor: {out}");
    assert!(out.contains("0 grains erased"), "nothing may be erased under a floor: {out}");

    let (ok, out, _err) = areev(&["recall", "acme", "--db", &db, "--ns", "books"]);
    assert!(ok);
    assert!(out.contains("4200"), "the floored grain must survive: {out}");

    // Cleared, the policy applies again.
    let (ok, out, err) =
        areev(&["retention", "floor-clear", "--db", &db, "--ns", "books"]);
    assert!(ok, "floor-clear failed: {err}");
    assert!(out.contains("cleared"), "{out}");

    let (ok, out, err) =
        areev(&["retention", "sweep", "--yes", "--db", &db, "--ns", "books"]);
    assert!(ok, "a sweep must proceed once the floor is cleared: {err}");
    assert!(out.contains("1 grains erased"), "the sweep must now do its work: {out}");
}

#[test]
fn a_sweep_without_yes_prints_the_plan_instead_of_erasing() {
    let dir = TempDir::new().unwrap();
    let db = seeded(&dir);

    // No --yes: this must describe what it *would* do and change nothing. A
    // destructive verb that acts on a bare invocation is the one mistake that
    // cannot be walked back.
    let (ok, _out, err) = areev(&["retention", "sweep", "--db", &db, "--ns", "books"]);
    assert!(!ok, "a bare sweep must not proceed");
    assert!(err.contains("--yes"), "it must say how to confirm: {err}");
    assert!(err.contains("erase grains older than"), "it must show the plan: {err}");

    let (ok, out, _err) = areev(&["recall", "acme", "--db", &db, "--ns", "books"]);
    assert!(ok);
    assert!(out.contains("4200"), "an unconfirmed sweep must erase nothing: {out}");

    // The process-wide cap refuses even with --yes.
    let (ok, _out, err) = areev(&[
        "retention", "sweep", "--yes", "--no-destructive-ops", "--db", &db, "--ns", "books",
    ]);
    assert!(!ok, "--no-destructive-ops must cap the sweep");
    assert!(err.contains("destructive"), "{err}");

    // Unknown subcommands name the accepted set rather than doing nothing.
    let (ok, _out, err) = areev(&["retention", "vacuum", "--db", &db]);
    assert!(!ok);
    assert!(err.contains("sweep") && err.contains("floor"), "{err}");

    let (ok, _out, err) = areev(&["hold", "revoke", "--db", &db]);
    assert!(!ok);
    assert!(err.contains("set") && err.contains("release"), "{err}");
}
