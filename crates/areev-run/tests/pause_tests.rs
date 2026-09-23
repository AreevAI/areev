//! Host-initiated resumable pause (#344): a live run parks at its next
//! superstep boundary on the host's request and continues under the SAME run
//! id, manifest and pins — no fork, no node re-executed across the pause, and
//! `verify` passing on the paused-then-resumed run.
//!
//! The pause is requested from INSIDE a tool's execution, which is the one
//! deterministic place to land it: the driver is blocked on the wave, so the
//! request is always visible at the boundary that wave closes into. A host
//! watching `onEvent` lands it asynchronously somewhere later — the bindings'
//! tests cover that shape.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Fact, Grain, Tool, ToolKind, Workflow};
use areev_run::{
    ExecResult, HostToolExecutor, RunEvent, RunObserver, RunOptions, RunSession, Runner,
    ScriptedClock,
};
use areev_run_core::{RunError, RunOutcome};
use areev_store::Areev;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const NS: &str = "ops";

type Hook = Box<dyn Fn(u32) + Send + Sync>;

/// Records every invocation; a per-tool hook runs inside the execution.
#[derive(Default)]
struct Exec {
    calls: Mutex<Vec<String>>,
    hooks: Mutex<BTreeMap<String, Hook>>,
}

impl Exec {
    fn on(&self, tool: &str, f: impl Fn(u32) + Send + Sync + 'static) {
        self.hooks.lock().unwrap().insert(tool.into(), Box::new(f));
    }
    fn count(&self, tool: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|t| *t == tool).count()
    }
    fn names(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl HostToolExecutor for Exec {
    fn execute(&self, tool: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        let n = {
            let mut c = self.calls.lock().unwrap();
            c.push(tool.to_string());
            c.iter().filter(|t| *t == tool).count() as u32
        };
        if let Some(f) = self.hooks.lock().unwrap().get(tool) {
            f(n);
        }
        ExecResult::Ok(json!({ tool.to_string(): true }))
    }
}

/// Never called: the host that pauses or cancels executes nothing.
struct NoExec;
impl HostToolExecutor for NoExec {
    fn execute(&self, _t: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        ExecResult::Err {
            cause: areev_run::FailCause::Unknown,
            detail: "the controlling host executes nothing".into(),
        }
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<RunEvent>>);
impl RunObserver for Events {
    fn event(&self, ev: &RunEvent) {
        self.0.lock().unwrap().push(ev.clone());
    }
}

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
    exec: Arc<Exec>,
}

fn clocks() -> Vec<u64> {
    (0..400).map(|i| 1_755_000_000_000 + i * 10).collect()
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig { _dir: dir, facade: Arc::new(AreevFacade::new(m)), exec: Arc::new(Exec::default()) }
    }

    /// `nodes` in a chain; `client` nodes are human gates.
    fn chain(&self, nodes: &[&str], client: &[&str]) -> Hash {
        let mut wf = Workflow::new(nodes.iter().map(|s| s.to_string()).collect());
        for w in nodes.windows(2) {
            wf = wf.edge(w[0], w[1]);
        }
        for n in nodes {
            let mut def = Tool::new(n)
                .kind(ToolKind::Definition)
                .tool_description("test tool")
                .created_at(500)
                .namespace(NS);
            if client.contains(n) {
                def = def.executor_kind(areev_core::types::ExecutorKind::Client);
            }
            let dh = self.facade.with_store(|m| m.add(&def)).unwrap();
            wf = wf.bind(n, &dh.to_hex());
        }
        self.facade.with_store(|m| m.add(&wf.created_at(600).namespace(NS))).unwrap()
    }

    /// The driver: runs tools through `exec`.
    fn runner(&self) -> Runner {
        self.runner_observed(None)
    }

    fn runner_observed(&self, observer: Option<Arc<dyn RunObserver>>) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(clocks())),
            executor: Arc::clone(&self.exec) as Arc<dyn HostToolExecutor>,
            llm: None,
            observer,
            ns: NS.into(),
            principal: "user:runner".into(),
        }
    }

    /// A second host on the same memory — the operator's side.
    fn operator(&self) -> Runner {
        operator(&self.facade)
    }

    /// Ask `run_id` to pause from inside `tool`'s `nth` execution.
    fn pause_during(&self, tool: &str, nth: u32, run_id: &str, because: &str) {
        let facade = Arc::clone(&self.facade);
        let (run_id, because) = (run_id.to_string(), because.to_string());
        self.exec.on(tool, move |n| {
            if n == nth {
                operator(&facade).pause(&run_id, "user:ops", &because).unwrap();
            }
        });
    }
}

