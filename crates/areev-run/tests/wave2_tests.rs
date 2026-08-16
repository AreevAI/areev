//! Wave-2 gates (governed-agents §8): abstract nodes as journaled LLM loops
//! over the ToolCallLlm seam, per-dispatch token reservation, and subgraph
//! nodes as child runs — each proven end-to-end over a real store AND
//! journal-consistent under verify (the replay never touches the model).

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_llm::{
    StopReason, ToolCallError, ToolCallLlm, ToolCallOut, ToolCallRequest, ToolCallResponse,
    Usage,
};
use areev_run::{
    BudgetsSpec, ExecResult, HostToolExecutor, OnDangling, RunManifest, RunOptions, Runner,
    RunSession, ScriptedClock,
};
use areev_run_core::{EffectKind, RunError, RunOutcome};
use areev_store::Areev;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// ---- rig -------------------------------------------------------------------

type Behaviors = BTreeMap<String, Box<dyn FnMut(&Value, u32) -> ExecResult + Send>>;

struct TestExec {
    behaviors: Mutex<Behaviors>,
    calls: Mutex<Vec<String>>,
}

impl TestExec {
    fn new() -> Self {
        TestExec { behaviors: Mutex::new(BTreeMap::new()), calls: Mutex::new(Vec::new()) }
    }
    fn on(&self, tool: &str, f: impl FnMut(&Value, u32) -> ExecResult + Send + 'static) {
        self.behaviors.lock().unwrap().insert(tool.into(), Box::new(f));
    }
    fn calls_for(&self, tool: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|t| *t == tool).count()
    }
}

impl HostToolExecutor for TestExec {
    fn execute(&self, tool_name: &str, _hash: &str, input: &Value, _idem: &str) -> ExecResult {
        let count = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(tool_name.to_string());
            calls.iter().filter(|t| *t == tool_name).count() as u32
        };
        let mut behaviors = self.behaviors.lock().unwrap();
        match behaviors.get_mut(tool_name) {
            Some(f) => f(input, count),
            None => ExecResult::Ok(json!({tool_name.to_string(): true})),
        }
    }
}

/// A scripted tool-calling model: responses pop in order; an exhausted
/// script is a TERMINAL error — a test that makes more model calls than it
/// scripted is wrong, and the failure should say so, not hang or loop.
struct ScriptedLlm {
    responses: Mutex<VecDeque<ToolCallResponse>>,
    calls: Mutex<u32>,
}

