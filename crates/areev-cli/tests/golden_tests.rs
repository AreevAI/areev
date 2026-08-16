//! Golden dataset tests — import a committed, deterministic dataset and
//! assert the *exact* data Areev produces: content hashes, recall results,
//! CAL payloads, render text, and cross-surface (CLI vs MCP) parity.
//!
//! Design mirrors areev's `tests/golden/` suite: a generator with a pinned
//! base epoch builds the dataset (`golden/generator.rs`), the exported
//! bundle + hash manifest are committed under `golden/dataset/`, and every
//! test validates Areev's output against those expectations. Each test
//! imports its own copy of the bundle (single-writer-per-file: concurrent
//! `areev` processes cannot share one memory file).
//!
//! Regenerating after a deliberate dataset change:
//! `cargo test -p areev --test golden_tests -- --ignored bless`
//! then re-bless renders with `GOLDEN_BLESS=1 cargo test -p areev
//! --test golden_tests render` and commit the diff. An *unintended* diff in
//! manifest hashes means canonical serialization changed — a frozen-format
//! break (see `golden_manifest_hashes_stable`).

mod golden;

use golden::generator::{generate, Manifest};
use golden::{
    bundle_path, dataset_dir, areev, grain_hashes, import_golden, manifest, manifest_path,
};
use std::collections::BTreeSet;
use std::io::Write as IoWrite;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Suite 1 — import + integrity
// ---------------------------------------------------------------------------

#[test]
fn golden_import_verifies_clean() {
    let g = import_golden();
    let (ok, out, err) = areev(&["verify", "--db", &g.db]);
    assert!(ok, "verify failed: {err}");
    assert!(out.contains("integrity: ok"), "bad verify: {out}");
    assert!(out.contains("hash mismatches: 0"), "bad verify: {out}");
    assert!(out.contains("undecodable: 0"), "bad verify: {out}");
}

/// Rows the imported file should hold: superseded versions persist, but a
/// forgotten grain ships as a zero-length blob in the bundle and does not
/// materialize as a row on import (only its op-log tombstone replays).
fn expected_rows(m: &Manifest) -> usize {
    m.grains.iter().filter(|e| !e.forgotten).count()
}

#[test]
fn golden_stats_match_manifest() {
    let m = manifest();
    let g = import_golden();
    let (ok, out, err) = areev(&["stats", "--db", &g.db]);
    assert!(ok, "stats failed: {err}");
    assert!(
        out.starts_with(&format!("grains: {}", expected_rows(&m))),
        "expected 'grains: {}' prefix, got: {out}",
        expected_rows(&m)
    );
}

#[test]
fn golden_every_live_hash_is_retrievable() {
    // Every non-forgotten manifest hash must decode from the imported file.
    let m = manifest();
    let g = import_golden();
    let mut store = areev_store::Areev::open(&g.db).expect("open imported db");
    for e in m.grains.iter().filter(|e| !e.forgotten) {
        let h = areev_core::error::Hash::from_hex(&e.hash).expect("manifest hash hex");
        let grain = store.get(&h);
        assert!(grain.is_ok(), "manifest grain not retrievable: {} ({})", e.hash, e.desc);
    }
}

#[test]
fn golden_import_is_idempotent() {
    // Importing the same bundle twice must not duplicate or corrupt.
    let m = manifest();
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("twice.db").to_str().unwrap().to_string();
    for _ in 0..2 {
        let (ok, _out, err) =
            areev(&["import", "--db", &db, "--bundle", bundle_path().to_str().unwrap()]);
        assert!(ok, "import failed: {err}");
    }
    let (ok, out, _err) = areev(&["stats", "--db", &db]);
    assert!(ok);
    assert!(
        out.starts_with(&format!("grains: {}", expected_rows(&m))),
        "double import changed grain count: {out}"
    );
    let (ok, out, _err) = areev(&["verify", "--db", &db]);
    assert!(ok && out.contains("integrity: ok"), "verify after double import: {out}");
}

// ---------------------------------------------------------------------------
// Suite 2 — recall correctness (exact sets, exact hashes)
// ---------------------------------------------------------------------------

/// Manifest hashes matching a predicate, as a set.
fn manifest_hashes(
    m: &Manifest,
    pred: impl Fn(&golden::generator::ManifestEntry) -> bool,
) -> BTreeSet<String> {
    m.grains.iter().filter(|g| pred(g)).map(|g| g.hash.clone()).collect()
}

