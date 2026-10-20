//! The per-node `llm` block (#391), dynamic tools declared in the run input
//! (`tools_from`, #392) and `locked_params` (#393), end to end over a real
//! store: what the plan declares is frozen into the manifest, sent to the
//! provider, journaled as sent, and replayed by `verify` from the journal
//! alone — the model is never called again.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_llm::{
    ChatMessage, ResponseFormat, StopReason, ToolCallError, ToolCallLlm, ToolCallOut,
    ToolCallRequest, ToolCallResponse, Usage,
};
use areev_run::{
    ExecResult, HostToolExecutor, PreparedCode, RunOptions, RunSession, Runner, ScriptedClock,
};
use areev_run_core::{EffectKind, RunOutcome};
use areev_store::Areev;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// ---- rig -------------------------------------------------------------------

/// Every host execution, as the executor saw it: (name, input, ran as code).
#[derive(Default)]
struct TestExec {
    seen: Mutex<Vec<(String, Value, Option<String>)>>,
    allow_code: bool,
}

impl TestExec {
    fn seen(&self) -> Vec<(String, Value, Option<String>)> {
        self.seen.lock().unwrap().clone()
    }
}

impl HostToolExecutor for TestExec {
    fn execute(&self, tool_name: &str, _hash: &str, input: &Value, _idem: &str) -> ExecResult {
        self.seen.lock().unwrap().push((tool_name.to_string(), input.clone(), None));
        ExecResult::Ok(json!({ "ran": tool_name, "rows": [1, 2, 3] }))
    }
    fn code_allowed(&self, _tool_hash: &str, _uri: &str) -> bool {
        self.allow_code
    }
    fn execute_code(
        &self,
        tool_name: &str,
        _tool_hash: &str,
        code: &PreparedCode,
        input: &Value,
        _idem: &str,
    ) -> ExecResult {
        self.seen
            .lock()
            .unwrap()
            .push((tool_name.to_string(), input.clone(), Some(code.uri.clone())));
        ExecResult::Ok(json!({ "status": 200, "result": {"payee": "ACME"} }))
    }
}

/// One request as the provider received it.
#[derive(Debug, Clone)]
struct Sent {
    system: Option<String>,
    temperature: f64,
    omit_temperature: bool,
    max_tokens: u32,
    response_format: Option<ResponseFormat>,
    parallel_tool_calls: Option<bool>,
    tools: Vec<(String, Option<Value>)>,
    first_user: Value,
    last_user: String,
}

struct CapturingLlm {
    script: Mutex<VecDeque<ToolCallResponse>>,
    sent: Mutex<Vec<Sent>>,
}

impl CapturingLlm {
    fn new(script: Vec<ToolCallResponse>) -> Arc<Self> {
        Arc::new(CapturingLlm { script: Mutex::new(script.into()), sent: Mutex::new(Vec::new()) })
    }
    fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
}

impl ToolCallLlm for CapturingLlm {
    fn model(&self) -> &str {
        "gpt-5-mini"
    }
    fn provider(&self) -> &'static str {
        "openai"
    }
    fn call(&self, req: &ToolCallRequest<'_>) -> Result<ToolCallResponse, ToolCallError> {
        let users: Vec<&String> = req
            .messages
            .iter()
            .filter_map(|m| match m {
                ChatMessage::User(s) => Some(s),
                _ => None,
            })
            .collect();
        self.sent.lock().unwrap().push(Sent {
            system: req.system.clone(),
            temperature: req.temperature,
            omit_temperature: req.omit_temperature,
            max_tokens: req.max_tokens,
            response_format: req.response_format.clone(),
            parallel_tool_calls: req.parallel_tool_calls,
            tools: req.tools.iter().map(|t| (t.tool_name.clone(), t.input_schema.clone())).collect(),
            first_user: users
                .first()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null),
            last_user: users.last().map(|s| s.to_string()).unwrap_or_default(),
        });
        self.script.lock().unwrap().pop_front().ok_or(ToolCallError {
            retryable: false,
            message: "script exhausted".into(),
            ..Default::default()
        })
    }
}

fn calls(calls: Vec<(&str, &str, Value)>) -> ToolCallResponse {
    ToolCallResponse::new(
        None,
        calls
            .into_iter()
            .map(|(id, name, arguments)| ToolCallOut {
                id: id.into(),
                name: name.into(),
                arguments,
                arguments_raw: None,
            })
            .collect(),
        StopReason::ToolUse,
        Usage { input_tokens: 10, output_tokens: 5, cache_read_tokens: None },
    )
}

fn answer(text: &str) -> ToolCallResponse {
    ToolCallResponse::new(
        Some(text.into()),
        vec![],
        StopReason::EndTurn,
        Usage { input_tokens: 20, output_tokens: 7, cache_read_tokens: None },
    )
}

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig { _dir: dir, facade: Arc::new(AreevFacade::new(m)) }
    }

    fn def(&self, t: Tool) -> String {
        let t = t.kind(ToolKind::Definition).created_at(500).namespace("ops");
        self.facade.with_store(|m| m.add(&t)).unwrap().to_hex()
    }

    fn blob(&self, bytes: &[u8]) -> String {
        self.facade.with_store(|m| m.put_blob(bytes)).unwrap()
    }

    /// `edges` as (src, dst, cond); `llm` is the plan's `llm` field.
    fn plan(
        &self,
        nodes: &[&str],
        edges: &[(&str, &str, Option<&str>)],
        bindings: &[(&str, &str)],
        llm: Value,
    ) -> Hash {
        let mut wf = Workflow::new(nodes.iter().map(|s| s.to_string()).collect());
        for (a, b, c) in edges {
            wf = match c {
                Some(c) => wf.cond_edge(a, b, c),
                None => wf.edge(a, b),
            };
        }
        for (n, h) in bindings {
            wf = wf.bind(n, h);
        }
        let mut wf = wf.created_at(600).namespace("ops");
        if !llm.is_null() {
            wf.common.extra_fields.insert("llm".into(), llm);
        }
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn runner(&self, exec: Arc<TestExec>, llm: Arc<CapturingLlm>) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new((0..400).map(|i| 1_756_000_000_000 + i * 10).collect())),
            executor: exec as Arc<dyn HostToolExecutor>,
            llm: Some(llm as Arc<dyn ToolCallLlm>),
            observer: None,
            ns: "ops".into(),
            principal: "user:runner".into(),
        }
    }

    fn context(&self, run_id: &str) -> Value {
        self.facade
            .with_store(|m| areev_run::journal::load(m, "ops", run_id))
            .unwrap()
            .checkpoints
            .last()
            .unwrap()
            .scheduler["context"]
            .clone()
    }

    /// The journaled intent INPUT of every effect of `node`, in key order.
    fn intent_inputs(&self, run_id: &str, node: &str) -> Vec<(u32, EffectKind, String, Value)> {
        let view = self.facade.with_store(|m| areev_run::journal::load(m, "ops", run_id)).unwrap();
        view.entries
            .values()
            .filter(|e| e.key.node == node)
            .map(|e| {
                let g = self.facade.with_store(|m| m.get(&e.intent)).unwrap();
                (
                    e.key.effect_seq,
                    e.key.kind,
                    g.get_str("tool_name").unwrap_or_default().to_string(),
                    g.fields.get("input").cloned().unwrap_or(Value::Null),
                )
            })
            .collect()
    }

    fn manifest_exists(&self, run_id: &str) -> bool {
        self.facade.with_store(|m| areev_run::RunManifest::load(m, run_id)).is_ok()
    }
}

fn opts() -> RunOptions {
    RunOptions { workers: 2, ..Default::default() }
}

/// Every run in this file completes; anything else is the failure.
fn finished(s: RunSession) -> RunOutcome {
    match s {
        RunSession::Finished { outcome, .. } => {
            assert_eq!(outcome, RunOutcome::Completed);
            outcome
        }
        other => panic!("expected a finished run, got {other:?}"),
    }
}

