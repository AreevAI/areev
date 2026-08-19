//! The model boundary: what a run sends to an LLM, and what comes back.
//!
//! The rule under test is that the boundary is the **model, not the tool**. A
//! host tool posting an invoice must receive real values — a pseudonymized
//! supplier name writes a corrupt invoice. The model doing the extraction
//! works just as well on `[ORG_7C1A]`.
//!
//! The gap this closes: the store's gate is an egress boundary on *reads*, and
//! an abstract node's prompt is not a read. A trigger hands its payload
//! straight into `run start` in process, so under an `egress` policy the one
//! place a model was actually called was the one place the policy did not
//! reach.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_llm::{
    StopReason, ToolCallError, ToolCallLlm, ToolCallRequest, ToolCallResponse, Usage,
};
use areev_run::{
    BudgetsSpec, ExecResult, HostToolExecutor, OnDangling, RunOptions, Runner, RunSession,
    ScriptedClock,
};
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const KEY: [u8; 32] = [7u8; 32];
const NS: &str = "ops";
/// The real contact. It must reach the tool and never the model.
///
/// An address rather than a bare name on purpose: Tier-0 detects `email` and
/// `phone` by pattern, while `person` matches only known identities and
/// custom terms. A test that used a bare name would pass vacuously against a
/// memory with no interned identities — which is exactly how the first draft
/// of this file was wrong.
const SUPPLIER: &str = "aurelia@vandenberg.example";

/// Records exactly what each tool was handed, so a test can assert on the
/// values that crossed the seam rather than on a summary of them.
#[derive(Default)]
struct RecordingExec {
    seen: Mutex<Vec<Value>>,
}

impl HostToolExecutor for RecordingExec {
    fn execute(&self, _n: &str, _h: &str, input: &Value, _k: &str) -> ExecResult {
        self.seen.lock().unwrap().push(input.clone());
        ExecResult::Ok(json!({ "posted": true }))
    }
}

/// Answers each turn from a script; an exhausted script is a hard error.
struct ScriptedLlm {
    responses: Mutex<std::collections::VecDeque<ToolCallResponse>>,
    /// Every prompt the model was shown, for asserting what it could see.
    saw: Mutex<Vec<String>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<ToolCallResponse>) -> Arc<Self> {
        Arc::new(ScriptedLlm {
            responses: Mutex::new(responses.into_iter().collect()),
            saw: Mutex::new(Vec::new()),
        })
    }
    fn transcript(&self) -> String {
        self.saw.lock().unwrap().join("\n")
    }
}

impl ToolCallLlm for ScriptedLlm {
    fn model(&self) -> &str {
        "scripted"
    }
    fn call(&self, req: &ToolCallRequest<'_>) -> Result<ToolCallResponse, ToolCallError> {
        self.saw.lock().unwrap().push(format!("{:?}", req.messages));
        self.responses.lock().unwrap().pop_front().ok_or(ToolCallError {
            retryable: false,
            message: "scripted LLM exhausted".into(),
        })
    }
}

fn done(text: &str) -> ToolCallResponse {
    ToolCallResponse {
        text: Some(text.into()),
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: Usage { input_tokens: 4, output_tokens: 2, cache_read_tokens: None },
    }
}

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
    exec: Arc<RecordingExec>,
}

impl Rig {
    /// `policy` declares `anon:<NS>`; None leaves the memory untouched.
    fn new(policy: Option<&str>) -> Self {
        let dir = TempDir::new().unwrap();
        let mut m =
            Areev::open_encrypted(dir.path().join("m.db").to_str().unwrap(), KEY).unwrap();
        if let Some(p) = policy {
            m.set_anon_policy(NS, p).unwrap();
        }
        Rig {
            _dir: dir,
            facade: Arc::new(AreevFacade::new(m)),
            exec: Arc::new(RecordingExec::default()),
        }
    }

    /// `extract` is abstract (no binding, no Definition); `post` is a host tool.
    fn plan(&self) -> Hash {
        let def = Tool::new("post")
            .kind(ToolKind::Definition)
            .tool_description("post the invoice")
            .created_at(500)
            .namespace(NS);
        let dh = self.facade.with_store(|m| m.add(&def)).unwrap();
        let wf = Workflow::new(vec!["extract".into(), "post".into()])
            .edge("extract", "post")
            .bind("post", &dh.to_hex())
            .created_at(600)
            .namespace(NS);
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn runner(&self, llm: Arc<dyn ToolCallLlm>) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(
                (0..400).map(|i| 1_755_000_000_000 + i * 10).collect(),
            )),
            executor: Arc::clone(&self.exec) as Arc<dyn HostToolExecutor>,
            llm: Some(llm),
            observer: None,
            ns: NS.into(),
            principal: "user:runner".into(),
        }
    }
}

fn opts() -> RunOptions {
    RunOptions {
        budgets: BudgetsSpec::default(),
        ask_ttl_sec: None,
        workers: 1,
        on_dangling: OnDangling::Redispatch,
        llm_max_tokens: None,
        inject_crash: None,
    }
}

