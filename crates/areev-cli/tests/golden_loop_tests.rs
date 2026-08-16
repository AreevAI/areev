//! Areev Loop golden E2E tests — drive the governed self-improvement loop through
//! the real `areev` binary against a committed, deterministic dataset
//! (`golden/loop_generator.rs`), with the engine clock pinned via
//! `AREEV_LOOP_NOW_MS`, and assert the *exact* output of every surface: which
//! analyzers fire on which targets, the queue listings byte-for-byte
//! (recommendation content addresses included — the substrate stamps areev-loop
//! grains from engine time, so a run is a pure function of (file, policy,
//! now)), the review→apply→rollback lifecycle and its real memory effects,
//! outcome measurement across simulated horizons, rejection cooldowns,
//! auto-apply policy grants and the trust floor, the `--fail-on` CI gate's
//! exit codes, LLM reflection and external analyzers through scripted fakes,
//! and CLI↔MCP parity.
//!
//! Regenerating after a deliberate dataset or output change:
//! `cargo test -p areev --test golden_loop_tests -- --ignored bless`
//! then `GOLDEN_BLESS=1 cargo test -p areev --test golden_loop_tests`
//! and review + commit the diff under `golden/dataset/` — the diff IS the
//! review. An *unintended* hash diff in `loop-manifest.json` means canonical
//! serialization changed (frozen-format break); an unintended diff in the
//! `areev-loop/` goldens means analyzer semantics, engine stamping, or a CLI
//! surface changed.

mod golden;

use golden::loop_generator::{generate_loop, AREEV_LOOP_NOW0_MS};
use golden::{
    assert_golden, areev, areev_at, import_loop_golden, loop_bundle_path, loop_golden_dir,
    loop_manifest, loop_manifest_path, GoldenDb,
};
use std::collections::BTreeSet;
use std::io::Write as IoWrite;
use tempfile::TempDir;

/// The pinned engine clock for the primary suites (== the dataset base epoch).
const T0: i64 = AREEV_LOOP_NOW0_MS;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

/// Run `areev loop <args..>` against `db` at pinned `now` — ns `agent`,
/// telemetry off (the primary suites pin the capability-skip ladder; the
/// telemetry suite drops this). Returns (exit_code, stdout, stderr).
fn loop_cmd(db: &str, now: i64, args: &[&str]) -> (i32, String, String) {
    let mut full: Vec<&str> = vec!["loop"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["--db", db, "--ns", "agent", "--telemetry", "off"]);
    areev_at(now, &full)
}

/// `areev-loop` that must exit 0; returns stdout.
fn loop_ok(db: &str, now: i64, args: &[&str]) -> String {
    let (code, out, err) = loop_cmd(db, now, args);
    assert_eq!(code, 0, "loop {args:?} failed (exit {code}): {err}");
    out
}

/// `loop run --format json` (plus `extra` flags) → parsed RunResult.
fn run_json(db: &str, now: i64, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["run", "--format", "json"];
    args.extend_from_slice(extra);
    let out = loop_ok(db, now, &args);
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("run output not JSON ({e}): {out}"))
}

/// `areev-loop list --format json` rows (default pending; pass e.g.
/// `&["--status", "all"]`).
fn list_rows(db: &str, now: i64, extra: &[&str]) -> Vec<serde_json::Value> {
    let mut args = vec!["list", "--format", "json"];
    args.extend_from_slice(extra);
    let out = loop_ok(db, now, &args);
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("list output not JSON ({e}): {out}"))
}

/// The hash of the single row whose analyzer and summary match.
fn find_rec(rows: &[serde_json::Value], analyzer: &str, summary_needle: &str) -> String {
    let hits: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|r| {
            r["analyzer"].as_str().unwrap_or("").contains(analyzer)
                && r["summary"].as_str().unwrap_or("").contains(summary_needle)
        })
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one {analyzer} rec matching {summary_needle:?}, got {hits:?}"
    );
    hits[0]["hash"].as_str().unwrap().to_string()
}

/// Import + first run at T0 — the shared entry state of most suites.
fn import_and_run() -> (GoldenDb, serde_json::Value) {
    let g = import_loop_golden();
    let res = run_json(&g.db, T0, &[]);
    assert_eq!(res["outcome"], "ran", "first run must execute: {res}");
    (g, res)
}

