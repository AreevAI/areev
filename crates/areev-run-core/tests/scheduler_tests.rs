//! The scheduler under a simulated driver — the seed of the DST harness
//! (governed-agents §8/§9). A mini-driver executes commands with a
//! deterministic mock executor and an **adversarial completion-order
//! permutation** knob: the scheduler-permutation gate in miniature. No
//! wall-clock anywhere: the sim owns a virtual clock (hand-rolled xorshift
//! for order shuffling — no rand crate enters even the dev tree).

use areev_run_core::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Deterministic per-seed order shuffle (xorshift64*).
fn permute<T>(items: &mut [T], seed: u64) {
    let mut s = seed.wrapping_mul(2685821657736338717).max(1);
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for i in (1..items.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

/// What the mock executor should do for one (node, attempt).
type Behavior<'a> = &'a dyn Fn(&JournalKey, &Value) -> EffectOutcome;

fn ok(result: Value) -> EffectOutcome {
    EffectOutcome::Completed {
        result,
        journal_bytes: 100,
        input_tokens: 0,
        output_tokens: 0,
        usd_micros: 0,
    }
}

/// Run a plan to a terminal state. Returns (final state, every command, the
/// envelopes seen). `respond` maps parked ask ids to outcomes when the sim
/// reaches a park (simulating a human).
struct Sim<'a> {
    env: StepEnv<'a>,
    behavior: Behavior<'a>,
    seed: u64,
    clock: u64,
    respond: BTreeMap<String, EffectOutcome>,
}

struct SimResult {
    state: SchedulerState,
    commands: Vec<Command>,
    checkpoints: Vec<DecisionRecord>,
}

impl Sim<'_> {
    fn run(mut self) -> SimResult {
        let mut st = SchedulerState::new("run-1", self.env.plan);
        let mut all: Vec<Command> = Vec::new();
        let mut checkpoints = Vec::new();
        let mut events = vec![
            EventIn::ClockReading { unix_ms: self.clock },
            EventIn::Start { input: json!({"seed_input": true}) },
        ];
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 500, "sim did not terminate");
            let out = step(&self.env, st, &events);
            st = out.state;
            let mut dispatches: Vec<(JournalKey, Value)> = Vec::new();
            let mut asks: Vec<Ask> = Vec::new();
            for c in &out.commands {
                match c {
                    Command::Dispatch { key, input, .. } => {
                        dispatches.push((key.clone(), input.clone()))
                    }
                    Command::EmitEnvelope { asks: a } => asks.extend(a.clone()),
                    Command::WriteCheckpoint { decision_record, .. } => {
                        checkpoints.push(decision_record.clone())
                    }
                    _ => {}
                }
            }
            all.extend(out.commands);
            if st.is_terminal() {
                return SimResult { state: st, commands: all, checkpoints };
            }
            // Build the next event batch: resolve every dispatch in a
            // permuted order (the adversarial scheduler), then answer any
            // parked asks the test chose to answer.
            self.clock += 10;
            events = vec![EventIn::ClockReading { unix_ms: self.clock }];
            permute(&mut dispatches, self.seed.wrapping_add(guard));
            for (key, input) in dispatches {
                let outcome = (self.behavior)(&key, &input);
                events.push(EventIn::EffectResolved { key, outcome });
            }
            for ask in asks {
                if let Some(outcome) = self.respond.remove(&ask.tool_call_id) {
                    // A human takes three days.
                    self.clock += 3 * 24 * 3600 * 1000;
                    events.push(EventIn::ClockReading { unix_ms: self.clock });
                    events.push(EventIn::ResponseSettled {
                        tool_call_id: ask.tool_call_id,
                        outcome,
                    });
                }
            }
            if events.len() == 1 && !st.is_terminal() {
                // Nothing resolvable: a parked run with no responder.
                return SimResult { state: st, commands: all, checkpoints };
            }
        }
    }
}

fn host_execs(plan: &PlanGraph) -> Vec<NodeExecutor> {
    plan.nodes
        .iter()
        .map(|n| NodeExecutor::Host { tool_hash: "deadbeef".into(), tool_name: n.clone() })
        .collect()
}

fn lww(_k: &str, _prev: Option<&Value>, new: &Value) -> Value {
    new.clone()
}

fn builtin_eval(edge: &PlanEdge, state: &Value) -> bool {
    edge.cond.as_ref().map(|c| cond::eval(c, state)).unwrap_or(true)
}

fn args_ok(_tool: &str, _args: &Value) -> std::result::Result<(), String> {
    Ok(())
}

fn env<'a>(plan: &'a PlanGraph, execs: &'a [NodeExecutor], budgets: Budgets) -> StepEnv<'a> {
    StepEnv {
        plan,
        executors: execs,
        budgets,
        reduce: &lww,
        eval_cond: &builtin_eval,
        ask_ttl_sec: None,
        validate_args: &args_ok,
        llm_reserve_tokens: 1024,
        max_effects_per_attempt: areev_run_core::DEFAULT_MAX_EFFECTS_PER_ATTEMPT,
        llm_tool_result_chars: None,
        llm_context_tokens: None,
        decide: None,
    }
}

fn wf(nodes: &[&str]) -> areev_core::types::Workflow {
    areev_core::types::Workflow::new(nodes.iter().map(|s| s.to_string()).collect())
}

#[test]
fn linear_pipeline_completes_with_intents_before_dispatches() {
    let plan = PlanGraph::build(&wf(&["a", "b", "c"]).edge("a", "b").edge("b", "c")).unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, _in: &Value| ok(json!({key.node.clone(): "done"}));
    let r = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 1,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();

    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    assert_eq!(r.state.spent.supersteps, 3, "one node per superstep in a chain");
    // Every key accumulated through the reducers.
    for n in ["a", "b", "c"] {
        assert_eq!(r.state.context[n], json!("done"));
    }
    // Protocol order: each node's WriteIntent precedes its Dispatch.
    let mut intent_seen = std::collections::BTreeSet::new();
    for c in &r.commands {
        match c {
            Command::WriteIntent { key, .. } => {
                intent_seen.insert(key.clone());
            }
            Command::Dispatch { key, .. } => {
                assert!(intent_seen.contains(key), "dispatch before intent for {key:?}");
            }
            _ => {}
        }
    }
    assert_eq!(r.checkpoints.len(), 3, "one checkpoint per superstep");
}

#[test]
fn diamond_with_untaken_branch_completes_via_dead_path() {
    // a fans out to b (cond false) and c; d is the AND-join.
    let w = wf(&["a", "b", "c", "d"])
        .cond_edge("a", "b", "take_b == true")
        .edge("a", "c")
        .edge("b", "d")
        .edge("c", "d");
    let plan = PlanGraph::build(&w).unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, _in: &Value| ok(json!({key.node.clone(): true}));
    let r = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 7,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();

    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    // b never ran; the join resolved through its dead arm.
    assert_eq!(r.state.context.get("b"), None, "the untaken branch must not run");
    assert_eq!(r.state.context["d"], json!(true));
    let dead_paths = r
        .checkpoints
        .iter()
        .flat_map(|c| &c.edges)
        .filter(|(_, _, o)| *o == EdgeOutcome::DeadPath)
        .count();
    assert!(dead_paths >= 1, "the b->d arm resolves as a dead path");
}

#[test]
fn all_conditions_false_stalls_naming_the_decision_node() {
    let w = wf(&["decide", "x", "y"])
        .cond_edge("decide", "x", "go == \"x\"")
        .cond_edge("decide", "y", "go == \"y\"");
    let plan = PlanGraph::build(&w).unwrap();
    let execs = host_execs(&plan);
    let behavior = |_k: &JournalKey, _in: &Value| ok(json!({"go": "neither"}));
    let r = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 3,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();
    assert_eq!(
        r.state.outcome(),
        Some(&RunOutcome::Stalled { node: "decide".into() }),
        "stall must NAME the all-conditions-false node — a distinct, \
         diagnosable outcome, never a silent success"
    );
}

