//! #430: a Loop pass reads what landed since its watermark, not the memory.
//!
//! The gate (`count_new`) and the analysis pass both read "grains of type T
//! since the watermark". Before #430 the adapter answered that by fetching
//! every grain in the memory and filtering in Rust, so a pass with nothing
//! new cost the whole memory — minutes and gigabytes on a large one. These
//! tests measure blobs decoded by the store (`Areev::blobs_decoded`) and pin
//! that the count does not grow with the number of OLD grains.

use areev_core::types::{Grain, Tool, ToolKind};
use areev_loop::analyzers::tool_failure::ToolFailureClustering;
use areev_loop::{AnalyzeCtx, Analyzer, Engine, RunOptions, SubstrateRead};
use areev_loop_adapter::AreevSubstrate;
use areev_store::{Areev, AreevOptions};

const NOW: i64 = 1_700_000_000_000;
const DAY: i64 = 86_400_000;

/// A memory holding `old` Tool grains, all before the first pass, with a
/// payload big enough that reading them is the cost the issue measured.
///
/// They are 60 days old: `tool_failure` reads a rolling `window_days` window
/// (default 30) rather than the watermark — a failure RATE needs the calls
/// that succeeded too — so grains inside that window are the analyzer's
/// declared input, not an unbounded read.
fn memory_with_old_tools(old: usize) -> (tempfile::TempDir, Areev) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let mut store = Areev::open(path.to_str().unwrap()).unwrap();
    let payload = "x".repeat(2_000);
    for i in 0..old {
        store
            .add(
                &Tool::new("crm.lookup")
                    .namespace("desk")
                    .content(&format!("{i} {payload}"))
                    .created_at(NOW - 60 * DAY + i as i64),
            )
            .unwrap();
    }
    (dir, store)
}

fn decoded(sub: &AreevSubstrate) -> u64 {
    sub.facade().with_store(|m| m.blobs_decoded())
}

/// Blobs decoded by (a) a gated pass with nothing new and (b) a pass over
/// `fresh` new grains, on a memory with `old` grains behind the watermark.
fn measure(old: usize, fresh: usize) -> (u64, u64) {
    let (_d, store) = memory_with_old_tools(old);
    let mut sub = AreevSubstrate::new(store, None);
    let engine = Engine::with_builtins();
    // First pass: no watermark yet, so it reads the memory. That is the
    // documented whole-memory read; it sets the watermark to NOW.
    assert!(engine.run(&mut sub, &RunOptions::default(), NOW).unwrap().ran());

    let gated = RunOptions { min_new: Some(1), ..RunOptions::default() };
    let before = decoded(&sub);
    let r = engine.run(&mut sub, &gated, NOW + 1_000).unwrap();
    assert!(!r.ran(), "nothing new: the gate must skip");
    let skipped_cost = decoded(&sub) - before;

    for i in 0..fresh {
        sub.facade().with_store(|m| {
            m.add(
                &Tool::new("crm.lookup")
                    .namespace("desk")
                    .content(&format!("fresh {i}"))
                    .created_at(NOW + 2_000 + i as i64),
            )
            .unwrap()
        });
    }
    let before = decoded(&sub);
    let r = engine.run(&mut sub, &gated, NOW + 10_000).unwrap();
    assert!(r.ran(), "{fresh} new grains must open the gate");
    (skipped_cost, decoded(&sub) - before)
}

#[test]
fn a_gated_pass_reads_nothing_older_than_its_watermark() {
    let (skip_small, run_small) = measure(40, 5);
    let (skip_large, run_large) = measure(400, 5);
    assert_eq!(
        skip_small, skip_large,
        "a skipped pass must cost the same whatever the memory holds \
         ({skip_small} blobs at 40 old grains, {skip_large} at 400)"
    );
    assert!(
        skip_large < 40,
        "a skipped pass decoded {skip_large} blobs over a 400-grain memory"
    );
    assert_eq!(
        run_small, run_large,
        "a pass over 5 new grains must not read the old ones \
         ({run_small} blobs at 40 old grains, {run_large} at 400)"
    );
}

// ---------------------------------------------------------------------------
// #433: the rolling window. `tool_failure` reads `window_days` back from now,
// not from the watermark, so on a memory younger than the window it is the
// whole memory — the case #430's tests (old grains at 60 days) never reached.
// A full pass with nothing new must decode what is FAILING, not the window.

/// `calls` successful calls and three identical failures, all inside the
/// window (5 days old), plus one Tool Definition pinning an evalset.
fn memory_with_recent_tools(calls: usize) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db").to_str().unwrap().to_string();
    let mut store = Areev::open(&path).unwrap();
    let payload = "y".repeat(2_000);
    for i in 0..calls {
        store
            .add(
                &Tool::new("crm.lookup")
                    .namespace("desk")
                    .content(&format!("{i} {payload}"))
                    .created_at(NOW - 5 * DAY + i as i64),
            )
            .unwrap();
    }
    for i in 0..3i64 {
        store
            .add(
                &Tool::new("crm.lookup")
                    .namespace("desk")
                    .content("timeout after 30s")
                    .is_error(true)
                    .created_at(NOW - 4 * DAY + i),
            )
            .unwrap();
    }
    let mut def = Tool::new("crm.lookup")
        .namespace("desk")
        .kind(ToolKind::Definition)
        .tool_description("look up an account")
        .created_at(NOW - 6 * DAY);
    def.common.extra_fields.insert("evalset_hash".into(), serde_json::json!("e".repeat(64)));
    store.add(&def).unwrap();
    (dir, path)
}

