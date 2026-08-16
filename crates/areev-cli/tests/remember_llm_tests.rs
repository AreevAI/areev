//! `areev remember` LLM extraction, end-to-end through the real binary.
//!
//! Hermetic: every "model" here is a small python script driven over the
//! `--llm-cmd` subprocess backend (the same fake-LLM pattern the Areev Loop golden
//! suite uses), so no test touches the network or needs a key. The scripts
//! speak the Areev Loop wire protocol — `probe` at construction, then `extract` and
//! `ground`.
//!
//! What is pinned here is the *trust posture*, not the prompt: the raw text
//! lands before the model is called, a failed or garbage extraction never costs
//! it, extracted facts carry model provenance and are `unverified` until a
//! separate grounding pass says otherwise, and nothing is dropped silently.

use std::process::Command;
use tempfile::TempDir;

fn areev(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(args)
        .env_remove("AREEV_DB")
        .output()
        .expect("spawn areev");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn find_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|c| {
        Command::new(c)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// A cooperative fake: two facts, one of them a low-confidence guess the
/// grounder later refuses to support.
const FAKE_LLM_PY: &str = r#"
import json, sys
d = json.loads(sys.stdin.read())
op = d.get("op", "")
if op == "probe":
    print(json.dumps({"model": "fake-extractor-1"}))
elif op == "extract":
    assert "instructions" in d and "content" in d
    print(json.dumps({"facts": [
        {"subject": "john", "relation": "prefers", "object": "window seat", "confidence": 0.9},
        {"subject": "john", "relation": "guess", "object": "likes jazz", "confidence": 0.2},
    ]}))
elif op == "ground":
    print(json.dumps({"results": [
        {"id": c["id"], "supported": "guess" not in c["claim"], "reason": "checked"}
        for c in d.get("claims", [])
    ]}))
else:
    print(json.dumps({}))
"#;

/// Answers the probe, then returns garbage for every real op.
const GARBAGE_LLM_PY: &str = r#"
import json, sys
d = json.loads(sys.stdin.read())
if d.get("op") == "probe":
    print(json.dumps({"model": "garbage-1"}))
else:
    print("I'm afraid I can't do that.")
"#;

/// Answers the probe, then dies — the mid-run failure the source-first
/// ordering exists for.
const DYING_LLM_PY: &str = r#"
import json, sys
d = json.loads(sys.stdin.read())
if d.get("op") == "probe":
    print(json.dumps({"model": "dying-1"}))
else:
    sys.exit(3)
"#;

struct Fake {
    _dir: TempDir,
    cmd: String,
    db: String,
}

/// Write `script` to a temp dir and return the `--llm-cmd` string plus a db
/// path beside it. `None` when there is no python to run (skip, don't fail).
fn fake(script: &str) -> Option<Fake> {
    let py = find_python()?;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("fake_llm.py");
    std::fs::write(&path, script).unwrap();
    let cmd = format!("{py} {}", path.display());
    let db = dir.path().join("m.db").to_str().unwrap().to_string();
    Some(Fake { _dir: dir, cmd, db })
}

fn remember(db: &str, content: &str, extra: &[&str]) -> (bool, serde_json::Value, String) {
    let mut args = vec![
        "remember", "--db", db, "--ns", "caller", "--content", content, "--observer", "voice-agent",
    ];
    args.extend_from_slice(extra);
    let (ok, out, err) = areev(&args);
    let json = serde_json::from_str(out.trim()).unwrap_or_else(|e| {
        panic!("remember did not print JSON ({e}): stdout={out:?} stderr={err:?}")
    });
    (ok, json, err)
}

/// `areev cal '<query>'` → parsed payload.
fn cal(db: &str, query: &str) -> serde_json::Value {
    let (ok, out, err) = areev(&["cal", query, "--db", db, "--ns", "caller"]);
    assert!(ok, "cal failed: {err}");
    serde_json::from_str(out.trim()).expect("cal printed JSON")
}

fn grains(v: &serde_json::Value) -> &Vec<serde_json::Value> {
    v["grains"].as_array().expect("grains array")
}

#[test]
fn extracts_facts_with_model_provenance() {
    let Some(f) = fake(FAKE_LLM_PY) else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let (ok, res, err) = remember(
        &f.db,
        "I always want a window seat.",
        &["--llm-cmd", &f.cmd],
    );
    assert!(ok, "remember failed: {err}");
    assert_eq!(res["model"], "fake-extractor-1");
    assert_eq!(res["proposed"], 2);
    assert_eq!(res["dropped"], 0);
    assert_eq!(res["verification_status"], "unverified");
    assert_eq!(res["facts"].as_array().unwrap().len(), 2);

    // Each fact points back at the event and is stamped unverified —
    // and `unverified` is CAL-filterable, which is what makes the extraction
    // queue reviewable rather than just a pile of new writes.
    let obs = res["event"].as_str().unwrap();
    let rows = cal(&f.db, r#"RECALL facts WHERE verification_status = "unverified""#);
    assert_eq!(grains(&rows).len(), 2, "{rows}");
    for g in grains(&rows) {
        assert_eq!(g["fields"]["derived_from"], obs);
        assert_eq!(g["fields"]["source_type"], "derived");
        // Which model wrote this is answerable from the grain itself.
        assert_eq!(g["fields"]["extractor_model"], "fake-extractor-1");
    }

    // The event still holds the raw text it was extracted from.
    let obs_rows = cal(&f.db, &format!(r#"RECALL events WHERE hash = "{obs}""#));
    let stored = serde_json::to_string(&obs_rows).unwrap();
    assert!(stored.contains("window seat"), "raw content missing: {stored}");
}

#[test]
fn grounding_drops_unsupported_facts_and_marks_the_rest_verified() {
    let Some(f) = fake(FAKE_LLM_PY) else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let (ok, res, err) = remember(
        &f.db,
        "I always want a window seat.",
        &["--llm-cmd", &f.cmd, "--ground-cmd", &f.cmd],
    );
    assert!(ok, "remember failed: {err}");
    assert_eq!(res["proposed"], 2);
    assert_eq!(res["dropped"], 1, "the ungrounded guess is dropped: {res}");
    assert_eq!(res["facts"].as_array().unwrap().len(), 1);
    assert_eq!(res["verification_status"], "verified");

    let rows = cal(&f.db, r#"RECALL facts WHERE verification_status = "verified""#);
    assert_eq!(grains(&rows).len(), 1);
    assert_eq!(grains(&rows)[0]["fields"]["relation"], "prefers");
}

#[test]
fn the_confidence_floor_drops_low_confidence_facts() {
    let Some(f) = fake(FAKE_LLM_PY) else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let (ok, res, err) = remember(
        &f.db,
        "I always want a window seat.",
        &["--llm-cmd", &f.cmd, "--min-confidence", "0.5"],
    );
    assert!(ok, "remember failed: {err}");
    assert_eq!(res["proposed"], 2);
    assert_eq!(res["dropped"], 1);
    assert_eq!(res["facts"].as_array().unwrap().len(), 1);
}

#[test]
fn garbage_from_the_model_still_keeps_the_source_text() {
    let Some(f) = fake(GARBAGE_LLM_PY) else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let (ok, res, err) = remember(&f.db, "some raw note", &["--llm-cmd", &f.cmd]);
    assert!(ok, "an unreadable response is not an error: {err}");
    assert_eq!(res["facts"].as_array().unwrap().len(), 0);
    assert_eq!(res["proposed"], 0);
    assert_eq!(res["event"].as_str().unwrap().len(), 64);

    let rows = cal(&f.db, "RECALL events");
    assert_eq!(grains(&rows).len(), 1, "the raw text survived: {rows}");
}

#[test]
fn a_failed_extraction_reports_the_event_and_exits_non_zero() {
    let Some(f) = fake(DYING_LLM_PY) else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let (ok, res, _err) = remember(&f.db, "some raw note", &["--llm-cmd", &f.cmd]);
    assert!(!ok, "a failed extraction must exit non-zero: {res}");
    // The hash is still printed, so the caller can retry against it.
    assert_eq!(res["event"].as_str().unwrap().len(), 64);
    assert!(res["error"].is_string(), "the failure is reported: {res}");

    let rows = cal(&f.db, "RECALL events");
    assert_eq!(grains(&rows).len(), 1, "the raw text survived: {rows}");
}

#[test]
fn dry_run_extracts_but_stores_nothing() {
    let Some(f) = fake(FAKE_LLM_PY) else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let (ok, res, err) = remember(
        &f.db,
        "I always want a window seat.",
        &["--llm-cmd", &f.cmd, "--dry-run"],
    );
    assert!(ok, "dry run failed: {err}");
    assert_eq!(res["dry_run"], true);
    assert_eq!(res["facts"].as_array().unwrap().len(), 2);
    assert!(res["event"].is_null(), "nothing was stored: {res}");
    assert_eq!(grains(&cal(&f.db, "RECALL events")).len(), 0);
    assert_eq!(grains(&cal(&f.db, "RECALL facts")).len(), 0);
}

#[test]
fn host_supplied_facts_never_call_the_model() {
    // Both flags together: --facts wins and the (dying) model is never built.
    let Some(f) = fake(DYING_LLM_PY) else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let facts = r#"[{"subject":"john","relation":"diet","object":"vegetarian","confidence":0.95}]"#;
    let (ok, res, err) = remember(
        &f.db,
        "I'm vegetarian.",
        &["--facts", facts, "--llm-cmd", &f.cmd],
    );
    assert!(ok, "remember failed: {err}");
    assert_eq!(res["facts"].as_array().unwrap().len(), 1);
    assert!(res["model"].is_null(), "no model ran: {res}");

    // A host asserting its own facts is not relaying a model's claim, so the
    // fact carries no verification status.
    let rows = cal(&f.db, "RECALL facts");
    assert_eq!(grains(&rows).len(), 1);
    assert!(grains(&rows)[0]["fields"]["verification_status"].is_null(), "{rows}");
}

#[test]
fn malformed_facts_json_is_rejected_rather_than_stored_empty() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let db = db.to_str().unwrap();
    let (ok, _out, err) = areev(&[
        "remember", "--db", db, "--ns", "caller", "--content", "note", "--facts",
        r#"[{"subject":"john"}]"#,
    ]);
    assert!(!ok, "an incomplete triple must not become an empty-subject fact");
    assert!(err.contains("facts[0]"), "error names the row: {err}");
}

/// The divergence this write path exists to remove: `areev remember` and the
/// MCP `areev_remember` tool used to store *different grain types* for the
/// same input — an Observation and an Event. They now share `Areev::capture`,
/// so the same input produces the same grain on both surfaces.
///
/// Compared field-by-field rather than by content address: the address covers
/// `created_at`, so two invocations a millisecond apart legitimately differ.
#[test]
fn cli_and_mcp_remember_produce_the_same_grain() {
    use std::io::Write as IoWrite;
    use std::process::Stdio;

    let dir = TempDir::new().unwrap();
    let cli_db = dir.path().join("cli.db").to_str().unwrap().to_string();
    let mcp_db = dir.path().join("mcp.db").to_str().unwrap().to_string();
    let content = "caller asked about refunds";

    let (ok, cli_res, err) = {
        let (ok, out, err) = areev(&[
            "remember", "--db", &cli_db, "--ns", "caller", "--content", content,
            "--observer", "voice-agent", "--session-id", "call-1", "--role", "user",
        ]);
        let json: serde_json::Value = serde_json::from_str(out.trim()).expect("remember JSON");
        (ok, json, err)
    };
    assert!(ok, "cli remember failed: {err}");

    let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["serve", "--mcp", "--db", &mcp_db, "--ns", "caller"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_remove("AREEV_DB")
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        let call = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "areev_remember", "arguments": {
                "content": content, "observer": "voice-agent",
                "session_id": "call-1", "role": "user"}}});
        writeln!(stdin, "{call}").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let line = String::from_utf8_lossy(&out.stdout);
    let resp: serde_json::Value = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().expect("tool result text");
    let mcp_res: serde_json::Value = serde_json::from_str(text).unwrap();

    assert_eq!(mcp_res["stored_as"], "event");
    assert_eq!(mcp_res["hash"], mcp_res["event"], "MCP keeps its documented `hash` key");

    // Both surfaces wrote an Event carrying the same fields.
    let shape = |db: &str, hash: &str| -> serde_json::Value {
        let rows = cal(db, &format!(r#"RECALL events WHERE hash = "{hash}""#));
        let g = grains(&rows)[0].clone();
        assert_eq!(g["grain_type"], "event");
        let f = &g["fields"];
        serde_json::json!({
            "content": f["content"], "session_id": f["session_id"],
            "role": f["role"], "observer": f["observer"], "namespace": f["namespace"],
        })
    };
    assert_eq!(
        shape(&cli_db, cli_res["event"].as_str().unwrap()),
        shape(&mcp_db, mcp_res["event"].as_str().unwrap()),
        "same input must yield the same grain on both surfaces"
    );
}

#[test]
fn a_typo_in_role_is_rejected_before_anything_is_written() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let db = db.to_str().unwrap();
    let (ok, _out, err) = areev(&[
        "remember", "--db", db, "--ns", "caller", "--content", "hi", "--role", "usr",
    ]);
    assert!(!ok, "a bad role must not be silently dropped");
    assert!(err.contains("user|assistant|system|tool"), "error lists the roles: {err}");
    // Rejected before the write, so nothing landed.
    assert_eq!(grains(&cal(db, "RECALL events")).len(), 0);

    // And rejected on the --dry-run path too, which returns early.
    let (ok, _out, err) = areev(&[
        "remember", "--db", db, "--ns", "caller", "--content", "hi", "--role", "usr", "--dry-run",
    ]);
    assert!(!ok, "--dry-run must validate too: {err}");
}

#[test]
fn without_a_model_remember_stores_the_event_alone() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let db = db.to_str().unwrap();
    let (ok, res, err) = remember(db, "just a note", &[]);
    assert!(ok, "remember failed: {err}");
    assert_eq!(res["facts"].as_array().unwrap().len(), 0);
    assert!(res["model"].is_null());
    assert_eq!(res["event"].as_str().unwrap().len(), 64);
}
