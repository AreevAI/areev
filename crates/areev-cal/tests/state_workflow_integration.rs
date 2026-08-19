//! End-to-end coverage for the State (0x03) and Workflow (0x04) grains.
//!
//! Both types were writable from CAL/MCP/bindings but had no test that crossed
//! the parser boundary — every existing workflow test asserted on the AST and
//! stopped. That gap hid a family of bugs: the State snapshot being overwritten
//! by the common `context` field, `context_data` accepted on the write path but
//! never read, workflow `name`/`status` landing somewhere the templates could
//! not see, and `* N` bounds silently overwriting each other on a shared
//! destination node. These tests drive real CAL text through a real store and
//! read the grains back.

use areev_cal::executor::CalResultPayload;
use areev_cal::{CalExecutor, CalExecutorConfig, AreevFacade};
use areev_store::Areev;
use tempfile::TempDir;

fn setup() -> (CalExecutor, AreevFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = AreevFacade::with_session(m, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn added_hash(payload: &CalResultPayload) -> String {
    match payload {
        CalResultPayload::Added { hash, .. } => hash.clone(),
        CalResultPayload::Superseded { new_hash, .. } => new_hash.clone(),
        other => panic!("expected Added/Superseded payload, got: {other:?}"),
    }
}

/// Recall every grain of `plural` and return the first as JSON.
fn recall_one(ex: &CalExecutor, facade: &AreevFacade, plural: &str) -> serde_json::Value {
    let out = ex
        .execute(&format!("RECALL {plural} RECENT 10"), facade)
        .unwrap();
    match out.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1, "expected exactly one {plural} grain");
            serde_json::to_value(&grains[0]).unwrap()
        }
        other => panic!("expected Grains for {plural}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Workflow (0x04)
// ---------------------------------------------------------------------------

#[test]
fn workflow_graph_survives_the_round_trip() {
    let (ex, facade, _d) = setup();

    let add = ex
        .execute(
            r#"ADD workflow "CI pipeline"
                 build -> test -> deploy
               BIND test = sha256:1111111111111111111111111111111111111111111111111111111111111111
               REASON "integration""#,
            &facade,
        )
        .unwrap();
    assert_eq!(added_hash(&add.result).len(), 64);

    let g = recall_one(&ex, &facade, "workflows");
    let f = &g["fields"];

    // No `trigger` field: it was removed in 1.3 because nothing read it. A
    // plan is its graph; what starts it is a Trigger grain pointing at this
    // hash.
    assert!(f.get("trigger").is_none(), "the inert trigger field must be gone");
    assert_eq!(
        f["nodes"],
        serde_json::json!(["build", "test", "deploy"]),
        "node order is load-bearing — it defines the entry point (OMS §8.4)"
    );

    // The topology itself must survive, not just its cardinality.
    let edges = f["edges"].as_array().expect("edges must round-trip");
    assert_eq!(edges.len(), 2, "got {edges:?}");
    assert_eq!(edges[0]["src"], "build");
    assert_eq!(edges[0]["dst"], "test");
    assert_eq!(edges[1]["src"], "test");
    assert_eq!(edges[1]["dst"], "deploy");

    assert_eq!(
        f["bindings"]["test"],
        "sha256:1111111111111111111111111111111111111111111111111111111111111111"
    );
}

#[test]
fn workflow_name_is_readable_at_top_level() {
    // `name` has no OMS §8.4 field, so it rides in `extra_fields` and serializes
    // verbatim at top level. It used to be routed into `common.context`, where
    // `templates::field_str` — which only ever reads top-level fields — could not
    // see it, leaving every shipped `{{name}}` template rendering blank.
    let (ex, facade, _d) = setup();

    ex.execute(
        r#"ADD workflow "nightly backup" a -> b REASON "integration""#,
        &facade,
    )
    .unwrap();

    let g = recall_one(&ex, &facade, "workflows");
    assert_eq!(
        g["fields"]["name"], "nightly backup",
        "workflow name must be a top-level field, not buried in context"
    );
}

#[test]
fn workflow_conditional_edges_keep_their_conditions() {
    let (ex, facade, _d) = setup();

    ex.execute(
        r#"ADD workflow "triage" review -> escalate WHEN "severity > 3" REASON "integration""#,
        &facade,
    )
    .unwrap();

    let g = recall_one(&ex, &facade, "workflows");
    let edges = g["fields"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["cond"], "severity > 3");
}

#[test]
fn workflow_diamond_converges_with_a_single_retry_bound() {
    // `retries` is keyed by node, so when several edges share a destination the
    // executor must decide what a repeat means. Today the group syntax gives every
    // converging edge the *same* `* N`, so the fold is a no-op — the executor takes
    // the largest bound defensively, which is only observable if the grammar ever
    // grows a way to give converging edges different repeats (there is no
    // multi-chain syntax at present).
    let (ex, facade, _d) = setup();

    ex.execute(
        r#"ADD workflow "fanin" fetch -> (left, right) -> merge * 2 REASON "integration""#,
        &facade,
    )
    .unwrap();

    let g = recall_one(&ex, &facade, "workflows");
    assert_eq!(
        g["fields"]["edges"].as_array().unwrap().len(),
        4,
        "diamond expands to fetch->left, fetch->right, left->merge, right->merge"
    );
    assert_eq!(
        g["fields"]["retries"]["merge"], 2,
        "both converging edges carry * 2; the node keeps one bound"
    );
}

#[test]
fn workflow_repeat_attaches_to_the_target_node() {
    let (ex, facade, _d) = setup();

    ex.execute(
        r#"ADD workflow "retry" a -> c * 2 -> d REASON "integration""#,
        &facade,
    )
    .unwrap();
    let g = recall_one(&ex, &facade, "workflows");
    assert_eq!(g["fields"]["retries"]["c"], 2);
    assert!(
        g["fields"]["retries"].get("d").is_none(),
        "only the node the * N was attached to gets a bound"
    );
}

#[test]
fn workflow_rejects_edges_referencing_unknown_nodes() {
    // OMS §8.4 validation constraint: every `src`/`dst` MUST reference an element
    // of `nodes`. CAL's graph syntax makes a dangling edge unspellable, but the
    // JSON path (MCP / Python / Node all reach it) can express one, so the
    // validation has to live there.
    let (_ex, facade, _d) = setup();
    use areev_cal::facade::CalStoreFacade;

    let fields = serde_json::json!({
        "nodes": ["build", "test"],
        "edges": [{ "src": "build", "dst": "nowhere" }],
        "namespace": "caller",
    });
    let err = facade
        .cal_add("workflow", fields.as_object().unwrap())
        .expect_err("a dangling edge must be rejected, not silently stored");
    let msg = err.to_string();
    assert!(
        msg.contains("nowhere"),
        "error should name the offending node, got: {msg}"
    );
}

#[test]
fn workflow_rejects_duplicate_node_ids() {
    // OMS §8.4: node IDs MUST be unique within the grain — they are also the
    // keys for `bindings` and `retries`, so a duplicate makes those ambiguous.
    let (_ex, facade, _d) = setup();
    use areev_cal::facade::CalStoreFacade;

    let fields = serde_json::json!({
        "nodes": ["build", "build"],
        "namespace": "caller",
    });
    assert!(
        facade
            .cal_add("workflow", fields.as_object().unwrap())
            .is_err(),
        "duplicate node IDs must be rejected"
    );
}

// ---------------------------------------------------------------------------
// State (0x03)
// ---------------------------------------------------------------------------

#[test]
fn state_snapshot_round_trips_through_the_store() {
    // `context` is the OMS §8.3 field name and takes precedence over the `data` /
    // `context_data` aliases. (The common-context clobber this used to guard is
    // only reachable through the Rust API — see areev-core's
    // `state_context_is_not_clobbered_by_common_context`.)
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context": { "label": "planning_phase", "active_node": "build", "step": 3 },
        "namespace": "caller",
    });
    let hash = facade_add(&facade, "state", &fields);
    assert_eq!(hash.len(), 64);

    let g = recall_one(&ex, &facade, "states");
    let ctx = &g["fields"]["context"];
    assert_eq!(ctx["label"], "planning_phase");
    assert_eq!(ctx["active_node"], "build");
    assert_eq!(ctx["step"], 3);
}