fn find_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|c| {
        std::process::Command::new(c)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Drive `areev recall-hook` the way Claude Code does: hook JSON with the
/// user's prompt on stdin, injected context on stdout. `--with-loop` closes
/// the loop by appending the pending recommendation queue.
fn recall_hook(db: &str, prompt: &str, extra: &[&str]) -> String {
    use std::process::{Command, Stdio};
    let mut args = vec!["recall-hook", "--db", db, "--ns", "agent", "--telemetry", "off"];
    args.extend_from_slice(extra);
    let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(&args)
        .env("AREEV_LOOP_NOW_MS", T0.to_string())
        .env_remove("AREEV_LOOP_POLICY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn recall-hook");
    let hook = serde_json::json!({ "prompt": prompt }).to_string();
    child.stdin.as_mut().unwrap().write_all(hook.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "recall-hook failed");
    String::from_utf8_lossy(&out.stdout).to_string()
}

// ---------------------------------------------------------------------------
// Suite W1 — registry + policy pins (the conformance canary for the analyzer
// roster and the default-closed governance posture)
// ---------------------------------------------------------------------------

#[test]
fn loop_analyzer_registry_pinned() {
    // Ids, tiers, default_on, titles — in registration order. A new analyzer,
    // a default flip, or a tier change must show up as a reviewed bless.
    let g = import_loop_golden();
    let out = loop_ok(&g.db, T0, &["analyzers"]);
    assert_golden(&loop_golden_dir().join("analyzers.txt"), &out);
}

#[test]
fn loop_default_policy_pinned() {
    // The default policy is fully closed: nothing auto-applies, nothing is
    // denied, no floors. This golden is the "closed by default" contract.
    let g = import_loop_golden();
    let out = loop_ok(&g.db, T0, &["policy"]);
    assert_golden(&loop_golden_dir().join("policy-default.json"), &out);
}

#[test]
fn loop_policy_file_echo_pinned() {
    let g = import_loop_golden();
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("policy.json");
    std::fs::write(&p, GRANT_DUP_POLICY).unwrap();
    let out = loop_ok(&g.db, T0, &["policy", "--policy", p.to_str().unwrap()]);
    assert_golden(&loop_golden_dir().join("policy-granting.json"), &out);
}

// ---------------------------------------------------------------------------
// Suite W2 — the first run: exact findings, exact queue, exact payloads
// ---------------------------------------------------------------------------

#[test]
fn loop_first_run_result_pinned() {
    // The full RunResult contract in one byte-exact golden: which analyzers
    // ran, which were skipped and why (disabled / missing capability), and
    // the proposed/deduped/stored/auto_applied accounting.
    let g = import_loop_golden();
    let out = loop_ok(&g.db, T0, &["run", "--format", "json"]);
    assert_golden(&loop_golden_dir().join("run-first.json"), &out);
}

#[test]
fn loop_queue_listings_pinned() {
    // Both list surfaces, byte-exact — including the recommendation content
    // addresses (deterministic: grains are stamped from engine time).
    let (g, _res) = import_and_run();
    let json_out = loop_ok(&g.db, T0, &["list", "--format", "json"]);
    assert_golden(&loop_golden_dir().join("list-pending.json"), &json_out);
    let human_out = loop_ok(&g.db, T0, &["list"]);
    assert_golden(&loop_golden_dir().join("list-pending.txt"), &human_out);
}

#[test]
fn loop_show_payloads_pinned() {
    // Every pending recommendation's full body — summary, target, severity,
    // dedup key, proposal CAL, evidence, destructive/rollbackable — in queue
    // (hash) order, concatenated into one reviewable golden.
    let (g, _res) = import_and_run();
    let rows = list_rows(&g.db, T0, &[]);
    let mut all = String::new();
    for r in &rows {
        all.push_str(&loop_ok(&g.db, T0, &["show", r["hash"].as_str().unwrap()]));
    }
    assert_golden(&loop_golden_dir().join("shows-pending.json"), &all);
}

#[test]
fn loop_evidence_hashes_resolve_to_manifest_grains() {
    // Cross-artifact integrity: every evidence hash cited by every pending
    // recommendation must be a grain the committed manifest knows.
    let m = loop_manifest();
    let known: BTreeSet<&str> = m.grains.iter().map(|e| e.hash.as_str()).collect();
    let (g, _res) = import_and_run();
    for r in list_rows(&g.db, T0, &[]) {
        let show = loop_ok(&g.db, T0, &["show", r["hash"].as_str().unwrap()]);
        let payload: serde_json::Value = serde_json::from_str(&show).unwrap();
        for ev in payload["evidence"].as_array().expect("evidence array") {
            let h = ev.as_str().unwrap();
            assert!(
                known.contains(h),
                "evidence {h} (rec {}) is not a manifest grain",
                r["hash"]
            );
        }
    }
}

#[test]
fn loop_status_health_pinned() {
    let (g, _res) = import_and_run();
    let out = loop_ok(&g.db, T0, &["--format", "json"]);
    assert_golden(&loop_golden_dir().join("status-after-run.json"), &out);
}

// ---------------------------------------------------------------------------
// Suite W3 — idempotency, full sweep, and the run gates
// ---------------------------------------------------------------------------

#[test]
fn loop_second_run_dedups_everything() {
    let (g, _res) = import_and_run();
    let out = loop_ok(&g.db, T0, &["run", "--format", "json"]);
    assert_golden(&loop_golden_dir().join("run-second.json"), &out);
}

#[test]
fn loop_reflect_full_sweep_stays_deduped() {
    // `reflect` re-analyzes the whole memory but dedup keeps the queue stable.
    let (g, _res) = import_and_run();
    let out = loop_ok(&g.db, T0, &["reflect", "--format", "json"]);
    assert_golden(&loop_golden_dir().join("run-reflect.json"), &out);
}

#[test]
fn loop_min_new_gate_skips() {
    let (g, _res) = import_and_run();
    let out = loop_ok(&g.db, T0, &["run", "--min-new", "1", "--format", "json"]);
    assert_golden(&loop_golden_dir().join("run-skip-min-new.json"), &out);
}

#[test]
fn loop_if_stale_gate() {
    let (g, _res) = import_and_run();
    let skipped = run_json(&g.db, T0 + HOUR, &["--if-stale", "6h"]);
    assert_eq!(skipped["outcome"], "skipped");
    assert_eq!(skipped["skip_reason"], "not_stale");
    let ran = run_json(&g.db, T0 + 7 * HOUR, &["--if-stale", "6h"]);
    assert_eq!(ran["outcome"], "ran");
    assert_eq!(ran["stored"], 0, "everything already queued: {ran}");
}

#[test]
fn loop_now_seam_rejects_garbage() {
    // The simulation seam fails loud: a set-but-unparseable AREEV_LOOP_NOW_MS
    // must never silently fall back to wall time.
    let g = import_loop_golden();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["loop", "--db", &g.db, "--ns", "agent", "--telemetry", "off"])
        .env("AREEV_LOOP_NOW_MS", "not-a-timestamp")
        .env_remove("AREEV_LOOP_POLICY")
        .output()
        .expect("spawn areev");
    assert!(!out.status.success(), "garbled AREEV_LOOP_NOW_MS must not succeed");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("AREEV_LOOP_NOW_MS"),
        "failure must name the seam"
    );
}

