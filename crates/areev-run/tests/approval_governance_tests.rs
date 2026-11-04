//! Approval governance (#416, #420, #423, #424): the responder grant check on
//! an unbound handle, distinct approvers across gates, N-of-M quorum on one
//! ask, the durable timer node with its veto window, and the hash-chained
//! approval trail every decision lands on.

use areev_cal::AreevFacade;
use areev_core::authz::{AUTHZ_NS, REL_PERMITS};
use areev_core::error::Hash;
use areev_core::types::{Fact, Grain, Tool, ToolKind, Workflow};
use areev_run::{Clock, ExecResult, HostToolExecutor, RunOptions, RunSession, Runner};
use areev_run_core::RunOutcome;
use areev_store::Areev;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Counts host executions per tool, so "apply never ran early" is observable.
#[derive(Default)]
struct Counting {
    calls: Mutex<HashMap<String, u32>>,
}
impl Counting {
    fn count(&self, tool: &str) -> u32 {
        *self.calls.lock().unwrap().get(tool).unwrap_or(&0)
    }
}
impl HostToolExecutor for Counting {
    fn execute(&self, tool: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        *self.calls.lock().unwrap().entry(tool.to_string()).or_default() += 1;
        ExecResult::Ok(json!({ tool: true }))
    }
}

/// A monotone clock a test can jump forward: every reading advances 1ms.
struct JumpClock(AtomicU64);
impl JumpClock {
    fn jump(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}
impl Clock for JumpClock {
    fn now_ms(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }
}

const T0: u64 = 1_755_000_000_000;
const H72: u64 = 72 * 3600 * 1000;

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
    exec: Arc<Counting>,
    clock: Arc<JumpClock>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig {
            _dir: dir,
            facade: Arc::new(AreevFacade::new(m)),
            exec: Arc::new(Counting::default()),
            clock: Arc::new(JumpClock(AtomicU64::new(T0))),
        }
    }

    /// A linear plan; `defs` gives a node's Definition its extra fields
    /// (`executor_kind: "client"` is spelled `"client": true`).
    fn plan(&self, nodes: &[&str], defs: &[(&str, Value)], plan_extra: Option<(&str, Value)>) -> Hash {
        let mut wf = Workflow::new(nodes.iter().map(|s| s.to_string()).collect());
        for w in nodes.windows(2) {
            wf = wf.edge(w[0], w[1]);
        }
        for n in nodes {
            let mut def = Tool::new(n)
                .kind(ToolKind::Definition)
                .tool_description("test tool")
                .created_at(500)
                .namespace("ops");
            if let Some((_, extra)) = defs.iter().find(|(d, _)| d == n) {
                for (k, v) in extra.as_object().unwrap() {
                    if k == "client" {
                        def = def.executor_kind(areev_core::types::ExecutorKind::Client);
                    } else if k == "executor_uri" {
                        def = def.executor_uri(v.as_str().unwrap());
                    } else {
                        def.common.extra_fields.insert(k.clone(), v.clone());
                    }
                }
            }
            let dh = self.facade.with_store(|m| m.add(&def)).unwrap();
            wf = wf.bind(n, &dh.to_hex());
        }
        let mut wf = wf.created_at(600).namespace("ops");
        if let Some((k, v)) = plan_extra {
            wf.common.extra_fields.insert(k.into(), v);
        }
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn runner(&self, principal: &str) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::clone(&self.clock) as Arc<dyn Clock>,
            executor: Arc::clone(&self.exec) as Arc<dyn HostToolExecutor>,
            llm: None,
            observer: None,
            ns: "ops".into(),
            principal: principal.into(),
        }
    }

    fn grant(&self, who: &str, what: &str) {
        self.facade
            .with_store(|m| m.add(&Fact::new(who, REL_PERMITS, what).namespace(AUTHZ_NS).created_at(900)))
            .unwrap();
    }

    /// The run's approval-trail records, oldest first.
    fn trail(&self, run_id: &str) -> Vec<Value> {
        self.facade
            .with_store(|m| areev_run::approval::load_for_run(m, run_id))
            .unwrap()
    }
}

fn opts() -> RunOptions {
    RunOptions { workers: 2, ..Default::default() }
}

