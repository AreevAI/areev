//! Plan-change shadow: journaled runs re-driven through the pure scheduler
//! under a CANDIDATE plan, every effect answered from the journal, nothing
//! dispatched, nothing written. The out-of-support rule is the scope limit
//! Dream-RSI states and this module keeps: a candidate that asks for an
//! effect the journal never recorded earns no score.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    ExecResult, HostToolExecutor, PlanCandidate, RunOptions, Runner, RunSession, ScriptedClock,
};
use areev_run_core::{FailCause, RunOutcome};
use areev_store::Areev;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Counts every live call; `b` fails its first `fail_b` calls; `b` reports
/// `done: true` only from its `done_after`th call.
struct Exec {
    calls: AtomicU64,
    b_calls: AtomicU64,
    fail_b: u64,
    done_after: u64,
}

impl Exec {
    fn new(fail_b: u64, done_after: u64) -> Arc<Self> {
        Arc::new(Exec { calls: AtomicU64::new(0), b_calls: AtomicU64::new(0), fail_b, done_after })
    }
}

impl HostToolExecutor for Exec {
    fn execute(&self, tool_name: &str, _h: &str, _in: &Value, _idem: &str) -> ExecResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if tool_name == "b" {
            let n = self.b_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.fail_b {
                return ExecResult::Err { cause: FailCause::ExecutorError, detail: format!("b down #{n}") };
            }
            return ExecResult::Ok(json!({"done": n >= self.done_after}));
        }
        ExecResult::Ok(json!({tool_name.to_string(): true}))
    }
}

struct Fx {
    _dir: TempDir,
    facade: Arc<AreevFacade>,
    exec: Arc<Exec>,
    defs: Map<String, Value>,
}

fn fixture(fail_b: u64, done_after: u64) -> Fx {
    let dir = TempDir::new().unwrap();
    let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = Arc::new(AreevFacade::new(m));
    let mut defs = Map::new();
    for n in ["a", "b", "c", "d"] {
        let def = Tool::new(n).kind(ToolKind::Definition).tool_description("t").created_at(500).namespace("ops");
        let dh = facade.with_store(|m| m.add(&def)).unwrap();
        defs.insert(n.into(), json!(dh.to_hex()));
    }
    Fx { _dir: dir, facade, exec: Exec::new(fail_b, done_after), defs }
}

impl Fx {
    fn bind(&self, mut wf: Workflow, nodes: &[&str]) -> Workflow {
        for n in nodes {
            wf = wf.bind(n, self.defs[*n].as_str().unwrap());
        }
        wf
    }
    fn store(&self, wf: Workflow) -> Hash {
        self.facade.with_store(|m| m.add(&wf.created_at(600).namespace("ops"))).unwrap()
    }
    fn runner(&self) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new((0..400).map(|i| 1_757_000_000_000 + i * 10).collect())),
            executor: Arc::clone(&self.exec) as Arc<dyn HostToolExecutor>,
            llm: None,
            observer: None,
            ns: "ops".into(),
            principal: "user:runner".into(),
        }
    }
    fn body(&self, wf: &Workflow) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("nodes".into(), json!(wf.nodes));
        m.insert(
            "edges".into(),
            json!(wf
                .edges
                .iter()
                .map(|e| {
                    let mut o = json!({"src": e.src, "dst": e.dst});
                    if let Some(c) = &e.cond {
                        o["cond"] = json!(c);
                    }
                    if let Some(n) = e.max_cycles {
                        o["max_cycles"] = json!(n);
                    }
                    o
                })
                .collect::<Vec<_>>()),
        );
        m.insert("bindings".into(), json!(wf.bindings));
        m.insert("retries".into(), json!(wf.retries));
        m
    }
    fn ops(&self) -> usize {
        self.facade.with_store(|m| m.stats()).unwrap().ops
    }
}

fn run(runner: &Runner, plan: &Hash, id: &str) -> RunOutcome {
    match runner.start(plan, id, json!({"q": id}), &RunOptions { workers: 1, ..Default::default() }).unwrap() {
        RunSession::Finished { outcome, .. } => outcome,
        other => panic!("unexpected session: {other:?}"),
    }
}