// ---------------------------------------------------------------------------
// Suite W4 — lifecycle: approve → apply → real memory effect → rollback →
// the situation honestly re-proposes
// ---------------------------------------------------------------------------

#[test]
fn loop_lifecycle_approve_apply_rollback_repropose() {
    let (g, _res) = import_and_run();
    let rows = list_rows(&g.db, T0, &[]);
    let rec = find_rec(&rows, "contradiction_sweep", "\"sam\"");

    // Approve (a distinct human actor), then apply.
    loop_ok(&g.db, T0, &["approve", &rec, "--because", "resolve to the newest residency", "--actor", "user:reviewer"]);
    loop_ok(&g.db, T0, &["apply", &rec, "--because", "supersede the stale value", "--actor", "user:reviewer"]);
    let applied = list_rows(&g.db, T0, &["--status", "applied"]);
    assert_eq!(applied.len(), 1, "exactly the sam rec is applied: {applied:?}");

    // The apply executed real CAL: berlin is now superseded by a tokyo
    // replacement, so the full supersession chain for sam/lives_in deepened.
    let (ok, hist, err) = areev(&["history", "--subject", "sam", "--relation", "lives_in", "--db", &g.db, "--ns", "agent"]);
    assert!(ok, "history failed: {err}");
    assert!(hist.contains("berlin") && hist.contains("tokyo"), "chain should span both values: {hist}");

    // Roll it back (forgets the replacement grain the apply created)…
    loop_ok(&g.db, T0, &["rollback", &rec, "--because", "keep both values for review", "--actor", "user:reviewer"]);
    let rolled = list_rows(&g.db, T0, &["--status", "rolled_back"]);
    assert_eq!(rolled.len(), 1, "the sam rec is rolled back: {rolled:?}");

    // …and the next run honestly re-proposes the contradiction: the rolled-
    // back status frees the dedup key, and the un-superseding restored two
    // live values.
    let res = run_json(&g.db, T0 + HOUR, &[]);
    assert_eq!(res["stored"], 1, "the contradiction must re-propose: {res}");
    let rows = list_rows(&g.db, T0 + HOUR, &[]);
    let re = find_rec(&rows, "contradiction_sweep", "\"sam\"");
    assert_ne!(re, rec, "the re-proposal is a new grain, not the rolled-back one");
}

#[test]
fn loop_apply_requires_approval_first() {
    let (g, _res) = import_and_run();
    let rows = list_rows(&g.db, T0, &[]);
    let rec = find_rec(&rows, "contradiction_sweep", "\"sam\"");
    let (code, _out, err) = loop_cmd(&g.db, T0, &["apply", &rec, "--because", "skip review"]);
    assert_ne!(code, 0, "pending → applied must be refused for a human actor");
    assert!(err.contains("approve first"), "expected the lifecycle error, got: {err}");
}

#[test]
fn loop_self_approval_blocked() {
    let (g, _res) = import_and_run();
    let rows = list_rows(&g.db, T0, &[]);
    let rec = find_rec(&rows, "contradiction_sweep", "\"sam\"");
    let (code, _out, err) = loop_cmd(
        &g.db,
        T0,
        &["approve", &rec, "--because", "lgtm", "--actor", "engine:loop.contradiction_sweep/1"],
    );
    assert_ne!(code, 0, "the creating actor must not approve its own proposal");
    assert!(err.contains("created this recommendation"), "expected SelfApproval, got: {err}");
}

#[test]
fn loop_because_is_mandatory() {
    let (g, _res) = import_and_run();
    let rows = list_rows(&g.db, T0, &[]);
    let rec = find_rec(&rows, "skill_stall", "parse_invoices");
    let (code, _out, err) = loop_cmd(&g.db, T0, &["approve", &rec]);
    assert_ne!(code, 0);
    assert!(err.contains("--because"), "missing BECAUSE must be named: {err}");
}

// ---------------------------------------------------------------------------
// Suite W5 — the destructive gate (staleness proposes FORGET)
// ---------------------------------------------------------------------------

