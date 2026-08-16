//! Wave-6 gates: the kill-switch DRILL — the enterprise clause is "<5
//! minutes to stop"; this measures cancel→drain against real wall time
//! with in-flight work, and checks the measurement lands where
//! `areev run oversight-report` reads it (the journaled cancel Fact and
//! the terminal checkpoint's journaled close).

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    BudgetsSpec, ExecResult, HostToolExecutor, OnDangling, RunOptions, Runner, RunSession,
    SystemClock,
};
use areev_run_core::RunOutcome;
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// An executor whose every call takes real time — the drill needs work IN
/// FLIGHT when the brake is pulled.
struct SlowExec;
impl HostToolExecutor for SlowExec {
    fn execute(&self, tool_name: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        std::thread::sleep(std::time::Duration::from_millis(150));
        ExecResult::Ok(json!({tool_name.to_string(): true}))
    }
}

#[test]
fn kill_switch_drill_measures_cancel_to_drain_under_the_clause() {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));

    // A 6-node chain of slow tools: plenty of runway to cancel into.
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
        executor: Arc::new(SlowExec),
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:driver".into(),
    };

    // The drill: pull the brake from ANOTHER thread while the run is
    // mid-flight — exactly how an operator would.
    let brake_facade = Arc::clone(&facade);
    let braker = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let brake = Runner {
            facade: brake_facade,
            clock: Arc::new(SystemClock),
            executor: Arc::new(SlowExec),
            llm: None,
            observer: None,
            ns: "ops".into(),
            principal: "user:operator".into(),
        };
        let at = std::time::Instant::now();
        brake.cancel("drill-1", "user:operator", "kill-switch drill").unwrap();
        at
    });

    let started = std::time::Instant::now();
    let opts = RunOptions {
        budgets: BudgetsSpec::default(),
        ask_ttl_sec: None,
        workers: 1,
        on_dangling: OnDangling::Redispatch,
        llm_max_tokens: None,
        inject_crash: None,
    };
    let session = runner.start(&plan, "drill-1", json!({}), &opts).unwrap();
    let drained_at = std::time::Instant::now();
    let cancel_at = braker.join().unwrap();

    // 1. The run terminated Canceled, without finishing the chain.
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert!(
        matches!(outcome, RunOutcome::Canceled { .. }),
        "the brake must terminate the run, got {outcome:?}"
    );

    // 2. THE CLAUSE: cancel → drain far inside 5 minutes (the drill asserts
    // seconds — a bounded drain is one in-flight effect, never the queue).
    let cancel_to_drain = drained_at.duration_since(cancel_at);
    assert!(
        cancel_to_drain < std::time::Duration::from_secs(30),
        "cancel→drain took {cancel_to_drain:?} — the <5min clause wants seconds"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(60));

    // 3. The measurement is JOURNALED where oversight-report reads it: the
    // cancel Fact's created_at precedes (or equals) the terminal
    // checkpoint's journaled close.
    let cancel_ms = facade
        .with_store(|m| m.latest("agent:harness", "run:drill-1", "mg:cancel"))
        .unwrap()
        .expect("cancel fact journaled")
        .get_i64("created_at")
        .unwrap();
    let view = facade
        .with_store(|m| areev_run::journal::load(m, "ops", "drill-1"))
        .unwrap();
    let close_ms = view.checkpoints.last().unwrap().decisions.clock_close_ms as i64;
    assert!(close_ms >= cancel_ms, "terminal close after the cancel mark");
    assert!(
        close_ms - cancel_ms < 300_000,
        "journaled cancel→drain is {}ms — must be inside the 5-minute clause",
        close_ms - cancel_ms
    );

    // 4. Verify still passes — a canceled run replays like any other.
    assert!(runner.verify("drill-1").unwrap().verified);
}

/// The review's P0, pinned at the DRIVER level: a bounded cycle must
/// EXECUTE every iteration — generation-unique journal keys, not replays
/// of iteration 1's result. (Before the fix, re-entry reset the attempt
/// counter, generation 1 minted generation 0's keys, and the driver
/// answered the loop body from the journal forever.)
#[test]
fn bounded_cycles_execute_every_iteration() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));

    struct Counting(AtomicU64);
    impl HostToolExecutor for Counting {
        fn execute(&self, t: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
            // Count LOOP-BODY executions only ("done" legitimately re-enters
            // once per generation via the unconditional fall-through edge).
            if t == "work" {
                let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
                ExecResult::Ok(json!({"count": n}))
            } else {
                ExecResult::Ok(json!({"done": true}))
            }
        }
    }
    let exec = Arc::new(Counting(AtomicU64::new(0)));

    let def = Tool::new("work")
        .kind(ToolKind::Definition)
        .tool_description("loop body")
        .created_at(500)
        .namespace("ops");
    let done_def = Tool::new("done")
        .kind(ToolKind::Definition)
        .tool_description("after the loop")
        .created_at(501)
        .namespace("ops");
    let (plan, _dh) = {
        let dh = facade.with_store(|m| m.add(&def)).unwrap();
        let done_h = facade.with_store(|m| m.add(&done_def)).unwrap();
        // work loops on itself (bounded), then falls through to done — the
        // terminal (a pure self-loop has no terminal node and would stall
        // by design).
        let wf = Workflow::new(vec!["work".into(), "done".into()])
            .edge_with_cycles("work", "work", 2)
            .edge("work", "done")
            .bind("work", &dh.to_hex())
            .bind("done", &done_h.to_hex())
            .created_at(600)
            .namespace("ops");
        (facade.with_store(|m| m.add(&wf)).unwrap(), dh)
    };

    let runner = Runner {
        facade: Arc::clone(&facade),
        clock: Arc::new(areev_run::ScriptedClock::new(
            (0..300).map(|i| 1_759_000_000_000 + i * 10).collect(),
        )),
        executor: Arc::clone(&exec) as Arc<dyn HostToolExecutor>,
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:runner".into(),
    };
    let session = runner
        .start(&plan, "cycle-1", json!({}), &RunOptions { workers: 1, ..Default::default() })
        .unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert!(matches!(outcome, RunOutcome::Completed), "{outcome:?}");

    // max_cycles = 2 ⇒ the edge fires twice ⇒ THREE executions, live.
    assert_eq!(exec.0.load(std::sync::atomic::Ordering::SeqCst), 3, "every iteration executed");

    // Three DISTINCT journal keys — generations never reuse one.
    let view = facade
        .with_store(|m| areev_run::journal::load(m, "ops", "cycle-1"))
        .unwrap();
    let mut keys: Vec<u32> = view
        .entries
        .keys()
        .filter(|k| k.node == "work")
        .map(|k| k.attempt)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec![1, 2, 3], "monotonic attempts across generations");

    // The final state carries the LAST iteration's result, and no spurious
    // redelivery Observations were fabricated.
    let last = view.checkpoints.last().unwrap();
    assert_eq!(last.scheduler["context"]["count"], 3);
    assert!(runner.verify("cycle-1").unwrap().verified);
    let redeliveries = facade
        .with_store(|m| {
            m.recent("agent:harness", Some(areev_core::types::GrainType::Observation), 4096)
        })
        .unwrap()
        .into_iter()
        .filter(|g| g.get_str("object").is_some_and(|o| o.contains("redelivery")))
        .count();
    assert_eq!(redeliveries, 0, "no crash happened; no redelivery may be recorded");
}