/// `lookup → reply (abstract) → gate`, the shape #391 measured: before the
/// block, `reply` was offered `gate` as a tool and saw the whole state.
fn three_step(rig: &Rig, llm: Value) -> Hash {
    let lookup = rig.def(
        Tool::new("lookup").tool_description("look a record up").input_schema(json!({
            "type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]
        })),
    );
    let gate = rig.def(Tool::new("gate").tool_description("final check"));
    rig.plan(
        &["lookup", "reply", "gate"],
        &[("lookup", "reply", None), ("reply", "gate", None)],
        &[("lookup", &lookup), ("gate", &gate)],
        llm,
    )
}

// ---- #391 ------------------------------------------------------------------

#[test]
fn the_block_is_frozen_sent_journaled_and_replayed_without_the_model() {
    let rig = Rig::new();
    let schema = json!({"type": "object", "properties": {"answer": {"type": "string"}},
                        "required": ["answer"], "additionalProperties": false});
    let plan = three_step(
        &rig,
        json!({"reply": {
            "system": "You are the accounts-payable assistant.",
            "instruction_from": "/prompt",
            "state": ["question"],
            "tools": ["lookup"],
            "response": {"json_schema": schema},
            "temperature": "omit",
            "max_tokens": 300,
            "parallel_tool_calls": false,
            "tool_results_into": "tool_results",
        }}),
    );
    let exec = Arc::new(TestExec::default());
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "lookup", json!({"q": "4471"}))]),
        answer(r#"{"answer": "paid on 2026-10-01"}"#),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    let input = json!({"prompt": "Was invoice 4471 paid?", "question": "4471", "bulk": "x".repeat(400)});
    assert_eq!(finished(runner.start(&plan, "r1", input, &opts()).unwrap()), RunOutcome::Completed);

    // Sent: the node's own system text, no temperature, its ceiling, the
    // schema, one call per turn, and ONLY the subset it names.
    let sent = llm.sent();
    assert_eq!(sent.len(), 2);
    for s in &sent {
        assert_eq!(s.system.as_deref(), Some("You are the accounts-payable assistant."));
        assert!(s.omit_temperature, "temperature: omit sends none");
        assert_eq!(s.max_tokens, 300);
        assert_eq!(s.parallel_tool_calls, Some(false));
        assert_eq!(
            s.response_format,
            Some(ResponseFormat::JsonSchema { name: "reply".into(), schema: schema.clone() })
        );
        let names: Vec<&str> = s.tools.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["lookup"], "the gate is never offered");
    }
    assert_eq!(
        sent[0].first_user,
        json!({"instruction": "Was invoice 4471 paid?", "state": {"question": "4471"}}),
        "the instruction came from the input; the bulky key stayed out of the prompt"
    );

    // The gate below sees what the model read, from the journal.
    let gate_input = exec.seen().into_iter().find(|(n, _, _)| n == "gate").unwrap().1;
    let log = gate_input["tool_results"].as_array().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["name"], "lookup");
    assert_eq!(log[0]["input"], json!({"q": "4471"}));
    assert_eq!(log[0]["result"]["ran"], "lookup");
    assert_eq!(log[0]["is_error"], false);
    assert_eq!(log[0]["effect_seq"], 1);
    assert_eq!(gate_input["answer"], "paid on 2026-10-01");

    // Journaled as sent: the mg:llm intent records the system text, the
    // instruction and response_format in the wire shape.
    let turns: Vec<Value> = rig
        .intent_inputs("r1", "reply")
        .into_iter()
        .filter(|(_, k, _, _)| *k == EffectKind::Llm)
        .map(|(_, _, name, input)| {
            assert_eq!(name, "mg:llm");
            input
        })
        .collect();
    assert_eq!(turns[0]["llm"]["system"], "You are the accounts-payable assistant.");
    assert_eq!(turns[0]["llm"]["temperature"], "omit");
    assert_eq!(turns[0]["llm"]["response_format"]["type"], "json_schema");
    assert_eq!(turns[0]["llm"]["response_format"]["json_schema"]["schema"], schema);
    assert_eq!(turns[0]["messages"][0]["content"]["instruction"], "Was invoice 4471 paid?");

    // inspect shows the frozen block under the node's pin.
    let report = runner.inspect("r1").unwrap();
    let row = report.pinned.iter().find(|r| r["node"] == "reply").unwrap();
    assert_eq!(row["llm"]["tools"], json!(["lookup"]));
    assert_eq!(row["llm"]["instruction_from"], "/prompt");
    assert_eq!(row["llm"]["max_tokens"], 300);
    assert!(row["llm"].get("narrow").is_none(), "the default rule is stored as nothing");

    // verify replays byte-identically, never calling the model.
    let report = runner.verify("r1").unwrap();
    assert!(report.verified, "{:?}", report.steps);
    assert_eq!(llm.sent().len(), 2, "verify never calls the model");
}