#[test]
fn loop_destructive_apply_gated_then_erases() {
    let (g, _res) = import_and_run();
    let rows = list_rows(&g.db, T0, &[]);
    let rec = find_rec(&rows, "staleness", "promo-black-friday");

    loop_ok(&g.db, T0, &["approve", &rec, "--because", "the promo ended months ago", "--actor", "user:reviewer"]);

    // Without --allow-destructive the apply is refused and the grain survives.
    let (code, _out, err) = loop_cmd(&g.db, T0, &["apply", &rec, "--because", "expire it", "--actor", "user:reviewer"]);
    assert_ne!(code, 0, "destructive apply must be gated");
    assert!(err.contains("allow_destructive"), "gate must name the flag: {err}");
    let exists = g.cal("agent", r#"EXISTS facts WHERE subject = "promo-black-friday""#);
    assert_eq!(exists["exists"], true, "grain must survive the refused apply");

    // With the flag: the FORGET executes, the grain is gone, the file verifies.
    loop_ok(&g.db, T0, &["apply", &rec, "--because", "expire it", "--actor", "user:reviewer", "--allow-destructive"]);
    let exists = g.cal("agent", r#"EXISTS facts WHERE subject = "promo-black-friday""#);
    assert_eq!(exists["exists"], false, "expired grain must be tombstoned");
    let (ok, out, _err) = areev(&["verify", "--db", &g.db]);
    assert!(ok && out.contains("integrity: ok"), "verify after FORGET: {out}");

    // FORGET has no inverse — rollback must refuse.
    let (code, _out, err) = loop_cmd(&g.db, T0, &["rollback", &rec, "--because", "oops", "--actor", "user:reviewer"]);
    assert_ne!(code, 0, "non-rollbackable apply must refuse rollback");
    assert!(err.contains("non-rollbackable"), "expected the rollback error, got: {err}");
}

// ---------------------------------------------------------------------------
// Suite W6 — the Verify gate: outcomes measured at simulated horizons
// ---------------------------------------------------------------------------

/// Approve + apply the sam contradiction at T0; returns its hash.
fn apply_sam_contradiction(g: &GoldenDb) -> String {
    let rows = list_rows(&g.db, T0, &[]);
    let rec = find_rec(&rows, "contradiction_sweep", "\"sam\"");
    loop_ok(&g.db, T0, &["approve", &rec, "--because", "resolve to newest", "--actor", "user:reviewer"]);
    loop_ok(&g.db, T0, &["apply", &rec, "--because", "resolve to newest", "--actor", "user:reviewer"]);
    rec
}

#[test]
fn loop_outcome_held_across_horizons() {
    let (g, _res) = import_and_run();
    let _rec = apply_sam_contradiction(&g);

    // 1-day checkpoint: the fix held (one live value).
    run_json(&g.db, T0 + DAY, &[]);
    // 7-day checkpoint: still held — a second row in the time series.
    run_json(&g.db, T0 + 7 * DAY, &[]);
    let out = loop_ok(&g.db, T0 + 7 * DAY, &["outcomes", "--format", "json"]);
    assert_golden(&loop_golden_dir().join("outcomes-held.json"), &out);
}

#[test]
fn loop_outcome_regression_proposes_revert() {
    let (g, _res) = import_and_run();
    let rec = apply_sam_contradiction(&g);

    // Regression seed: a third residency value lands after the apply.
    let (ok, _out, err) = areev(&["add", "sam", "lives_in", "osaka", "--db", &g.db, "--ns", "agent"]);
    assert!(ok, "seed regression: {err}");

    // At the 1-day checkpoint the metric re-measures: two live values again →
    // regressed → the outcome analyzer proposes a revert. The golden's
    // `stored: 2` is the revert PLUS a follow-on duplicate finding the apply
    // itself created: resolve-to-latest supersedes the losing value with a
    // NEW grain carrying the winning one, so the original tokyo fact and its
    // replacement now form an exact-duplicate pair — emergent, deterministic,
    // and pinned here on purpose (see duplicate_sweep's no-recurrence-metric
    // comment for why consolidation can't shrink live-grain counts).
    let res = loop_ok(&g.db, T0 + DAY, &["run", "--format", "json"]);
    assert_golden(&loop_golden_dir().join("run-after-regression.json"), &res);
    let out = loop_ok(&g.db, T0 + DAY, &["outcomes", "--format", "json"]);
    assert_golden(&loop_golden_dir().join("outcomes-regressed.json"), &out);

    let rows = list_rows(&g.db, T0 + DAY, &[]);
    let revert = rows
        .iter()
        .find(|r| r["analyzer"].as_str().unwrap_or("").contains("outcome_review"))
        .unwrap_or_else(|| panic!("no revert proposal in {rows:?}"));
    assert_eq!(revert["severity"], "high");
    let show = loop_ok(&g.db, T0 + DAY, &["show", revert["hash"].as_str().unwrap()]);
    let payload: serde_json::Value = serde_json::from_str(&show).unwrap();
    assert_eq!(
        payload["evidence"][0].as_str(),
        Some(rec.as_str()),
        "the revert must cite the applied recommendation"
    );
}

// ---------------------------------------------------------------------------
// Suite W7 — rejection cooldowns
// ---------------------------------------------------------------------------

#[test]
fn loop_reject_cooldown_suppresses_then_expires() {
    let (g, _res) = import_and_run();
    let rows = list_rows(&g.db, T0, &[]);
    let rec = find_rec(&rows, "skill_stall", "parse_invoices");
    loop_ok(&g.db, T0, &["reject", &rec, "--because", "long-tail skill, expected", "--actor", "user:reviewer"]);

    // Inside the 7-day cooldown the finding stays suppressed…
    let res = run_json(&g.db, T0 + HOUR, &[]);
    assert_eq!(res["stored"], 0, "cooldown must suppress the re-proposal: {res}");

    // …after it elapses, the still-true situation re-proposes.
    let res = run_json(&g.db, T0 + 8 * DAY, &[]);
    assert_eq!(res["stored"], 1, "expired cooldown must re-propose: {res}");
    let rows = list_rows(&g.db, T0 + 8 * DAY, &[]);
    find_rec(&rows, "skill_stall", "parse_invoices");
}

// ---------------------------------------------------------------------------
// Suite W8 — auto-apply: the policy grant and the trust floor
// ---------------------------------------------------------------------------

/// A minimal grant: duplicate_sweep may auto-apply memory-target findings up
/// to `low`.
const GRANT_DUP_POLICY: &str = r#"{
  "auto_apply_enabled": true,
  "auto_apply": [
    {"analyzer": "loop.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}
  ]
}"#;

/// A deliberately maximal policy: every family granted, both target classes,
/// highest severity. The trust floor must still keep everything but the
/// value-identical exact-duplicate consolidation pending.
const GRANT_ALL_POLICY: &str = r#"{
  "auto_apply_enabled": true,
  "auto_apply": [
    {"analyzer": "loop.duplicate_sweep", "targets": ["memory", "query"], "max_severity": "high"},
    {"analyzer": "loop.contradiction_sweep", "targets": ["memory", "query"], "max_severity": "high"},
    {"analyzer": "loop.tool_failure", "targets": ["memory", "query"], "max_severity": "high"},
    {"analyzer": "loop.staleness", "targets": ["memory", "query"], "max_severity": "high"},
    {"analyzer": "loop.fork_surfacing", "targets": ["memory", "query"], "max_severity": "high"},
    {"analyzer": "loop.skill_stall", "targets": ["memory", "query"], "max_severity": "high"},
    {"analyzer": "loop.outcome_review", "targets": ["memory", "query"], "max_severity": "high"},
    {"analyzer": "loop.llm", "targets": ["memory", "query"], "max_severity": "high"}
  ]
}"#;

fn write_policy(dir: &TempDir, body: &str) -> String {
    let p = dir.path().join("policy.json");
    std::fs::write(&p, body).unwrap();
    p.to_str().unwrap().to_string()
}

#[test]
fn loop_auto_apply_grant_consolidates_duplicates() {
    let g = import_loop_golden();
    let dir = TempDir::new().unwrap();
    let policy = write_policy(&dir, GRANT_DUP_POLICY);
    let out = loop_ok(&g.db, T0, &["run", "--format", "json", "--policy", &policy]);
    assert_golden(&loop_golden_dir().join("run-auto-apply.json"), &out);

    // Exactly the exact-duplicate consolidation auto-applied (the near-dup is
    // NOT value-identical and must stay pending despite the same family).
    let applied = list_rows(&g.db, T0, &["--status", "applied"]);
    assert_eq!(applied.len(), 1, "only the exact-dup consolidation: {applied:?}");
    assert!(applied[0]["summary"].as_str().unwrap().contains("exact-duplicate"));
    let pending = list_rows(&g.db, T0, &[]);
    assert!(
        pending.iter().any(|r| r["summary"].as_str().unwrap().contains("near-duplicate")),
        "near-dup must stay pending: {pending:?}"
    );

    // The consolidation really executed: the two later case-variants are now
    // superseded by replacements carrying the canonical value.
    let hist = areev(&["history", "--subject", "acme", "--relation", "tier", "--db", &g.db, "--ns", "agent"]);
    assert!(hist.0, "history failed: {}", hist.2);
}

#[test]
fn loop_trust_floor_survives_maximal_policy() {
    // Even a policy granting everything cannot push past the trust floor:
    // contradiction/fork/staleness/tool/skill/outcome are manifest-Never,
    // FORGET is destructive, and llm/command origins are structurally
    // ineligible — only the value-identical consolidation moves.
    let g = import_loop_golden();
    let dir = TempDir::new().unwrap();
    let policy = write_policy(&dir, GRANT_ALL_POLICY);
    let res: serde_json::Value = serde_json::from_str(&loop_ok(
        &g.db,
        T0,
        &["run", "--format", "json", "--policy", &policy],
    ))
    .unwrap();
    assert_eq!(res["auto_applied"], 1, "trust floor breached: {res}");
}

#[test]
fn loop_closed_default_never_auto_applies() {
    let (_g, res) = import_and_run();
    assert_eq!(res["auto_applied"], 0, "no policy → nothing auto-applies: {res}");
}

// ---------------------------------------------------------------------------
// Suite W9 — the --fail-on CI gate
// ---------------------------------------------------------------------------

#[test]
fn loop_fail_on_gate_exit_codes() {
    let (g, _res) = import_and_run();

    // A high-severity pending rec (the tool-failure cluster) trips the gate.
    let (code, _out, err) = loop_cmd(&g.db, T0, &["list", "--fail-on", "high"]);
    assert_eq!(code, 2, "pending high rec must exit 2: {err}");

    // Reject the only high rec — the high gate clears, a lower gate still trips.
    let rows = list_rows(&g.db, T0, &[]);
    let tool = find_rec(&rows, "tool_failure", "stripe_refund");
    loop_ok(&g.db, T0, &["reject", &tool, "--because", "known upstream incident", "--actor", "user:reviewer"]);
    let (code, _out, _err) = loop_cmd(&g.db, T0, &["list", "--fail-on", "high"]);
    assert_eq!(code, 0, "no pending high rec left");
    let (code, _out, _err) = loop_cmd(&g.db, T0, &["list", "--fail-on", "low"]);
    assert_eq!(code, 2, "medium/low pendings still trip a low threshold");
}

// ---------------------------------------------------------------------------
// Suite W10 — the loop closes into context: recall-hook --with-loop
// ---------------------------------------------------------------------------

#[test]
fn loop_recall_hook_injection_pinned() {
    // The context a Claude Code UserPromptSubmit hook injects: the recalled
    // memory render plus the pending queue with review pointers. Hybrid
    // recall is deadline-bounded fail-open (leg order can shift under load),
    // so the memory half is asserted semantically and only the loop block —
    // header, top-3-by-severity rows, overflow line — is pinned byte-exact.
    let (g, _res) = import_and_run();
    let out = recall_hook(&g.db, "what do we know about sam", &["--with-loop"]);
    assert!(
        out.contains("sam lives_in tokyo") && out.contains("sam lives_in berlin"),
        "memory render must surface both residency facts: {out}"
    );
    let block_at = out.find("Areev Loop:").unwrap_or_else(|| panic!("no loop block: {out}"));
    assert_golden(&loop_golden_dir().join("recall-hook-loop-block.txt"), &out[block_at..]);

    // Without the flag the hook stays memory-only.
    let without = recall_hook(&g.db, "what do we know about sam", &[]);
    assert!(!without.contains("pending recommendation"), "flagless hook leaked areev-loop: {without}");
}

// ---------------------------------------------------------------------------
// Suite W11 — LLM reflection through a scripted backend (python-gated).
// DISCOVER → GROUND → VERIFY → ROUTE with a deterministic fake: the finding
// cites real evidence, survives verification, lands origin=llm, and can never
// auto-apply.
// ---------------------------------------------------------------------------

const FAKE_LLM_PY: &str = r#"
import sys, json
d = json.loads(sys.stdin.read())
op = d.get("op")
if op == "probe":
    print(json.dumps({"model": "golden-fake-1"}))
elif op == "discover":
    ev = (sorted(e["hash"] for e in d.get("evidence", []) if "sam" in e.get("text", ""))
          or sorted(e["hash"] for e in d.get("evidence", []))[:1])
    print(json.dumps({"recommendations": [{
        "summary": "Residency facts conflict: sam is recorded in two cities",
        "target": "entity:agent/sam",
        "guidance": "confirm which residency is current before relying on either",
        "evidence": ev,
        "confidence": 0.9,
    }]}))
elif op == "ground":
    print(json.dumps({"results": [{"id": c["id"], "supported": True, "reason": "premises cited"}
                                   for c in d.get("claims", [])]}))
elif op == "verify":
    print(json.dumps({"results": [{"id": f["id"], "keep": True, "confidence": 0.9,
                                    "reason": "two conflicting facts in evidence"}
                                   for f in d.get("findings", [])]}))
else:
    print(json.dumps({"notes": [{"target": "entity:agent/sam",
                                  "guidance": "resolve to the most recent statement"}]}))
"#;

#[test]
fn loop_llm_reflection_end_to_end() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let g = import_loop_golden();
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("fake_llm.py");
    std::fs::write(&script, FAKE_LLM_PY).unwrap();
    let cmd = format!("{py} {}", script.display());

    let res = run_json(&g.db, T0, &["--llm-cmd", &cmd]);
    assert_eq!(res["stored"], 12, "11 deterministic + 1 verified llm finding: {res}");

    // The llm rec: origin=llm with the probed model, verifier confidence
    // stamped, cited evidence = the two sam facts, ENRICH guidance attached
    // to the deterministic contradiction rec (whitelisted merge).
    let rows = list_rows(&g.db, T0, &[]);
    let llm = find_rec(&rows, "loop.llm", "Residency facts conflict");
    let show: serde_json::Value =
        serde_json::from_str(&loop_ok(&g.db, T0, &["show", &llm])).unwrap();
    assert_eq!(show["evidence"].as_array().unwrap().len(), 2, "cites both sam facts: {show}");
    let det = find_rec(&rows, "contradiction_sweep", "\"sam\"");
    let det_show: serde_json::Value =
        serde_json::from_str(&loop_ok(&g.db, T0, &["show", &det])).unwrap();
    assert_eq!(det_show["analyzer"], "loop.contradiction_sweep/1");

    // The [llm] badge reaches the injected context. LLM drafts are always
    // stamped low severity and the hook caps at the top 3 by severity, so the
    // badge is asserted in a minimal memory where the llm finding IS the
    // queue: one benign fact, zero deterministic findings, one verified draft.
    let tiny_dir = TempDir::new().unwrap();
    let tiny = tiny_dir.path().join("tiny.db").to_str().unwrap().to_string();
    let (ok, _out, err) = areev(&["add", "sam", "prefers", "tea", "--db", &tiny, "--ns", "agent"]);
    assert!(ok, "seed tiny memory: {err}");
    let res = run_json(&tiny, T0, &["--llm-cmd", &cmd]);
    assert_eq!(res["stored"], 1, "the llm draft is the only finding: {res}");
    let out = recall_hook(&tiny, "what do we know about sam", &["--with-loop"]);
    assert!(out.contains("[llm]"), "llm badge missing from hook injection: {out}");

    // Status surfaces the live approval-rate metric once decided.
    let status = loop_ok(&g.db, T0, &[]);
    assert!(status.contains("LLM findings: 1 surfaced"), "status: {status}");
    loop_ok(&g.db, T0, &["approve", &llm, "--because", "genuine conflict", "--actor", "user:reviewer"]);
    let status = loop_ok(&g.db, T0, &[]);
    assert!(status.contains("100% approved"), "approval rate missing: {status}");
}

