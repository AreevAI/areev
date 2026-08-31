//! The two substrate capabilities an LLM `proposal` resolves against:
//! `validate_plan` (a `plan_revision` must produce a graph the runtime can
//! actually walk) and `tool_evalset` (a `code_revision`'s Rule E1 pin comes
//! from the TOOL, never from the proposer).
//!
//! Both are read-side gates that run BEFORE a recommendation is written, so a
//! silent wrong answer here is a proposal that reaches a reviewer looking
//! governed when it is not. That is what these pin.

use areev_cal::AreevFacade;
use areev_core::authz::{AuthzSet, Grant, Verb};
use areev_core::types::{Grain, Tool, ToolKind};
use areev_loop::{ReadOpts, SubstrateRead};
use areev_loop_adapter::{BorrowedSubstrate, AreevSubstrate};
use areev_store::Areev;
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;

fn open_temp() -> (tempfile::TempDir, Areev) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let store = Areev::open(path.to_str().unwrap()).unwrap();
    (dir, store)
}

/// A tool DEFINITION grain, optionally declaring its Rule E1 evalset. The pin
/// rides in `extra_fields` — the same shape the JSON `add` path writes, which
/// is how every host (`areev tune`, the examples' `db.add("tool", …)`) puts it
/// there.
fn definition(name: &str, evalset: Option<&str>) -> Tool {
    let mut t = Tool::new(name)
        .kind(ToolKind::Definition)
        .tool_description("a tool")
        .namespace("caller");
    if let Some(e) = evalset {
        t.common
            .extra_fields
            .insert("evalset_hash".into(), json!(e));
    }
    t
}

// ── validate_plan ──────────────────────────────────────────────────────

#[test]
fn a_runnable_plan_validates() {
    let (_d, store) = open_temp();
    let sub = AreevSubstrate::new(store, None);
    let plan = json!({
        "nodes": ["fetch", "score", "post"],
        "edges": [
            {"src": "fetch", "dst": "score"},
            {"src": "score", "dst": "post", "cond": "ok == true"},
        ],
        "bindings": {"fetch": "aa".repeat(32)},
        "retries": {"fetch": 2},
    });
    sub.validate_plan(&plan).expect("a walkable graph validates");
}

/// V3: a node nothing reaches. The loop must not hand a reviewer a plan whose
/// new node can never run.
#[test]
fn an_unreachable_node_is_refused() {
    let (_d, store) = open_temp();
    let sub = AreevSubstrate::new(store, None);
    let plan = json!({
        "nodes": ["fetch", "score", "orphan"],
        "edges": [{"src": "fetch", "dst": "score"}],
    });
    let err = sub.validate_plan(&plan).unwrap_err().to_string();
    assert!(
        err.contains("not runnable"),
        "the refusal must say the plan is the problem: {err}"
    );
}

/// V6: a cycle with no `max_cycles` anywhere on it runs forever. The runtime's
/// own Tarjan pass decides this — the loop carries no second opinion.
#[test]
fn an_unbounded_cycle_is_refused() {
    let (_d, store) = open_temp();
    let sub = AreevSubstrate::new(store, None);
    let plan = json!({
        "nodes": ["a", "b"],
        "edges": [
            {"src": "a", "dst": "b"},
            {"src": "b", "dst": "a"},
        ],
    });
    assert!(sub.validate_plan(&plan).is_err(), "unbounded cycle");

    // The same shape WITH a bound is a legitimate retry loop.
    let bounded = json!({
        "nodes": ["a", "b"],
        "edges": [
            {"src": "a", "dst": "b"},
            {"src": "b", "dst": "a", "max_cycles": 3},
        ],
    });
    sub.validate_plan(&bounded).expect("a bounded cycle is runnable");
}

/// V5: an edge condition that does not parse.
#[test]
fn an_unparsable_condition_is_refused() {
    let (_d, store) = open_temp();
    let sub = AreevSubstrate::new(store, None);
    let plan = json!({
        "nodes": ["a", "b"],
        "edges": [{"src": "a", "dst": "b", "cond": "ok ===== maybe"}],
    });
    assert!(sub.validate_plan(&plan).is_err());
}

/// The reason this reader is strict rather than the forward-compatible
/// `to_workflow`: a malformed edge must FAIL the check, never be skipped.
/// Skipping it would validate a plan that is not the plan under review — the
/// edge the proposal is *about* is exactly the one most likely to be
/// malformed.
#[test]
fn a_malformed_edge_fails_instead_of_being_skipped() {
    let (_d, store) = open_temp();
    let sub = AreevSubstrate::new(store, None);
    // Without the bad edge this graph is unreachable-node invalid; with the
    // edge dropped silently it would ALSO be invalid — so assert on the
    // message, which must name the edge, not the graph.
    for bad in [
        json!({"src": "a"}),                          // no dst
        json!({"dst": "b"}),                          // no src
        json!({"src": "a", "dst": "b", "cond": 7}),   // cond not a string
        json!({"src": "a", "dst": "b", "max_cycles": "many"}),
    ] {
        let plan = json!({"nodes": ["a", "b"], "edges": [bad.clone()]});
        let err = sub.validate_plan(&plan).unwrap_err().to_string();
        assert!(
            err.contains("edge"),
            "a malformed edge must be refused as an edge, not silently \
             dropped into a different graph — got {err} for {bad}"
        );
    }
}