#[test]
fn state_context_takes_precedence_over_data_alias() {
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context": { "label": "wins" },
        "data": { "label": "loses" },
        "namespace": "caller",
    });
    facade_add(&facade, "state", &fields);

    let g = recall_one(&ex, &facade, "states");
    assert_eq!(g["fields"]["context"]["label"], "wins");
}

#[test]
fn state_accepts_context_data_alias() {
    // `context_data` was listed as a known State field but the builder only ever
    // read `data`, so this stored an empty snapshot and dropped the payload.
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context_data": { "label": "resumed", "cursor": 42 },
        "namespace": "caller",
    });
    facade_add(&facade, "state", &fields);

    let g = recall_one(&ex, &facade, "states");
    assert_eq!(g["fields"]["context"]["label"], "resumed");
    assert_eq!(g["fields"]["context"]["cursor"], 42);
}

#[test]
fn state_plan_and_history_round_trip() {
    // OMS §8.3 optional fields that had no struct field at all until now.
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context": { "label": "mid-run" },
        "plan": ["fetch", "transform", "load"],
        "history": [{ "node": "fetch", "ok": true }],
        "namespace": "caller",
    });
    facade_add(&facade, "state", &fields);

    let g = recall_one(&ex, &facade, "states");
    assert_eq!(
        g["fields"]["plan"],
        serde_json::json!(["fetch", "transform", "load"])
    );
    assert_eq!(g["fields"]["history"][0]["node"], "fetch");
}