#[test]
fn a_pinned_tool_the_node_was_not_offered_is_the_unknown_tool_reprompt() {
    let rig = Rig::new();
    let plan = three_step(&rig, json!({"reply": {"tools": ["lookup"]}}));
    let exec = Arc::new(TestExec::default());
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "gate", json!({}))]),
        answer(r#"{"answer": "ok"}"#),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    assert_eq!(finished(runner.start(&plan, "r1", json!({}), &opts()).unwrap()), RunOutcome::Completed);
    let sent = llm.sent();
    assert!(sent[1].last_user.contains("Unknown tool"), "{}", sent[1].last_user);
    assert!(sent[1].last_user.contains(r#"choose among: ["lookup"]"#), "{}", sent[1].last_user);
    assert_eq!(
        exec.seen().iter().filter(|(n, _, _)| n == "gate").count(),
        1,
        "the gate ran once — as its own step, never as the model's call"
    );
    assert!(runner.verify("r1").unwrap().verified);
}

#[test]
fn a_numeric_temperature_is_sent_and_no_block_sends_what_it_always_did() {
    let rig = Rig::new();
    let plan = three_step(&rig, json!({"reply": {"temperature": 0.4}}));
    let llm = CapturingLlm::new(vec![answer(r#"{"answer": "ok"}"#)]);
    let runner = rig.runner(Arc::new(TestExec::default()), Arc::clone(&llm));
    finished(runner.start(&plan, "r1", json!({}), &opts()).unwrap());
    let s = &llm.sent()[0];
    assert_eq!((s.temperature, s.omit_temperature), (0.4, false));
    assert_eq!(s.max_tokens, 1024, "the run's default ceiling still applies");

    let plan = three_step(&rig, Value::Null);
    let llm = CapturingLlm::new(vec![answer(r#"{"answer": "ok"}"#)]);
    let runner = rig.runner(Arc::new(TestExec::default()), Arc::clone(&llm));
    finished(runner.start(&plan, "r2", json!({"q": 1}), &opts()).unwrap());
    let s = &llm.sent()[0];
    assert!(s.system.as_deref().unwrap().starts_with("You are executing one node of a governed workflow."));
    assert_eq!((s.temperature, s.omit_temperature), (0.0, false));
    assert_eq!(s.response_format, None);
    assert_eq!(s.parallel_tool_calls, None);
    assert_eq!(s.tools.len(), 2, "every pinned host tool, as before");
    assert_eq!(s.first_user["instruction"], "reply");
    assert_eq!(s.first_user["state"]["q"], 1, "the whole state, as before");
    assert_eq!(s.first_user["state"]["ran"], "lookup");
    let turn = &rig.intent_inputs("r2", "reply")[0].3;
    assert!(turn.get("llm").is_none(), "no block, nothing new in the journal: {turn}");
    let row = runner.inspect("r2").unwrap().pinned.into_iter().find(|r| r["node"] == "reply").unwrap();
    assert!(row.get("llm").is_none());
}

#[test]
fn a_malformed_block_is_refused_at_start_naming_the_node() {
    let rig = Rig::new();
    let cases: Vec<(Value, &str, &str)> = vec![
        (json!({"reply": {"tools": ["nope"]}}), "RUN-E004", "'nope'"),
        (json!({"lookup": {"system": "x"}}), "RUN-E019", "'lookup'"),
        (json!({"ghost": {"system": "x"}}), "RUN-E019", "'ghost'"),
        (json!({"reply": {"sytem": "x"}}), "RUN-E019", "\"sytem\""),
        (json!({"reply": {"instruction": "a", "instruction_from": "/b"}}), "RUN-E019", "exclusive"),
        (json!({"reply": {"instruction_from": "prompt"}}), "RUN-E019", "JSON pointer"),
        (json!({"reply": {"state": "some"}}), "RUN-E019", "`state`"),
        (json!({"reply": {"response": {"json": false}}}), "RUN-E019", "`response`"),
        (json!({"reply": {"response": {"json_schema": {"type": "string"}}}}), "RUN-E019", "json_schema"),
        (json!({"reply": {"temperature": "cold"}}), "RUN-E019", "`temperature`"),
        (json!({"reply": {"max_tokens": 0}}), "RUN-E019", "`max_tokens`"),
        (json!({"reply": {"parallel_tool_calls": "no"}}), "RUN-E019", "parallel_tool_calls"),
        (json!({"reply": {"tool_results_into": "$x"}}), "RUN-E019", "tool_results_into"),
        (json!({"reply": {"narrow": {"top_k": 0}}}), "RUN-E019", "`narrow`"),
        (json!({"reply": {"narrow": "no"}}), "RUN-E019", "`narrow`"),
    ];
    for (i, (block, code, needle)) in cases.into_iter().enumerate() {
        let plan = three_step(&rig, block.clone());
        let runner = rig.runner(Arc::new(TestExec::default()), CapturingLlm::new(vec![]));
        let run_id = format!("bad-{i}");
        let err = runner.start(&plan, &run_id, json!({}), &opts()).unwrap_err();
        assert_eq!(err.code(), code, "{block}: {err}");
        assert!(err.to_string().contains(needle), "{block}: {err}");
        assert!(!rig.manifest_exists(&run_id), "{block}: refused before the run exists");
    }
}

// ---- #393 ------------------------------------------------------------------

#[test]
fn locked_params_win_over_the_model_and_leave_the_offered_schema() {
    let rig = Rig::new();
    let post = rig.def(
        Tool::new("post")
            .tool_description("post a message")
            .input_schema(json!({"type": "object",
                "properties": {"url": {"type": "string"}, "body": {"type": "string"}},
                "required": ["url", "body"], "additionalProperties": false}))
            .strict(true)
            .locked_params(json!({"url": "https://a.example/x"})),
    );
    // `post` is pinned by a step on a branch that does not fire, so it is
    // offered to the agent and never runs as a step of its own.
    let done = rig.def(Tool::new("done"));
    let plan = rig.plan(
        &["agent", "done", "post"],
        &[("agent", "done", None), ("agent", "post", Some("never == true"))],
        &[("post", &post), ("done", &done)],
        Value::Null,
    );
    let exec = Arc::new(TestExec::default());
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "post", json!({"url": "https://b.example/y", "body": "hi"}))]),
        answer(r#"{"posted": true}"#),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    finished(runner.start(&plan, "r1", json!({}), &opts()).unwrap());

    let offered = llm.sent()[0].tools.iter().find(|(n, _)| n == "post").unwrap().1.clone().unwrap();
    assert!(offered["properties"].get("url").is_none(), "{offered}");
    assert_eq!(offered["required"], json!(["body"]));

    let (name, input, _) = exec.seen().into_iter().find(|(n, _, _)| n == "post").unwrap();
    assert_eq!(name, "post");
    assert_eq!(input, json!({"url": "https://a.example/x", "body": "hi"}), "the lock wins");
    let tool_intent = rig
        .intent_inputs("r1", "agent")
        .into_iter()
        .find(|(_, k, _, _)| *k == EffectKind::Tool)
        .unwrap();
    assert_eq!(tool_intent.3["url"], "https://a.example/x", "the merged input is journaled");

    let row = runner.inspect("r1").unwrap().pinned.into_iter().find(|r| r["node"] == "post").unwrap();
    assert_eq!(row["locked_params"], json!({"url": "https://a.example/x"}));
    assert!(runner.verify("r1").unwrap().verified);
}

#[test]
fn a_strict_definition_judges_the_merged_input() {
    let rig = Rig::new();
    // `body` is required and the model omits it: the merged input still
    // lacks it, so the strict check re-prompts — while `url`, required but
    // locked, is never reported missing.
    let post = rig.def(
        Tool::new("post")
            .input_schema(json!({"type": "object",
                "properties": {"url": {"type": "string"}, "body": {"type": "string"}},
                "required": ["url", "body"]}))
            .strict(true)
            .locked_params(json!({"url": "https://a.example/x"})),
    );
    let done = rig.def(Tool::new("done"));
    let plan = rig.plan(
        &["agent", "done", "post"],
        &[("agent", "done", None), ("agent", "post", Some("never == true"))],
        &[("post", &post), ("done", &done)],
        Value::Null,
    );
    let exec = Arc::new(TestExec::default());
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "post", json!({}))]),
        calls(vec![("c2", "post", json!({"body": "hi"}))]),
        answer("{\"ok\": true}"),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    finished(runner.start(&plan, "r1", json!({}), &opts()).unwrap());
    let sent = llm.sent();
    assert!(sent[1].last_user.contains("failed validation"), "{}", sent[1].last_user);
    assert_eq!(
        exec.seen().iter().filter(|(n, _, _)| n == "post").count(),
        1,
        "only the corrected call dispatched"
    );
    assert!(runner.verify("r1").unwrap().verified);
}

// ---- #392 ------------------------------------------------------------------

/// A plan pinning an `mcp.call`-shaped capability Definition (by a step on a
/// branch that never fires) and an agent whose tools come from the input.
fn mcp_plan(rig: &Rig, llm: Value) -> (Hash, String) {
    let uri = rig.blob(b"\0asm-stand-in");
    let mcp = rig.def(
        Tool::new("mcp.call")
            .tool_description("call a tool on an MCP server")
            .input_schema(json!({"type": "object", "properties": {
                "url": {"type": "string"}, "tool": {"type": "string"},
                "credential": {"type": "string"}, "arguments": {"type": "object"}},
                "required": ["url", "tool"]}))
            .strict(true)
            .executor_uri(&uri),
    );
    let done = rig.def(Tool::new("done"));
    let plan = rig.plan(
        &["agent", "done", "mcp.call"],
        &[("agent", "done", None), ("agent", "mcp.call", Some("never == true"))],
        &[("mcp.call", &mcp), ("done", &done)],
        llm,
    );
    (plan, mcp)
}

fn paid_to(definition: &str) -> Value {
    json!({
        "name": "paid_to",
        "description": "Who an invoice was paid to",
        "input_schema": {"type": "object", "properties": {"invoice": {"type": "string"}},
                         "required": ["invoice"], "additionalProperties": false},
        "dispatch": {"definition": definition,
                     "locked": {"url": "https://mcp.example/mcp", "tool": "paid_to", "credential": "gateway"},
                     "args_into": "arguments"},
    })
}

#[test]
fn dynamic_tools_are_pinned_offered_by_their_own_names_and_dispatched_through_the_definition() {
    let rig = Rig::new();
    let (plan, mcp_hash) = mcp_plan(&rig, json!({"agent": {"tools": [], "tools_from": "/tools"}}));
    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "paid_to", json!({"invoice": "4471"}))]),
        answer(r#"{"payee": "ACME"}"#),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    let input = json!({"tools": [paid_to("mcp.call")]});
    assert_eq!(finished(runner.start(&plan, "r1", input, &opts()).unwrap()), RunOutcome::Completed);

    // Offered by its own name and schema — and `mcp.call` itself not at all.
    let sent = llm.sent();
    assert_eq!(sent[0].tools.len(), 1);
    assert_eq!(sent[0].tools[0].0, "paid_to");
    assert_eq!(sent[0].tools[0].1.as_ref().unwrap()["required"], json!(["invoice"]));

    // Executed AS the Definition (its name, its pinned blob), locks set.
    let seen = exec.seen();
    assert!(!seen.iter().any(|(n, _, _)| n == "paid_to"), "nothing executes under the dynamic name");
    let (name, input, code) = seen.into_iter().find(|(n, _, _)| n != "done").unwrap();
    assert_eq!(name, "mcp.call", "the host executor and the broker know the Definition's name");
    assert!(code.is_some(), "the pinned blob ran — not --tool-cmd");
    assert_eq!(
        input,
        json!({"url": "https://mcp.example/mcp", "tool": "paid_to", "credential": "gateway",
               "arguments": {"invoice": "4471"}})
    );

    // Journaled under the dynamic name, with the Definition's address.
    let view = rig.facade.with_store(|m| areev_run::journal::load(m, "ops", "r1")).unwrap();
    let entry = view
        .entries
        .values()
        .find(|e| e.key.node == "agent" && e.key.kind == EffectKind::Tool)
        .unwrap();
    let g = rig.facade.with_store(|m| m.get(&entry.intent)).unwrap();
    assert_eq!(g.get_str("tool_name"), Some("paid_to"));
    assert_eq!(g.get_str("spec_hash"), Some(mcp_hash.as_str()));
    let (result, _) = entry.result.as_ref().unwrap();
    let rg = rig.facade.with_store(|m| m.get(result)).unwrap();
    assert_eq!(rg.get_str("tool_name"), Some("paid_to"), "the result re-states the intent's name");

    // inspect shows the pinned list.
    let row = runner.inspect("r1").unwrap().pinned.into_iter().find(|r| r["node"] == "agent").unwrap();
    assert_eq!(row["llm"]["dynamic_tools"][0]["name"], "paid_to");
    assert_eq!(row["llm"]["dynamic_tools"][0]["dispatch"]["definition"], "mcp.call");
    assert_eq!(row["llm"]["dynamic_tools"][0]["dispatch"]["hash"], mcp_hash);

    // verify replays from the journal alone: the model and the tool are
    // never called again.
    let before = exec.seen().len();
    assert!(runner.verify("r1").unwrap().verified);
    assert_eq!(exec.seen().len(), before);
    assert_eq!(llm.sent().len(), 2);
}

#[test]
fn a_dispatch_by_hash_reaches_the_same_definition() {
    let rig = Rig::new();
    let (plan, mcp_hash) = mcp_plan(&rig, json!({"agent": {"tools_from": "/tools"}}));
    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![answer("{}")]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    finished(runner.start(&plan, "r1", json!({"tools": [paid_to(&mcp_hash)]}), &opts()).unwrap());
    let names: Vec<String> = llm.sent()[0].tools.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(names, vec!["done", "mcp.call", "paid_to"], "no `tools` subset: every pin, then the list");
}

#[test]
fn a_bad_tools_list_is_refused_at_start_naming_the_entry() {
    let rig = Rig::new();
    let (plan, _) = mcp_plan(&rig, json!({"agent": {"tools_from": "/tools"}}));
    let mut dup = paid_to("mcp.call");
    dup["name"] = json!("mcp_call");
    let cases: Vec<(Value, &str, &str)> = vec![
        (json!({"tools": [paid_to("http.call")]}), "RUN-E004", "'http.call'"),
        (json!({"tools": [paid_to("mcp.call"), paid_to("mcp.call")]}), "RUN-E019", "duplicate"),
        (json!({"tools": [dup]}), "RUN-E019", "collides"),
        (json!({}), "RUN-E019", "resolves to nothing"),
        (json!({"tools": {"a": 1}}), "RUN-E019", "not a list"),
        (json!({"tools": [{"name": "x", "dispatch": {"definition": "mcp.call"}, "inputSchema": {}}]}),
         "RUN-E019", "\"inputSchema\""),
        (json!({"tools": [{"name": "x y", "dispatch": {"definition": "mcp.call"}}]}), "RUN-E019", "valid tool name"),
        (json!({"tools": [{"name": "x", "input_schema": {"type": "array"}, "dispatch": {"definition": "mcp.call"}}]}),
         "RUN-E019", "input_schema"),
        (json!({"tools": [{"name": "x", "dispatch": {"definition": "mcp.call", "locked": [1]}}]}),
         "RUN-E019", "locked"),
    ];
    for (i, (input, code, needle)) in cases.into_iter().enumerate() {
        let runner = rig.runner(Arc::new(TestExec { allow_code: true, ..Default::default() }), CapturingLlm::new(vec![]));
        let run_id = format!("bad-{i}");
        let err = runner.start(&plan, &run_id, input.clone(), &opts()).unwrap_err();
        assert_eq!(err.code(), code, "{input}: {err}");
        assert!(err.to_string().contains(needle), "{input}: {err}");
        assert!(err.to_string().contains("'agent'"), "names the node: {err}");
        assert!(!rig.manifest_exists(&run_id));
    }
}

#[test]
fn a_dynamic_tools_own_schema_is_enforced_with_a_reprompt() {
    let rig = Rig::new();
    let (plan, _) = mcp_plan(&rig, json!({"agent": {"tools_from": "/tools", "tool_results_into": "seen"}}));
    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "paid_to", json!({"invoice": 4471}))]),
        calls(vec![("c2", "paid_to", json!({"invoice": "4471"}))]),
        answer(r#"{"payee": "ACME"}"#),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    finished(runner.start(&plan, "r1", json!({"tools": [paid_to("mcp.call")]}), &opts()).unwrap());
    assert!(llm.sent()[1].last_user.contains("failed validation"));
    assert_eq!(exec.seen().iter().filter(|(n, _, _)| n == "mcp.call").count(), 1);
    let ctx = rig.context("r1");
    assert_eq!(ctx["seen"][0]["name"], "paid_to");
    assert_eq!(ctx["seen"][0]["input"]["arguments"], json!({"invoice": "4471"}));
    assert_eq!(ctx["seen"][0]["result"]["result"]["payee"], "ACME");
    assert!(runner.verify("r1").unwrap().verified);
}