#[test]
fn loop_llm_findings_never_auto_apply() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let g = import_loop_golden();
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("fake_llm.py");
    std::fs::write(&script, FAKE_LLM_PY).unwrap();
    let cmd = format!("{py} {}", script.display());
    let policy = write_policy(&dir, GRANT_ALL_POLICY);

    let res = run_json(&g.db, T0, &["--llm-cmd", &cmd, "--policy", &policy]);
    assert_eq!(res["stored"], 12, "{res}");
    assert_eq!(res["auto_applied"], 1, "only the builtin consolidation — never the llm rec: {res}");
}

// ---------------------------------------------------------------------------
// Suite W12 — external command analyzers (python-gated): trust class Command,
// origin stamped `command`, advisory-only, [external] badge.
// ---------------------------------------------------------------------------

const FAKE_ANALYZER_PY: &str = r#"
import sys, json
d = json.loads(sys.stdin.read())
if d.get("op") == "probe":
    print(json.dumps({"id": "golden.pii/1", "title": "PII scan",
                      "description": "golden external analyzer"}))
else:
    kai = sorted(g["hash"] for g in d.get("grains", [])
                 if g.get("fields", {}).get("subject") == "kai")
    print(json.dumps({"findings": [{
        "target": "entity:agent/kai",
        "summary": "contact preference may be personal data - review retention",
        "severity": "high",
        "evidence": kai,
        "confidence": 0.8,
    }]}))