#[test]
fn bounded_self_loop_iterates_exactly_max_cycles_plus_one_runs() {
    let mut w = wf(&["work", "done"]).edge("work", "done");
    w.edges.push(areev_core::types::WorkflowEdge {
        src: "work".into(),
        dst: "work".into(),
        cond: None,
        max_cycles: Some(2),
    });
    let plan = PlanGraph::build(&w).unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, input: &Value| {
        // Only the loop body counts; `done` re-runs per iteration too (an
        // edge firing into a completed node re-enters it) and must not
        // pollute the counter.
        if key.node == "work" {
            let count = input.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
            ok(json!({"count": count + 1, "work": true}))
        } else {
            ok(json!({key.node.clone(): true}))
        }
    };
    let r = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 5,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();

    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    // Initial run + 2 re-entries: the loop body observed count 0, 1, 2.
    assert_eq!(r.state.context["count"], json!(3), "work ran exactly 3 times");
    let exhausted = r
        .checkpoints
        .iter()
        .flat_map(|c| &c.edges)
        .filter(|(_, _, o)| *o == EdgeOutcome::CycleExhausted)
        .count();
    assert_eq!(exhausted, 1, "the back-edge reports exhaustion exactly once");
}

#[test]
fn cycle_whose_back_edge_targets_a_non_entry_node_parks_instead_of_stalling() {
    // GitHub issue #33's exact repro shape: a -> g, g -> c (cond +
    // max_cycles), c -> g — the back-edge closes the cycle on `g`, NOT on
    // the plan's entry `a`. Before the fix, `refresh_readiness` required
    // the not-yet-resolvable `c -> g` edge before `g` could ever go Ready,
    // so the run stalled naming "a" on superstep 1 — even though `a`'s own
    // edge fired — and `g` was never dispatched at all.
    let mut w = wf(&["a", "g", "c"])
        .edge("a", "g")
        .cond_edge("g", "c", "decision == \"question\"");
    w.edges[1].max_cycles = Some(6);
    w.edges.push(areev_core::types::WorkflowEdge {
        src: "c".into(),
        dst: "g".into(),
        cond: None,
        max_cycles: None,
    });
    let plan = PlanGraph::build(&w).unwrap();
    let mut execs = host_execs(&plan);
    execs[1] = NodeExecutor::Client { tool_hash: "cafe".into(), tool_name: "g".into(), approval: true };
    let behavior = |key: &JournalKey, _in: &Value| ok(json!({key.node.clone(): true}));

    let r = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 9,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();

    assert!(!r.state.is_terminal(), "must park on g's ask, not finish stalled");
    assert_eq!(r.state.pending_asks.len(), 1);
    let ask_id = r.state.pending_asks.keys().next().unwrap().clone();
    // The ask id is the journal key digest for g's first attempt — proof
    // the park landed on g, not on a stall diagnosis naming a.
    let expect_key = JournalKey {
        run_id: "run-1".into(),
        task_path: "".into(),
        node: "g".into(),
        attempt: 1,
        effect_seq: 0,
        kind: EffectKind::Tool,
    };
    assert_eq!(ask_id, expect_key.tool_call_id(), "parked on g's ask, not stalled at a");
}

#[test]
fn all_host_cycle_whose_back_edge_targets_a_non_entry_node_iterates_correctly() {
    // Same shape, no Client node (the issue's "not the client node" check)
    // — driven far enough to prove `g` and `c` both re-enter correctly
    // across generations, not just that the very first dispatch succeeds.
    let mut w = wf(&["a", "g", "c"])
        .edge("a", "g")
        .cond_edge("g", "c", "decision == \"question\"");
    w.edges[1].max_cycles = Some(2);
    w.edges.push(areev_core::types::WorkflowEdge {
        src: "c".into(),
        dst: "g".into(),
        cond: None,
        max_cycles: None,
    });
    let plan = PlanGraph::build(&w).unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, input: &Value| match key.node.as_str() {
        "g" => {
            let runs = input.get("g_runs").and_then(|v| v.as_i64()).unwrap_or(0);
            ok(json!({"decision": "question", "g_runs": runs + 1}))
        }
        "c" => {
            let runs = input.get("c_runs").and_then(|v| v.as_i64()).unwrap_or(0);
            ok(json!({"c_runs": runs + 1}))
        }
        _ => ok(json!({key.node.clone(): true})),
    };

    let r = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 13,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();

    // g dispatches once per generation (0, 1, 2 — the third g->c attempt
    // exhausts max_cycles=2); c runs once per successfully fired loop edge.
    assert_eq!(r.state.context["g_runs"], json!(3), "g ran across three generations");
    assert_eq!(r.state.context["c_runs"], json!(2), "c ran once per fired loop edge");
    let exhausted = r
        .checkpoints
        .iter()
        .flat_map(|c| &c.edges)
        .filter(|(_, _, o)| *o == EdgeOutcome::CycleExhausted)
        .count();
    assert_eq!(exhausted, 1, "the bounded edge reports exhaustion exactly once");
}

#[test]
fn retryable_failures_retry_within_budget_then_succeed() {
    let w = wf(&["flaky", "next"]).edge("flaky", "next");
    let mut w = w;
    w.retries.insert("flaky".into(), 2);
    let plan = PlanGraph::build(&w).unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, _in: &Value| {
        if key.node == "flaky" && key.attempt < 3 {
            EffectOutcome::Failed {
                cause: FailCause::Timeout,
                detail: "upstream 504".into(),
                journal_bytes: 10,
            }
        } else {
            ok(json!({key.node.clone(): key.attempt}))
        }
    };
    let r = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 11,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();
    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    assert_eq!(r.state.context["flaky"], json!(3), "succeeded on attempt 3");
    // Three intents journaled for flaky — each attempt its own record.
    let flaky_intents = r
        .commands
        .iter()
        .filter(|c| matches!(c, Command::WriteIntent { key, .. } if key.node == "flaky"))
        .count();
    assert_eq!(flaky_intents, 3);
}