/// Identity: the candidate equal to the incumbent reproduces `verify`
/// checkpoint for checkpoint, replays every effect, is out of support
/// nowhere, and spends exactly what the incumbent spent.
#[test]
fn identity_reproduces_verify_with_nothing_out_of_support() {
    let fx = fixture(0, 1);
    let plan = fx.store(fx.bind(Workflow::new(vec!["a".into(), "b".into()]).edge("a", "b"), &["a", "b"]));
    let runner = fx.runner();
    for id in ["r1", "r2", "r3"] {
        assert_eq!(run(&runner, &plan, id), RunOutcome::Completed);
    }
    let live = fx.exec.calls.load(Ordering::SeqCst);
    let ops = fx.ops();
    let ids: Vec<String> = ["r1", "r2", "r3"].iter().map(|s| s.to_string()).collect();
    let report = runner.shadow_plan(&ids, &PlanCandidate::Hash(plan)).unwrap();
    assert_eq!(report.runs.len(), 3);
    for r in &report.runs {
        let id = r.identity.as_ref().expect("identity is checked when the plan is the incumbent's");
        assert!(id.consistent && id.checkpoints_compared > 0, "{r:?}");
        let verify = runner.verify(&r.run_id).unwrap();
        assert_eq!(id.checkpoints_compared, verify.steps.len(), "one comparison per verify step: {r:?}");
        assert_eq!((r.effects_replayed, r.out_of_support.len(), r.verdict.as_str()), (2, 0, "same"), "{r:?}");
        assert_eq!((r.incumbent_outcome.as_str(), r.candidate_outcome.as_str()), ("completed", "completed"));
        assert_eq!(r.candidate_spent, r.incumbent_spent, "identity consumes exactly the incumbent's spend");
    }
    assert!(report.no_worse);
    assert_eq!(report.out_of_support_fraction, 0.0);
    assert_eq!((report.effect_dispatches, report.writes), (0, 0));
    assert_eq!(fx.exec.calls.load(Ordering::SeqCst), live, "the shadow dispatched nothing");
    assert_eq!(fx.ops(), ops, "the shadow wrote nothing");
}

/// Retry reduction: a run that completed on b's third attempt, rehearsed
/// under `retries: 1`, fails at b — and only attempt 1 counts as replayed.
#[test]
fn a_lower_retry_count_replays_as_failed_at_that_node() {
    let fx = fixture(2, 1);
    let wf = fx.bind(Workflow::new(vec!["a".into(), "b".into()]).edge("a", "b").retry("b", 3), &["a", "b"]);
    let plan = fx.store(wf.clone());
    let runner = fx.runner();
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed, "b succeeds on attempt 3");
    let live = fx.exec.calls.load(Ordering::SeqCst);
    // `retries` counts attempts AFTER the first: 0 → attempt 1 only.
    let mut cand = wf.clone();
    cand.retries.insert("b".into(), 0);
    let report = runner.shadow_plan(&["r1".into()], &PlanCandidate::Body(fx.body(&cand))).unwrap();
    assert!(report.candidate_is_draft);
    let r = &report.runs[0];
    assert_eq!(r.candidate_outcome, "failed", "{r:?}");
    assert_eq!(r.incumbent_outcome, "completed");
    assert_eq!(r.verdict, "worse");
    assert_eq!(r.effects_replayed, 2, "a, then b's attempt 1 only: {r:?}");
    assert!(r.out_of_support.is_empty());
    assert!(!report.no_worse);
    // One retry: attempts 1 and 2 are consumed, still failed.
    cand.retries.insert("b".into(), 1);
    let report = runner.shadow_plan(&["r1".into()], &PlanCandidate::Body(fx.body(&cand))).unwrap();
    assert_eq!((report.runs[0].candidate_outcome.as_str(), report.runs[0].effects_replayed), ("failed", 3));
    assert_eq!(fx.exec.calls.load(Ordering::SeqCst), live);
}

