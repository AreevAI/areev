//! A connector that is a GRAIN: pinned code on the trigger path (#185).
//!
//! The failure this closes is the one nobody watches. A polling connector was
//! a host command, so the code most likely to be wrong — cursors, pagination,
//! a provider's quirks — lived outside the memory: it did not travel in a
//! bundle, `tool provenance` could not chase it, and the loop could not
//! propose a `code_revision` against it.
//!
//! What is pinned here is both directions of the same rule. A trigger naming a
//! Definition runs THAT code, through the same `CodeExecutor` a run's nodes go
//! through — and it runs nothing at all unless the evaluating host pinned the
//! address, because a bundle carries the code and a permission arriving beside
//! the code it authorizes is not a permission.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Trigger, TriggerKind, Workflow};
use areev_run::{ExecResult, HostToolExecutor};
use areev_store::Areev;
use areev_trigger::{
    clock::FixedClock, ConnectorCode, EvalOptions, Evaluator, RunStarter, StartResult,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

const NS: &str = "ops";
const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

/// Stands in for `--connector-cmd`. Reaching it means the grain path was
/// bypassed and a different program ran than the declaration names — the
/// failure with no symptom this whole feature exists to refuse.
struct HostCommand;
impl HostToolExecutor for HostCommand {
    fn execute(&self, _n: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        // A cursor, because absent means "leave it where it is" — and a
        // trigger whose cursor never moves is always on its first contact.
        ExecResult::Ok(json!({
            "items": [ { "id": "from-the-host-command",
                         "payload": { "id": "from-the-host-command" } } ],
            "cursor": "c-1"
        }))
    }
}

/// Records what would have been started, without spending a run.
#[derive(Default)]
struct Started(std::sync::Mutex<Vec<String>>);

impl RunStarter for Started {
    fn start(&self, _workflow: &str, run_id: &str, _input: Value) -> StartResult {
        self.0.lock().unwrap().push(run_id.to_string());
        StartResult::Started
    }
}

struct Rig {
    _dir: TempDir,
    dir: std::path::PathBuf,
    facade: Arc<AreevFacade>,
    clock: Arc<FixedClock>,
    started: Arc<Started>,
}

impl Rig {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let m = Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
        Rig {
            _dir: dir,
            dir: path,
            facade: Arc::new(AreevFacade::new(m)),
            clock: Arc::new(FixedClock::new(T0)),
            started: Arc::new(Started::default()),
        }
    }

    fn put_blob(&self, bytes: &[u8]) -> String {
        self.facade.with_store(|m| m.put_blob(bytes)).unwrap()
    }

    /// A Definition for the connector's code. `uri`/`runtime` are what the
    /// evaluator reads back through `pin_from_definition`.
    fn connector_def(&self, uri: Option<&str>, runtime: Option<&str>) -> Hash {
        let mut def = Tool::new("mailbox.poll")
            .kind(ToolKind::Definition)
            .tool_description("read the mailbox")
            .created_at(T0 - 2000)
            .namespace(NS);
        if let Some(u) = uri {
            def = def.executor_uri(u);
        }
        if let Some(rt) = runtime {
            def = def.runtime(rt);
        }
        self.facade.with_store(|m| m.add(&def)).unwrap()
    }

    fn plan(&self) -> Hash {
        let wf = Workflow::new(vec!["triage".into()]).created_at(T0 - 1000).namespace(NS);
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    /// A polling trigger whose connector is the Definition at `def`.
    fn declare(&self, def: Option<&Hash>) -> String {
        let plan = self.plan();
        let mut t = Trigger::new(TriggerKind::Polling, &plan.to_hex())
            .connector("mailbox")
            .interval_secs(60)
            .dedup_key("/id")
            .created_at(T0)
            .namespace(NS);
        if let Some(d) = def {
            t = t.connector_tool(&d.to_hex());
        }
        self.facade.with_store(|m| m.add(&t)).unwrap().to_hex()
    }

    fn evaluator(&self, code: Option<ConnectorCode>) -> Evaluator {
        Evaluator {
            facade: Arc::clone(&self.facade),
            clock: Arc::clone(&self.clock) as Arc<dyn areev_trigger::Clock>,
            connector: Some(Arc::new(HostCommand)),
            connector_code: code,
            starter: Some(Arc::clone(&self.started) as Arc<dyn RunStarter>),
            credentials: Default::default(),
            ns: NS.into(),
            principal: "user:heartbeat".into(),
        }
    }

    fn code(&self, allow: &[&str]) -> ConnectorCode {
        ConnectorCode {
            allow: allow.iter().map(|a| a.to_string()).collect(),
            cache_dir: Some(self.dir.join("execache")),
            ..Default::default()
        }
    }
}

fn opts() -> EvalOptions {
    EvalOptions { node: "node-A".into(), ..Default::default() }
}

/// A connector script in the CAS. Native, not wasm: the workspace cannot run
/// the sandbox (a separate package), and what is under test here is the
/// resolve → pin → dispatch path, which is identical for both runtimes.
#[cfg(unix)]
const CONNECTOR: &[u8] = b"#!/bin/sh\nread -r line\necho '{\"items\":[{\"id\":\"m-1\",\"payload\":{\"id\":\"m-1\"}}],\"cursor\":\"c-1\"}'\n";

#[cfg(unix)]
#[test]
fn a_trigger_polls_the_code_its_declaration_names() {
    let rig = Rig::new();
    let uri = rig.put_blob(CONNECTOR);
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let def = rig.connector_def(Some(&uri), None);
    let hash = rig.declare(Some(&def));

    let ev = rig.evaluator(Some(rig.code(&[&addr])));
    // First contact seeds the cursor and fires nothing — declaring a mailbox
    // trigger must not replay its history.
    let report = ev.run(&opts()).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.runs_started, 0, "the first poll seeds");

    rig.clock.advance(120_000);
    let report = ev.run(&opts()).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.items, 1, "the blob's item came back: {report:?}");
    assert_eq!(report.runs_started, 1, "and started the plan it names");

    // The Event carries the blob's item, not the host command's — a bypassed
    // grain path would have ingested `from-the-host-command`.
    let events = rig
        .facade
        .with_store(|m| m.recent(NS, Some(areev_core::types::GrainType::Event), 20))
        .unwrap();
    let bodies: String = events.iter().filter_map(|g| g.get_str("content")).collect();
    assert!(!bodies.contains("from-the-host-command"), "the host command ran instead: {bodies}");
    let _ = hash;
}

