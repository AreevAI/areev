//! `areev tune` — the tuning seam, through the real binary.
//!
//! The trainer is a fake (`printf` of a canned adapter-reference JSON), so
//! the whole governed pipeline runs keyless and deterministic: corpus →
//! tune → the `adapter_intake` analyzer proposes → the evalset gates →
//! approve/apply writes the `mg:adapter_promotion` Fact → rollback retracts
//! it and the candidate honestly re-proposes.

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

/// Pull the first 64-hex token out of a receipt line.
fn first_hash(out: &str) -> String {
    out.split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_hexdigit()))
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| panic!("no hash in: {out}"))
        .to_string()
}

/// A fake trainer that prints a canned adapter reference on stdout. The
/// command must speak the platform shell (cmd.exe has no printf and treats
/// single quotes as literals).
fn fake_trainer() -> &'static str {
    #[cfg(not(windows))]
    {
        r#"printf '{"adapter":{"uri":"file:///tmp/a.safetensors","sha256":"feedfeed"},"base_model":"qwen3-4b","quantization":"bf16","serving_runtime":"vllm","serves_as":"acme-support"}'"#
    }
    #[cfg(windows)]
    {
        r#"echo {"adapter":{"uri":"file:///tmp/a.safetensors","sha256":"feedfeed"},"base_model":"qwen3-4b","quantization":"bf16","serving_runtime":"vllm","serves_as":"acme-support"}"#
    }
}

/// Seed a memory, an evalset (the E1 pin), and a recorded corpus export.
/// Returns (evalset_hash, corpus_path, manifest_hash).
fn seed(dir: &TempDir, db: &str) -> (String, String, String) {
    let (ok, _out, err) = areev(&["add", "acme", "deploy_target", "k8s", "--db", db]);
    assert!(ok, "seed fact failed: {err}");

    let cases = dir.path().join("cases.json");
    std::fs::write(
        &cases,
        r#"[{"name": "echo", "input": {"q": 1}, "expect": {"equals": {"q": 1}}}]"#,
    )
    .unwrap();
    let (ok, out, err) = areev(&[
        "eval", "create", "--db", db, "--name", "gate", "--cases", cases.to_str().unwrap(),
    ]);
    assert!(ok, "eval create failed: {err}");
    let evalset = first_hash(&out);

    let corpus = dir.path().join("train.jsonl").to_str().unwrap().to_string();
    let (ok, _out, err) = areev(&[
        "corpus", "--db", db, "--select", r#"RECALL facts WHERE subject = "acme""#,
        "--out", &corpus,
    ]);
    assert!(ok, "corpus export failed: {err}");
    let manifest = first_hash(&err); // the receipt is on stderr
    (evalset, corpus, manifest)
}

#[test]
fn tune_validates_its_inputs_before_spawning_anything() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db").to_str().unwrap().to_string();
    let (evalset, corpus, manifest) = seed(&dir, &db);

    // Required flags: the error is the whole usage line.
    let (ok, _out, err) = areev(&["tune", "--db", &db, "--evalset", &evalset]);
    assert!(!ok);
    assert!(err.contains("--cmd"), "{err}");
    let (ok, _out, err) = areev(&["tune", "--db", &db, "--cmd", "true"]);
    assert!(!ok);
    assert!(err.contains("--evalset"), "{err}");

    // The pin must be a live evalset — a plain fact is refused up front.
    let (ok, out, err) = areev(&["add", "x", "y", "z", "--db", &db]);
    assert!(ok, "{err}");
    let plain = first_hash(&out);
    let (ok, _out, err) = areev(&[
        "tune", "--db", &db, "--cmd", "true", "--evalset", &plain,
        "--corpus", &corpus, "--manifest", &manifest,
    ]);
    assert!(!ok, "a non-evalset pin must be refused");
    assert!(err.contains("not an evalset"), "{err}");

    // Exactly one corpus source.
    let (ok, _out, err) = areev(&["tune", "--db", &db, "--cmd", "true", "--evalset", &evalset]);
    assert!(!ok);
    assert!(err.contains("--select") && err.contains("--corpus"), "{err}");
    let (ok, _out, err) = areev(&[
        "tune", "--db", &db, "--cmd", "true", "--evalset", &evalset,
        "--select", "RECALL facts", "--out", "x.jsonl", "--corpus", &corpus, "--manifest", &manifest,
    ]);
    assert!(!ok, "both corpus sources must be refused");
    assert!(err.contains("not both"), "{err}");

    // The corpus is lineage: integrated mode needs a real file.
    let (ok, _out, err) = areev(&[
        "tune", "--db", &db, "--cmd", "true", "--evalset", &evalset, "--select", "RECALL facts",
    ]);
    assert!(!ok);
    assert!(err.contains("--out"), "{err}");
    let (ok, _out, err) = areev(&[
        "tune", "--db", &db, "--cmd", "true", "--evalset", &evalset,
        "--select", "RECALL facts", "--out", "stdout",
    ]);
    assert!(!ok, "stdout cannot feed a trainer");
    assert!(err.contains("file path"), "{err}");

    // Bring-your-own lineage cannot be asserted from the command line.
    let (ok, _out, err) = areev(&[
        "tune", "--db", &db, "--cmd", "true", "--evalset", &evalset,
        "--corpus", &corpus, "--manifest", &"ab".repeat(32),
    ]);
    assert!(!ok, "an unrecorded manifest must be refused");
    assert!(err.contains("not a recorded corpus export"), "{err}");
}