/// Cycle bound: a run that looped through `b` three times under
/// `max_cycles: 5` before `done` came true, rehearsed under `max_cycles: 1`,
/// exhausts the bound after two and stalls — consuming only the two
/// journaled attempts it asked for.
#[test]
fn a_tighter_cycle_bound_replays_as_stalled() {
    let fx = fixture(0, 3);
    let mut wf = fx.bind(Workflow::new(vec!["b".into(), "c".into()]).cond_edge("b", "c", "done == true"), &["b", "c"]);
    wf.edges.push(areev_core::types::WorkflowEdge {
        src: "b".into(),
        dst: "b".into(),
        cond: Some("done != true".into()),
        max_cycles: Some(5),
    });
    let plan = fx.store(wf.clone());
    let runner = fx.runner();
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed, "b, b, b, c");
    assert_eq!(fx.exec.b_calls.load(Ordering::SeqCst), 3);
    let live = fx.exec.calls.load(Ordering::SeqCst);
    let mut cand = wf.clone();
    cand.edges.iter_mut().filter(|e| e.dst == "b").for_each(|e| e.max_cycles = Some(1));
    let report = runner.shadow_plan(&["r1".into()], &PlanCandidate::Body(fx.body(&cand))).unwrap();
    let r = &report.runs[0];
    assert_eq!(r.candidate_outcome, "stalled", "{r:?}");
    assert_eq!(r.verdict, "worse");
    assert!(r.out_of_support.is_empty(), "every effect it asked for was journaled: {r:?}");
    assert_eq!(r.effects_replayed, 2, "b#1 and b#2; b#3 and c are never consumed: {r:?}");
    assert!(r.candidate_spent.usd_micros <= r.incumbent_spent.usd_micros);
    assert_eq!(fx.exec.calls.load(Ordering::SeqCst), live);
}

/// Condition change: the candidate routes to a node the live run never
/// executed — no journal rows for it → out of support, no score.
#[test]
fn a_rewritten_condition_that_needs_an_unjournaled_node_is_out_of_support() {
    let fx = fixture(0, 1);
    let wf = fx.bind(
        Workflow::new(vec!["a".into(), "b".into(), "c".into(), "d".into()])
            .edge("a", "b")
            .cond_edge("b", "c", "done == true")
            .cond_edge("b", "d", "done != true"),
        &["a", "b", "c", "d"],
    );
    let plan = fx.store(wf.clone());
    let runner = fx.runner();
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed, "a, b, c — d never runs");
    let live = fx.exec.calls.load(Ordering::SeqCst);
    let mut cand = wf.clone();
    for e in cand.edges.iter_mut() {
        if e.dst == "c" {
            e.cond = Some("done != true".into());
        } else if e.dst == "d" {
            e.cond = Some("done == true".into());
        }
    }
    let report = runner.shadow_plan(&["r1".into()], &PlanCandidate::Body(fx.body(&cand))).unwrap();
    let r = &report.runs[0];
    assert_eq!(r.candidate_outcome, "out_of_support", "{r:?}");
    assert_eq!(r.verdict, "out_of_support");
    assert_eq!(r.out_of_support, vec!["d@1#0/tool".to_string()], "{r:?}");
    assert_eq!(r.effects_replayed, 2, "a and b were consumed; c was not needed, d has no rows");
    assert_eq!(report.totals.out_of_support, 1);
    assert_eq!(report.out_of_support_fraction, 1.0);
    assert!(!report.no_worse, "no scored run → no claim");
    assert_eq!(fx.exec.calls.load(Ordering::SeqCst), live, "out of support dispatches nothing");
}

/// Rebound node: renaming a node leaves it with no journal rows.
#[test]
fn a_renamed_node_is_out_of_support_and_the_executor_never_runs() {
    let fx = fixture(0, 1);
    let wf = fx.bind(Workflow::new(vec!["a".into(), "b".into()]).edge("a", "b"), &["a", "b"]);
    let plan = fx.store(wf.clone());
    let runner = fx.runner();
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed);
    let live = fx.exec.calls.load(Ordering::SeqCst);
    let mut cand = Workflow::new(vec!["a".into(), "c".into()]).edge("a", "c");
    cand = fx.bind(cand, &["a", "c"]);
    let report = runner.shadow_plan(&["r1".into()], &PlanCandidate::Body(fx.body(&cand))).unwrap();
    let r = &report.runs[0];
    assert_eq!(r.verdict, "out_of_support");
    assert_eq!(r.out_of_support, vec!["c@1#0/tool".to_string()]);
    assert_eq!(fx.exec.calls.load(Ordering::SeqCst), live);
}