#[test]
fn a_body_that_is_not_a_workflow_is_refused() {
    let (_d, store) = open_temp();
    let sub = AreevSubstrate::new(store, None);
    for body in [
        json!("nodes"),
        json!([1, 2]),
        json!({}),                                 // no nodes
        json!({"nodes": "fetch"}),                 // nodes not an array
        json!({"nodes": [1]}),                     // a node that is not a name
        json!({"nodes": ["a"], "edges": {}}),      // edges not an array
        json!({"nodes": ["a"], "bindings": []}),   // bindings not an object
        json!({"nodes": ["a"], "bindings": {"a": 3}}),
        json!({"nodes": ["a"], "retries": []}),
        json!({"nodes": ["a"], "retries": {"a": "lots"}}),
    ] {
        assert!(
            sub.validate_plan(&body).is_err(),
            "must refuse {body}"
        );
    }
}

// ── tool_evalset ───────────────────────────────────────────────────────

#[test]
fn the_pin_comes_from_the_tools_live_definition() {
    let (_d, mut store) = open_temp();
    store.add(&definition("screen", Some("beef"))).unwrap();
    let sub = AreevSubstrate::new(store, None);
    assert_eq!(sub.tool_evalset("screen").unwrap().as_deref(), Some("beef"));
}

#[test]
fn a_tool_that_declares_no_evalset_has_no_pin() {
    let (_d, mut store) = open_temp();
    store.add(&definition("triage", None)).unwrap();
    // An empty declaration is not a pin either — an empty string would
    // otherwise resolve to a gate nobody can run.
    store.add(&definition("release", Some("   "))).unwrap();
    let sub = AreevSubstrate::new(store, None);
    assert_eq!(sub.tool_evalset("triage").unwrap(), None);
    assert_eq!(sub.tool_evalset("release").unwrap(), None);
    assert_eq!(sub.tool_evalset("no_such_tool").unwrap(), None);
}

/// Execution records carry the same `tool_name`. Only the DEFINITION declares
/// the gate; reading a pin off a call record would let anything that invoked
/// the tool name its own grader.
#[test]
fn an_execution_record_never_supplies_the_pin() {
    let (_d, mut store) = open_temp();
    let mut call = Tool::new("screen")
        .content("matched")
        .is_error(false)
        .namespace("caller");
    call.common
        .extra_fields
        .insert("evalset_hash".into(), json!("attacker-chosen"));
    store.add(&call).unwrap();
    let sub = AreevSubstrate::new(store, None);
    assert_eq!(
        sub.tool_evalset("screen").unwrap(),
        None,
        "an execution grain is not a declaration"
    );
}

/// The pin follows the head. A superseded definition's pin is not the gate any
/// more — otherwise retiring an evalset would leave the old bar in force.
#[test]
fn superseding_the_definition_moves_the_pin() {
    let (_d, mut store) = open_temp();
    let first = store
        .add(&definition("screen", Some("aaaa")).created_at(NOW - 1_000))
        .unwrap();
    let mut newer = definition("screen", Some("bbbb")).created_at(NOW);
    store.supersede(&first, &mut newer).unwrap();
    let sub = AreevSubstrate::new(store, None);
    assert_eq!(sub.tool_evalset("screen").unwrap().as_deref(), Some("bbbb"));
}

/// Reads are authorization-gated like every other substrate read, and the
/// direction this fails in is the safe one: a session with no grant on the
/// namespace the definition lives in resolves NO pin, and the engine drops a
/// `code_revision` that has no pin (`tool_evalset(...).ok().flatten()?`) —
/// so a restricted session proposes nothing rather than proposing a code
/// change with a gate nobody checked.
#[test]
fn a_session_without_the_grant_resolves_no_pin() {
    let (_d, mut store) = open_temp();
    store.add(&definition("screen", Some("beef"))).unwrap();
    let facade = AreevFacade::with_session(store, Some("areev-loop".into()), None);
    facade.bind(AuthzSet::restricted(
        "agent:scoped",
        vec![Grant {
            verbs: vec![Verb::Read, Verb::LoopRun],
            namespaces: vec!["areev-loop".into()],
        }],
    ));
    let sub = BorrowedSubstrate::new(&facade);
    assert_eq!(
        sub.tool_evalset("screen").unwrap(),
        None,
        "an unreadable definition must never yield its pin"
    );
    // Asking for the namespace outright is still refused — the silent filter
    // above is the plural read's contract, not a hole in the grant check.
    assert!(sub
        .grains_of_type("tool", Some("caller"), ReadOpts::default())
        .is_err());
}

/// Both capabilities are declared, because both are implemented — the engine
/// gates `plan_revision`/`code_revision` proposals on exactly these flags, so
/// a substrate that implements one and declares neither silently drops the
/// proposal kind.
#[test]
fn the_adapter_declares_the_plan_and_code_capabilities() {
    let (_d, store) = open_temp();
    let sub = AreevSubstrate::new(store, None);
    let caps = sub.capabilities();
    assert!(caps.plans, "plans");
    assert!(caps.code, "code");
    // And the reads they gate really work on this substrate.
    assert!(sub
        .grains_of_type("tool", None, ReadOpts::default())
        .is_ok());
}
