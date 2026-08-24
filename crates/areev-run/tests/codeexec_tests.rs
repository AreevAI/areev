//! Code-carrying tools: `executor_uri` on a Definition, and the host-side
//! pin that decides whether any of it runs.
//!
//! The invariant under test is the one that makes this safe to ship: a blob
//! travels with the memory, so importing a peer's bundle imports their
//! connector code — and **the authorization to execute it never travels with
//! it**. An unpinned address is refused at start, by name.

use areev_cal::AreevFacade;
use areev_core::error::Hash;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    BudgetsSpec, CodeExecutor, ExecResult, HostToolExecutor, OnDangling, RunOptions, Runner,
    RunSession, ScriptedClock,
};
use areev_run_core::RunOutcome;
use areev_store::Areev;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// Stands in for `--tool-cmd`. Anything reaching it means the code path was
/// bypassed, which is the failure this whole feature exists to prevent.
struct Fallback;
impl HostToolExecutor for Fallback {
    fn execute(&self, tool_name: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        ExecResult::Ok(json!({ "fell_back_to_tool_cmd": tool_name }))
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

    /// A one-node plan whose Definition names `executor_uri`.
    fn plan_with_uri(&self, uri: Option<&str>, client: bool) -> Hash {
        let mut def = Tool::new("work")
            .kind(ToolKind::Definition)
            .tool_description("a code-carrying tool")
            .created_at(500)
            .namespace("ops");
        if let Some(u) = uri {
            def = def.executor_uri(u);
        }
        if client {
            def = def.executor_kind(areev_core::types::ExecutorKind::Client);
        }
        let dh = self.facade.with_store(|m| m.add(&def)).unwrap();
        let wf = Workflow::new(vec!["work".into()])
            .bind("work", &dh.to_hex())
            .created_at(600)
            .namespace("ops");
        self.facade.with_store(|m| m.add(&wf)).unwrap()
    }

    fn put_blob(&self, bytes: &[u8]) -> String {
        self.facade.with_store(|m| m.put_blob(bytes)).unwrap()
    }

    fn runner(&self, exec: Arc<dyn HostToolExecutor>) -> Runner {
        Runner {
            facade: Arc::clone(&self.facade),
            clock: Arc::new(ScriptedClock::new(
                (0..200).map(|i| 1_755_000_000_000 + i * 10).collect(),
            )),
            executor: exec,
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

#[test]
fn an_executor_uri_that_is_not_a_content_address_is_refused_by_name() {
    // The original defect: this value was written, parsed, and read by
    // nothing, so the node quietly executed whatever --tool-cmd was. Now it
    // either dispatches or says why it cannot.
    let rig = Rig::new();
    let plan = rig.plan_with_uri(Some("executor://crm.lookup@v3"), false);
    let err = rig
        .runner(Arc::new(Fallback))
        .start(&plan, "r1", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    let msg = err.to_string();
    assert!(msg.contains("executor://crm.lookup@v3"), "{msg}");
    assert!(msg.contains("cas://sha256:"), "the message must name the form that works: {msg}");
}

#[test]
fn an_unpinned_code_executor_is_refused_before_the_run_exists() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"#!/bin/sh\necho '{}'\n");
    let plan = rig.plan_with_uri(Some(&uri), false);

    // Fallback does not implement code_allowed, so it inherits the default:
    // refuse. A host that never opted in cannot be handed code by a plan.
    let err = rig
        .runner(Arc::new(Fallback))
        .start(&plan, "r1", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("--allow-executor"), "{err}");

    // Refused BEFORE the run exists — no manifest, no lease, nothing to
    // explain afterwards.
    let journal = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "r1").map(|v| v.checkpoints.len()));
    assert!(journal.map(|n| n == 0).unwrap_or(true), "a refused start must leave no run");
}

#[test]
fn a_client_tool_carrying_an_executor_uri_is_refused() {
    // A client tool is answered by a person through `run respond`. Naming an
    // executor on one describes something that cannot happen.
    let rig = Rig::new();
    let uri = rig.put_blob(b"#!/bin/sh\necho '{}'\n");
    let plan = rig.plan_with_uri(Some(&uri), true);
    let err = rig
        .runner(Arc::new(Fallback))
        .start(&plan, "r1", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("client tool"), "{err}");
}

#[cfg(unix)]
#[test]
fn a_pinned_code_executor_runs_and_its_output_is_the_node_result() {
    let rig = Rig::new();
    // Reads the tool input on stdin and answers with JSON, exactly like a
    // --tool-cmd tool. The contract is deliberately identical.
    let script = b"#!/bin/sh\nread -r line\necho \"{\\\"ran\\\":true,\\\"saw\\\":$line}\"\n";
    let uri = rig.put_blob(script);
    let addr = uri.strip_prefix("cas://sha256:").unwrap().to_string();
    let plan = rig.plan_with_uri(Some(&uri), false);

    let exec = CodeExecutor::new(Arc::new(Fallback))
        .allow(&addr)
        .cache_dir(rig.dir.join("execache"));
    let runner = rig.runner(Arc::new(exec));

    let session = runner.start(&plan, "r1", json!({ "q": 7 }), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    let entries = rig
        .facade
        .with_store(|m| areev_run::journal::load(m, "ops", "r1").unwrap().entries.len());
    assert_eq!(entries, 1);

    // The blob ran — not the fallback. If the code path were bypassed the
    // result would carry `fell_back_to_tool_cmd`.
    // Read the execution record itself: `step_actions` gives (node, hash),
    // and the result grain carries what the effect produced.
    let records = rig
        .facade
        .with_store(|m| m.step_actions("ops", &plan, None, 10))
        .unwrap();
    assert_eq!(records.len(), 1);
    let grain = rig.facade.with_store(|m| m.get(&records[0].1)).unwrap();
    let content = grain.get_str("tool_content").expect("a result grain carries its content");
    let produced: Value = serde_json::from_str(content).expect("the blob emitted JSON");
    assert_eq!(produced["ran"], json!(true), "the blob's output IS the node result");
    assert_eq!(produced["saw"]["q"], json!(7), "the tool input reached the blob's stdin");
    assert!(
        produced.get("fell_back_to_tool_cmd").is_none(),
        "a pinned code executor must not fall through to --tool-cmd"
    );
}

#[test]
fn a_tool_naming_no_executor_still_goes_to_tool_cmd() {
    // The feature is additive: everything that worked before works unchanged.
    let rig = Rig::new();
    let plan = rig.plan_with_uri(None, false);
    let session = rig
        .runner(Arc::new(CodeExecutor::new(Arc::new(Fallback))))
        .start(&plan, "r1", json!({}), &opts())
        .unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);
}

/// #87: a code-carrying tool gets the SAME brokered-credential story as a
/// `--tool-cmd` — `AREEV_EGRESS_URL`/`AREEV_EGRESS_TOKEN` when it holds a
/// grant, nothing when it does not, and never the secret itself.
#[cfg(unix)]
#[test]
fn a_pinned_blob_reaches_the_credential_broker_on_the_same_terms() {
    use areev_run::{
        Broker, CallerGrant, CodeExecutor, EgressGrants, EgressHandle, EgressPolicy,
    };

    std::env::set_var("AREEV_TEST_ZOHO2", "super-secret-value");
    let broker = Broker::start(
        EgressPolicy::unrestricted(),
        [("zoho".to_string(), areev_run::Credential::bearer_from_env("AREEV_TEST_ZOHO2").unwrap().into())]
            .into_iter()
            .collect(),
        EgressGrants::new().grant("poster", CallerGrant::new().credential("zoho").method("POST")),
        "RUN-E022",
    )
    .unwrap();
    std::env::remove_var("AREEV_TEST_ZOHO2");
    let broker = Arc::new(broker);

    // A blob that echoes its egress environment back as the result.
    let script = b"#!/bin/sh\nprintf '{\"url\":\"%s\",\"token\":\"%s\",\"leak\":\"%s\"}' \
                   \"$AREEV_EGRESS_URL\" \"$AREEV_EGRESS_TOKEN\" \"$AREEV_TEST_ZOHO2\"\n";
    let hex = format!("{:x}", {
        use sha2::{Digest, Sha256};
        Sha256::digest(script)
    });
    let uri = format!("cas://sha256:{hex}");
    let cache = TempDir::new().unwrap();

    let exec = CodeExecutor::new(Arc::new(Fallback))
        .allow(&hex)
        .cache_dir(cache.path())
        .with_egress(EgressHandle::new(Arc::clone(&broker)));

    let code = areev_run::PreparedCode {
        uri: uri.clone(),
        bytes: script.to_vec(),
        runtime: None,
        limits: None,
        capabilities: None,
    };
    // A granted tool sees the broker's address and its own capability token.
    let seen = match exec.execute_code("poster", "h", &code, &json!({}), "k") {
        ExecResult::Ok(v) => v,
        ExecResult::Err { detail, .. } => panic!("blob failed: {detail}"),
    };
    assert_eq!(seen["url"], json!(broker.url()), "the blob gets the broker's address");
    assert_eq!(
        seen["token"],
        json!(broker.token_for("poster").unwrap()),
        "and its own capability token"
    );
    assert_eq!(seen["leak"], json!(""), "the credential value must never reach the blob");

    // A tool with NO grant cannot even see the broker.
    let seen = match exec.execute_code("stranger", "h", &code, &json!({}), "k") {
        ExecResult::Ok(v) => v,
        ExecResult::Err { detail, .. } => panic!("blob failed: {detail}"),
    };
    assert_eq!(seen["url"], json!(""), "no grant, no broker address");
    assert_eq!(seen["token"], json!(""), "no grant, no token");
}

// ---- declared runtime → sandbox dispatch (#86) -----------------------------

/// A one-node plan whose Definition names an executor AND a runtime.
fn plan_with_runtime(rig: &Rig, uri: &str, runtime: &str, limits: Option<Value>) -> Hash {
    let mut def = Tool::new("work")
        .kind(ToolKind::Definition)
        .tool_description("a sandboxed tool")
        .executor_uri(uri)
        .runtime(runtime)
        .created_at(500)
        .namespace("ops");
    if let Some(l) = limits {
        def = def.runtime_limits(l);
    }
    let dh = rig.facade.with_store(|m| m.add(&def)).unwrap();
    let wf = Workflow::new(vec!["work".into()])
        .bind("work", &dh.to_hex())
        .created_at(600)
        .namespace("ops");
    rig.facade.with_store(|m| m.add(&wf)).unwrap()
}

/// A declared runtime on a host with no sandbox refuses BEFORE the run
/// exists — same fail-closed shape as the unpinned executor.
#[test]
fn a_wasm_runtime_without_a_sandbox_refuses_at_start() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"\0asm-not-really");
    let plan = plan_with_runtime(&rig, &uri, "wasm32-areev", None);

    let exec = areev_run::CodeExecutor::new(Arc::new(Fallback)).allow(&uri);
    let err = rig.runner(Arc::new(exec)).start(&plan, "r1", json!({}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("--sandbox-cmd"), "{err}");
}

