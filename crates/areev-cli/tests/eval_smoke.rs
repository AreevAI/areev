//! `areev eval` — the §7.4 gating edge, through the real binary.
//!
//! This verb is load-bearing for Rule E1: a `code_revision` recommendation is
//! pinned to the evalset it was judged against and may be applied only through
//! the gating edge that judged it. That makes "did the gate actually run, and
//! what did it record" a governance question, not a convenience — and it had
//! no test.
//!
//! Every case runs a host command with the case input on stdin, so `cat` is a
//! perfect echo tool: `expect.equals` against the input passes, anything else
//! fails, with no fixture binary and no model key.

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

/// Pull the trailing hash out of `evalset 'NAME' stored: <hash> (N cases)`.
fn stored_hash(out: &str) -> String {
    let h = out
        .split_whitespace()
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| panic!("no hash in: {out}"));
    h.to_string()
}

fn write_cases(dir: &TempDir, name: &str, json: &str) -> String {
    let p = dir.path().join(name);
    std::fs::write(&p, json).unwrap();
    p.to_str().unwrap().to_string()
}

#[test]
fn an_evalset_is_validated_before_it_is_stored() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("e.db").to_str().unwrap().to_string();

    let good = write_cases(
        &dir,
        "cases.json",
        r#"[{"name": "echo", "input": {"q": 1}, "expect": {"equals": {"q": 1}}}]"#,
    );

    // Both arguments are required, and the error says the whole invocation
    // rather than naming one missing flag at a time.
    let (ok, _out, err) = areev(&["eval", "create", "--db", &db, "--name", "gate"]);
    assert!(!ok, "create without --cases must fail");
    assert!(err.contains("--cases"), "{err}");

    let (ok, _out, err) = areev(&["eval", "create", "--db", &db, "--cases", &good]);
    assert!(!ok, "create without --name must fail");
    assert!(err.contains("--name"), "{err}");

    // An evalset with no cases is a gate that passes everything — refused.
    let empty = write_cases(&dir, "empty.json", "[]");
    let (ok, _out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "g", "--cases", &empty]);
    assert!(!ok, "an empty evalset must be refused");
    assert!(err.contains("at least one case"), "{err}");

    // A case missing its expectation is the dangerous shape: it would score as
    // a failure forever, or (worse) as a pass against nothing.
    let bad = write_cases(&dir, "bad.json", r#"[{"name": "x", "input": {}}]"#);
    let (ok, _out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "g", "--cases", &bad]);
    assert!(!ok, "a case without an expectation must be refused");
    assert!(err.contains("expect"), "{err}");

    let notjson = write_cases(&dir, "notjson.json", "{ not json");
    let (ok, _out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "g", "--cases", &notjson]);
    assert!(!ok, "a malformed cases file must be refused");
    assert!(err.contains("JSON"), "{err}");

    let (ok, out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "gate", "--cases", &good]);
    assert!(ok, "create refused: {err}");
    assert_eq!(stored_hash(&out).len(), 64);
    assert!(out.contains("1 cases"), "{out}");

    let (ok, _out, err) = areev(&["eval", "bless", "--db", &db]);
    assert!(!ok, "an unknown eval subcommand must fail");
    assert!(err.contains("create") && err.contains("run"), "{err}");
}