#[test]
fn a_resume_uses_the_pinned_block_not_the_live_input() {
    // Park on an approval BEFORE the agent runs, then change everything a
    // re-resolution would read — supersede the plan with a different block,
    // and publish a newer `mcp_call` under the same name — and resume. The
    // agent must run the block, the tool list and the Definition frozen at
    // start.
    let rig = Rig::new();
    let v1_uri = rig.blob(b"\0asm-v1");
    let v1 = Tool::new("mcp_call")
        .kind(ToolKind::Definition)
        .input_schema(json!({"type": "object", "properties": {"url": {"type": "string"},
            "credential": {"type": "string"}, "tool": {"type": "string"}, "arguments": {"type": "object"}}}))
        .executor_uri(&v1_uri)
        .created_at(500)
        .namespace("ops");
    rig.facade.with_store(|m| m.add(&v1)).unwrap();
    let approve = rig.def(Tool::new("approve").executor_kind(areev_core::types::ExecutorKind::Client));
    let block = |system: &str| json!({"agent": {"system": system, "dispatch": ["mcp_call"], "tools": [], "tools_from": "/tools"}});
    let mut wf = Workflow::new(vec!["approve".into(), "agent".into()])
        .edge("approve", "agent")
        .bind("approve", &approve)
        .created_at(600)
        .namespace("ops");
    wf.common.extra_fields.insert("llm".into(), block("S1"));
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();

    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "bank_paid_to", json!({"invoice": "4471"}))]),
        answer("{}"),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    let RunSession::Parked { envelope, .. } =
        runner.start(&plan, "pr1", json!({"tools": [bank_paid_to()]}), &opts()).unwrap()
    else {
        panic!("parks on the approval")
    };
    assert!(llm.sent().is_empty(), "the agent has not run yet");

    // The live world moves on.
    let mut wf2 = Workflow::new(vec!["approve".into(), "agent".into()])
        .edge("approve", "agent")
        .bind("approve", &approve)
        .created_at(700)
        .namespace("ops");
    wf2.common.extra_fields.insert("llm".into(), block("S2"));
    rig.facade.with_store(|m| m.supersede(&plan, &mut wf2)).unwrap();
    let v2_uri = rig.blob(b"\0asm-v2");
    let v2 = Tool::new("mcp_call")
        .kind(ToolKind::Definition)
        .executor_uri(&v2_uri)
        .created_at(900)
        .namespace("ops");
    rig.facade.with_store(|m| m.add(&v2)).unwrap();

    let ask = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();
    runner.respond("pr1", &ask, json!({"ok": true}), false, "user:officer").unwrap();
    finished(runner.resume("pr1", &opts()).unwrap());

    let sent = llm.sent();
    assert_eq!(sent[0].system.as_deref(), Some("S1"), "the frozen block, not the superseding plan's");
    assert_eq!(sent[0].tools[0].0, "bank.paid_to", "the frozen tool list");
    let (_, input, code) = exec.seen().into_iter().find(|(n, _, _)| n == "mcp_call").unwrap();
    assert_eq!(code.as_deref(), Some(v1_uri.as_str()), "the Definition pinned at start, not the newer one");
    assert_eq!(input["arguments"], json!({"invoice": "4471"}));
    assert!(runner.verify("pr1").unwrap().verified);
}