#[test]
fn state_snapshot_keys_are_never_rewritten() {
    // A State's inner keys are user data. Applying the OMS FIELD_MAP to them
    // would rewrite any key colliding with a short code (`o` -> `object`),
    // corrupting the payload and destabilizing the content address on rewrite.
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context": { "o": "not-an-object-field", "s": 1, "rel": "keep-me" },
        "namespace": "caller",
    });
    facade_add(&facade, "state", &fields);

    let g = recall_one(&ex, &facade, "states");
    let ctx = &g["fields"]["context"];
    assert_eq!(ctx["o"], "not-an-object-field");
    assert_eq!(ctx["s"], 1);
    assert_eq!(ctx["rel"], "keep-me");
    assert!(ctx.get("object").is_none(), "inner keys must stay verbatim");
}

// ---------------------------------------------------------------------------
// Event.run_id — the only run-scoped correlation key in the grain model
// ---------------------------------------------------------------------------

#[test]
fn events_are_filterable_by_run_id() {
    // `run_id` has been serialized since 1.0 but was absent from the registry's
    // queryable_fields, so it was write-only: you could store it and never ask
    // for it back.
    let (ex, facade, _d) = setup();
    use areev_cal::facade::CalStoreFacade;

    for (run, body) in [
        ("run-a", "started build"),
        ("run-a", "build ok"),
        ("run-b", "started build"),
    ] {
        let fields = serde_json::json!({
            "content": body,
            "run_id": run,
            "namespace": "caller",
        });
        facade
            .cal_add("event", fields.as_object().unwrap())
            .unwrap();
    }

    let out = ex
        .execute(r#"RECALL events WHERE run_id = "run-a" RECENT 20"#, &facade)
        .unwrap();
    match out.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 2, "expected only run-a's events");
            for g in &grains {
                let v = serde_json::to_value(g).unwrap();
                assert_eq!(v["fields"]["run_id"], "run-a");
            }
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn run_id_round_trips_under_its_oms_wire_key() {
    // The typed builder must set `Event.run_id` (wire key `rid`), not leave it to
    // `extra_fields` — which wrote it twice and never under its spec key.
    let (ex, facade, _d) = setup();
    use areev_cal::facade::CalStoreFacade;

    let fields = serde_json::json!({
        "content": "step complete",
        "run_id": "wf_1234",
        "namespace": "caller",
    });
    facade
        .cal_add("event", fields.as_object().unwrap())
        .unwrap();

    let g = recall_one(&ex, &facade, "events");
    assert_eq!(g["fields"]["run_id"], "wf_1234");
    assert!(
        g["fields"]["context"].get("run_id").is_none(),
        "run_id must not be duplicated into the common context map"
    );
}

// ---------------------------------------------------------------------------
// `add_via_set` is a shape fact, not a permission
// ---------------------------------------------------------------------------

#[test]
fn generic_add_set_rejects_types_it_cannot_shape() {
    // A Workflow is a graph and a State is an arbitrary JSON snapshot; neither
    // can be expressed as a flat list of `SET k = v` pairs, so the generic form
    // refuses them — with a message naming what it *can* build.
    let (ex, facade, _d) = setup();

    for (stmt, ty) in [
        (
            r#"ADD workflow SET nodes = "a" REASON "x""#,
            "workflow",
        ),
        (
            r#"ADD state SET data = "a" REASON "x""#,
            "state",
        ),
    ] {
        match ex.execute(stmt, &facade) {
            // Either the parser refuses the shape or the executor reports it.
            Err(_) => {}
            Ok(out) => match out.result {
                CalResultPayload::Unsupported { message, .. } => {
                    assert!(
                        message.contains(ty),
                        "message should name the rejected type, got: {message}"
                    );
                }
                other => panic!("expected rejection for `{stmt}`, got {other:?}"),
            },
        }
    }
}

#[test]
fn structured_paths_are_not_gated_by_add_via_set() {
    // The same two types are perfectly writable through their purpose-built
    // paths. This is the invariant the flag's old name and doc obscured: it
    // gates one CAL form, not the right to create the grain. Gating `cal_add`
    // on it would break remember(), the memory-tool adapter, the migrate
    // importers, and every host that checkpoints State.
    let (ex, facade, _d) = setup();
    use areev_cal::facade::CalStoreFacade;

    // Dedicated CAL statement.
    ex.execute(
        r#"ADD workflow "shaped" a -> b REASON "integration""#,
        &facade,
    )
    .unwrap();
    assert_eq!(recall_one(&ex, &facade, "workflows")["fields"]["name"], "shaped");

    // Per-type JSON builder — the surface MCP, Python and Node reach.
    let fields = serde_json::json!({ "data": { "label": "ok" }, "namespace": "caller" });
    facade
        .cal_add("state", fields.as_object().unwrap())
        .expect("cal_add must not be gated by add_via_set");
    assert_eq!(recall_one(&ex, &facade, "states")["fields"]["context"]["label"], "ok");
}

/// Write a grain through the facade's JSON add path (the surface MCP, Python and
/// Node all reach), returning the hex hash.
fn facade_add(facade: &AreevFacade, grain_type: &str, fields: &serde_json::Value) -> String {
    use areev_cal::facade::CalStoreFacade;
    let map = fields.as_object().unwrap().clone();
    let hash = facade.cal_add(grain_type, &map).unwrap();
    hex::encode(hash.as_bytes())
}

// ---------------------------------------------------------------------------
// WITH multi_hop(n) — entity-graph expansion
// ---------------------------------------------------------------------------

/// A chain the first pass cannot reach in one step: the query matches only
/// `alice`, but `carol`'s fact is two entity hops away.
fn hop_chain(ex: &CalExecutor, facade: &AreevFacade) {
    for (s, r, o) in [
        ("alice", "reports_to", "bob"),
        ("bob", "reports_to", "carol"),
        ("carol", "owns", "the-deploy-key"),
    ] {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "{s}" SET relation = "{r}" SET object = "{o}"
                   SET namespace = "caller" REASON "hop""#
            ),
            facade,
        )
        .unwrap();
    }
}

