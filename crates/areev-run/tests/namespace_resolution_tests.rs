//! #230: a run started where it cannot see the plan's Definitions.
//!
//! A plan is addressed by hash, so it runs from any namespace — but a node
//! that names its tool instead of binding it is resolved through the RUN's
//! namespace, and a definition catalogue that comes back empty used to mean
//! one of two things, neither of them "you are in the wrong namespace":
//! abstract (with a model configured) or `RUN-E006`. The first is the bad
//! one — the Definition's `executor_uri`, runtime and capabilities are
//! dropped, a model answers instead, and the run reports Completed having
//! called nothing.
//!
//! The other half of the issue is diagnosis: `run inspect` printed a bound,
//! correctly-resolved capability node as a bare `"executor": "host"`, which
//! is exactly what a node with no Definition at all looks like. These tests
//! pin both.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    CodeExecutor, ExecResult, HostToolExecutor, RunOptions, Runner,
    RunSession, ScriptedClock,
};
use areev_run_core::RunOutcome;
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// Where the pack put everything.
const PLAN_NS: &str = "ap";
/// Where the operator forgot to say `--ns ap`.
const OTHER_NS: &str = "shared";

struct Echo;
impl HostToolExecutor for Echo {
    fn execute(&self, tool_name: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        ExecResult::Ok(json!({ "ran": tool_name }))
    }
}

struct Rig {
    _dir: TempDir,
    dir: std::path::PathBuf,
    facade: Arc<AreevFacade>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig { _dir: dir, dir: path, facade: Arc::new(AreevFacade::new(m)) }
    }

    fn definition(&self, name: &str, uri: Option<&str>) -> Hash {
        let mut def = Tool::new(name)
            .kind(ToolKind::Definition)
            .tool_description("read one invoice from the vendor API")
            .created_at(500)
            .namespace(PLAN_NS);
        if let Some(u) = uri {
            def = def.executor_uri(u);
        }
        self.facade.with_store(|m| m.add(&def)).unwrap()
    }

    /// The plan reaches its tool by NAME — resolution goes through the run's
    /// namespace.
    fn named_plan(&self, node: &str) -> Hash {
        let wf = Workflow::new(vec![node.into()]).created_at(600).namespace(PLAN_NS);
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    /// The same plan, BOUND by hash — resolution is namespace-independent.
    fn bound_plan(&self, node: &str, def: &Hash) -> Hash {
        let wf =
            Workflow::new(vec![node.into()]).bind(node, &def.to_hex()).created_at(600).namespace(PLAN_NS);
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn put_blob(&self, bytes: &[u8]) -> String {
        self.facade.with_store(|m| m.put_blob(bytes)).unwrap()
    }

    fn runner(&self, ns: &str, exec: Arc<dyn HostToolExecutor>) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(
                (0..200).map(|i| 1_755_000_000_000 + i * 10).collect(),
            )),
            executor: exec,
            llm: None,
            observer: None,
            ns: ns.into(),
            principal: "user:runner".into(),
        }
    }

    /// Did a run get as far as existing? A refusal at resolve must leave
    /// nothing behind in either namespace.
    fn checkpoints(&self, ns: &str, run_id: &str) -> usize {
        self.facade
            .with_store(|m| areev_run::journal::load(m, ns, run_id).map(|v| v.checkpoints.len()))
            .unwrap_or(0)
    }
}

fn opts() -> RunOptions {
    RunOptions { workers: 2, ..Default::default() }
}

#[test]
fn a_named_node_refuses_when_the_run_cannot_see_the_plans_definitions() {
    let rig = Rig::new();
    rig.definition("vendor_api", Some("cas://sha256:11".to_string().repeat(32).as_str()));
    let plan = rig.named_plan("vendor_api");

    let err = rig
        .runner(OTHER_NS, Arc::new(Echo))
        .start(&plan, "misrouted", json!({}), &opts())
        .unwrap_err();

    assert_eq!(err.code(), "RUN-E004");
    let msg = err.to_string();
    // Everything an operator needs to act, named: the node, the namespace
    // that was searched, and the one that would have worked.
    assert!(msg.contains("vendor_api"), "{msg}");
    assert!(msg.contains(OTHER_NS), "the namespace searched must be named: {msg}");
    assert!(msg.contains(&format!("--ns {PLAN_NS}")), "the fix must be copy-pasteable: {msg}");

    // Refused at resolve — before the lease, before the manifest. Nothing to
    // explain afterwards, in either namespace.
    assert_eq!(rig.checkpoints(OTHER_NS, "misrouted"), 0);
    assert_eq!(rig.checkpoints(PLAN_NS, "misrouted"), 0);
}

#[test]
fn the_same_plan_run_in_its_own_namespace_resolves() {
    // The control: the refusal above must be about the namespace and nothing
    // else, or it is just a broken runtime.
    let rig = Rig::new();
    rig.definition("vendor_api", None);
    let plan = rig.named_plan("vendor_api");

    let session =
        rig.runner(PLAN_NS, Arc::new(Echo)).start(&plan, "right-place", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected a finish") };
    assert_eq!(outcome, RunOutcome::Completed);
}

#[test]
fn a_node_with_no_definition_anywhere_is_still_abstract_not_misrouted() {
    // The new check must not swallow the old verdict: a node that names no
    // Definition in ANY namespace — including the plan's — is an abstract
    // node, and without a tool-calling model that is still RUN-E006.
    let rig = Rig::new();
    let plan = rig.named_plan("summarize_the_invoice");

    let err = rig
        .runner(OTHER_NS, Arc::new(Echo))
        .start(&plan, "abstract", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E006", "{err}");
}

#[cfg(unix)]
#[test]
fn a_bound_node_resolves_from_any_namespace_and_inspect_says_what_it_pinned() {
    // The half of #230 that was a REPORTING bug. This run is correct — a
    // binding is a content address, so the run's namespace never enters into
    // it — and `run inspect` used to describe it exactly as it describes a
    // node with no Definition at all: `"executor": "host"`, nothing else.
    // Reading that output is what produced the wrong conclusion.
    let rig = Rig::new();
    let script = b"#!/bin/sh\nread -r line\necho '{\"ran\":true}'\n";
    let uri = rig.put_blob(script);
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let def = rig.definition("vendor_api", Some(&uri));
    let plan = rig.bound_plan("vendor_api", &def);

    let exec =
        CodeExecutor::new(Arc::new(Echo)).allow(&addr).cache_dir(rig.dir.join("execache"));
    let runner = rig.runner(OTHER_NS, Arc::new(exec));

    let session = runner.start(&plan, "bound", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected a finish") };
    assert_eq!(outcome, RunOutcome::Completed, "a bound node resolves from any namespace");

    let report = runner.inspect("bound").unwrap();
    let row = &report.pinned[0];
    assert_eq!(row["node"], json!("vendor_api"));
    assert_eq!(
        row["executor_uri"], json!(uri),
        "inspect must report the code this run froze, not just 'host': {row}"
    );
}