/// The full dispatch: the blob is materialized and handed to the sandbox
/// command as `--module`, with the declared limits as flags — the sandbox is
/// the program, the blob is data. The fake sandbox echoes its argv.
#[cfg(unix)]
#[test]
fn a_wasm_runtime_dispatches_the_blob_to_the_sandbox_command() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let rig = Rig::new();
    let uri = rig.put_blob(b"\0asm-module-bytes");
    let plan = plan_with_runtime(
        &rig,
        &uri,
        "wasm32-areev",
        Some(json!({"fuel": 5000, "max_pages": 64})),
    );

    // The fake sandbox: prints its argv as the tool result.
    let fake = rig.dir.join("fake-sandbox.sh");
    {
        let mut f = std::fs::File::create(&fake).unwrap();
        f.write_all(b"#!/bin/sh\nprintf '{\"argv\":\"%s\"}' \"$*\"\n").unwrap();
        let mut perm = f.metadata().unwrap().permissions();
        perm.set_mode(0o700);
        f.set_permissions(perm).unwrap();
    }

    let exec = areev_run::CodeExecutor::new(Arc::new(Fallback))
        .allow(&uri)
        .cache_dir(rig.dir.join("cache"))
        .sandbox_cmd(fake.to_str().unwrap());
    let session = rig.runner(Arc::new(exec)).start(&plan, "r1", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    // The journaled result carries what the sandbox saw: the module path in
    // the cache (named by the content address) and the declared limits.
    let records = rig.facade.with_store(|m| m.step_actions("ops", &plan, None, 10)).unwrap();
    assert_eq!(records.len(), 1);
    let grain = rig.facade.with_store(|m| m.get(&records[0].1)).unwrap();
    let argv = grain.get_str("tool_content").expect("a result grain carries its content").to_string();
    assert!(argv.contains("--module"), "{argv}");
    assert!(argv.contains(uri.trim_start_matches("cas://sha256:")), "{argv}");
    assert!(argv.contains("--fuel 5000"), "{argv}");
    assert!(argv.contains("--max-pages 64"), "{argv}");
}