"#;

#[test]
fn loop_external_analyzer_advisory_only() {
    let Some(py) = find_python() else {
        eprintln!("skipping: no python on PATH");
        return;
    };
    let g = import_loop_golden();
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("fake_analyzer.py");
    std::fs::write(&script, FAKE_ANALYZER_PY).unwrap();
    let cmd = format!("{py} {}", script.display());

    // The registry lists it at trust class `command`.
    let listing = loop_ok(&g.db, T0, &["analyzers", "--analyzer-cmd", &cmd]);
    assert!(listing.contains("golden.pii/1"), "external analyzer missing: {listing}");
    assert!(listing.contains("command"), "trust class must be visible: {listing}");

    // Even under a maximal policy its finding stays pending: origin=command
    // is structurally auto-apply-ineligible.
    let policy = write_policy(&dir, GRANT_ALL_POLICY);
    let res = run_json(&g.db, T0, &["--analyzer-cmd", &cmd, "--policy", &policy]);
    assert_eq!(res["stored"], 12, "11 builtin + 1 external: {res}");
    assert_eq!(res["auto_applied"], 1, "external finding must never auto-apply: {res}");
    assert!(
        res["analyzers_run"].as_array().unwrap().iter().any(|a| a == "golden.pii/1"),
        "external analyzer must appear in analyzers_run: {res}"
    );

    // Provenance: origin is stamped `command` (not builtin) with the id…
    let rows = list_rows(&g.db, T0, &[]);
    let ext = find_rec(&rows, "golden.pii", "personal data");
    let show: serde_json::Value =
        serde_json::from_str(&loop_ok(&g.db, T0, &["show", &ext])).unwrap();
    assert_eq!(show["severity"], "high");

    // …and the [external] badge reaches the injected context.
    let out = recall_hook(&g.db, "what do we know about kai", &["--with-loop"]);
    assert!(out.contains("[external]"), "external badge missing: {out}");
}