fn opts_for(initiator: &str) -> RunOptions {
    RunOptions { workers: 2, initiator: Some(initiator.into()), ..Default::default() }
}

fn parked(s: RunSession) -> Value {
    match s {
        RunSession::Parked { envelope, .. } => envelope,
        RunSession::Finished { outcome, .. } => panic!("expected a park, finished {outcome:?}"),
    }
}

fn finished(s: RunSession) -> RunOutcome {
    match s {
        RunSession::Finished { outcome, .. } => outcome,
        RunSession::Parked { envelope, .. } => panic!("expected a finish, parked {envelope}"),
    }
}

fn ask_of(env: &Value) -> String {
    env["asks"][0]["tool_call_id"].as_str().unwrap().to_string()
}

fn client() -> Value {
    json!({"client": true})
}

// ---------------------------------------------------------------------------
// #423 — the responder's grant is checked whenever the FILE is governed
// ---------------------------------------------------------------------------

#[test]
fn an_unbound_handle_still_checks_the_responder_when_grants_exist() {
    // The issue's repro: an actor-only handle (no principal bound), one
    // run.respond grant to volunteer:11203, an answer from volunteer:99999.
    let rig = Rig::new();
    rig.grant("volunteer:11203", "read,run.respond ON ops");
    let plan = rig.plan(&["a", "approve"], &[("approve", client())], None);
    let runner = rig.runner("service:wtl-engine");
    assert!(rig.facade.authz().is_owner(), "the handle is unbound — the owner posture");
    let env = parked(runner.start(&plan, "r1", json!({}), &opts()).unwrap());
    let ask = ask_of(&env);

    let err = runner
        .respond("r1", &ask, json!({"approved": true}), false, "volunteer:99999")
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E012", "{err}");
    assert!(err.to_string().contains("AUT-E001"), "{err}");
    // Refused, and the refusal is on the approval trail.
    let trail = rig.trail("r1");
    assert_eq!(trail.len(), 1, "{trail:?}");
    assert_eq!(trail[0]["outcome"], "refused");
    assert_eq!(trail[0]["refusal"], "missing_grant");

    // The granted volunteer is accepted and the run completes.
    runner.respond("r1", &ask, json!({"approved": true}), false, "volunteer:11203").unwrap();
    assert_eq!(finished(runner.resume("r1", &opts()).unwrap()), RunOutcome::Completed);
    assert!(runner.verify("r1").unwrap().verified);
}

#[test]
fn a_file_with_no_grants_keeps_the_single_user_path() {
    let rig = Rig::new();
    let plan = rig.plan(&["a", "approve"], &[("approve", client())], None);
    let runner = rig.runner("service:wtl-engine");
    let env = parked(runner.start(&plan, "r1", json!({}), &opts()).unwrap());
    // Nobody holds any grant, so any second person may answer.
    runner
        .respond("r1", &ask_of(&env), json!({"approved": true}), false, "volunteer:99999")
        .unwrap();
    assert_eq!(finished(runner.resume("r1", &opts()).unwrap()), RunOutcome::Completed);
    let trail = rig.trail("r1");
    assert_eq!(trail.len(), 1);
    assert_eq!(trail[0]["outcome"], "accepted");
    assert_eq!(trail[0]["responder"], "volunteer:99999");
}

// ---------------------------------------------------------------------------
// #424 — distinct approvers across gates
// ---------------------------------------------------------------------------

fn four_eyes(rig: &Rig) -> Hash {
    rig.plan(
        &["intake", "approve", "approve_2", "send"],
        &[("approve", client()), ("approve_2", client())],
        Some(("separation", json!([["approve", "approve_2"]]))),
    )
}