fn objects(payload: CalResultPayload) -> Vec<String> {
    match payload {
        CalResultPayload::Grains { grains, .. } => grains
            .iter()
            .filter_map(|g| {
                serde_json::to_value(g).ok()?["fields"]["object"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect(),
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn multi_hop_reaches_facts_the_first_pass_cannot() {
    // `WITH multi_hop(n)` was parsed, clamped and stored on RecallParams — and
    // never read by any recall path, so the documented option did nothing.
    let (ex, facade, _d) = setup();
    hop_chain(&ex, &facade);

    let plain = objects(
        ex.execute(r#"RECALL facts WHERE subject = "alice""#, &facade)
            .unwrap()
            .result,
    );
    assert_eq!(plain, vec!["bob"], "one anchored match without expansion");

    let hopped = objects(
        ex.execute(
            r#"RECALL facts WHERE subject = "alice" LIMIT 10 WITH multi_hop(2)"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    assert!(
        hopped.contains(&"the-deploy-key".to_string()),
        "two hops must reach carol's fact, got {hopped:?}"
    );
    assert_eq!(hopped[0], "bob", "the direct match still ranks first");
}

#[test]
fn multi_hop_depth_bounds_how_far_it_reaches() {
    let (ex, facade, _d) = setup();
    hop_chain(&ex, &facade);

    let one = objects(
        ex.execute(
            r#"RECALL facts WHERE subject = "alice" LIMIT 10 WITH multi_hop(1)"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    assert!(one.contains(&"carol".to_string()), "one hop reaches bob's fact");
    assert!(
        !one.contains(&"the-deploy-key".to_string()),
        "one hop must not reach two hops out: {one:?}"
    );
}

#[test]
fn multi_hop_respects_the_limit() {
    // Expansion competes for the k slots the caller asked for; it must not
    // silently return more rows than LIMIT.
    let (ex, facade, _d) = setup();
    hop_chain(&ex, &facade);

    let hits = objects(
        ex.execute(
            r#"RECALL facts WHERE subject = "alice" LIMIT 2 WITH multi_hop(3)"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    assert!(hits.len() <= 2, "LIMIT 2 must cap the result: {hits:?}");
}

// ---------------------------------------------------------------------------
// Recommendation (0x0C) — OMS 1.5 / CAL 1.2
// ---------------------------------------------------------------------------

#[test]
fn recommendations_are_recallable_but_never_addable() {
    // OMS 1.5 makes Recommendation query-only: it is engine-emitted and
    // lifecycle-gated, so CAL deliberately has no `ADD recommendation` and
    // lifecycle transitions never happen through ADD/SUPERSEDE SET.
    let (ex, facade, _d) = setup();
    use areev_cal::facade::CalStoreFacade;

    // The generic ADD form refuses it.
    let out = ex
        .execute(
            r#"ADD recommendation SET target_ref = "entity:caller/alice" REASON "x""#,
            &facade,
        )
        .unwrap();
    match out.result {
        CalResultPayload::Unsupported { message, .. } => {
            assert!(message.contains("recommendation"), "{message}");
        }
        other => panic!("ADD recommendation must be refused, got {other:?}"),
    }

    // And so does the JSON write path every binding reaches.
    let fields = serde_json::json!({ "target_ref": "entity:caller/alice" });
    assert!(
        facade
            .cal_add("recommendation", fields.as_object().unwrap())
            .is_err(),
        "cal_add must not mint recommendations either"
    );

    // But the type is a first-class query target.
    let recalled = ex.execute("RECALL recommendations RECENT 10", &facade).unwrap();
    match recalled.result {
        CalResultPayload::Grains { grains, .. } => assert!(grains.is_empty()),
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn recommendation_round_trips_through_the_store_and_renders() {
    // Written through the store (the engine path), read back through CAL.
    let (ex, facade, _d) = setup();
    use areev_core::types::*;

    let rec = Recommendation::new(
        "entity:caller/alice",
        Analyzer::new("loop.duplicate_sweep/1"),
        Summary {
            template_id: "dup.consolidate".into(),
            args: serde_json::json!({ "subject": "alice", "count": 3 }),
        },
        Proposal::Cal("SUPERSEDE sha256:aa BECAUSE \"duplicate\"".into()),
    )
    .severity(Severity::Medium)
    .namespace("caller");
    facade.with_store(|m| m.add(&rec)).unwrap();

    let g = recall_one(&ex, &facade, "recommendations");
    let f = &g["fields"];
    assert_eq!(f["target_ref"], "entity:caller/alice");
    assert_eq!(f["analyzer"]["id"], "loop.duplicate_sweep/1");
    assert_eq!(f["summary"]["template_id"], "dup.consolidate");
    assert_eq!(f["summary"]["args"]["count"], 3);
    assert_eq!(f["severity"], "medium");
    assert_eq!(f["proposal_cal"], "SUPERSEDE sha256:aa BECAUSE \"duplicate\"");
    assert_eq!(f["dedup_key"].as_str().unwrap().len(), 64);

    // rec_status is index-layer: it must never appear in the blob, which is
    // what keeps the content address stable across the review lifecycle.
    assert!(
        f.get("rec_status").is_none(),
        "rec_status must not be serialized: {f}"
    );
}

#[test]
fn recommendations_are_filterable_by_their_type_specific_fields() {
    let (ex, facade, _d) = setup();
    use areev_core::types::*;

    for (target, sev) in [
        ("entity:caller/alice", Severity::High),
        ("entity:caller/bob", Severity::Low),
    ] {
        let rec = Recommendation::new(
            target,
            Analyzer::new("loop.staleness/1"),
            Summary {
                template_id: "stale".into(),
                args: serde_json::json!({}),
            },
            Proposal::Cal("x".into()),
        )
        .severity(sev)
        .namespace("caller");
        facade.with_store(|m| m.add(&rec)).unwrap();
    }

    let out = ex
        .execute(
            r#"RECALL recommendations WHERE severity = "high" RECENT 10"#,
            &facade,
        )
        .unwrap();
    match out.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1, "severity must filter");
            let v = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(v["fields"]["target_ref"], "entity:caller/alice");
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// CAL 1.2 — the ELEMENT shorthand (§10.6.1)
// ---------------------------------------------------------------------------

fn rendered(payload: CalResultPayload) -> String {
    match payload {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got {other:?}"),
    }
}

fn three_facts(ex: &CalExecutor, facade: &AreevFacade) {
    for (s, o) in [("alice", "tea"), ("bob", "coffee"), ("cara", "water")] {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "{s}" SET relation = "prefers" SET object = "{o}"
                   SET namespace = "caller" REASON "seed""#
            ),
            facade,
        )
        .unwrap();
    }
}

#[test]
fn element_shorthand_renders_once_per_grain_not_once_per_result() {
    // §10.6.1: `TEMPLATE "<text>"` is exactly `TEMPLATE { ELEMENT { <text> } }`
    // — the string renders ONE grain and the engine iterates. Rendering it
    // whole-result would be a different model entirely.
    let (ex, facade, _d) = setup();
    three_facts(&ex, &facade);

    let out = rendered(
        ex.execute(
            r#"RECALL facts WHERE relation = "prefers" RECENT 10
               FORMAT TEMPLATE "{{grain.subject}} likes {{grain.object}}""#,
            &facade,
        )
        .unwrap()
        .result,
    );
    for (s, o) in [("alice", "tea"), ("bob", "coffee"), ("cara", "water")] {
        assert!(out.contains(&format!("{s} likes {o}")), "missing {s}: {out}");
    }
}

#[test]
fn a_named_shorthand_template_is_indistinguishable_from_its_section_form() {
    // The spec defines the shorthand by equivalence, so both spellings must
    // produce byte-identical output.
    let (ex, facade, _d) = setup();
    three_facts(&ex, &facade);

    ex.execute(
        r#"DEFINE TEMPLATE short AS "{{grain.subject}}={{grain.object}}""#,
        &facade,
    )
    .unwrap();
    ex.execute(
        "DEFINE TEMPLATE long ELEMENT {\n{{grain.subject}}={{grain.object}}\n}",
        &facade,
    )
    .unwrap();

    let a = rendered(
        ex.execute(
            r#"RECALL facts WHERE relation = "prefers" RECENT 10 FORMAT TEMPLATE short"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    let b = rendered(
        ex.execute(
            r#"RECALL facts WHERE relation = "prefers" RECENT 10 FORMAT TEMPLATE long"#,
            &facade,
        )
        .unwrap()
        .result,
    );
    assert_eq!(a.trim(), b.trim(), "shorthand must equal its section form");
}

#[test]
fn a_definition_may_not_combine_the_shorthand_and_a_section_list() {
    // §10.6.1: "A definition MUST NOT combine the two forms."
    let (ex, facade, _d) = setup();
    assert!(
        ex.execute(
            r#"DEFINE TEMPLATE bad AS "{{grain.subject}}" ELEMENT { x }"#,
            &facade,
        )
        .is_err(),
        "combining the shorthand with a section list must be refused"
    );
}

#[test]
fn an_inline_shorthand_can_be_aliased_alongside_another_format() {
    // The §10.1.1 example the spec's own grammar used to reject.
    let (ex, facade, _d) = setup();
    three_facts(&ex, &facade);

    let out = ex
        .execute(
            r#"RECALL facts WHERE relation = "prefers" RECENT 10
               FORMAT [json AS structured, TEMPLATE "{{grain.subject}}: {{grain.object}}" AS oneliner]"#,
            &facade,
        )
        .unwrap();
    // Multi-format results carry both renderings.
    let v = serde_json::to_value(&out.result).unwrap();
    let s = v.to_string();
    assert!(s.contains("structured"), "{s}");
    assert!(s.contains("oneliner"), "{s}");
}

#[test]
fn multi_hop_follows_a_relation_in_both_directions() {
    // Entities are harvested from a result's `subject` *and* `object`, but
    // expansion used to re-anchor each one as a subject only. So "who else
    // reports to this manager" — reaching an entity through the object position
    // and looking back — returned nothing, and multi_hop looked like it worked
    // because the forward direction did.
    let (ex, facade, _d) = setup();
    for q in [
        r#"ADD fact SET subject = "alice" SET relation = "reports_to" SET object = "carol" REASON "seed""#,
        r#"ADD fact SET subject = "bob" SET relation = "reports_to" SET object = "carol" REASON "seed""#,
        r#"ADD fact SET subject = "carol" SET relation = "part_of" SET object = "platform" REASON "seed""#,
    ] {
        ex.execute(q, &facade).unwrap();
    }

    let out = ex
        .execute(
            r#"RECALL facts WHERE subject = "alice" LIMIT 20 WITH multi_hop(2)"#,
            &facade,
        )
        .unwrap();
    let s = serde_json::to_value(&out.result).unwrap().to_string();

    assert!(
        s.contains("platform"),
        "forward hop alice -> carol -> platform must be reached: {s}"
    );
    assert!(
        s.contains("bob"),
        "reverse hop carol <- bob must be reached: {s}"
    );
}

#[test]
fn auto_relate_warns_under_its_own_code_and_reads_cleanly() {
    let (ex, facade, _d) = setup();
    let out = ex
        .execute(
            r#"ADD fact SET subject = "a" SET relation = "knows" SET object = "b" WITH auto_relate REASON "r""#,
            &facade,
        )
        .unwrap();

    let w = out
        .warnings
        .iter()
        .find(|w| w.contains("auto_relate"))
        .unwrap_or_else(|| panic!("expected an auto_relate warning, got {:?}", out.warnings));

    assert!(w.starts_with("CAL-W013:"), "{w}");
    assert!(
        !w.contains("CAL-W004"),
        "W004 is UnknownExtensionOption; a code must locate one variant: {w}"
    );
    assert!(
        !w.contains("  "),
        "a broken line continuation left runs of spaces in the message: {w:?}"
    );
}

#[test]
fn a_workflow_name_is_stored_once_and_readably() {
    // `name` used to be written twice: verbatim at top level *and* inside the
    // common context, where `common.context` compacts its keys and the reader
    // reverses only the `int:*` profile set — so the second copy came back as
    // the unreadable short code `skname`.
    let (ex, facade, _d) = setup();
    let added = ex
        .execute(
            r#"ADD workflow "CI pipeline" graph build -> test REASON "seed""#,
            &facade,
        )
        .unwrap();
    let hash = added_hash(&added.result);

    let got = ex
        .execute(r#"RECALL workflows RECENT 5"#, &facade)
        .unwrap();
    let v = serde_json::to_value(&got.result).unwrap();
    let s = v.to_string();

    assert!(s.contains(&hash), "the workflow must come back: {s}");
    assert!(s.contains("CI pipeline"), "the name must survive: {s}");
    assert!(
        !s.contains("skname"),
        "a compacted duplicate leaked into the context map: {s}"
    );
}
