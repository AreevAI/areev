//! `record_tool_call` records occurrences, not values (#66).
//!
//! Grains are content-addressed and `created_at` has millisecond resolution, so
//! byte-identical tool calls recorded inside one millisecond hash to a single
//! address — and since 1.1.1 a duplicate add is a silent no-op rather than an
//! error. That combination quietly deleted the flagship analyzer's input: an
//! agent retrying a failing call with identical arguments is exactly the
//! workload `loop.tool_failure` exists to catch, and five identical failures
//! collapsed to one grain with nothing left to cluster.
//!
//! These tests run the documented five-failure example through the real
//! binding path and assert the recommendation it is documented to produce.
//! They are deliberately timing-sensitive in the direction that matters: they
//! record with no sleep, which is what made the bug reproduce in release builds
//! (and hide in debug ones).

use areev_loop::{Engine, RecStatus, RunOptions};
use areev_cal::AreevFacade;
use areev_loop_adapter::BorrowedSubstrate;
use areev_store::Areev;

const NOW: i64 = 1_700_000_000_000;

fn open_temp() -> (tempfile::TempDir, AreevFacade) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proof.db");
    let store = Areev::open(path.to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(store, Some("caller".into()), None);
    (dir, facade)
}

/// The README / `docs/loop.md` / `docs/cookbook.md` "proof" block, verbatim in
/// shape: five identical failures and two identical successes, no jitter, no
/// `thread=`. Every call must survive as its own occurrence.
#[test]
fn identical_tool_calls_are_distinct_occurrences() {
    let (_d, facade) = open_temp();

    let mut hashes = Vec::new();
    for _ in 0..5 {
        hashes.push(
            facade
                .record_tool_call(
                    "caller",
                    "stripe_refund",
                    None,
                    r#"{"error":"rate_limited"}"#,
                    true,
                    None,
                    None, None, None, None, None, None, None, None
                )
                .unwrap()
                .to_hex(),
        );
    }
    for _ in 0..2 {
        facade
            .record_tool_call("caller", "stripe_refund", None, r#"{"ok":true}"#, false, None, None, None, None, None, None, None, None, None)
            .unwrap();
    }

    let mut uniq = hashes.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(
        uniq.len(),
        5,
        "five retries of one failing call are five occurrences, not one"
    );

    // The count alone is a weak guard: in a debug build the writes are slow
    // enough to straddle milliseconds, so five grains appear even with the
    // identity stamping removed — which is exactly how #66 shipped past a
    // green suite. Assert the mechanism instead. Distinct `tool_call_id`s are
    // true regardless of build profile, machine speed or clock resolution.
    let ex = areev_cal::CalExecutor::new(areev_cal::CalExecutorConfig::default());
    let res = ex
        .execute(r#"RECALL tools WHERE tool_name = "stripe_refund""#, &facade)
        .unwrap();
    let payload = serde_json::to_value(&res.result).unwrap();
    let mut ids: Vec<String> = payload["grains"]
        .as_array()
        .expect("grains")
        .iter()
        .filter_map(|g| g["fields"]["tool_call_id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(ids.len(), 7, "every recorded call carries an occurrence id");
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 7, "and no two calls share one: {ids:?}");
    assert!(
        ids.iter().all(|i| i.starts_with("auto:")),
        "a synthesized id is marked as such so it is never mistaken for a \
         provider's own tool_call_id: {ids:?}"
    );
}

/// A supplied `call_id` reaches the grain as its `tool_call_id`, so the record
/// can be correlated back to the provider transcript it came from. That field
/// was serialized and listed as queryable since 1.0 but no builder ever set it,
/// which is why the collapse had no host-supplied identity to fall back on.
#[test]
fn a_supplied_call_id_is_stored_and_queryable() {
    let (_d, facade) = open_temp();

    facade
        .record_tool_call(
            "caller",
            "stripe_refund",
            Some(r#"{"amount":42,"currency":"USD"}"#),
            "boom",
            true,
            None,
            Some("call_a1"), None, None, None, None, None, None, None
        )
        .unwrap();
    facade
        .record_tool_call("caller", "stripe_refund", None, "boom", true, None, Some("call_a2"), None, None, None, None, None, None, None)
        .unwrap();

    let ex = areev_cal::CalExecutor::new(areev_cal::CalExecutorConfig::default());
    let res = ex
        .execute(
            r#"RECALL tools WHERE tool_call_id = "call_a1""#,
            &facade,
        )
        .unwrap();
    let payload = serde_json::to_value(&res.result).unwrap();
    assert_eq!(
        payload["grains"].as_array().map(|g| g.len()),
        Some(1),
        "the supplied id must be stored and filterable, got: {payload}"
    );
    assert_eq!(payload["grains"][0]["fields"]["input"]["amount"], 42);
}

/// The end-to-end claim the docs make: five failures out of seven calls
/// produce a high-severity `loop.tool_failure` recommendation naming the count
/// and the rate. This is the assertion that would have caught #66 — and the
/// `STO-E001` crash (#40) before it — since both left this queue empty.
#[test]
fn the_documented_proof_block_yields_a_recommendation() {
    let (_d, facade) = open_temp();

    for _ in 0..5 {
        facade
            .record_tool_call(
                "caller",
                "stripe_refund",
                None,
                r#"{"error":"rate_limited"}"#,
                true,
                None,
                None, None, None, None, None, None, None, None
            )
            .unwrap();
    }
    for _ in 0..2 {
        facade
            .record_tool_call("caller", "stripe_refund", None, r#"{"ok":true}"#, false, None, None, None, None, None, None, None, None, None)
            .unwrap();
    }

    let engine = Engine::with_builtins();
    let mut sub = BorrowedSubstrate::new(&facade);
    engine.run(&mut sub, &RunOptions::default(), NOW).unwrap();

    let recs = engine
        .recommendations(&sub, Some(RecStatus::Pending))
        .unwrap();
    let tf = recs
        .iter()
        .find(|r| r.analyzer.starts_with("loop.tool_failure"))
        .expect("the documented tool-failure recommendation — an empty queue is #66");

    let summary = tf.summary.render();
    assert!(
        summary.contains('5'),
        "the count the docs print is 5, got: {summary}"
    );
    assert!(
        summary.contains("71"),
        "5 of 7 calls is the documented 71%, got: {summary}"
    );
    assert!(
        summary.contains("rate_limited"),
        "the error signature must survive, got: {summary}"
    );
}
