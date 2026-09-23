//! #339 end to end: the committed, blessed `http.call` blob, run by the REAL
//! `areev-sandbox` binary, answered by the real broker — a 2 MiB + 13 byte
//! non-UTF-8 document downloaded under a 25 MiB declaration arrives in CAS
//! byte-exact, and the same document under a 1 MiB declaration is refused
//! with the 1 MiB limit named.
//!
//! `areev-sandbox` is a standalone package (not a workspace member), so this
//! suite needs its binary built first:
//!
//! ```sh
//! cargo build --manifest-path areev-sandbox/Cargo.toml
//! ```
//!
//! or `AREEV_SANDBOX=/path/to/areev-sandbox`. Without one the tests say so on
//! stderr and pass vacuously; the broker-level cases in `broker.rs` cover the
//! same limits without the sandbox.

use areev_cal::AreevFacade;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    CallerGrant, CodeExecutor, EgressGrants, EgressHandle, EgressPolicy, ExecResult,
    HostToolExecutor, RunOptions, RunSession, Runner, ScriptedClock,
};
use areev_run_core::RunOutcome;
use areev_store::Areev;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

struct Fallback;
impl HostToolExecutor for Fallback {
    fn execute(&self, tool_name: &str, _h: &str, _i: &Value, _k: &str) -> ExecResult {
        ExecResult::Ok(json!({ "fell_back_to_tool_cmd": tool_name }))
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sandbox() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AREEV_SANDBOX") {
        return Some(PathBuf::from(p));
    }
    ["debug", "release"]
        .iter()
        .map(|p| root().join("areev-sandbox/target").join(p).join("areev-sandbox"))
        .find(|p| p.exists())
}

/// 2 MiB + 13 bytes, not UTF-8.
fn document() -> Vec<u8> {
    let mut bytes: Vec<u8> =
        (0..2 * 1024 * 1024 + 13u32).map(|i| 0x80 | (i.wrapping_mul(13) % 127) as u8).collect();
    bytes[..5].copy_from_slice(b"%PDF\xff");
    assert!(std::str::from_utf8(&bytes).is_err());
    bytes
}

/// A one-shot upstream serving `body` with a Content-Length.
fn upstream(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let t = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
        }
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        );
        let _ = s.write_all(&body);
        let _ = s.flush();
        // Let the broker finish (or abandon) its read before closing.
        let mut sink = [0u8; 16];
        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let _ = s.read(&mut sink);
    });
    (origin, t)
}

/// Install the blessed blob as a Definition declaring `max_response_bytes`,
/// run one node through the real sandbox, and return its result content.
fn download(sandbox: &std::path::Path, declared: u64) -> (Arc<AreevFacade>, tempfile::TempDir, Value) {
    let dir = tempfile::tempdir().unwrap();
    let facade = Arc::new(AreevFacade::new(
        Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap(),
    ));
    let wasm = std::fs::read(root().join("areev-tools/dist/http.call.wasm")).unwrap();
    let uri = facade.with_store(|m| m.put_blob(&wasm)).unwrap();

    let bytes = document();
    let (origin, server) = upstream(bytes);
    let def = Tool::new("fetch_statement")
        .kind(ToolKind::Definition)
        .tool_description("the blessed http.call")
        .executor_uri(&uri)
        .runtime("wasm32-areev-io")
        .capabilities(json!([{"http": {"hosts": [origin], "methods": ["GET"]}}]))
        .runtime_limits(json!({"max_response_bytes": declared}))
        .created_at(500)
        .namespace("ops");
    let dh = facade.with_store(|m| m.add(&def)).unwrap();
    let wf = Workflow::new(vec!["fetch_statement".into()])
        .bind("fetch_statement", &dh.to_hex())
        .created_at(600)
        .namespace("ops");
    let plan = facade.with_store(|m| m.add(&wf)).unwrap();

    let broker = areev_run::Broker::start(
        EgressPolicy::from_config(Some(&json!({"int:allowed_outbound_hosts": [origin]}))).unwrap(),
        Default::default(),
        EgressGrants::new().grant("fetch_statement", CallerGrant::new().method("GET")),
        "RUN-E022",
    )
    .unwrap();
    let exec = CodeExecutor::new(Arc::new(Fallback))
        .allow(&uri)
        .cache_dir(dir.path().join("cache"))
        .sandbox_cmd(sandbox.to_str().unwrap())
        .with_egress(EgressHandle::new(Arc::new(broker)));
    let runner = Runner {
        facade: Arc::clone(&facade),
        clock: Arc::new(ScriptedClock::new((0..200).map(|i| 1_755_000_000_000 + i * 10).collect())),
        executor: Arc::new(exec),
        llm: None,
        observer: None,
        ns: "ops".into(),
        principal: "user:runner".into(),
    };
    let input = json!({"url": format!("{origin}/statement.pdf"), "response_mode": "artifact"});
    let session = runner
        .start(&plan, "r339", input, &RunOptions { workers: 1, ..Default::default() })
        .unwrap();
    server.join().unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected a terminal") };
    assert_eq!(outcome, RunOutcome::Completed, "{outcome:?}");
    let records = facade.with_store(|m| m.step_actions("ops", &plan, None, 10)).unwrap();
    let grain = facade.with_store(|m| m.get(&records[0].1)).unwrap();
    let content = grain.get_str("tool_content").expect("a result carries content").to_string();
    let answer: Value = serde_json::from_str(&content).unwrap_or(Value::String(content));
    (facade, dir, answer)
}

#[test]
fn blessed_http_call_downloads_a_2_mib_document_under_a_25_mib_declaration() {
    let Some(sandbox) = sandbox() else {
        eprintln!("SKIPPED: no areev-sandbox binary — cargo build --manifest-path areev-sandbox/Cargo.toml");
        return;
    };
    let (facade, _dir, answer) = download(&sandbox, 26_214_400);
    let bytes = document();
    assert_eq!(answer["status"], 200, "{answer}");
    assert_eq!(answer["bytes"], bytes.len(), "{answer}");
    let stored = facade.with_store(|m| m.get_blob(answer["ref"].as_str().unwrap())).unwrap();
    assert_eq!(stored.len(), 2 * 1024 * 1024 + 13);
    assert_eq!(stored, bytes, "byte-exact");
    use sha2::Digest;
    let sha = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&bytes)));
    assert_eq!(answer["sha256"], sha);
}

#[test]
fn blessed_http_call_refuses_it_under_a_1_mib_declaration_naming_the_limit() {
    let Some(sandbox) = sandbox() else {
        eprintln!("SKIPPED: no areev-sandbox binary — cargo build --manifest-path areev-sandbox/Cargo.toml");
        return;
    };
    let (_facade, _dir, answer) = download(&sandbox, 1_048_576);
    assert_eq!(answer["code"], "RUN-E022", "{answer}");
    assert!(answer["error"].as_str().unwrap_or("").contains("1048576-byte"), "{answer}");
    assert!(answer.get("ref").is_none() && answer.get("bytes").is_none(), "no partial success: {answer}");
}
