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

/// The read and lifecycle verbs — `show`, `status`, `pause`, `resume`,
/// `render`, `deliver` — none of which had a test.
///
/// These are the verbs an operator actually types between declaring a trigger
/// and debugging why it has not fired, and the README now points people at
/// them. Each assertion below pins what the operator is told, not merely that
/// the command exited zero.
#[test]
fn the_trigger_read_and_lifecycle_verbs_report_what_an_operator_needs() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db");
    let db = db.to_str().unwrap();

    let (ok, out, err) = areev(&[
        "trigger", "add", "--db", db, "--ns", "ops", "--type", "interval",
        "--interval", "120", "--workflow", WF, "--because", "poll the mailbox",
    ]);
    assert!(ok, "add refused: {err}");
    let id = out.trim().rsplit(' ').next().unwrap().to_string();
    assert_eq!(id.len(), 64, "add should print the declaration hash: {out}");
    let short = &id[..12];

    // ── show ────────────────────────────────────────────────────────────────
    let (ok, out, err) = areev(&["trigger", "show", short, "--db", db, "--ns", "ops"]);
    assert!(ok, "show refused: {err}");
    assert!(out.contains(&id), "show must name the full trigger: {out}");
    assert!(out.contains("kind         interval"), "{out}");
    assert!(out.contains("enabled      true"), "{out}");
    assert!(out.contains("paused       false"), "{out}");
    // A never-fired trigger must SAY so. "last fired <blank>" would read as a
    // rendering glitch; "never" is the answer to the question being asked.
    assert!(out.contains("last fired   never"), "{out}");

    // A prefix that matches nothing is an error naming the namespace it
    // searched — the usual cause is being in the wrong one.
    let (ok, _out, err) = areev(&["trigger", "show", "deadbeef", "--db", db, "--ns", "ops"]);
    assert!(!ok, "show of an unknown trigger must fail");
    assert!(err.contains("ops"), "the error should name the namespace: {err}");

    let (ok, _out, err) = areev(&["trigger", "show", "--db", db, "--ns", "ops"]);
    assert!(!ok, "show with no argument must fail");
    assert!(err.contains("usage:"), "{err}");

    // ── status ──────────────────────────────────────────────────────────────
    // The staleness warning is the point of this verb: an enabled trigger that
    // has never fired usually means nobody put `trigger run` on a heartbeat,
    // and that failure is otherwise completely silent.
    let (ok, _out, err) = areev(&["trigger", "status", "--db", db, "--ns", "ops"]);
    assert!(ok, "status refused: {err}");
    assert!(
        err.contains("never fired") && err.contains("heartbeat"),
        "status must warn about the never-fired trigger: {err}"
    );

    // ── pause / resume ──────────────────────────────────────────────────────
    // Pausing a standing rule is a governed act: it stops work silently, so
    // the reason is mandatory the same way a loop decision's is.
    let (ok, _out, err) = areev(&["trigger", "pause", short, "--db", db, "--ns", "ops"]);
    assert!(!ok, "pause without a reason must be refused");
    assert!(err.contains("because"), "{err}");

    let (ok, _out, err) = areev(&[
        "trigger", "pause", short, "--db", db, "--ns", "ops",
        "--because", "the vendor mailbox is being migrated",
    ]);
    assert!(ok, "pause refused: {err}");
    let (_ok, out, _err) = areev(&["trigger", "show", short, "--db", db, "--ns", "ops"]);
    assert!(out.contains("paused       true"), "pause must be visible in show: {out}");

    // A paused trigger is not a never-fired-and-forgotten one, so it must NOT
    // raise the staleness warning — otherwise the warning trains operators to
    // ignore it.
    let (_ok, _out, err) = areev(&["trigger", "status", "--db", db, "--ns", "ops"]);
    assert!(
        !err.contains("never fired"),
        "a deliberately paused trigger must not be reported as stale: {err}"
    );

    let (ok, _out, err) = areev(&[
        "trigger", "resume", short, "--db", db, "--ns", "ops",
        "--because", "migration done",
    ]);
    assert!(ok, "resume refused: {err}");
    let (_ok, out, _err) = areev(&["trigger", "show", short, "--db", db, "--ns", "ops"]);
    assert!(out.contains("paused       false"), "resume must be visible in show: {out}");

    // ── show --format json ──────────────────────────────────────────────────
    let (ok, out, err) =
        areev(&["trigger", "show", short, "--db", db, "--ns", "ops", "--format", "json"]);
    assert!(ok, "show --format json refused: {err}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("show --format json");
    assert_eq!(v["trigger"], id.as_str());
    assert_eq!(v["kind"], "interval");
    assert_eq!(v["paused"], false);

    // ── render ──────────────────────────────────────────────────────────────
    // Every declared target must produce something that mentions the verb it
    // is scheduling; a target that renders an empty unit file is worse than an
    // error, because it installs cleanly and never runs.
    for target in ["cron", "launchd", "systemd", "k8s-cronjob"] {
        let (ok, out, err) =
            areev(&["trigger", "render", target, "--db", db, "--ns", "ops"]);
        assert!(ok, "render {target} refused: {err}");
        // launchd renders the argv as separate <string> elements, so match the
        // tokens rather than the joined command line.
        assert!(
            out.contains("trigger") && out.contains("run"),
            "render {target} must schedule the verb: {out}"
        );
        assert!(
            err.contains("heartbeat"),
            "render must explain the cadence it chose: {err}"
        );
    }

    let (ok, _out, _err) = areev(&["trigger", "render", "sysvinit", "--db", db, "--ns", "ops"]);
    assert!(!ok, "an unknown render target must fail");

    // ── unknown subcommand ──────────────────────────────────────────────────
    let (ok, _out, err) = areev(&["trigger", "frobnicate", "--db", db, "--ns", "ops"]);
    assert!(!ok, "an unknown subcommand must fail");
    assert!(
        err.contains("add") && err.contains("deliver"),
        "the error should list the accepted subcommands: {err}"
    );
}