#[test]
fn retry_exhaustion_fails_fast_naming_the_lowest_failed_node() {
    // Two parallel nodes fail permanently; under every permutation the run
    // names the LOWEST-index one (chosen at close, not at arrival).
    let w = wf(&["root", "b_fail", "a_fail"]) // note: b before a in declaration
        .edge("root", "b_fail")
        .edge("root", "a_fail");
    let plan = PlanGraph::build(&w).unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, _in: &Value| {
        if key.node.ends_with("_fail") {
            EffectOutcome::Failed {
                cause: FailCause::UserAborted,
                detail: "no".into(),
                journal_bytes: 1,
            }
        } else {
            ok(json!({"root": true}))
        }
    };
    let mut outcomes = std::collections::BTreeSet::new();
    for seed in [1u64, 2, 3, 4, 5, 6, 7, 8] {
        let r = Sim {
            env: env(&plan, &execs, Budgets::default()),
            behavior: &behavior,
            seed,
            clock: 1_000,
            respond: BTreeMap::new(),
        }
        .run();
        match r.state.outcome() {
            Some(RunOutcome::Failed { node, .. }) => {
                outcomes.insert(node.clone());
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
    assert_eq!(
        outcomes.into_iter().collect::<Vec<_>>(),
        vec!["b_fail".to_string()],
        "every permutation names node index 1 (b_fail) — deterministic"
    );
}

/// The scheduler-permutation gate in miniature: adversarial completion
/// orders produce byte-identical final state and checkpoints.
#[test]
fn permutation_determinism_over_a_parallel_fan_out() {
    let w = wf(&["a", "p1", "p2", "p3", "join"])
        .edge("a", "p1")
        .edge("a", "p2")
        .edge("a", "p3")
        .edge("p1", "join")
        .edge("p2", "join")
        .edge("p3", "join");
    let plan = PlanGraph::build(&w).unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, _in: &Value| ok(json!({key.node.clone(): key.attempt}));

    let mut states: Vec<String> = Vec::new();
    let mut records: Vec<String> = Vec::new();
    for seed in [1u64, 99, 424242] {
        let r = Sim {
            env: env(&plan, &execs, Budgets::default()),
            behavior: &behavior,
            seed,
            clock: 1_000,
            respond: BTreeMap::new(),
        }
        .run();
        states.push(serde_json::to_string(&r.state).unwrap());
        records.push(serde_json::to_string(&r.checkpoints).unwrap());
    }
    assert_eq!(states[0], states[1]);
    assert_eq!(states[1], states[2], "final state is schedule-independent");
    assert_eq!(records[0], records[1]);
    assert_eq!(records[1], records[2], "decision records are schedule-independent");
}

#[test]
fn client_ask_parks_once_resumes_by_id_and_never_charges_the_wait() {
    let w = wf(&["auto", "approve", "ship"])
        .edge("auto", "approve")
        .edge("approve", "ship");
    let plan = PlanGraph::build(&w).unwrap();
    let mut execs = host_execs(&plan);
    execs[1] = NodeExecutor::Client { tool_hash: "cafe".into(), tool_name: "approve".into(), approval: true };

    let behavior = |key: &JournalKey, _in: &Value| ok(json!({key.node.clone(): true}));

    // First: run WITHOUT a responder — the run parks, and polling again
    // must not re-announce the envelope.
    let r = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 2,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();
    assert!(!r.state.is_terminal());
    assert_eq!(r.state.pending_asks.len(), 1);
    let envelopes = r
        .commands
        .iter()
        .filter(|c| matches!(c, Command::EmitEnvelope { .. }))
        .count();
    assert_eq!(envelopes, 1, "one park, one envelope — never re-announced");
    let ask_id = r.state.pending_asks.keys().next().unwrap().clone();
    // The ask id is the journal key digest — reproducible.
    let expect_key = JournalKey {
        run_id: "run-1".into(),
        task_path: "".into(),
        node: "approve".into(),
        attempt: 1,
        effect_seq: 0,
        kind: EffectKind::Tool,
    };
    assert_eq!(ask_id, expect_key.tool_call_id());

    // Second: with a responder that takes three days.
    let mut respond = BTreeMap::new();
    respond.insert(ask_id, ok(json!({"approve": "granted"})));
    let r2 = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 2,
        clock: 1_000,
        respond,
    }
    .run();
    assert_eq!(r2.state.outcome(), Some(&RunOutcome::Completed));
    assert_eq!(r2.state.context["approve"], json!("granted"));
    // Wall charged active segments only; the three-day wait is elapsed.
    let three_days = 3 * 24 * 3600 * 1000;
    assert!(
        r2.state.spent.wall_ms < 1_000,
        "wall must not include the park: {}",
        r2.state.spent.wall_ms
    );
    assert!(
        r2.state.elapsed_ms >= three_days,
        "the wait is reported as elapsed: {}",
        r2.state.elapsed_ms
    );
}

/// The wall/elapsed rule, extended to the gap a CRASH leaves. A driver that
/// dies between supersteps and comes back later must charge the dead span to
/// `elapsed`, never to `wall` — the same treatment a park gets, and the reason
/// `verify` can reproduce a recovered run at all.
///
/// Pure-level, because the driver-level twin (`runner_tests`) needs real
/// crash injection and cannot isolate the arithmetic.
#[test]
fn a_resume_boundary_accrues_elapsed_and_never_wall() {
    let plan = PlanGraph::build(&wf(&["a", "b"]).edge("a", "b")).unwrap();
    let execs = host_execs(&plan);
    let e = env(&plan, &execs, Budgets::default());
    let key = |node: &str| JournalKey {
        run_id: "run-1".into(),
        task_path: String::new(),
        node: node.into(),
        attempt: 1,
        effect_seq: 0,
        kind: EffectKind::Tool,
    };

    // Superstep 1 opens at 1_000 and closes at 1_010.
    let mut st = SchedulerState::new("run-1", &plan);
    let out = step(&e, st, &[
        EventIn::ClockReading { unix_ms: 1_000 },
        EventIn::Start { input: json!({}) },
    ]);
    st = out.state;
    let out = step(&e, st, &[
        EventIn::ClockReading { unix_ms: 1_010 },
        EventIn::EffectResolved { key: key("a"), outcome: ok(json!({"a": true})) },
    ]);
    st = out.state;
    // Without a Resumed marker the next superstep opened at the close, so no
    // time is unaccounted yet.
    assert_eq!(st.elapsed_ms, 0);
    let wall_before = st.spent.wall_ms;

    // Now the crash: rewind to that closed state and re-enter it the way a
    // driver picking the run back up does — 5 minutes later.
    let closed = st_at_close(&e, &plan, &key);
    let gap_ms = 5 * 60 * 1_000;
    let out = step(&e, closed, &[
        EventIn::Resumed,
        EventIn::ClockReading { unix_ms: 1_010 + gap_ms },
    ]);
    let st = out.state;

    assert_eq!(st.elapsed_ms, gap_ms, "the dead span is REPORTED as elapsed");
    assert_eq!(
        st.spent.wall_ms, wall_before,
        "…and never billed as wall: a crashed run is not a working run"
    );
    match &st.phase {
        Phase::Open { record, .. } => assert_eq!(
            record.resumed_at,
            Some(1_010 + gap_ms),
            "the reading is journaled, or verify could not reproduce this"
        ),
        other => panic!("expected an open superstep, got {other:?}"),
    }
}

/// The state at superstep 1's close — the shape a checkpoint stores.
fn st_at_close(
    e: &StepEnv<'_>,
    plan: &PlanGraph,
    key: &dyn Fn(&str) -> JournalKey,
) -> SchedulerState {
    let mut st = SchedulerState::new("run-1", plan);
    let out = step(e, st, &[
        EventIn::ClockReading { unix_ms: 1_000 },
        EventIn::Start { input: json!({}) },
    ]);
    st = out.state;
    // Close superstep 1 WITHOUT letting it roll straight into the next open:
    // impossible through `step`, so take the checkpoint the close commanded,
    // which is exactly what a driver persists and a resume reloads.
    let out = step(e, st, &[
        EventIn::ClockReading { unix_ms: 1_010 },
        EventIn::EffectResolved { key: key("a"), outcome: ok(json!({"a": true})) },
    ]);
    for c in out.commands {
        if let Command::WriteCheckpoint { state_json, .. } = c {
            return serde_json::from_value(state_json).unwrap();
        }
    }
    panic!("superstep 1 wrote no checkpoint");
}

#[test]
fn cancel_drains_without_new_dispatches() {
    let plan =
        PlanGraph::build(&wf(&["a", "b", "c"]).edge("a", "b").edge("b", "c")).unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, _in: &Value| ok(json!({key.node.clone(): true}));

    // Drive manually: cancel arrives while superstep 1 is in flight.
    let e = env(&plan, &execs, Budgets::default());
    let st = SchedulerState::new("run-1", &plan);
    let out = step(
        &e,
        st,
        &[
            EventIn::ClockReading { unix_ms: 1_000 },
            EventIn::Start { input: json!({}) },
        ],
    );
    let dispatched: Vec<JournalKey> = out
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::Dispatch { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(dispatched.len(), 1);

    let mut events = vec![
        EventIn::ClockReading { unix_ms: 2_000 },
        EventIn::CancelSeen { principal: "user:ops".into(), reason: "abort".into() },
    ];
    for key in dispatched {
        events.push(EventIn::EffectResolved {
            key: key.clone(),
            outcome: behavior(&key, &json!({})),
        });
    }
    let out2 = step(&e, out.state, &events);
    assert_eq!(
        out2.state.outcome(),
        Some(&RunOutcome::Canceled { by: "user:ops".into(), reason: "abort".into() })
    );
    assert!(
        !out2.commands.iter().any(|c| matches!(c, Command::Dispatch { .. })),
        "a canceled run must not dispatch new work"
    );
    // The terminal checkpoint's clock_close_ms reflects the fresh reading
    // this batch carried alongside CancelSeen — the shape a driver MUST
    // produce (doc comment on `step()`: "any call that may open or close a
    // superstep includes a fresh ClockReading"). This is the positive half
    // of the contract; `cancel_without_a_fresh_reading_journals_a_stale_close`
    // below pins what happens when a driver skips it.
    let close_ms = out2
        .commands
        .iter()
        .find_map(|c| match c {
            Command::WriteCheckpoint { decision_record, .. } => Some(decision_record.clock_close_ms),
            _ => None,
        })
        .expect("cancellation writes a terminal checkpoint");
    assert_eq!(close_ms, 2_000, "clock_close_ms must take the batch's fresh reading");
}

/// The failure mode a real driver can produce by skipping the reading: if
/// `CancelSeen` rides a batch with NO accompanying `ClockReading`, the
/// terminal checkpoint's `clock_close_ms` stays at whatever the LAST
/// reading was — even if that reading predates the moment the cancel was
/// actually discovered. `step()` has no way to notice this on its own (it
/// only ever sees what a caller hands it); the guarantee has to come from
/// every driver honoring the doc comment on `step()`. `areev-run`'s live
/// driver used to violate exactly this (the kill-switch drill's
/// `close_ms >= cancel_ms` assertion caught it under real concurrency);
/// this is the deterministic, non-racy pin of the underlying mechanism.
#[test]
fn cancel_without_a_fresh_reading_journals_a_stale_close() {
    let plan = PlanGraph::build(&wf(&["a", "b", "c"]).edge("a", "b").edge("b", "c")).unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, _in: &Value| ok(json!({key.node.clone(): true}));

    let e = env(&plan, &execs, Budgets::default());
    let st = SchedulerState::new("run-1", &plan);
    let out = step(
        &e,
        st,
        &[
            EventIn::ClockReading { unix_ms: 1_000 },
            EventIn::Start { input: json!({}) },
        ],
    );
    let dispatched: Vec<JournalKey> = out
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::Dispatch { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();

    // Same scenario as `cancel_drains_without_new_dispatches`, EXCEPT this
    // batch carries no ClockReading before CancelSeen — the shape the live
    // driver used to produce.
    let mut events = vec![EventIn::CancelSeen { principal: "user:ops".into(), reason: "abort".into() }];
    for key in dispatched {
        events.push(EventIn::EffectResolved { key: key.clone(), outcome: behavior(&key, &json!({})) });
    }
    let out2 = step(&e, out.state, &events);
    assert_eq!(
        out2.state.outcome(),
        Some(&RunOutcome::Canceled { by: "user:ops".into(), reason: "abort".into() })
    );
    let close_ms = out2
        .commands
        .iter()
        .find_map(|c| match c {
            Command::WriteCheckpoint { decision_record, .. } => Some(decision_record.clock_close_ms),
            _ => None,
        })
        .expect("cancellation writes a terminal checkpoint");
    assert_eq!(
        close_ms, 1_000,
        "without a fresh reading, clock_close_ms is stuck at the superstep's OPEN — \
         exactly the staleness a driver skipping the reading would journal"
    );
}

/// #344: a pause in the batch that closes a superstep stops the NEXT one from
/// opening — and changes nothing else. The state it leaves is byte-identical
/// to the close checkpoint an uninterrupted run writes, and continuing is an
/// ordinary resume boundary that bills the paused span as elapsed, not wall.
#[test]
fn a_pause_holds_at_the_boundary_and_leaves_the_uninterrupted_checkpoint() {
    let plan =
        PlanGraph::build(&wf(&["a", "b", "c"]).edge("a", "b").edge("b", "c")).unwrap();
    let execs = host_execs(&plan);
    let e = env(&plan, &execs, Budgets::default());
    let dispatches = |cmds: &[Command]| -> Vec<JournalKey> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Dispatch { key, .. } => Some(key.clone()),
                _ => None,
            })
            .collect()
    };
    let close_state = |cmds: &[Command]| -> Value {
        cmds.iter()
            .find_map(|c| match c {
                Command::WriteCheckpoint { state_json, .. } => Some(state_json.clone()),
                _ => None,
            })
            .expect("the close checkpoints")
    };

    let out = step(
        &e,
        SchedulerState::new("run-1", &plan),
        &[EventIn::ClockReading { unix_ms: 1_000 }, EventIn::Start { input: json!({}) }],
    );
    let first = dispatches(&out.commands);
    assert_eq!(first.len(), 1);
    let resolve = |mut events: Vec<EventIn>| {
        for key in &first {
            events.push(EventIn::EffectResolved {
                key: key.clone(),
                outcome: ok(json!({key.node.clone(): true})),
            });
        }
        events
    };

    // Uninterrupted: close superstep 1 and open 2 (dispatching b) in one call.
    let straight = step(&e, out.state.clone(), &resolve(vec![EventIn::ClockReading { unix_ms: 2_000 }]));
    assert_eq!(dispatches(&straight.commands).len(), 1);

    // Paused: the same close, and nothing opened.
    let mut batch = resolve(vec![EventIn::ClockReading { unix_ms: 2_000 }]);
    batch.push(EventIn::PauseRequested);
    let held = step(&e, out.state, &batch);
    assert!(dispatches(&held.commands).is_empty(), "a paused run opens nothing");
    assert!(matches!(held.state.phase, Phase::Idle));
    assert!(!held.state.is_terminal());
    assert_eq!(
        close_state(&held.commands),
        close_state(&straight.commands),
        "the paused checkpoint is the uninterrupted one, byte for byte"
    );
    assert_eq!(
        serde_json::to_value(&held.state).unwrap(),
        close_state(&held.commands),
        "nothing about the pause lives in state"
    );

    // Still requested on the next call: still held (the driver re-feeds it).
    let again = step(&e, held.state.clone(), &[EventIn::PauseRequested]);
    assert!(again.commands.is_empty());

    // Resume ten seconds later: b opens, the gap is elapsed, never wall.
    let resumed = step(
        &e,
        held.state,
        &[EventIn::Resumed, EventIn::ClockReading { unix_ms: 12_000 }],
    );
    assert_eq!(dispatches(&resumed.commands).len(), 1);
    assert_eq!(resumed.state.elapsed_ms, 10_000);
    assert_eq!(resumed.state.spent.wall_ms, 1_000, "only superstep 1's active span");

    // Terminal outcomes still win: a run with nothing left finishes.
    let last = PlanGraph::build(&wf(&["a"])).unwrap();
    let last_execs = host_execs(&last);
    let le = env(&last, &last_execs, Budgets::default());
    let o = step(
        &le,
        SchedulerState::new("run-2", &last),
        &[EventIn::ClockReading { unix_ms: 1_000 }, EventIn::Start { input: json!({}) }],
    );
    let k = dispatches(&o.commands).remove(0);
    let done = step(
        &le,
        o.state,
        &[
            EventIn::ClockReading { unix_ms: 2_000 },
            EventIn::EffectResolved { key: k, outcome: ok(json!({"a": true})) },
            EventIn::PauseRequested,
        ],
    );
    assert_eq!(done.state.outcome(), Some(&RunOutcome::Completed));
}

#[test]
fn superstep_budget_exhausts_resumably() {
    let plan = PlanGraph::build(
        &wf(&["a", "b", "c", "d"]).edge("a", "b").edge("b", "c").edge("c", "d"),
    )
    .unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, _in: &Value| ok(json!({key.node.clone(): true}));
    let r = Sim {
        env: env(
            &plan,
            &execs,
            Budgets { max_supersteps: 2, ..Budgets::default() },
        ),
        behavior: &behavior,
        seed: 1,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();
    assert_eq!(
        r.state.outcome(),
        Some(&RunOutcome::BudgetExhausted { axis: BudgetAxis::Supersteps })
    );
    // The two completed supersteps' work survives in state — resumable.
    assert_eq!(r.state.context["b"], json!(true));
}

/// Checkpoint fidelity: mid-run state round-trips serde byte-identically —
/// what the replay-equivalence gate compares.
#[test]
fn scheduler_state_serde_round_trips_mid_run() {
    let w = wf(&["auto", "approve"]).edge("auto", "approve");
    let plan = PlanGraph::build(&w).unwrap();
    let mut execs = host_execs(&plan);
    execs[1] = NodeExecutor::Client { tool_hash: "cafe".into(), tool_name: "approve".into(), approval: true };
    let behavior = |key: &JournalKey, _in: &Value| ok(json!({key.node.clone(): true}));
    let r = Sim {
        env: env(&plan, &execs, Budgets::default()),
        behavior: &behavior,
        seed: 4,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run();
    assert!(!r.state.is_terminal(), "parked mid-run");
    let bytes = serde_json::to_string(&r.state).unwrap();
    let back: SchedulerState = serde_json::from_str(&bytes).unwrap();
    assert_eq!(back, r.state);
    assert_eq!(serde_json::to_string(&back).unwrap(), bytes, "byte-stable");
}

/// The §5.6 gate-3 requirement, grown to Sends: adversarial completion
/// orders over a map-reduce fan-out must yield identical final state,
/// identical decision records (spawns included), and journal keys that are
/// unique per occurrence — the property `tool_call_id`-as-key-digest exists
/// for.
#[test]
fn send_fan_out_is_permutation_invariant_with_unique_keys() {
    let plan =
        PlanGraph::build(&wf(&["seed", "worker", "collect"]).edge("seed", "worker").edge("worker", "collect"))
            .unwrap();
    let execs = host_execs(&plan);
    let behavior = |key: &JournalKey, input: &Value| match key.node.as_str() {
        "seed" => ok(json!({
            "seeded": true,
            "$send": [
                {"node": "worker", "input": {"v": 1}},
                {"node": "worker", "input": {"v": 2}},
                {"node": "worker", "input": {"v": 3}},
                {"node": "worker", "input": {"v": 4}},
            ],
        })),
        "worker" => {
            let v = input["v"].as_u64().unwrap();
            ok(json!({ format!("out{v}"): v * 10 }))
        }
        _ => ok(json!({"collected": true})),
    };

    let mut reference: Option<(Value, Vec<DecisionRecord>)> = None;
    for seed in 1..=8u64 {
        let r = Sim {
            env: env(&plan, &execs, Budgets::default()),
            behavior: &behavior,
            seed,
            clock: 1_000,
            respond: BTreeMap::new(),
        }
        .run();
        assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed), "seed {seed}");
        for v in [1u64, 2, 3, 4] {
            assert_eq!(r.state.context[format!("out{v}")], json!(v * 10), "seed {seed}");
        }
        assert_eq!(r.state.context["collected"], json!(true), "collect ran after the join");

        // Key uniqueness across the whole run, Sends included.
        let keys: Vec<String> = r
            .commands
            .iter()
            .filter_map(|c| match c {
                Command::WriteIntent { key, .. } => Some(key.tool_call_id()),
                _ => None,
            })
            .collect();
        let unique: std::collections::BTreeSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "seed {seed}: duplicate journal key");
        assert_eq!(keys.len(), 6, "seed {seed}: seed + 4 tasks + collect");

        // The spawn decision is journaled identically regardless of order.
        let spawns: Vec<&(String, usize)> =
            r.checkpoints.iter().flat_map(|d| &d.spawns).collect();
        assert_eq!(spawns.len(), 4, "seed {seed}");
        assert_eq!(spawns[0].0, "/0000");

        match &reference {
            None => reference = Some((r.state.context.clone(), r.checkpoints.clone())),
            Some((ctx, records)) => {
                assert_eq!(&r.state.context, ctx, "seed {seed}: state diverged");
                assert_eq!(&r.checkpoints, records, "seed {seed}: decisions diverged");
            }
        }
    }
}

