//! End-to-end tests for the Areev substrate adapter against a real temp
//! `.db`: grain round-trip, liveness, state persistence, and the full
//! run → review → apply loop through `areev_loop::Engine`.

use areev_cal::AreevFacade;
use areev_core::authz::{AuthzSet, Grant, Verb};
use areev_core::types::{Fact, Grain, Tool};
use areev_store::Areev;
use areev_loop_adapter::{BorrowedSubstrate, AreevSubstrate};
use areev_loop::{
    Decision, Engine, ObserverType, OmsSubstrate, ReadOpts, RecStatus, RunOptions, ScopeSet,
    SubstrateRead,
};

const NOW: i64 = 1_700_000_000_000;

fn open_temp() -> (tempfile::TempDir, Areev) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let store = Areev::open(path.to_str().unwrap()).unwrap();
    (dir, store)
}

fn seed(store: &mut Areev) {
    // Exact-duplicate facts (also a fork on the same value).
    //
    // The two `created_at` stamps are explicit because they are the only thing
    // that makes these two *grains* rather than one. A grain is addressed by its
    // whole blob, `created_at` included at millisecond resolution, so with the
    // clock left to itself these collapse into a single grain whenever both
    // writes land in one tick — and then there is no duplicate for
    // `loop.duplicate_sweep` to find. That is a genuine content-addressing
    // property, not a bug, but leaving it to timing made this seed produce
    // three recommendations or two depending on how fast the machine was:
    // reliably two grains in a debug build, roughly a coin flip in release.
    // Pinning the stamps makes the fixture say what it means.
    for stamp in [NOW - 2_000, NOW - 1_000] {
        store
            .add(
                &Fact::new("acme", "tier", "Enterprise")
                    .namespace("caller")
                    .created_at(stamp),
            )
            .unwrap();
    }
    // Two live values under a functional relation → contradiction (+ fork).
    for (target, stamp) in [("us-east-1", NOW - 2_000), ("eu-west-1", NOW - 1_000)] {
        store
            .add(
                &Fact::new("acme", "deploy_target", target)
                    .namespace("caller")
                    .created_at(stamp),
            )
            .unwrap();
    }
}

#[test]
fn state_round_trips_through_the_file() {
    let (_d, store) = open_temp();
    let mut sub = AreevSubstrate::new(store, None);
    assert!(
        sub.load_state().unwrap().is_null(),
        "fresh file has no state"
    );
    sub.store_state(&serde_json::json!({"watermark_ms": 42}))
        .unwrap();
    assert_eq!(
        sub.load_state().unwrap(),
        serde_json::json!({"watermark_ms": 42})
    );
    // Overwrite (supersede the state chain).
    sub.store_state(&serde_json::json!({"watermark_ms": 99}))
        .unwrap();
    assert_eq!(sub.load_state().unwrap()["watermark_ms"], 99);
}

#[test]
fn user_facts_are_readable_and_live_filtered() {
    let (_d, mut store) = open_temp();
    let h1 = store
        .add(&Fact::new("u", "name", "Ann").namespace("caller"))
        .unwrap();
    // Supersede it; the old grain must drop out of a live read.
    let mut newer = Fact::new("u", "name", "Anne").namespace("caller");
    store.supersede(&h1, &mut newer).unwrap();
    let sub = AreevSubstrate::new(store, None);

    let live = sub
        .grains_of_type("fact", None, ReadOpts::default())
        .unwrap();
    let objs: Vec<_> = live
        .iter()
        .filter_map(|g| g.fact_object().map(str::to_string))
        .collect();
    assert!(objs.contains(&"Anne".to_string()), "live head present");
    assert!(
        !objs.contains(&"Ann".to_string()),
        "superseded grain filtered out via derived_from"
    );
}