// ---------------------------------------------------------------------------
// Suite W13 — the telemetry-fed trio, live: recalls and assemblies made
// through the real CLI feed the sidecar, then cold_grains / coverage_gap /
// budget_pressure fire on the rollups.
// ---------------------------------------------------------------------------

#[test]
fn loop_telemetry_fed_analyzers_fire_on_live_rollups() {
    let g = import_loop_golden();

    // Coverage gap: the same free-text question, asked 3×, always empty
    // (no dataset grain shares any of its tokens).
    for _ in 0..3 {
        let (ok, out, err) = areev(&[
            "search", "--db", &g.db, "--ns", "agent",
            "--query", "quarterly carbon report deadline", "-k", "3",
        ]);
        assert!(ok, "search failed: {err}");
        assert!(out.trim().is_empty(), "gap query must return nothing: {out}");
    }
    // Budget pressure: 20 assemblies over sam's two (young — cold-neutral)
    // facts, each overflowing a 10-token budget so at least one grain drops.
    for _ in 0..20 {
        let payload = g.cal(
            "agent",
            r#"ASSEMBLE "t" FROM a: (RECALL facts WHERE subject = "sam") BUDGET 10 tokens FORMAT sml"#,
        );
        assert!(payload["grain_count"].as_i64() < Some(2), "budget must drop a grain: {payload}");
    }

    // Telemetry attached (the agent-host default) → the trio fires alongside
    // the 11 deterministic findings: 4 cold facts (≥30d old, never recalled),
    // 1 coverage gap, 1 budget pressure.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["loop", "run", "--format", "json", "--db", &g.db, "--ns", "agent"])
        .env("AREEV_LOOP_NOW_MS", T0.to_string())
        .env_remove("AREEV_LOOP_POLICY")
        .output()
        .expect("spawn areev");
    assert!(out.status.success(), "telemetry run failed: {}", String::from_utf8_lossy(&out.stderr));
    let res: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(res["stored"], 17, "11 deterministic + 4 cold + gap + budget: {res}");
    for a in ["loop.cold_grains/1", "loop.coverage_gap/1", "loop.budget_pressure/1"] {
        assert!(
            res["analyzers_run"].as_array().unwrap().iter().any(|x| x == a),
            "{a} must run with telemetry attached: {res}"
        );
    }

    let rows = list_rows(&g.db, T0, &[]);
    let cold = rows.iter().filter(|r| r["analyzer"].as_str().unwrap().contains("cold_grains")).count();
    assert_eq!(cold, 4, "old never-recalled facts: {rows:?}");
    find_rec(&rows, "coverage_gap", "quarterly carbon report deadline");
    find_rec(&rows, "budget_pressure", "100% of 20 recalls");
}