impl ScriptedLlm {
    fn new(responses: Vec<ToolCallResponse>) -> Self {
        ScriptedLlm {
            responses: Mutex::new(responses.into_iter().collect()),
            calls: Mutex::new(0),
        }
    }
    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

impl ToolCallLlm for ScriptedLlm {
    fn model(&self) -> &str {
        "scripted"
    }
    fn call(&self, _req: &ToolCallRequest<'_>) -> Result<ToolCallResponse, ToolCallError> {
        *self.calls.lock().unwrap() += 1;
        self.responses.lock().unwrap().pop_front().ok_or(ToolCallError {
            retryable: false,
            message: "scripted LLM exhausted — unexpected extra call".into(),
        })
    }
}

fn turn_tools(calls: Vec<(&str, &str, Value)>) -> ToolCallResponse {
    ToolCallResponse {
        text: None,
        tool_calls: calls
            .into_iter()
            .map(|(id, name, arguments)| ToolCallOut {
                id: id.to_string(),
                name: name.to_string(),
                arguments,
                arguments_raw: None,
            })
            .collect(),
        stop_reason: StopReason::ToolUse,
        usage: Usage { input_tokens: 10, output_tokens: 5, cache_read_tokens: None },
    }
}

fn turn_final(text: &str) -> ToolCallResponse {
    ToolCallResponse {
        text: Some(text.to_string()),
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: Usage { input_tokens: 20, output_tokens: 7, cache_read_tokens: None },
    }
}

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
    exec: Arc<TestExec>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig {
            _dir: dir,
            facade: Arc::new(AreevFacade::new(m)),
            exec: Arc::new(TestExec::new()),
        }
    }

    /// A workflow whose named nodes get bound Definitions; nodes listed in
    /// `abstract_nodes` get NO binding and NO Definition — they resolve
    /// abstract.
    fn plan(&self, nodes: &[&str], edges: &[(&str, &str)], abstract_nodes: &[&str]) -> Hash {
        let mut wf = Workflow::new(nodes.iter().map(|s| s.to_string()).collect());
        for (a, b) in edges {
            wf = wf.edge(a, b);
        }
        for n in nodes {
            if abstract_nodes.contains(n) {
                continue;
            }
            let def = Tool::new(n)
                .kind(ToolKind::Definition)
                .tool_description("test tool")
                .created_at(500)
                .namespace("ops");
            let dh = self.facade.with_store(|m| m.add(&def)).unwrap();
            wf = wf.bind(n, &dh.to_hex());
        }
        let wf = wf.created_at(600).namespace("ops");
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn runner(&self, llm: Option<Arc<dyn ToolCallLlm>>) -> Runner {
        self.runner_observed(llm, None)
    }

    fn runner_observed(
        &self,
        llm: Option<Arc<dyn ToolCallLlm>>,
        observer: Option<Arc<dyn areev_run::RunObserver>>,
    ) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(clocks())),
            executor: Arc::clone(&self.exec) as Arc<dyn HostToolExecutor>,
            llm,
            observer,
            ns: "ops".into(),
            principal: "user:runner".into(),
        }
    }

    fn final_context(&self, run_id: &str) -> Value {
        self.facade
            .with_store(|m| areev_run::journal::load(m, "ops", run_id))
            .unwrap()
            .checkpoints
            .last()
            .unwrap()
            .scheduler
            .get("context")
            .cloned()
            .unwrap()
    }

    fn journal_keys(&self, run_id: &str, node: &str) -> Vec<(u32, u32, EffectKind)> {
        self.facade
            .with_store(|m| areev_run::journal::load(m, "ops", run_id))
            .unwrap()
            .entries
            .keys()
            .filter(|k| k.node == node)
            .map(|k| (k.attempt, k.effect_seq, k.kind))
            .collect()
    }
}

fn opts() -> RunOptions {
    RunOptions {
        budgets: BudgetsSpec::default(),
        ask_ttl_sec: None,
        workers: 2,
        on_dangling: OnDangling::Redispatch,
        llm_max_tokens: None,
        inject_crash: None,
    }
}

fn clocks() -> Vec<u64> {
    (0..400).map(|i| 1_756_000_000_000 + i * 10).collect()
}

// ---- abstract nodes --------------------------------------------------------