#[test]
fn substrate_reads_respect_namespace_grants() {
    let (_d, mut store) = open_temp();
    let allowed = store
        .add(&Fact::new("visible", "kind", "allowed").namespace("allowed"))
        .unwrap();
    let denied = store
        .add(&Fact::new("secret", "kind", "denied").namespace("denied"))
        .unwrap();
    let facade = AreevFacade::with_session(store, Some("areev-loop".into()), None);
    facade.bind(AuthzSet::restricted(
        "agent:scoped",
        vec![Grant {
            verbs: vec![Verb::Read, Verb::LoopRun],
            namespaces: vec!["allowed".into(), "areev-loop".into()],
        }],
    ));
    let sub = BorrowedSubstrate::new(&facade);

    let all = sub
        .grains_of_type("fact", None, ReadOpts::default())
        .unwrap();
    assert!(all.iter().any(|g| g.hash == allowed.to_hex()));
    assert!(!all.iter().any(|g| g.hash == denied.to_hex()));
    assert!(sub
        .grains_of_type("fact", Some("denied"), ReadOpts::default())
        .is_err());
    assert!(sub.grain(&denied.to_hex()).is_err());
}

#[test]
fn agent_reserved_namespaces_are_not_analyzer_input() {
    let (_d, mut store) = open_temp();
    store
        .add(
            &Fact::new("run:r1", "mg:harness", "config")
                .namespace(areev_core::authz::HARNESS_NS),
        )
        .unwrap();
    store
        .add(&Fact::new("visible", "kind", "user-data").namespace("caller"))
        .unwrap();
    let sub = AreevSubstrate::new(store, None);
    let facts = sub
        .grains_of_type("fact", None, ReadOpts::default())
        .unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].namespace, "caller");
}

#[test]
fn replay_analysis_leaves_real_journal_unchanged() {
    let (_d, mut store) = open_temp();
    seed(&mut store);
    let before = store.stats().unwrap();
    let sub = AreevSubstrate::new(store, None);
    let engine = Engine::with_builtins();

    let decisions = engine
        .analyze_only(
            &sub,
            &RunOptions::default(),
            &std::collections::BTreeMap::new(),
            NOW,
        )
        .unwrap();
    assert!(!decisions.is_empty(), "the fixture must exercise the analyzers");

    let mut store = sub.into_store();
    let after = store.stats().unwrap();
    assert_eq!(after.ops, before.ops, "replay must not append to the op-log");
    assert_eq!(after.grains, before.grains, "replay must not append grains");
}

#[test]
fn end_to_end_run_review_apply() {
    let (_d, mut store) = open_temp();
    seed(&mut store);
    let mut sub = AreevSubstrate::new(store, None);
    let engine = Engine::with_builtins();

    let r = engine.run(&mut sub, &RunOptions::default(), NOW).unwrap();
    assert!(r.ran());
    assert!(
        r.stored >= 2,
        "expected duplicate + contradiction (+fork), got {}",
        r.stored
    );

    let pending = engine
        .recommendations(&sub, Some(RecStatus::Pending))
        .unwrap();
    assert_eq!(pending.len() as u64, r.stored, "listed == stored");

    // Second run is idempotent (dedup suppresses re-proposals).
    let r2 = engine
        .run(&mut sub, &RunOptions::default(), NOW + 1000)
        .unwrap();
    assert_eq!(r2.stored, 0, "no re-proposals on the second run");

    // Approve + apply a non-destructive recommendation through the real store.
    let target = pending
        .iter()
        .find(|x| !x.destructive)
        .expect("a non-destructive rec");
    let hash = target.hash.clone();
    let scopes = ScopeSet::all();
    engine
        .review(
            &mut sub,
            &hash,
            Decision::Approve,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "confirmed",
            NOW + 2000,
        )
        .unwrap();
    engine
        .apply(
            &mut sub,
            &hash,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "applying",
            false,
            NOW + 3000,
        )
        .unwrap();

    let after = engine.recommendations(&sub, None).unwrap();
    let applied = after.iter().find(|x| x.hash == hash).unwrap();
    assert_eq!(
        applied.status,
        RecStatus::Applied,
        "status persisted as applied"
    );
}

