//! The Wave-1 driver gates (governed-agents §8): end-to-end runs over a real
//! store, the crash-window redelivery contract, HITL with separation of
//! duties, cancel, and journal-consistent verify — including verify
//! CATCHING a tampered journal, which is the claim's teeth.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    BudgetsSpec, ExecResult, HostToolExecutor, OnDangling, RunOptions, Runner, RunSession,
    ScriptedClock,
};
use areev_run_core::{RunError, RunOutcome};
use areev_store::Areev;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// A scriptable executor that records every invocation's idempotency key —
/// what the crash-injection gate counts.
/// tool_name -> behavior; a behavior may consult the call count.
type Behaviors = BTreeMap<String, Box<dyn FnMut(&Value, u32) -> ExecResult + Send>>;

struct TestExec {
    behaviors: Mutex<Behaviors>,
    calls: Mutex<Vec<(String, String)>>, // (tool_name, idempotency_key)
}

impl TestExec {
    fn new() -> Self {
        TestExec { behaviors: Mutex::new(BTreeMap::new()), calls: Mutex::new(Vec::new()) }
    }
    fn on(&self, tool: &str, f: impl FnMut(&Value, u32) -> ExecResult + Send + 'static) {
        lock_ok(&self.behaviors).insert(tool.into(), Box::new(f));
    }
    fn calls_for(&self, tool: &str) -> Vec<String> {
        lock_ok(&self.calls)
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, k)| k.clone())
            .collect()
    }
}

/// Poison-tolerant lock: a behavior that panics (the pool converts it into
/// a Failed effect) must not poison the whole rig for later attempts.
fn lock_ok<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl HostToolExecutor for TestExec {
    fn execute(&self, tool_name: &str, _hash: &str, input: &Value, idem: &str) -> ExecResult {
        let count = {
            let mut calls = lock_ok(&self.calls);
            calls.push((tool_name.to_string(), idem.to_string()));
            calls.iter().filter(|(t, _)| t == tool_name).count() as u32
        };
        let mut behaviors = lock_ok(&self.behaviors);
        match behaviors.get_mut(tool_name) {
            Some(f) => f(input, count),
            None => ExecResult::Ok(json!({tool_name.to_string(): true})),
        }
    }
}

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
    exec: Arc<TestExec>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig {
            _dir: dir,
            facade: Arc::new(AreevFacade::new(m)),
            exec: Arc::new(TestExec::new()),
        }
    }

    /// A Definition grain per node name, plus the bound workflow.
    fn plan(&self, nodes: &[&str], edges: &[(&str, &str)], client: &[&str]) -> Hash {
        let mut wf = Workflow::new(nodes.iter().map(|s| s.to_string()).collect());
        for (a, b) in edges {
            wf = wf.edge(a, b);
        }
        for n in nodes {
            let mut def = Tool::new(n)
                .kind(ToolKind::Definition)
                .tool_description("test tool")
                .created_at(500)
                .namespace("ops");
            if client.contains(n) {
                def = def.executor_kind(areev_core::types::ExecutorKind::Client);
            }
            let dh = self.facade.with_store(|m| m.add(&def)).unwrap();
            wf = wf.bind(n, &dh.to_hex());
        }
        let wf = wf.created_at(600).namespace("ops");
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn runner(&self, clock_readings: Vec<u64>) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(clock_readings)),
            executor: Arc::clone(&self.exec) as Arc<dyn HostToolExecutor>,
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

fn clocks() -> Vec<u64> {
    (0..200).map(|i| 1_755_000_000_000 + i * 10).collect()
}

