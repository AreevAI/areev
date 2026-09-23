//! Declared memory reads (#255): a plan names `{subject, relation, at, axis}`
//! and the RUNTIME answers it from the store it already holds — no tool ever
//! holds a handle on the memory its own run is holding.
//!
//! The fixture is `examples/agents/insurance-documents`' centrepiece, booked
//! the way that example's `apply_change` books it: an endorsement raising
//! POL-4471's limit to 750,000 effective 1 May, received 15 June (BACKDATED),
//! and a correction restating the deductible from 5,000 to 10,000, received 20
//! June. That example asserts the two clocks from the driver; these tests
//! assert the same answers from INSIDE a run.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Fact, Grain, Tool, ToolKind, Workflow};
use areev_run::{ExecResult, HostToolExecutor, RunOptions, RunSession, Runner, ScriptedClock};
use areev_run_core::{EffectOutcome, FailCause, JournalKey, NodeExecutor, RunError, RunOutcome};
use areev_store::{Areev, Axis};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const RUN_NS: &str = "org.uw";
const POLICY_NS: &str = "org.uw.policies";

fn ms(date: &str) -> i64 {
    areev_core::time::iso8601_to_ms(date).unwrap()
}

/// Records every host call: (tool name, the state it was handed).
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, Value)>>,
}

impl HostToolExecutor for Recorder {
    fn execute(&self, tool_name: &str, _hash: &str, input: &Value, _idem: &str) -> ExecResult {
        self.calls
            .lock()
            .unwrap()
            .push((tool_name.to_string(), input.clone()));
        ExecResult::Ok(json!({ format!("{tool_name}_done"): true }))
    }
}

impl Recorder {
    fn names(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }
    fn input_of(&self, tool: &str) -> Value {
        let calls = self.calls.lock().unwrap();
        calls
            .iter()
            .find(|(n, _)| n == tool)
            .map(|(_, v)| v.clone())
            .expect(tool)
    }
}

struct Rig {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
    exec: Arc<Recorder>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig {
            _dir: dir,
            facade: Arc::new(AreevFacade::new(m)),
            exec: Arc::new(Recorder::default()),
        }
    }

    fn runner(&self, principal: &str) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(
                (0..400).map(|i| 1_785_000_000_000 + i * 10).collect(),
            )),
            executor: Arc::clone(&self.exec) as Arc<dyn HostToolExecutor>,
            llm: None,
            observer: None,
            ns: RUN_NS.into(),
            principal: principal.into(),
        }
    }

    /// The schedule as issued, then the week's two coverage documents, booked
    /// on both clocks exactly as the example's `apply_change` does.
    fn book_pol_4471(&self) {
        self.facade
            .with_store(|m| {
                let issued = ms("2026-01-01");
                let limit = m.add(
                    &Fact::new("POL-4471", "mg:coverage_limit", "500000")
                        .namespace(POLICY_NS)
                        .valid_from(issued)
                        .created_at(issued),
                )?;
                let deductible = m.add(
                    &Fact::new("POL-4471", "mg:deductible", "5000")
                        .namespace(POLICY_NS)
                        .valid_from(issued)
                        .created_at(issued),
                )?;
                m.add(&Fact::new("POL-4471", "mg:owned_by", "ACME-HOLDINGS").namespace(POLICY_NS))?;
                m.add(&Fact::new("POL-5120", "mg:owned_by", "ACME-HOLDINGS").namespace(POLICY_NS))?;

                // END-2201: a VARIATION, effective 1 May, received 15 June. The
                // open window closes (keeping its original created_at) and a
                // new one opens — both stay live for the world axis.
                let (eff, recv) = (ms("2026-05-01"), ms("2026-06-15"));
                m.supersede(
                    &limit,
                    &mut Fact::new("POL-4471", "mg:coverage_limit", "500000")
                        .namespace(POLICY_NS)
                        .valid_from(issued)
                        .valid_to(eff)
                        .created_at(issued),
                )?;
                m.add(
                    &Fact::new("POL-4471", "mg:coverage_limit", "750000")
                        .namespace(POLICY_NS)
                        .valid_from(eff)
                        .created_at(recv),
                )?;

                // CORR-118: a RESTATEMENT, received 20 June — the deductible
                // was always 10,000; the desk believed 5,000 until then.
                m.supersede(
                    &deductible,
                    &mut Fact::new("POL-4471", "mg:deductible", "10000")
                        .namespace(POLICY_NS)
                        .valid_from(issued)
                        .created_at(ms("2026-06-20")),
                )?;
                Ok::<_, areev_core::error::AreevError>(())
            })
            .unwrap();
    }

    /// `intake` fans out to every declared read, which join on `assess` — a
    /// host tool that reads the answers out of merged state.
    fn plan(&self, reads: Value) -> Hash {
        let names: Vec<String> = reads.as_object().unwrap().keys().cloned().collect();
        let mut nodes = vec!["intake".to_string()];
        nodes.extend(names.iter().cloned());
        nodes.push("assess".into());
        let mut wf = Workflow::new(nodes);
        for n in &names {
            wf = wf.edge("intake", n).edge(n, "assess");
        }
        for tool in ["intake", "assess"] {
            let def = Tool::new(tool)
                .kind(ToolKind::Definition)
                .tool_description("test tool")
                .created_at(500)
                .namespace(RUN_NS);
            let h = self.facade.with_store(|m| m.add(&def)).unwrap();
            wf = wf.bind(tool, &h.to_hex());
        }
        let mut wf = wf.created_at(600).namespace(RUN_NS);
        wf.common.extra_fields.insert("reads".into(), reads);
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    /// What `db.entity_at(subject, relation, at, axis)` returns on the bindings,
    /// built the way both bindings build it.
    fn binding_entity_at(&self, subject: &str, relation: &str, at: i64, axis: Axis) -> Value {
        match self
            .facade
            .with_store(|m| m.entity_at(POLICY_NS, subject, relation, at, axis))
            .unwrap()
        {
            Some(g) => json!({"found": true, "grain": g}),
            None => json!({"found": false}),
        }
    }
}