// ---- abstract nodes: the name the model was OFFERED (#251) -----------------

/// The tools an abstract node offers, by their canonical (Definition) names.
fn abstract_exec(names: &[&str]) -> Vec<NodeExecutor> {
    vec![NodeExecutor::Abstract {
        tools: names
            .iter()
            .map(|n| OfferedTool { tool_name: (*n).into(), tool_hash: "deadbeef".into() })
            .collect(),
    }]
}

/// A fake provider: turn 0 calls `called`, every later turn ends the loop.
/// It echoes back a name the way a real provider does — whatever the tools
/// array showed it — so what the test varies is exactly the spelling.
fn echoing_provider(called: &str) -> impl Fn(&JournalKey, &Value) -> EffectOutcome + '_ {
    move |key: &JournalKey, _in: &Value| match (key.kind, key.effect_seq) {
        (EffectKind::Llm, 0) => ok(json!({
            "text": Value::Null,
            "tool_calls": [{"id": "call_0", "name": called, "arguments": {}}],
            "stop_reason": "tool_use",
        })),
        (EffectKind::Llm, _) => ok(json!({
            "text": "{\"filed\": true}",
            "tool_calls": [],
            "stop_reason": "end_turn",
        })),
        (EffectKind::Tool, _) => ok(json!({"receipt": "r-1"})),
    }
}

