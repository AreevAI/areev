//! `areev migrate` / `areev reindex` through the real binary: mem0 export +
//! history land as searchable supersession chains, a Basic Memory vault
//! becomes live memory-tool files, re-runs are no-ops, and reindex makes an
//! `--index-text false` file searchable after the flip.

use std::process::Command;
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
fn mem0_migrate_end_to_end() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let db = db.to_str().unwrap();
    let export = dir.path().join("export.json");
    let history = dir.path().join("history.json");
    std::fs::write(
        &export,
        r#"{"results": [
            {"id": "m-1", "memory": "Works at Initech", "user_id": "u7",
             "created_at": "2024-06-01T10:00:00Z"},
            {"id": "m-2", "memory": "Allergic to peanuts", "user_id": "u7",
             "created_at": "2024-04-01T09:00:00Z"}
        ]}"#,
    )
    .unwrap();
    std::fs::write(
        &history,
        r#"[
            {"memory_id": "m-1", "event": "ADD", "new_memory": "Works at Acme",
             "created_at": "2024-03-01T10:00:00Z"},
            {"memory_id": "m-1", "event": "UPDATE", "new_memory": "Works at Initech",
             "created_at": "2024-06-01T10:00:00Z"}
        ]"#,
    )
    .unwrap();

    let (ok, out, err) = areev(&[
        "migrate", "--from", "mem0", "--file", export.to_str().unwrap(),
        "--history", history.to_str().unwrap(), "--db", db, "--ns", "main",
    ]);
    assert!(ok, "migrate failed: {err}");
    let rep: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(rep["added"], 2, "chain root + m-2: {out}");
    assert_eq!(rep["superseded"], 1, "{out}");

    // history survived the migration as a supersession chain
    let (ok, out, err) = areev(&[
        "history", "--subject", "mem0/m-1", "--relation", "mem0_memory",
        "--db", db, "--ns", "main",
    ]);
    assert!(ok, "history failed: {err}");
    assert_eq!(out.trim().lines().count(), 2, "{out}");

    // prose is BM25-searchable through the hybrid search verb
    let (ok, out, err) = areev(&[
        "search", "--query", "peanuts", "--db", db, "--ns", "main",
    ]);
    assert!(ok, "search failed: {err}");
    assert!(out.contains("Allergic to peanuts"), "{out}");

    // re-run: everything skips, nothing duplicates
    let (ok, out, _) = areev(&[
        "migrate", "--from", "mem0", "--file", export.to_str().unwrap(),
        "--history", history.to_str().unwrap(), "--db", db, "--ns", "main",
    ]);
    assert!(ok);
    let rep: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(rep["added"], 0, "{out}");
}

#[test]
fn basic_memory_vault_becomes_memory_tool_files() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("bm.db");
    let db = db.to_str().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("recipes")).unwrap();
    std::fs::write(
        vault.join("recipes/espresso.md"),
        "---\ntitle: Espresso\npermalink: recipes/espresso\n---\n18g in, 36g out, 28 seconds.\n",
    )
    .unwrap();
    std::fs::write(vault.join("inbox.md"), "Call the venue about the offsite.\n").unwrap();

    let (ok, out, err) = areev(&[
        "migrate", "--from", "basic-memory", "--file", vault.to_str().unwrap(),
        "--db", db, "--ns", "main",
    ]);
    assert!(ok, "migrate failed: {err}");
    let rep: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(rep["added"], 2, "{out}");

    // imported notes are live for the Anthropic memory tool
    let (ok, out, err) = areev(&[
        "memtool", r#"{"command": "view", "path": "/memories"}"#, "--db", db, "--ns", "main",
    ]);
    assert!(ok, "memtool failed: {err}");
    assert!(out.contains("/memories/recipes/espresso"), "{out}");
    assert!(out.contains("/memories/inbox"), "{out}");

    let (ok, out, _) = areev(&[
        "memtool", r#"{"command": "view", "path": "/memories/recipes/espresso"}"#,
        "--db", db, "--ns", "main",
    ]);
    assert!(ok);
    assert!(out.contains("36g out"), "{out}");
}

