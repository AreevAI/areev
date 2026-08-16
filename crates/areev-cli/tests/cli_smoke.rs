//! CLI smoke test — the M2 exit flow end-to-end through the binary:
//! add → recall → cal → bundle → import into a second memory → verify.

use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn areev(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(args)
        .output()
        .expect("spawn areev");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn cli_end_to_end() {
    let dir = TempDir::new().unwrap();
    let db_a = dir.path().join("a.db");
    let db_a = db_a.to_str().unwrap();
    let db_b = dir.path().join("b.db");
    let db_b = db_b.to_str().unwrap();
    let bundle = dir.path().join("delta.mgb");
    let bundle = bundle.to_str().unwrap();

    // add
    let (ok, hash, err) = areev(&[
        "add", "--db", db_a, "--ns", "caller", "--subject", "alice", "--relation", "prefers",
        "--object", "tea",
    ]);
    assert!(ok, "add failed: {err}");
    let hash = hash.trim().to_string();
    assert_eq!(hash.len(), 64);

    // recall
    let (ok, out, err) = areev(&[
        "recall", "--db", db_a, "--ns", "caller", "--subject", "alice",
    ]);
    assert!(ok, "recall failed: {err}");
    assert!(out.contains("\"object\":\"tea\"") || out.contains("\"object\": \"tea\""), "{out}");

    // cal — the language from the shell (ADD tier + read tier)
    let (ok, _out, err) = areev(&[
        "cal",
        r#"ADD fact SET subject = "alice" SET relation = "speaks" SET object = "German" SET namespace = "caller" REASON "cli""#,
        "--db", db_a, "--ns", "caller",
    ]);
    assert!(ok, "cal add failed: {err}");
    let (ok, out, err) = areev(&[
        "cal", r#"RECALL facts WHERE subject = "alice" | COUNT"#, "--db", db_a, "--ns", "caller",
    ]);
    assert!(ok, "cal count failed: {err}");
    assert!(out.contains("\"count\": 2") || out.contains("\"count\":2"), "{out}");

    // get by hash
    let (ok, out, _) = areev(&["get", &hash, "--db", db_a]);
    assert!(ok);
    assert!(out.contains("prefers"));

    // bundle → import into fresh memory → recall parity
    let (ok, out, err) = areev(&["bundle", "--db", db_a, "--out", bundle]);
    assert!(ok, "bundle failed: {err}");
    assert!(out.contains("bundled"));
    let (ok, out, err) = areev(&["import", "--db", db_b, "--bundle", bundle]);
    assert!(ok, "import failed: {err}");
    assert!(out.contains("applied"));
    let (ok, out, _) = areev(&[
        "recall", "--db", db_b, "--ns", "caller", "--subject", "alice",
    ]);
    assert!(ok);
    assert!(out.lines().count() == 2, "replica should hold both facts: {out}");

    // verify + stats + log on the replica
    let (ok, out, err) = areev(&["verify", "--db", db_b]);
    assert!(ok, "verify failed: {err}\n{out}");
    assert!(out.contains("integrity: ok"));
    let (ok, out, _) = areev(&["stats", "--db", db_b]);
    assert!(ok);
    assert!(out.contains("grains: 2"));
    let (ok, out, _) = areev(&["log", "--db", db_b]);
    assert!(ok);
    assert_eq!(out.lines().count(), 2);

    // destructive CAL statement fails through the CLI too
    let (ok, _, err) = areev(&["cal", "DELETE sha256:abc", "--db", db_a]);
    assert!(!ok, "DELETE must fail, got success");
    assert!(!err.is_empty());
}

#[test]
fn record_tool_call_cli_keeps_json_arguments() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("tools.db");
    let db = db.to_str().unwrap();
    let (ok, hash, err) = areev(&[
        "record-tool-call",
        "--db", db,
        "--ns", "ops",
        "--name", "stripe_refund",
        "--input", r#"{"amount":42}"#,
        "--result", "rate limited",
        "--is-error",
        "--call-id", "toolu_cli",
    ]);
    assert!(ok, "record-tool-call failed: {err}");
    assert_eq!(hash.trim().len(), 64);

    let (ok, out, err) = areev(&[
        "cal",
        r#"RECALL tools WHERE tool_call_id = "toolu_cli""#,
        "--db", db,
        "--ns", "ops",
    ]);
    assert!(ok, "tool recall failed: {err}");
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(payload["grains"][0]["fields"]["input"]["amount"], 42);
    assert_eq!(payload["grains"][0]["fields"]["is_error"], true);

    let (ok, out, err) = areev(&[
        "run-manifest",
        "--db", db,
        "--run-id", "run-cli",
        "--config", r#"{"model":{"base":"test"},"sampling":{"seed":7}}"#,
    ]);
    assert!(ok, "run-manifest failed: {err}");
    let manifest: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(manifest["config_hash"].as_str().map(str::len), Some(64));
    assert_eq!(manifest["link_hash"].as_str().map(str::len), Some(64));
}

/// Ergonomics: positional `add <s> <r> <o>` + `-d`, and recall resolving the
/// memory file from `$AREEV_DB` when no --db/-d is given.
#[test]
fn version_flag_prints_crate_version() {
    for arg in ["--version", "-V", "version"] {
        let (ok, out, _) = areev(&[arg]);
        assert!(ok, "`areev {arg}` should exit 0");
        assert_eq!(
            out.trim(),
            format!("areev {}", env!("CARGO_PKG_VERSION")),
            "`areev {arg}` prints the crate version"
        );
    }
}

