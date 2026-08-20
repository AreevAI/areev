//! `areev trigger` through the real binary.
//!
//! The surface this covers had none: `memory` and `composite` are two of the
//! eight declared kinds, and neither could be declared from the CLI at all —
//! `incoherence()` refuses a memory trigger without a predicate, and there was
//! no flag that could supply one. A unit test on the parser would not have
//! caught that, because the parser was fine; the wiring was missing.

use std::process::Command;
use tempfile::TempDir;

/// A content address shaped like a real one. `trigger add` does not resolve the
/// workflow — that is TRG-E002 at firing time — so a literal is honest here.
const WF: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
const MEMBER_A: &str = "b1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
const MEMBER_B: &str = "c1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

fn areev(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_areev")).args(args).output().expect("spawn areev");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn memory_and_composite_triggers_can_be_declared_from_the_cli() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db");
    let db = db.to_str().unwrap();

    // A memory trigger: the predicate is CAL `WHERE` syntax.
    let (ok, out, err) = areev(&[
        "trigger", "add", "--db", db, "--ns", "ops", "--type", "memory", "--workflow", WF,
        "--where", r#"grain_type = "fact" AND subject = "acme""#,
        "--because", "start review when an Acme fact lands",
    ]);
    assert!(ok, "memory trigger refused: {err}");
    assert_eq!(out.trim().rsplit(' ').next().unwrap().len(), 64, "{out}");

    // A composite: aliases, a gate over them, and a correlation window.
    let members = format!("invoice={MEMBER_A},purchase_order={MEMBER_B}");
    let (ok, _out, err) = areev(&[
        "trigger", "add", "--db", db, "--ns", "ops", "--type", "composite", "--workflow", WF,
        "--members", &members,
        "--where", "invoice = true AND purchase_order = true",
        "--correlate", "/thread_id", "--window", "10m",
        "--because", "match an invoice to its purchase order",
    ]);
    assert!(ok, "composite trigger refused: {err}");

    let (ok, out, err) = areev(&["trigger", "list", "--db", db, "--ns", "ops", "--json"]);
    assert!(ok, "list failed: {err}");
    assert!(out.contains("memory"), "{out}");
    assert!(out.contains("composite"), "{out}");
}

#[test]
fn a_declaration_that_could_never_fire_is_refused_rather_than_stored() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db");
    let db = db.to_str().unwrap();

    // A memory trigger with no predicate selects nothing, forever.
    let (ok, _out, err) = areev(&[
        "trigger", "add", "--db", db, "--ns", "ops", "--type", "memory", "--workflow", WF,
        "--because", "no predicate",
    ]);
    assert!(!ok, "a memory trigger with no predicate must be refused");
    assert!(err.contains("predicate"), "{err}");

    // A gate naming a member the declaration does not carry can never be
    // satisfied. Firing already refused this; declaring must too.
    let (ok, _out, err) = areev(&[
        "trigger", "add", "--db", db, "--ns", "ops", "--type", "composite", "--workflow", WF,
        "--members", &format!("invoice={MEMBER_A},purchase_order={MEMBER_B}"),
        "--where", "invoice = true AND ghost = true",
        "--because", "names a member it does not declare",
    ]);
    assert!(!ok, "a gate over an undeclared member must be refused");
    assert!(err.contains("TRG-E008"), "{err}");
    assert!(err.contains("ghost"), "{err}");

    // A window with no unit is ambiguous between seconds and milliseconds.
    let (ok, _out, err) = areev(&[
        "trigger", "add", "--db", db, "--ns", "ops", "--type", "composite", "--workflow", WF,
        "--members", &format!("invoice={MEMBER_A},purchase_order={MEMBER_B}"),
        "--where", "invoice = true AND purchase_order = true",
        "--window", "600",
        "--because", "unitless window",
    ]);
    assert!(!ok, "a unitless --window must be refused, not guessed");
    assert!(err.contains("--window"), "{err}");
}

/// The k8s manifest must not carry the authoring machine's binary path (#69).
///
/// Driven through the real binary on purpose: the defect was
/// `std::env::current_exe()` reaching the render, which only has a wrong value
/// when a real process produced it. A unit test supplying `exe: "areev"` cannot
/// see it — that is exactly why this shipped.
#[test]
fn k8s_render_uses_the_image_binary_not_the_authoring_hosts_path() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db");
    let db = db.to_str().unwrap();

    let (ok, _out, err) = areev(&[
        "trigger", "add", "--db", db, "--ns", "ops", "--type", "interval",
        "--workflow", WF, "--interval", "900", "--because", "heartbeat",
    ]);
    assert!(ok, "{err}");

    let (ok, out, err) =
        areev(&["trigger", "render", "--db", db, "--ns", "ops", "--target", "k8s-cronjob"]);
    assert!(ok, "{err}");

    let exe = env!("CARGO_BIN_EXE_areev");
    assert!(
        !out.contains(exe),
        "a path from this machine cannot be right inside a container:\n{out}"
    );
    assert!(out.contains("\n            - areev\n"), "command[0] must be the PATH name:\n{out}");

    // The host targets run where they were rendered, so for them it IS right.
    for target in ["cron", "launchd", "systemd"] {
        let (ok, out, err) =
            areev(&["trigger", "render", "--db", db, "--ns", "ops", "--target", target]);
        assert!(ok, "{err}");
        assert!(out.contains(exe), "{target} runs locally and must name the local binary:\n{out}");
    }
}

/// A schedule this build cannot evaluate is refused at declaration (#67).
#[test]
fn a_non_utc_timezone_is_refused_when_declared() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db");
    let db = db.to_str().unwrap();

    let (ok, _out, err) = areev(&[
        "trigger", "add", "--db", db, "--ns", "ops", "--type", "schedule", "--workflow", WF,
        "--cron", "0 9 * * *", "--timezone", "Asia/Kolkata", "--because", "probe",
    ]);
    assert!(!ok, "a non-UTC timezone must be refused");
    assert!(err.contains("TRG-E006"), "{err}");

    // Nothing was stored, so `status` cannot report it as healthy.
    let (ok, out, _err) = areev(&["trigger", "status", "--db", db, "--ns", "ops"]);
    assert!(ok);
    assert!(out.contains("no triggers declared"), "{out}");
}
