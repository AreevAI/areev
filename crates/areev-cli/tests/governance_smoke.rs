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

    // Releasing is held to the same bar as placing: it is the act an auditor
    // asks about, and the hold row is DELETED on release, so without a reason
    // on the trail the placement would be the only half on record.
    let (ok, _out, err) = areev(&["hold", "release", "--db", &db, "--ns", "books"]);
    assert!(!ok, "a release with no reason must be refused");
    assert!(err.contains("usage:"), "{err}");

    // Released, the same sweep proceeds.
    let (ok, out, err) = areev(&[
        "hold", "release", "--db", &db, "--ns", "books",
        "--because", "matter closed", "--by", "user:counsel",
    ]);
    assert!(ok, "hold release failed: {err}");
    assert!(out.contains("released"), "{out}");
    assert!(out.contains("matter closed"), "the release echoes its reason: {out}");

    let (ok, out, err) =
        areev(&["retention", "sweep", "--yes", "--db", &db, "--ns", "books"]);
    assert!(ok, "a sweep must proceed once the hold is released: {err}");
    assert!(!out.contains("legal hold"), "the released hold must stop blocking: {out}");
    assert!(out.contains("1 grains erased"), "the sweep must now do its work: {out}");
    // Both transitions are on the Tier-2 trail `audit export` reads — the
    // hold row itself is gone, so this is the only record that survives.
    let (ok, out, err) = areev(&["audit", "export", "--db", &db]);
    assert!(ok, "audit export failed: {err}");
    assert!(out.contains("hold.set"), "the placement must be on the trail: {out}");
    assert!(out.contains("hold.release"), "and so must the release: {out}");
    assert!(out.contains("matter closed"), "with the reason given: {out}");


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

/// #420's repro through the real binary: an approval run with one refused and
/// one accepted response, plus one `forget-subject`, exports the destruction
/// record AND both approval records — each trail an unbroken chain — and
/// `--trail` narrows the output without narrowing the verification.
#[test]
fn audit_export_carries_the_approval_trail_beside_the_destruction_trail() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("a.db");
    let db = db.to_str().unwrap();
    let (ok, out, err) = areev(&["run", "--db", db, "--ns", "ops", "demo"]);
    assert!(ok, "demo seed failed: {err}");
    let wf = out
        .lines()
        .find_map(|l| l.strip_prefix("demo plan seeded (workflow "))
        .and_then(|l| l.strip_suffix(")"))
        .expect("workflow hash in demo output")
        .to_string();
    #[cfg(not(windows))]
    let greet_cmd = r#"printf '{"greeting":"hello"}'"#;
    #[cfg(windows)]
    let greet_cmd = r#"echo {"greeting":"hello"}"#;
    let (ok, out, err) = areev(&[
        "run", "--db", db, "--ns", "ops", "start", "--workflow", &wf, "--run-id", "r1",
        "--tool-cmd", greet_cmd, "--initiator", "citizen:4821",
    ]);
    assert!(ok, "start failed: {err}");
    let env: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let ask = env["asks"][0]["tool_call_id"].as_str().unwrap().to_string();
    // Refused: the initiator may not approve.
    let (ok, _, _) = areev(&[
        "run", "--db", db, "--ns", "ops", "respond", "--run-id", "r1", "--ask", &ask,
        "--result", r#"{"approved":true}"#, "--as", "citizen:4821",
    ]);
    assert!(!ok, "the initiator's approval must be refused");
    // Accepted.
    let (ok, _, err) = areev(&[
        "run", "--db", db, "--ns", "ops", "respond", "--run-id", "r1", "--ask", &ask,
        "--result", r#"{"approved":true}"#, "--as", "volunteer:33410",
    ]);
    assert!(ok, "respond failed: {err}");
    // One erasure.
    let (ok, _, err) = areev(&["add", "pat", "likes", "tea", "--db", db, "--ns", "ops"]);
    assert!(ok, "{err}");
    let (ok, _, err) = areev(&[
        "forget-subject", "pat", "--db", db, "--ns", "ops", "--yes", "--because", "Art. 17 #42",
    ]);
    assert!(ok, "forget-subject failed: {err}");

    let (ok, out, err) = areev(&["audit", "export", "--db", db]);
    assert!(ok, "audit export failed: {err}");
    let rows: Vec<serde_json::Value> =
        out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let trail = |t: &str| rows.iter().filter(|r| r["trail"] == t).collect::<Vec<_>>();
    assert_eq!(trail("destruction").len(), 1, "{out}");
    let approvals = trail("approval");
    assert_eq!(approvals.len(), 2, "{out}");
    assert_eq!(approvals[0]["outcome"], "refused");
    assert_eq!(approvals[0]["refusal"], "separation_of_duties");
    assert_eq!(approvals[0]["run_id"], "r1", "{out}");
    assert_eq!(approvals[0]["ask"], ask.as_str());
    assert_eq!(approvals[0]["node"], "approve");
    assert_eq!(approvals[0]["responder"], "citizen:4821");
    assert_eq!(approvals[0]["initiator"], "citizen:4821");
    assert_eq!(approvals[0]["seq"], 1);
    assert_eq!(approvals[0]["chain_root"], true);
    assert_eq!(approvals[1]["outcome"], "accepted");
    assert_eq!(approvals[1]["responder"], "volunteer:33410");
    assert_eq!(approvals[1]["seq"], 2);
    assert_eq!(approvals[1]["previous_audit"], approvals[0]["hash"]);
    assert!(rows.iter().all(|r| r.get("chain_break").is_none()), "{out}");
    assert!(!err.contains("truncated"), "{err}");

    // The filter narrows the output, and only the output.
    let (ok, out, err) = areev(&["audit", "export", "--db", db, "--trail", "approval"]);
    assert!(ok, "{err}");
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(out.lines().all(|l| l.contains("\"trail\":\"approval\"")), "{out}");
    let (ok, out, _) = areev(&["audit", "export", "--db", db, "--trail", "destruction,loop"]);
    assert!(ok);
    assert_eq!(out.lines().count(), 1, "{out}");
    let (ok, _, err) = areev(&["audit", "export", "--db", db, "--trail", "reads"]);
    assert!(!ok && err.contains("--trail"), "{err}");
}

/// `areev run tick` (#416): with nothing waiting it reports an empty pass —
/// and it is a real verb, not an unknown-subcommand error.
#[test]
fn run_tick_reports_an_empty_pass() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db");
    let db = db.to_str().unwrap();
    let (ok, _, err) = areev(&["add", "acme", "owes", "4200", "--db", db, "--ns", "ops"]);
    assert!(ok, "{err}");
    let (ok, out, err) = areev(&["run", "--db", db, "--ns", "ops", "tick"]);
    assert!(ok, "tick failed: {err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["resumed"], serde_json::json!([]));
    assert_eq!(v["waiting"], serde_json::json!([]));
    assert!(v["now_ms"].as_u64().unwrap() > 0);
}