/// An unknown runtime refuses at resolve — possible on a grain that arrived
/// by sync (our own write path refuses the value), and it must never fall
/// back to native exec, which would run foreign bytes as a program.
#[test]
fn an_unknown_runtime_is_refused_by_name() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"whatever");
    let plan = plan_with_runtime(&rig, &uri, "python3", None);
    let exec = areev_run::CodeExecutor::new(Arc::new(Fallback)).allow(&uri);
    let err = rig.runner(Arc::new(exec)).start(&plan, "r1", json!({}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("python3"), "{err}");
}

/// A runtime with no code blob describes nothing — refused at resolve.
#[test]
fn a_runtime_without_an_executor_uri_is_refused() {
    let rig = Rig::new();
    let def = Tool::new("work")
        .kind(ToolKind::Definition)
        .runtime("wasm32-areev")
        .created_at(500)
        .namespace("ops");
    let dh = rig.facade.with_store(|m| m.add(&def)).unwrap();
    let wf = Workflow::new(vec!["work".into()])
        .bind("work", &dh.to_hex())
        .created_at(600)
        .namespace("ops");
    let plan = rig.facade.with_store(|m| m.add(&wf)).unwrap();
    let err = rig
        .runner(Arc::new(areev_run::CodeExecutor::new(Arc::new(Fallback))))
        .start(&plan, "r1", json!({}), &opts())
        .unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("names no executor_uri"), "{err}");
}