#[test]
fn tool_failure_clusters_from_tool_grains_and_applies() {
    let (_d, mut store) = open_temp();
    // Record tool calls as raw Tool grains — the store-level API, below the
    // occurrence identity that `record_tool_call` stamps. Content compacts to
    // `cnt` → `tool_content`.
    //
    // The four failures carry distinct payloads because at this layer
    // content-addressed dedup is still exactly right: `Tool::new(…)` with the
    // same fields inside one millisecond *is* one grain, and nothing here
    // claims otherwise. The occurrence semantics a host gets are covered by
    // `tests/occurrence_recording.rs`, which records identical payloads
    // through the binding path and asserts all five survive (#66).
    for attempt in 0..4 {
        store
            .add(
                &Tool::new("stripe_refund")
                    .content(&format!("rate_limited 429 (attempt {attempt})"))
                    .is_error(true)
                    .namespace("caller"),
            )
            .unwrap();
    }
    store
        .add(
            &Tool::new("stripe_refund")
                .content("ok")
                .is_error(false)
                .namespace("caller"),
        )
        .unwrap();
    let mut sub = AreevSubstrate::new(store, None);
    let engine = Engine::with_builtins();
    engine.run(&mut sub, &RunOptions::default(), NOW).unwrap();

    let recs = engine
        .recommendations(&sub, Some(RecStatus::Pending))
        .unwrap();
    let tf = recs
        .iter()
        .find(|r| r.analyzer.starts_with("loop.tool_failure"))
        .expect("a tool-failure recommendation");
    // The error signature (hence the lesson's object) must be non-empty.
    assert!(
        tf.summary.render().contains("rate_limited"),
        "signature must be present, got: {}",
        tf.summary.render()
    );

    // Apply must succeed — an empty object would trip VAL-E001.
    let hash = tf.hash.clone();
    let scopes = ScopeSet::all();
    engine
        .review(
            &mut sub,
            &hash,
            Decision::Approve,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "ok",
            NOW + 1,
        )
        .unwrap();
    engine
        .apply(
            &mut sub,
            &hash,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "apply",
            false,
            NOW + 2,
        )
        .unwrap();
    let after = engine.recommendations(&sub, None).unwrap();
    assert_eq!(
        after.iter().find(|r| r.hash == hash).unwrap().status,
        RecStatus::Applied
    );
}

#[test]
fn self_approval_block_holds_on_the_real_store() {
    let (_d, mut store) = open_temp();
    seed(&mut store);
    let mut sub = AreevSubstrate::new(store, None);
    let engine = Engine::with_builtins();
    engine.run(&mut sub, &RunOptions::default(), NOW).unwrap();
    let rec = engine
        .recommendations(&sub, Some(RecStatus::Pending))
        .unwrap()[0]
        .clone();
    let creator = format!("engine:{}", rec.analyzer);

    let blocked = engine.review(
        &mut sub,
        &rec.hash,
        Decision::Approve,
        &creator,
        ObserverType::System,
        &ScopeSet::all(),
        "self",
        NOW + 100,
    );
    assert!(matches!(blocked, Err(areev_loop::Error::SelfApproval(_))));
}

/// The review queue must come back worst-first and in the *same* order every
/// run. It used to sort on the content hash alone, which is deterministic
/// within a run but reshuffles across runs, because a grain's hash covers its
/// creation timestamp — so an identical queue was presented to the reviewer in
/// a different order each time, and severity had no bearing on it at all.
#[test]
fn review_queue_is_severity_ordered_and_stable_across_runs() {
    let mut orders = Vec::new();
    for _ in 0..3 {
        let (_d, mut store) = open_temp();
        seed(&mut store);
        // A tool failure lands a higher severity alongside the seeded findings.
        for i in 0..4 {
            store
                .add(
                    &Tool::new("crm_export")
                        .content(&format!("timeout after 30s (attempt {i})"))
                        .is_error(true)
                        .namespace("caller"),
                )
                .unwrap();
        }
        let mut sub = AreevSubstrate::new(store, None);
        let engine = Engine::with_builtins();
        engine.run(&mut sub, &RunOptions::default(), NOW).unwrap();

        let pending = engine
            .recommendations(&sub, Some(RecStatus::Pending))
            .unwrap();
        assert!(pending.len() >= 2, "need several to have an order at all");

        let severities: Vec<_> = pending.iter().map(|r| r.severity).collect();
        let mut sorted = severities.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(
            severities, sorted,
            "queue must be highest-severity-first, got {severities:?}"
        );

        orders.push(
            pending
                .iter()
                .map(|r| (r.severity, r.analyzer.clone(), r.summary.render()))
                .collect::<Vec<_>>(),
        );
    }

    assert_eq!(orders[0], orders[1], "order drifted between run 1 and 2");
    assert_eq!(orders[1], orders[2], "order drifted between run 2 and 3");
}