fn opts() -> RunOptions {
    RunOptions {
        workers: 2,
        ..Default::default()
    }
}

fn as_of(axis: &str, relation: &str, at_from: &str) -> Value {
    json!({"op": "entity_at", "ns": POLICY_NS, "subject_from": "/policy_id",
           "relation": relation, "at_from": at_from, "axis": axis})
}

fn claim() -> Value {
    json!({"policy_id": "POL-4471", "date_of_loss": "2026-03-18",
           "asked_on": "2026-05-20", "today": "2026-08-01"})
}

fn object(v: &Value) -> Option<&str> {
    v.pointer("/grain/fields/object").and_then(Value::as_str)
}

/// Acceptance 1 and 3: the run's merged state holds exactly what
/// `db.entity_at` returns, on both axes — the backdated window found on the
/// world axis and `{"found": false}` on the knowledge axis before it was
/// received — read by a plan node, with no host call for any read.
#[test]
fn a_run_reads_both_clocks_of_its_own_memory() {
    let rig = Rig::new();
    rig.book_pol_4471();
    let plan = rig.plan(json!({
        "world_at_loss":     as_of("world", "mg:coverage_limit", "/date_of_loss"),
        "world_on_may20":    as_of("world", "mg:coverage_limit", "/asked_on"),
        "known_on_may20":    as_of("knowledge", "mg:coverage_limit", "/asked_on"),
        "world_today":       as_of("world", "mg:coverage_limit", "/today"),
        "known_today":       as_of("knowledge", "mg:coverage_limit", "/today"),
        "deductible_world":  as_of("world", "mg:deductible", "/date_of_loss"),
        "deductible_known":  as_of("knowledge", "mg:deductible", "/date_of_loss"),
    }));
    let runner = rig.runner("user:desk");
    let session = runner.start(&plan, "claim-8801", claim(), &opts()).unwrap();
    assert!(
        matches!(
            session,
            RunSession::Finished {
                outcome: RunOutcome::Completed,
                ..
            }
        ),
        "{session:?}"
    );

    // The reads were the runtime's: the host executor saw only the two tools.
    let mut names = rig.exec.names();
    names.sort();
    assert_eq!(names, vec!["assess", "intake"], "no read reached a tool");

    let state = rig.exec.input_of("assess");
    let (loss, may20, today) = (ms("2026-03-18"), ms("2026-05-20"), ms("2026-08-01"));
    let expect = [
        ("world_at_loss", "mg:coverage_limit", loss, Axis::World),
        ("world_on_may20", "mg:coverage_limit", may20, Axis::World),
        (
            "known_on_may20",
            "mg:coverage_limit",
            may20,
            Axis::Knowledge,
        ),
        ("world_today", "mg:coverage_limit", today, Axis::World),
        ("known_today", "mg:coverage_limit", today, Axis::Knowledge),
        ("deductible_world", "mg:deductible", loss, Axis::World),
        ("deductible_known", "mg:deductible", loss, Axis::Knowledge),
    ];
    for (key, relation, at, axis) in expect {
        assert_eq!(
            state[key],
            rig.binding_entity_at("POL-4471", relation, at, axis),
            "{key}: the run must see exactly what db.entity_at returns"
        );
    }

    // (a) the centrepiece: on the date of loss the cover in force was 500,000,
    //     in a window that has since closed.
    assert_eq!(object(&state["world_at_loss"]), Some("500000"));
    assert!(state["world_at_loss"]
        .pointer("/grain/fields/valid_to")
        .is_some_and(|v| !v.is_null()));
    // (b) the divergence: on 20 May 750,000 was in force and nobody knew it.
    assert_eq!(object(&state["world_on_may20"]), Some("750000"));
    assert_eq!(state["known_on_may20"], json!({"found": false}));
    // (c) and by today the clocks agree.
    assert_eq!(object(&state["world_today"]), Some("750000"));
    assert_eq!(object(&state["known_today"]), Some("750000"));
    // The correction runs the other way: true in March, believed otherwise.
    assert_eq!(object(&state["deductible_world"]), Some("10000"));
    assert_eq!(object(&state["deductible_known"]), Some("5000"));
}