#[test]
fn one_responder_cannot_answer_both_gates_of_a_separation_group() {
    let rig = Rig::new();
    let plan = four_eyes(&rig);
    let runner = rig.runner("service:wtl-intake");
    let env = parked(runner.start(&plan, "W10431", json!({}), &opts_for("citizen:4821")).unwrap());
    let ask1 = ask_of(&env);
    runner.respond("W10431", &ask1, json!({"approved": true}), false, "volunteer:33410").unwrap();
    let env = parked(runner.resume("W10431", &opts()).unwrap());
    assert_eq!(env["asks"][0]["node"], "approve_2");
    let ask2 = ask_of(&env);

    // The issue's expectation: the second answer is refused, naming the first.
    let err = runner
        .respond("W10431", &ask2, json!({"approved": true}), false, "volunteer:33410")
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E012", "{err}");
    assert!(err.to_string().contains("'approve'") && err.to_string().contains(&ask1), "{err}");
    let refusal = rig.trail("W10431").into_iter().find(|r| r["outcome"] == "refused").unwrap();
    assert_eq!(refusal["refusal"], "distinct_approver");
    assert_eq!(refusal["initiator"], "citizen:4821");
    assert_eq!(refusal["node"], "approve_2");
    // `send` has not run, and verify still passes over the park.
    assert_eq!(rig.exec.count("send"), 0);
    assert!(runner.verify("W10431").unwrap().verified);

    // A different person completes it; verify checks the separation held.
    runner.respond("W10431", &ask2, json!({"approved": true}), false, "volunteer:51002").unwrap();
    assert_eq!(finished(runner.resume("W10431", &opts()).unwrap()), RunOutcome::Completed);
    assert_eq!(rig.exec.count("send"), 1);
    let report = runner.verify("W10431").unwrap();
    assert!(report.verified, "{report:?}");
    assert!(report.steps.iter().any(|s| s.verdict.contains("separation")), "{report:?}");

    // Inspect and oversight-report show the groups.
    let inspect = serde_json::to_value(runner.inspect("W10431").unwrap()).unwrap();
    assert_eq!(inspect["separation"], json!([["approve", "approve_2"]]));
    let ov = serde_json::to_value(runner.oversight_report(Some("W10431"), None).unwrap()).unwrap();
    assert_eq!(ov["human_gates"]["separation_groups"], json!([["approve", "approve_2"]]));
}

#[test]
fn a_separation_group_naming_a_non_gate_is_refused_at_start() {
    let rig = Rig::new();
    let runner = rig.runner("service:x");
    for (group, needle) in [
        (json!([["approve", "nope"]]), "not a node"),
        (json!([["approve", "intake"]]), "client approval node"),
        (json!([["approve"]]), "fewer than two"),
        (json!("approve"), "array"),
    ] {
        let plan = rig.plan(
            &["intake", "approve", "approve_2"],
            &[("approve", client()), ("approve_2", client())],
            Some(("separation", group.clone())),
        );
        let err = runner.start(&plan, "bad", json!({}), &opts()).unwrap_err();
        assert_eq!(err.code(), "RUN-E019", "{group}: {err}");
        assert!(err.to_string().contains(needle), "{group}: {err}");
    }
}

// ---------------------------------------------------------------------------
// #424 — N-of-M quorum on one ask
// ---------------------------------------------------------------------------

fn quorum_plan(rig: &Rig, n: u64) -> Hash {
    rig.plan(
        &["a", "approve", "send"],
        &[("approve", json!({"client": true, "quorum": {"required": n}}))],
        None,
    )
}