#[test]
fn an_abstract_nodes_call_to_a_code_carrying_definition_runs_its_blob() {
    // The lookup a model-issued call is dispatched through used to search by
    // NODE, find the abstract node's own pin, and send the call to
    // `--tool-cmd`: a capability Definition offered to an agent never ran.
    let rig = Rig::new();
    let (plan, _) = mcp_plan(&rig, Value::Null);
    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "mcp_call", json!({"url": "https://mcp.example/mcp", "tool": "t"}))]),
        answer("{}"),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    finished(runner.start(&plan, "r1", json!({}), &opts()).unwrap());
    let (_, _, code) = exec.seen().into_iter().find(|(n, _, _)| n == "mcp.call").unwrap();
    assert!(code.is_some(), "ran as its pinned code");
}

#[test]
fn every_dispatch_of_a_mixed_run_journals_under_its_kind() {
    // Belt and braces for the journal's name rule across all three tool
    // shapes in one attempt: a pinned tool, a locked pinned tool and a
    // dynamic tool each journal under the name the model called.
    let rig = Rig::new();
    let (plan, _) = mcp_plan(&rig, json!({"agent": {"tools_from": "/tools"}}));
    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![
        calls(vec![
            ("c1", "paid_to", json!({"invoice": "1"})),
            ("c2", "mcp_call", json!({"url": "u", "tool": "t"})),
        ]),
        answer("{}"),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    finished(runner.start(&plan, "r1", json!({"tools": [paid_to("mcp.call")]}), &opts()).unwrap());
    let names: Vec<String> = rig
        .intent_inputs("r1", "agent")
        .into_iter()
        .filter(|(_, k, _, _)| *k == EffectKind::Tool)
        .map(|(_, _, n, _)| n)
        .collect();
    assert_eq!(names, vec!["paid_to".to_string(), "mcp.call".to_string()]);
    assert!(runner.verify("r1").unwrap().verified);
}

#[test]
fn a_run_using_every_key_at_once_verifies_from_the_journal_alone() {
    let rig = Rig::new();
    let uri = rig.blob(b"\0asm-stand-in");
    let mcp = rig.def(
        Tool::new("mcp.call")
            .input_schema(json!({"type": "object", "properties": {
                "url": {"type": "string"}, "tool": {"type": "string"}, "arguments": {"type": "object"}},
                "required": ["url", "tool"]}))
            .strict(true)
            .executor_uri(&uri)
            // The Definition's own lock sits over the entry's.
            .locked_params(json!({"credential": "gateway"})),
    );
    let post = rig.def(
        Tool::new("post")
            .input_schema(json!({"type": "object",
                "properties": {"url": {"type": "string"}, "body": {"type": "string"}},
                "required": ["url", "body"]}))
            .strict(true)
            .locked_params(json!({"url": "https://a.example/x"})),
    );
    let gate = rig.def(Tool::new("gate"));
    let plan = rig.plan(
        &["agent", "gate", "mcp.call", "post"],
        &[
            ("agent", "gate", None),
            ("agent", "mcp.call", Some("never == true")),
            ("agent", "post", Some("never == true")),
        ],
        &[("mcp.call", &mcp), ("post", &post), ("gate", &gate)],
        json!({"agent": {
            "system": "Persona.",
            "instruction_from": "/ask",
            "state": ["ask"],
            "tools": ["post"],
            "tools_from": "/tools",
            "response": {"json": true},
            "temperature": "omit",
            "max_tokens": 222,
            "parallel_tool_calls": true,
            "tool_results_into": "read",
        }}),
    );
    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![
        calls(vec![
            ("c1", "paid_to", json!({"invoice": "4471"})),
            ("c2", "post", json!({"url": "https://evil.example", "body": "b"})),
        ]),
        answer("prose first"),
        answer(r#"{"payee": "ACME"}"#),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    let input = json!({"ask": "Who was 4471 paid to?", "tools": [{
        "name": "paid_to",
        "input_schema": {"type": "object", "properties": {"invoice": {"type": "string"}}, "required": ["invoice"]},
        "dispatch": {"definition": "mcp.call", "locked": {"url": "https://mcp.example/mcp", "tool": "paid_to",
                     "credential": "the-entry-loses"}, "args_into": "arguments"},
    }]});
    finished(runner.start(&plan, "all-1", input, &opts()).unwrap());

    let sent = llm.sent();
    assert_eq!(sent.len(), 3, "two turns plus one corrective re-prompt for the prose answer");
    let names: Vec<&str> = sent[0].tools.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["post", "paid_to"]);
    assert_eq!(sent[0].response_format, Some(ResponseFormat::JsonObject));
    assert_eq!(sent[0].parallel_tool_calls, Some(true));
    assert_eq!(sent[0].max_tokens, 222);
    assert!(sent[0].omit_temperature);
    let seen = exec.seen();
    let mcp_input = &seen.iter().find(|(n, _, _)| n == "mcp.call").unwrap().1;
    assert_eq!(mcp_input["credential"], "gateway", "the Definition's lock wins over the entry's");
    assert_eq!(mcp_input["arguments"], json!({"invoice": "4471"}));
    let post_input = &seen.iter().find(|(n, _, _)| n == "post").unwrap().1;
    assert_eq!(post_input["url"], "https://a.example/x");
    let gate_input = &seen.iter().find(|(n, _, _)| n == "gate").unwrap().1;
    assert_eq!(gate_input["payee"], "ACME");
    let read = gate_input["read"].as_array().unwrap();
    assert_eq!(read.len(), 2);
    assert_eq!((read[0]["name"].as_str(), read[1]["name"].as_str()), (Some("paid_to"), Some("post")));

    let report = runner.verify("all-1").unwrap();
    assert!(report.verified, "{:?}", report.steps);
    assert_eq!(llm.sent().len(), 3, "verify never calls the model");
    assert_eq!(exec.seen().len(), seen.len(), "nor any tool");
}