#[test]
fn abstract_node_llm_loop_calls_tools_and_completes() {
    let rig = Rig::new();
    // "fetch" is a bound Host node; "summarize" is abstract — its offered
    // tools are the manifest's Host pins (fetch).
    let plan = rig.plan(&["fetch", "summarize"], &[("fetch", "summarize")], &["summarize"]);
    rig.exec.on("fetch", |_, _| ExecResult::Ok(json!({"fetched": 7})));
    let llm = Arc::new(ScriptedLlm::new(vec![
        turn_tools(vec![("call_1", "fetch", json!({"q": "x"}))]),
        turn_final(r#"{"summary": "seven"}"#),
    ]));
    let runner = rig.runner(Some(llm.clone()));

    let session = runner.start(&plan, "abs-1", json!({"q": 1}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(llm.calls(), 2, "turn, tool round, final turn");
    // fetch ran twice: once as the static node, once as the model's call.
    assert_eq!(rig.exec.calls_for("fetch"), 2);

    // The final state carries the loop's answer, merged as a JSON object.
    let ctx = rig.final_context("abs-1");
    assert_eq!(ctx["summary"], "seven");
    assert_eq!(ctx["fetched"], 7);

    // The journal shape §5.1 pins: turn 0 (llm), the model's tool call at
    // seq 1 (tool), the closing turn at seq 2 (llm) — one attempt.
    let keys = rig.journal_keys("abs-1", "summarize");
    assert_eq!(
        keys,
        vec![
            (1, 0, EffectKind::Llm),
            (1, 1, EffectKind::Tool),
            (1, 2, EffectKind::Llm),
        ]
    );

    // Token accounting followed the journaled usage.
    let sched = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "abs-1"))
        .unwrap();
    let spent = sched.checkpoints.last().unwrap().scheduler.get("spent").cloned().unwrap();
    assert_eq!(spent["input_tokens"], 30);
    assert_eq!(spent["output_tokens"], 12);

    // Verify replays from the journal — the scripted model is exhausted, so
    // any model call during verify would fail loudly.
    let report = runner.verify("abs-1").unwrap();
    assert!(report.verified, "{report:?}");
    assert_eq!(llm.calls(), 2, "verify never called the model");
}

#[test]
fn abstract_node_without_llm_refuses_at_resolve() {
    let rig = Rig::new();
    let plan = rig.plan(&["think"], &[], &["think"]);
    let runner = rig.runner(None);
    let err = runner.start(&plan, "abs-2", json!({}), &opts()).unwrap_err();
    assert!(matches!(err, RunError::NoToolLlm { .. }), "{err}");
    assert!(err.to_string().starts_with("RUN-E006"), "{err}");
}

#[test]
fn llm_reservation_refuses_turn_that_cannot_fit() {
    let rig = Rig::new();
    let plan = rig.plan(&["think"], &[], &["think"]);
    // Ceiling 100 < the 256-token reservation: the turn is never dispatched.
    let llm = Arc::new(ScriptedLlm::new(vec![]));
    let runner = rig.runner(Some(llm.clone()));
    let mut o = opts();
    o.budgets.max_tokens = Some(100);
    o.llm_max_tokens = Some(256);

    let session = runner.start(&plan, "abs-3", json!({}), &o).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert!(
        matches!(outcome, RunOutcome::BudgetExhausted { .. }),
        "reservation must trip the token axis, got {outcome:?}"
    );
    assert_eq!(llm.calls(), 0, "no model call may precede the reservation check");

    // The reservation refusal is replay-consistent like everything else.
    let report = runner.verify("abs-3").unwrap();
    assert!(report.verified, "{report:?}");
}

#[test]
fn abstract_flow_tool_failure_is_model_visible_not_a_retry() {
    let rig = Rig::new();
    let plan = rig.plan(&["lookup", "answer"], &[("lookup", "answer")], &["answer"]);
    rig.exec.on("lookup", |input, count| {
        // The static node run succeeds; the model's call fails.
        if input.get("from_model").is_some() || count > 1 {
            ExecResult::Err {
                cause: areev_run_core::FailCause::ExecutorError,
                detail: "backend down".into(),
            }
        } else {
            ExecResult::Ok(json!({"looked_up": true}))
        }
    });
    let llm = Arc::new(ScriptedLlm::new(vec![
        turn_tools(vec![("c1", "lookup", json!({"from_model": true}))]),
        turn_final(r#"{"answer": "degraded"}"#),
    ]));
    let runner = rig.runner(Some(llm.clone()));

    let session = runner.start(&plan, "abs-4", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    // The model saw the error result and still answered: the run completes.
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(rig.final_context("abs-4")["answer"], "degraded");
    // Exactly one model-issued call — the scheduler never retried the tool
    // behind the model's back.
    assert_eq!(rig.exec.calls_for("lookup"), 2, "static run + one model call");
    assert!(runner.verify("abs-4").unwrap().verified);
}

#[test]
fn unknown_tool_gets_one_reprompt_then_fails() {
    let rig = Rig::new();
    let plan = rig.plan(&["fetch", "act"], &[("fetch", "act")], &["act"]);
    let llm = Arc::new(ScriptedLlm::new(vec![
        turn_tools(vec![("c1", "bogus", json!({}))]),
        turn_tools(vec![("c2", "bogus", json!({}))]),
    ]));
    let runner = rig.runner(Some(llm.clone()));

    let session = runner.start(&plan, "unk-1", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    let RunOutcome::Failed { node, detail } = outcome else {
        panic!("expected Failed, got {outcome:?}")
    };
    assert_eq!(node, "act");
    assert!(detail.contains("unknown tool"), "{detail}");
    assert_eq!(llm.calls(), 2, "one correction, then failure — never a third turn");
    assert!(runner.verify("unk-1").unwrap().verified);
}

#[test]
fn strict_args_violation_reprompts_with_the_error() {
    let rig = Rig::new();
    // A STRICT definition: q is required and must be a string.
    let def = Tool::new("search")
        .kind(ToolKind::Definition)
        .tool_description("strict tool")
        .input_schema(json!({
            "type": "object",
            "required": ["q"],
            "properties": {"q": {"type": "string"}},
        }))
        .strict(true)
        .created_at(500)
        .namespace("ops");
    let def_hash = rig.facade.with_store(|m| m.add(&def)).unwrap();
    let wf = Workflow::new(vec!["search".into(), "act".into()])
        .edge("search", "act")
        .bind("search", &def_hash.to_hex())
        .created_at(600)
        .namespace("ops");
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
    rig.exec.on("search", |_, _| ExecResult::Ok(json!({"hits": 3})));

    let llm = Arc::new(ScriptedLlm::new(vec![
        // Wrong args first (q missing) — must NOT dispatch.
        turn_tools(vec![("c1", "search", json!({"query": 1}))]),
        // Corrected.
        turn_tools(vec![("c2", "search", json!({"q": "ok"}))]),
        turn_final(r#"{"done": true}"#),
    ]));
    let runner = rig.runner(Some(llm.clone()));

    let session = runner.start(&plan, "val-1", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(llm.calls(), 3);
    // The invalid call never reached the executor: one static run + one
    // corrected model call.
    assert_eq!(rig.exec.calls_for("search"), 2);
    assert_eq!(rig.final_context("val-1")["done"], true);
    assert!(runner.verify("val-1").unwrap().verified);
}

// ---- streaming (§6.10) -----------------------------------------------------

/// The §6.10 acceptance check: the journal is IDENTICAL with no subscriber,
/// a subscriber, and a deliberately slow subscriber — events observe the
/// run, they never participate in it.
#[test]
fn streaming_observers_never_change_the_journal() {
    struct Collect {
        events: Mutex<Vec<areev_run::RunEvent>>,
        delay: std::time::Duration,
    }
    impl areev_run::RunObserver for Collect {
        fn event(&self, ev: &areev_run::RunEvent) {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            self.events.lock().unwrap().push(ev.clone());
        }
    }

    let rig = Rig::new();
    let plan = rig.plan(&["a", "b", "c"], &[("a", "b"), ("b", "c")], &[]);
    let normal = Arc::new(Collect { events: Mutex::new(vec![]), delay: Default::default() });
    let slow = Arc::new(Collect {
        events: Mutex::new(vec![]),
        delay: std::time::Duration::from_millis(3),
    });

    let fingerprint = |run_id: &str| {
        let view = rig
            .facade
            .with_store(|m| areev_run::journal::load(m, "ops", run_id))
            .unwrap();
        let keys: Vec<(String, String, u32, u32)> = view
            .entries
            .keys()
            .map(|k| (k.task_path.clone(), k.node.clone(), k.attempt, k.effect_seq))
            .collect();
        (keys, view.checkpoints.len(), rig.final_context(run_id))
    };

    for (run_id, observer) in [
        ("st-none", None::<Arc<dyn areev_run::RunObserver>>),
        ("st-obs", Some(Arc::clone(&normal) as _)),
        ("st-slow", Some(Arc::clone(&slow) as _)),
    ] {
        let runner = rig.runner_observed(None, observer);
        let session = runner.start(&plan, run_id, json!({"q": 1}), &opts()).unwrap();
        assert!(matches!(
            session,
            RunSession::Finished { outcome: RunOutcome::Completed, .. }
        ));
    }

    let base = fingerprint("st-none");
    assert_eq!(fingerprint("st-obs"), base, "a subscriber must not change the journal");
    assert_eq!(fingerprint("st-slow"), base, "a SLOW subscriber must not change the journal");

    // Both subscribers saw the full, ordered story: RunStarted first,
    // RunFinished last with zero drops at this volume, and one
    // EffectSettled per journal entry.
    for obs in [&normal, &slow] {
        let events = obs.events.lock().unwrap();
        assert!(
            matches!(events.first(), Some(areev_run::RunEvent::RunStarted { .. })),
            "{:?}",
            events.first()
        );
        match events.last() {
            Some(areev_run::RunEvent::RunFinished { outcome, dropped_events, .. }) => {
                assert_eq!(outcome, "Completed");
                assert_eq!(*dropped_events, 0);
            }
            other => panic!("expected RunFinished last, got {other:?}"),
        }
        let settled = events
            .iter()
            .filter(|e| matches!(e, areev_run::RunEvent::EffectSettled { .. }))
            .count();
        assert_eq!(settled, base.0.len(), "one EffectSettled per journal entry");
    }
}

/// TokenChunk wiring: an OBSERVED abstract node streams its model text
/// through the bus (the scripted seam delivers the final text as one chunk
/// via the default `call_streaming`), and the journal still verifies.
#[test]
fn observed_abstract_node_emits_token_chunks() {
    struct Collect(Mutex<Vec<areev_run::RunEvent>>);
    impl areev_run::RunObserver for Collect {
        fn event(&self, ev: &areev_run::RunEvent) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }
    let rig = Rig::new();
    let plan = rig.plan(&["think"], &[], &["think"]);
    let llm = Arc::new(ScriptedLlm::new(vec![turn_final(r#"{"idea": "streamed"}"#)]));
    let obs = Arc::new(Collect(Mutex::new(vec![])));
    let runner = rig.runner_observed(Some(llm), Some(Arc::clone(&obs) as _));

    let session = runner.start(&plan, "tok-1", json!({}), &opts()).unwrap();
    assert!(matches!(session, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    let events = obs.0.lock().unwrap();
    let chunks: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            areev_run::RunEvent::TokenChunk { node, text, .. } => {
                assert_eq!(node, "think");
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(chunks.join(""), r#"{"idea": "streamed"}"#);
    drop(events);
    assert!(runner.verify("tok-1").unwrap().verified);
}

// ---- forks (§5.4) ----------------------------------------------------------

#[test]
fn fork_replays_time_travel_without_touching_the_base() {
    let rig = Rig::new();
    let plan = rig.plan(&["a", "b"], &[("a", "b")], &[]);
    // "b" answers with its lifetime call count — a fork that re-executes it
    // observably diverges from the base.
    rig.exec.on("b", |_, count| ExecResult::Ok(json!({"b_ran": count})));
    let runner = rig.runner(None);

    let session = runner.start(&plan, "base-1", json!({}), &opts()).unwrap();
    assert!(matches!(session, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    assert_eq!(rig.final_context("base-1")["b_ran"], 1);
    let base_entries = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "base-1"))
        .unwrap()
        .entries
        .len();

    // Fork at superstep 1 (after "a", before "b") and resume: "b"
    // re-executes under the fork's own journal keys.
    let seed = runner.fork("base-1", Some(1), "fork-1", None, &opts()).unwrap();
    let session = runner.resume("fork-1", &opts()).unwrap();
    assert!(matches!(session, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    assert_eq!(rig.final_context("fork-1")["b_ran"], 2, "the fork re-ran b");

    // The base is untouched: same journal, same final state, still verified.
    assert_eq!(rig.final_context("base-1")["b_ran"], 1);
    let base_after = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "base-1"))
        .unwrap();
    assert_eq!(base_after.entries.len(), base_entries, "base journal untouched");
    assert!(runner.verify("base-1").unwrap().verified);
    assert!(runner.verify("fork-1").unwrap().verified, "fork tail verifies from the seed");

    // The lineage is recorded: manifest fork_of + the mg:fork_of Fact, and
    // the seed checkpoint chains to the base checkpoint.
    let manifest = rig
        .facade
        .with_store(|m| RunManifest::load(m, "fork-1"))
        .unwrap();
    let fork_of = manifest.fork_of.expect("fork manifest records its base");
    assert_eq!(fork_of.base_run, "base-1");
    assert_eq!(fork_of.base_superstep, 1);
    let seed_grain = rig.facade.with_store(|m| m.get(&seed)).unwrap();
    assert_eq!(
        seed_grain.get_str("derived_from"),
        Some(fork_of.base_checkpoint.as_str()),
        "seed checkpoint derives from the base checkpoint"
    );
}

#[test]
fn fork_onto_new_plan_migrates_with_inherited_context() {
    let rig = Rig::new();
    let plan_v1 = rig.plan(&["gather"], &[], &[]);
    rig.exec.on("gather", |_, _| ExecResult::Ok(json!({"gathered": 11})));
    let runner = rig.runner(None);
    let session = runner.start(&plan_v1, "mig-base", json!({"tenant": "acme"}), &opts()).unwrap();
    assert!(matches!(session, RunSession::Finished { outcome: RunOutcome::Completed, .. }));

    // The v2 plan has a different graph; its node must see the base state.
    let plan_v2 = rig.plan(&["publish"], &[], &[]);
    let seen = Arc::new(Mutex::new(Value::Null));
    let cap = Arc::clone(&seen);
    rig.exec.on("publish", move |input, _| {
        *cap.lock().unwrap() = input.clone();
        ExecResult::Ok(json!({"published": true}))
    });

    // Fork from the terminal run? Refused — pick the last CLOSED superstep.
    let err = runner
        .fork("mig-base", None, "mig-2", Some(&plan_v2), &opts())
        .unwrap_err();
    assert!(err.to_string().contains("terminal"), "{err}");

    let _seed = runner
        .fork("mig-base", Some(1), "mig-2", Some(&plan_v2), &opts())
        .unwrap();
    let session = runner.resume("mig-2", &opts()).unwrap();
    assert!(matches!(session, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    let input_seen = seen.lock().unwrap().clone();
    assert_eq!(input_seen["gathered"], 11, "migrated run inherited the context");
    assert_eq!(input_seen["tenant"], "acme");
    assert!(runner.verify("mig-2").unwrap().verified);
}

// ---- Send fan-out ----------------------------------------------------------

#[test]
fn send_fan_out_spawns_tasks_and_joins_before_downstream() {
    let rig = Rig::new();
    // seed → worker → collect; seed's RESULT spawns three worker tasks with
    // task-scoped inputs. The static activation of worker is preempted.
    let plan = rig.plan(
        &["seed", "worker", "collect"],
        &[("seed", "worker"), ("worker", "collect")],
        &[],
    );
    rig.exec.on("seed", |_, _| {
        ExecResult::Ok(json!({
            "seeded": true,
            "$send": [
                {"node": "worker", "input": {"v": 1}},
                {"node": "worker", "input": {"v": 2}},
                {"node": "worker", "input": {"v": 3}},
            ],
        }))
    });
    rig.exec.on("worker", |input, _| {
        let v = input["v"].as_u64().unwrap();
        ExecResult::Ok(json!({ format!("out{v}"): v * 2 }))
    });
    let seen_by_collect = Arc::new(Mutex::new(Value::Null));
    let seen = Arc::clone(&seen_by_collect);
    rig.exec.on("collect", move |input, _| {
        *seen.lock().unwrap() = input.clone();
        ExecResult::Ok(json!({"collected": true}))
    });

    let runner = rig.runner(None);
    let session = runner.start(&plan, "send-1", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    // Each task ran once with ITS input; the join held collect until all
    // three results had merged.
    assert_eq!(rig.exec.calls_for("worker"), 3);
    let joined = seen_by_collect.lock().unwrap().clone();
    assert_eq!(joined["out1"], 2, "collect saw task 1's merge: {joined}");
    assert_eq!(joined["out2"], 4);
    assert_eq!(joined["out3"], 6);
    let ctx = rig.final_context("send-1");
    assert_eq!(ctx["collected"], true);
    assert!(ctx.get("$send").is_none(), "$send is stripped, never merged");

    // §5.1 task paths: fresh zero-padded ordinals under the static parent.
    let mut paths: Vec<String> = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "send-1"))
        .unwrap()
        .entries
        .keys()
        .filter(|k| !k.task_path.is_empty())
        .map(|k| k.task_path.clone())
        .collect();
    paths.dedup();
    assert_eq!(paths, vec!["/0000", "/0001", "/0002"]);

    assert!(runner.verify("send-1").unwrap().verified);
}

#[test]
fn send_task_failure_retries_with_task_scoped_attempts() {
    let rig = Rig::new();
    let wf_nodes = &["seed", "worker"];
    // Build with a retry budget on worker.
    let mut wf = Workflow::new(wf_nodes.iter().map(|s| s.to_string()).collect())
        .edge("seed", "worker")
        .retry("worker", 1);
    for n in wf_nodes {
        let def = Tool::new(n)
            .kind(ToolKind::Definition)
            .tool_description("test tool")
            .created_at(500)
            .namespace("ops");
        let dh = rig.facade.with_store(|m| m.add(&def)).unwrap();
        wf = wf.bind(n, &dh.to_hex());
    }
    let plan = rig
        .facade
        .with_store(|m| m.add(&wf.created_at(600).namespace("ops")))
        .unwrap();

    rig.exec.on("seed", |_, _| {
        ExecResult::Ok(json!({"$send": [{"node": "worker", "input": {"v": 9}}]}))
    });
    let failed_once = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&failed_once);
    rig.exec.on("worker", move |_, _| {
        let mut f = flag.lock().unwrap();
        if !*f {
            *f = true;
            ExecResult::Err {
                cause: areev_run_core::FailCause::ExecutorError,
                detail: "transient".into(),
            }
        } else {
            ExecResult::Ok(json!({"ok": true}))
        }
    });

    let runner = rig.runner(None);
    let session = runner.start(&plan, "send-2", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(rig.exec.calls_for("worker"), 2, "attempt 1 failed, attempt 2 succeeded");

    // Both attempts journaled under the SAME task path.
    let attempts: Vec<u32> = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "send-2"))
        .unwrap()
        .entries
        .keys()
        .filter(|k| k.task_path == "/0000")
        .map(|k| k.attempt)
        .collect();
    assert_eq!(attempts, vec![1, 2]);
    assert!(runner.verify("send-2").unwrap().verified);
}

#[test]
fn append_reducer_accumulates_fan_out_results_in_spawn_order() {
    let rig = Rig::new();
    // Same map-reduce shape, but "results" is declared APPEND on the
    // workflow — the three workers accumulate instead of clobbering.
    let mut wf = Workflow::new(vec!["seed".into(), "worker".into(), "collect".into()])
        .edge("seed", "worker")
        .edge("worker", "collect");
    for n in ["seed", "worker", "collect"] {
        let def = Tool::new(n)
            .kind(ToolKind::Definition)
            .tool_description("test tool")
            .created_at(500)
            .namespace("ops");
        let dh = rig.facade.with_store(|m| m.add(&def)).unwrap();
        wf = wf.bind(n, &dh.to_hex());
    }
    let mut wf = wf.created_at(600).namespace("ops");
    wf.common
        .extra_fields
        .insert("reducers".into(), json!({"results": "append", "total": "sum"}));
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();

    rig.exec.on("seed", |_, _| {
        ExecResult::Ok(json!({"$send": [
            {"node": "worker", "input": {"v": 1}},
            {"node": "worker", "input": {"v": 2}},
            {"node": "worker", "input": {"v": 3}},
        ]}))
    });
    rig.exec.on("worker", |input, _| {
        let v = input["v"].as_u64().unwrap();
        ExecResult::Ok(json!({"results": [v * 2], "total": v}))
    });
    rig.exec.on("collect", |_, _| ExecResult::Ok(json!({"collected": true})));

    let runner = rig.runner(None);
    let session = runner.start(&plan, "red-1", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    let ctx = rig.final_context("red-1");
    assert_eq!(ctx["results"], json!([2, 4, 6]), "append in spawn order: {ctx}");
    assert_eq!(ctx["total"], 6, "sum across tasks");
    // The frozen table replays identically.
    assert!(runner.verify("red-1").unwrap().verified);
}

#[test]
fn unknown_reducer_name_refuses_at_start() {
    let rig = Rig::new();
    let def = Tool::new("a")
        .kind(ToolKind::Definition)
        .tool_description("t")
        .created_at(500)
        .namespace("ops");
    let dh = rig.facade.with_store(|m| m.add(&def)).unwrap();
    let mut wf = Workflow::new(vec!["a".into()])
        .bind("a", &dh.to_hex())
        .created_at(600)
        .namespace("ops");
    wf.common
        .extra_fields
        .insert("reducers".into(), json!({"x": "median"}));
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
    let runner = rig.runner(None);
    let err = runner.start(&plan, "red-2", json!({}), &opts()).unwrap_err();
    assert!(err.to_string().contains("unknown reducer 'median'"), "{err}");
}

#[test]
fn malformed_send_decision_fails_fast_naming_the_spawner() {
    let rig = Rig::new();
    let plan = rig.plan(&["seed", "worker"], &[("seed", "worker")], &[]);
    rig.exec.on("seed", |_, _| {
        // Unknown target node.
        ExecResult::Ok(json!({"$send": [{"node": "nope", "input": {}}]}))
    });
    let runner = rig.runner(None);
    let session = runner.start(&plan, "send-3", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    let RunOutcome::Failed { node, detail } = outcome else {
        panic!("expected Failed, got {outcome:?}")
    };
    assert_eq!(node, "seed");
    assert!(detail.contains("unknown node"), "{detail}");
    assert_eq!(rig.exec.calls_for("worker"), 0, "no task spawned from a bad decision");
    assert!(runner.verify("send-3").unwrap().verified);
}

// ---- subgraphs -------------------------------------------------------------

#[test]
fn subgraph_runs_as_child_run_and_merges_final_context() {
    let rig = Rig::new();
    // Child plan: one bound node.
    let child_plan = rig.plan(&["double"], &[], &[]);
    rig.exec.on("double", |_, _| ExecResult::Ok(json!({"doubled": 4})));
    rig.exec.on("prep", |_, _| ExecResult::Ok(json!({"n": 2})));

    // Parent plan: prep → sub, where sub BINDS the child Workflow hash.
    let prep_def = Tool::new("prep")
        .kind(ToolKind::Definition)
        .tool_description("test tool")
        .created_at(500)
        .namespace("ops");
    let prep_hash = rig.facade.with_store(|m| m.add(&prep_def)).unwrap();
    let wf = Workflow::new(vec!["prep".into(), "sub".into()])
        .edge("prep", "sub")
        .bind("prep", &prep_hash.to_hex())
        .bind("sub", &child_plan.to_hex())
        .created_at(600)
        .namespace("ops");
    let parent_plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();

    let runner = rig.runner(None);
    let session = runner.start(&parent_plan, "par-1", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    // The child ran under its own deterministic run id with its own journal.
    let parent_view = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "par-1"))
        .unwrap();
    let sub_key = parent_view
        .entries
        .keys()
        .find(|k| k.node == "sub")
        .expect("subgraph effect journaled in the parent");
    let child_run_id = format!("par-1~{}", &sub_key.tool_call_id()[..16]);
    let child_manifest = rig
        .facade
        .with_store(|m| RunManifest::load(m, &child_run_id))
        .expect("child manifest exists");
    assert_eq!(child_manifest.plan_hash, child_plan.to_hex());
    assert_eq!(rig.final_context(&child_run_id)["doubled"], 4);

    // The child's final context became the parent node's result.
    let ctx = rig.final_context("par-1");
    assert_eq!(ctx["doubled"], 4);
    assert_eq!(ctx["n"], 2);

    // Both journals verify; the parent's replay answers the subgraph effect
    // from the journal, never re-running the child.
    assert!(runner.verify(&child_run_id).unwrap().verified);
    assert!(runner.verify("par-1").unwrap().verified);
    assert_eq!(rig.exec.calls_for("double"), 1, "the child executed once, ever");
}