#[test]
fn a_quorum_ask_settles_only_on_n_distinct_approvals() {
    let rig = Rig::new();
    let plan = quorum_plan(&rig, 2);
    let runner = rig.runner("service:wtl-intake");
    let env = parked(runner.start(&plan, "q1", json!({}), &opts()).unwrap());
    let ask = ask_of(&env);

    // One approval leaves the run parked.
    runner.respond("q1", &ask, json!({"approved": true}), false, "volunteer:1").unwrap();
    let env = parked(runner.resume("q1", &opts()).unwrap());
    assert_eq!(env["asks"][0]["quorum"], json!({"required": 2, "approvals": 1, "approvers": ["volunteer:1"]}));
    assert_eq!(rig.exec.count("send"), 0);
    let inspect = serde_json::to_value(runner.inspect("q1").unwrap()).unwrap();
    assert_eq!(inspect["quorum"][0]["approvals"], 1, "{inspect}");
    assert_eq!(inspect["quorum"][0]["required"], 2, "{inspect}");

    // A second approval from the same principal is refused.
    let err = runner.respond("q1", &ask, json!({"approved": true}), false, "volunteer:1").unwrap_err();
    assert_eq!(err.code(), "RUN-E012", "{err}");
    assert!(err.to_string().contains("already voted"), "{err}");

    // A second approval from a different principal resumes the run.
    runner.respond("q1", &ask, json!({"approved": true}), false, "volunteer:2").unwrap();
    assert_eq!(finished(runner.resume("q1", &opts()).unwrap()), RunOutcome::Completed);
    assert_eq!(rig.exec.count("send"), 1);

    let outcomes: Vec<String> =
        rig.trail("q1").iter().map(|r| r["outcome"].as_str().unwrap().to_string()).collect();
    assert_eq!(outcomes, ["vote", "refused", "accepted"]);
    let report = runner.verify("q1").unwrap();
    assert!(report.verified, "{report:?}");
    assert!(
        report.steps.iter().any(|s| s.ok && s.verdict.contains("2 of 2 distinct approver")),
        "{report:?}"
    );
    let ov = serde_json::to_value(runner.oversight_report(Some("q1"), None).unwrap()).unwrap();
    assert_eq!(ov["human_gates"]["client_gated_nodes"][0]["quorum"]["required"], 2, "{ov}");
    assert_eq!(ov["approvals"].as_array().unwrap().len(), 3, "{ov}");
}

#[test]
fn any_refusal_settles_a_quorum_ask_as_refused() {
    let rig = Rig::new();
    let plan = quorum_plan(&rig, 3);
    let runner = rig.runner("service:wtl-intake");
    let env = parked(runner.start(&plan, "q2", json!({}), &opts()).unwrap());
    let ask = ask_of(&env);
    runner.respond("q2", &ask, json!({"approved": true}), false, "volunteer:1").unwrap();
    runner
        .respond("q2", &ask, json!({"reason": "unsafe"}), true, "volunteer:2")
        .unwrap();
    // The refusal settled it: the gate fails, nothing downstream runs.
    let outcome = finished(runner.resume("q2", &opts()).unwrap());
    assert!(matches!(outcome, RunOutcome::Failed { .. }), "{outcome:?}");
    assert_eq!(rig.exec.count("send"), 0);
    assert_eq!(rig.trail("q2").last().unwrap()["outcome"], "declined");
    assert!(runner.verify("q2").unwrap().verified);
}

#[test]
fn a_malformed_quorum_is_refused_at_start() {
    let rig = Rig::new();
    let runner = rig.runner("service:x");
    for (extra, needle) in [
        (json!({"client": true, "quorum": {"required": 0}}), "between 1"),
        (json!({"client": true, "quorum": {"required": 2, "of": 3}}), "unknown key"),
        (json!({"quorum": {"required": 2}}), "client tool"),
    ] {
        let plan = rig.plan(&["a", "approve"], &[("approve", extra.clone())], None);
        let err = runner.start(&plan, "bad", json!({}), &opts()).unwrap_err();
        assert_eq!(err.code(), "RUN-E019", "{extra}: {err}");
        assert!(err.to_string().contains(needle), "{extra}: {err}");
    }
}

// ---------------------------------------------------------------------------
// #416 — a durable timer with a veto window
// ---------------------------------------------------------------------------

fn cooling_off(rig: &Rig, wait: Value) -> Hash {
    let mut def = json!({"executor_uri": areev_run::WAIT_URI});
    if !wait.is_null() {
        def["wait"] = wait;
    }
    rig.plan(&["approve", "wait", "apply"], &[("approve", client()), ("wait", def)], None)
}