/// Every host tool the run actually dispatched, in command order.
fn dispatched_tools(cmds: &[Command]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::Dispatch { executor: NodeExecutor::Host { tool_name, .. }, .. } => {
                Some(tool_name.clone())
            }
            _ => None,
        })
        .collect()
}

fn run_abstract(
    plan: &PlanGraph,
    execs: &[NodeExecutor],
    behavior: Behavior<'_>,
) -> SimResult {
    Sim {
        env: env(plan, execs, Budgets::default()),
        behavior,
        seed: 1,
        clock: 1_000,
        respond: BTreeMap::new(),
    }
    .run()
}

#[test]
fn a_dotted_definition_is_callable_by_the_normalized_name_it_was_offered_as() {
    // Anthropic/OpenAI forbid dots, so `receipt.prepare` reaches the model as
    // `receipt_prepare` and the model calls that back. Before #251 the node
    // failed after one re-prompt it could not possibly satisfy.
    let plan = PlanGraph::build(&wf(&["decide"])).unwrap();
    let execs = abstract_exec(&["receipt.prepare"]);
    let behavior = echoing_provider("receipt_prepare");
    let r = run_abstract(&plan, &execs, &behavior);

    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    assert_eq!(
        dispatched_tools(&r.commands),
        vec!["receipt.prepare".to_string()],
        "the call must dispatch the DEFINITION, under its canonical name"
    );
    assert_eq!(r.state.context["filed"], json!(true));
}

#[test]
fn the_canonical_name_still_works_when_the_model_uses_it() {
    // A provider that leaves dots alone (or a model quoting the plan) is not
    // punished for it: exact match is tried first.
    let plan = PlanGraph::build(&wf(&["decide"])).unwrap();
    let execs = abstract_exec(&["receipt.prepare"]);
    let behavior = echoing_provider("receipt.prepare");
    let r = run_abstract(&plan, &execs, &behavior);

    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    assert_eq!(dispatched_tools(&r.commands), vec!["receipt.prepare".to_string()]);
}

#[test]
fn an_exact_name_wins_over_a_normalized_collision() {
    // `receipt.prepare` and `receipt_prepare` both render as
    // `receipt_prepare`. The tool literally named that is what runs — a
    // guess between the two would silently execute the wrong Definition.
    let plan = PlanGraph::build(&wf(&["decide"])).unwrap();
    let execs = abstract_exec(&["receipt.prepare", "receipt_prepare"]);
    let behavior = echoing_provider("receipt_prepare");
    let r = run_abstract(&plan, &execs, &behavior);

    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    assert_eq!(dispatched_tools(&r.commands), vec!["receipt_prepare".to_string()]);
}

#[test]
fn a_name_matching_neither_form_still_re_prompts_once_then_fails() {
    // The fold must not swallow the unknown-tool guard: a name that is not an
    // offered tool under either spelling gets exactly one correction, and the
    // correction names the tools the way the model was shown them.
    let plan = PlanGraph::build(&wf(&["decide"])).unwrap();
    let execs = abstract_exec(&["receipt.prepare"]);
    let behavior = |key: &JournalKey, _in: &Value| match key.kind {
        EffectKind::Llm => ok(json!({
            "text": Value::Null,
            "tool_calls": [{"id": "call_0", "name": "receipt.file", "arguments": {}}],
            "stop_reason": "tool_use",
        })),
        EffectKind::Tool => ok(json!({})),
    };
    let r = run_abstract(&plan, &execs, &behavior);

    assert!(
        matches!(r.state.outcome(), Some(RunOutcome::Failed { node, detail })
            if node == "decide" && detail.contains("receipt.file")),
        "{:?}",
        r.state.outcome()
    );
    assert!(dispatched_tools(&r.commands).is_empty(), "nothing may execute");
    // Two turns: the first call, then the one after the correction.
    let turns = r
        .commands
        .iter()
        .filter(|c| matches!(c, Command::Dispatch { key, .. } if key.kind == EffectKind::Llm))
        .count();
    assert_eq!(turns, 2, "one corrective re-prompt, then the node fails");
    let correction = r
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::Dispatch { key, input, .. } if key.kind == EffectKind::Llm => {
                Some(input.clone())
            }
            _ => None,
        })
        .next_back()
        .expect("a second turn");
    let text = correction.to_string();
    assert!(text.contains("receipt_prepare"), "the correction offers the model-facing name: {text}");
    assert!(!text.contains("receipt.prepare"), "a dotted choice is one the provider forbids: {text}");
}