/// Acceptance 2: every read is in the run journal with its axis, its instant
/// and the grain hash — and `verify` reproduces the run from the journal after
/// the file has moved on, never re-reading it.
#[test]
fn a_read_is_journaled_and_verify_survives_the_file_moving_on() {
    let rig = Rig::new();
    rig.book_pol_4471();
    let plan = rig.plan(json!({
        "cover_at_loss": {"op": "entity_at", "ns": POLICY_NS, "subject_from": "/policy_id",
                          "relation": "mg:coverage_limit", "at_from": "/date_of_loss",
                          "into": "cover"},
        "known_on_may20": as_of("knowledge", "mg:coverage_limit", "/asked_on"),
    }));
    let runner = rig.runner("user:desk");
    runner.start(&plan, "claim-j", claim(), &opts()).unwrap();

    let trace = rig
        .facade
        .with_store(|m| m.run_trace(RUN_NS, "claim-j", 1024))
        .unwrap();
    let reads: Vec<_> = trace
        .iter()
        .filter(|g| g.get_str("tool_name") == Some("mg:entity_at"))
        .filter(|g| g.fields.contains_key("read"))
        .collect();
    assert_eq!(reads.len(), 2, "one result grain per read");
    let by_node = |node: &str| {
        reads
            .iter()
            .find(|g| g.get_str("node") == Some(node))
            .copied()
            .expect(node)
    };
    let loss = by_node("cover_at_loss");
    let expected_hash = rig
        .facade
        .with_store(|m| {
            m.entity_at(
                POLICY_NS,
                "POL-4471",
                "mg:coverage_limit",
                ms("2026-03-18"),
                Axis::World,
            )
        })
        .unwrap()
        .unwrap()
        .hash
        .to_hex();
    assert_eq!(
        loss.fields["read"],
        json!({"op": "entity_at", "ns": POLICY_NS, "subject": "POL-4471",
               "relation": "mg:coverage_limit", "at": ms("2026-03-18"), "axis": "world",
               "grain": expected_hash})
    );
    // `into` renamed the state key; the result content is the merged answer.
    let content: Value = serde_json::from_str(loss.get_str("tool_content").unwrap()).unwrap();
    assert_eq!(content["cover"]["grain"]["hash"], json!(expected_hash));
    let known = by_node("known_on_may20");
    assert_eq!(known.fields["read"]["axis"], "knowledge");
    assert_eq!(known.fields["read"]["at"], json!(ms("2026-05-20")));
    assert_eq!(
        known.fields["read"]["grain"],
        Value::Null,
        "nothing was known: no grain"
    );

    // The file moves on: the policy is cancelled from 1 June. A re-read would
    // now answer differently for 20 May; verify must not re-read.
    rig.facade
        .with_store(|m| {
            let head = m
                .latest(POLICY_NS, "POL-4471", "mg:coverage_limit")?
                .unwrap();
            m.supersede(
                &head.hash,
                &mut Fact::new("POL-4471", "mg:coverage_limit", "750000")
                    .namespace(POLICY_NS)
                    .valid_from(ms("2026-05-01"))
                    .valid_to(ms("2026-06-01"))
                    .created_at(ms("2026-06-15")),
            )
        })
        .unwrap();
    let calls_before = rig.exec.names().len();
    let report = runner.verify("claim-j").unwrap();
    assert!(
        report.verified,
        "verify answers reads from the journal: {report:?}"
    );
    assert_eq!(
        rig.exec.names().len(),
        calls_before,
        "verify executed nothing"
    );
}

/// `related` is served the same way, in `db.related`'s shape.
#[test]
fn a_related_walk_is_served_in_the_bindings_shape() {
    let rig = Rig::new();
    rig.book_pol_4471();
    let plan = rig.plan(json!({
        "siblings": {"op": "related", "ns": POLICY_NS, "start": "ACME-HOLDINGS",
                     "relations": ["mg:owned_by"], "direction": "in", "depth": 1},
    }));
    rig.runner("user:desk")
        .start(&plan, "walk-1", claim(), &opts())
        .unwrap();
    let reached = rig
        .facade
        .with_store(|m| {
            m.related(
                POLICY_NS,
                "ACME-HOLDINGS",
                &["mg:owned_by"],
                areev_store::Direction::In,
                1,
                64,
            )
        })
        .unwrap();
    assert_eq!(reached.len(), 2, "both policies owned by the insured");
    assert_eq!(
        rig.exec.input_of("assess")["siblings"],
        json!({"start": "ACME-HOLDINGS", "reached": reached})
    );
}