/// Blobs decoded by a full (non-skipped) pass with nothing new.
fn full_pass_cost(calls: usize) -> u64 {
    let (_d, path) = memory_with_recent_tools(calls);
    let mut sub = AreevSubstrate::new(Areev::open(&path).unwrap(), None);
    let engine = Engine::with_builtins();
    assert!(engine.run(&mut sub, &RunOptions::default(), NOW).unwrap().ran());
    let always = RunOptions { min_new: Some(0), ..RunOptions::default() };
    let before = decoded(&sub);
    assert!(engine.run(&mut sub, &always, NOW + 1_000).unwrap().ran(), "min_new 0 runs");
    decoded(&sub) - before
}

#[test]
fn a_full_pass_with_nothing_new_decodes_the_failures_not_the_window() {
    let small = full_pass_cost(40);
    let large = full_pass_cost(400);
    assert_eq!(
        small, large,
        "a full pass must cost the same whatever the window holds \
         ({small} blobs at 40 calls, {large} at 400)"
    );
    assert!(large < 40, "a full pass decoded {large} blobs over a 400-call window");
}

/// #432: the evalset pin and `cold_grains`' in-use set read the Definitions
/// alone, never the calls.
#[test]
fn definitions_are_read_without_the_calls() {
    let cost = |calls| {
        let (_d, path) = memory_with_recent_tools(calls);
        let sub = AreevSubstrate::new(Areev::open(&path).unwrap(), None);
        let before = decoded(&sub);
        assert_eq!(sub.tool_evalset("crm.lookup").unwrap(), Some("e".repeat(64)));
        assert_eq!(sub.tool_definitions(None).unwrap().len(), 1);
        decoded(&sub) - before
    };
    assert_eq!(cost(40), 2, "one Definition, read twice");
    assert_eq!(cost(400), 2, "the call grains are never read");
}

/// The index path and the fold (a handle whose Tool index is not built)
/// must propose the same thing — same clusters, rates and evidence — over a
/// window with a supersession in it.
#[test]
fn the_index_and_the_fold_cluster_identically() {
    let (_d, path) = memory_with_recent_tools(30);
    {
        let mut m = Areev::open(&path).unwrap();
        // A failure retried into a success: the supersession retires it.
        let failed = m
            .add(&Tool::new("mail.send").namespace("desk").content("smtp 451").is_error(true).created_at(NOW - 3 * DAY))
            .unwrap();
        m.supersede(
            &failed,
            &mut Tool::new("mail.send").namespace("desk").content("sent").created_at(NOW - 3 * DAY + 5),
        )
        .unwrap();
        for i in 0..4i64 {
            m.add(
                &Tool::new("mail.send")
                    .namespace("desk")
                    .content("smtp 451")
                    .is_error(true)
                    .created_at(NOW - 2 * DAY + i),
            )
            .unwrap();
        }
    }
    let drafts = |sub: &AreevSubstrate| {
        let a = ToolFailureClustering::new();
        let params = a.manifest().resolve_params(&Default::default()).unwrap();
        let verdicts = Default::default();
        let ctx = AnalyzeCtx::new(sub, &params, &[], None, NOW, &[], &verdicts);
        a.analyze(&ctx).unwrap()
    };
    let indexed = drafts(&AreevSubstrate::new(Areev::open(&path).unwrap(), None));
    // crm.lookup's 3 failures in 34 calls stay under `min_rate`; mail.send
    // clusters with 4 failures — the retried one retired by its supersession —
    // over 5 opportunities (the retry's success + the cluster).
    assert_eq!(indexed.len(), 1, "{indexed:?}");
    assert_eq!(indexed[0].summary.args["count"], 4, "the superseded failure is not counted");
    assert_eq!(indexed[0].metric.as_ref().unwrap().n, 5);

    // Unbuild the index, then read through a read-only handle, which never
    // rebuilds it: the analyzer folds.
    Areev::open(&path).unwrap().meta_delete("tool_index").unwrap();
    let ro = Areev::open_with(&path, AreevOptions { read_only: true, ..AreevOptions::default() }).unwrap();
    let folded_sub = AreevSubstrate::new(ro, None);
    assert!(
        folded_sub.tool_tallies(None, Default::default()).unwrap().is_none(),
        "the read-only handle has no index to answer from"
    );
    let folded = drafts(&folded_sub);
    assert_eq!(indexed, folded, "the index path and the fold agree");
}