#[test]
fn reindex_after_index_text_flip() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("edge.db");
    let db = db.to_str().unwrap();

    // voice/edge profile: write with text indexing off
    let (ok, _, err) = areev(&[
        "add", "alice", "prefers", "matcha", "--db", db, "--ns", "main",
        "--index-text", "false",
    ]);
    assert!(ok, "add failed: {err}");
    let (ok, out, _) = areev(&["search", "--query", "matcha", "--db", db, "--ns", "main"]);
    assert!(ok);
    assert!(!out.contains("matcha"), "BM25 leg should be off: {out}");

    // flip the declaration and rebuild in one command
    let (ok, out, err) = areev(&["reindex", "--db", db, "--ns", "main", "--index-text", "true"]);
    assert!(ok, "reindex failed: {err}");
    // Both numbers matter and they are not the same: one grain needed its text
    // column backfilled (it was written with indexing off), and one grain ends
    // up in the index. On a file that already had text, the first is 0 while
    // the second is the whole corpus.
    assert!(out.contains("1 grains indexed"), "{out}");
    assert!(out.contains("1 needed their text backfilled"), "{out}");

    let (ok, out, _) = areev(&["search", "--query", "matcha", "--db", db, "--ns", "main"]);
    assert!(ok);
    assert!(out.contains("matcha"), "{out}");
}

#[test]
fn openai_tool_log_round_trips_typed_calls_and_dedups() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("tools.db");
    let log = dir.path().join("tools.jsonl");
    std::fs::write(
        &log,
        [
            r#"{"created_at":100,"tool_calls":[{"id":"call-1","is_error":true,"function":{"name":"charge","arguments":"{\"amount\":42}"}}]}"#,
            r#"{"created_at":101,"tool_calls":[{"id":"call-2","is_error":true,"function":{"name":"charge","arguments":"{\"amount\":42}"}}]}"#,
            r#"{"created_at":102,"role":"tool","name":"charge","tool_call_id":"call-1","content":"declined","is_error":true}"#,
            r#"{"tool_name":"retry","content":"timeout","is_error":true}"#,
            r#"{"tool_name":"retry","content":"timeout","is_error":true}"#,
            r#"{"tool_name":"retry","content":"timeout","is_error":true}"#,
            r#"{"tool_name":"retry","content":"timeout","is_error":true}"#,
            r#"{"tool_name":"retry","content":"timeout","is_error":true}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let args = [
        "migrate",
        "--from",
        "openai-tools",
        "--file",
        log.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--ns",
        "ops",
    ];
    let (ok, out, err) = areev(&args);
    assert!(ok, "tool migration failed: {err}");
    let report: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(report["added"], 8);

    let (ok, out, err) = areev(&[
        "cal",
        r#"RECALL tools WHERE tool_name = "charge""#,
        "--db",
        db.to_str().unwrap(),
        "--ns",
        "ops",
    ]);
    assert!(ok, "recall imported tools failed: {err}");
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    let grains = payload["grains"].as_array().unwrap();
    assert_eq!(grains.len(), 3, "retries with distinct call ids must survive");
    assert_eq!(
        grains
            .iter()
            .find(|g| g["fields"]["tool_call_id"] == "call-2")
            .unwrap()["fields"]["input"]["amount"],
        42
    );
    assert!(grains.iter().all(|g| g["fields"]["is_error"] == true));
    assert!(
        grains.iter().any(|g| g["fields"]["tool_content"] == "declined"),
        "imported fields: {grains:?}"
    );
    assert!(grains
        .iter()
        .filter(|g| g["fields"]["input"]["amount"] == 42)
        .all(|g| g["fields"].get("content").is_none()), "arguments are never result content");

    let (ok, out, err) = areev(&[
        "cal", r#"RECALL tools WHERE tool_name = "retry""#, "--db",
        db.to_str().unwrap(), "--ns", "ops",
    ]);
    assert!(ok, "recall imported retries failed: {err}");
    let payload: serde_json::Value = serde_json::from_str(&out).unwrap();
    let retries = payload["grains"].as_array().unwrap();
    assert_eq!(retries.len(), 5, "identical log lines are distinct occurrences");
    let mut ids: Vec<&str> = retries
        .iter()
        .filter_map(|g| g["fields"]["tool_call_id"].as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 5, "synthetic import ids must distinguish retries");

    let (ok, out, err) = areev(&args);
    assert!(ok, "repeat tool migration failed: {err}");
    let report: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(report["added"], 0);
    assert_eq!(report["skipped"], 8);
}