// ---- #101: the capability runtime -----------------------------------------

/// Build a plan whose one node is a capability tool.
fn plan_with_capabilities(
    rig: &Rig,
    uri: &str,
    runtime: &str,
    capabilities: Option<Value>,
    limits: Option<Value>,
) -> Hash {
    let mut def = Tool::new("work")
        .kind(ToolKind::Definition)
        .tool_description("a capability tool")
        .executor_uri(uri)
        .runtime(runtime)
        .created_at(500)
        .namespace("ops");
    if let Some(c) = capabilities {
        def = def.capabilities(c);
    }
    if let Some(l) = limits {
        def = def.runtime_limits(l);
    }
    let dh = rig.facade.with_store(|m| m.add(&def)).unwrap();
    let wf = Workflow::new(vec!["work".into()])
        .bind("work", &dh.to_hex())
        .created_at(600)
        .namespace("ops");
    rig.facade.with_store(|m| m.add(&wf)).unwrap()
}

fn gmail_caps() -> Value {
    json!([{"http": {
        "hosts": ["https://gmail.googleapis.com"],
        "methods": ["POST"],
        "credentials": ["gmail"]
    }}])
}

/// A fake sandbox that echoes its argv as the tool result.
#[cfg(unix)]
fn fake_sandbox(rig: &Rig, name: &str) -> std::path::PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = rig.dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"#!/bin/sh\nprintf '{\"argv\":\"%s\"}' \"$*\"\n").unwrap();
    let mut perm = f.metadata().unwrap().permissions();
    perm.set_mode(0o700);
    f.set_permissions(perm).unwrap();
    path
}

/// A broker granting `work` what a capability tool needs.
fn capability_broker() -> Arc<areev_run::Broker> {
    Arc::new(
        areev_run::Broker::start(
            areev_run::EgressPolicy::unrestricted(),
            Default::default(),
            areev_run::EgressGrants::new()
                .grant("work", areev_run::CallerGrant::new().method("POST")),
            "RUN-E022",
        )
        .unwrap(),
    )
}