#[test]
fn a_parked_run_resumes_under_the_block_it_started_with() {
    // An approval first, then the agent: the block the agent runs under after
    // a resume is the one frozen at start — whatever input the resume call
    // could have carried, it carries none, and the plan is never re-read.
    let rig = Rig::new();
    let approve = rig.def(Tool::new("approve").executor_kind(areev_core::types::ExecutorKind::Client));
    let lookup = rig.def(Tool::new("lookup"));
    let plan = rig.plan(
        &["approve", "agent", "lookup"],
        &[("approve", "agent", None), ("agent", "lookup", None)],
        &[("approve", &approve), ("lookup", &lookup)],
        json!({"agent": {"system": "Frozen persona.", "instruction_from": "/ask", "tools": []}}),
    );
    let exec = Arc::new(TestExec::default());
    let llm = CapturingLlm::new(vec![answer(r#"{"done": true}"#)]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    let session = runner.start(&plan, "park-1", json!({"ask": "Reconcile."}), &opts()).unwrap();
    let RunSession::Parked { envelope, .. } = session else { panic!("expected a park") };
    let ask_id = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();
    runner.respond("park-1", &ask_id, json!({"ok": true}), false, "user:officer").unwrap();
    finished(runner.resume("park-1", &opts()).unwrap());
    let sent = llm.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].system.as_deref(), Some("Frozen persona."));
    assert_eq!(sent[0].first_user["instruction"], "Reconcile.");
    assert!(sent[0].tools.is_empty(), "`tools: []` offers nothing");
    assert!(runner.verify("park-1").unwrap().verified);
}

// ---- `llm.dispatch`: a Definition no node binds (#392) ----------------------

/// The blessed `mcp.call` shape under the name a pack gave it, bound to NO
/// node: only the agent's `dispatch` list pins it.
fn unbound_mcp(rig: &Rig, ns: &str) -> String {
    let uri = rig.blob(b"\0asm-mcp");
    let t = Tool::new("mcp_call")
        .kind(ToolKind::Definition)
        .tool_description("call a tool on an MCP server")
        .input_schema(json!({"type": "object", "properties": {
            "url": {"type": "string"}, "credential": {"type": "string"},
            "tool": {"type": "string"}, "arguments": {"type": "object"}},
            "required": ["url", "tool", "arguments"], "additionalProperties": false}))
        .executor_uri(&uri)
        .created_at(500)
        .namespace(ns);
    rig.facade.with_store(|m| m.add(&t)).unwrap().to_hex()
}

fn bank_paid_to() -> Value {
    json!({
        "name": "bank.paid_to",
        "description": "Who an invoice was paid to",
        "input_schema": {"type": "object", "properties": {"invoice": {"type": "string"}},
                         "required": ["invoice"]},
        "dispatch": {"definition": "mcp_call",
                     "locked": {"url": "http://127.0.0.1:7798/mcp/orgs/org_x", "credential": "gateway",
                                "tool": "bank.paid_to"},
                     "args_into": "arguments"},
    })
}

fn agent_only_plan(rig: &Rig, ns: &str, llm: Value) -> Hash {
    let mut wf = Workflow::new(vec!["context".into(), "agent".into()]).edge("context", "agent");
    let ctx = Tool::new("context").kind(ToolKind::Definition).created_at(500).namespace(ns);
    let ctx = rig.facade.with_store(|m| m.add(&ctx)).unwrap().to_hex();
    wf = wf.bind("context", &ctx);
    let mut wf = wf.created_at(600).namespace(ns);
    wf.common.extra_fields.insert("llm".into(), llm);
    rig.facade.with_store(|m| m.add(&wf)).unwrap()
}

#[test]
fn a_dispatch_list_pins_an_unbound_definition_that_is_never_offered_itself() {
    let rig = Rig::new();
    let hash = unbound_mcp(&rig, "ops");
    let plan = agent_only_plan(
        &rig,
        "ops",
        json!({"agent": {
            "dispatch": ["mcp_call"],
            "tools": [],
            "tools_from": "/tools",
            // A nested pointer: an `op: query` read with FORMAT markdown
            // lands `{type: "formatted", text, …}` under its key.
            "instruction_from": "/prompt/text",
        }}),
    );
    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "bank_paid_to", json!({"invoice": "4471"}))]),
        answer(r#"{"payee": "ACME"}"#),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    let input = json!({
        "prompt": {"type": "formatted", "format": "markdown", "text": "## Invoice 4471\nWho was paid?"},
        "tools": [bank_paid_to()],
    });
    finished(runner.start(&plan, "d1", input, &opts()).unwrap());

    let sent = llm.sent();
    assert_eq!(sent[0].first_user["instruction"], "## Invoice 4471\nWho was paid?");
    let names: Vec<&str> = sent[0].tools.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["bank.paid_to"], "only the dynamic tool; mcp_call itself is never offered");

    let (name, input, code) =
        exec.seen().into_iter().find(|(n, _, _)| n == "mcp_call").expect("dispatched as mcp_call");
    assert_eq!(name, "mcp_call");
    assert!(code.is_some(), "the pinned blob ran");
    assert_eq!(
        input,
        json!({"url": "http://127.0.0.1:7798/mcp/orgs/org_x", "credential": "gateway",
               "tool": "bank.paid_to", "arguments": {"invoice": "4471"}}),
        "{{...locked, args_into: <model args>}}"
    );
    let journaled = rig
        .intent_inputs("d1", "agent")
        .into_iter()
        .find(|(_, k, _, _)| *k == EffectKind::Tool)
        .unwrap();
    assert_eq!(journaled.2, "bank.paid_to");
    assert_eq!(journaled.3, input, "the merged input is what is journaled");

    let m = rig.facade.with_store(|m| areev_run::RunManifest::load(m, "d1")).unwrap();
    assert_eq!(m.dispatch_pins.len(), 1);
    assert_eq!(m.dispatch_pins[0].tool_hash, hash);
    assert_eq!(m.dispatch_pins[0].node, "agent");
    assert!(m.dispatch_pins[0].executor_uri.is_some(), "pinned with its code address");
    let report = runner.inspect("d1").unwrap();
    let row = report.pinned.iter().find(|r| r["dispatch_only"] == true).unwrap();
    assert_eq!(row["tool"], "mcp_call");
    assert!(row.get("executor_uri").is_some());
    assert!(runner.verify("d1").unwrap().verified);
}

#[test]
fn the_merged_input_is_judged_by_the_definitions_schema_strict_or_not() {
    // mcp_call is NOT strict, and its schema forbids extra keys: an entry
    // that wraps nothing puts the model's `invoice` at the top level, which
    // the Definition refuses — so the model gets a correction, and the
    // Definition never receives a call it does not take.
    let rig = Rig::new();
    unbound_mcp(&rig, "ops");
    let plan = agent_only_plan(&rig, "ops", json!({"agent": {"dispatch": ["mcp_call"], "tools_from": "/tools"}}));
    let mut entry = bank_paid_to();
    entry["dispatch"].as_object_mut().unwrap().remove("args_into");
    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "bank_paid_to", json!({"invoice": "4471"}))]),
        answer("{}"),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    finished(runner.start(&plan, "d2", json!({"tools": [entry]}), &opts()).unwrap());
    assert!(llm.sent()[1].last_user.contains("failed validation"), "{}", llm.sent()[1].last_user);
    assert!(!exec.seen().iter().any(|(n, _, _)| n == "mcp_call"), "never dispatched");
}

