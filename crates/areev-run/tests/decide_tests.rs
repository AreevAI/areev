//! Decisions in a run (`docs/decision-model-proposal.md` §4, rows C1–C3),
//! end to end over a real store: a decision NODE bound to `areev://decide`
//! (C3), the decision-guided fold (C2) and the tool-offer narrowing (C1).
//! Every decision is journaled like any other effect, so `verify` answers it
//! from the journal and never asks the backend again — each test pins that
//! by counting backend calls across the verify.

use areev_cal::AreevFacade;
use areev_core::decide::{Answer, DecideError, DecideRequest, Decision, DecisionBackend};
use areev_core::error::Hash;
use areev_core::types::{ExecutorKind, Grain, Tool, ToolKind, Workflow};
use areev_llm::{
    StopReason, ToolCallError, ToolCallLlm, ToolCallOut, ToolCallRequest, ToolCallResponse, Usage,
};
use areev_run::{
    ExecResult, HostToolExecutor, RunManifest, RunOptions, RunSession, Runner, ScriptedClock,
};
use areev_run_core::{EffectKind, EffectOutcome, RunError, RunOutcome};
use areev_store::Areev;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// ---- fakes -----------------------------------------------------------------

type Answerer = Box<dyn Fn(&DecideRequest) -> Result<BTreeMap<String, Answer>, DecideError> + Send + Sync>;

/// A scripted decision backend: records every request, answers through a
/// closure. `calibrated` is what it reports — the run pins it at start.
struct FakeDecider {
    calibrated: bool,
    answer: Answerer,
    asked: Mutex<Vec<DecideRequest>>,
}

impl FakeDecider {
    fn new(calibrated: bool, answer: Answerer) -> Arc<Self> {
        Arc::new(FakeDecider { calibrated, answer, asked: Mutex::new(Vec::new()) })
    }
    fn calls(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
    fn request(&self, i: usize) -> DecideRequest {
        self.asked.lock().unwrap()[i].clone()
    }
}

impl DecisionBackend for FakeDecider {
    fn decide(&self, req: &DecideRequest) -> Result<Decision, DecideError> {
        req.validate()?;
        self.asked.lock().unwrap().push(req.clone());
        Ok(Decision {
            answers: (self.answer)(req)?,
            model: "jev-test".into(),
            provider: "fake".into(),
            calibrated: self.calibrated,
            input_tokens: Some(40),
            output_tokens: Some(2),
            latency_ms: 12,
        })
    }
    fn calibrated(&self) -> bool {
        self.calibrated
    }
    fn describe(&self) -> String {
        "fake:jev-test".into()
    }
}

fn choice(pick: &str, probs: &[(&str, f32)]) -> Answer {
    Answer::Choice {
        choice: pick.into(),
        probabilities: probs.iter().map(|(k, p)| (k.to_string(), *p)).collect(),
        confidence: 0.5,
    }
}

struct CountingExec {
    calls: Mutex<Vec<String>>,
}

impl HostToolExecutor for CountingExec {
    fn execute(&self, tool_name: &str, _h: &str, _input: &Value, _idem: &str) -> ExecResult {
        let n = {
            let mut c = self.calls.lock().unwrap();
            c.push(tool_name.to_string());
            c.iter().filter(|t| *t == tool_name).count()
        };
        ExecResult::Ok(json!({ tool_name.to_string(): true, "round": n, "blob": "x".repeat(900) }))
    }
}

struct ScriptedLlm {
    responses: Mutex<VecDeque<ToolCallResponse>>,
    offered: Mutex<Vec<Vec<String>>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<ToolCallResponse>) -> Arc<Self> {
        Arc::new(ScriptedLlm {
            responses: Mutex::new(responses.into_iter().collect()),
            offered: Mutex::new(Vec::new()),
        })
    }
    fn offered(&self) -> Vec<Vec<String>> {
        self.offered.lock().unwrap().clone()
    }
}

