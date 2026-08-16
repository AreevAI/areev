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