/// A candidate that is a subset of the incumbent consumes a subset of its
/// spend; a draft that does not validate surfaces the existing RUN error.
#[test]
fn spend_is_bounded_by_what_was_consumed_and_a_bad_draft_is_refused() {
    let fx = fixture(0, 1);
    let wf = fx.bind(Workflow::new(vec!["a".into(), "b".into(), "c".into()]).edge("a", "b").edge("b", "c"), &["a", "b", "c"]);
    let plan = fx.store(wf.clone());
    let runner = fx.runner();
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed);
    // A shorter candidate: a → b only, still completes, spends no more.
    let short = fx.bind(Workflow::new(vec!["a".into(), "b".into()]).edge("a", "b"), &["a", "b"]);
    let report = runner.shadow_plan(&["r1".into()], &PlanCandidate::Body(fx.body(&short))).unwrap();
    let r = &report.runs[0];
    assert_eq!((r.candidate_outcome.as_str(), r.verdict.as_str(), r.effects_replayed), ("completed", "same", 2));
    assert!(r.candidate_spent.usd_micros <= r.incumbent_spent.usd_micros);
    // A draft with an unbounded cycle fails V-validation before any run is read.
    let mut bad = fx.body(&wf);
    bad["edges"].as_array_mut().unwrap().push(json!({"src": "c", "dst": "a"}));
    let err = runner.shadow_plan(&["r1".into()], &PlanCandidate::Body(bad)).unwrap_err();
    assert!(err.to_string().starts_with("RUN-E"), "{err}");
}

/// A canceled run rehearses as canceled: the cancel the live driver saw is
/// fed at the same superstep, so the candidate's terminal label matches.
#[test]
fn a_canceled_run_shadows_as_canceled_under_a_candidate() {
    let fx = fixture(0, 1);
    let nodes: Vec<String> = (0..4).map(|i| format!("s{i}")).collect();
    let mut wf = Workflow::new(nodes.clone());
    for w in nodes.windows(2) {
        wf = wf.edge(&w[0], &w[1]);
    }
    let mut defs = Map::new();
    for n in &nodes {
        let def = Tool::new(n).kind(ToolKind::Definition).tool_description("t").created_at(500).namespace("ops");
        let dh = fx.facade.with_store(|m| m.add(&def)).unwrap();
        defs.insert(n.clone(), json!(dh.to_hex()));
        wf = wf.bind(n, dh.to_hex().as_str());
    }
    let plan = fx.store(wf.clone());
    let runner = fx.runner();
    // Cancel before the run starts: the driver sees the marker at its first
    // boundary and drains — a canceled journal with a cancel-bearing
    // checkpoint, the shape the fixture needs.
    runner.cancel("r1", "user:operator", "drill").unwrap();
    let outcome = run(&runner, &plan, "r1");
    assert!(matches!(outcome, RunOutcome::Canceled { .. }), "{outcome:?}");
    let mut cand = wf.clone();
    cand.retries.insert("s1".into(), 2);
    let report = runner.shadow_plan(&["r1".into()], &PlanCandidate::Body(fx.body(&cand))).unwrap();
    let r = &report.runs[0];
    assert_eq!((r.incumbent_outcome.as_str(), r.candidate_outcome.as_str(), r.verdict.as_str()), ("canceled", "canceled", "same"), "{r:?}");
    let _ = Mutex::new(());
}

// ---- #277: shadowing a candidate VERSION, not only a candidate plan --------
//
// Answering every effect from the journal by its key means the binding is
// never consulted — so a candidate that rebinds one node to a Definition
// carrying DIFFERENT BYTES replays the old bytes' result and rehearses as
// `same`. `reexecute: "pure"` closes that for the one class of tool where
// re-running is provably free of external effect: a `wasm32-areev` module,
// whose frozen import set is exactly `areev::emit`.
//
// The sandbox here is a shell script standing in for `areev-sandbox`, the
// same fixture `codeexec_tests.rs` uses: what is under test is the
// rehearsal's plumbing — which node runs, which is answered from the
// journal, and what the report says changed — not wasmtime.

/// A rig with blobs and a sandbox.
struct CodeFx {
    _dir: TempDir,
    dir: std::path::PathBuf,
    facade: Arc<AreevFacade>,
}