/// The substrate is the last line of defense under the engine's destructive
/// gate: destructive CAL beyond the audited single-grain FORGET arm must be
/// refused at validation AND at execution, so a mis-stamped proposal can
/// never reach the process-permissive fallthrough executor.
#[test]
fn destructive_cal_beyond_forget_hash_is_refused() {
    let (_dir, store) = open_temp();
    let mut sub = AreevSubstrate::new(store, None);

    let purge = r#"PURGE OLDER THAN 90d BECAUSE "retention""#;
    let forget_subject = r#"FORGET SUBJECT "pat" BECAUSE "gdpr""#;

    assert!(sub.validate_cal(purge).is_err(), "PURGE must fail validation");
    assert!(
        sub.validate_cal(forget_subject).is_err(),
        "FORGET SUBJECT must fail validation (loop FORGET is hash-only)"
    );
    assert!(sub.execute_cal(purge).is_err(), "PURGE must fail execution");
    assert!(
        sub.execute_cal(forget_subject).is_err(),
        "FORGET SUBJECT must fail execution"
    );

    // Reads and the writer's own statements still pass.
    assert!(sub.validate_cal(r#"RECALL facts WHERE subject = "acme""#).is_ok());
    assert!(sub.validate_cal(r#"ADD fact {"subject":"a","relation":"r","object":"o"}"#).is_ok());
}

/// A saved-query rewrite is executable AND undoable (issue #28).
///
/// The loop can already evolve grains; it could not evolve the saved CAL that
/// assembles a prompt, which is exactly where a self-improving agent's
/// behaviour lives. What made that unsafe was the missing inverse: `DEFINE
/// QUERY` writes a `qry:` registry row, not a grain, so a rollback that only
/// retracts `created_hashes` would report success while the new definition
/// stayed live. This pins the round trip.
#[test]
fn a_definition_rewrite_records_an_inverse_that_restores_the_previous_body() {
    use areev_cal::facade::CalStoreFacade;
    let (_d, store) = open_temp();
    let facade = AreevFacade::with_session(store, Some("caller".into()), None);
    let sub = BorrowedSubstrate::new(&facade);

    // Nothing defined yet: the inverse is a DROP, which restores "no such
    // definition" — the correct prior state, not a no-op.
    let first = r#"DEFINE QUERY "triage_ctx" AS { RECALL facts RECENT 5 }"#;
    let inv = sub.definition_inverse(first).unwrap().expect("an inverse");
    assert!(inv.starts_with("DROP QUERY"), "got: {inv}");

    // Define it, then ask for the inverse of a REWRITE: now it must reproduce
    // the body that is about to be replaced.
    run_cal(&facade, first);
    let rewrite = r#"DEFINE QUERY "triage_ctx" AS { RECALL facts RECENT 20 }"#;
    let inv2 = sub.definition_inverse(rewrite).unwrap().expect("an inverse");
    assert!(inv2.contains("RECENT 5"), "must carry the OLD body: {inv2}");
    assert!(inv2.starts_with("DEFINE QUERY"), "got: {inv2}");

    // Applying the inverse really restores the previous definition.
    run_cal(&facade, rewrite);
    assert!(facade.get_query("triage_ctx").unwrap().body.contains("RECENT 20"));
    run_cal(&facade, &inv2);
    assert!(
        facade.get_query("triage_ctx").unwrap().body.contains("RECENT 5"),
        "the inverse must restore the prior body"
    );

    // And the DROP inverse really removes a definition that did not exist
    // before the apply.
    run_cal(&facade, &inv);
    assert!(
        facade.get_query("triage_ctx").is_none(),
        "the DROP inverse must restore 'no such definition'"
    );
}

/// A non-definition proposal is untouched: no inverse, nothing to restore.
#[test]
fn ordinary_proposals_have_no_definition_inverse() {
    let (_d, store) = open_temp();
    let facade = AreevFacade::with_session(store, Some("caller".into()), None);
    let sub = BorrowedSubstrate::new(&facade);
    assert!(sub.definition_inverse("ADD fact {}").unwrap().is_none());
    assert!(sub.definition_inverse("FORGET abc").unwrap().is_none());
}

fn run_cal(facade: &AreevFacade, cal: &str) {
    let ex = areev_cal::CalExecutor::new(areev_cal::CalExecutorConfig::default());
    ex.execute(cal, facade).unwrap_or_else(|e| panic!("{cal}: {e}"));
}