#[test]
fn capture_stop_keeps_the_ordered_typed_transcript() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("code.db");
    let db = db.to_str().unwrap();

    // A Claude Code transcript where the load-bearing signal is a failing
    // tool result, not the final prose. The old capture kept only text blocks
    // and would have dropped it.
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(
        &transcript,
        [
            r#"{"message":{"role":"user","content":[{"type":"text","text":"fix the flaky test"}]}}"#,
            r#"{"message":{"role":"assistant","model":"claude-test","stop_reason":"tool_use","usage":{"input_tokens":12,"output_tokens":3},"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test flaky"}}]}}"#,
            r#"{"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"assertion failed: shared tempdir race"}]}}"#,
            r#"{"message":{"role":"assistant","model":"claude-test","stop_reason":"end_turn","usage":{"input_tokens":20,"output_tokens":8},"content":[{"type":"text","text":"Root cause: tests share a tempdir."}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let hook = serde_json::json!({
        "session_id": "sess-1",
        "transcript_path": transcript.to_str().unwrap(),
    })
    .to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["capture-stop", "--db", db, "--ns", "code"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn capture-stop");
    child.stdin.as_mut().unwrap().write_all(hook.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "capture-stop failed");
    assert!(String::from_utf8_lossy(&out.stdout).contains("captured 4 events"));

    // Stop receives the cumulative transcript. Growing it and invoking the
    // hook again must append only the two new turns, not replay the first four.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    file.write_all(
        br#"
{"message":{"role":"user","content":"please apply the fix"}}
{"message":{"role":"assistant","content":"Fixed and verified."}}
"#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["capture-stop", "--db", db, "--ns", "code"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn second capture-stop");
    child.stdin.as_mut().unwrap().write_all(hook.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "second capture-stop failed");
    assert!(String::from_utf8_lossy(&out.stdout).contains("captured 2 events"));

    // The captured events must carry the tool outcome, flagged as an error.
    let recall = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["cal", "RECALL events RECENT 10", "--db", db, "--ns", "code"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&recall.stdout);
    assert!(text.contains("tool_result ERROR"), "tool error signal missing: {text}");
    assert!(text.contains("shared tempdir race"), "tool output body missing: {text}");
    let payload: serde_json::Value = serde_json::from_slice(&recall.stdout).unwrap();
    let grains = payload["grains"].as_array().unwrap();
    assert_eq!(grains.len(), 6, "the cumulative transcript must not replay: {payload}");
    assert_eq!(
        grains
            .iter()
            .filter(|g| g["fields"]["parent_message_id"].is_string())
            .count(),
        5,
        "every turn after the first must be threaded"
    );
    let tool_use = grains
        .iter()
        .find(|g| g["fields"]["stop_reason"] == "tool_use")
        .expect("typed assistant tool-use event");
    assert_eq!(tool_use["fields"]["model_id"], "claude-test");
    assert_eq!(tool_use["fields"]["token_usage"]["input_tokens"], 12);
    assert_eq!(tool_use["fields"]["content_blocks"][0]["type"], "tool_use");

    // Phase-C corpus export consumes the existing CAL selector, emits OpenAI
    // chat JSONL with step masking, and records an export-manifest grain.
    let corpus = dir.path().join("train.jsonl");
    let (ok, _, err) = areev(&[
        "corpus",
        "--db",
        db,
        "--ns",
        "code",
        "--select",
        r#"RECALL events WHERE session_id = "sess-1""#,
        "--out",
        corpus.to_str().unwrap(),
    ]);
    assert!(ok, "corpus export failed: {err}");
    assert!(err.contains("manifest"), "manifest receipt missing: {err}");
    let row: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&corpus).unwrap().trim()).unwrap();
    assert_eq!(row["messages"].as_array().unwrap().len(), 6);
    assert!(row["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool" && m["content"].as_str().unwrap().contains("tempdir")));
    // Step masking must reach the *call*, not just the environment's reply.
    // A tool_result is weighted 0.0 whether or not it errored, so asserting on
    // one proves nothing — the earlier form of this test passed with the
    // masking deleted.
    let steps = row["steps"].as_array().unwrap();
    let failed_call = steps
        .iter()
        .find(|s| s["kind"] == "tool_call" && s["tool_call_id"] == "toolu_1")
        .expect("the assistant's tool call must appear as a step");
    assert_eq!(
        failed_call["loss_weight"], 0.0,
        "a tool call whose result errored must be masked out of the loss: {failed_call}"
    );
    assert_eq!(failed_call["quality"], "failed");
    // …and masking is targeted: the assistant's prose is still a training target.
    assert!(
        steps
            .iter()
            .any(|s| s["kind"] == "text" && s["loss_weight"] == 1.0),
        "masking must not zero the whole trajectory: {steps:?}"
    );

    let (ok, _, err) = areev(&[
        "corpus",
        "--db",
        db,
        "--select",
        r#"ADD fact SET subject = "x" SET relation = "y" SET object = "z" REASON "no""#,
    ]);
    assert!(!ok, "a write selector must be refused");
    assert!(err.contains("read-only"), "unclear selector refusal: {err}");
}

#[test]
fn corpus_registry_requires_harness_write_grant() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("corpus-auth.db");
    let out = dir.path().join("train.jsonl");
    let db = db.to_str().unwrap();
    let out = out.to_str().unwrap();
    let (ok, _, err) = areev(&[
        "add", "alice", "prefers", "tea", "--db", db, "--ns", "caller",
    ]);
    assert!(ok, "seed failed: {err}");
    let (ok, _, err) = areev(&[
        "add", "agent:reader", "mg:permits", "read ON caller", "--db", db,
        "--ns", "agent:authz",
    ]);
    assert!(ok, "grant failed: {err}");

    let args = [
        "corpus", "--db", db, "--ns", "caller", "--as", "agent:reader",
        "--select", "RECALL facts", "--out", out,
    ];
    let (ok, _, err) = areev(&args);
    assert!(!ok, "read-only principal must not write the registry");
    assert!(err.contains("write") && err.contains("agent:harness"), "{err}");
    assert!(!std::path::Path::new(out).exists(), "denial must precede output creation");

    let (ok, _, err) = areev(&[
        "add", "agent:exporter", "mg:permits",
        "read,write ON caller,agent:harness", "--db", db, "--ns", "agent:authz",
    ]);
    assert!(ok, "export grant failed: {err}");
    let (ok, _, err) = areev(&[
        "corpus", "--db", db, "--ns", "caller", "--as", "agent:exporter",
        "--select", "RECALL facts", "--out", out, "--recipient", "trainer:v1",
    ]);
    assert!(ok, "authorized export failed: {err}");
}

/// `recall-hook --with-loop` closes the loop: pending recommendations ride
/// into the injected context (compact, capped) instead of waiting to be
/// polled; without the flag the hook stays memory-only.
#[test]
fn recall_hook_with_loop_injects_pending_queue() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("w.db");
    let db = db.to_str().unwrap();

    let (ok, _, err) = areev(&["init", "--db", db, "--ns", "caller", "--template", "demo"]);
    assert!(ok, "init demo failed: {err}");
    let (ok, _, err) = areev(&["loop", "run", "--db", db, "--ns", "caller"]);
    assert!(ok, "loop run failed: {err}");

    let hook = serde_json::json!({ "prompt": "what do we know about acme" }).to_string();
    let run_hook = |extra: &[&str]| {
        let mut args = vec!["recall-hook", "--db", db, "--ns", "caller"];
        args.extend_from_slice(extra);
        let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn recall-hook");
        child.stdin.as_mut().unwrap().write_all(hook.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "recall-hook failed");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let with = run_hook(&["--with-loop"]);
    assert!(with.contains("pending recommendation"), "loop block missing: {with}");
    assert!(with.contains("areev loop list"), "review pointer missing: {with}");

    let without = run_hook(&[]);
    assert!(
        !without.contains("pending recommendation"),
        "flagless hook must stay memory-only: {without}"
    );
}