#[cfg(unix)]
impl CodeFx {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        CodeFx { _dir: dir, dir: path, facade: Arc::new(AreevFacade::new(m)) }
    }

    fn put_blob(&self, bytes: &[u8]) -> String {
        self.facade.with_store(|m| m.put_blob(bytes)).unwrap()
    }

    /// A `file` Definition naming `uri`, optionally under a declared runtime.
    /// `created_at` is bumped per version so two Definitions of one name are
    /// two grains.
    fn def(&self, uri: &str, runtime: Option<&str>, caps: Option<Value>, at: i64) -> String {
        let mut def = Tool::new("file")
            .kind(ToolKind::Definition)
            .tool_description("reads a total off a document")
            .executor_uri(uri)
            .created_at(at)
            .namespace("ops");
        if let Some(rt) = runtime {
            def = def.runtime(rt).runtime_limits(json!({"fuel": 5000, "max_pages": 64}));
        }
        if let Some(c) = caps {
            def = def.capabilities(c);
        }
        self.facade.with_store(|m| m.add(&def)).unwrap().to_hex()
    }

    fn plan(&self, def_hex: &str) -> Hash {
        let wf = Workflow::new(vec!["file".into()])
            .bind("file", def_hex)
            .created_at(600)
            .namespace("ops");
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn body(&self, def_hex: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("nodes".into(), json!(["file"]));
        m.insert("edges".into(), json!([]));
        m.insert("bindings".into(), json!({"file": def_hex}));
        m
    }

    /// A shell script standing in for `areev-sandbox`: argv is
    /// `--module PATH [--fuel N] [--max-pages N]`, so `$2` is the blob, whose
    /// bytes it reports as the node's `total`.
    fn sandbox(&self) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = self.dir.join("fake-sandbox.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"#!/bin/sh\nprintf '{\"total\":%s}' \"$(cat \"$2\")\"\n").unwrap();
        let mut perm = f.metadata().unwrap().permissions();
        perm.set_mode(0o700);
        f.set_permissions(perm).unwrap();
        path
    }

    fn runner(&self, exec: Arc<dyn HostToolExecutor>) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(
                (0..400).map(|i| 1_757_000_000_000 + i * 10).collect(),
            )),
            executor: exec,
            llm: None,
            observer: None,
            ns: "ops".into(),
            principal: "user:runner".into(),
        }
    }
}

/// The ticket's shape: one node, one plan, two module versions. Without the
/// option the rehearsal cannot see the difference; with it, the report names
/// the state key that moved — and names only the key, never the value.
#[cfg(unix)]
#[test]
fn a_pure_module_rehearses_the_candidate_bytes_and_reports_which_keys_moved() {
    let fx = CodeFx::new();
    let a = fx.put_blob(b"1");
    let b = fx.put_blob(b"2");
    let v1 = fx.def(&a, Some("wasm32-areev"), None, 500);
    let v2 = fx.def(&b, Some("wasm32-areev"), None, 501);
    let plan = fx.plan(&v1);
    let exec = areev_run::CodeExecutor::new(Exec::new(0, 1))
        .allow(&a)
        .allow(&b)
        .cache_dir(fx.dir.join("cache"))
        .sandbox_cmd(fx.sandbox().to_str().unwrap());
    let runner = fx.runner(Arc::new(exec));
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed);
    let ops = fx.facade.with_store(|m| m.stats()).unwrap().ops;

    // Today's answer: the binding is never consulted, so the version change
    // is invisible. This is the defect, pinned so a regression reads as one.
    let plain = runner.shadow_plan(&["r1".into()], &PlanCandidate::Body(fx.body(&v2))).unwrap();
    assert_eq!((plain.runs[0].verdict.as_str(), plain.runs[0].effects_replayed), ("same", 1));
    let json = serde_json::to_string(&plain).unwrap();
    for absent in ["reexecut", "sandbox_executions", "changed_keys", "added_keys", "removed_keys"] {
        assert!(!json.contains(absent), "the default report must not grow a field: {json}");
    }

    let report = runner
        .shadow_plan_with(
            &["r1".into()],
            &PlanCandidate::Body(fx.body(&v2)),
            &areev_run::ShadowOptions::reexecute(areev_run::Reexecute::Pure),
        )
        .unwrap();
    let r = &report.runs[0];
    assert_eq!(r.candidate_outcome, "completed", "{r:?}");
    assert_eq!(r.reexecuted.as_deref(), Some(&["file".to_string()][..]), "{r:?}");
    assert_eq!(r.not_reexecuted.as_ref().map(Vec::len), Some(0), "{r:?}");
    assert_eq!(r.sandbox_executions, Some(1));
    assert_eq!(r.effects_replayed, 0, "the node ran; no journal row was consumed: {r:?}");
    assert_eq!(r.changed_keys.as_deref(), Some(&["/total".to_string()][..]), "{r:?}");
    assert_eq!(r.added_keys.as_ref().map(Vec::len), Some(0));
    assert_eq!(r.removed_keys.as_ref().map(Vec::len), Some(0));
    assert_eq!(report.sandbox_executions, Some(1));
    assert_eq!(report.reexecute.as_deref(), Some("pure"));
    assert_eq!((report.effect_dispatches, report.writes), (0, 0));
    // Key paths only: the values 1 and 2 are nowhere in the artifact.
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("\"changed_keys\":[\"/total\"]"), "{json}");
    assert!(!json.contains("\"total\":1") && !json.contains("\"total\":2"), "{json}");
    assert_eq!(fx.facade.with_store(|m| m.stats()).unwrap().ops, ops, "a rehearsal writes nothing");

    // Re-running the SAME module version changes nothing, which is what makes
    // a reported change evidence rather than noise.
    let same = runner
        .shadow_plan_with(
            &["r1".into()],
            &PlanCandidate::Body(fx.body(&v1)),
            &areev_run::ShadowOptions::reexecute(areev_run::Reexecute::Pure),
        )
        .unwrap();
    assert_eq!(same.runs[0].changed_keys.as_ref().map(Vec::len), Some(0), "{:?}", same.runs[0]);
    assert_eq!(same.runs[0].sandbox_executions, Some(1));
}