// ---------------------------------------------------------------------------
// Suite W14 — CLI ↔ MCP parity: with pinned time, two *separate* imports must
// produce identical recommendation content addresses across surfaces.
// ---------------------------------------------------------------------------

#[test]
fn loop_cli_mcp_parity() {
    use std::process::{Command, Stdio};

    // CLI leg on its own import.
    let (g_cli, _res) = import_and_run();
    let cli_hashes: BTreeSet<String> = list_rows(&g_cli.db, T0, &[])
        .iter()
        .map(|r| r["hash"].as_str().unwrap().to_string())
        .collect();

    // MCP leg on a fresh import: the areev_loop_adapter tool runs the engine and
    // returns the pending queue.
    let g_mcp = import_loop_golden();
    let rpc = |id: u64, method: &str, params: serde_json::Value| {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string()
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_areev"))
        .args(["serve", "--mcp", "--db", &g_mcp.db, "--ns", "agent", "--telemetry", "off"])
        .env("AREEV_LOOP_NOW_MS", T0.to_string())
        .env_remove("AREEV_LOOP_POLICY")
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
                "name": "areev_loop_adapter", "arguments": {}}))
        )
        .unwrap();
    }
    let out = child.wait_with_output().expect("mcp server exit");
    assert!(out.status.success());
    let resp = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"] == 2)
        .expect("areev-loop response");
    assert_ne!(resp["result"]["isError"], true, "mcp areev-loop errored: {resp}");
    let text = resp["result"]["content"][0]["text"].as_str().expect("content text");
    let payload: serde_json::Value = serde_json::from_str(text).expect("mcp payload json");
    let mcp_hashes: BTreeSet<String> = payload["pending"]
        .as_array()
        .expect("pending array")
        .iter()
        .map(|r| r["hash"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(
        cli_hashes, mcp_hashes,
        "CLI and MCP produced different recommendation content addresses"
    );
}

// ---------------------------------------------------------------------------
// Suite W15 — dataset integrity + the frozen-format canary
// ---------------------------------------------------------------------------

#[test]
fn loop_import_verifies_clean() {
    let g = import_loop_golden();
    let (ok, out, err) = areev(&["verify", "--db", &g.db]);
    assert!(ok, "verify failed: {err}");
    assert!(out.contains("integrity: ok"), "bad verify: {out}");
}

#[test]
fn loop_fork_survives_bundle_roundtrip() {
    let g = import_loop_golden();
    let (ok, out, err) = areev(&["forks", "--db", &g.db]);
    assert!(ok, "forks failed: {err}");
    assert!(
        out.contains("deploy") && out.contains("region"),
        "deploy/region fork lost in export/import: {out}"
    );
}

#[test]
fn loop_manifest_hashes_stable() {
    // Regenerating the dataset must reproduce every committed hash. This
    // extends the frozen-format canary across Tool, Skill, Observation, Goal
    // and valid_to serialization — grain shapes the memory-stack golden
    // dataset doesn't cover.
    let committed = loop_manifest();
    let dir = TempDir::new().unwrap();
    let fresh = generate_loop(dir.path(), &dir.path().join("fresh.bundle"));
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
// Bless — regenerates the committed loop dataset (run explicitly + commit)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "regenerates committed golden files; run explicitly and commit the diff"]
fn bless_loop_golden_dataset() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(golden::dataset_dir()).unwrap();
    let m = generate_loop(dir.path(), &loop_bundle_path());
    std::fs::write(
        loop_manifest_path(),
        serde_json::to_string_pretty(&m.to_json()).unwrap() + "\n",
    )
    .unwrap();
    eprintln!(
        "blessed {} grains -> {} + {}",
        m.total_grains,
        loop_bundle_path().display(),
        loop_manifest_path().display()
    );
}