#[test]
fn apply_never_runs_before_due_at_and_tick_resumes_it_after() {
    let rig = Rig::new();
    let plan = cooling_off(&rig, json!({"for_secs": 259_200}));
    let runner = rig.runner("service:governance");
    let env = parked(runner.start(&plan, "g1", json!({}), &opts_for("volunteer:author")).unwrap());
    runner.respond("g1", &ask_of(&env), json!({"approved": true}), false, "volunteer:2").unwrap();

    // Approved: now parked on the wait, kind "waiting", with its due_at.
    let env = parked(runner.resume("g1", &opts()).unwrap());
    assert_eq!(env["kind"], "waiting", "{env}");
    assert_eq!(env["asks"][0]["ask_kind"], "veto");
    let due = env["due_at_ms"].as_u64().unwrap();
    assert!((T0 + H72..T0 + H72 + 1000).contains(&due), "{due}");

    // Resuming early does not advance: apply has not run.
    rig.clock.jump(H72 / 2);
    let env = parked(runner.resume("g1", &opts()).unwrap());
    assert_eq!(env["kind"], "waiting");
    assert_eq!(rig.exec.count("apply"), 0);
    // Nor does an early tick; it reports the wait.
    let tick = runner.tick(&opts()).unwrap();
    assert!(tick.resumed.is_empty(), "{tick:?}");
    assert_eq!(tick.waiting[0]["run_id"], "g1");
    assert_eq!(tick.waiting[0]["due_at_ms"], due);
    let inspect = serde_json::to_value(runner.inspect("g1").unwrap()).unwrap();
    assert_eq!(inspect["waits"][0]["due_at_ms"], due);
    assert!(runner.verify("g1").unwrap().verified, "verify passes mid-wait");

    // Past due_at, the tick resumes it to completion.
    rig.clock.jump(H72);
    let tick = runner.tick(&opts()).unwrap();
    assert_eq!(tick.resumed.len(), 1, "{tick:?}");
    assert_eq!(tick.resumed[0]["run_id"], "g1");
    assert_eq!(tick.resumed[0]["session"], "finished:Completed");
    assert_eq!(rig.exec.count("apply"), 1);
    let report = runner.verify("g1").unwrap();
    assert!(report.verified, "verify passes across the wait: {report:?}");
}

#[test]
fn a_veto_inside_the_window_ends_the_run_and_is_on_the_record() {
    let rig = Rig::new();
    rig.grant("volunteer:2", "read,run.respond ON ops");
    rig.grant("volunteer:3", "read,run.respond ON ops");
    rig.grant("volunteer:author", "read,run.respond ON ops");
    let plan = cooling_off(&rig, json!({"for_secs": 259_200}));
    let runner = rig.runner("service:governance");
    let env = parked(runner.start(&plan, "g2", json!({}), &opts_for("volunteer:author")).unwrap());
    runner.respond("g2", &ask_of(&env), json!({"approved": true}), false, "volunteer:2").unwrap();
    let env = parked(runner.resume("g2", &opts()).unwrap());
    let wait_ask = ask_of(&env);

    // The initiator cannot veto (nor waive) the cooling-off on their own change,
    // and a principal without run.respond cannot veto at all.
    let err = runner
        .respond("g2", &wait_ask, json!({"reason": "never mind"}), false, "volunteer:author")
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E012", "{err}");
    let err = runner
        .respond("g2", &wait_ask, json!({"reason": "no standing"}), false, "volunteer:99")
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E012", "{err}");

    rig.clock.jump(3600 * 1000);
    runner
        .respond("g2", &wait_ask, json!({"reason": "threshold too low"}), false, "volunteer:3")
        .unwrap();
    let outcome = finished(runner.resume("g2", &opts()).unwrap());
    let RunOutcome::Failed { node, detail } = outcome else { panic!("{outcome:?}") };
    assert_eq!(node, "wait");
    assert!(detail.contains("vetoed by volunteer:3") && detail.contains("threshold too low"), "{detail}");
    assert_eq!(rig.exec.count("apply"), 0);

    let ov = serde_json::to_value(runner.oversight_report(Some("g2"), None).unwrap()).unwrap();
    let veto = ov["approvals"].as_array().unwrap().iter().find(|r| r["outcome"] == "veto").unwrap();
    assert_eq!(veto["responder"], "volunteer:3");
    assert_eq!(veto["reason"], "threshold too low");
    assert!(runner.verify("g2").unwrap().verified);
}