#[test]
fn cli_positional_and_env_db() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("p.db");
    let db = db.to_str().unwrap();

    // positional add with the -d short flag
    let out = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["add", "alice", "prefers", "tea", "-d", db])
        .output()
        .expect("spawn areev");
    assert!(out.status.success(), "positional add: {}", String::from_utf8_lossy(&out.stderr));

    // positional recall, memory file resolved from $AREEV_DB (no --db/-d)
    let out = Command::new(env!("CARGO_BIN_EXE_areev"))
        .env("AREEV_DB", db)
        .args(["recall", "alice"])
        .output()
        .expect("spawn areev");
    assert!(out.status.success(), "env-db recall failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"object\":\"tea\"") || stdout.contains("\"object\": \"tea\""),
        "{stdout}"
    );
}

/// Regression: a boolean flag (`--once`, `--mcp`, `--allow-remote`) must not
/// swallow a following `-d`. `areev serve --mcp -d mem.db` used to lose the
/// path to `--mcp` and silently fall back to the default memory file.
#[test]
fn boolean_flag_does_not_swallow_short_db_flag() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let db = db.to_str().unwrap();
    let seg = dir.path().join("seg");
    let seg = seg.to_str().unwrap();

    let (ok, _, err) = areev(&["add", "bob", "likes", "chess", "-d", db]);
    assert!(ok, "add failed: {err}");

    // --once is boolean; the -d after it must still name the memory file.
    let (ok, _out, err) = areev(&["stream", "--once", "-d", db, "--to", seg]);
    assert!(ok, "stream failed: {err}");
    assert!(
        !err.contains("using default memory"),
        "-d was swallowed by --once; areev fell back to the default file: {err}"
    );
    // The first run into an empty dir opens a generation with a full
    // snapshot, so the proof that `-d` landed is the snapshot's op count.
    assert!(
        err.contains("checkpoint: full snapshot of 1 ops"),
        "expected the grain from {db} in the snapshot: {err}"
    );
}

/// Regression (#16): opening an encrypted memory without the passphrase used
/// to fail with a bare STO-E001 "file is not a database". When a `<db>.kdf`
/// sidecar exists and no key was given, the error must say the file is
/// encrypted and point at `--passphrase-env`.
#[test]
fn encrypted_open_without_passphrase_hints_at_kdf_sidecar() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("enc.db");
    let db = db.to_str().unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["add", "alice", "likes", "tea", "-d", db, "--passphrase-env", "AREEV_TEST_PASS"])
        .env("AREEV_TEST_PASS", "correct horse")
        .output()
        .expect("spawn areev");
    assert!(
        out.status.success(),
        "encrypted add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Same file, no passphrase: must fail, and must say why.
    let (ok, _out, err) = areev(&["recall", "alice", "-d", db]);
    assert!(!ok, "recall without the passphrase unexpectedly succeeded");
    assert!(err.contains(".kdf exists"), "no encryption hint in: {err}");
    assert!(
        err.contains("--passphrase-env"),
        "no --passphrase-env pointer in: {err}"
    );
}

/// `areev hub` is the sync hub (`areevd`). Unlike the console it is a network
/// service by construction, so the shared key is mandatory and a non-loopback
/// bind must be opted into — both refusals happen before anything is served.
#[test]
fn hub_requires_a_key_and_guards_the_bind_address() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("hub.db");
    let db = db.to_str().unwrap();
    let seg = dir.path().join("segments");
    let seg = seg.to_str().unwrap();

    // no --token-env at all
    let (ok, _o, err) = areev(&["hub", "-d", db, "--dir", seg]);
    assert!(!ok, "hub without a token must refuse to start");
    assert!(err.contains("--token-env"), "no pointer to --token-env in: {err}");

    // named variable is not set
    let (ok, _o, err) = areev(&["hub", "-d", db, "--dir", seg, "--token-env", "AREEV_UNSET_VAR"]);
    assert!(!ok, "hub with an unset token variable must refuse to start");
    assert!(err.contains("is not set"), "unhelpful message: {err}");

    // set but empty
    let out = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["hub", "-d", db, "--dir", seg, "--token-env", "AREEV_EMPTY_VAR"])
        .env("AREEV_EMPTY_VAR", "   ")
        .output()
        .expect("spawn areev");
    assert!(!out.status.success(), "hub with an empty token must refuse to start");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("token is empty"),
        "unhelpful message: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // a real key, but bound off-loopback without --allow-remote
    let out = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args([
            "hub", "-d", db, "--dir", seg, "--token-env", "AREEV_HUB_TEST_KEY",
            "--addr", "0.0.0.0:0",
        ])
        .env("AREEV_HUB_TEST_KEY", "a-real-key")
        .output()
        .expect("spawn areev");
    assert!(!out.status.success(), "off-loopback hub must require --allow-remote");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("non-loopback") && err.contains("--allow-remote"),
        "bind guard message is unclear: {err}"
    );
}

/// The graph and as-of reads used to be reachable only by linking the crate.
#[test]
fn cli_graph_and_temporal_verbs() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("g.db");
    let db = db.to_str().unwrap();

    for (s, o) in [("alice", "bob"), ("bob", "carol")] {
        let (ok, _, err) = areev(&[
            "add", "--db", db, "--ns", "org", "--subject", s, "--relation", "reports_to",
            "--object", o,
        ]);
        assert!(ok, "add failed: {err}");
    }

    // Forward walk, two hops.
    let (ok, out, err) = areev(&[
        "related", "--db", db, "--ns", "org", "--start", "alice", "--relations", "reports_to",
    ]);
    assert!(ok, "related failed: {err}");
    assert!(out.contains("bob") && out.contains("carol"), "{out}");

    // Reverse walk uses the OSP index; `reports_to` is entity-valued by default.
    let (ok, out, err) = areev(&[
        "related", "--db", db, "--ns", "org", "--start", "carol", "--relations", "reports_to",
        "--direction", "in",
    ]);
    assert!(ok, "reverse related failed: {err}");
    assert!(out.contains("alice"), "{out}");

    // A bad direction is refused rather than silently defaulting.
    let (ok, _, err) = areev(&[
        "related", "--db", db, "--ns", "org", "--start", "alice", "--relations", "reports_to",
        "--direction", "sideways",
    ]);
    assert!(!ok, "bad direction must fail");
    assert!(err.contains("out, in, both"), "{err}");

    // As-of read on the knowledge axis.
    let (ok, out, err) = areev(&[
        "entity-at", "--db", db, "--ns", "org", "--subject", "alice", "--relation", "reports_to",
        "--at", "4102444800000", "--axis", "knowledge",
    ]);
    assert!(ok, "entity-at failed: {err}");
    assert!(out.contains("bob"), "{out}");

    // An unknown entity answers, it does not error.
    let (ok, out, err) = areev(&[
        "entity-at", "--db", db, "--ns", "org", "--subject", "nobody", "--relation", "reports_to",
        "--at", "4102444800000",
    ]);
    assert!(ok, "entity-at on unknown subject failed: {err}");
    assert!(out.contains("nothing known"), "{out}");
}