#[test]
fn golden_recall_john_exact_hashes() {
    let m = manifest();
    let g = import_golden();
    let expected = manifest_hashes(&m, |e| e.desc.starts_with("john "));
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "john" LIMIT 50"#);
    let got: BTreeSet<String> = grain_hashes(&payload).into_iter().collect();
    assert_eq!(got, expected, "john recall returned wrong grain set");
}

#[test]
fn golden_recall_relation_filter_exact() {
    let g = import_golden();
    let payload = g.cal(
        "personal",
        r#"RECALL facts WHERE subject = "john" AND relation = "speaks" LIMIT 10"#,
    );
    let objects: BTreeSet<String> = payload["grains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["fields"]["object"].as_str().unwrap().to_string())
        .collect();
    let expected: BTreeSet<String> = ["german", "english"].iter().map(|s| s.to_string()).collect();
    assert_eq!(objects, expected);
}

#[test]
fn golden_namespace_isolation() {
    // bob lives only in ns work; john only in ns personal.
    let g = import_golden();
    let bob_in_personal = g.cal("personal", r#"RECALL facts WHERE subject = "bob" LIMIT 10"#);
    assert_eq!(grain_hashes(&bob_in_personal).len(), 0, "bob leaked into personal");
    let john_in_work = g.cal("work", r#"RECALL facts WHERE subject = "john" LIMIT 10"#);
    assert_eq!(grain_hashes(&john_in_work).len(), 0, "john leaked into work");
    let bob_in_work = g.cal("work", r#"RECALL facts WHERE subject = "bob" LIMIT 20"#);
    assert_eq!(grain_hashes(&bob_in_work).len(), 8, "bob's work facts wrong count");
}

#[test]
fn golden_recall_limit_and_order_deterministic() {
    // Recall order is insertion recency (op_seq desc), NOT created_at desc:
    // the generator inserts john's facts coffee -> ... -> jazz, so LIMIT 3
    // returns the last three inserted. Pinned here as the ordering contract;
    // if this ever changes it is a deliberate semantics change.
    let g = import_golden();
    let one = g.cal("personal", r#"RECALL facts WHERE subject = "john" LIMIT 3"#);
    let two = g.cal("personal", r#"RECALL facts WHERE subject = "john" LIMIT 3"#);
    let h1 = grain_hashes(&one);
    assert_eq!(h1.len(), 3);
    assert_eq!(h1, grain_hashes(&two), "recall order not stable across runs");
    let objects: Vec<String> = one["grains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["fields"]["object"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        objects,
        vec!["jazz", "1990-03-15", "peanuts"],
        "insertion-recency ordering broke"
    );
}

#[test]
fn golden_text_search_finds_seeded_token() {
    // BM25 leg: the unique token planted in call-001's third utterance.
    let g = import_golden();
    let (ok, out, err) = areev(&[
        "search", "--db", &g.db, "--ns", "shared", "--query", "golden-token-alpha-2", "-k", "3",
    ]);
    assert!(ok, "search failed: {err}");
    assert!(out.contains("golden-token-alpha-2"), "seeded token not found: {out}");
}

// ---------------------------------------------------------------------------
// Suite 3 — CAL semantics
// ---------------------------------------------------------------------------

#[test]
fn golden_cal_count_exact() {
    let g = import_golden();
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "john" COUNT"#);
    assert_eq!(payload["count"].as_i64(), Some(10), "john COUNT: {payload}");
    let payload = g.cal("work", r#"RECALL facts WHERE subject = "bob" COUNT"#);
    assert_eq!(payload["count"].as_i64(), Some(8), "bob COUNT: {payload}");
}

#[test]
fn golden_cal_exists() {
    let g = import_golden();
    let yes =
        g.cal("personal", r#"EXISTS facts WHERE subject = "john" AND relation = "allergic_to""#);
    assert_eq!(yes["exists"].as_bool(), Some(true));
    let no =
        g.cal("personal", r#"EXISTS facts WHERE subject = "john" AND relation = "dislikes""#);
    assert_eq!(no["exists"].as_bool(), Some(false));
}

#[test]
fn golden_supersession_chain_exact() {
    // HISTORY by triple: the kim chain must be senior -> junior -> intern
    // with correct superseded_by links, verified against manifest hashes.
    let m = manifest();
    let g = import_golden();
    let chain: Vec<&golden::generator::ManifestEntry> =
        m.grains.iter().filter(|e| e.desc.starts_with("kim status")).collect();
    assert_eq!(chain.len(), 3, "manifest should carry 3 kim versions");
    let payload = g.cal("personal", r#"HISTORY WHERE subject = "kim" AND relation = "status""#);
    let versions = payload["versions"].as_array().expect("versions");
    assert_eq!(versions.len(), 3, "history depth: {payload}");
    let objects: Vec<&str> = versions.iter().map(|v| v["object"].as_str().unwrap()).collect();
    assert_eq!(objects, vec!["senior", "junior", "intern"]);
    // Head has no successor; each older version points at the next newer one.
    assert!(versions[0]["superseded_by"].is_null());
    assert_eq!(
        versions[1]["superseded_by"].as_str(),
        Some(versions[0]["hash"].as_str().unwrap())
    );
    assert_eq!(
        versions[2]["superseded_by"].as_str(),
        Some(versions[1]["hash"].as_str().unwrap())
    );
    // Hashes match the manifest exactly.
    let manifest_kim: BTreeSet<String> = chain.iter().map(|e| e.hash.clone()).collect();
    let got: BTreeSet<String> =
        versions.iter().map(|v| v["hash"].as_str().unwrap().to_string()).collect();
    assert_eq!(got, manifest_kim);
}

#[test]
fn golden_recall_returns_head_not_superseded() {
    let g = import_golden();
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "kim" LIMIT 10"#);
    let objects: Vec<&str> = payload["grains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["fields"]["object"].as_str().unwrap())
        .collect();
    assert_eq!(objects, vec!["senior"], "recall must return only the head");
}

#[test]
fn golden_forgotten_grain_stays_gone() {
    let g = import_golden();
    let payload = g.cal("work", r#"RECALL facts WHERE subject = "classified" LIMIT 10"#);
    assert_eq!(
        grain_hashes(&payload).len(),
        0,
        "forgotten grain resurfaced after bundle import"
    );
}

#[test]
fn golden_assemble_multi_source() {
    let g = import_golden();
    let payload = g.cal(
        "personal",
        r#"ASSEMBLE "profile" FROM a: (RECALL facts WHERE subject = "john" AND relation = "prefers"), b: (RECALL facts WHERE namespace = "work" AND subject = "bob" AND relation = "prefers") FORMAT sml"#,
    );
    let text = payload["text"].as_str().expect("assemble text");
    assert!(text.contains("coffee") && text.contains("window seat"), "john slice missing: {text}");
    assert!(text.contains("tea"), "bob (cross-ns) slice missing: {text}");
}

#[test]
fn golden_cal_destructive_rejected_with_code() {
    let g = import_golden();
    let (ok, _out, err) = areev(&[
        "cal", r#"DELETE facts WHERE subject = "john""#, "--db", &g.db, "--ns", "personal",
    ]);
    assert!(!ok, "DELETE must not execute");
    assert!(err.contains("CAL-E002"), "expected CAL-E002, got: {err}");
    // And the data is untouched.
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "john" COUNT"#);
    assert_eq!(payload["count"].as_i64(), Some(10));
}

// ---------------------------------------------------------------------------
// Suite 4 — NFC / content-address determinism
// ---------------------------------------------------------------------------

#[test]
fn golden_nfc_hash_equivalence() {
    // The dataset stores "rené ... café münchen" composed. Adding the
    // decomposed spelling (e + U+0301 ...) with the same pinned created_at
    // must produce the *same* content address (S-6 NFC canonicalization).
    use areev_core::types::Grain;
    let m = manifest();
    let anchor = m
        .grains
        .iter()
        .find(|e| e.desc.contains("NFC anchor"))
        .expect("unicode anchor in manifest");
    let dir = TempDir::new().unwrap();
    let mut store =
        areev_store::Areev::open(dir.path().join("nfc.db").to_str().unwrap()).unwrap();
    let mut f = areev_core::types::Fact::new(
        "rene\u{0301}",                 // decomposed é
        "prefers",
        "cafe\u{0301} mu\u{0308}nchen", // decomposed é and ü
    )
    .confidence(1.0);
    f.common.namespace = Some("personal".to_string());
    f.common.created_at = Some(golden::generator::BASE_EPOCH_MS - 11 * 86_400_000);
    let h = store.add(&f).expect("add decomposed");
    assert_eq!(
        h.to_hex(),
        anchor.hash,
        "NFC broke: decomposed spelling produced a different content address"
    );
}

#[test]
fn golden_manifest_hashes_stable() {
    // THE frozen-format guard: regenerating the dataset from source must
    // reproduce every committed hash. A diff here means canonical
    // serialization changed — an OMS conformance break (invariant #2).
    let committed = manifest();
    let dir = TempDir::new().unwrap();
    let fresh = generate(dir.path(), &dir.path().join("fresh.bundle"));
    assert_eq!(fresh.total_grains, committed.total_grains, "grain count drifted");
    for (f, c) in fresh.grains.iter().zip(committed.grains.iter()) {
        assert_eq!(
            f.hash, c.hash,
            "content address drifted for '{}' — canonical serialization changed?",
            c.desc
        );
    }
}

// ---------------------------------------------------------------------------
// Suite 5 — golden renders (exact text)
// ---------------------------------------------------------------------------

fn render_golden(fmt: &str) {
    let g = import_golden();
    let (ok, out, err) = areev(&[
        "recall", "--db", &g.db, "--ns", "personal", "--subject", "john",
        "--render", fmt, "-k", "20",
    ]);
    assert!(ok, "render {fmt} failed: {err}");
    let path = dataset_dir().join("renders").join(format!("john.{fmt}.golden"));
    if std::env::var("GOLDEN_BLESS").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &out).unwrap();
        eprintln!("blessed {}", path.display());
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {} — bless with GOLDEN_BLESS=1", path.display()));
    assert_eq!(out, expected, "{fmt} render drifted from golden file");
}

#[test]
fn golden_render_sml() {
    render_golden("sml");
}

#[test]
fn golden_render_markdown() {
    render_golden("markdown");
}

#[test]
fn golden_render_toon() {
    render_golden("toon");
}

#[test]
fn golden_render_json() {
    render_golden("json");
}

// ---------------------------------------------------------------------------
// Suite 6 — cross-surface parity (CLI vs MCP over real stdio)
// ---------------------------------------------------------------------------

#[test]
fn golden_cli_mcp_parity() {
    use std::process::{Command, Stdio};

    let g = import_golden();
    // CLI leg first — the MCP server below holds the file lock while alive.
    let cli = g.cal("personal", r#"RECALL facts WHERE subject = "john" LIMIT 20"#);
    let cli_hashes: BTreeSet<String> = grain_hashes(&cli).into_iter().collect();

    let rpc = |id: u64, method: &str, params: serde_json::Value| {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string()
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["serve", "--mcp", "--db", &g.db, "--ns", "personal"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp server");
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            rpc(1, "initialize", serde_json::json!({
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "golden", "version": "0"}}))
        )
        .unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
        writeln!(
            stdin,
            "{}",
            rpc(2, "tools/call", serde_json::json!({
                "name": "areev_recall",
                "arguments": {"subject": "john", "k": 20}}))
        )
        .unwrap();
    }
    let out = child.wait_with_output().expect("mcp server exit");
    assert!(out.status.success());
    let resp = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"] == 2)
        .expect("recall response");
    assert_ne!(resp["result"]["isError"], true, "mcp recall errored: {resp}");
    let text = resp["result"]["content"][0]["text"].as_str().expect("content text");
    let mcp_grains: serde_json::Value = serde_json::from_str(text).expect("mcp payload json");
    let mcp_hashes: BTreeSet<String> = mcp_grains
        .as_array()
        .expect("mcp grain array")
        .iter()
        .map(|x| x["hash"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(cli_hashes, mcp_hashes, "CLI and MCP disagree on john's grains");
}

// ---------------------------------------------------------------------------
// Suite 7 — ASSEMBLE x FORMAT combinations (byte-exact)
// ---------------------------------------------------------------------------

/// The canonical two-source ASSEMBLE used across the format grid: john's
/// preferences (session ns) + bob's preferences (explicit work ns).
const ASSEMBLE_PROFILE: &str = r#"ASSEMBLE "profile" FROM a: (RECALL facts WHERE subject = "john" AND relation = "prefers"), b: (RECALL facts WHERE namespace = "work" AND subject = "bob" AND relation = "prefers") FORMAT "#;

fn assemble_golden(fmt: &str) {
    let g = import_golden();
    let payload = g.cal("personal", &format!("{ASSEMBLE_PROFILE}{fmt}"));
    assert_eq!(payload["format"].as_str(), Some(fmt), "format echo: {payload}");
    assert_eq!(payload["grain_count"].as_i64(), Some(3), "2 john + 1 bob prefers");
    let text = payload["text"].as_str().expect("assemble text");
    let path = dataset_dir().join("renders").join(format!("assemble.profile.{fmt}.golden"));
    if std::env::var("GOLDEN_BLESS").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        eprintln!("blessed {}", path.display());
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {} — bless with GOLDEN_BLESS=1", path.display()));
    assert_eq!(text, expected, "ASSEMBLE {fmt} drifted from golden file");
}

#[test]
fn golden_assemble_grid_sml() {
    assemble_golden("sml");
}

#[test]
fn golden_assemble_grid_toon() {
    assemble_golden("toon");
}

#[test]
fn golden_assemble_grid_markdown() {
    assemble_golden("markdown");
}

#[test]
fn golden_assemble_grid_json_hashes() {
    // FORMAT json returns a `grains` payload (not rendered text), so this leg
    // pins the exact hash set instead of golden text.
    let m = manifest();
    let g = import_golden();
    let payload = g.cal("personal", &format!("{ASSEMBLE_PROFILE}json"));
    let got: BTreeSet<String> = grain_hashes(&payload).into_iter().collect();
    let expected = manifest_hashes(&m, |e| {
        e.desc == "john prefers coffee"
            || e.desc == "john prefers window seat"
            || e.desc == "bob prefers tea"
    });
    assert_eq!(got, expected, "ASSEMBLE json grain set drifted");
}

#[test]
fn golden_assemble_budget_truncates_deterministically() {
    // BUDGET must precede FORMAT (see known_bug_budget_after_format_is_dropped
    // for the other order). A 30-token budget over john's 10 facts keeps
    // exactly one grain, and which one it keeps is deterministic.
    let g = import_golden();
    let payload = g.cal(
        "personal",
        r#"ASSEMBLE "mini" FROM a: (RECALL facts WHERE subject = "john") BUDGET 30 tokens FORMAT sml"#,
    );
    assert_eq!(payload["grain_count"].as_i64(), Some(1), "budget truncation: {payload}");
    let text = payload["text"].as_str().expect("text");
    let path = dataset_dir().join("renders").join("assemble.budget30.sml.golden");
    if std::env::var("GOLDEN_BLESS").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        eprintln!("blessed {}", path.display());
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {} — bless with GOLDEN_BLESS=1", path.display()));
    assert_eq!(text, expected, "budgeted ASSEMBLE drifted from golden file");
}

#[test]
fn golden_assemble_per_source_where() {
    // Each source carries its own WHERE; the merge must contain exactly the
    // union (2 prefers + 2 speaks = 4 grains).
    let g = import_golden();
    let payload = g.cal(
        "personal",
        r#"ASSEMBLE "two" FROM p: (RECALL facts WHERE subject = "john" AND relation = "prefers"), s: (RECALL facts WHERE subject = "john" AND relation = "speaks") FORMAT sml"#,
    );
    assert_eq!(payload["grain_count"].as_i64(), Some(4), "{payload}");
    let text = payload["text"].as_str().unwrap();
    for needle in ["coffee", "window seat", "german", "english"] {
        assert!(text.contains(needle), "missing {needle}: {text}");
    }
}

// ---------------------------------------------------------------------------
// Suite 8 — WITH-option combinations (pinning verified semantics)
// ---------------------------------------------------------------------------

#[test]
fn golden_with_limit_before_with_applies() {
    // Clause order matters: LIMIT written BEFORE the WITH clause is honored.
    let g = import_golden();
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "john" LIMIT 3 WITH dedup"#);
    assert_eq!(grain_hashes(&payload).len(), 3);
}

#[test]
fn golden_with_dedup_preserves_distinct_facts() {
    // Similarity dedup must NOT collapse john's 10 genuinely distinct facts.
    let g = import_golden();
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "john" LIMIT 20 WITH dedup"#);
    assert_eq!(grain_hashes(&payload).len(), 10, "dedup dropped distinct facts");
}

#[test]
fn golden_with_min_score_keeps_deterministic_results() {
    // Deterministic (non-ABOUT) recall scores are exactly 1.0, so a 0.99
    // threshold must keep them.
    let g = import_golden();
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "fay" WITH min_score(0.99)"#);
    let grains = payload["grains"].as_array().unwrap();
    assert_eq!(grains.len(), 1);
    assert_eq!(grains[0]["score"].as_f64(), Some(1.0));
    assert_eq!(grains[0]["fields"]["object"].as_str(), Some("matcha"));
}

#[test]
fn golden_with_then_format_combination() {
    // FORMAT after a WITH clause survives parsing (unlike pipeline stages —
    // see the known-bug suite) and renders normally.
    let g = import_golden();
    let payload =
        g.cal("personal", r#"RECALL facts WHERE subject = "kim" WITH superseded FORMAT sml"#);
    assert_eq!(payload["type"].as_str(), Some("formatted"));
    assert_eq!(payload["format"].as_str(), Some("sml"));
    assert!(payload["text"].as_str().unwrap().contains("senior"));
}

// ---------------------------------------------------------------------------
// Suite 9 — known-bug regressions, found by combination probing.
// Each asserts the CORRECT behavior and is #[ignore]d until the bug is
// fixed; un-ignore as part of the fix so they become permanent guards.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "BUG: pipeline stages after a WITH clause are silently dropped (EXPLAIN shows COUNT missing from the plan)"]
fn known_bug_pipeline_after_with_is_dropped() {
    let g = import_golden();
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "kim" WITH superseded COUNT"#);
    assert_eq!(payload["type"].as_str(), Some("count"), "COUNT was swallowed: {payload}");
}

#[test]
#[ignore = "BUG: LIMIT after a WITH clause is silently dropped (EXPLAIN shows limit: None)"]
fn known_bug_limit_after_with_is_dropped() {
    let g = import_golden();
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "john" WITH dedup LIMIT 3"#);
    assert_eq!(grain_hashes(&payload).len(), 3, "LIMIT was swallowed");
}

#[test]
#[ignore = "BUG: BUDGET after FORMAT is silently dropped in ASSEMBLE (works in the documented BUDGET-then-FORMAT order)"]
fn known_bug_budget_after_format_is_dropped() {
    let g = import_golden();
    let payload = g.cal(
        "personal",
        r#"ASSEMBLE "mini" FROM a: (RECALL facts WHERE subject = "john") FORMAT sml BUDGET 30 tokens"#,
    );
    assert_eq!(payload["grain_count"].as_i64(), Some(1), "BUDGET was swallowed: {payload}");
}

#[test]
fn golden_with_superseded_surfaces_the_chain() {
    // Was a known bug: the executor mapped `WITH superseded` to
    // exclude_superseded=false, but every anchored leg was hard-wired to heads
    // (structural `cur=1`, BM25's liveness filter, the vector leg's
    // `svt IS NULL`), so the option was a silent no-op unless the recall
    // happened to be unanchored. Now all three legs widen.
    let g = import_golden();
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "kim" WITH superseded"#);
    assert_eq!(
        grain_hashes(&payload).len(),
        3,
        "WITH superseded should surface the full kim chain"
    );

    // …and the two stale versions must come back *labeled*. Returning history
    // unlabeled would hand a model outdated values that read as current.
    let stale = payload["grains"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| g["fields"]["superseded_by"].is_string())
        .count();
    assert_eq!(stale, 2, "history must be marked superseded, not just returned");
}

#[test]
fn golden_without_superseded_stays_heads_only() {
    // The default is the load-bearing half: stale values must not reach a
    // model's context unless it explicitly asked for history.
    let g = import_golden();
    let payload = g.cal("personal", r#"RECALL facts WHERE subject = "kim""#);
    assert_eq!(grain_hashes(&payload).len(), 1, "default recall must stay heads-only");
    assert!(
        payload["grains"][0]["fields"]["superseded_by"].is_null(),
        "a head carries no supersession label"
    );
}

#[test]
#[ignore = "BUG: OR across subject equalities returns only the first subject (silent partial result)"]
fn known_bug_or_subjects_returns_partial() {
    let g = import_golden();
    let payload = g.cal(
        "personal",
        r#"RECALL facts WHERE subject = "dave" OR subject = "erin" OR subject = "fay" LIMIT 10"#,
    );
    assert_eq!(grain_hashes(&payload).len(), 3, "OR-subject recall silently partial");
}

// ---------------------------------------------------------------------------
// Bless — regenerates the committed dataset (run explicitly, then commit)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "regenerates committed golden files; run explicitly and commit the diff"]
fn bless_golden_dataset() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dataset_dir()).unwrap();
    let m = generate(dir.path(), &bundle_path());
    std::fs::write(
        manifest_path(),
        serde_json::to_string_pretty(&m.to_json()).unwrap() + "\n",
    )
    .unwrap();
    eprintln!(
        "blessed {} grains -> {} + {}",
        m.total_grains,
        bundle_path().display(),
        manifest_path().display()
    );
}