#[test]
fn pipeline_runs_journals_and_verifies() {
    let rig = Rig::new();
    let plan = rig.plan(&["a", "b", "c"], &[("a", "b"), ("b", "c")], &[]);
    let runner = rig.runner(clocks());

    let session = runner.start(&plan, "run-1", json!({"q": 1}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    // The journal holds intent+result per node (results are supersessions),
    // plus one checkpoint per superstep and the terminal one.
    let (entries, checkpoints) = rig.facade.with_store(|m| {
        let v = areev_run::journal::load(m, "ops", "run-1").unwrap();
        (v.entries.len(), v.checkpoints.len())
    });
    assert_eq!(entries, 3, "one journal entry per node attempt");
    assert_eq!(checkpoints, 4, "three supersteps + the terminal checkpoint");

    // The execution records are queryable through the plan join — and they
    // are the RESULTS (the re-statement rule), not the superseded intents.
    let records = rig
        .facade
        .with_store(|m| m.step_actions("ops", &plan, None, 100))
        .unwrap();
    assert_eq!(records.len(), 3);

    // §5.5 gate 1: journal-consistent replay, byte-compared checkpoints.
    let report = runner.verify("run-1").unwrap();
    assert!(report.verified, "verify must pass a clean run: {report:?}");
    assert_eq!(report.steps.len(), 4, "every checkpoint verified: {report:?}");
}

/// The crash window (§5.3): the driver dies AFTER the effect fired but
/// BEFORE its result is journaled (deterministic injection — the §5.5
/// gate-2 mechanism, never sleeps). Resume re-delivers under the SAME
/// idempotency key, records the redelivery as an Observation, writes no
/// duplicate intent — and the run completes with the journal exactly-once.
#[test]
fn crash_between_intent_and_result_redelivers_not_duplicates() {
    let rig = Rig::new();
    let plan = rig.plan(&["a", "boom"], &[("a", "boom")], &[]);
    rig.exec.on("boom", |_input, _count| ExecResult::Ok(json!({"boom": "recovered"})));
    let runner = rig.runner(clocks());
    // Result #1 is node `a`; result #2 is `boom` — the crash lands after
    // boom EXECUTED, before its result reached the journal.
    let err = runner
        .start(
            &plan,
            "run-c",
            json!({}),
            &RunOptions {
                workers: 1,
                inject_crash: Some(areev_run::CrashPoint::BeforeResult(2)),
                ..opts()
            },
        )
        .unwrap_err();
    assert!(matches!(err, RunError::Storage { .. }), "the injected crash: {err}");

    // The intent for boom is journaled and dangling.
    let dangling = rig.facade.with_store(|m| {
        let v = areev_run::journal::load(m, "ops", "run-c").unwrap();
        v.entries.values().filter(|e| e.result.is_none()).count()
    });
    assert_eq!(dangling, 1, "the crash left exactly one dangling intent");

    // Resume: completes, same idempotency key on both deliveries, exactly
    // one redelivery Observation, no duplicate intent grains.
    let session = runner.resume("run-c", &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    let keys = rig.exec.calls_for("boom");
    assert_eq!(keys.len(), 2, "one crashed delivery + one redelivery");
    assert_eq!(keys[0], keys[1], "SAME idempotency key across the crash");

    let (entry_count, redeliveries) = rig.facade.with_store(|m| {
        let v = areev_run::journal::load(m, "ops", "run-c").unwrap();
        let boom_entries = v.entries.values().filter(|e| e.key.node == "boom").count();
        let obs = m
            .run_trace(areev_core::authz::HARNESS_NS, "run-c", 100)
            .unwrap()
            .into_iter()
            .filter(|g| {
                g.get_str("object").is_some_and(|o| o.contains("redelivery of intent"))
            })
            .count();
        (boom_entries, obs)
    });
    assert_eq!(entry_count, 1, "ONE journal entry — the intent was adopted, not duplicated");
    assert_eq!(redeliveries, 1, "the redelivery is recorded, never silent");
}

/// A panicking host executor is a FAILED EFFECT (retryable, §6.3), never a
/// dead pool: the first integration run of this crate hung forever on
/// exactly this — a worker died holding half the channel while its idle
/// twin kept the other half open, and `recv()` waited on nobody.
#[test]
fn a_panicking_executor_is_a_failed_effect_not_a_hang() {
    let rig = Rig::new();
    let plan = rig.plan(&["flaky"], &[], &[]);
    let mut w = rig
        .facade
        .with_store(|m| m.get(&plan))
        .unwrap()
        .to_workflow()
        .unwrap();
    w.retries.insert("flaky".into(), 1);
    let plan = rig.facade.with_store(|m| m.add(&w.created_at(700))).unwrap();

    rig.exec.on("flaky", |_input, count| {
        if count == 1 {
            panic!("tool blew up");
        }
        ExecResult::Ok(json!({"flaky": "second try"}))
    });
    let runner = rig.runner(clocks());
    let session = runner.start(&plan, "run-p", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed, "panic → ExecutorError → retry → ok");
    assert_eq!(rig.exec.calls_for("flaky").len(), 2);
    // The panic message survives as the failed attempt's journaled detail.
    let has_panic_detail = rig.facade.with_store(|m| {
        let v = areev_run::journal::load(m, "ops", "run-p").unwrap();
        v.entries.values().any(|e| {
            matches!(&e.result, Some((_, areev_run_core::EffectOutcome::Failed { detail, .. }))
                if detail.contains("tool blew up"))
        })
    });
    assert!(has_panic_detail, "the failed attempt's cause is journaled");
}

#[test]
fn hitl_parks_enforces_separation_and_completes_on_resume() {
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"]);
    let runner = rig.runner(clocks());

    let session = runner.start(&plan, "run-h", json!({}), &opts()).unwrap();
    let RunSession::Parked { envelope, .. } = session else { panic!("expected park") };
    let ask_id = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();
    assert_eq!(envelope["asks"][0]["approval"], true);

    // The triggering principal may not approve their own run's ask (§6.6):
    // refused STRUCTURALLY.
    let err = runner
        .respond("run-h", &ask_id, json!({"ok": true}), false, "user:runner")
        .unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");

    // A second principal approves; resuming completes the run and the
    // responder is attributed on the superseding result grain.
    runner
        .respond("run-h", &ask_id, json!({"approve": "granted"}), false, "user:officer")
        .unwrap();
    let session = runner.resume("run-h", &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    let responder = rig.facade.with_store(|m| {
        let v = areev_run::journal::load(m, "ops", "run-h").unwrap();
        let entry = v.entries.values().find(|e| e.key.node == "approve").unwrap();
        let (h, _) = entry.result.as_ref().unwrap();
        m.get(h).unwrap().get_str("author_did").map(str::to_string)
    });
    assert_eq!(responder.as_deref(), Some("user:officer"), "who approved is ON the grain");

    // A second response to the settled ask: journaled as a rejection, then
    // refused — the losing responder is audit evidence, not noise.
    let err = runner
        .respond("run-h", &ask_id, json!({"no": true}), false, "user:late")
        .unwrap_err();
    assert!(matches!(err, RunError::UnknownAsk { .. }));
    let rejections = rig.facade.with_store(|m| {
        m.run_trace(areev_core::authz::HARNESS_NS, "run-h", 100)
            .unwrap()
            .into_iter()
            .filter(|g| g.get_str("object").is_some_and(|o| o.contains("rejected response")))
            .count()
    });
    assert_eq!(rejections, 1, "the losing response is journaled");

    // Verify covers the parked span too.
    let report = runner.verify("run-h").unwrap();
    assert!(report.verified, "{report:?}");
}

#[test]
fn cancel_drains_a_parked_run() {
    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"]);
    let runner = rig.runner(clocks());
    let session = runner.start(&plan, "run-x", json!({}), &opts()).unwrap();
    assert!(matches!(session, RunSession::Parked { .. }));

    runner.cancel("run-x", "user:ops", "no longer needed").unwrap();
    let session = runner.resume("run-x", &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(
        outcome,
        RunOutcome::Canceled { by: "user:ops".into(), reason: "no longer needed".into() }
    );
}

/// Verify's teeth: a tampered journal (a result superseded with altered
/// content) makes replay diverge from the stored checkpoints — reported,
/// with the failing checkpoint named.
#[test]
fn verify_catches_a_tampered_result() {
    let rig = Rig::new();
    let plan = rig.plan(&["a", "b"], &[("a", "b")], &[]);
    let runner = rig.runner(clocks());
    runner.start(&plan, "run-t", json!({}), &opts()).unwrap();
    assert!(runner.verify("run-t").unwrap().verified);

    // Tamper: supersede a's result with a forged payload. A realistic
    // forger copies the journal-key fields faithfully (a grain without them
    // would not even register as a journal entry), so the forgery becomes
    // the entry the loader reads — and the divergence is caught by replay,
    // not by shape.
    rig.facade.with_store(|m| {
        let v = areev_run::journal::load(m, "ops", "run-t").unwrap();
        let entry = v.entries.values().find(|e| e.key.node == "a").unwrap().clone();
        let (result_hash, _) = entry.result.unwrap();
        let stored = m.get(&result_hash).unwrap();
        let mut forged = stored.to_tool().unwrap();
        forged.content = Some(json!({"a": "FORGED"}).to_string());
        forged.common.created_at = Some(1_755_999_999_999);
        forged.common.namespace = Some("ops".into());
        forged = forged.step_action(&plan.to_hex(), "a");
        for key in [
            "run_id", "task_path", "node", "attempt", "effect_seq", "superstep",
            "effect_kind", "usage_input_tokens", "usage_output_tokens",
            "usage_usd_micros", "usage_journal_bytes",
        ] {
            if let Some(val) = stored.fields.get(key) {
                forged.common.extra_fields.insert(key.into(), val.clone());
            }
        }
        m.supersede(&result_hash, &mut forged).unwrap();
    });

    let report = runner.verify("run-t").unwrap();
    assert!(!report.verified, "tampering must not verify: {report:?}");
    assert!(
        report.steps.iter().any(|s| !s.ok && s.verdict.contains("DIVERGES")),
        "the divergent checkpoint is named: {report:?}"
    );
}

/// Determinism across the driver too: two identical runs (same scripted
/// clock, same behaviors) produce byte-identical checkpoint chains.
#[test]
fn two_identical_runs_have_identical_checkpoint_states() {
    let mk = || {
        let rig = Rig::new();
        let plan = rig.plan(&["a", "p1", "p2", "join"], &[("a", "p1"), ("a", "p2"), ("p1", "join"), ("p2", "join")], &[]);
        let runner = rig.runner(clocks());
        runner.start(&plan, "run-d", json!({"seed": 1}), &opts()).unwrap();
        rig.facade.with_store(|m| {
            let v = areev_run::journal::load(m, "ops", "run-d").unwrap();
            serde_json::to_string(&v.checkpoints.iter().map(|c| &c.scheduler).collect::<Vec<_>>())
                .unwrap()
        })
    };
    assert_eq!(mk(), mk(), "same script, same plan ⇒ identical checkpoint states");
}

/// D5 at the driver boundary: on a GOVERNED session (a bound principal),
/// run.execute gates start/resume and the RESPONDER's own grants must cover
/// run.respond — an approval boundary where whoever holds the process can
/// approve is not an approval boundary. The unbound single-user path stays
/// owner-permissive, same as every other surface.
#[test]
fn governed_sessions_enforce_the_run_verbs() {
    use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
    use areev_core::types::Fact;

    let rig = Rig::new();
    let plan = rig.plan(&["auto", "approve"], &[("auto", "approve")], &["approve"]);
    // Grants: runner may execute; the officer may respond; the clerk may not.
    rig.facade.with_store(|m| {
        for (who, what) in [
            ("agent:runner", "read,write,run.execute,run.cancel ON ops"),
            ("user:officer", "read,run.respond ON ops"),
        ] {
            m.add(
                &Fact::new(who, REL_PERMITS, what)
                    .namespace(AUTHZ_NS)
                    .created_at(900),
            )
            .unwrap();
        }
        Ok::<_, areev_core::error::AreevError>(())
    })
    .unwrap();

    // Bind the session: this is now a governed process, not the owner path.
    rig.facade.bind_principal("agent:runner").unwrap();
    let runner = Runner {
        facade: Arc::clone(&rig.facade),
        clock: Arc::new(ScriptedClock::new(clocks())),
        executor: Arc::clone(&rig.exec) as Arc<dyn HostToolExecutor>,
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "agent:runner".into(),
    };

    let session = runner.start(&plan, "run-g", json!({}), &opts()).unwrap();
    let RunSession::Parked { envelope, .. } = session else { panic!("expected park") };
    let ask_id = envelope["asks"][0]["tool_call_id"].as_str().unwrap().to_string();

    // A responder WITHOUT run.respond is refused — before any other check.
    let err = runner
        .respond("run-g", &ask_id, json!({"ok": true}), false, "user:clerk")
        .unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");

    // The granted officer responds; the run resumes and completes.
    runner
        .respond("run-g", &ask_id, json!({"approve": true}), false, "user:officer")
        .unwrap();
    let session = runner.resume("run-g", &opts()).unwrap();
    assert!(matches!(
        session,
        RunSession::Finished { outcome: RunOutcome::Completed, .. }
    ));

    // A session whose principal lacks run.execute cannot start runs at all.
    rig.facade.bind_principal("user:officer").unwrap();
    let err = runner.start(&plan, "run-g2", json!({}), &opts()).unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");
}

/// A plan whose bound Tool Definition carries a malformed `tool_name` is
/// refused at `run start`, before any effect is dispatched.
///
/// The name reaches a host tool as `$AREEV_TOOL_NAME`, and it arrives from a
/// plan grain — which may have been imported from a bundle whose author we do
/// not vouch for, since import verifies content integrity and not authorship.
/// Refusing at resolve time keeps a newline- or metacharacter-bearing name out
/// of a child's environment and fails loudly, rather than mid-superstep.
#[test]
fn malformed_tool_name_is_refused_at_resolve_time() {
    let rig = Rig::new();

    // Author the definition directly so the bad name is what a crafted bundle
    // would carry, not something the builder would reject.
    let mut wf = Workflow::new(vec!["step".to_string()]);
    let def = Tool::new("evil; rm -rf /")
        .kind(ToolKind::Definition)
        .tool_description("crafted")
        .created_at(500)
        .namespace("ops");
    let dh = rig.facade.with_store(|m| m.add(&def)).unwrap();
    wf = wf.bind("step", &dh.to_hex());
    let plan = rig
        .facade
        .with_store(|m| m.add(&wf.created_at(600).namespace("ops")))
        .unwrap();

    let err = rig
        .runner(vec![1_000])
        .start(&plan, "run-bad-tool-name", json!({}), &opts())
        .unwrap_err();

    match err {
        RunError::UnresolvedRef { what } => {
            assert!(what.contains("not a valid tool name"), "{what}");
            assert!(what.contains("evil"), "the message should name the offender: {what}");
        }
        other => panic!("expected UnresolvedRef, got {other:?}"),
    }
}