/// A read whose pointer lands on nothing fails its NODE — not retried, since
/// the same state would fail the same way — and reaches no tool.
#[test]
fn an_unresolvable_operand_fails_the_node_without_a_retry() {
    let rig = Rig::new();
    rig.book_pol_4471();
    let plan = rig.plan(json!({"cover": as_of("world", "mg:coverage_limit", "/date_of_loss")}));
    let session = rig
        .runner("user:desk")
        .start(
            &plan,
            "claim-bad",
            json!({"policy_id": "POL-4471"}),
            &opts(),
        )
        .unwrap();
    let RunSession::Finished { outcome, .. } = session else {
        panic!("{session:?}")
    };
    let RunOutcome::Failed { node, detail } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(node, "cover");
    assert!(
        detail.contains("/date_of_loss does not resolve"),
        "{detail}"
    );
    assert_eq!(rig.exec.names(), vec!["intake"], "assess never ran");
    let attempts = rig
        .facade
        .with_store(|m| m.run_trace(RUN_NS, "claim-bad", 1024))
        .unwrap()
        .iter()
        .filter(|g| {
            g.get_str("node") == Some("cover") && g.get_str("tool_name") == Some("mg:entity_at")
        })
        .filter(|g| g.get_str("status") == Some("failed"))
        .count();
    assert_eq!(attempts, 1, "schema failures are not retried");
}

/// The scope is the run's own namespace, on the plan, before the run exists:
/// a sibling tenant is refused, and so is a namespace the session cannot read.
#[test]
fn a_read_outside_the_runs_reach_refuses_at_start() {
    use areev_core::authz::{AUTHZ_NS, REL_PERMITS};

    let rig = Rig::new();
    rig.book_pol_4471();
    let sibling = rig.plan(json!({
        "peek": {"op": "entity_at", "ns": "org.other", "subject": "POL-4471",
                 "relation": "mg:coverage_limit", "at": "2026-03-18"},
    }));
    let err = rig
        .runner("user:desk")
        .start(&sibling, "peek-1", claim(), &opts())
        .unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");
    assert!(
        err.to_string().contains("outside this run's namespace"),
        "{err}"
    );

    // A governed session holding run.execute on the run's namespace but no
    // read on the policies it declares.
    rig.facade
        .with_store(|m| {
            m.add(
                &Fact::new(
                    "agent:desk",
                    REL_PERMITS,
                    "read,run.execute,write ON org.uw",
                )
                .namespace(AUTHZ_NS)
                .created_at(900),
            )
        })
        .unwrap();
    rig.facade.bind_principal("agent:desk").unwrap();
    let plan = rig.plan(json!({"cover": as_of("world", "mg:coverage_limit", "/date_of_loss")}));
    let runner = rig.runner("agent:desk");
    let err = runner
        .start(&plan, "claim-g", claim(), &opts())
        .unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");
    assert!(
        err.to_string()
            .contains("declares a read of 'org.uw.policies'"),
        "{err}"
    );
    assert!(
        runner.inspect("claim-g").is_err(),
        "a refused start leaves no run behind"
    );
    assert!(rig.exec.names().is_empty());
}

/// A read node cannot also bind a tool: the node would otherwise be answerable
/// by either party.
#[test]
fn a_read_node_that_binds_a_tool_is_refused() {
    let rig = Rig::new();
    let tool = rig
        .facade
        .with_store(|m| {
            m.add(
                &Tool::new("cover")
                    .kind(ToolKind::Definition)
                    .created_at(500)
                    .namespace(RUN_NS),
            )
        })
        .unwrap();
    let mut wf = Workflow::new(vec!["cover".into()])
        .bind("cover", &tool.to_hex())
        .created_at(600)
        .namespace(RUN_NS);
    wf.common.extra_fields.insert(
        "reads".into(),
        json!({"cover": as_of("world", "mg:coverage_limit", "/d")}),
    );
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
    let err = rig
        .runner("user:desk")
        .start(&plan, "both-1", claim(), &opts())
        .unwrap_err();
    assert!(matches!(err, RunError::InvalidPlan { .. }), "{err}");
    assert!(err.to_string().contains("both binds"), "{err}");
}

/// Acceptance 5, at the one place a tool could be handed the read: the pool.
/// A memory read that ever reached it fails without the host executor — and
/// so without any `--tool-cmd`, native blob or capability module — being
/// consulted at all.
#[test]
fn the_tool_pool_never_answers_a_memory_read() {
    let exec = Arc::new(Recorder::default());
    let pool =
        areev_run::executor::Pool::new(Arc::clone(&exec) as Arc<dyn HostToolExecutor>, None, 1);
    let key = JournalKey {
        run_id: "r".into(),
        task_path: String::new(),
        node: "cover".into(),
        attempt: 1,
        effect_seq: 0,
        kind: areev_run_core::EffectKind::Tool,
    };
    pool.submit(areev_run::executor::DispatchJob {
        key,
        executor: NodeExecutor::MemoryRead {
            op: "entity_at".into(),
            spec: as_of("world", "mg:coverage_limit", "/d"),
        },
        input: json!({"d": 1}),
        idempotency_key: "k".into(),
        llm: None,
        code: None,
    });
    let done = pool.done_rx.recv().unwrap();
    let EffectOutcome::Failed { cause, detail, .. } = done.outcome else {
        panic!()
    };
    assert_eq!(cause, FailCause::Unknown);
    assert!(detail.contains("never a tool"), "{detail}");
    assert!(
        exec.names().is_empty(),
        "the host executor was never called"
    );
}

