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

// ── `--model`: grading through the ToolCallLlm seam (the adapter gate) ────

/// Serve `count` canned OpenAI-compatible chat completions on a loopback
/// listener — no live keys, no new dependencies (the areev-llm fixture
/// pattern). Each response carries `usage`, which the seam requires.
fn canned_openai_server(count: usize, content: &str, with_usage: bool) -> String {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut body = serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": content},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 2}
    });
    if !with_usage {
        body.as_object_mut().unwrap().remove("usage");
    }
    let body = body.to_string();
    std::thread::spawn(move || {
        for _ in 0..count {
            let Ok((mut stream, _)) = listener.accept() else { return };
            // Drain headers + Content-Length body bytes, then answer.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let header_end = loop {
                let Ok(n) = stream.read(&mut tmp) else { return };
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let content_length: usize = String::from_utf8_lossy(&buf[..header_end])
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);
            while buf.len() < header_end + content_length {
                let Ok(n) = stream.read(&mut tmp) else { return };
                buf.extend_from_slice(&tmp[..n]);
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn areev_env(args: &[&str], envs: &[(&str, &str)]) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_areev"));
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn areev");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn tool_cmd_and_model_are_one_of() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("e.db").to_str().unwrap().to_string();
    let cases = write_cases(
        &dir,
        "cases.json",
        r#"[{"name": "x", "input": "q", "expect": {"contains": "a"}}]"#,
    );
    let (ok, out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "gate", "--cases", &cases]);
    assert!(ok, "{err}");
    let hash = stored_hash(&out);

    let (ok, _out, err) = areev(&[
        "eval", "run", "--db", &db, "--evalset", &hash,
        "--tool-cmd", "cat", "--model", "openai-compat:x",
    ]);
    assert!(!ok, "both executors must be refused");
    assert!(err.contains("not both"), "{err}");

    let (ok, _out, err) = areev(&["eval", "run", "--db", &db, "--evalset", &hash]);
    assert!(!ok, "no executor must be refused");
    assert!(err.contains("--tool-cmd") && err.contains("--model"), "{err}");
}

#[test]
fn a_model_grades_the_gate_through_an_openai_compatible_endpoint() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("e.db").to_str().unwrap().to_string();

    // Two prompt cases against a model that always answers "k8s": one
    // passes, one fails — a gate that cannot fail is not a gate. The second
    // case's messages-array shape exercises the conversation mapping.
    let cases = write_cases(
        &dir,
        "cases.json",
        r#"[
            {"name": "target", "input": "what is the deploy target?",
             "expect": {"contains": "k8s"}},
            {"name": "cloud",
             "input": [{"role": "system", "content": "answer tersely"},
                        {"role": "user", "content": "which cloud?"}],
             "expect": {"contains": "aws"}}
        ]"#,
    );
    let (ok, out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "gate", "--cases", &cases]);
    assert!(ok, "{err}");
    let hash = stored_hash(&out);

    let base = canned_openai_server(2, "k8s", true);
    let (ok, out, err) = areev_env(
        &[
            "eval", "run", "--db", &db, "--evalset", &hash,
            "--model", "openai-compat:fake-adapter", "--base-url", &base, "--key-env", "FAKE_KEY",
        ],
        &[("FAKE_KEY", "dummy")],
    );
    assert!(!ok, "a run with a failing case must exit non-zero: {err}");
    let report: serde_json::Value = serde_json::from_str(out.trim()).expect("report is JSON");
    assert_eq!(report["passed"], 1, "{out}");
    assert_eq!(report["failed"], 1, "{out}");

    // The recorded edge names WHICH model was judged — part of the evidence.
    let (ok, out, err) = areev(&[
        "cal",
        r#"RECALL facts WHERE relation = "mg:eval_run""#,
        "--db", &db, "--ns", "agent:harness",
    ]);
    assert!(ok, "{err}");
    assert!(
        out.contains("openai-compat:fake-adapter"),
        "the summary must record the graded model: {out}"
    );
}

#[test]
fn model_cases_are_prevalidated_and_record_nothing_on_shape_errors() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("e.db").to_str().unwrap().to_string();
    // An object input is fine for tool-cmd (any JSON on stdin) but has no
    // meaning as a chat — the model path must refuse it BEFORE any case
    // runs, and record no summary (a partial gate is not a gate).
    let cases = write_cases(
        &dir,
        "cases.json",
        r#"[
            {"name": "ok", "input": "hi", "expect": {"contains": "x"}},
            {"name": "shapeless", "input": {"q": 1}, "expect": {"contains": "x"}}
        ]"#,
    );
    let (ok, out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "gate", "--cases", &cases]);
    assert!(ok, "{err}");
    let hash = stored_hash(&out);

    let (ok, _out, err) = areev_env(
        &[
            "eval", "run", "--db", &db, "--evalset", &hash,
            "--model", "openai-compat:x", "--base-url", "http://127.0.0.1:1", "--key-env", "FAKE_KEY",
        ],
        &[("FAKE_KEY", "dummy")],
    );
    assert!(!ok, "a shapeless model case must refuse the whole run");
    assert!(err.contains("shapeless"), "the refusal names the case: {err}");

    let (ok, out, err) = areev(&[
        "cal", r#"RECALL facts WHERE relation = "mg:eval_run""#, "--db", &db, "--ns", "agent:harness",
    ]);
    assert!(ok, "{err}");
    let payload: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        payload["grains"].as_array().map(Vec::len),
        Some(0),
        "a refused run records no gating edge: {out}"
    );
}

#[test]
fn a_response_without_usage_fails_the_case() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("e.db").to_str().unwrap().to_string();
    let cases = write_cases(
        &dir,
        "cases.json",
        r#"[{"name": "target", "input": "q", "expect": {"contains": "k8s"}}]"#,
    );
    let (ok, out, err) =
        areev(&["eval", "create", "--db", &db, "--name", "gate", "--cases", &cases]);
    assert!(ok, "{err}");
    let hash = stored_hash(&out);

    // The seam requires token usage on every response (budgeted runs). A
    // proxy that strips `usage` must fail the case, not silently pass it.
    let base = canned_openai_server(1, "k8s", false);
    let (ok, out, _err) = areev_env(
        &[
            "eval", "run", "--db", &db, "--evalset", &hash,
            "--model", "openai-compat:x", "--base-url", &base, "--key-env", "FAKE_KEY",
        ],
        &[("FAKE_KEY", "dummy")],
    );
    assert!(!ok, "a usage-less response must fail the gate");
    let report: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(report["failed"], 1, "{out}");
}
