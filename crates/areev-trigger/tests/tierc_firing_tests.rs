//! A firing starts the same run `run start` does — code-carrying nodes and all.
//!
//! The regression (#90): every surface that started runs from a trigger built
//! a deliberately reduced runner — a bare `CommandExecutor` with `llm: None` —
//! so a plan with a Tier C / code-carrying node refused with `RUN-E018` on the
//! trigger path while running happily by hand, no matter which flags the
//! operator passed. `--context-query` (#92) and `runtime` (#86) shipped in the
//! same release and were meant to compose; used together, the run refused at
//! start, and an agent could have declared context **or** sandboxed tools on
//! the trigger path, not both.
//!
//! What is pinned here is the composition — evaluator → starter → real
//! `areev_run::Runner` → pinned `CodeExecutor` — because that is where the
//! reduction lived. The flag and `$AREEV_RUN_*` plumbing that feeds it is
//! pinned next to the builder itself, in `areev-cli`'s `run_stack`.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Trigger, TriggerKind, Workflow};
use areev_run::{BudgetsSpec, CodeExecutor, ExecResult, HostToolExecutor, OnDangling, RunOptions};
use areev_store::Areev;
use areev_trigger::{clock::FixedClock, EvalOptions, Evaluator, RunStarter, StartResult};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

const NS: &str = "ops";
const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

/// Stands in for `--tool-cmd`. Anything reaching it means the code path was
/// bypassed — the failure the pin exists to prevent.
struct Fallback;
impl HostToolExecutor for Fallback {
    fn execute(&self, tool_name: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        ExecResult::Ok(json!({ "fell_back_to_tool_cmd": tool_name }))
    }
}

/// The CLI's bridge, reproduced: the evaluator hands a workflow reference and
/// a run id to a real runtime.
struct RunnerStarter {
    runner: areev_run::Runner,
    opts: RunOptions,
}

impl RunStarter for RunnerStarter {
    fn start(&self, workflow: &str, run_id: &str, input: Value) -> StartResult {
        let hash = match Hash::from_hex(workflow) {
            Ok(h) => h,
            Err(e) => return StartResult::Failed(format!("workflow {workflow}: {e}")),
        };
        match self.runner.start(&hash, run_id, input, &self.opts) {
            Ok(_) => StartResult::Started,
            Err(e) => StartResult::Failed(e.to_string()),
        }
    }
}

struct Rig {
    _dir: TempDir,
    dir: std::path::PathBuf,
    facade: Arc<AreevFacade>,
    clock: Arc<FixedClock>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig {
            _dir: dir,
            dir: path,
            facade: Arc::new(AreevFacade::new(m)),
            clock: Arc::new(FixedClock::new(T0)),
        }
    }

    /// A one-node plan bound to a Definition that names its executor by
    /// content address — the shape #90 reported as refused from a trigger.
    fn code_plan(&self, uri: &str) -> Hash {
        let def = Tool::new("validate_rows")
            .kind(ToolKind::Definition)
            .tool_description("a code-carrying tool")
            .executor_uri(uri)
            .created_at(T0 - 2000)
            .namespace(NS);
        let dh = self.facade.with_store(|m| m.add(&def)).unwrap();
        let wf = Workflow::new(vec!["validate_rows".into()])
            .bind("validate_rows", &dh.to_hex())
            .created_at(T0 - 1000)
            .namespace(NS);
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn put_blob(&self, bytes: &[u8]) -> String {
        self.facade.with_store(|m| m.put_blob(bytes)).unwrap()
    }

    fn declare(&self, workflow: &Hash) -> String {
        let t = Trigger::new(TriggerKind::Interval, &workflow.to_hex())
            .interval_secs(60)
            .created_at(T0)
            .namespace(NS);
        self.facade.with_store(|m| m.add(&t)).unwrap().to_hex()
    }

    fn evaluator(&self, executor: Arc<dyn HostToolExecutor>) -> Evaluator {
        let runner = areev_run::Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(areev_run::ScriptedClock::new(
                (0..400u64).map(|i| T0 as u64 + i * 10).collect(),
            )),
            executor,
            llm: None,
            observer: None,
            ns: NS.into(),
            principal: "user:heartbeat".into(),
        };
        Evaluator {
            facade: Arc::clone(&self.facade),
            clock: Arc::clone(&self.clock) as Arc<dyn areev_trigger::Clock>,
            connector: None,
            starter: Some(Arc::new(RunnerStarter {
                runner,
                opts: RunOptions {
                    budgets: BudgetsSpec::default(),
                    ask_ttl_sec: None,
                    workers: 2,
                    on_dangling: OnDangling::Redispatch,
                    llm_max_tokens: None,
                    inject_crash: None,
                },
            }) as Arc<dyn RunStarter>),
            credentials: Default::default(),
            ns: NS.into(),
            principal: "user:heartbeat".into(),
        }
    }
}

fn opts() -> EvalOptions {
    EvalOptions { node: "node-A".into(), ..Default::default() }
}

#[cfg(unix)]
#[test]
fn a_firing_executes_a_pinned_code_carrying_node() {
    let rig = Rig::new();
    // Reads the tool input on stdin and answers with JSON, exactly like a
    // --tool-cmd tool. The contract is deliberately identical.
    let uri = rig.put_blob(b"#!/bin/sh\nread -r line\necho '{\"validated\":true}'\n");
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let plan = rig.code_plan(&uri);
    rig.declare(&plan);

    let exec = CodeExecutor::new(Arc::new(Fallback))
        .allow(&addr)
        .cache_dir(rig.dir.join("execache"));
    let report = rig.evaluator(Arc::new(exec)).run(&opts()).unwrap();

    assert!(report.errors.is_empty(), "the firing must not refuse: {:?}", report.errors);
    assert_eq!(report.runs_started, 1);

    // The blob ran, not the fallback: a bypassed code path would have written
    // `fell_back_to_tool_cmd` instead.
    let records = rig.facade.with_store(|m| m.step_actions(NS, &plan, None, 10)).unwrap();
    assert_eq!(records.len(), 1, "the node executed exactly once");
    let grain = rig.facade.with_store(|m| m.get(&records[0].1)).unwrap();
    let body = grain.get_str("tool_content").or_else(|| grain.get_str("content")).unwrap_or("");
    assert!(body.contains("validated"), "the pinned blob's own output: {body}");
    assert!(!body.contains("fell_back_to_tool_cmd"), "the code path was bypassed: {body}");
}

#[test]
fn an_unpinned_code_carrying_node_still_refuses_from_a_trigger() {
    // The other direction, and the more important one: the fix widens what a
    // firing CAN execute, never what it MAY. The authorization to run code
    // comes from the host — a grant in the file would arrive in the same
    // bundle as the code it authorizes.
    let rig = Rig::new();
    let uri = rig.put_blob(b"#!/bin/sh\necho '{}'\n");
    let plan = rig.code_plan(&uri);
    rig.declare(&plan);

    // Fallback does not implement `code_allowed`, so it inherits the default:
    // refuse. This is a host that never opted in.
    let report = rig.evaluator(Arc::new(Fallback)).run(&opts()).unwrap();

    assert_eq!(report.runs_started, 0);
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("RUN-E018"), "{:?}", report.errors);
    // And the refusal names a surface the operator is actually using.
    assert!(
        report.errors[0].contains("areev trigger"),
        "the message must name the trigger path it refused on: {:?}",
        report.errors
    );
}
