//! Wave-4 gates: the shadow evaluator replays journaled runs and PROVABLY
//! dispatches zero effects — the §7.4 pre-apply check for code changes.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    BudgetsSpec, ExecResult, HostToolExecutor, OnDangling, RunOptions, Runner, RunSession,
    ScriptedClock,
};
use areev_run_core::RunOutcome;
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

struct CountingExec(AtomicU64);

impl HostToolExecutor for CountingExec {
    fn execute(&self, tool_name: &str, _h: &str, _in: &Value, _idem: &str) -> ExecResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        ExecResult::Ok(json!({tool_name.to_string(): true}))
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
fn shadow_eval_replays_runs_with_zero_dispatches() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));
    let exec = Arc::new(CountingExec(AtomicU64::new(0)));

    let mut wf = Workflow::new(vec!["a".into(), "b".into()]).edge("a", "b");
    for n in ["a", "b"] {
        let def = Tool::new(n)
            .kind(ToolKind::Definition)
            .tool_description("t")
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
        clock: Arc::new(ScriptedClock::new(
            (0..200).map(|i| 1_757_000_000_000 + i * 10).collect(),
        )),
        executor: Arc::clone(&exec) as Arc<dyn HostToolExecutor>,
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:runner".into(),
    };

    for run in ["sh-1", "sh-2", "sh-3"] {
        let session = runner.start(&plan, run, json!({"q": run}), &opts()).unwrap();
        assert!(matches!(
            session,
            RunSession::Finished { outcome: RunOutcome::Completed, .. }
        ));
    }
    let live_calls = exec.0.load(Ordering::SeqCst);
    assert_eq!(live_calls, 6, "two tools × three runs executed live");

    // recent_runs enumerates what we just ran, newest first.
    let recent = runner.recent_runs(10).unwrap();
    assert_eq!(recent.len(), 3);
    assert!(recent.contains(&"sh-1".to_string()));

    // The shadow pass: every run journal-consistent, every effect answered
    // from the journal — and the EXECUTOR COUNTER DID NOT MOVE.
    let report = runner.shadow_eval(&recent);
    assert!(report.all_consistent, "{report:?}");
    assert_eq!(report.runs.len(), 3);
    assert_eq!(report.effect_dispatches, 0);
    assert!(report.runs.iter().all(|r| r.effects_replayed == 2));
    assert_eq!(
        exec.0.load(Ordering::SeqCst),
        live_calls,
        "shadow evaluation must dispatch NOTHING"
    );

    // A tampered/unknown run is reported inconsistent, never skipped.
    let mut ids = recent.clone();
    ids.push("no-such-run".into());
    let report = runner.shadow_eval(&ids);
    assert!(!report.all_consistent);
    assert!(report.runs.iter().any(|r| !r.consistent));
    assert_eq!(exec.0.load(Ordering::SeqCst), live_calls);
}
