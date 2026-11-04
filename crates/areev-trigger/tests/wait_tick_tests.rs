//! `trigger run` also ticks the namespace's durable waits (#416): every acting
//! pass calls `RunStarter::tick` once, reports the resumed runs under
//! `waits_resumed`, and a dry run or a single-trigger pass does not tick.

use areev_cal::AreevFacade;
use areev_store::Areev;
use areev_trigger::{clock::FixedClock, EvalOptions, Evaluator, RunStarter, StartResult};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Default)]
struct TickingStarter {
    ticks: AtomicUsize,
}

impl RunStarter for TickingStarter {
    fn start(&self, _workflow: &str, _run_id: &str, _input: Value) -> StartResult {
        StartResult::Started
    }

    fn tick(&self) -> Result<areev_run::TickReport, String> {
        self.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(areev_run::TickReport {
            now_ms: 1,
            resumed: vec![json!({"run_id": "chg-42", "session": "finished:Completed"})],
            waiting: vec![],
            errors: vec!["chg-43: lease held".into()],
        })
    }
}

fn evaluator(starter: Arc<TickingStarter>) -> (tempfile::TempDir, Evaluator) {
    let dir = tempfile::TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let ev = Evaluator {
        facade: Arc::new(AreevFacade::new(m)),
        clock: Arc::new(FixedClock::new(1_767_225_600_000)),
        connector: None,
        connector_code: None,
        starter: Some(starter as Arc<dyn RunStarter>),
        credentials: Default::default(),
        ns: "ops".into(),
        principal: "user:local".into(),
    };
    (dir, ev)
}

#[test]
fn an_acting_pass_ticks_the_waits_once_and_reports_them() {
    let starter = Arc::new(TickingStarter::default());
    let (_d, ev) = evaluator(Arc::clone(&starter));
    let report = ev.run(&EvalOptions::default()).unwrap();
    assert_eq!(starter.ticks.load(Ordering::SeqCst), 1);
    assert_eq!(report.waits_resumed, vec!["chg-42".to_string()]);
    assert!(report.errors.iter().any(|e| e == "wait tick: chg-43: lease held"), "{report:?}");
}

#[test]
fn a_dry_run_or_a_single_trigger_pass_does_not_tick() {
    let starter = Arc::new(TickingStarter::default());
    let (_d, ev) = evaluator(Arc::clone(&starter));
    ev.run(&EvalOptions { dry_run: true, ..Default::default() }).unwrap();
    ev.run(&EvalOptions { only: Some("ab".repeat(32)), ..Default::default() }).unwrap();
    assert_eq!(starter.ticks.load(Ordering::SeqCst), 0);
}