#[test]
fn a_connector_grain_is_refused_on_a_host_that_pinned_nothing() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"#!/bin/sh\necho '{}'\n");
    let def = rig.connector_def(Some(&uri), None);
    rig.declare(Some(&def));

    // No `connector_code` at all: this host runs host-command connectors only,
    // and must NOT fall through to `--connector-cmd` — that would run a
    // different program than the declaration names.
    let report = rig.evaluator(None).run(&opts()).unwrap();
    assert_eq!(report.runs_started, 0);
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("TRG-E012"), "{:?}", report.errors);
    assert!(report.errors[0].contains("--allow-executor"), "{:?}", report.errors);
}

#[test]
fn an_unpinned_address_is_refused_and_named() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"#!/bin/sh\necho '{}'\n");
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let def = rig.connector_def(Some(&uri), None);
    rig.declare(Some(&def));

    // A host that pinned SOMETHING, but not this.
    let other = "a".repeat(64);
    let report = rig.evaluator(Some(rig.code(&[&other]))).run(&opts()).unwrap();
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("TRG-E012"), "{:?}", report.errors);
    assert!(
        report.errors[0].contains(&addr),
        "the refusal must name the address to pin: {:?}",
        report.errors
    );
}

#[test]
fn a_definition_that_carries_no_code_is_refused() {
    let rig = Rig::new();
    let def = rig.connector_def(None, None);
    rig.declare(Some(&def));
    let report = rig.evaluator(Some(rig.code(&["b".repeat(64).as_str()]))).run(&opts()).unwrap();
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("TRG-E012"), "{:?}", report.errors);
    assert!(report.errors[0].contains("executor_uri"), "{:?}", report.errors);
}

#[test]
fn a_sandbox_runtime_with_no_sandbox_configured_is_refused_before_the_poll() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"\0asm not really a module");
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let def = rig.connector_def(Some(&uri), Some("wasm32-areev"));
    rig.declare(Some(&def));

    let report = rig.evaluator(Some(rig.code(&[&addr]))).run(&opts()).unwrap();
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("TRG-E012"), "{:?}", report.errors);
    assert!(report.errors[0].contains("--sandbox-cmd"), "{:?}", report.errors);
}

#[test]
fn a_trigger_that_names_no_connector_grain_still_uses_the_host_command() {
    // The other half of the compatibility promise: this feature widens what a
    // connector CAN be, and changes nothing about what it was.
    let rig = Rig::new();
    rig.declare(None);
    let ev = rig.evaluator(Some(rig.code(&["c".repeat(64).as_str()])));
    let report = ev.run(&opts()).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    rig.clock.advance(120_000);
    let report = ev.run(&opts()).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.items, 1, "the host command's item: {report:?}");
    assert_eq!(report.runs_started, 1);
}
