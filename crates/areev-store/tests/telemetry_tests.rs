//! Telemetry sidecar (`<file>.telemetry.db`) — capture, rollups, scrub, and
//! the host-only mode gate. The recall-path latency of the ON mode is proven
//! separately by `examples/voice_loop.rs` (run with telemetry on).

use areev_core::types::{Fact, Grain};
use areev_store::{Areev, AreevOptions, TelemetryMode};
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9).source_type("user_explicit");
    f.common.namespace = Some(ns.to_string());
    f
}

fn open(dir: &TempDir, mode: TelemetryMode) -> Areev {
    let path = dir.path().join("agent.db");
    Areev::open_with(
        path.to_str().unwrap(),
        AreevOptions { telemetry: mode, ..Default::default() },
    )
    .unwrap()
}

fn sidecar_path(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("agent.db");
    std::path::PathBuf::from(format!("{}.telemetry.db", path.to_str().unwrap()))
}

#[test]
fn off_mode_writes_no_sidecar() {
    let dir = TempDir::new().unwrap();
    let mut m = open(&dir, TelemetryMode::Off);
    m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    let _ = m.recall_hybrid("caller", Some("alice"), None, None, 16, None).unwrap();
    assert_eq!(m.telemetry_mode(), TelemetryMode::Off);
    drop(m);
    assert!(!sidecar_path(&dir).exists(), "Off must create no telemetry sidecar");
}

#[test]
fn aggregate_captures_grain_access() {
    let dir = TempDir::new().unwrap();
    let mut m = open(&dir, TelemetryMode::Aggregate);
    assert_eq!(m.telemetry_mode(), TelemetryMode::Aggregate);
    let h = m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    m.add(&fact("caller", "alice", "lives_in", "Berlin")).unwrap();

    // Two subject recalls — the instrumented hybrid path.
    let got = m.recall_hybrid("caller", Some("alice"), None, None, 16, None).unwrap();
    assert!(!got.is_empty());
    let _ = m.recall_hybrid("caller", Some("alice"), None, None, 16, None).unwrap();

    // Reader flushes the buffer first, so the rollup is current.
    let stats = m.telemetry_access_stats(None).unwrap();
    let hit = stats
        .iter()
        .find(|s| s.hash == h.to_hex())
        .expect("recalled grain should have an access rollup");
    assert!(hit.recall_count >= 2, "two recalls → count ≥ 2, got {}", hit.recall_count);
    assert!(sidecar_path(&dir).exists(), "aggregate creates the sidecar file");
}

#[test]
fn free_text_query_feeds_query_stats() {
    let dir = TempDir::new().unwrap();
    let mut m = open(&dir, TelemetryMode::Aggregate);
    m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();

    // A query that matches nothing → recorded as an empty question (the
    // coverage-gap signal).
    let _ = m
        .recall_hybrid("caller", None, None, Some("nonexistent zzzz"), 16, None)
        .unwrap();
    let stats = m.telemetry_query_stats(None).unwrap();
    let q = stats.iter().find(|s| s.sample.contains("nonexistent"));
    let q = q.expect("free-text query should be recorded");
    assert_eq!(q.run_count, 1);
    assert_eq!(q.empty_count, 1, "a query returning nothing counts as empty");
}

#[test]
fn forget_scrubs_grain_access() {
    let dir = TempDir::new().unwrap();
    let mut m = open(&dir, TelemetryMode::Aggregate);
    let h = m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    let _ = m.recall_hybrid("caller", Some("alice"), None, None, 16, None).unwrap();

    // Flush into the rollup, confirm it's there.
    assert!(m.telemetry_access_stats(None).unwrap().iter().any(|s| s.hash == h.to_hex()));

    m.forget(&h).unwrap();
    let after = m.telemetry_access_stats(None).unwrap();
    assert!(
        !after.iter().any(|s| s.hash == h.to_hex()),
        "forget must scrub the grain's telemetry access row"
    );
}

#[test]
fn structural_recall_feeds_telemetry() {
    let dir = TempDir::new().unwrap();
    let mut m = open(&dir, TelemetryMode::Aggregate);
    let h = m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    // The plain structural `recall` (not `recall_hybrid`) — the voice/CLI path.
    let got = m.recall("caller", "alice", None, 16).unwrap();
    assert!(!got.is_empty());
    let stats = m.telemetry_access_stats(None).unwrap();
    assert!(
        stats.iter().any(|s| s.hash == h.to_hex() && s.recall_count >= 1),
        "structural recall must feed the grain-access rollup"
    );
}

#[test]
fn note_budget_accumulates() {
    let dir = TempDir::new().unwrap();
    let mut m = open(&dir, TelemetryMode::Aggregate);
    // Feeds the budget_pressure analyzer: overflow=true when ASSEMBLE dropped
    // grains to fit its token budget.
    m.telemetry_note_budget(true).unwrap();
    m.telemetry_note_budget(false).unwrap();
    m.telemetry_note_budget(true).unwrap();
    let b = m.telemetry_budget_stats().unwrap();
    assert_eq!(b.sample_count, 3);
    assert_eq!(b.overflow_count, 2);
}

#[test]
fn full_mode_keeps_a_recall_log() {
    let dir = TempDir::new().unwrap();
    let mut m = open(&dir, TelemetryMode::Full);
    m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    let _ = m.recall_hybrid("caller", Some("alice"), None, None, 16, None).unwrap();
    // The reader path flushes; full mode additionally persists per-recall rows.
    // We assert indirectly via the access rollup (present in both modes) plus
    // the sidecar existing — the ring log itself is exercised by the console.
    assert!(!m.telemetry_access_stats(None).unwrap().is_empty());
    assert!(sidecar_path(&dir).exists());
}