#[test]
fn a_bad_trainer_reply_registers_nothing() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db").to_str().unwrap().to_string();
    let (evalset, corpus, manifest) = seed(&dir, &db);

    // Missing `serves_as` → the reply is rejected and nothing is written.
    #[cfg(not(windows))]
    let bad = r#"printf '{"adapter":{"uri":"u","sha256":"s"},"base_model":"x"}'"#;
    #[cfg(windows)]
    let bad = r#"echo {"adapter":{"uri":"u","sha256":"s"},"base_model":"x"}"#;
    let (ok, _out, err) = areev(&[
        "tune", "--db", &db, "--cmd", bad, "--evalset", &evalset,
        "--corpus", &corpus, "--manifest", &manifest,
    ]);
    assert!(!ok, "an incomplete reply must fail");
    assert!(err.contains("trainer reply rejected"), "{err}");
    assert!(err.contains("serves_as"), "the refusal names the missing field: {err}");

    // A trainer that dies is a trainer failure, not a registration.
    let (ok, _out, err) = areev(&[
        "tune", "--db", &db, "--cmd", "exit 3", "--evalset", &evalset,
        "--corpus", &corpus, "--manifest", &manifest,
    ]);
    assert!(!ok);
    assert!(err.contains("trainer"), "{err}");

    let (ok, out, err) = areev(&[
        "cal", r#"RECALL facts WHERE relation = "mg:adapter""#, "--db", &db, "--ns", "agent:harness",
    ]);
    assert!(ok, "{err}");
    let payload: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        payload["grains"].as_array().map(Vec::len),
        Some(0),
        "no adapter grain may exist after failed tunes: {out}"
    );
}