/// The declaration is FROZEN into the persisted manifest beside the runtime,
/// so a mid-run supersession of the Definition cannot widen what a module
/// reaches — and a resume or a verify reads the same set the run started with.
#[cfg(unix)]
#[test]
fn a_capability_declaration_is_pinned_into_the_manifest() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"\0asm-io-module");
    let plan = plan_with_capabilities(&rig, &uri, "wasm32-areev-io", Some(gmail_caps()), None);
    let fake = fake_sandbox(&rig, "fake-sandbox-pin.sh");

    let exec = areev_run::CodeExecutor::new(Arc::new(Fallback))
        .allow(&uri)
        .cache_dir(rig.dir.join("cache"))
        .sandbox_cmd(fake.to_str().unwrap())
        .with_egress(areev_run::EgressHandle::new(capability_broker()));
    rig.runner(Arc::new(exec)).start(&plan, "r-pin", json!({}), &opts()).unwrap();

    let manifest = rig
        .facade
        .with_store(|m| areev_run::RunManifest::load(m, "r-pin"))
        .expect("the manifest persisted");
    let pin = manifest.pinned.iter().find(|p| p.node == "work").expect("the node is pinned");
    assert_eq!(pin.runtime.as_deref(), Some("wasm32-areev-io"));
    assert_eq!(pin.capabilities.as_ref(), Some(&gmail_caps()), "frozen verbatim");
}

/// A capability RUNTIME with nothing declared can reach nothing. Saying so at
/// start beats a module that instantiates and has every call refused.
#[test]
fn a_capability_runtime_with_no_declaration_refuses_at_start() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"\0asm");
    let plan = plan_with_capabilities(&rig, &uri, "wasm32-areev-io", None, None);
    let exec = areev_run::CodeExecutor::new(Arc::new(Fallback)).allow(&uri);
    let err = rig.runner(Arc::new(exec)).start(&plan, "r1", json!({}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("no capabilities"), "{err}");
}

/// And the inverse: a declaration on a runtime that cannot honour it describes
/// nothing. Our own write path refuses this pairing, but a grain can arrive by
/// sync from another implementation, so the run path refuses it too.
#[test]
fn capabilities_on_a_pure_runtime_refuse_at_start() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"\0asm");
    let plan = plan_with_capabilities(&rig, &uri, "wasm32-areev", Some(gmail_caps()), None);
    let exec = areev_run::CodeExecutor::new(Arc::new(Fallback)).allow(&uri);
    let err = rig.runner(Arc::new(exec)).start(&plan, "r1", json!({}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("wasm32-areev-io"), "{err}");
}

/// A malformed declaration is refused at start rather than at the first call —
/// same reasoning as the unknown runtime, and reachable the same way (sync).
#[test]
fn a_malformed_capability_declaration_refuses_at_start() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"\0asm");
    let plan = plan_with_capabilities(
        &rig,
        &uri,
        "wasm32-areev-io",
        // A bare `*` allows the whole internet under the appearance of a policy.
        Some(json!([{"http": {"hosts": ["https://*"]}}])),
        None,
    );
    let exec = areev_run::CodeExecutor::new(Arc::new(Fallback)).allow(&uri);
    let err = rig.runner(Arc::new(exec)).start(&plan, "r1", json!({}), &opts()).unwrap_err();
    assert_eq!(err.code(), "RUN-E018");
    assert!(err.to_string().contains("malformed capabilities"), "{err}");
}

/// The `-io` runtime dispatches to the same sandbox binary, with the gate flag.
/// `--allow-fetch` is what LINKS `areev::fetch`, so a pure module on the same
/// host still cannot import it.
#[cfg(unix)]
#[test]
fn the_capability_runtime_passes_allow_fetch_to_the_sandbox() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"\0asm-io-module");
    let plan = plan_with_capabilities(
        &rig,
        &uri,
        "wasm32-areev-io",
        Some(gmail_caps()),
        Some(json!({"fuel": 5000, "max_response_bytes": 4096})),
    );

    let fake = fake_sandbox(&rig, "fake-sandbox-io.sh");
    // A capability module needs a broker: the credential lives there and the
    // module holds only a token.
    let exec = areev_run::CodeExecutor::new(Arc::new(Fallback))
        .allow(&uri)
        .cache_dir(rig.dir.join("cache"))
        .sandbox_cmd(fake.to_str().unwrap())
        .with_egress(areev_run::EgressHandle::new(capability_broker()));
    let session = rig.runner(Arc::new(exec)).start(&plan, "r1", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
    assert_eq!(outcome, RunOutcome::Completed);

    let records = rig.facade.with_store(|m| m.step_actions("ops", &plan, None, 10)).unwrap();
    let grain = rig.facade.with_store(|m| m.get(&records[0].1)).unwrap();
    let argv = grain.get_str("tool_content").expect("a result grain carries its content").to_string();
    assert!(argv.contains("--allow-fetch"), "the capability gate is opened: {argv}");
    assert!(argv.contains("--max-response-bytes 4096"), "{argv}");
    assert!(argv.contains("--fuel 5000"), "{argv}");
}