#[test]
fn full_mode_joins_recalls_to_runs_without_splitting_intent_rollups() {
    let dir = TempDir::new().unwrap();
    let mut m = open(&dir, TelemetryMode::Full);
    m.add(&fact("caller", "alice", "prefers", "window seat")).unwrap();
    m.set_run_id(Some("run-a"));
    m.recall_hybrid("caller", None, None, Some("window seat"), 16, None)
        .unwrap();
    m.set_run_id(Some("run-b"));
    m.recall_hybrid("caller", None, None, Some("window seat"), 16, None)
        .unwrap();
    m.set_run_id(None);

    let all = m.telemetry_recall_log(None).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(
        all.iter().filter_map(|row| row.run_id.as_deref()).collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["run-a", "run-b"])
    );
    assert_eq!(m.telemetry_recall_log(Some("run-a")).unwrap().len(), 1);
    let stats = m.telemetry_query_stats(Some("caller")).unwrap();
    assert_eq!(stats.len(), 1, "run_id must not enter the query intent key");
    assert_eq!(stats[0].run_count, 2);
}

#[test]
fn aggregate_hashed_keeps_no_query_text_anywhere() {
    // #306: what a person types is content. Under `aggregate` it was
    // retained memory-wide in `qkey` and `sample`, and no scrub reached a
    // zero-result query.
    const MARKER: &str = "zzmarkerqueryzz";
    let d = TempDir::new().unwrap();
    let mut m = open(&d, TelemetryMode::AggregateHashed);
    m.add(&fact("ns", "alice", "prefers", "tea")).unwrap();
    for _ in 0..3 {
        let _ = m.recall_hybrid("ns", None, None, Some(MARKER), 5, None).unwrap();
    }
    m.telemetry_flush().unwrap();

    let stats = m.telemetry_query_stats(None).unwrap();
    assert!(!stats.is_empty(), "rollups still accumulate");
    for q in &stats {
        assert!(!q.key.contains(MARKER), "qkey leaked the text: {}", q.key);
        assert!(q.sample.is_empty(), "sample must be empty, got {:?}", q.sample);
    }
    // Counters are what the analyzers need, and they are intact.
    let total: i64 = stats.iter().map(|q| q.run_count).sum();
    assert_eq!(total, 3);
    // And the ring log is never written under a hashed mode.
    assert!(m.telemetry_recall_log(None).unwrap().is_empty());
}

#[test]
fn aggregate_still_keeps_the_sample() {
    // Nothing moves for existing deployments.
    const MARKER: &str = "zzkeeptextzz";
    let d = TempDir::new().unwrap();
    let mut m = open(&d, TelemetryMode::Aggregate);
    m.add(&fact("ns", "alice", "prefers", "tea")).unwrap();
    let _ = m.recall_hybrid("ns", None, None, Some(MARKER), 5, None).unwrap();
    m.telemetry_flush().unwrap();
    let stats = m.telemetry_query_stats(None).unwrap();
    assert!(stats.iter().any(|q| q.sample.contains(MARKER)));
}

#[test]
fn telemetry_scrub_namespace_clears_one_namespace_and_leaves_the_others() {
    const MARKER: &str = "zzscrubmezz";
    let d = TempDir::new().unwrap();
    let mut m = open(&d, TelemetryMode::Aggregate);
    m.add(&fact("a", "alice", "prefers", "tea")).unwrap();
    m.add(&fact("b", "bob", "prefers", "chai")).unwrap();
    // A ZERO-RESULT free-text query: names no grain hash, so `scrub(hash)`
    // can never reach it. This is the case the namespace scrub exists for.
    let _ = m.recall_hybrid("a", None, None, Some(MARKER), 5, None).unwrap();
    let _ = m.recall_hybrid("b", None, None, Some("chai"), 5, None).unwrap();
    m.telemetry_flush().unwrap();
    assert!(m
        .telemetry_query_stats(None)
        .unwrap()
        .iter()
        .any(|q| q.sample.contains(MARKER)));

    m.telemetry_scrub_namespace("a").unwrap();
    let after = m.telemetry_query_stats(None).unwrap();
    assert!(
        !after.iter().any(|q| q.sample.contains(MARKER)),
        "a's rows are gone"
    );
    assert!(after.iter().any(|q| q.ns == "b"), "b's rows survive");
    assert!(m.telemetry_access_stats(Some("a")).unwrap().is_empty());
    assert!(!m.telemetry_access_stats(Some("b")).unwrap().is_empty());
}

#[test]
fn telemetry_scrub_namespace_refuses_a_pattern() {
    let d = TempDir::new().unwrap();
    let mut m = open(&d, TelemetryMode::Aggregate);
    assert!(m.telemetry_scrub_namespace("a.*").is_err());
}

#[test]
fn aggregate_hashed_parses_by_every_documented_spelling() {
    for spelling in ["aggregate-hashed", "aggregate_hashed", "hashed"] {
        assert_eq!(
            TelemetryMode::parse(spelling),
            Some(TelemetryMode::AggregateHashed),
            "{spelling}"
        );
    }
    assert_eq!(TelemetryMode::AggregateHashed.as_str(), "aggregate-hashed");
}