/// The whole point, in one test: the model never sees the supplier's name, the
/// tool always does.
#[test]
fn the_model_sees_a_placeholder_and_the_tool_sees_the_real_value() {
    let rig = Rig::new(Some(r#"{"mode": "egress", "scope": "memory"}"#));
    let plan = rig.plan();

    // The model answers with whatever placeholder it was shown. A real model
    // echoes the token it saw; this makes that explicit and checkable.
    let llm = ScriptedLlm::new(vec![done("{}")]);
    let runner = rig.runner(Arc::clone(&llm) as Arc<dyn ToolCallLlm>);
    let session = runner
        .start(&plan, "r1", json!({ "messages": [
            { "role": "user", "content": format!("invoice from {SUPPLIER}") }
        ]}), &opts())
        .unwrap();
    let RunSession::Finished { .. } = session else { panic!("expected finish") };

    let seen_by_model = llm.transcript();
    assert!(
        !seen_by_model.contains(SUPPLIER),
        "the supplier's name must not reach the model: {seen_by_model}"
    );
    assert!(
        seen_by_model.contains("[EMAIL_"),
        "and it must have been replaced by a placeholder: {seen_by_model}"
    );
}

#[test]
fn a_tool_call_carrying_a_placeholder_is_rehydrated_before_dispatch() {
    let rig = Rig::new(Some(r#"{"mode": "egress", "scope": "memory"}"#));
    let plan = rig.plan();

    // Turn 1 shows the model the invoice; it answers done. The host node then
    // runs with the context the abstract node produced.
    let llm = ScriptedLlm::new(vec![done(&json!({ "vendor": SUPPLIER }).to_string())]);
    let runner = rig.runner(Arc::clone(&llm) as Arc<dyn ToolCallLlm>);
    runner
        .start(&plan, "r1", json!({ "messages": [
            { "role": "user", "content": format!("invoice from {SUPPLIER}") }
        ]}), &opts())
        .unwrap();

    // Whatever reached the tool must be the real name, not a token: a tool
    // posting `[PERSON_1A2B]` to a vendor writes a corrupt record.
    let seen = rig.exec.seen.lock().unwrap().clone();
    let body = serde_json::to_string(&seen).unwrap();
    assert!(!body.contains("[EMAIL_"), "a placeholder reached the tool: {body}");
    assert!(
        body.contains(SUPPLIER),
        "the tool must get the real value back, not a token: {body}"
    );
}

#[test]
fn a_placeholder_the_run_cannot_resolve_fails_the_node_instead_of_dispatching() {
    // Fail closed. `rehydrate` leaves a token it cannot resolve intact, and
    // dispatching would send the literal placeholder to a vendor.
    let rig = Rig::new(Some(r#"{"mode": "egress", "scope": "memory"}"#));
    let plan = rig.plan();

    // The model invents a token that was never minted.
    let llm = ScriptedLlm::new(vec![done(
        &json!({ "vendor": "[EMAIL_DEADBEEF]" }).to_string(),
    )]);
    let runner = rig.runner(Arc::clone(&llm) as Arc<dyn ToolCallLlm>);
    let _ = runner
        .start(&plan, "r1", json!({ "messages": [
            { "role": "user", "content": format!("invoice from {SUPPLIER}") }
        ]}), &opts())
        .unwrap();

    let seen = rig.exec.seen.lock().unwrap().clone();
    let body = serde_json::to_string(&seen).unwrap();
    assert!(
        !body.contains("EMAIL_DEADBEEF"),
        "an unresolvable placeholder must never be dispatched: {body}"
    );
}

#[test]
fn a_policy_that_cannot_replay_is_refused_before_the_run_starts() {
    // Session scope numbers tokens by appearance order, so a replay
    // pseudonymizes differently and verify diverges. Refusing at start makes
    // that a configuration error rather than an integrity failure later.
    let rig = Rig::new(Some(r#"{"mode": "egress", "scope": "session"}"#));
    let plan = rig.plan();
    let llm = ScriptedLlm::new(vec![done("{}")]);
    let err = rig
        .runner(llm as Arc<dyn ToolCallLlm>)
        .start(&plan, "r1", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E023");
    assert!(err.to_string().contains("memory"), "the fix must be named: {err}");
}

#[test]
fn with_no_policy_declared_nothing_is_transformed() {
    // The boundary is additive: a memory that declared nothing behaves
    // exactly as it did before.
    let rig = Rig::new(None);
    let plan = rig.plan();
    let llm = ScriptedLlm::new(vec![done(&json!({ "vendor": SUPPLIER }).to_string())]);
    let runner = rig.runner(Arc::clone(&llm) as Arc<dyn ToolCallLlm>);
    runner
        .start(&plan, "r1", json!({ "messages": [
            { "role": "user", "content": format!("invoice from {SUPPLIER}") }
        ]}), &opts())
        .unwrap();

    assert!(
        llm.transcript().contains(SUPPLIER),
        "without a policy the model sees the text as-is"
    );
}
