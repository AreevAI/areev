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
        max_effects_per_attempt: 16,
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
    execs[1] = NodeExecutor::Client { tool_hash: "cafe".into(), tool_name: "approve".into() };

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
    execs[1] = NodeExecutor::Client { tool_hash: "cafe".into(), tool_name: "approve".into() };
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