impl ToolCallLlm for ScriptedLlm {
    fn model(&self) -> &str {
        "scripted"
    }
    fn call(&self, req: &ToolCallRequest<'_>) -> Result<ToolCallResponse, ToolCallError> {
        self.offered
            .lock()
            .unwrap()
            .push(req.tools.iter().map(|t| t.tool_name.clone()).collect());
        self.responses.lock().unwrap().pop_front().ok_or(ToolCallError {
            retryable: false,
            message: "scripted LLM exhausted".into(),
            ..Default::default()
        })
    }
}

fn calls_at(names: &[&str], input_tokens: u64) -> ToolCallResponse {
    ToolCallResponse::new(
        None,
        names
            .iter()
            .enumerate()
            .map(|(k, n)| ToolCallOut {
                id: format!("c{input_tokens}_{k}"),
                name: n.to_string(),
                arguments: json!({}),
                arguments_raw: None,
            })
            .collect(),
        StopReason::ToolUse,
        Usage { input_tokens, output_tokens: 5, cache_read_tokens: None },
    )
}

fn final_at(text: &str, input_tokens: u64) -> ToolCallResponse {
    ToolCallResponse::new(
        Some(text.into()),
        vec![],
        StopReason::EndTurn,
        Usage { input_tokens, output_tokens: 5, cache_read_tokens: None },
    )
}

// ---- rig -------------------------------------------------------------------

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
    exec: Arc<CountingExec>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig {
            _dir: dir,
            facade: Arc::new(AreevFacade::new(m)),
            exec: Arc::new(CountingExec { calls: Mutex::new(Vec::new()) }),
        }
    }

    fn add<G: Grain + 'static>(&self, g: &G) -> Hash {
        self.facade.with_store(|m| m.add(g)).unwrap()
    }

    fn host_def(&self, name: &str) -> Hash {
        self.add(
            &Tool::new(name)
                .kind(ToolKind::Definition)
                .tool_description(&format!("does {name}\nsecond line never shown"))
                .created_at(500)
                .namespace("ops"),
        )
    }

    /// A decision-node Definition: `executor_uri: "areev://decide"` plus the
    /// optional `decide` declaration.
    fn decide_def(&self, name: &str, decide: Option<Value>) -> Hash {
        let mut def = Tool::new(name)
            .kind(ToolKind::Definition)
            .executor_uri(areev_run::DECIDE_URI)
            .input_schema(json!({
                "type": "object",
                "properties": {"state": {}, "questions": {"type": "object"}},
                "required": ["state", "questions"],
            }))
            .strict(true)
            .created_at(500)
            .namespace("ops");
        if let Some(d) = decide {
            def.common.extra_fields.insert("decide".into(), d);
        }
        self.add(&def)
    }

    fn runner(&self, llm: Option<Arc<dyn ToolCallLlm>>) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new((0..400).map(|i| 1_756_000_000_000 + i * 10).collect())),
            executor: Arc::clone(&self.exec) as Arc<dyn HostToolExecutor>,
            llm,
            observer: None,
            ns: "ops".into(),
            principal: "user:runner".into(),
        }
    }

    fn calls_for(&self, tool: &str) -> usize {
        self.exec.calls.lock().unwrap().iter().filter(|t| *t == tool).count()
    }

    fn view(&self, run_id: &str) -> areev_run::journal::JournalView {
        self.facade.with_store(|m| areev_run::journal::load(m, "ops", run_id)).unwrap()
    }

    fn final_context(&self, run_id: &str) -> Value {
        self.view(run_id).checkpoints.last().unwrap().scheduler["context"].clone()
    }
}

fn opts() -> RunOptions {
    RunOptions { workers: 2, ..Default::default() }
}

fn route_questions() -> Value {
    json!({"route": {"type": "choice", "instructions": "Does this item need a person now?",
                     "criteria": {"escalate": "a person must act today", "ignore": "routine"}}})
}