/// `areev trigger deliver` — the push half of the surface. The host owns the
/// listener and hands Areev the payload, so this is the only way a `webhook`
/// or `manual` trigger ever fires.
#[test]
fn deliver_hands_a_payload_to_a_push_trigger() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db");
    let db = db.to_str().unwrap();

    let (ok, out, err) = areev(&[
        "trigger", "add", "--db", db, "--ns", "ops", "--type", "webhook",
        "--workflow", WF, "--dedup-key", "/invoice_id",
        "--because", "the host receives invoice webhooks",
    ]);
    assert!(ok, "webhook trigger refused: {err}");
    let id = out.trim().rsplit(' ').next().unwrap().to_string();
    let short = &id[..12];

    // A literal payload. Without --tool-cmd nothing executes, which is the
    // documented ingest-only mode rather than a failure.
    let (ok, out, err) = areev(&[
        "trigger", "deliver", "--db", db, "--ns", "ops", "--id", short,
        "--payload", r#"{"invoice_id":"INV-1","amount":420}"#,
    ]);
    assert!(ok, "deliver refused: {err}\n{out}");
    assert!(out.contains("ingested 1"), "the payload should be taken in: {out}");
    assert!(out.contains("runs 0"), "…but nothing executes without --tool-cmd: {out}");

    // Ingesting is not firing. `last fired` tracks runs STARTED, and no tool
    // command was given, so the honest answer is still "never" — the counter
    // does not inflate itself on a payload nobody executed.
    let (_ok, out, _err) = areev(&["trigger", "show", short, "--db", db, "--ns", "ops"]);
    assert!(
        out.contains("last fired   never"),
        "an ingested-but-unexecuted payload must not count as a firing: {out}"
    );

    // Re-delivering the same invoice is a DUPLICATE, not a second run. A
    // webhook that retries must not pay the invoice twice.
    let (ok, out, err) = areev(&[
        "trigger", "deliver", "--db", db, "--ns", "ops", "--id", short,
        "--payload", r#"{"invoice_id":"INV-1","amount":420}"#,
    ]);
    assert!(ok, "re-deliver refused: {err}");
    assert!(out.contains("already delivered"), "a repeat must be reported as such: {out}");

    // A payload with nothing at the declared dedup key cannot be identified,
    // so nothing starts — and the operator is told, rather than left to infer
    // it from a run that never appears.
    let (ok, out, err) = areev(&[
        "trigger", "deliver", "--db", db, "--ns", "ops", "--id", short,
        "--payload", r#"{"no_identity_here":true}"#,
    ]);
    assert!(ok, "deliver refused: {err}");
    assert!(out.contains("dedup key"), "an unidentifiable payload must say so: {out}");

    // Delivering to an unknown id is an error, not a silent no-op — a webhook
    // handler that swallows this drops real traffic.
    let (ok, _out, _err) = areev(&[
        "trigger", "deliver", "--db", db, "--ns", "ops", "--id", "deadbeef",
        "--payload", "{}",
    ]);
    assert!(!ok, "deliver to an unknown trigger must fail");

    // `--id` is required: without it there is no trigger to deliver to.
    let (ok, _out, err) =
        areev(&["trigger", "deliver", "--db", db, "--ns", "ops", "--payload", "{}"]);
    assert!(!ok, "deliver without --id must fail");
    assert!(err.contains("id"), "{err}");
}