// ---- decisions inside an abstract flow (C1, C2) ------------------------------
//
// The scheduler never calls a backend: it emits a `NodeExecutor::Decide`
// effect and consumes the journaled answer like any other result. These
// tests play the driver — scripted model turns, scripted decisions — and pin
// what the scheduler does with each answer, and that every non-answer is the
// deterministic path byte for byte.

use std::cell::RefCell;
use std::collections::VecDeque;

fn llm_calls(calls: &[&str], tokens: u64) -> EffectOutcome {
    EffectOutcome::Completed {
        result: json!({
            "text": Value::Null,
            "tool_calls": calls
                .iter()
                .enumerate()
                .map(|(k, name)| json!({"id": format!("c{tokens}_{k}"), "name": name, "arguments": {}}))
                .collect::<Vec<_>>(),
            "stop_reason": "tool_use",
        }),
        journal_bytes: 10,
        input_tokens: tokens,
        output_tokens: 5,
        usd_micros: 0,
    }
}

fn llm_text(text: &str, tokens: u64) -> EffectOutcome {
    EffectOutcome::Completed {
        result: json!({"text": text, "tool_calls": [], "stop_reason": "end_turn"}),
        journal_bytes: 10,
        input_tokens: tokens,
        output_tokens: 5,
        usd_micros: 0,
    }
}

/// A decision as the driver journals it (`Decision::to_json()`).
fn decision(answers: Value, calibrated: bool) -> EffectOutcome {
    ok(json!({
        "model": "jev-test",
        "provider": "fake",
        "calibrated": calibrated,
        "latency_ms": 7,
        "answers": answers,
    }))
}

fn noul(p: f64) -> Value {
    json!({"type": "noul", "noul": p})
}

/// Model turns in dispatch order; decisions by a caller closure; every tool
/// result is 1000 characters of payload tagged with its effect_seq.
fn scripted<'a>(
    turns: Vec<EffectOutcome>,
    decide: &'a dyn Fn(&Value) -> EffectOutcome,
) -> impl Fn(&JournalKey, &Value) -> EffectOutcome + 'a {
    let turns = RefCell::new(turns.into_iter().collect::<VecDeque<_>>());
    move |key: &JournalKey, input: &Value| match key.kind {
        EffectKind::Llm => turns.borrow_mut().pop_front().expect("model script exhausted"),
        EffectKind::Tool if input.get("decide").is_some() => decide(&input["decide"]),
        EffectKind::Tool => ok(json!({"seq": key.effect_seq, "blob": "x".repeat(1000)})),
    }
}

fn no_decision(_: &Value) -> EffectOutcome {
    panic!("no decision may be asked in this run")
}

fn decide_env<'a>(
    plan: &'a PlanGraph,
    execs: &'a [NodeExecutor],
    descs: &'a BTreeMap<String, String>,
    decide: Option<bool>,
) -> StepEnv<'a> {
    StepEnv {
        llm_reserve_tokens: 100,
        llm_context_tokens: Some(1_000),
        decide: decide.map(|calibrated| DecideEnv {
            calibrated,
            plan_label: Some("triage inbound mail"),
            tool_descriptions: descs,
        }),
        ..env(plan, execs, Budgets::default())
    }
}

fn run_with(env: StepEnv<'_>, behavior: Behavior<'_>) -> SimResult {
    Sim { env, behavior, seed: 1, clock: 1_000, respond: BTreeMap::new() }.run()
}

/// Every dispatch in command order: (key, executor, input).
fn dispatches(cmds: &[Command]) -> Vec<(JournalKey, NodeExecutor, Value)> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::Dispatch { key, executor, input } => {
                Some((key.clone(), executor.clone(), input.clone()))
            }
            _ => None,
        })
        .collect()
}

fn llm_input(cmds: &[Command], effect_seq: u32) -> Value {
    dispatches(cmds)
        .into_iter()
        .find(|(k, _, _)| k.kind == EffectKind::Llm && k.effect_seq == effect_seq)
        .map(|(_, _, i)| i)
        .unwrap_or_else(|| panic!("no model turn at seq {effect_seq}"))
}

fn decide_dispatches(cmds: &[Command]) -> Vec<(JournalKey, Value)> {
    dispatches(cmds)
        .into_iter()
        .filter(|(_, e, _)| matches!(e, NodeExecutor::Decide { .. }))
        .map(|(k, e, i)| {
            let NodeExecutor::Decide { tool_hash, tool_name } = e else { unreachable!() };
            assert_eq!(tool_name, DECIDE_TOOL, "a scheduler ask journals as mg:decide");
            assert!(tool_hash.is_empty(), "a scheduler ask binds no Definition");
            (k, i)
        })
        .collect()
}

fn fold_records(r: &SimResult) -> Vec<FoldRecord> {
    r.checkpoints.iter().flat_map(|c| c.folds.clone()).collect()
}

/// Five rounds of one `fetch` each; the fifth reports 950 prompt tokens, so
/// the turn after it (seq 10) is due a fold (950 + 100 > 1000). The window
/// is entries 1..7 — three rounds: (1,2), (3,4), (5,6).
fn five_rounds_then(rest: Vec<EffectOutcome>) -> Vec<EffectOutcome> {
    let mut turns = vec![
        llm_calls(&["fetch"], 100),
        llm_calls(&["fetch"], 200),
        llm_calls(&["fetch"], 300),
        llm_calls(&["fetch"], 400),
        llm_calls(&["fetch"], 950),
    ];
    turns.extend(rest);
    turns
}

fn one_agent() -> (PlanGraph, Vec<NodeExecutor>) {
    let plan = PlanGraph::build(&wf(&["agent"])).unwrap();
    (plan, abstract_exec(&["fetch"]))
}

