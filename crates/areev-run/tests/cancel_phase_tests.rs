//! The cancel-phase sweep: a canceled run must verify no matter WHERE in
//! the run the brake lands. The first cross-platform CI run caught verify
//! diverging when the cancel arrived during the FIRST superstep (before
//! any checkpoint existed): replay fed CancelSeen at the superstep's
//! open — a phase the live driver only produces when the marker predates
//! the run. Slow runners hit that phase; fast dev machines never did.
//! This sweeps the brake across the whole run so every placement is
//! exercised on every machine.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    BudgetsSpec, ExecResult, HostToolExecutor, OnDangling, RunOptions, Runner, RunSession,
    SystemClock,
};
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

struct SlowExec(u64);
impl HostToolExecutor for SlowExec {
    fn execute(&self, tool_name: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        std::thread::sleep(std::time::Duration::from_millis(self.0));
        ExecResult::Ok(json!({tool_name.to_string(): true}))
    }
}

fn drill(delay_ms: u64, node_ms: u64, run_id: &str) -> (String, bool, String) {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));
    let nodes: Vec<String> = (0..6).map(|i| format!("step{i}")).collect();
    let mut wf = Workflow::new(nodes.clone());
    for w in nodes.windows(2) {
        wf = wf.edge(&w[0], &w[1]);
    }
    for n in &nodes {
        let def = Tool::new(n)
            .kind(ToolKind::Definition)
            .tool_description("slow")
            .created_at(500)
            .namespace("ops");
        let dh = facade.with_store(|m| m.add(&def)).unwrap();
        wf = wf.bind(n, &dh.to_hex());
    }
    let plan: Hash = facade
        .with_store(|m| m.add(&wf.created_at(600).namespace("ops")))
        .unwrap();
    let runner = Runner {
        facade: Arc::clone(&facade),
        clock: Arc::new(SystemClock),
        executor: Arc::new(SlowExec(node_ms)),
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:driver".into(),
    };
    let brake_facade = Arc::clone(&facade);
    let rid = run_id.to_string();
    let braker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let brake = Runner {
            facade: brake_facade,
            clock: Arc::new(SystemClock),
            executor: Arc::new(SlowExec(0)),
            llm: None,
            observer: None,
            ns: "ops".into(),
            principal: "user:operator".into(),
        };
        let _ = brake.cancel(&rid, "user:operator", "kill-switch drill");
    });
    let opts = RunOptions {
        budgets: BudgetsSpec::default(),
        ask_ttl_sec: None,
        workers: 1,
        on_dangling: OnDangling::Redispatch,
        llm_max_tokens: None,
        inject_crash: None,
    };
    let session = runner.start(&plan, run_id, json!({}), &opts).unwrap();
    braker.join().unwrap();
    let outcome = match session {
        RunSession::Finished { outcome, .. } => format!("{outcome:?}"),
        other => format!("{other:?}"),
    };
    let report = runner.verify(run_id).unwrap();
    let detail = if report.verified {
        String::new()
    } else {
        format!("{report:?}")
    };
    (outcome, report.verified, detail)
}

#[test]
fn cancel_at_every_phase_verifies() {
    let mut bad = 0;
    for delay in (0..=160).step_by(20) {
        let (outcome, ok, detail) = drill(delay, 50, &format!("probe-{delay}"));
        if !ok {
            bad += 1;
            println!("DELAY {delay}ms outcome={outcome} VERIFY-FAILED:\n{detail}\n");
        } else {
            println!("delay {delay}ms outcome={outcome} verify ok");
        }
    }
    assert_eq!(bad, 0, "{bad} phases failed verify");
}