#[test]
fn a_wait_reads_its_instant_from_state_and_refuses_a_veto_after_it() {
    let rig = Rig::new();
    let plan = cooling_off(&rig, Value::Null);
    let runner = rig.runner("service:governance");
    let until = T0 + 10_000;
    let env = parked(runner.start(&plan, "g3", json!({"until_ms": until}), &opts()).unwrap());
    runner.respond("g3", &ask_of(&env), json!({"approved": true}), false, "volunteer:2").unwrap();
    let env = parked(runner.resume("g3", &opts()).unwrap());
    assert_eq!(env["due_at_ms"], until);
    rig.clock.jump(20_000);
    // The window is closed: a veto now is refused, and the next resume settles.
    let err = runner
        .respond("g3", &ask_of(&env), json!({"reason": "late"}), false, "volunteer:3")
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E011", "{err}");
    assert_eq!(finished(runner.resume("g3", &opts()).unwrap()), RunOutcome::Completed);
    assert!(runner.verify("g3").unwrap().verified);
}

#[test]
fn a_wait_with_no_instant_fails_its_node_instead_of_parking_forever() {
    let rig = Rig::new();
    let plan = cooling_off(&rig, Value::Null);
    let runner = rig.runner("service:governance");
    let env = parked(runner.start(&plan, "g4", json!({}), &opts()).unwrap());
    runner.respond("g4", &ask_of(&env), json!({"approved": true}), false, "volunteer:2").unwrap();
    // The park says what is wrong; the next resume (or tick) fails the node.
    let env = parked(runner.resume("g4", &opts()).unwrap());
    assert!(env["asks"][0]["wait_error"].as_str().unwrap().contains("until_ms"), "{env}");
    let tick = runner.tick(&opts()).unwrap();
    let session = tick.resumed[0]["session"].as_str().unwrap();
    assert!(session.starts_with("finished:Failed") && session.contains("for_secs"), "{tick:?}");
    let outcome = finished(runner.resume("g4", &opts()).unwrap());
    let RunOutcome::Failed { node, detail } = outcome else { panic!("{outcome:?}") };
    assert_eq!(node, "wait");
    assert!(detail.contains("for_secs"), "{detail}");
}

#[test]
fn a_malformed_wait_is_refused_at_start() {
    let rig = Rig::new();
    let runner = rig.runner("service:x");
    for wait in [json!({"for_secs": 0}), json!({"for_secs": 1, "until_ms": 2}), json!({"hours": 3})] {
        let plan = cooling_off(&rig, wait.clone());
        let err = runner.start(&plan, "bad", json!({}), &opts()).unwrap_err();
        assert_eq!(err.code(), "RUN-E019", "{wait}: {err}");
    }
}

// ---------------------------------------------------------------------------
// #420 — the trail is one hash chain
// ---------------------------------------------------------------------------

#[test]
fn every_decision_is_chained_on_the_approval_trail() {
    let rig = Rig::new();
    let plan = rig.plan(&["a", "approve"], &[("approve", client())], None);
    let runner = rig.runner("service:wtl-intake");
    let env = parked(runner.start(&plan, "c1", json!({}), &opts()).unwrap());
    let ask = ask_of(&env);
    // Refused (self-approval), then accepted.
    runner.respond("c1", &ask, json!({"approved": true}), false, "service:wtl-intake").unwrap_err();
    runner.respond("c1", &ask, json!({"approved": true}), false, "volunteer:2").unwrap();
    let trail = rig.trail("c1");
    assert_eq!(trail.len(), 2);
    assert_eq!(trail[0]["refusal"], "separation_of_duties");
    assert_eq!(trail[1]["outcome"], "accepted");
    // seq 1 → 2, the second naming the first.
    let g1 = rig
        .facade
        .with_store(|m| m.get(&Hash::from_hex(trail[0]["hash"].as_str().unwrap()).unwrap()))
        .unwrap();
    let g2 = rig
        .facade
        .with_store(|m| m.get(&Hash::from_hex(trail[1]["hash"].as_str().unwrap()).unwrap()))
        .unwrap();
    assert_eq!(g1.fields["context"]["seq"], 1);
    assert_eq!(g1.fields["context"]["chain_root"], true);
    assert_eq!(g2.fields["context"]["seq"], 2);
    assert_eq!(g2.fields["context"]["previous_audit"], trail[0]["hash"]);
    assert_eq!(g2.get_str("observer_id"), Some("volunteer:2"));
}