#[test]
fn cli_step_actions_reads_workflow_execution_records() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("w.db");
    let db = db.to_str().unwrap();

    let (ok, out, err) = areev(&[
        "cal", "--db", db, "--ns", "ci",
        r#"ADD workflow "pipeline" build -> test REASON "smoke""#,
    ]);
    assert!(ok, "add workflow failed: {err}");
    let wf = out
        .split('"')
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .expect("a workflow hash in the CAL output")
        .to_string();

    // The plan exists and nothing has run against it yet.
    let (ok, out, err) = areev(&["step-actions", "--db", db, "--ns", "ci", "--workflow", &wf]);
    assert!(ok, "step-actions failed: {err}");
    assert!(out.contains("no execution records"), "{out}");

    let (ok, _, err) = areev(&[
        "step-actions", "--db", db, "--ns", "ci", "--workflow", "not-a-hash",
    ]);
    assert!(!ok, "a malformed hash must fail");
    assert!(!err.is_empty());
}

/// The join: run history and semantic memory queried across the seam.
#[test]
fn cli_run_join_verbs() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("j.db");
    let db = db.to_str().unwrap();

    // An event in a run, then a fact distilled from it.
    let (ok, ev, err) = areev(&[
        "remember", "--db", db, "--ns", "ops", "--content", "caller asked about refunds",
        "--session-id", "s1",
    ]);
    assert!(ok, "remember failed: {err}");
    let ev_hash = ev
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|t| t.len() == 64)
        .expect("an event hash")
        .to_string();

    let (ok, _, err) = areev(&[
        "cal", "--db", db, "--ns", "ops",
        &format!(
            r#"ADD fact SET subject = "refunds" SET relation = "window_days" SET object = "30" SET derived_from = "{ev_hash}" REASON "distilled""#
        ),
    ]);
    assert!(ok, "derived add failed: {err}");

    // Nothing carries a run_id here, so the run query answers empty — cleanly.
    let (ok, out, err) = areev(&["run-trace", "--db", db, "--ns", "ops", "--run-id", "nope"]);
    assert!(ok, "run-trace failed: {err}");
    assert!(out.contains("no grains recorded"), "{out}");

    // runs-touching walks provenance and reports honestly when no run made it.
    let (ok, out, err) = areev(&[
        "runs-touching", "--db", db, "--ns", "ops", "--hash", &ev_hash,
    ]);
    assert!(ok, "runs-touching failed: {err}");
    assert!(out.contains("no run produced") || !out.trim().is_empty(), "{out}");

    let (ok, _, err) = areev(&[
        "runs-touching", "--db", db, "--ns", "ops", "--hash", "not-a-hash",
    ]);
    assert!(!ok, "a malformed hash must fail");
    assert!(!err.is_empty());
}

/// `areev remember --run-id` closes the loop on the run/memory join: before it,
/// no CLI verb could put a grain in a run, so `run-trace` could only ever print
/// "no grains recorded" and the index behind it was unreachable from the shell.
#[test]
fn remember_run_id_is_readable_by_run_trace() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("runid.db");
    let db = db.to_str().unwrap();

    for text in ["caller asked about refunds", "agent quoted the 30-day policy"] {
        let (ok, _, err) = areev(&[
            "remember", "--db", db, "--ns", "ops", "--content", text, "--run-id", "run-a",
        ]);
        assert!(ok, "remember --run-id failed: {err}");
    }
    let (ok, _, err) = areev(&[
        "remember", "--db", db, "--ns", "ops", "--content", "different run", "--run-id", "run-b",
    ]);
    assert!(ok, "remember failed: {err}");

    let (ok, out, err) = areev(&["run-trace", "--db", db, "--ns", "ops", "--run-id", "run-a"]);
    assert!(ok, "run-trace failed: {err}");
    assert!(
        out.contains("recorded during run-a: 2 grain(s)"),
        "both of run-a's turns must appear, and only those: {out}"
    );

    let (ok, out_b, err) = areev(&["run-trace", "--db", db, "--ns", "ops", "--run-id", "run-b"]);
    assert!(ok, "run-trace failed: {err}");
    assert!(
        out_b.contains("recorded during run-b: 1 grain(s)"),
        "runs must not bleed into each other: {out_b}"
    );

    // An unknown run still answers cleanly rather than erroring.
    let (ok, out, _) = areev(&["run-trace", "--db", db, "--ns", "ops", "--run-id", "nope"]);
    assert!(ok);
    assert!(out.contains("no grains recorded"), "{out}");
}

/// The DSAR flow through the binary: subject-report shows exactly what
/// forget-subject then erases, and the --bundle export imports elsewhere.
#[test]
fn cli_subject_report_then_erase() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("dsar.db");
    let db = db.to_str().unwrap();
    let bundle = dir.path().join("pat.mgb");
    let bundle = bundle.to_str().unwrap();

    for (s, r, o) in
        [("pat", "prefers", "tea"), ("pat#visit1", "note", "late"), ("mary", "prefers", "juice")]
    {
        let (ok, _, err) = areev(&[
            "add", "--db", db, "--ns", "caller", "--subject", s, "--relation", r, "--object", o,
        ]);
        assert!(ok, "add failed: {err}");
    }

    // The report: two JSONL grains, mary excluded, bundle written.
    let (ok, out, err) =
        areev(&["subject-report", "pat", "--db", db, "--ns", "caller", "--bundle", bundle]);
    assert!(ok, "subject-report failed: {err}");
    let rows: Vec<serde_json::Value> =
        out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 2, "exact + partition key: {out}");
    assert!(rows.iter().all(|r| r["fields"]["subject"] != "mary"), "{out}");
    assert!(err.contains("2 grains"), "summary on stderr: {err}");
    assert!(err.contains("pat#visit1"), "matched identities listed: {err}");

    // The bundle imports into a fresh memory — Art. 20 portability.
    let db2 = dir.path().join("portable.db");
    let db2 = db2.to_str().unwrap();
    let (ok, _, err) = areev(&["import", "--db", db2, "--bundle", bundle]);
    assert!(ok, "import failed: {err}");
    let (ok, out, _) = areev(&["stats", "--db", db2]);
    assert!(ok);
    assert!(out.contains("\"grains\": 2") || out.contains("grains: 2"), "{out}");

    // Then the erasure removes exactly what the report showed.
    let (ok, out, err) =
        areev(&["forget-subject", "pat", "--db", db, "--ns", "caller", "--yes"]);
    assert!(ok, "forget-subject failed: {err}");
    assert!(out.contains("erased 2 grains"), "{out}");
    let (ok, out, _) = areev(&["subject-report", "pat", "--db", db, "--ns", "caller"]);
    assert!(ok);
    assert_eq!(out.trim(), "", "nothing left to report");
}