/// A fan-out of reads: each `$send` task resolves the read's pointers against
/// its OWN input, and an `append` reducer gathers the answers.
#[test]
fn send_fans_a_read_out_per_task() {
    struct Spawner;
    impl HostToolExecutor for Spawner {
        fn execute(&self, tool: &str, _h: &str, _input: &Value, _k: &str) -> ExecResult {
            match tool {
                "intake" => ExecResult::Ok(json!({"$send": [
                    {"node": "limit_at", "input": {"policy_id": "POL-4471", "at": "2026-03-18"}},
                    {"node": "limit_at", "input": {"policy_id": "POL-4471", "at": "2026-05-20"}},
                ]})),
                _ => ExecResult::Ok(json!({})),
            }
        }
    }
    let rig = Rig::new();
    rig.book_pol_4471();
    let def = rig
        .facade
        .with_store(|m| {
            m.add(
                &Tool::new("intake")
                    .kind(ToolKind::Definition)
                    .created_at(500)
                    .namespace(RUN_NS),
            )
        })
        .unwrap();
    let mut wf = Workflow::new(vec!["intake".into(), "limit_at".into()])
        .edge("intake", "limit_at")
        .bind("intake", &def.to_hex())
        .created_at(600)
        .namespace(RUN_NS);
    wf.common.extra_fields.insert(
        "reads".into(),
        json!({"limit_at": {"op": "entity_at", "ns": POLICY_NS, "subject_from": "/policy_id",
                            "relation": "mg:coverage_limit", "at_from": "/at", "into": "limits"}}),
    );
    wf.common
        .extra_fields
        .insert("reducers".into(), json!({"limits": "append"}));
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
    let mut runner = rig.runner("user:desk");
    runner.executor = Arc::new(Spawner);
    let session = runner.start(&plan, "fan-1", json!({}), &opts()).unwrap();
    assert!(
        matches!(
            session,
            RunSession::Finished {
                outcome: RunOutcome::Completed,
                ..
            }
        ),
        "{session:?}"
    );
    let view = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, RUN_NS, "fan-1"))
        .unwrap();
    let limits = view.checkpoints.last().unwrap().scheduler["context"]["limits"].clone();
    let objects: Vec<&str> = limits
        .as_array()
        .unwrap()
        .iter()
        .filter_map(object)
        .collect();
    assert_eq!(objects, vec!["500000", "750000"], "{limits}");
    assert!(runner.verify("fan-1").unwrap().verified);
}

/// Rehearsal must see reads too. A DRAFT plan (`shadow --plan-file`, a
/// loop-drafted `plan_revision`) is not in the store, so its `reads` ride only
/// in the body — resolved without them, every read node would turn into an
/// LLM step and the rehearsal would report a run it never rehearsed as out of
/// support.
#[test]
fn a_draft_plan_with_reads_rehearses_against_its_journaled_runs() {
    let rig = Rig::new();
    rig.book_pol_4471();
    let plan = rig.plan(json!({"cover": as_of("world", "mg:coverage_limit", "/date_of_loss")}));
    let runner = rig.runner("user:desk");
    runner.start(&plan, "claim-s", claim(), &opts()).unwrap();

    let body: serde_json::Map<String, Value> = rig
        .facade
        .with_store(|m| m.get(&plan))
        .unwrap()
        .fields
        .into_iter()
        .collect();
    assert!(body.contains_key("reads"));
    let report = runner
        .shadow_plan(&["claim-s".into()], &areev_run::PlanCandidate::Body(body))
        .unwrap();
    let r = &report.runs[0];
    assert_eq!(
        (
            r.verdict.as_str(),
            r.out_of_support.len(),
            r.effects_replayed
        ),
        ("same", 0, 3),
        "intake, the read and assess all answered from the journal: {r:?}"
    );
    assert_eq!(report.effect_dispatches, 0);
    assert_eq!(report.writes, 0);
}

// ---------------------------------------------------------------------------
// `op: recall` (#342): a bounded recall over the run's own records.
// ---------------------------------------------------------------------------

const LEDGER_NS: &str = "org.uw.ledger";