/// A `native` blob is the ticket's second acceptance case: it is a program,
/// not a proof — it may read a clock, a file or a socket — so it is answered
/// from the journal and says so.
#[cfg(unix)]
#[test]
fn a_native_tool_is_answered_from_the_journal_and_listed_as_not_reexecuted() {
    let fx = CodeFx::new();
    let a = fx.put_blob(b"#!/bin/sh\necho '{\"total\":1}'\n");
    let b = fx.put_blob(b"#!/bin/sh\necho '{\"total\":9}'\n");
    let v1 = fx.def(&a, None, None, 500);
    let v2 = fx.def(&b, None, None, 501);
    let plan = fx.plan(&v1);
    let exec = areev_run::CodeExecutor::new(Exec::new(0, 1))
        .allow(&a)
        .allow(&b)
        .cache_dir(fx.dir.join("cache"))
        .sandbox_cmd(fx.sandbox().to_str().unwrap());
    let runner = fx.runner(Arc::new(exec));
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed);

    let report = runner
        .shadow_plan_with(
            &["r1".into()],
            &PlanCandidate::Body(fx.body(&v2)),
            &areev_run::ShadowOptions::reexecute(areev_run::Reexecute::Pure),
        )
        .unwrap();
    let r = &report.runs[0];
    assert_eq!(r.reexecuted.as_ref().map(Vec::len), Some(0), "{r:?}");
    assert_eq!(r.sandbox_executions, Some(0));
    assert_eq!(report.sandbox_executions, Some(0));
    assert_eq!(r.effects_replayed, 1, "the journal answered it: {r:?}");
    let refused = r.not_reexecuted.as_ref().expect("the mode reports its refusals");
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].node, "file");
    assert!(refused[0].why.contains("native"), "{:?}", refused[0]);
    // Answered from the journal ⇒ the terminal context is the incumbent's.
    assert_eq!(r.changed_keys.as_ref().map(Vec::len), Some(0), "{r:?}");
    assert_eq!(r.verdict, "same");
}