#[test]
fn the_governed_pipeline_tune_propose_gate_apply_rollback() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db").to_str().unwrap().to_string();
    let (evalset, corpus, manifest) = seed(&dir, &db);

    // 1. tune: fake trainer, bring-your-own corpus. The receipt is JSON on
    // stdout; the guidance (next steps) is stderr.
    let (ok, out, err) = areev(&[
        "tune", "--db", &db, "--cmd", fake_trainer(), "--evalset", &evalset,
        "--corpus", &corpus, "--manifest", &manifest,
    ]);
    assert!(ok, "tune failed: {err}");
    let receipt: serde_json::Value = serde_json::from_str(out.trim()).expect("receipt is JSON");
    assert_eq!(receipt["serves_as"], "acme-support");
    assert_eq!(receipt["evalset"], evalset.as_str());
    assert_eq!(receipt["manifest"], manifest.as_str());
    let adapter_grain = receipt["adapter_grain"].as_str().unwrap().to_string();
    assert!(err.contains("areev loop run"), "guidance must name the next step: {err}");

    // The registry grain is provenance-walkable from the corpus manifest.
    let (ok, out, err) = areev(&["provenance", &manifest, "--db", &db]);
    assert!(ok, "{err}");
    assert!(out.contains(&adapter_grain), "adapter must derive from the manifest: {out}");

    // 2. propose: the builtin adapter_intake analyzer files the
    // recommendation, pinned to the evalset.
    let (ok, _out, err) = areev(&["loop", "run", "--db", &db, "--telemetry", "off"]);
    assert!(ok, "loop run failed: {err}");
    let (ok, out, err) = areev(&["loop", "list", "--format", "json", "--db", &db]);
    assert!(ok, "{err}");
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    let rec = rows
        .iter()
        .find(|r| r["analyzer"].as_str().unwrap_or("").contains("adapter_intake"))
        .unwrap_or_else(|| panic!("adapter_intake must propose: {rows:?}"));
    assert!(
        rec["summary"].as_str().unwrap_or("").contains("acme-support"),
        "{rec}"
    );
    let rec_hash = rec["hash"].as_str().unwrap().to_string();

    // 3. gate: a clean run of the pinned evalset (cat echoes the case).
    let (ok, out, err) =
        areev(&["eval", "run", "--db", &db, "--evalset", &evalset, "--tool-cmd", "cat"]);
    assert!(ok, "gate run failed: {err}");
    let report: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(report["failed"], 0);
    let run_id = report["run_id"].as_str().unwrap().to_string();

    // 4. approve, then apply — REFUSED without the gating edge, admitted
    // with it, writing the promotion Fact hosts re-resolve from.
    let (ok, _out, err) = areev(&[
        "loop", "approve", &rec_hash, "--db", &db,
        "--because", "corpus and lineage reviewed", "--actor", "user:reviewer",
    ]);
    assert!(ok, "approve failed: {err}");
    let (ok, _out, err) = areev(&[
        "loop", "apply", &rec_hash, "--db", &db,
        "--because", "ship it", "--actor", "user:reviewer",
    ]);
    assert!(!ok, "an ungated adapter apply must refuse");
    assert!(err.contains("gating"), "{err}");
    let (ok, _out, err) = areev(&[
        "loop", "apply", &rec_hash, "--db", &db,
        "--because", "gated and green", "--actor", "user:reviewer", "--gating-run", &run_id,
    ]);
    assert!(ok, "gated apply failed: {err}");

    let (ok, out, err) = areev(&[
        "cal", r#"RECALL facts WHERE relation = "mg:adapter_promotion""#,
        "--db", &db, "--ns", "areev-loop",
    ]);
    assert!(ok, "{err}");
    let payload: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let promos = payload["grains"].as_array().unwrap();
    assert_eq!(promos.len(), 1, "exactly one live promotion: {out}");
    assert_eq!(promos[0]["fields"]["subject"], "model:acme-support");
    assert_eq!(promos[0]["fields"]["gating_run_id"], run_id.as_str());

    // 5. While promoted, the model's slot is settled — no new candidate.
    let (ok, _out, err) = areev(&["loop", "run", "--db", &db, "--telemetry", "off"]);
    assert!(ok, "{err}");
    let (_ok, out, _err) = areev(&["loop", "list", "--format", "json", "--db", &db]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(
        rows.iter().all(|r| !r["analyzer"].as_str().unwrap_or("").contains("adapter_intake")),
        "a promoted model must not re-propose: {rows:?}"
    );

    // 6. rollback retracts the promotion (the recorded inverse) and tells
    // the operator the host must stop serving it.
    let (ok, _out, err) = areev(&[
        "loop", "rollback", &rec_hash, "--db", &db,
        "--because", "regressed in prod", "--actor", "user:reviewer",
    ]);
    assert!(ok, "rollback failed: {err}");
    assert!(
        err.contains("stop serving"),
        "the rollback must name the serving consequence: {err}"
    );
    let (_ok, out, _err) = areev(&[
        "cal", r#"RECALL facts WHERE relation = "mg:adapter_promotion""#,
        "--db", &db, "--ns", "areev-loop",
    ]);
    let payload: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        payload["grains"].as_array().map(Vec::len),
        Some(0),
        "rollback must retract the promotion: {out}"
    );

    // 7. …and the still-live candidate honestly re-proposes; retiring the
    // mg:adapter grain is how a host silences it.
    let (ok, _out, err) = areev(&["loop", "run", "--db", &db, "--telemetry", "off"]);
    assert!(ok, "{err}");
    let (_ok, out, _err) = areev(&["loop", "list", "--format", "json", "--db", &db]);
    let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
    assert!(
        rows.iter().any(|r| r["analyzer"].as_str().unwrap_or("").contains("adapter_intake")),
        "post-rollback the candidate must re-propose: {rows:?}"
    );
}

#[test]
fn erasure_names_the_adapters_a_stale_corpus_trained() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("t.db").to_str().unwrap().to_string();
    let (evalset, corpus, manifest) = seed(&dir, &db);

    // Register an adapter trained on the export, then erase the subject the
    // corpus drew from. The notice must reach one provenance hop past the
    // corpus: the adapter derived from it is stale too.
    let (ok, out, err) = areev(&[
        "tune", "--db", &db, "--cmd", fake_trainer(), "--evalset", &evalset,
        "--corpus", &corpus, "--manifest", &manifest,
    ]);
    assert!(ok, "tune failed: {err}");
    let receipt: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let adapter_grain = receipt["adapter_grain"].as_str().unwrap().to_string();

    let (ok, _out, err) = areev(&["forget-subject", "acme", "--db", &db, "--yes"]);
    assert!(ok, "forget-subject failed: {err}");
    assert!(err.contains("is stale and must be retired or re-derived"), "{err}");
    assert!(
        err.contains("adapter model:acme-support") && err.contains("derives from stale corpus"),
        "the notice must name the stale adapter: {err}"
    );

    // The audit record carries the machine-readable list beside stale_corpora.
    let (ok, out, err) = areev(&["audit", "export", "--db", &db]);
    assert!(ok, "audit export failed: {err}");
    let row: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let adapters = row["stale_adapters"].as_array().unwrap_or_else(|| {
        panic!("stale_adapters missing from the audit row: {row}")
    });
    assert_eq!(adapters.len(), 1, "{row}");
    assert_eq!(adapters[0]["subject"], "model:acme-support");
    assert_eq!(adapters[0]["grain_hash"], adapter_grain.as_str());
    assert!(adapters[0]["export_id"].as_str().unwrap().starts_with("export:"));
}
