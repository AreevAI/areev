//! Code-carrying tools: `executor_uri` on a Definition, and the host-side
//! pin that decides whether any of it runs.
//!
//! The invariant under test is the one that makes this safe to ship: a blob
//! travels with the memory, so importing a peer's bundle imports their
//! connector code — and **the authorization to execute it never travels with
//! it**. An unpinned address is refused at start, by name.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    BudgetsSpec, CodeExecutor, ExecResult, HostToolExecutor, OnDangling, RunOptions, Runner,
    RunSession, ScriptedClock,
};
use areev_run_core::RunOutcome;
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// Stands in for `--tool-cmd`. Anything reaching it means the code path was
/// bypassed, which is the failure this whole feature exists to prevent.
struct Fallback;
impl HostToolExecutor for Fallback {
    fn execute(&self, tool_name: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        ExecResult::Ok(json!({ "fell_back_to_tool_cmd": tool_name }))
    }
}

struct Rig {
    _dir: TempDir,
    dir: std::path::PathBuf,
    facade: Arc<AreevFacade>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig { _dir: dir, dir: path, facade: Arc::new(AreevFacade::new(m)) }
    }

    /// A one-node plan whose Definition names `executor_uri`.
    fn plan_with_uri(&self, uri: Option<&str>, client: bool) -> Hash {
        let mut def = Tool::new("work")
            .kind(ToolKind::Definition)
            .tool_description("a code-carrying tool")
            .created_at(500)
            .namespace("ops");
        if let Some(u) = uri {
            def = def.executor_uri(u);
        }
        if client {
            def = def.executor_kind(areev_core::types::ExecutorKind::Client);
        }
        let dh = self.facade.with_store(|m| m.add(&def)).unwrap();
        let wf = Workflow::new(vec!["work".into()])
            .bind("work", &dh.to_hex())
            .created_at(600)
            .namespace("ops");
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn put_blob(&self, bytes: &[u8]) -> String {
        self.facade.with_store(|m| m.put_blob(bytes)).unwrap()
    }

    fn runner(&self, exec: Arc<dyn HostToolExecutor>) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(
                (0..200).map(|i| 1_755_000_000_000 + i * 10).collect(),
            )),
            executor: exec,
            llm: None,
            observer: None,
            ns: "ops".into(),
            principal: "user:runner".into(),
        }
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

#[test]
fn an_executor_uri_that_is_not_a_content_address_is_refused_by_name() {
    // The original defect: this value was written, parsed, and read by
    // nothing, so the node quietly executed whatever --tool-cmd was. Now it
    // either dispatches or says why it cannot.
    let rig = Rig::new();
    let plan = rig.plan_with_uri(Some("executor://crm.lookup@v3"), false);
    let err = rig
        .runner(Arc::new(Fallback))
        .start(&plan, "r1", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    let msg = err.to_string();
    assert!(msg.contains("executor://crm.lookup@v3"), "{msg}");
    assert!(msg.contains("cas://sha256:"), "the message must name the form that works: {msg}");
}

#[test]
fn an_unpinned_code_executor_is_refused_before_the_run_exists() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"#!/bin/sh\necho '{}'\n");
    let plan = rig.plan_with_uri(Some(&uri), false);

    // Fallback does not implement code_allowed, so it inherits the default:
    // refuse. A host that never opted in cannot be handed code by a plan.
    let err = rig
        .runner(Arc::new(Fallback))
        .start(&plan, "r1", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("--allow-executor"), "{err}");

    // Refused BEFORE the run exists — no manifest, no lease, nothing to
    // explain afterwards.
    let journal = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "r1").map(|v| v.checkpoints.len()));
    assert!(journal.map(|n| n == 0).unwrap_or(true), "a refused start must leave no run");
}

#[test]
fn a_client_tool_carrying_an_executor_uri_is_refused() {
    // A client tool is answered by a person through `run respond`. Naming an
    // executor on one describes something that cannot happen.
    let rig = Rig::new();
    let uri = rig.put_blob(b"#!/bin/sh\necho '{}'\n");
    let plan = rig.plan_with_uri(Some(&uri), true);
    let err = rig
        .runner(Arc::new(Fallback))
        .start(&plan, "r1", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("client tool"), "{err}");
}

#[cfg(unix)]
#[test]
fn a_pinned_code_executor_runs_and_its_output_is_the_node_result() {
    let rig = Rig::new();
    // Reads the tool input on stdin and answers with JSON, exactly like a
    // --tool-cmd tool. The contract is deliberately identical.
    let script = b"#!/bin/sh\nread -r line\necho \"{\\\"ran\\\":true,\\\"saw\\\":$line}\"\n";
    let uri = rig.put_blob(script);
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let plan = rig.plan_with_uri(Some(&uri), false);

    let exec = CodeExecutor::new(Arc::new(Fallback))
        .allow(&addr)
        .cache_dir(rig.dir.join("execache"));
    let runner = rig.runner(Arc::new(exec));

    let session = runner.start(&plan, "r1", json!({ "q": 7 }), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    let entries = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "r1").unwrap().entries.len());
    assert_eq!(entries, 1);

    // The blob ran — not the fallback. If the code path were bypassed the
    // result would carry `fell_back_to_tool_cmd`.
    // Read the execution record itself: `step_actions` gives (node, hash),
    // and the result grain carries what the effect produced.
    let records = rig
        .facade
        .with_store(|m| m.step_actions("ops", &plan, None, 10))
        .unwrap();
    assert_eq!(records.len(), 1);
    let grain = rig.facade.with_store(|m| m.get(&records[0].1)).unwrap();
    let content = grain.get_str("tool_content").expect("a result grain carries its content");
    let produced: Value = serde_json::from_str(content).expect("the blob emitted JSON");
    assert_eq!(produced["ran"], json!(true), "the blob's output IS the node result");
    assert_eq!(produced["saw"]["q"], json!(7), "the tool input reached the blob's stdin");
    assert!(
        produced.get("fell_back_to_tool_cmd").is_none(),
        "a pinned code executor must not fall through to --tool-cmd"
    );
}

#[test]
fn a_tool_naming_no_executor_still_goes_to_tool_cmd() {
    // The feature is additive: everything that worked before works unchanged.
    let rig = Rig::new();
    let plan = rig.plan_with_uri(None, false);
    let session = rig
        .runner(Arc::new(CodeExecutor::new(Arc::new(Fallback))))
        .start(&plan, "r1", json!({}), &opts())
        .unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);
}