fn operator(facade: &Arc<AreevFacade>) -> Runner {
    Runner {
        facade: Arc::clone(facade),
        clock: Arc::new(ScriptedClock::new(clocks())),
        executor: Arc::new(NoExec),
        llm: None,
        observer: None,
        ns: NS.into(),
        principal: "user:ops".into(),
    }
}

fn opts() -> RunOptions {
    RunOptions { workers: 1, ..Default::default() }
}

fn terminal_context(rig: &Rig, run_id: &str) -> Value {
    rig.facade
        .with_store(|m| areev_run::journal::load(m, NS, run_id))
        .unwrap()
        .checkpoints
        .last()
        .unwrap()
        .scheduler["context"]
        .clone()
}

#[test]
fn a_paused_run_parks_at_the_boundary_and_resumes_under_the_same_id() {
    let rig = Rig::new();
    let plan = rig.chain(&["a", "b", "c"], &[]);
    rig.pause_during("a", 1, "run-p", "quota reached");

    let events = Arc::new(Events::default());
    let driver = rig.runner_observed(Some(Arc::clone(&events) as Arc<dyn RunObserver>));
    let session = driver.start(&plan, "run-p", json!({"q": 1}), &opts()).unwrap();

    // Parked, reason `paused`, after superstep 1 — and b and c never ran.
    let RunSession::Parked { envelope, run_id } = session else { panic!("expected a park") };
    assert_eq!(run_id, "run-p");
    assert_eq!(envelope["kind"], "paused");
    assert_eq!(envelope["reason"], "paused");
    assert_eq!(envelope["superstep"], 1);
    assert_eq!(envelope["paused_by"], "user:ops");
    assert_eq!(envelope["because"], "quota reached");
    assert_eq!(envelope["asks"], json!([]));
    assert_eq!(rig.exec.names(), vec!["a"], "nothing past the boundary dispatched");

    // The leg ends at `RunPaused`, the way a gate's leg ends at `AskRaised`.
    // The bus delivers on its own thread and is drained when the drive
    // returns, so the events are all in by now.
    let seen = events.0.lock().unwrap().clone();
    assert!(matches!(seen.last(), Some(RunEvent::RunPaused { superstep: 1, .. })), "{seen:?}");
    assert!(!seen.iter().any(|e| matches!(e, RunEvent::RunFinished { .. })));

    // Inspect names the phase and who, when, why.
    let inspect = serde_json::to_value(rig.operator().inspect("run-p").unwrap()).unwrap();
    assert_eq!(inspect["phase"], "paused");
    assert_eq!(inspect["pause"]["status"], "paused");
    assert_eq!(inspect["pause"]["paused_by"], "user:ops");
    assert_eq!(inspect["pause"]["because"], "quota reached");
    assert_eq!(inspect["pause"]["superstep"], 1);
    assert!(inspect["pause"]["requested_at"].as_i64().unwrap() > 0);
    assert!(inspect["pause"]["paused_at"].as_i64().unwrap() > 0);

    // A paused run verifies whole: it has done nothing past its checkpoint.
    let report = rig.operator().verify("run-p").unwrap();
    assert!(report.verified, "{report:?}");

    // Resume: same run id, the remaining two nodes, each exactly once.
    let session = rig.runner().resume("run-p", &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected a finish") };
    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!((rig.exec.count("a"), rig.exec.count("b"), rig.exec.count("c")), (1, 1, 1));

    // The request is consumed, and the resumed run replays identically.
    let view = rig.facade.with_store(|m| areev_run::journal::load(m, NS, "run-p")).unwrap();
    assert!(view.pause.active.is_none(), "a resume consumes the request");
    let report = rig.operator().verify("run-p").unwrap();
    assert!(report.verified, "{report:?}");
    // Positive control: verify really crossed the pause — the resumed
    // superstep is stamped as a resume boundary, and every stored checkpoint
    // (the terminal one included) was byte-compared, not skipped.
    assert!(view.checkpoints.iter().any(|c| c.decisions.resumed_at.is_some()));
    assert_eq!(report.steps.len(), view.checkpoints.len(), "{report:?}");
    let inspect = serde_json::to_value(rig.operator().inspect("run-p").unwrap()).unwrap();
    assert_eq!(inspect["phase"], "finished");
    assert!(inspect.get("pause").is_none(), "no standing request, no pause record");

    // …and ends in the state an uninterrupted run of the same plan ends in.
    let straight = rig.runner().start(&plan, "run-u", json!({"q": 1}), &opts()).unwrap();
    assert!(matches!(straight, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    assert_eq!(terminal_context(&rig, "run-p"), terminal_context(&rig, "run-u"));

    // A second resume is not needed and changes nothing.
    let again = rig.runner().resume("run-p", &opts()).unwrap();
    assert!(matches!(again, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    assert_eq!(rig.exec.count("b"), 2, "only run-u ran b again");
}

#[test]
fn a_consumed_request_never_re_pauses_and_a_new_one_pauses_again() {
    let rig = Rig::new();
    let plan = rig.chain(&["a", "b", "c"], &[]);
    // Two pause cycles, both inside one scripted millisecond family: the
    // second request must still be a distinct grain, and the first — once
    // consumed — must not park the resumed leg.
    rig.pause_during("a", 1, "run-2", "first");
    rig.pause_during("b", 1, "run-2", "first");

    let s1 = rig.runner().start(&plan, "run-2", json!({}), &opts()).unwrap();
    assert!(matches!(&s1, RunSession::Parked { envelope, .. } if envelope["superstep"] == 1));
    let s2 = rig.runner().resume("run-2", &opts()).unwrap();
    let RunSession::Parked { envelope, .. } = s2 else { panic!("the second request parks") };
    assert_eq!(envelope["superstep"], 2);
    assert_eq!(rig.exec.names(), vec!["a", "b"]);

    let s3 = rig.runner().resume("run-2", &opts()).unwrap();
    assert!(matches!(s3, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    assert_eq!(rig.exec.names(), vec!["a", "b", "c"], "every node exactly once");

    let view = rig.facade.with_store(|m| areev_run::journal::load(m, NS, "run-2")).unwrap();
    assert_eq!(view.pause.requests, 2, "two requests, two grains");
    assert!(view.pause.active.is_none());
    assert!(rig.operator().verify("run-2").unwrap().verified);
}

#[test]
fn pausing_twice_is_idempotent() {
    let rig = Rig::new();
    let plan = rig.chain(&["a", "b", "c"], &[]);
    let facade = Arc::clone(&rig.facade);
    let receipts = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&receipts);
    rig.exec.on("a", move |_| {
        let op = operator(&facade);
        for why in ["first", "second"] {
            let r = op.pause("run-i", "user:ops", why).unwrap();
            sink.lock().unwrap().push(serde_json::to_value(r).unwrap());
        }
    });

    let session = rig.runner().start(&plan, "run-i", json!({}), &opts()).unwrap();
    assert!(matches!(session, RunSession::Parked { .. }));
    let r = receipts.lock().unwrap().clone();
    assert_eq!((r[0]["status"].as_str(), r[0]["already"].as_bool()), (Some("requested"), Some(false)));
    assert_eq!((r[1]["status"].as_str(), r[1]["already"].as_bool()), (Some("requested"), Some(true)));
    assert_eq!(r[0]["request"], r[1]["request"], "the standing request answers");
    assert_eq!(r[1]["because"], "first", "the second call wrote nothing");

    // …and on a run that is already PARKED on it.
    let third = serde_json::to_value(rig.operator().pause("run-i", "user:ops", "third").unwrap())
        .unwrap();
    assert_eq!(third["status"], "paused");
    assert_eq!(third["already"], true);
    assert_eq!(third["request"], r[0]["request"]);
    let view = rig.facade.with_store(|m| areev_run::journal::load(m, NS, "run-i")).unwrap();
    assert_eq!(view.pause.requests, 1, "one request grain, however many asks");
}

#[test]
fn pausing_a_finished_or_canceled_run_is_refused_with_run_e029() {
    let rig = Rig::new();
    let plan = rig.chain(&["a", "b"], &[]);
    rig.runner().start(&plan, "done", json!({}), &opts()).unwrap();
    let err = rig.operator().pause("done", "user:ops", "too late").unwrap_err();
    assert_eq!(err.code(), "RUN-E029", "{err}");
    assert!(matches!(err, RunError::NotPausable { .. }));
    assert!(err.to_string().contains("completed"), "{err}");

    // A pending cancel wins: the run is going to be canceled, not paused.
    let facade = Arc::clone(&rig.facade);
    rig.exec.on("a", move |n| {
        if n == 2 {
            operator(&facade).cancel("braked", "user:ops", "stop").unwrap();
            let err = operator(&facade).pause("braked", "user:ops", "hold").unwrap_err();
            assert_eq!(err.code(), "RUN-E029");
        }
    });
    let s = rig.runner().start(&plan, "braked", json!({}), &opts()).unwrap();
    assert!(matches!(s, RunSession::Finished { outcome: RunOutcome::Canceled { .. }, .. }));
    let err = rig.operator().pause("braked", "user:ops", "hold").unwrap_err();
    assert_eq!(err.code(), "RUN-E029");
    assert!(err.to_string().contains("canceled"), "{err}");

    // An unknown run is not a request filed for later.
    assert!(rig.operator().pause("nope", "user:ops", "x").is_err());
}

#[test]
fn cancel_on_a_paused_run_finalizes_it_as_canceled() {
    let rig = Rig::new();
    let plan = rig.chain(&["a", "b", "c"], &[]);
    rig.pause_during("a", 1, "run-c", "hold");
    let s = rig.runner().start(&plan, "run-c", json!({}), &opts()).unwrap();
    assert!(matches!(s, RunSession::Parked { .. }));

    // Cancel is the brake, and a paused run has no driver to notice a marker:
    // the cancel itself finishes it.
    rig.operator().cancel("run-c", "user:ops", "abandon").unwrap();
    let inspect = serde_json::to_value(rig.operator().inspect("run-c").unwrap()).unwrap();
    assert_eq!(inspect["phase"], "finished");
    assert!(inspect.get("pause").is_none());
    let resumed = rig.runner().resume("run-c", &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = resumed else { panic!("expected a finish") };
    assert_eq!(
        outcome,
        RunOutcome::Canceled { by: "user:ops".into(), reason: "abandon".into() }
    );
    assert_eq!(rig.exec.names(), vec!["a"], "nothing ran after the pause");
    let report = rig.operator().verify("run-c").unwrap();
    assert!(report.verified, "{report:?}");
    assert!(
        report.steps.last().is_some_and(|s| s.verdict.starts_with("terminal checkpoint")),
        "the canceled terminal is replayed, not skipped: {report:?}"
    );
}

#[test]
fn a_pause_asked_while_parked_on_a_gate_applies_after_the_answer() {
    let rig = Rig::new();
    let plan = rig.chain(&["a", "gate", "c"], &["gate"]);
    let s = rig.runner().start(&plan, "run-g", json!({}), &opts()).unwrap();
    let RunSession::Parked { envelope, .. } = s else { panic!("parks on the gate") };
    let ask = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();

    // Requested, not honoured: the run is parked on a person, not a pause.
    let r = serde_json::to_value(rig.operator().pause("run-g", "user:ops", "drain").unwrap())
        .unwrap();
    assert_eq!(r["status"], "requested");
    let inspect = serde_json::to_value(rig.operator().inspect("run-g").unwrap()).unwrap();
    assert_eq!(inspect["pause"]["status"], "requested");
    assert_ne!(inspect["phase"], "paused");

    rig.operator().respond("run-g", &ask, json!({"ok": true}), false, "user:officer").unwrap();
    // The resume that settles the answer must NOT consume a request it never
    // honoured: it closes the gate's superstep and parks there.
    let s = rig.runner().resume("run-g", &opts()).unwrap();
    let RunSession::Parked { envelope, .. } = s else { panic!("the pause applies") };
    assert_eq!(envelope["kind"], "paused");
    assert_eq!(rig.exec.count("c"), 0);
    assert_eq!(
        serde_json::to_value(rig.operator().inspect("run-g").unwrap()).unwrap()["phase"],
        "paused"
    );

    let s = rig.runner().resume("run-g", &opts()).unwrap();
    assert!(matches!(s, RunSession::Finished { outcome: RunOutcome::Completed, .. }));
    assert_eq!((rig.exec.count("a"), rig.exec.count("c")), (1, 1));
    assert!(rig.operator().verify("run-g").unwrap().verified);
}

#[test]
fn pause_takes_the_resume_grant_not_the_cancel_grant() {
    use areev_core::authz::{AUTHZ_NS, REL_PERMITS};

    let rig = Rig::new();
    let plan = rig.chain(&["a", "gate"], &["gate"]);
    let s = rig.runner().start(&plan, "run-a", json!({}), &opts()).unwrap();
    assert!(matches!(s, RunSession::Parked { .. }));

    rig.facade
        .with_store(|m| {
            m.add(
                &Fact::new("agent:brake", REL_PERMITS, "run.cancel ON ops")
                    .namespace(AUTHZ_NS)
                    .created_at(900),
            )
        })
        .unwrap();
    rig.facade.bind_principal("agent:brake").unwrap();
    let err = rig.operator().pause("run-a", "agent:brake", "hold").unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");
    // The brake itself stays reachable at the lower bar.
    rig.operator().cancel("run-a", "agent:brake", "stop").unwrap();
}
