//! #430: a Loop pass reads what landed since its watermark, not the memory.
//!
//! The gate (`count_new`) and the analysis pass both read "grains of type T
//! since the watermark". Before #430 the adapter answered that by fetching
//! every grain in the memory and filtering in Rust, so a pass with nothing
//! new cost the whole memory — minutes and gigabytes on a large one. These
//! tests measure blobs decoded by the store (`Areev::blobs_decoded`) and pin
//! that the count does not grow with the number of OLD grains.

use areev_core::types::{Grain, Tool};
use areev_loop::{Engine, RunOptions};
use areev_loop_adapter::AreevSubstrate;
use areev_store::Areev;

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