/// Host-level erasure is audited like the CAL path, and `audit export`
/// emits the evidence as JSONL.
#[test]
fn cli_audit_export_covers_host_erasure() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("audit.db");
    let db = db.to_str().unwrap();

    for (s, r, o) in [("pat", "prefers", "tea"), ("mary", "prefers", "juice")] {
        let (ok, _, err) = areev(&[
            "add", "--db", db, "--ns", "caller", "--subject", s, "--relation", r, "--object", o,
        ]);
        assert!(ok, "add failed: {err}");
    }

    // Nothing destructive yet → an empty (but well-formed) export.
    let (ok, out, err) = areev(&["audit", "export", "--db", db]);
    assert!(ok, "audit export failed: {err}");
    assert_eq!(out.trim(), "");
    assert!(err.contains("0 records"), "{err}");

    // A host-level erasure must leave an audit record — the CLI is a host.
    let (ok, _, err) = areev(&[
        "forget-subject", "pat", "--db", db, "--ns", "caller", "--yes", "--because",
        "gdpr request 42",
    ]);
    assert!(ok, "forget-subject failed: {err}");

    let (ok, out, _) = areev(&["audit", "export", "--db", db]);
    assert!(ok);
    let rows: Vec<serde_json::Value> =
        out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 1, "one destructive op, one audit record: {out}");
    assert_eq!(rows[0]["trail"], "destruction");
    assert_eq!(rows[0]["verb"], "erase");
    // A fingerprint, not the identity — the audit record must not resurrect
    // what the erasure removed. Verifiable by recomputation.
    let fp = areev_core::authz::subject_fingerprint("pat");
    assert_eq!(rows[0]["target"], format!("subject:{fp} ns:caller"));
    assert!(!out.contains("pat "), "the erased identity must not appear in evidence: {out}");
    assert_eq!(rows[0]["because"], "gdpr request 42");
    assert_eq!(rows[0]["grains_erased"], 1);

    // --since windows the export by epoch ms.
    let at = rows[0]["at_ms"].as_i64().unwrap();
    let (ok, out, _) =
        areev(&["audit", "export", "--db", db, "--since", &(at + 1).to_string()]);
    assert!(ok);
    assert_eq!(out.trim(), "", "a window after the event excludes it");

    // --out writes the file instead of stdout.
    let path = dir.path().join("evidence.jsonl");
    let path = path.to_str().unwrap();
    let (ok, out, _) = areev(&["audit", "export", "--db", db, "--out", path]);
    assert!(ok);
    assert!(out.contains("wrote 1 audit records"), "{out}");
    assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 1);
}

#[test]
fn erasure_audit_names_stale_corpus_exports() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("corpus-audit.db");
    let corpus = dir.path().join("training.jsonl");
    let db = db.to_str().unwrap();
    let corpus = corpus.to_str().unwrap();

    let (ok, _, err) = areev(&[
        "add", "--db", db, "--ns", "caller", "--subject", "pat", "--relation",
        "prefers", "--object", "tea",
    ]);
    assert!(ok, "add failed: {err}");
    let (ok, _, err) = areev(&[
        "corpus",
        "--db",
        db,
        "--ns",
        "caller",
        "--select",
        r#"RECALL facts WHERE subject = "pat""#,
        "--out",
        corpus,
        "--recipient",
        "trainer:model-v2",
    ]);
    assert!(ok, "corpus export failed: {err}");

    let (ok, _, err) = areev(&[
        "forget-subject",
        "pat",
        "--db",
        db,
        "--ns",
        "caller",
        "--yes",
    ]);
    assert!(ok, "forget-subject failed: {err}");
    assert!(err.contains("is stale and must be retired or re-derived"), "{err}");
    assert!(err.contains("for recipient trainer:model-v2"), "{err}");

    let (ok, out, err) = areev(&["audit", "export", "--db", db]);
    assert!(ok, "audit export failed: {err}");
    let row: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let stale = row["stale_corpora"].as_array().unwrap();
    assert_eq!(stale.len(), 1, "{row}");
    assert_eq!(stale[0]["destination"], corpus);
    assert_eq!(stale[0]["recipient"], "trainer:model-v2");
    assert!(stale[0]["export_id"].as_str().unwrap().starts_with("export:"));
}

/// Archive retention: a checkpoint snapshots the already-erased live store
/// into a fresh generation, and `--retain` drops the older generations that
/// still hold the pre-erasure bytes. This is the Art. 17 archive
/// guarantee — asserted by grepping the archive for the erased identity.
#[test]
fn cli_stream_checkpoint_and_retention_reach_archives() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("s.db");
    let db = db.to_str().unwrap();
    let arc = dir.path().join("archive");
    let arc = arc.to_str().unwrap();

    let add = |s: &str, o: &str| {
        let (ok, _, err) = areev(&[
            "add", "--db", db, "--ns", "caller", "--subject", s, "--relation", "prefers",
            "--object", o,
        ]);
        assert!(ok, "add failed: {err}");
    };
    add("wellumbrix", "tea"); // a distinctive identity we can grep for
    add("mary", "juice");

    // First stream run opens generation 1 with a full snapshot.
    let (ok, _, err) = areev(&["stream", "--db", db, "--to", arc, "--once"]);
    assert!(ok, "stream failed: {err}");
    let cursor = std::fs::read_to_string(format!("{arc}/CURSOR")).unwrap();
    let gen1 = cursor.split(' ').next().unwrap().to_string();
    assert_eq!(cursor.split(' ').count(), 3, "CURSOR tracks the next segment: {cursor}");
    assert!(
        std::path::Path::new(&format!("{arc}/gen-{gen1}/segment-00000000.mgb")).exists(),
        "segment 0 of a generation is the full snapshot"
    );
    assert!(archive_contains(arc, "wellumbrix"), "the archive holds the identity's bytes");

    // Erase, then checkpoint into a NEW generation with a huge window so
    // the old generation is deliberately kept: the erased bytes must still
    // be findable, proving the grep is a real probe.
    let (ok, _, err) =
        areev(&["forget-subject", "wellumbrix", "--db", db, "--ns", "caller", "--yes"]);
    assert!(ok, "forget-subject failed: {err}");
    let (ok, _, err) =
        areev(&["stream", "--db", db, "--to", arc, "--once", "--checkpoint", "--retain", "30d"]);
    assert!(ok, "checkpoint failed: {err}");
    let gen2 = std::fs::read_to_string(format!("{arc}/CURSOR"))
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .to_string();
    assert_ne!(gen1, gen2, "a checkpoint opens a new generation");
    assert!(
        !archive_contains(&format!("{arc}/gen-{gen2}"), "wellumbrix"),
        "the new generation is snapshotted from the erased store"
    );
    assert!(
        archive_contains(&format!("{arc}/gen-{gen1}"), "wellumbrix"),
        "the pre-erasure generation still holds it until the window expires"
    );

    // Now expire the window (0s = everything older than now) — the old
    // generation goes, and with it the last copy of the erased identity.
    let (ok, _, err) =
        areev(&["stream", "--db", db, "--to", arc, "--once", "--checkpoint", "--retain", "0s"]);
    assert!(ok, "retention sweep failed: {err}");
    assert!(
        !std::path::Path::new(&format!("{arc}/gen-{gen1}")).exists(),
        "generations older than the window are dropped whole"
    );
    assert!(
        !archive_contains(arc, "wellumbrix"),
        "erasure has reached the archive — the Art. 17 guarantee"
    );
    // Mary survives all of it.
    assert!(archive_contains(arc, "mary"), "retention must not lose live data");

    // And the retained archive still restores the surviving store.
    let db2 = dir.path().join("restored.db");
    let db2 = db2.to_str().unwrap();
    let (ok, out, err) = areev(&["restore", "--db", db2, "--from", arc]);
    assert!(ok, "restore failed: {err}");
    assert!(out.contains("restored"), "{out}");
    let (ok, out, _) = areev(&["recall", "--db", db2, "--ns", "caller", "--subject", "mary"]);
    assert!(ok);
    assert!(out.contains("juice"), "restored store keeps live data: {out}");
}

