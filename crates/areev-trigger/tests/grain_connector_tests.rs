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

    /// The same Definition, plus the `config` a generic blob is wired with
    /// (#231) — where the items are, which pointer is the id.
    fn connector_def_with_config(&self, uri: &str, config: Value) -> Hash {
        let def = Tool::new("mailbox.poll")
            .kind(ToolKind::Definition)
            .tool_description("read the mailbox")
            .created_at(T0 - 2000)
            .namespace(NS)
            .executor_uri(uri)
            .extra_field("config", config);
        self.facade.with_store(|m| m.add(&def)).unwrap()
    }

    /// A polling trigger with its own instance `config`.
    fn declare_with_config(&self, def: &Hash, config: Value) -> String {
        let plan = self.plan();
        let t = Trigger::new(TriggerKind::Polling, &plan.to_hex())
            .connector("mailbox")
            .interval_secs(60)
            .dedup_key("/id")
            .created_at(T0)
            .namespace(NS)
            .connector_tool(&def.to_hex())
            .config(config);
        self.facade.with_store(|m| m.add(&t)).unwrap().to_hex()
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

/// Echoes the whole request back as the item payload, so a test can assert
/// what the evaluator actually handed the connector.
#[cfg(unix)]
const ECHO_REQUEST: &str = r#"#!/bin/sh
read -r line
printf '{"items":[{"id":"seen","payload":{"id":"seen","request":%s}}],"cursor":"c-1"}\n' "$line"
"#;

/// Reports a failure in the house shape every blessed blob fails in.
#[cfg(unix)]
const FAILING: &str = r#"#!/bin/sh
read -r line
printf '{"error":"upstream answered 503","code":"RUN-E022"}\n'
"#;

#[cfg(unix)]
#[test]
fn a_definitions_config_wires_the_connector_and_the_trigger_specializes_it() {
    // #231: a generic blob (`rest.poll`) is made provider-specific by the
    // Definition's `config`, beside the `capabilities` block it must agree
    // with — so "a Gmail connector" is a declaration, not a crate. The
    // trigger's own config is the instance, and wins where they collide.
    let rig = Rig::new();
    let uri = rig.put_blob(ECHO_REQUEST.as_bytes());
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let def = rig.connector_def_with_config(
        &uri,
        json!({ "items": "/messages", "id": "/id", "query": { "q": "from:vendor" } }),
    );
    rig.declare_with_config(&def, json!({ "query": { "q": "newer_than:1d" }, "scope": "ap@desk" }));

    let ev = rig.evaluator(Some(rig.code(&[&addr])));
    ev.run(&opts()).unwrap(); // seeds
    rig.clock.advance(120_000);
    let report = ev.run(&opts()).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let events = rig
        .facade
        .with_store(|m| m.recent(NS, Some(areev_core::types::GrainType::Event), 20))
        .unwrap();
    let body: String = events.iter().filter_map(|g| g.get_str("content")).collect();
    let seen: Value = serde_json::from_str(&body).expect("the Event carries the item payload");
    let config = &seen["request"]["config"];
    assert_eq!(config["items"], "/messages", "the Definition's wiring reached the connector");
    assert_eq!(config["id"], "/id");
    assert_eq!(
        config["query"]["q"], "newer_than:1d",
        "and the trigger specialized it: the instance wins on a collision"
    );
    assert_eq!(config["scope"], "ap@desk", "the trigger may add keys of its own");
}

#[cfg(unix)]
#[test]
fn a_connector_that_reports_an_error_fails_the_poll_instead_of_looking_empty() {
    // `PollResponse` is `#[serde(default)]`, so `{"error": …}` used to
    // deserialize into an empty page with no cursor: a source that is DOWN,
    // reported as a source with nothing new, on every tick, silently. It is
    // the shape every blessed blob fails in, so this is not a corner case.
    let rig = Rig::new();
    let uri = rig.put_blob(FAILING.as_bytes());
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let def = rig.connector_def(Some(&uri), None);
    rig.declare(Some(&def));

    let report = rig.evaluator(Some(rig.code(&[&addr]))).run(&opts()).unwrap();
    assert_eq!(report.runs_started, 0);
    assert_eq!(report.errors.len(), 1, "a failed poll is an error, not a quiet tick: {report:?}");
    assert!(report.errors[0].contains("upstream answered 503"), "{:?}", report.errors);
    assert!(report.errors[0].contains("RUN-E022"), "the code survives: {:?}", report.errors);
}

/// Returns the SAME item every time, with a cursor derived from the one it
/// was handed — so a page can be entirely duplicates and still have moved.
#[cfg(unix)]
const SAME_ITEM: &str = r#"#!/bin/sh
read -r line
cur=$(printf '%s' "$line" | sed -n 's/.*"cursor":"\([^"]*\)".*/\1/p')
printf '{"items":[{"id":"m-1","payload":{"id":"m-1"}}],"cursor":"%sx"}\n' "$cur"
"#;

#[cfg(unix)]
#[test]
fn a_page_that_is_entirely_duplicates_still_advances_the_cursor() {
    // The rule a connector author reaches for last and needs most: advance on
    // everything LOOKED AT, not on everything that turned into work. A page
    // whose items were all deduped away is a page that was read — holding the
    // cursor there re-fetches it forever, on every tick, for as long as the
    // trigger lives. (A page that failed to START is the opposite case, and
    // #129 holds the cursor for exactly that one.)
    let rig = Rig::new();
    let uri = rig.put_blob(SAME_ITEM.as_bytes());
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let def = rig.connector_def(Some(&uri), None);
    let hash = rig.declare(Some(&def));
    let ev = rig.evaluator(Some(rig.code(&[&addr])));

    ev.run(&opts()).unwrap(); // seeds: cursor "x", nothing fires
    rig.clock.advance(120_000);
    let first = ev.run(&opts()).unwrap();
    assert_eq!(first.runs_started, 1, "the item is new here: {first:?}");

    rig.clock.advance(120_000);
    let again = ev.run(&opts()).unwrap();
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    assert_eq!(again.runs_started, 0, "the same item must not start a second run");
    assert_eq!(again.duplicates, 1, "it is a duplicate, not a failure: {again:?}");
    assert!(again.cursor_held.is_empty(), "and nothing held the cursor: {again:?}");

    let status = ev.status().unwrap();
    let st = status.iter().find(|s| s.trigger == hash).expect("the trigger has state");
    assert_eq!(
        st.cursor.as_deref(),
        Some("xxx"),
        "three polls, three advances — a page of duplicates moved it too"
    );
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