#[test]
fn a_dispatch_name_resolves_in_the_plans_namespace_and_a_miss_is_refused() {
    let rig = Rig::new();
    // Authored in `lib`, run in `ops`: the plan grain's namespace answers.
    unbound_mcp(&rig, "lib");
    let plan = agent_only_plan(&rig, "lib", json!({"agent": {"dispatch": ["mcp_call"], "tools_from": "/tools"}}));
    let runner = rig.runner(
        Arc::new(TestExec { allow_code: true, ..Default::default() }),
        CapturingLlm::new(vec![answer("{}")]),
    );
    finished(runner.start(&plan, "d3", json!({"tools": [bank_paid_to()]}), &opts()).unwrap());

    for (block, input, code, needle) in [
        (json!({"agent": {"dispatch": ["nope"], "tools_from": "/tools"}}), json!({"tools": []}), "RUN-E004", "'nope'"),
        (json!({"agent": {"dispatch": "mcp_call"}}), json!({}), "RUN-E019", "`dispatch`"),
        // An entry naming a Definition outside the list (and unpinned) is refused.
        (json!({"agent": {"tools_from": "/tools"}}), json!({"tools": [bank_paid_to()]}), "RUN-E004", "'mcp_call'"),
    ] {
        let plan = agent_only_plan(&rig, "lib", block.clone());
        let runner = rig.runner(Arc::new(TestExec { allow_code: true, ..Default::default() }), CapturingLlm::new(vec![]));
        let err = runner.start(&plan, "bad", input, &opts()).unwrap_err();
        assert_eq!(err.code(), code, "{block}: {err}");
        assert!(err.to_string().contains(needle), "{block}: {err}");
        assert!(!rig.manifest_exists("bad"));
    }
}

#[test]
fn an_unpinned_dispatch_blob_is_refused_before_the_run_exists() {
    let rig = Rig::new();
    unbound_mcp(&rig, "ops");
    let plan = agent_only_plan(&rig, "ops", json!({"agent": {"dispatch": ["mcp_call"], "tools_from": "/tools"}}));
    let runner = rig.runner(Arc::new(TestExec::default()), CapturingLlm::new(vec![]));
    let err = runner.start(&plan, "d4", json!({"tools": [bank_paid_to()]}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E018", "{err}");
    assert!(!rig.manifest_exists("d4"));
}

#[test]
fn a_dynamic_schema_carrying_annotation_keywords_is_pinned_and_dispatched() {
    // #397: what an MCP server's tools/list publishes, annotations and all.
    let rig = Rig::new();
    unbound_mcp(&rig, "ops");
    let plan = agent_only_plan(&rig, "ops", json!({"agent": {"dispatch": ["mcp_call"], "tools": [], "tools_from": "/tools"}}));
    let mut entry = bank_paid_to();
    entry["input_schema"] = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "urn:bank:paid_to", "$comment": "generated",
        "title": "paid_to", "description": "arguments",
        "type": "object",
        "properties": {"invoice": {"type": "string", "title": "Invoice", "description": "number",
                                   "examples": ["4471"], "default": "0", "deprecated": false,
                                   "readOnly": false, "writeOnly": false}},
        "required": ["invoice"],
    });
    let exec = Arc::new(TestExec { allow_code: true, ..Default::default() });
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "bank_paid_to", json!({"invoice": "4471"}))]),
        answer("{}"),
    ]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    finished(runner.start(&plan, "a1", json!({"tools": [entry]}), &opts()).unwrap());
    let offered = llm.sent()[0].tools[0].1.clone().unwrap();
    for k in ["$schema", "$id", "$comment"] {
        assert!(offered.get(k).is_none(), "{k} is ignored, not offered: {offered}");
    }
    assert!(offered["properties"]["invoice"].get("readOnly").is_none());
    assert_eq!(offered["properties"]["invoice"]["title"], "Invoice", "portable annotations stay");
    let (_, input, _) = exec.seen().into_iter().find(|(n, _, _)| n == "mcp_call").unwrap();
    assert_eq!(input["arguments"], json!({"invoice": "4471"}));
    assert!(runner.verify("a1").unwrap().verified);
}

// ---- review fixes ------------------------------------------------------------

#[test]
fn a_flat_entry_with_a_required_locked_key_is_offered_and_dispatched_without_it() {
    // Locks without `args_into`: the entry requires `url`, the host locks it.
    let rig = Rig::new();
    unbound_mcp(&rig, "ops");
    let http = rig.def(Tool::new("http_get").input_schema(json!({"type": "object",
        "properties": {"url": {"type": "string"}, "q": {"type": "string"}}, "required": ["url"]})));
    let plan = agent_only_plan(&rig, "ops", json!({"agent": {"dispatch": [http.clone()], "tools": [], "tools_from": "/tools"}}));
    let entry = json!({"name": "search", "input_schema": {"type": "object",
        "properties": {"url": {"type": "string"}, "q": {"type": "string"}}, "required": ["url", "q"]},
        "dispatch": {"definition": "http_get", "locked": {"url": "https://a.example/s"}}});
    let exec = Arc::new(TestExec::default());
    let llm = CapturingLlm::new(vec![calls(vec![("c1", "search", json!({"q": "x"}))]), answer("{}")]);
    let runner = rig.runner(Arc::clone(&exec), Arc::clone(&llm));
    finished(runner.start(&plan, "f1", json!({"tools": [entry]}), &opts()).unwrap());
    let offered = llm.sent()[0].tools[0].1.clone().unwrap();
    assert_eq!(offered["required"], json!(["q"]), "{offered}");
    assert!(offered["properties"].get("url").is_none());
    assert_eq!(llm.sent().len(), 2, "no re-prompt: the call the model was shown how to make is valid");
    let (_, input, _) = exec.seen().into_iter().find(|(n, _, _)| n == "http_get").unwrap();
    assert_eq!(input, json!({"q": "x", "url": "https://a.example/s"}));
}

#[test]
fn a_lock_on_the_args_into_key_or_a_dotted_lock_is_refused_at_start() {
    let rig = Rig::new();
    unbound_mcp(&rig, "ops");
    let plan = agent_only_plan(&rig, "ops", json!({"agent": {"dispatch": ["mcp_call"], "tools_from": "/tools"}}));
    let mut on_args = bank_paid_to();
    on_args["dispatch"]["locked"]["arguments"] = json!({});
    let mut dotted = bank_paid_to();
    dotted["dispatch"]["locked"]["arguments.invoice"] = json!("1");
    for (entry, needle) in [(on_args, "`args_into` key"), (dotted, "dotted")] {
        let runner = rig.runner(Arc::new(TestExec { allow_code: true, ..Default::default() }), CapturingLlm::new(vec![]));
        let err = runner.start(&plan, "bad", json!({"tools": [entry]}), &opts()).unwrap_err();
        assert_eq!(err.code(), "RUN-E019", "{err}");
        assert!(err.to_string().contains(needle) && err.to_string().contains("bank.paid_to"), "{err}");
    }
}

#[test]
fn a_response_schema_strict_mode_would_refuse_is_refused_at_start() {
    let rig = Rig::new();
    for (schema, needle) in [
        (json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]}), "additionalProperties"),
        (json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
                "required": ["a"], "additionalProperties": false}), "'b'"),
        (json!({"type": "object", "properties": {"n": {"type": "object", "properties": {"x": {"type": "string"}},
                "required": ["x"]}}, "required": ["n"], "additionalProperties": false}), "/properties/n"),
    ] {
        let plan = three_step(&rig, json!({"reply": {"response": {"json_schema": schema}}}));
        let runner = rig.runner(Arc::new(TestExec::default()), CapturingLlm::new(vec![]));
        let err = runner.start(&plan, "bad", json!({}), &opts()).unwrap_err();
        assert_eq!(err.code(), "RUN-E019", "{err}");
        assert!(err.to_string().contains("strict mode") && err.to_string().contains(needle), "{err}");
    }
}