/// The two fail-closed arms, both host-side: an address this host never
/// pinned, and a capability runtime. The declaration replicates; the
/// authorization to execute never does — and a rehearsal is the same act of
/// running someone's code as a run, so it is not a weaker place to say so.
#[cfg(unix)]
#[test]
fn an_unpinned_or_capability_candidate_is_never_reexecuted() {
    let fx = CodeFx::new();
    let a = fx.put_blob(b"1");
    let b = fx.put_blob(b"2");
    let v1 = fx.def(&a, Some("wasm32-areev"), None, 500);
    let v2 = fx.def(&b, Some("wasm32-areev"), None, 501);
    let io = fx.def(
        &b,
        Some("wasm32-areev-io"),
        Some(json!([{"http": {"hosts": ["https://api.example.com"], "methods": ["GET"]}}])),
        502,
    );
    let plan = fx.plan(&v1);
    // Only v1's address is pinned: the candidate's bytes are code this host
    // never vouched for.
    let exec = areev_run::CodeExecutor::new(Exec::new(0, 1))
        .allow(&a)
        .cache_dir(fx.dir.join("cache"))
        .sandbox_cmd(fx.sandbox().to_str().unwrap());
    let runner = fx.runner(Arc::new(exec));
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed);
    let opts = areev_run::ShadowOptions::reexecute(areev_run::Reexecute::Pure);

    let report = runner
        .shadow_plan_with(&["r1".into()], &PlanCandidate::Body(fx.body(&v2)), &opts)
        .unwrap();
    let why = &report.runs[0].not_reexecuted.as_ref().unwrap()[0].why;
    assert!(why.contains("not pinned by this host"), "{why}");
    assert!(why.contains("--allow-executor"), "the fix must be a copy-paste: {why}");
    assert_eq!(report.sandbox_executions, Some(0));
    assert_eq!(report.runs[0].effects_replayed, 1, "still answered from the journal");

    let report = runner
        .shadow_plan_with(&["r1".into()], &PlanCandidate::Body(fx.body(&io)), &opts)
        .unwrap();
    let why = &report.runs[0].not_reexecuted.as_ref().unwrap()[0].why;
    assert!(why.contains("wasm32-areev-io"), "{why}");
    assert_eq!(report.sandbox_executions, Some(0));
}

/// A rehearsal with no executor behind it (the loop's substrate adapter, the
/// console) reports every node as not re-executed rather than pretending the
/// mode did something.
#[cfg(unix)]
#[test]
fn a_rehearsal_with_no_executor_reexecutes_nothing_and_says_why() {
    let fx = CodeFx::new();
    let a = fx.put_blob(b"1");
    let v1 = fx.def(&a, Some("wasm32-areev"), None, 500);
    let plan = fx.plan(&v1);
    let exec = areev_run::CodeExecutor::new(Exec::new(0, 1))
        .allow(&a)
        .cache_dir(fx.dir.join("cache"))
        .sandbox_cmd(fx.sandbox().to_str().unwrap());
    let runner = fx.runner(Arc::new(exec));
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed);

    let report = areev_run::shadow::shadow_plan_scoped_with(
        &fx.facade,
        "user:runner",
        &[("r1".to_string(), "ops".to_string())],
        &PlanCandidate::Body(fx.body(&v1)),
        &areev_run::ShadowOptions::reexecute(areev_run::Reexecute::Pure),
        None,
    )
    .unwrap();
    let why = &report.runs[0].not_reexecuted.as_ref().unwrap()[0].why;
    assert!(why.contains("no host executor"), "{why}");
    assert_eq!(report.sandbox_executions, Some(0));
}

/// Under the incumbent plan itself, pure re-execution is ALSO a verify: the
/// module actually runs and every checkpoint still byte-compares. That is the
/// `wasm32-areev` row of the Tier C table ("re-execution-provable") cashed in
/// rather than asserted — and it is what makes a reported `changed_keys`
/// evidence about the candidate rather than noise about the sandbox.
#[cfg(unix)]
#[test]
fn an_identity_rehearsal_under_pure_reexecution_still_byte_compares() {
    let fx = CodeFx::new();
    let a = fx.put_blob(b"1");
    let v1 = fx.def(&a, Some("wasm32-areev"), None, 500);
    let plan = fx.plan(&v1);
    let exec = areev_run::CodeExecutor::new(Exec::new(0, 1))
        .allow(&a)
        .cache_dir(fx.dir.join("cache"))
        .sandbox_cmd(fx.sandbox().to_str().unwrap());
    let runner = fx.runner(Arc::new(exec));
    assert_eq!(run(&runner, &plan, "r1"), RunOutcome::Completed);

    let report = runner
        .shadow_plan_with(
            &["r1".into()],
            &PlanCandidate::Hash(plan),
            &areev_run::ShadowOptions::reexecute(areev_run::Reexecute::Pure),
        )
        .unwrap();
    let r = &report.runs[0];
    let id = r.identity.as_ref().expect("the candidate IS the incumbent plan");
    assert!(id.consistent && id.checkpoints_compared > 0, "{r:?}");
    assert_eq!(id.checkpoints_compared, runner.verify("r1").unwrap().steps.len());
    assert_eq!(r.sandbox_executions, Some(1), "it really ran: {r:?}");
    assert_eq!(r.changed_keys.as_ref().map(Vec::len), Some(0), "{r:?}");
    assert_eq!(r.verdict, "same");
}