/// triage (decision node) → escalate | ignore, branching on the answer.
fn triage_plan(rig: &Rig, decide: Option<Value>) -> Hash {
    let triage = rig.decide_def("triage", decide);
    let escalate = rig.host_def("escalate");
    let ignore = rig.host_def("ignore");
    let wf = Workflow::new(vec!["triage".into(), "escalate".into(), "ignore".into()])
        .cond_edge("triage", "escalate", r#"triage.answers.route.choice == "escalate""#)
        .cond_edge("triage", "ignore", r#"triage.answers.route.choice == "ignore""#)
        .bind("triage", &triage.to_hex())
        .bind("escalate", &escalate.to_hex())
        .bind("ignore", &ignore.to_hex())
        .created_at(600)
        .namespace("ops");
    rig.add(&wf)
}

// ---- C3: the decision node --------------------------------------------------

#[test]
fn a_decision_node_answers_through_the_host_backend_and_edges_branch_on_it() {
    let rig = Rig::new();
    let plan = triage_plan(&rig, Some(json!({"questions": route_questions()})));
    let decider = FakeDecider::new(
        true,
        Box::new(|_| {
            Ok([("route".to_string(), choice("escalate", &[("escalate", 0.9), ("ignore", 0.1)]))]
                .into_iter()
                .collect())
        }),
    );
    let runner = rig.runner(None).with_decider(decider.clone());
    let input = json!({"subject": "prod database down", "from": "oncall"});
    let RunSession::Finished { outcome, .. } =
        runner.start(&plan, "c3-1", input.clone(), &opts()).unwrap()
    else {
        panic!("expected finish")
    };
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!((rig.calls_for("escalate"), rig.calls_for("ignore")), (1, 0), "branched on the answer");
    assert_eq!(rig.calls_for("triage"), 0, "never reached --tool-cmd");

    // What the backend was asked: the node's input as state (it has no
    // `state` key), the frozen questions.
    assert_eq!(decider.calls(), 1);
    let req = decider.request(0);
    assert_eq!(req.state, input);
    assert_eq!(areev_core::decide::questions_to_wire(&req.questions), route_questions());

    // The answer landed under the node's id, provenance and all.
    let ctx = rig.final_context("c3-1");
    assert_eq!(ctx["triage"]["answers"]["route"]["choice"], "escalate");
    assert_eq!(ctx["triage"]["provider"], "fake");
    assert_eq!(ctx["triage"]["calibrated"], true);
    assert_eq!(ctx["triage"]["latency_ms"], 12);

    // Journaled as an ordinary Tool execution grain under the Definition's
    // own name — what run-trace / step-actions / run_outcome read.
    let view = rig.view("c3-1");
    let (key, entry) = view.entries.iter().find(|(k, _)| k.node == "triage").unwrap();
    assert_eq!((key.kind, key.effect_seq), (EffectKind::Tool, 0));
    let (result_hash, outcome) = entry.result.clone().expect("result journaled");
    let grain = rig.facade.with_store(|m| m.get(&result_hash)).unwrap();
    assert_eq!(grain.get_str("tool_name"), Some("triage"));
    assert_eq!(grain.get_str("status"), Some("completed"));
    let EffectOutcome::Completed { input_tokens, output_tokens, .. } = outcome else { panic!() };
    assert_eq!((input_tokens, output_tokens), (40, 2), "the decision's usage is accounted");
    let manifest = rig.facade.with_store(|m| RunManifest::load(m, "c3-1")).unwrap();
    assert_eq!(
        manifest.decider,
        Some(areev_run::DeciderPin { describe: "fake:jev-test".into(), calibrated: true })
    );
    assert_eq!(manifest.pinned[0].executor, "decide");
    assert!(manifest.pinned[0].executor_uri.is_none(), "not a code address");

    // verify is journal replay: it never asks the backend again.
    let report = runner.verify("c3-1").unwrap();
    assert!(report.verified, "{report:?}");
    assert_eq!(decider.calls(), 1, "verify never re-asks");
}

#[test]
fn the_other_answer_takes_the_other_branch() {
    let rig = Rig::new();
    let plan = triage_plan(&rig, Some(json!({"questions": route_questions(), "into": "verdict"})));
    // `into` moves the answer — the edges in this plan read `triage.…`, so
    // with the answer elsewhere neither fires: the run stalls AT triage.
    let decider = FakeDecider::new(
        true,
        Box::new(|_| Ok([("route".to_string(), choice("ignore", &[("escalate", 0.2), ("ignore", 0.8)]))].into_iter().collect())),
    );
    let runner = rig.runner(None).with_decider(decider);
    let RunSession::Finished { outcome, .. } = runner.start(&plan, "c3-into", json!({}), &opts()).unwrap() else {
        panic!()
    };
    assert_eq!(outcome, RunOutcome::Stalled { node: "triage".into() });
    assert_eq!(rig.final_context("c3-into")["verdict"]["answers"]["route"]["choice"], "ignore");

    let rig = Rig::new();
    let plan = triage_plan(&rig, Some(json!({"questions": route_questions()})));
    let decider = FakeDecider::new(
        true,
        Box::new(|_| Ok([("route".to_string(), choice("ignore", &[("escalate", 0.2), ("ignore", 0.8)]))].into_iter().collect())),
    );
    let runner = rig.runner(None).with_decider(decider);
    let RunSession::Finished { outcome, .. } = runner.start(&plan, "c3-2", json!({}), &opts()).unwrap() else {
        panic!()
    };
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!((rig.calls_for("escalate"), rig.calls_for("ignore")), (0, 1));
}

#[test]
fn a_decision_node_without_a_backend_refuses_at_start_with_run_e030() {
    let rig = Rig::new();
    let plan = triage_plan(&rig, Some(json!({"questions": route_questions()})));
    let err = rig.runner(None).start(&plan, "c3-none", json!({}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E030");
    assert!(err.to_string().contains("node 'triage'"), "{err}");
    assert!(
        rig.facade.with_store(|m| RunManifest::load(m, "c3-none")).is_err(),
        "refused before the run existed"
    );
    assert_eq!(rig.calls_for("escalate") + rig.calls_for("ignore"), 0);
}

#[test]
fn resuming_a_decision_node_run_on_a_host_without_a_backend_is_run_e030() {
    // triage → approve (a person), so the run parks after the decision.
    let rig = Rig::new();
    let triage = rig.decide_def("triage", Some(json!({"questions": route_questions()})));
    let approve = rig.add(
        &Tool::new("approve")
            .kind(ToolKind::Definition)
            .executor_kind(ExecutorKind::Client)
            .created_at(500)
            .namespace("ops"),
    );
    let wf = Workflow::new(vec!["triage".into(), "approve".into()])
        .edge("triage", "approve")
        .bind("triage", &triage.to_hex())
        .bind("approve", &approve.to_hex())
        .created_at(600)
        .namespace("ops");
    let plan = rig.add(&wf);
    let decider = FakeDecider::new(
        true,
        Box::new(|_| Ok([("route".to_string(), choice("escalate", &[("escalate", 0.9), ("ignore", 0.1)]))].into_iter().collect())),
    );
    let session = rig.runner(None).with_decider(decider).start(&plan, "c3-park", json!({}), &opts()).unwrap();
    assert!(matches!(session, RunSession::Parked { .. }));
    let err = rig.runner(None).resume("c3-park", &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E030", "{err}");
}

#[test]
fn a_malformed_decide_declaration_refuses_at_start() {
    let rig = Rig::new();
    let one_option = json!({"questions": {"q": {"type": "choice", "instructions": "pick",
                                                "criteria": {"only": "one"}}}});
    let plan = triage_plan(&rig, Some(one_option));
    let decider = FakeDecider::new(true, Box::new(|_| unreachable!()));
    let err = rig.runner(None).with_decider(decider.clone()).start(&plan, "c3-bad", json!({}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E019");
    assert!(err.to_string().contains("DEC-E006"), "{err}");

    let rig = Rig::new();
    let plan = triage_plan(&rig, Some(json!({"into": "$send"})));
    let err = rig.runner(None).with_decider(decider.clone()).start(&plan, "c3-bad2", json!({}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E019", "{err}");
    assert_eq!(decider.calls(), 0);
}

#[test]
fn questions_may_come_from_the_input_and_the_tools_strict_schema_applies() {
    // No frozen questions: the node's input must carry {state, questions}.
    let rig = Rig::new();
    let plan = triage_plan(&rig, None);
    let decider = FakeDecider::new(
        true,
        Box::new(|_| Ok([("route".to_string(), choice("escalate", &[("escalate", 0.7), ("ignore", 0.3)]))].into_iter().collect())),
    );
    let runner = rig.runner(None).with_decider(decider.clone());
    let input = json!({"state": "disk 97% full", "questions": route_questions()});
    let RunSession::Finished { outcome, .. } = runner.start(&plan, "c3-in", input, &opts()).unwrap() else {
        panic!()
    };
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(decider.request(0).state, json!("disk 97% full"), "`state` is taken when present");

    // Neither pinned nor in the input: the strict schema refuses it before
    // anything is asked.
    let RunSession::Finished { outcome, .. } = runner.start(&plan, "c3-missing", json!({"x": 1}), &opts()).unwrap()
    else {
        panic!()
    };
    let RunOutcome::Failed { node, detail } = outcome else { panic!("expected failure, got {outcome:?}") };
    assert_eq!(node, "triage");
    assert!(detail.starts_with("SchemaValidationFailed"), "{detail}");
    assert_eq!(decider.calls(), 1, "nothing asked for the refused input");
}

#[test]
fn a_backend_failure_fails_the_decision_node_through_its_retry_table() {
    let rig = Rig::new();
    let plan = triage_plan(&rig, Some(json!({"questions": route_questions()})));
    let decider = FakeDecider::new(true, Box::new(|_| Err(DecideError::Deadline("2000 ms".into()))));
    let runner = rig.runner(None).with_decider(decider);
    let RunSession::Finished { outcome, .. } = runner.start(&plan, "c3-fail", json!({}), &opts()).unwrap() else {
        panic!()
    };
    let RunOutcome::Failed { node, detail } = outcome else { panic!("{outcome:?}") };
    assert_eq!(node, "triage");
    assert!(detail.starts_with("Timeout: DEC-E004"), "{detail}");
    assert!(runner.verify("c3-fail").unwrap().verified);
}

// ---- C2: the decision-guided fold -------------------------------------------

/// fetch → agent (abstract, offered `fetch`).
fn fold_plan(rig: &Rig) -> Hash {
    let fetch = rig.host_def("fetch");
    let wf = Workflow::new(vec!["fetch".into(), "agent".into()])
        .edge("fetch", "agent")
        .bind("fetch", &fetch.to_hex())
        .created_at(600)
        .namespace("ops");
    rig.add(&wf)
}

fn intent_input(rig: &Rig, run_id: &str, kind: EffectKind, seq: u32) -> Value {
    let view = rig.view(run_id);
    let (_, e) = view
        .entries
        .iter()
        .find(|(k, _)| k.node == "agent" && k.kind == kind && k.effect_seq == seq)
        .unwrap_or_else(|| panic!("no {kind:?} effect at seq {seq}"));
    rig.facade.with_store(|m| m.get(&e.intent)).unwrap().fields["input"].clone()
}

#[test]
fn a_calibrated_decision_prunes_the_transcript_instead_of_summarizing_it() {
    let rig = Rig::new();
    let plan = fold_plan(&rig);
    let llm = ScriptedLlm::new(vec![
        calls_at(&["fetch"], 100),
        calls_at(&["fetch"], 400),
        calls_at(&["fetch"], 950),
        // seq 7, after the decision at seq 6: NOT a summarizer.
        final_at(r#"{"verdict": "done"}"#, 300),
    ]);
    // Window 1..3 = one round (1;2). Drop it whole.
    let decider = FakeDecider::new(
        true,
        Box::new(|req| {
            Ok(req.questions.keys().map(|k| (k.clone(), Answer::Noul { p: 0.1 })).collect())
        }),
    );
    let runner = rig.runner(Some(llm.clone())).with_decider(decider.clone());
    let o = RunOptions { llm_context_tokens: Some(1_000), llm_max_tokens: Some(100), ..opts() };
    let RunSession::Finished { outcome, .. } = runner.start(&plan, "c2-1", json!({}), &o).unwrap() else {
        panic!()
    };
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(llm.offered().len(), 4, "three rounds and a closing turn — no summarizer turn");
    assert!(llm.offered().iter().all(|o| o == &vec!["fetch".to_string()]));

    // What the backend saw: notes, never the results.
    let req = decider.request(0);
    let keys: Vec<&str> = req.questions.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["keep_call_2", "keep_result_2"]);
    assert!(!req.state.to_string().contains("xxxx"), "{}", req.state);
    assert!(req.state.to_string().contains("chars (omitted)"));

    // The decision is an ordinary journaled effect named mg:decide.
    let ask = intent_input(&rig, "c2-1", EffectKind::Tool, 6);
    assert_eq!(ask["decide"]["purpose"], "fold");
    let view = rig.view("c2-1");
    let (_, e) = view.entries.iter().find(|(k, _)| k.node == "agent" && k.effect_seq == 6).unwrap();
    let res = rig.facade.with_store(|m| m.get(&e.result.as_ref().unwrap().0)).unwrap();
    assert_eq!(res.get_str("tool_name"), Some(areev_run::DECIDE_TOOL));

    // The next turn saw the pruned prompt; the journal still has everything.
    let next = intent_input(&rig, "c2-1", EffectKind::Llm, 7);
    let shown = next["messages"].to_string();
    assert!(shown.contains("Areev fold 1 (decision)"), "{shown}");
    assert!(!shown.contains(r#"\"round\":2"#) && !shown.contains(r#""round":2"#), "{shown}");
    let (_, dropped) = view
        .entries
        .iter()
        .find(|(k, _)| k.node == "agent" && k.kind == EffectKind::Tool && k.effect_seq == 1)
        .unwrap();
    assert!(dropped.result.is_some(), "dropped from the prompt, never from the journal");

    // The fold record rides the superstep's decision record.
    let recs: Vec<areev_run_core::FoldRecord> =
        view.checkpoints.iter().flat_map(|c| c.decisions.folds.clone()).collect();
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0].kind, "decide");
    assert_eq!(recs[0].dropped, vec![1, 2]);
    assert_eq!(recs[0].provenance.as_ref().unwrap()["provider"], "fake");
    assert!(recs[0].applied);

    assert!(runner.verify("c2-1").unwrap().verified);
    assert_eq!(decider.calls(), 1, "verify answered the decision from the journal");
}

#[test]
fn an_uncalibrated_backend_leaves_the_summarizer_fold_exactly_as_it_was() {
    let rig = Rig::new();
    let plan = fold_plan(&rig);
    let llm = ScriptedLlm::new(vec![
        calls_at(&["fetch"], 100),
        calls_at(&["fetch"], 400),
        calls_at(&["fetch"], 950),
        final_at("fetched rounds 2 and 3", 950),
        final_at(r#"{"verdict": "done"}"#, 300),
    ]);
    let decider = FakeDecider::new(false, Box::new(|_| unreachable!("never asked")));
    let runner = rig.runner(Some(llm.clone())).with_decider(decider.clone());
    let o = RunOptions { llm_context_tokens: Some(1_000), llm_max_tokens: Some(100), ..opts() };
    let RunSession::Finished { outcome, .. } = runner.start(&plan, "c2-u", json!({}), &o).unwrap() else {
        panic!()
    };
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(decider.calls(), 0);
    assert_eq!(
        intent_input(&rig, "c2-u", EffectKind::Llm, 6)["fold"],
        json!({"from": 1, "to": 3, "seq": 6, "prompt_v": 1}),
        "the summarizer fold at the very seq a no-backend run uses"
    );
    assert!(runner.verify("c2-u").unwrap().verified);
}

// ---- C1: narrowing the offer --------------------------------------------------

#[test]
fn more_than_eight_pinned_tools_are_narrowed_before_the_first_turn() {
    let rig = Rig::new();
    let names: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
    let mut wf = Workflow::new(
        std::iter::once("agent".to_string()).chain(names.iter().cloned()).collect(),
    );
    for n in &names {
        let h = rig.host_def(n);
        wf = wf.edge("agent", n).bind(n, &h.to_hex());
    }
    let plan = rig.add(&wf.created_at(600).namespace("ops"));
    let llm = ScriptedLlm::new(vec![final_at(r#"{"plan": "ok"}"#, 100)]);
    let decider = FakeDecider::new(
        true,
        Box::new(|req| {
            let probs: Vec<(String, f32)> = (0..10)
                .map(|i| (format!("t{i}"), if i < 2 { 0.4 } else { 0.2 / 8.0 }))
                .collect();
            let refs: Vec<(&str, f32)> = probs.iter().map(|(k, p)| (k.as_str(), *p)).collect();
            assert_eq!(req.state["tools"][3]["description"], "does t3", "one line only");
            Ok([("tool".to_string(), choice("t0", &refs))].into_iter().collect())
        }),
    );
    let runner = rig.runner(Some(llm.clone())).with_decider(decider.clone());
    let RunSession::Finished { outcome, .. } = runner.start(&plan, "c1-1", json!({"q": 1}), &opts()).unwrap()
    else {
        panic!()
    };
    assert_eq!(outcome, RunOutcome::Completed);
    // Ten pinned, top eight kept (the rest at 2.5% fall under the 5% floor).
    assert_eq!(llm.offered(), vec![names[..8].to_vec()]);
    let turn = intent_input(&rig, "c1-1", EffectKind::Llm, 1);
    assert_eq!(turn["offer"]["tools"], json!(names[..8]), "journaled beside the turn");
    assert_eq!(turn["offer"]["provider"], "fake");
    assert!(runner.verify("c1-1").unwrap().verified);
    assert_eq!(decider.calls(), 1);
}

#[test]
fn a_failed_narrowing_offers_every_pinned_tool() {
    let rig = Rig::new();
    let names: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
    let mut wf = Workflow::new(
        std::iter::once("agent".to_string()).chain(names.iter().cloned()).collect(),
    );
    for n in &names {
        let h = rig.host_def(n);
        wf = wf.edge("agent", n).bind(n, &h.to_hex());
    }
    let plan = rig.add(&wf.created_at(600).namespace("ops"));
    let llm = ScriptedLlm::new(vec![final_at(r#"{"plan": "ok"}"#, 100)]);
    let decider = FakeDecider::new(
        true,
        Box::new(|_| Err(DecideError::RateLimited { provider: "fake".into(), retry_after_secs: Some(3) })),
    );
    let runner = rig.runner(Some(llm.clone())).with_decider(decider);
    let RunSession::Finished { outcome, .. } = runner.start(&plan, "c1-f", json!({}), &opts()).unwrap() else {
        panic!()
    };
    assert_eq!(outcome, RunOutcome::Completed, "a failed narrowing never fails the node");
    assert_eq!(llm.offered(), vec![names.clone()]);
    assert!(intent_input(&rig, "c1-f", EffectKind::Llm, 1).get("offer").is_none());
    assert!(runner.verify("c1-f").unwrap().verified);
}

/// The whole seam off: a run on a host with no backend asks nothing and
/// pins nothing — the manifest serializes without a `decider` key.
#[test]
fn no_backend_pins_nothing() {
    let rig = Rig::new();
    let plan = fold_plan(&rig);
    let llm = ScriptedLlm::new(vec![final_at(r#"{"ok": true}"#, 10)]);
    let runner = rig.runner(Some(llm));
    runner.start(&plan, "none-1", json!({}), &opts()).unwrap();
    let m = rig.facade.with_store(|m| RunManifest::load(m, "none-1")).unwrap();
    assert!(m.decider.is_none());
    assert!(!serde_json::to_string(&m).unwrap().contains("decider"));
    let _ = RunError::NoDecider { node: String::new() }; // the code exists for hosts to match on
}