#[test]
fn a_dispatch_pin_may_not_be_another_version_of_a_bound_definition() {
    let rig = Rig::new();
    let v1 = rig.def(Tool::new("lookup").input_schema(json!({"type": "object", "properties": {"q": {"type": "string"}}})));
    let v2 = {
        let t = Tool::new("lookup")
            .kind(ToolKind::Definition)
            .input_schema(json!({"type": "object", "properties": {"z": {"type": "string"}}}))
            .created_at(900)
            .namespace("ops");
        rig.facade.with_store(|m| m.add(&t)).unwrap().to_hex()
    };
    let gate = rig.def(Tool::new("gate"));
    let plan = rig.plan(
        &["lookup", "agent", "gate"],
        &[("lookup", "agent", None), ("agent", "gate", None)],
        &[("lookup", &v1), ("gate", &gate)],
        json!({"agent": {"dispatch": [v2.clone()]}}),
    );
    let runner = rig.runner(Arc::new(TestExec::default()), CapturingLlm::new(vec![]));
    let err = runner.start(&plan, "bad", json!({}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E019", "{err}");
    assert!(err.to_string().contains(&v2) && err.to_string().contains("'lookup'"), "{err}");
}

#[test]
fn locked_params_on_a_client_tool_are_ignored_with_a_warning_not_refused() {
    // 1.12 accepted this shape; a minor release must keep running it.
    let rig = Rig::new();
    let approve = rig.def(
        Tool::new("approve")
            .executor_kind(areev_core::types::ExecutorKind::Client)
            .locked_params(json!({"level": "senior"})),
    );
    let plan = rig.plan(&["approve"], &[], &[("approve", &approve)], Value::Null);
    let runner = rig.runner(Arc::new(TestExec::default()), CapturingLlm::new(vec![]));
    let session = runner.start(&plan, "w1", json!({}), &opts()).unwrap();
    assert!(matches!(session, RunSession::Parked { .. }));
    let report = runner.inspect("w1").unwrap();
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].starts_with("RUN-W001") && report.warnings[0].contains("approve"));
    let m = rig.facade.with_store(|m| areev_run::RunManifest::load(m, "w1")).unwrap();
    assert!(m.pinned[0].locked_params.is_none());
}

#[test]
fn shadow_rehearses_with_the_locks_the_incumbent_froze_not_the_definitions() {
    // A run recorded WITHOUT locks (any 1.12 run): the strict Definition
    // requires `url`, so the model's first call {body} was re-prompted and
    // its second, with a url, dispatched. Rehearsing it with the Definition's
    // locks would accept the first call and diverge on an unchanged plan.
    let rig = Rig::new();
    let approve = rig.def(Tool::new("approve").executor_kind(areev_core::types::ExecutorKind::Client));
    let post = rig.def(
        Tool::new("post")
            .input_schema(json!({"type": "object",
                "properties": {"url": {"type": "string"}, "body": {"type": "string"}},
                "required": ["url", "body"]}))
            .strict(true)
            .locked_params(json!({"url": "https://a.example/x"})),
    );
    let done = rig.def(Tool::new("done"));
    let plan = rig.plan(
        &["approve", "agent", "done", "post"],
        &[("approve", "agent", None), ("agent", "done", None), ("agent", "post", Some("never == true"))],
        &[("approve", &approve), ("post", &post), ("done", &done)],
        Value::Null,
    );
    let llm = CapturingLlm::new(vec![
        calls(vec![("c1", "post", json!({"body": "hi"}))]),
        calls(vec![("c2", "post", json!({"url": "https://b.example", "body": "hi"}))]),
        answer("{}"),
    ]);
    let runner = rig.runner(Arc::new(TestExec::default()), Arc::clone(&llm));
    let RunSession::Parked { envelope, .. } = runner.start(&plan, "old", json!({}), &opts()).unwrap() else {
        panic!("parks on the approval")
    };
    // Make the frozen manifest the 1.12 shape: no locks.
    let mut m = rig.facade.with_store(|m| areev_run::RunManifest::load(m, "old")).unwrap();
    for p in &mut m.pinned {
        p.locked_params = None;
    }
    rig.facade.with_store(|mm| m.persist_in_namespace(mm, "ops")).unwrap();
    let ask = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();
    runner.respond("old", &ask, json!({"ok": true}), false, "user:officer").unwrap();
    finished(runner.resume("old", &opts()).unwrap());
    assert_eq!(llm.sent().len(), 3, "recorded without locks: one re-prompt");

    let report = runner.shadow_plan(&["old".to_string()], &areev_run::PlanCandidate::Hash(plan)).unwrap();
    let run = &report.runs[0];
    assert_eq!(run.verdict, "same", "{run:?}");
    assert!(run.out_of_support.is_empty(), "{run:?}");
    assert!(run.identity.as_ref().is_some_and(|i| i.consistent), "{run:?}");
}

#[test]
fn the_result_records_the_temperature_that_went_out_not_the_one_asked_for() {
    // A transport whose profile pins 0.7, under a node asking 0.2.
    struct Pinned(Arc<CapturingLlm>);
    impl ToolCallLlm for Pinned {
        fn model(&self) -> &str {
            "m"
        }
        fn effective_temperature(&self, _requested: f64) -> Option<f64> {
            Some(0.7)
        }
        fn request_profile_digest(&self) -> Option<String> {
            Some("pinned".into())
        }
        fn call(&self, req: &ToolCallRequest<'_>) -> Result<ToolCallResponse, ToolCallError> {
            self.0.call(req)
        }
    }
    let rig = Rig::new();
    let plan = three_step(&rig, json!({"reply": {"temperature": 0.2}}));
    let inner = CapturingLlm::new(vec![answer("{}")]);
    let runner = Runner { llm: Some(Arc::new(Pinned(inner)) as Arc<dyn ToolCallLlm>), ..rig.runner(Arc::new(TestExec::default()), CapturingLlm::new(vec![])) };
    finished(runner.start(&plan, "t1", json!({}), &opts()).unwrap());
    let view = rig.facade.with_store(|m| areev_run::journal::load(m, "ops", "t1")).unwrap();
    let e = view.entries.values().find(|e| e.key.node == "reply").unwrap();
    let areev_run_core::EffectOutcome::Completed { result, .. } = &e.result.as_ref().unwrap().1 else { panic!() };
    assert_eq!(result["temperature_sent"], json!(0.7));

    // No block and no profile: nothing new on the result.
    let plan = three_step(&rig, Value::Null);
    let llm = CapturingLlm::new(vec![answer("{}")]);
    finished(rig.runner(Arc::new(TestExec::default()), llm).start(&plan, "t2", json!({}), &opts()).unwrap());
    let view = rig.facade.with_store(|m| areev_run::journal::load(m, "ops", "t2")).unwrap();
    let e = view.entries.values().find(|e| e.key.node == "reply").unwrap();
    let areev_run_core::EffectOutcome::Completed { result, .. } = &e.result.as_ref().unwrap().1 else { panic!() };
    assert!(result.get("temperature_sent").is_none(), "{result}");
}

#[test]
fn two_dispatch_pins_of_one_name_and_different_versions_are_refused() {
    let rig = Rig::new();
    let mk = |at: i64, prop: &str| {
        let t = Tool::new("mcp_call")
            .kind(ToolKind::Definition)
            .input_schema(json!({"type": "object", "properties": {prop: {"type": "string"}}}))
            .created_at(at)
            .namespace("ops");
        rig.facade.with_store(|m| m.add(&t)).unwrap().to_hex()
    };
    let (a, b) = (mk(500, "x"), mk(510, "y"));
    for block in [
        json!({"agent": {"dispatch": [a.clone(), b.clone()]}}),
        json!({"agent": {"dispatch": [a.clone()]}, "agent2": {"dispatch": [b.clone()]}}),
    ] {
        let ctx = rig.def(Tool::new("context"));
        let mut wf = Workflow::new(vec!["context".into(), "agent".into(), "agent2".into()])
            .edge("context", "agent")
            .edge("context", "agent2")
            .bind("context", &ctx)
            .created_at(600)
            .namespace("ops");
        wf.common.extra_fields.insert("llm".into(), block.clone());
        let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
        let runner = rig.runner(Arc::new(TestExec::default()), CapturingLlm::new(vec![]));
        let err = runner.start(&plan, "bad", json!({}), &opts()).unwrap_err();
        assert_eq!(err.code(), "RUN-E019", "{block}: {err}");
        assert!(err.to_string().contains(&a) && err.to_string().contains(&b), "{err}");
    }
}