/// True if any `.mgb` file under `path` contains `needle` as raw bytes.
fn archive_contains(path: &str, needle: &str) -> bool {
    fn walk(p: &std::path::Path, needle: &[u8], found: &mut bool) {
        if *found {
            return;
        }
        let Ok(entries) = std::fs::read_dir(p) else { return };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(&path, needle, found);
            } else if path.extension().is_some_and(|x| x == "mgb") {
                if let Ok(bytes) = std::fs::read(&path) {
                    if bytes.windows(needle.len()).any(|w| w == needle) {
                        *found = true;
                        return;
                    }
                }
            }
        }
    }
    let mut found = false;
    walk(std::path::Path::new(path), needle.as_bytes(), &mut found);
    found
}

/// Declarative retention: policy is a file-truth that travels with the
/// memory, declaring never deletes, and the sweep is audited.
#[test]
fn cli_retention_declares_then_enforces() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("r.db");
    let db = db.to_str().unwrap();

    let (ok, _, err) = areev(&[
        "add", "--db", db, "--ns", "caller", "--subject", "keep", "--relation", "r",
        "--object", "v",
    ]);
    assert!(ok, "add failed: {err}");

    // Declaring is inert.
    let (ok, out, err) = areev(&[
        "retention", "set", "--db", db, "--ns", "caller", "--days", "30", "--because",
        "support tickets age out",
    ]);
    assert!(ok, "retention set failed: {err}");
    assert!(out.contains("30 days"), "{out}");
    assert!(err.contains("declared, not enforced"), "{err}");
    let (ok, out, _) = areev(&["recall", "--db", db, "--ns", "caller", "--subject", "keep"]);
    assert!(ok);
    assert!(out.contains("keep"), "declaring a policy must not delete: {out}");

    // The policy is readable back — it lives in the file.
    let (ok, out, _) = areev(&["retention", "list", "--db", db]);
    assert!(ok);
    let row: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(row["namespace"], "caller");
    assert_eq!(row["days"], 30.0);
    assert_eq!(row["because"], "support tickets age out");

    // A sweep without --yes refuses and explains what it would do.
    let (ok, _, err) = areev(&["retention", "sweep", "--db", db]);
    assert!(!ok, "bulk erasure must demand --yes");
    assert!(err.contains("older than 30 days"), "{err}");

    // Nothing is old enough yet, so the sweep is a no-op — and the grain lives.
    let (ok, out, err) = areev(&["retention", "sweep", "--db", db, "--yes"]);
    assert!(ok, "sweep failed: {err}");
    assert!(out.contains("0 grains erased"), "{out}");
    let (ok, out, _) = areev(&["recall", "--db", db, "--ns", "caller", "--subject", "keep"]);
    assert!(ok);
    assert!(out.contains("keep"));
    // A no-op sweep writes no audit grain: the trail records destructions,
    // not the fact that a cron ran.
    let (ok, out, _) = areev(&["audit", "export", "--db", db]);
    assert!(ok);
    assert_eq!(out.trim(), "", "a sweep that erased nothing must not audit: {out}");

    // A zero-day policy makes everything overdue; the sweep erases and audits.
    let (ok, _, err) =
        areev(&["retention", "set", "--db", db, "--ns", "caller", "--days", "0"]);
    assert!(ok, "{err}");
    let (ok, out, err) = areev(&["retention", "sweep", "--db", db, "--yes"]);
    assert!(ok, "sweep failed: {err}");
    assert!(out.contains("1 grains erased"), "{out}");
    let (ok, out, _) = areev(&["audit", "export", "--db", db]);
    assert!(ok);
    let rows: Vec<serde_json::Value> =
        out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 1, "the sweep is audited: {out}");
    assert!(
        rows[0]["target"].as_str().unwrap().starts_with("retention:"),
        "the evidence names the rule that fired: {out}"
    );

    // clear removes the declaration.
    let (ok, _, _) = areev(&["retention", "clear", "--db", db, "--ns", "caller"]);
    assert!(ok);
    let (ok, out, _) = areev(&["retention", "list", "--db", db]);
    assert!(ok);
    assert!(out.contains("no retention policies"), "{out}");
}