/// Six monthly statements for one account, oldest first.
fn book_statements(rig: &Rig) -> Vec<Hash> {
    rig.facade
        .with_store(|m| {
            (1..=6)
                .map(|month| {
                    m.add(
                        &Fact::new("ACC-7", "mg:statement", &format!("2026-0{month} balance"))
                            .namespace(LEDGER_NS)
                            .created_at(ms(&format!("2026-0{month}-28"))),
                    )
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap()
}

/// What `db.recall(subject, relation, k, ns)` returns on the bindings, built
/// the way both bindings build it.
fn recall_shape(grains: &[areev_core::format::DeserializedGrain]) -> Value {
    Value::Array(
        grains
            .iter()
            .map(|g| {
                json!({"hash": g.hash.to_hex(),
                       "type": format!("{:?}", g.grain_type).to_lowercase(),
                       "fields": g.fields})
            })
            .collect(),
    )
}

fn recall_trace(rig: &Rig, run_id: &str) -> Vec<areev_core::format::DeserializedGrain> {
    rig.facade
        .with_store(|m| m.run_trace(RUN_NS, run_id, 1024))
        .unwrap()
        .into_iter()
        .filter(|g| g.get_str("tool_name") == Some("mg:recall"))
        .filter(|g| g.fields.contains_key("read"))
        .collect()
}

/// "The last three statements for this account": the run's state holds
/// exactly what `db.recall` returns, never more than `k`, with the subject
/// taken from state — and a recall naming no relation reads every relation.
#[test]
fn a_recall_returns_what_db_recall_returns_bounded_by_k() {
    let rig = Rig::new();
    let booked = book_statements(&rig);
    let plan = rig.plan(json!({
        "last_three": {"op": "recall", "ns": LEDGER_NS, "subject_from": "/account",
                       "relation": "mg:statement", "k": 3},
        "everything": {"op": "recall", "ns": LEDGER_NS, "subject": "ACC-7"},
    }));
    let session = rig
        .runner("user:desk")
        .start(&plan, "stmt-1", json!({"account": "ACC-7"}), &opts())
        .unwrap();
    assert!(
        matches!(session, RunSession::Finished { outcome: RunOutcome::Completed, .. }),
        "{session:?}"
    );
    let mut names = rig.exec.names();
    names.sort();
    assert_eq!(names, vec!["assess", "intake"], "no read reached a tool");

    let state = rig.exec.input_of("assess");
    let direct = rig
        .facade
        .with_store(|m| m.recall(LEDGER_NS, "ACC-7", Some("mg:statement"), 3))
        .unwrap();
    assert_eq!(state["last_three"], recall_shape(&direct));
    let got: Vec<&str> = state["last_three"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|g| g["hash"].as_str())
        .collect();
    let newest: Vec<String> = booked.iter().rev().take(3).map(|h| h.to_hex()).collect();
    assert_eq!(got, newest, "the three newest, newest first — never more than k");

    let all = rig
        .facade
        .with_store(|m| m.recall(LEDGER_NS, "ACC-7", None, 16))
        .unwrap();
    assert_eq!(all.len(), 6, "the default k (16) is above the six there are");
    assert_eq!(state["everything"], recall_shape(&all));
}

/// With an instant it is an as-of recall: per relation, exactly what
/// `entity_at` answers on that clock — so the backdated endorsement and the
/// restated deductible read the same from a recall as from the as-of read.
#[test]
fn an_as_of_recall_answers_both_clocks_like_entity_at() {
    let rig = Rig::new();
    rig.book_pol_4471();
    let recall_at = |axis: &str| {
        json!({"op": "recall", "ns": POLICY_NS, "subject_from": "/policy_id",
               "at_from": "/date_of_loss", "axis": axis})
    };
    let plan = rig.plan(json!({
        "world_at_loss": recall_at("world"),
        "known_at_loss": recall_at("knowledge"),
        "limit_on_may20": {"op": "recall", "ns": POLICY_NS, "subject": "POL-4471",
                           "relation": "mg:coverage_limit", "k": 1, "at": "2026-05-20"},
    }));
    rig.runner("user:desk")
        .start(&plan, "asof-1", claim(), &opts())
        .unwrap();
    let state = rig.exec.input_of("assess");
    let loss = ms("2026-03-18");
    for (key, axis) in [("world_at_loss", Axis::World), ("known_at_loss", Axis::Knowledge)] {
        let direct = rig
            .facade
            .with_store(|m| m.recall_at(POLICY_NS, "POL-4471", None, 16, loss, axis))
            .unwrap();
        assert_eq!(state[key], recall_shape(&direct), "{key}");
    }
    let by_relation = |key: &str, relation: &str| -> Option<String> {
        state[key]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["fields"]["relation"] == relation)
            .and_then(|g| g["fields"]["object"].as_str().map(str::to_string))
    };
    // World: the cover in force on the date of loss, and the deductible as
    // it truly was (restated to 10,000 in June, true since January).
    assert_eq!(by_relation("world_at_loss", "mg:coverage_limit").as_deref(), Some("500000"));
    assert_eq!(by_relation("world_at_loss", "mg:deductible").as_deref(), Some("10000"));
    // Knowledge: what the desk believed — 5,000 — and a relation with no
    // knowledge-clock answer is absent, exactly as entity_at says.
    assert_eq!(by_relation("known_at_loss", "mg:deductible").as_deref(), Some("5000"));
    assert_eq!(
        by_relation("known_at_loss", "mg:coverage_limit"),
        rig.binding_entity_at("POL-4471", "mg:coverage_limit", loss, Axis::Knowledge)
            .pointer("/grain/fields/object")
            .and_then(Value::as_str)
            .map(str::to_string),
        "parity with entity_at on the knowledge clock"
    );
    // A named relation with k = 1 IS entity_at, in recall's shape.
    let as_of =
        rig.binding_entity_at("POL-4471", "mg:coverage_limit", ms("2026-05-20"), Axis::World);
    assert_eq!(state["limit_on_may20"][0]["hash"], as_of["grain"]["hash"]);
    assert_eq!(state["limit_on_may20"].as_array().unwrap().len(), 1);
}

/// The journal records the resolved operands and every result hash; verify
/// reproduces the run after the ledger has moved on, and `shadow` under the
/// same plan rehearses it without reading the file.
#[test]
fn a_recall_is_journaled_with_its_result_hashes_and_replays() {
    let rig = Rig::new();
    let booked = book_statements(&rig);
    rig.book_pol_4471();
    let plan = rig.plan(json!({
        "last_two": {"op": "recall", "ns": LEDGER_NS, "subject_from": "/account",
                     "relation": "mg:statement", "k": 2, "into": "stmts"},
        "cover_then": {"op": "recall", "ns": POLICY_NS, "subject": "POL-4471",
                       "relation": "mg:coverage_limit", "k": 4,
                       "at_from": "/date_of_loss", "axis": "world"},
    }));
    let runner = rig.runner("user:desk");
    let mut input = claim();
    input["account"] = json!("ACC-7");
    runner.start(&plan, "stmt-j", input, &opts()).unwrap();

    let reads = recall_trace(&rig, "stmt-j");
    assert_eq!(reads.len(), 2, "one result grain per read");
    let by_node = |node: &str| {
        reads
            .iter()
            .find(|g| g.get_str("node") == Some(node))
            .expect(node)
    };
    let newest_two: Vec<String> = booked.iter().rev().take(2).map(|h| h.to_hex()).collect();
    assert_eq!(
        by_node("last_two").fields["read"],
        json!({"op": "recall", "ns": LEDGER_NS, "subject": "ACC-7",
               "relation": "mg:statement", "k": 2, "grains": newest_two})
    );
    let content: Value =
        serde_json::from_str(by_node("last_two").get_str("tool_content").unwrap()).unwrap();
    assert_eq!(content["stmts"].as_array().unwrap().len(), 2, "`into` renamed the key");
    let cover = rig
        .facade
        .with_store(|m| {
            m.entity_at(POLICY_NS, "POL-4471", "mg:coverage_limit", ms("2026-03-18"), Axis::World)
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        by_node("cover_then").fields["read"],
        json!({"op": "recall", "ns": POLICY_NS, "subject": "POL-4471",
               "relation": "mg:coverage_limit", "k": 4, "at": ms("2026-03-18"),
               "axis": "world", "grains": [cover.hash.to_hex()]})
    );

    // The ledger moves on: a seventh statement. A re-read would now answer
    // differently; verify and shadow must not re-read.
    rig.facade
        .with_store(|m| {
            m.add(
                &Fact::new("ACC-7", "mg:statement", "2026-07 balance")
                    .namespace(LEDGER_NS)
                    .created_at(ms("2026-07-28")),
            )
        })
        .unwrap();
    let calls_before = rig.exec.names().len();
    assert!(runner.verify("stmt-j").unwrap().verified, "answered from the journal");
    let report = runner
        .shadow_plan(&["stmt-j".into()], &areev_run::PlanCandidate::Hash(plan))
        .unwrap();
    let r = &report.runs[0];
    assert_eq!((r.verdict.as_str(), r.out_of_support.len()), ("same", 0), "{r:?}");
    assert!(r.identity.as_ref().is_some_and(|i| i.consistent), "{r:?}");
    assert_eq!((report.effect_dispatches, report.writes), (0, 0));
    assert_eq!(rig.exec.names().len(), calls_before, "nothing executed");
}

/// Replay's teeth for a recall: a forged result — the answer swapped for
/// one the memory never gave — makes both verify and an identity shadow
/// diverge.
#[test]
fn a_tampered_recall_result_fails_verify_and_shadow() {
    let rig = Rig::new();
    book_statements(&rig);
    let plan = rig.plan(json!({
        "last_two": {"op": "recall", "ns": LEDGER_NS, "subject": "ACC-7",
                     "relation": "mg:statement", "k": 2},
    }));
    let runner = rig.runner("user:desk");
    runner.start(&plan, "stmt-t", json!({}), &opts()).unwrap();
    assert!(runner.verify("stmt-t").unwrap().verified);

    rig.facade.with_store(|m| {
        let v = areev_run::journal::load(m, RUN_NS, "stmt-t").unwrap();
        let entry = v.entries.values().find(|e| e.key.node == "last_two").unwrap().clone();
        let (result_hash, _) = entry.result.unwrap();
        let stored = m.get(&result_hash).unwrap();
        let mut forged = stored.to_tool().unwrap();
        forged.content = Some(
            json!({"last_two": [{"hash": "00", "type": "fact", "fields": {"object": "FORGED"}}]})
                .to_string(),
        );
        forged.common.created_at = Some(1_785_999_999_999);
        forged.common.namespace = Some(RUN_NS.into());
        forged = forged.step_action(&plan.to_hex(), "last_two");
        for key in [
            "run_id", "task_path", "node", "attempt", "effect_seq", "superstep",
            "effect_kind", "usage_input_tokens", "usage_output_tokens",
            "usage_usd_micros", "usage_journal_bytes", "read",
        ] {
            if let Some(val) = stored.fields.get(key) {
                forged.common.extra_fields.insert(key.into(), val.clone());
            }
        }
        m.supersede(&result_hash, &mut forged).unwrap();
    });

    let report = runner.verify("stmt-t").unwrap();
    assert!(!report.verified, "a forged recall must not verify: {report:?}");
    let shadow = runner
        .shadow_plan(&["stmt-t".into()], &areev_run::PlanCandidate::Hash(plan))
        .unwrap();
    let identity = shadow.runs[0].identity.as_ref().expect("identity replay");
    assert!(!identity.consistent, "{:?}", shadow.runs[0]);
}

/// The ceiling is the runtime's, not only the validator's: a pinned spec that
/// never went through `parse_reads` (a hand-edited, replicated manifest) and
/// asks for 65 fails the read instead of returning 65 grains, and a
/// pattern namespace is refused rather than widened into a scope.
#[test]
fn the_executed_recall_enforces_the_ceiling_on_a_pinned_spec() {
    let rig = Rig::new();
    rig.facade
        .with_store(|m| {
            for i in 0..70 {
                m.add(
                    &Fact::new("ACC-9", "mg:line", &format!("line {i}"))
                        .namespace(LEDGER_NS)
                        .created_at(1_000 + i),
                )?;
            }
            Ok::<_, areev_core::error::AreevError>(())
        })
        .unwrap();
    let exec = |spec: Value| areev_run::memread::execute(&rig.facade, RUN_NS, &spec, &json!({}));
    let base = json!({"op": "recall", "ns": LEDGER_NS, "into": "r", "subject": "ACC-9"});

    let mut over = base.clone();
    over["k"] = json!(65);
    let EffectOutcome::Failed { detail, .. } = exec(over).outcome else {
        panic!("a pinned k past the ceiling must fail")
    };
    assert!(detail.contains("outside 1..=64"), "{detail}");

    let mut at_max = base.clone();
    at_max["k"] = json!(64);
    let done = exec(at_max);
    let EffectOutcome::Completed { result, .. } = done.outcome else { panic!() };
    assert_eq!(result["r"].as_array().unwrap().len(), 64, "70 stored, 64 returned");
    assert_eq!(done.record.unwrap()["grains"].as_array().unwrap().len(), 64);

    let mut scoped = base;
    scoped["k"] = json!(5);
    scoped["ns"] = json!("org.uw.*");
    let EffectOutcome::Failed { detail, .. } = exec(scoped).outcome else {
        panic!("a pattern namespace must fail")
    };
    assert!(detail.contains("refused"), "{detail}");
}

/// The ceiling and the scope, on the plan, before the run exists: `k` past
/// 64 (or 0) is refused, free text is an unknown key, and so is a namespace
/// the session cannot read — and a refused start leaves no run behind.
#[test]
fn a_recall_past_its_ceiling_or_grant_refuses_at_start() {
    use areev_core::authz::{AUTHZ_NS, REL_PERMITS};

    let rig = Rig::new();
    book_statements(&rig);
    for k in [0, 65, 1000] {
        let plan = rig.plan(json!({
            "all": {"op": "recall", "ns": LEDGER_NS, "subject": "ACC-7", "k": k},
        }));
        let run_id = format!("k-{k}");
        let runner = rig.runner("user:desk");
        let err = runner.start(&plan, &run_id, json!({}), &opts()).unwrap_err();
        assert!(matches!(err, RunError::InvalidPlan { .. }), "k={k}: {err}");
        assert!(err.to_string().contains("`k` must be an integer from 1 to 64"), "{err}");
        assert!(runner.inspect(&run_id).is_err(), "no run left behind");
    }
    let noisy = rig.plan(json!({
        "all": {"op": "recall", "ns": LEDGER_NS, "subject": "ACC-7", "query": "unpaid"},
    }));
    let err = rig
        .runner("user:desk")
        .start(&noisy, "free-text", json!({}), &opts())
        .unwrap_err();
    assert!(err.to_string().contains("unknown key `query`"), "{err}");

    rig.facade
        .with_store(|m| {
            m.add(
                &Fact::new("agent:desk", REL_PERMITS, "read,run.execute,write ON org.uw")
                    .namespace(AUTHZ_NS)
                    .created_at(900),
            )
        })
        .unwrap();
    rig.facade.bind_principal("agent:desk").unwrap();
    let plan = rig.plan(json!({
        "all": {"op": "recall", "ns": LEDGER_NS, "subject": "ACC-7", "k": 5},
    }));
    let runner = rig.runner("agent:desk");
    let err = runner.start(&plan, "g-1", json!({}), &opts()).unwrap_err();
    assert!(matches!(err, RunError::Unauthorized { .. }), "{err}");
    assert!(err.to_string().contains("declares a read of 'org.uw.ledger'"), "{err}");
    assert!(runner.inspect("g-1").is_err());
    assert!(rig.exec.names().is_empty());
}