/// `--allow-blob` comes off the DECLARATION, not the runtime string (#106).
///
/// The asymmetry with `--allow-fetch` is the point: egress has a host-side
/// grant behind it that narrows the runtime's permission afterwards, so that
/// flag can be broad. A blob read has no second key — the declaration is the
/// only thing that says yes — so an http-only module must not get the import
/// merely for being `wasm32-areev-io`.
#[cfg(unix)]
#[test]
fn allow_blob_is_passed_only_when_the_module_declared_it() {
    for (caps, want_blob) in [
        (gmail_caps(), false),
        (json!([{"blob": {"read": true}}]), true),
        (
            json!([
                {"http": {"hosts": ["https://gmail.googleapis.com"], "methods": ["POST"],
                          "credentials": ["gmail"]}},
                {"blob": {"read": true}}
            ]),
            true,
        ),
        // Declared and declined: the import stays unlinked.
        (json!([{"blob": {"read": false}}]), false),
    ] {
        let rig = Rig::new();
        let uri = rig.put_blob(b"\0asm-io-module");
        let plan = plan_with_capabilities(
            &rig,
            &uri,
            "wasm32-areev-io",
            Some(caps.clone()),
            Some(json!({"max_blob_bytes": 2048})),
        );
        let fake = fake_sandbox(&rig, "fake-sandbox-blob.sh");
        let exec = areev_run::CodeExecutor::new(Arc::new(Fallback))
            .allow(&uri)
            .cache_dir(rig.dir.join("cache"))
            .sandbox_cmd(fake.to_str().unwrap())
            .with_egress(areev_run::EgressHandle::new(capability_broker()));
        let session = rig.runner(Arc::new(exec)).start(&plan, "r1", json!({}), &opts()).unwrap();
        let RunSession::Finished { outcome, .. } = session else { panic!("expected finish") };
        assert_eq!(outcome, RunOutcome::Completed);

        let records = rig.facade.with_store(|m| m.step_actions("ops", &plan, None, 10)).unwrap();
        let grain = rig.facade.with_store(|m| m.get(&records[0].1)).unwrap();
        let argv = grain.get_str("tool_content").unwrap_or_default().to_string();
        assert_eq!(
            argv.contains("--allow-blob"),
            want_blob,
            "declaration {caps} should{} open the blob gate: {argv}",
            if want_blob { "" } else { " not" }
        );
        assert!(argv.contains("--max-blob-bytes 2048"), "the ceiling rides too: {argv}");
    }
}

/// A capability module with no broker configured fails with the fix named,
/// rather than starting and having every call refused by a broker that is not
/// there.
#[cfg(unix)]
#[test]
fn a_capability_module_without_a_broker_says_so() {
    let rig = Rig::new();
    let uri = rig.put_blob(b"\0asm-io-module");
    let plan = plan_with_capabilities(&rig, &uri, "wasm32-areev-io", Some(gmail_caps()), None);
    let fake = fake_sandbox(&rig, "fake-sandbox-nb.sh");
    let exec = areev_run::CodeExecutor::new(Arc::new(Fallback))
        .allow(&uri)
        .cache_dir(rig.dir.join("cache"))
        .sandbox_cmd(fake.to_str().unwrap());

    let session = rig.runner(Arc::new(exec)).start(&plan, "r1", json!({}), &opts()).unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected a terminal") };
    assert!(matches!(outcome, RunOutcome::Failed { .. }), "got {outcome:?}");
    let records = rig.facade.with_store(|m| m.step_actions("ops", &plan, None, 10)).unwrap();
    let grain = rig.facade.with_store(|m| m.get(&records[0].1)).unwrap();
    let detail = grain.get_str("failure_detail").unwrap_or_default().to_string();
    assert!(detail.contains("no credential broker"), "{detail}");
}