/// A positional watermark counted every grain in the `(namespace, session)`
/// thread, so anything else writing there — `areev remember --session-id`, a
/// binding, a second hook — silently consumed the count and skipped that many
/// real turns. Identity now comes from the content address, so a foreign writer
/// is irrelevant.
#[test]
fn capture_stop_is_idempotent_even_when_another_writer_shares_the_thread() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("shared.db");
    let db = db.to_str().unwrap();
    let transcript = dir.path().join("t.jsonl");

    let write_hook = |transcript: &std::path::Path| {
        serde_json::json!({
            "session_id": "sess-shared",
            "transcript_path": transcript.to_str().unwrap(),
        })
        .to_string()
    };
    let run_capture = |hook: &str| -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
            .args(["capture-stop", "--db", db, "--ns", "code"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn capture-stop");
        child.stdin.as_mut().unwrap().write_all(hook.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "capture-stop failed");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    std::fs::write(
        &transcript,
        [
            r#"{"timestamp":"2026-08-14T10:00:00Z","message":{"role":"user","content":"first"}}"#,
            r#"{"timestamp":"2026-08-14T10:00:01Z","message":{"role":"assistant","content":"ack"}}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let hook = write_hook(&transcript);
    assert!(run_capture(&hook).contains("captured 2 events"));

    // A different writer lands three grains in the very same thread.
    for note in ["side note one", "side note two", "side note three"] {
        let status = Command::new(env!("CARGO_BIN_EXE_areev"))
            .args([
                "remember", "--content", note, "--db", db, "--ns", "code",
                "--session-id", "sess-shared", "--role", "user", "--facts", "[]",
            ])
            .status()
            .expect("spawn remember");
        assert!(status.success(), "remember failed");
    }

    // Grow the transcript by two turns and fire the hook again.
    let mut file = std::fs::OpenOptions::new().append(true).open(&transcript).unwrap();
    file.write_all(
        br#"
{"timestamp":"2026-08-14T10:00:02Z","message":{"role":"user","content":"second"}}
{"timestamp":"2026-08-14T10:00:03Z","message":{"role":"assistant","content":"done"}}
"#,
    )
    .unwrap();

    let out = run_capture(&hook);
    assert!(
        out.contains("captured 2 events"),
        "the two new turns must still be captured despite three foreign grains \
         in the thread; got {out:?}"
    );
    assert!(
        out.contains("(2 already stored)"),
        "the first two turns must dedupe by content address, not be re-added; got {out:?}"
    );

    // Firing again with an unchanged transcript stores nothing at all.
    let out = run_capture(&hook);
    assert!(out.contains("captured 0 events"), "replay must be a no-op; got {out:?}");
    assert!(out.contains("(4 already stored)"), "got {out:?}");
}

/// Two corpus fields were emitted structurally but were always empty: the
/// reader looked for elisions under `context.elisions` while the writer put
/// them at the top level, and nothing in the workspace ever produced a policy
/// version. Both are now end-to-end.
#[test]
fn corpus_reports_elisions_and_policy_binding() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("elide.db");
    let db = db.to_str().unwrap();

    // A tool result past the 32 KiB elision threshold.
    let huge = "x".repeat(40 * 1024);
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(
        &transcript,
        [
            r#"{"message":{"role":"user","content":[{"type":"text","text":"dump the log"}]}}"#.to_string(),
            r#"{"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_9","name":"Bash","input":{"command":"cat big.log"}}]}}"#.to_string(),
            format!(
                r#"{{"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_9","content":"{huge}"}}]}}}}"#
            ),
        ]
        .join("\n"),
    )
    .unwrap();

    let hook = serde_json::json!({
        "session_id": "sess-e",
        "transcript_path": transcript.to_str().unwrap(),
    })
    .to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args([
            "capture-stop", "--db", db, "--ns", "code",
            "--policy-version", "policy-2026-08",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn capture-stop");
    child.stdin.as_mut().unwrap().write_all(hook.as_bytes()).unwrap();
    assert!(child.wait_with_output().unwrap().status.success());

    let corpus = dir.path().join("train.jsonl");
    let (ok, _, err) = areev(&[
        "corpus", "--db", db, "--ns", "code",
        "--select", r#"RECALL events WHERE session_id = "sess-e""#,
        "--out", corpus.to_str().unwrap(),
    ]);
    assert!(ok, "corpus export failed: {err}");
    let row: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&corpus).unwrap().trim()).unwrap();

    let elisions = row["observation_elisions"].as_array().unwrap();
    assert_eq!(
        elisions.len(),
        1,
        "a truncated observation must be declared, not silently trained on: {row}"
    );
    assert_eq!(elisions[0]["detail"]["kind"], "tool_result");
    assert_eq!(
        elisions[0]["detail"]["original_bytes"].as_u64().unwrap(),
        40 * 1024,
        "the pre-truncation size is what makes the elision auditable"
    );

    assert_eq!(
        row["binding"]["policy_versions"],
        serde_json::json!(["policy-2026-08"]),
        "the governance regime that produced the turn must reach the row: {row}"
    );
}

/// The 10-minute proof, through the real binary: seed the demo plan, start
/// (parks on the approval), respond as a SECOND principal (the starter is
/// refused — separation of duties), resume to completion, verify the
/// journal-consistent replay, and read the journal back through run-trace.
/// No LLM key anywhere.
#[test]
fn run_demo_end_to_end() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("demo.db");
    let db = db.to_str().unwrap();

    // Seed the demo plan and pull the workflow hash out of the output.
    let (ok, out, err) = areev(&["run", "--db", db, "--ns", "ops", "demo"]);
    assert!(ok, "demo seed failed: {err}");
    let wf = out
        .lines()
        .find_map(|l| l.strip_prefix("demo plan seeded (workflow "))
        .and_then(|l| l.strip_suffix(")"))
        .expect("workflow hash in demo output")
        .to_string();
    assert_eq!(wf.len(), 64);

    // Start: greet executes via --tool-cmd; approve parks the run. The
    // command must speak the platform shell: cmd.exe has no printf and
    // treats single quotes as literals.
    #[cfg(not(windows))]
    let greet_cmd = r#"printf '{"greeting":"hello"}'"#;
    #[cfg(windows)]
    let greet_cmd = r#"echo {"greeting":"hello"}"#;
    let (ok, out, err) = areev(&[
        "run", "--db", db, "--ns", "ops", "start", "--workflow", &wf, "--run-id", "demo-1",
        "--input", r#"{"who":"world"}"#, "--tool-cmd", greet_cmd,
    ]);
    assert!(ok, "start failed: {err}");
    let envelope: serde_json::Value =
        serde_json::from_str(out.trim()).expect("requires_action envelope");
    assert_eq!(envelope["kind"], "requires_action");
    let ask = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();

    // The starting principal may not approve its own run's ask.
    let (ok, _out, err) = areev(&[
        "run", "--db", db, "--ns", "ops", "respond", "--run-id", "demo-1", "--ask", &ask,
        "--result", r#"{"approved":true}"#,
    ]);
    assert!(!ok, "self-approval must be refused");
    assert!(err.contains("RUN-E012") || err.contains("responder"), "{err}");

    // A second principal approves; resume completes; verify passes.
    let (ok, _out, err) = areev(&[
        "run", "--db", db, "--ns", "ops", "respond", "--run-id", "demo-1", "--ask", &ask,
        "--result", r#"{"approved":true}"#, "--as", "user:officer",
    ]);
    assert!(ok, "officer respond failed: {err}");
    let (ok, out, err) = areev(&["run", "--db", db, "--ns", "ops", "resume", "--run-id", "demo-1"]);
    assert!(ok, "resume failed: {err}");
    assert!(out.contains("Completed"), "{out}");
    let (ok, out, err) = areev(&["run", "--db", db, "--ns", "ops", "verify", "--run-id", "demo-1"]);
    assert!(ok, "verify failed: {err}\n{out}");
    assert!(out.contains("\"verified\": true"), "{out}");

    // The journal is queryable through the standing surfaces.
    let (ok, out, err) = areev(&["run-trace", "--db", db, "--ns", "ops", "--run-id", "demo-1"]);
    assert!(ok, "run-trace failed: {err}");
    assert!(out.contains("recorded during demo-1"), "{out}");
    // Intents + results (Tool) and checkpoints (State) — the whole journal.
    assert!(out.contains("Tool") && out.contains("State"), "{out}");
}

#[test]
fn anonymize_scan_is_pure_text_and_fails_closed_on_bad_policy() {
    // `--text` mode: no --db, no store open, JSON out with the pin span.
    let (ok, out, err) = areev(&[
        "anonymize", "scan", "--text", "my user name is john, and pin number is 1462",
    ]);
    assert!(ok, "anonymize scan failed: {err}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("scan prints JSON");
    let cats: Vec<&str> = v["detections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["category"].as_str().unwrap())
        .collect();
    assert!(cats.contains(&"pin"), "expected a pin detection, got {cats:?}");

    // stdin mode.
    let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["anonymize", "scan"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn areev");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"reach me at a@b.co")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdin scan prints JSON");
    assert_eq!(v["detections"][0]["category"], "email");

    // An unreadable policy is a hard refusal (D3), not a silent no-policy.
    let dir = TempDir::new().unwrap();
    let policy = dir.path().join("bad.json");
    std::fs::write(&policy, r#"{"surprise": 1}"#).unwrap();
    let (ok, _out, err) = areev(&[
        "anonymize", "scan", "--text", "x", "--policy-file", policy.to_str().unwrap(),
    ]);
    assert!(!ok, "bad policy must refuse");
    assert!(err.contains("VAL-E001"), "want VAL-E001 in: {err}");
}

#[test]
fn anonymize_policy_verbs_and_floor_cover_recall() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("anon.db");
    let db = db.to_str().unwrap();

    let (ok, _out, err) = areev(&[
        "add", "--db", db, "--ns", "caller", "--subject", "caller:john", "--relation",
        "prefers", "--object", "call me at +1 415 555 0142",
    ]);
    assert!(ok, "add failed: {err}");

    // Declare: recall output is pseudonymized from now on.
    let (ok, _out, err) = areev(&[
        "anonymize", "set", "--db", db, "--ns", "caller", "--policy", r#"{"mode": "egress"}"#,
    ]);
    assert!(ok, "anonymize set failed: {err}");
    let (ok, out, _err) = areev(&["recall", "--db", db, "--ns", "caller", "--subject", "caller:john"]);
    assert!(ok);
    assert!(!out.contains("caller:john") && !out.contains("415 555"), "leaked: {out}");
    assert!(out.contains("[PERSON_1]"), "expected pseudonym: {out}");

    // list / clear
    let (ok, out, _err) = areev(&["anonymize", "list", "--db", db]);
    assert!(ok && out.contains("\"egress\""), "list: {out}");
    let (ok, _out, err) = areev(&["anonymize", "clear", "--db", db, "--ns", "caller"]);
    assert!(ok, "clear failed: {err}");
    let (ok, out, _err) = areev(&["recall", "--db", db, "--ns", "caller", "--subject", "caller:john"]);
    assert!(ok && out.contains("caller:john"), "cleared policy must restore raw reads: {out}");

    // The floor is a per-invocation host cap, no declared policy needed.
    let (ok, out, _err) = areev(&[
        "recall", "--db", db, "--ns", "caller", "--subject", "caller:john", "--anonymize-egress",
    ]);
    assert!(ok);
    assert!(!out.contains("caller:john"), "floor must transform: {out}");
}

#[test]
fn anonymize_vault_reveal_through_the_binary() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("vault.db");
    let db = db.to_str().unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_areev"))
            .args(args)
            .env("SMOKE_PASS", "correct horse battery staple")
            .output()
            .expect("spawn areev");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };

    let (ok, _out, err) = run(&[
        "add", "--db", db, "--passphrase-env", "SMOKE_PASS", "--ns", "caller",
        "--subject", "caller:john", "--relation", "prefers", "--object", "tea",
    ]);
    assert!(ok, "add failed: {err}");
    let (ok, _out, err) = run(&[
        "anonymize", "set", "--db", db, "--passphrase-env", "SMOKE_PASS", "--ns", "caller",
        "--policy", r#"{"mode": "egress", "scope": "session", "vault": true}"#,
    ]);
    assert!(ok, "set failed: {err}");

    // A recall mints + persists the vault row; this process exits after.
    let (ok, out, err) = run(&[
        "recall", "--db", db, "--passphrase-env", "SMOKE_PASS", "--ns", "caller",
        "--subject", "caller:john",
    ]);
    assert!(ok, "recall failed: {err}");
    assert!(out.contains("[PERSON_1]") && !out.contains("caller:john"), "{out}");

    // A LATER process reverse-looks the token up from the sealed vault.
    let (ok, out, err) = run(&[
        "anonymize", "reveal", "--db", db, "--passphrase-env", "SMOKE_PASS", "--ns", "caller",
        "--token", "[PERSON_1]",
    ]);
    assert!(ok, "reveal failed: {err}");
    assert!(out.contains("caller:john"), "reveal must recover the identity: {out}");
}

#[test]
fn ns_pattern_guards_through_the_binary() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("scope.db");
    let db = db.to_str().unwrap();

    // Seed a small tree.
    for ns in ["org", "org.sales", "personal"] {
        let (ok, _, err) = areev(&[
            "add", "--db", db, "--ns", ns, "--subject", "acme", "--relation", "hq",
            "--object", &format!("value-{ns}"),
        ]);
        assert!(ok, "seed add failed: {err}");
    }

    // A prefix scope answers exactly the tree.
    let (ok, out, err) = areev(&["recall", "acme", "--db", db, "--ns", "org.*"]);
    assert!(ok, "scoped recall failed: {err}");
    assert_eq!(out.lines().count(), 2, "org + org.sales only: {out}");
    assert!(!out.contains("value-personal"), "personal leaked: {out}");

    // A write into a pattern namespace refuses with the VAL code.
    let (ok, _, err) = areev(&[
        "add", "--db", db, "--ns", "org.*", "--subject", "s", "--relation", "r", "--object", "o",
    ]);
    assert!(!ok, "a wildcard write must fail");
    assert!(err.contains("VAL-E001"), "refusal must carry the code: {err}");

    // A malformed pattern errors loudly instead of matching nothing.
    let (ok, _, err) = areev(&["recall", "acme", "--db", db, "--ns", "org*"]);
    assert!(!ok, "org* must refuse");
    assert!(err.contains("VAL-E001"), "{err}");

    // Destruction never takes a pattern.
    let (ok, _, err) = areev(&[
        "purge-older-than", "0", "--db", db, "--ns", "org.*", "--yes",
    ]);
    assert!(!ok, "a wildcard purge must fail");
    assert!(err.contains("VAL-E001") || err.contains("exact namespace"), "{err}");
}