#[test]
fn a_gate_run_records_its_result_and_fails_the_command_when_a_case_fails() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("e.db").to_str().unwrap().to_string();

    // Two cases against a perfect echo: one expects the echo (passes), one
    // expects something else (fails). A gate that cannot fail is not a gate.
    let cases = write_cases(
        &dir,
        "cases.json",
        r#"[
            {"name": "echoes", "input": {"q": 1}, "expect": {"equals": {"q": 1}}},
            {"name": "does-not", "input": {"q": 2}, "expect": {"equals": {"q": 99}}}
        ]"#,
    );
    let (ok, out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "gate", "--cases", &cases]);
    assert!(ok, "create refused: {err}");
    let hash = stored_hash(&out);

    let (ok, _out, err) = areev(&["eval", "run", "--db", &db, "--evalset", &hash]);
    assert!(!ok, "run without --tool-cmd must fail");
    assert!(err.contains("--tool-cmd"), "{err}");

    let (ok, _out, _err) = areev(&[
        "eval", "run", "--db", &db, "--evalset", "not-a-hash", "--tool-cmd", "cat",
    ]);
    assert!(!ok, "run against a malformed evalset hash must fail");

    // A real grain that is not an evalset must be refused rather than scored
    // as zero cases.
    let (ok, out, err) = areev(&["add", "a", "b", "c", "--db", &db, "--ns", "agent:harness"]);
    assert!(ok, "{err}");
    let plain = stored_hash(&out);
    let (ok, _out, err) = areev(&[
        "eval", "run", "--db", &db, "--evalset", &plain, "--tool-cmd", "cat",
    ]);
    assert!(!ok, "a non-evalset grain must be refused");
    assert!(err.contains("evalset"), "{err}");

    // The real run: one case passes, one fails, and the command exits
    // non-zero *because* a case failed — that non-zero is what a CI gate reads.
    let (ok, out, err) =
        areev(&["eval", "run", "--db", &db, "--evalset", &hash, "--tool-cmd", "cat"]);
    assert!(!ok, "a run with a failing case must exit non-zero");
    assert!(err.contains("1 case(s) failed"), "{err}");

    let report: serde_json::Value = serde_json::from_str(out.trim()).expect("run report is JSON");
    assert_eq!(report["passed"], 1);
    assert_eq!(report["failed"], 1);
    assert_eq!(report["evalset"], hash.as_str());
    let run_id = report["run_id"].as_str().unwrap().to_string();
    assert!(run_id.starts_with("eval-"), "{run_id}");

    let rows = report["cases"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["case"], "echoes");
    assert_eq!(rows[0]["ok"], true);
    assert_eq!(rows[1]["case"], "does-not");
    assert_eq!(rows[1]["ok"], false);

    // The gate is evidence, not just an exit code: every case is journaled
    // under the run id, so `run-trace` can show what the gate actually saw.
    let (ok, out, err) = areev(&["run-trace", "--db", &db, "--ns", "agent:harness", "--run-id", &run_id]);
    assert!(ok, "run-trace failed: {err}");
    assert!(
        out.contains("eval:echoes") || out.contains("Tool"),
        "the gate run must be journaled: {out}"
    );
}

#[test]
fn reacceptance_compares_a_rerun_against_a_recorded_baseline() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("e.db").to_str().unwrap().to_string();

    // An all-passing evalset, so a baseline run is a clean 100%.
    let cases = write_cases(
        &dir,
        "cases.json",
        r#"[
            {"name": "one", "input": {"q": 1}, "expect": {"equals": {"q": 1}}},
            {"name": "two", "input": {"q": 2}, "expect": {"equals": {"q": 2}}}
        ]"#,
    );
    let (ok, out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "gate", "--cases", &cases]);
    assert!(ok, "{err}");
    let hash = stored_hash(&out);

    let (ok, out, err) =
        areev(&["eval", "run", "--db", &db, "--evalset", &hash, "--tool-cmd", "cat"]);
    assert!(ok, "the baseline run should pass every case: {err}");
    let baseline: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(baseline["failed"], 0);
    let baseline_run = baseline["run_id"].as_str().unwrap().to_string();

    // Re-run the SAME evalset against a degraded tool — the model-swap shape.
    // `head -c1` truncates the echo, so both cases now miss.
    let (ok, out, _err) = areev(&[
        "eval", "run", "--db", &db, "--evalset", &hash,
        "--tool-cmd", "head -c1", "--baseline", &baseline_run,
    ]);
    assert!(!ok, "a regressed re-run must exit non-zero");
    let report: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(report["failed"], 2);

    // The comparison must be *recorded*, not just printed: "we re-ran
    // acceptance after the swap" has to be evidence someone can query later.
    let re = &report["reacceptance"];
    assert!(!re.is_null(), "a --baseline run must report a comparison: {report}");
    assert_eq!(re["baseline_run"], baseline_run.as_str());
    assert_eq!(re["baseline_pass_rate"], 100.0);
    assert_eq!(re["pass_rate"], 0.0);
    assert_eq!(re["tolerance_points"], 0.0);
    assert_eq!(
        re["accepted"], false,
        "a drop from 100% to 0% must not pass at the default zero tolerance: {re}"
    );

    // A tolerance wide enough to swallow the drop accepts it — the knob has to
    // actually move, or it is decoration.
    let (_ok, out, _err) = areev(&[
        "eval", "run", "--db", &db, "--evalset", &hash,
        "--tool-cmd", "head -c1", "--baseline", &baseline_run, "--tolerance", "100",
    ]);
    let report: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        report["reacceptance"]["accepted"], true,
        "a 100-point tolerance must accept the same drop: {report}"
    );
    assert_eq!(report["reacceptance"]["tolerance_points"], 100.0);
}