#[test]
fn a_calibrated_fold_decision_keeps_truncates_and_drops_and_journals_why() {
    let (plan, execs) = one_agent();
    let descs = BTreeMap::new();
    let decide = |ask: &Value| {
        assert_eq!(ask["purpose"], "fold");
        assert_eq!((ask["from"].as_u64(), ask["to"].as_u64()), (Some(1), Some(7)));
        // Every result is shown as a note, never its contents.
        let shown = ask["state"]["transcript"].to_string();
        assert!(!shown.contains("xxxx"), "tool results never reach the decision: {shown}");
        assert!(shown.contains("ok, "), "{shown}");
        let q = ask["questions"].as_object().unwrap();
        let ids: Vec<&str> = q.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            vec!["keep_call_2", "keep_call_4", "keep_call_6", "keep_result_2", "keep_result_4", "keep_result_6"]
        );
        decision(
            json!({
                "keep_call_2": noul(0.2), "keep_result_2": noul(0.9),   // verbatim
                "keep_call_4": noul(0.8), "keep_result_4": noul(0.1),   // truncated
                "keep_call_6": noul(0.1), "keep_result_6": noul(0.2),   // dropped
            }),
            true,
        )
    };
    let behavior = scripted(five_rounds_then(vec![llm_text(r#"{"done": true}"#, 300)]), &decide);
    let r = run_with(decide_env(&plan, &execs, &descs, Some(true)), &behavior);
    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));

    // The decision was the only fold: no summarizer turn anywhere.
    let asks = decide_dispatches(&r.commands);
    assert_eq!(asks.len(), 1);
    assert_eq!((asks[0].0.effect_seq, asks[0].0.kind), (10, EffectKind::Tool));
    assert!(
        dispatches(&r.commands).iter().all(|(_, _, i)| i.get("fold").is_none()),
        "no summarizer fold ran"
    );

    // The turn after it saw the pruned transcript.
    let next = llm_input(&r.commands, 11);
    let msgs = next["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 10, "11 entries, minus a dropped round of two, plus the note");
    let note = msgs[1]["content"].as_str().unwrap();
    assert!(note.starts_with("[Areev fold 1 (decision): of transcript entries 1..7"), "{note}");
    assert!(note.contains("fake/jev-test") && note.contains("effect_seq 10"), "{note}");
    // Entry 2 verbatim; entry 4 cut to 300 characters plus the note.
    assert_eq!(msgs[3]["content"]["seq"], 1, "kept verbatim: {}", msgs[3]);
    let cut = msgs[5]["content"].as_str().unwrap();
    assert!(cut.contains("[truncated to 300 of"), "{cut}");
    assert_eq!(cut.split('…').next().unwrap().chars().count(), 300);
    // Round (5,6) is gone whole — its call AND its result.
    let shown = next["messages"].to_string();
    assert!(!shown.contains(r#""seq":5"#), "the dropped result left the prompt");
    assert!(!shown.contains("c300_0"), "…and so did the call that issued it");
    assert!(shown.contains("c400_0") && shown.contains(r#""seq":7"#), "the tail is untouched");

    // The journaled record: indices, the version, and the provenance.
    let recs = fold_records(&r);
    assert_eq!(recs.len(), 1);
    let rec = &recs[0];
    assert_eq!((rec.kind.as_str(), rec.v, rec.seq, rec.from, rec.to), ("decide", 1, 10, 1, 7));
    assert!(rec.applied && rec.reason.is_none());
    assert_eq!((rec.kept.clone(), rec.truncated.clone(), rec.dropped.clone()), (vec![2], vec![4], vec![5, 6]));
    assert_eq!(
        rec.provenance,
        Some(json!({"provider": "fake", "model": "jev-test", "calibrated": true, "latency_ms": 7}))
    );
    assert_eq!(r.state.folds, 1, "a decision fold is a fold in the run-outcome count");
}

#[test]
fn a_dropped_result_never_leaves_its_round_behind() {
    // Round 1 issues TWO calls. The decision drops one and keeps the other:
    // the round cannot go whole, so the dropped result is truncated instead
    // and its call stays. Round 2 is dropped whole: assistant entry included.
    let (plan, execs) = one_agent();
    let descs = BTreeMap::new();
    let turns = vec![
        llm_calls(&["fetch", "fetch"], 100), // seq0 → results at 1, 2 → entries 1 (a), 2, 3
        llm_calls(&["fetch"], 200),          // seq3 → 4 (a), 5
        llm_calls(&["fetch"], 300),          // seq5 → 6 (a), 7
        llm_calls(&["fetch"], 400),          // seq7 → 8, 9
        llm_calls(&["fetch"], 950),          // seq9 → 10, 11
        llm_text(r#"{"done": true}"#, 300),
    ];
    let decide = |ask: &Value| {
        // Window 1..8 (12 entries − 4): rounds (1;2,3), (4;5), (6;7).
        assert_eq!((ask["from"].as_u64(), ask["to"].as_u64()), (Some(1), Some(8)));
        decision(
            json!({
                "keep_call_2": noul(0.1), "keep_result_2": noul(0.1),  // drop …
                "keep_call_3": noul(0.1), "keep_result_3": noul(0.9),  // … beside a keep
                "keep_call_5": noul(0.1), "keep_result_5": noul(0.1),  // whole round
                "keep_call_7": noul(0.1), "keep_result_7": noul(0.7),
            }),
            true,
        )
    };
    let behavior = scripted(turns, &decide);
    let r = run_with(decide_env(&plan, &execs, &descs, Some(true)), &behavior);
    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    let rec = &fold_records(&r)[0];
    assert_eq!(rec.truncated, vec![2], "downgraded from drop: its round survives");
    assert_eq!(rec.kept, vec![3, 7]);
    assert_eq!(rec.dropped, vec![4, 5], "a whole round, assistant entry included");

    // No orphan in either direction: every call has its result, every result
    // its call.
    let seq = decide_dispatches(&r.commands)[0].0.effect_seq + 1;
    let msgs = llm_input(&r.commands, seq)["messages"].as_array().unwrap().clone();
    let mut open: Vec<String> = Vec::new();
    for m in &msgs {
        match m["role"].as_str().unwrap() {
            "assistant" => {
                assert!(open.is_empty(), "a call lost its result: {open:?}");
                open = m["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["id"].as_str().unwrap().to_string())
                    .collect();
            }
            "tool" => {
                let id = m["tool_call_id"].as_str().unwrap();
                let at = open.iter().position(|o| o == id).expect("a result with no call");
                open.remove(at);
            }
            _ => {}
        }
    }
}

#[test]
fn still_too_long_after_a_decision_falls_through_to_the_summarizer() {
    let (plan, execs) = one_agent();
    let descs = BTreeMap::new();
    let asked = RefCell::new(0);
    let decide = |_: &Value| {
        *asked.borrow_mut() += 1;
        decision(
            json!({
                "keep_call_2": noul(0.1), "keep_result_2": noul(0.1),
                "keep_call_4": noul(0.9), "keep_result_4": noul(0.9),
                "keep_call_6": noul(0.9), "keep_result_6": noul(0.9),
            }),
            true,
        )
    };
    let behavior = scripted(
        five_rounds_then(vec![
            // seq 11: the pruned transcript is STILL over the ceiling.
            llm_calls(&["fetch"], 980),
            // seq 13: the summarizer — not a second decision.
            llm_text("rounds 1-5 fetched", 500),
            llm_text(r#"{"done": true}"#, 200),
        ]),
        &decide,
    );
    let r = run_with(decide_env(&plan, &execs, &descs, Some(true)), &behavior);
    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    assert_eq!(*asked.borrow(), 1, "one decision per trigger; the next one is the summarizer's");
    let fold = llm_input(&r.commands, 13);
    assert!(fold.get("fold").is_some(), "the summarizer fold ran: {fold}");
    assert_eq!(r.state.folds, 2, "the decision fold and the summarizer fold");
}

/// The summarizer-fold input the SAME script produces with no backend at all
/// — the baseline every fail-open path must reproduce.
fn baseline_fold_input() -> Value {
    let (plan, execs) = one_agent();
    let descs = BTreeMap::new();
    let behavior = scripted(
        five_rounds_then(vec![llm_text("summary", 500), llm_text(r#"{"done": true}"#, 200)]),
        &no_decision,
    );
    let r = run_with(decide_env(&plan, &execs, &descs, None), &behavior);
    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    llm_input(&r.commands, 10)
}

#[test]
fn an_uncalibrated_answer_leaves_the_fold_unchanged() {
    let (plan, execs) = one_agent();
    let descs = BTreeMap::new();
    let decide = |_: &Value| {
        // Would drop everything — if only the backend were calibrated.
        decision(
            json!({
                "keep_call_2": noul(0.0), "keep_result_2": noul(0.0),
                "keep_call_4": noul(0.0), "keep_result_4": noul(0.0),
                "keep_call_6": noul(0.0), "keep_result_6": noul(0.0),
            }),
            false,
        )
    };
    let behavior = scripted(
        five_rounds_then(vec![llm_text("summary", 500), llm_text(r#"{"done": true}"#, 200)]),
        &decide,
    );
    let r = run_with(decide_env(&plan, &execs, &descs, Some(true)), &behavior);
    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    let rec = &fold_records(&r)[0];
    assert!(!rec.applied);
    assert!(rec.reason.as_deref().unwrap().contains("uncalibrated"), "{rec:?}");
    assert!(rec.dropped.is_empty() && rec.truncated.is_empty());
    // The summarizer then folds EXACTLY what it would have with no backend.
    let fold = llm_input(&r.commands, 11);
    let base = baseline_fold_input();
    assert_eq!(fold["messages"], base["messages"]);
    assert_eq!((&fold["fold"]["from"], &fold["fold"]["to"]), (&base["fold"]["from"], &base["fold"]["to"]));
}

#[test]
fn a_failed_decision_leaves_the_fold_unchanged() {
    let (plan, execs) = one_agent();
    let descs = BTreeMap::new();
    let decide = |_: &Value| EffectOutcome::Failed {
        cause: FailCause::Timeout,
        detail: "DEC-E004: deadline".into(),
        journal_bytes: 18,
    };
    let behavior = scripted(
        five_rounds_then(vec![llm_text("summary", 500), llm_text(r#"{"done": true}"#, 200)]),
        &decide,
    );
    let r = run_with(decide_env(&plan, &execs, &descs, Some(true)), &behavior);
    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed), "a decision failure never fails the node");
    let rec = &fold_records(&r)[0];
    assert!(!rec.applied && rec.provenance.is_none());
    assert!(rec.reason.as_deref().unwrap().contains("DEC-E004"), "{rec:?}");
    let fold = llm_input(&r.commands, 11);
    assert_eq!(fold["messages"], baseline_fold_input()["messages"]);
}

#[test]
fn a_malformed_decision_leaves_the_fold_unchanged() {
    let (plan, execs) = one_agent();
    let descs = BTreeMap::new();
    // Calibrated, but one question unanswered.
    let decide =
        |_: &Value| decision(json!({"keep_call_2": noul(0.0), "keep_result_2": noul(0.0)}), true);
    let behavior = scripted(
        five_rounds_then(vec![llm_text("summary", 500), llm_text(r#"{"done": true}"#, 200)]),
        &decide,
    );
    let r = run_with(decide_env(&plan, &execs, &descs, Some(true)), &behavior);
    let rec = &fold_records(&r)[0];
    assert!(!rec.applied && rec.reason.as_deref().unwrap().contains("keep_call_4"), "{rec:?}");
    assert_eq!(llm_input(&r.commands, 11)["messages"], baseline_fold_input()["messages"]);
}

/// Rule 2 at the source: with an uncalibrated backend pinned, nothing is
/// asked at all — every command, every checkpoint byte, is the no-backend
/// run's.
#[test]
fn an_uncalibrated_backend_is_never_asked_and_changes_nothing() {
    let (plan, execs) = one_agent();
    let descs = BTreeMap::new();
    let script =
        || five_rounds_then(vec![llm_text("summary", 500), llm_text(r#"{"done": true}"#, 200)]);
    let with = scripted(script(), &no_decision);
    let without = scripted(script(), &no_decision);
    let a = run_with(decide_env(&plan, &execs, &descs, Some(false)), &with);
    let b = run_with(decide_env(&plan, &execs, &descs, None), &without);
    assert_eq!(
        serde_json::to_string(&a.commands).unwrap(),
        serde_json::to_string(&b.commands).unwrap()
    );
}

// ---- C1: narrowing the offer ------------------------------------------------

fn twelve_tools() -> Vec<String> {
    (0..12).map(|i| format!("t{i:02}")).collect()
}

fn offer_probabilities() -> Value {
    json!({
        "t00": 0.30, "t01": 0.20, "t02": 0.10, "t03": 0.08, "t04": 0.07, "t05": 0.06,
        "t06": 0.055, "t07": 0.052,
        "t08": 0.051, // ninth — kept by the ≥ 0.05 rule, not the top 8
        "t09": 0.01, "t10": 0.01, "t11": 0.002,
    })
}

#[test]
fn more_than_eight_tools_narrow_to_the_top_eight_plus_anything_at_five_percent() {
    let names = twelve_tools();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let plan = PlanGraph::build(&wf(&["agent"])).unwrap();
    let execs = abstract_exec(&refs);
    let descs: BTreeMap<String, String> =
        names.iter().map(|n| (n.clone(), format!("does {n}"))).collect();
    let decide = |ask: &Value| {
        assert_eq!(ask["purpose"], "offer");
        assert_eq!(ask["state"]["plan"], "triage inbound mail");
        assert_eq!(ask["state"]["input"], json!({"seed_input": true}));
        assert_eq!(ask["questions"]["tool"]["criteria"]["t03"], "does t03");
        ok(json!({
            "model": "jev-test", "provider": "fake", "calibrated": true, "latency_ms": 3,
            "answers": {"tool": {"type": "choice", "choice": "t00",
                                 "probabilities": offer_probabilities(), "confidence": 0.2}},
        }))
    };
    let behavior = scripted(
        vec![
            // The model calls a PINNED tool it was not offered: unknown to it.
            llm_calls(&["t10"], 100),
            llm_text(r#"{"done": true}"#, 100),
        ],
        &decide,
    );
    let r = run_with(decide_env(&plan, &execs, &descs, Some(true)), &behavior);
    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));

    let asks = decide_dispatches(&r.commands);
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].0.effect_seq, 0, "asked before the first turn");
    let turn = llm_input(&r.commands, 1);
    let offered: Vec<&str> =
        turn["offer"]["tools"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(offered, vec!["t00", "t01", "t02", "t03", "t04", "t05", "t06", "t07", "t08"]);
    assert_eq!(turn["offer"]["seq"], 0);
    assert_eq!((&turn["offer"]["provider"], &turn["offer"]["calibrated"]), (&json!("fake"), &json!(true)));
    // Narrowing only removes: t10 is pinned, but the model was never shown
    // it, so calling it is the unknown-tool re-prompt — and the correction
    // lists only what WAS offered.
    assert!(dispatched_tools(&r.commands).is_empty(), "t10 never dispatched");
    let reprompt = llm_input(&r.commands, 2)["messages"].to_string();
    assert!(reprompt.contains("Unknown tool(s)") && !reprompt.contains("t11"), "{reprompt}");
    assert_eq!(llm_input(&r.commands, 2)["offer"], turn["offer"], "every turn carries the offer");
}

#[test]
fn eight_or_fewer_tools_are_never_narrowed() {
    let names: Vec<String> = (0..8).map(|i| format!("t{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let plan = PlanGraph::build(&wf(&["agent"])).unwrap();
    let execs = abstract_exec(&refs);
    let descs = BTreeMap::new();
    let script = || vec![llm_text(r#"{"done": true}"#, 100)];
    let with = scripted(script(), &no_decision);
    let without = scripted(script(), &no_decision);
    let a = run_with(decide_env(&plan, &execs, &descs, Some(true)), &with);
    let b = run_with(decide_env(&plan, &execs, &descs, None), &without);
    assert_eq!(
        serde_json::to_string(&a.commands).unwrap(),
        serde_json::to_string(&b.commands).unwrap(),
        "no ask, and a byte-identical run"
    );
}

#[test]
fn an_uncalibrated_or_failed_narrowing_offers_everything() {
    let names = twelve_tools();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let plan = PlanGraph::build(&wf(&["agent"])).unwrap();
    let execs = abstract_exec(&refs);
    let descs = BTreeMap::new();
    let uncalibrated = |_: &Value| {
        ok(json!({
            "model": "m", "provider": "llm", "calibrated": false, "latency_ms": 1,
            "answers": {"tool": {"type": "choice", "choice": "t00",
                                 "probabilities": offer_probabilities(), "confidence": 0.2}},
        }))
    };
    let failed = |_: &Value| EffectOutcome::Failed {
        cause: FailCause::ExecutorError,
        detail: "DEC-E005: chain exhausted".into(),
        journal_bytes: 10,
    };
    let partial = |_: &Value| {
        ok(json!({
            "model": "m", "provider": "fake", "calibrated": true, "latency_ms": 1,
            "answers": {"tool": {"type": "choice", "choice": "t00", "probabilities": {"t00": 1.0}}},
        }))
    };
    for decide in [&uncalibrated as &dyn Fn(&Value) -> EffectOutcome, &failed, &partial] {
        let behavior = scripted(vec![llm_text(r#"{"done": true}"#, 100)], decide);
        let r = run_with(decide_env(&plan, &execs, &descs, Some(true)), &behavior);
        assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
        assert_eq!(decide_dispatches(&r.commands).len(), 1);
        let turn = llm_input(&r.commands, 1);
        assert!(turn.get("offer").is_none(), "the full pinned set: {turn}");
    }
}

/// A decision node (C3) dispatches like a Host tool — intent, then dispatch —
/// and its answer lands in state, where an edge branches on it in the frozen
/// grammar.
#[test]
fn a_decision_node_dispatches_like_a_host_tool_and_edges_branch_on_its_answer() {
    let w = wf(&["triage", "escalate", "ignore"])
        .cond_edge("triage", "escalate", r#"triage.answers.route.choice == "escalate""#)
        .cond_edge("triage", "ignore", r#"triage.answers.route.choice == "ignore""#);
    let plan = PlanGraph::build(&w).unwrap();
    let mut execs = host_execs(&plan);
    execs[0] = NodeExecutor::Decide { tool_hash: "def".into(), tool_name: "triage".into() };
    let behavior = |key: &JournalKey, _: &Value| match key.node.as_str() {
        "triage" => ok(json!({"triage": {"answers": {"route": {"type": "choice", "choice": "escalate"}}}})),
        n => ok(json!({n: true})),
    };
    let r = run_with(env(&plan, &execs, Budgets::default()), &behavior);
    assert_eq!(r.state.outcome(), Some(&RunOutcome::Completed));
    assert_eq!(r.state.context["escalate"], json!(true));
    assert!(r.state.context.get("ignore").is_none(), "the other branch never ran");
    let first = &dispatches(&r.commands)[0];
    assert!(matches!(first.1, NodeExecutor::Decide { .. }) && first.0.kind == EffectKind::Tool);
}
