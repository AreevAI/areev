//! #395 end to end: the committed, blessed `mcp.call` blob, run by the REAL
//! `areev-sandbox` binary and answered by the REAL broker, against a local
//! MCP server that follows "Streamable HTTP" strictly — it refuses a request
//! without `Accept: application/json, text/event-stream`, refuses anything
//! but `initialize` without the `Mcp-Session-Id` it issued in a RESPONSE
//! HEADER, wants `notifications/initialized` before `tools/call`, and answers
//! the call as `text/event-stream`. Beside it: a stateless JSON-RPC server
//! that knows only `tools/call`, which must keep working.
//!
//! `areev-sandbox` is a standalone package (not a workspace member), so this
//! suite needs its binary built first:
//!
//! ```sh
//! cargo build --manifest-path areev-sandbox/Cargo.toml
//! ```
//!
//! or `AREEV_SANDBOX=/path/to/areev-sandbox`. Without one the tests say so on
//! stderr and pass vacuously; `broker.rs`'s #395 cases cover the header
//! exposure and `areev-sandbox/tests/blessed_tools.rs` the blob's handshake
//! against a stand-in broker, both without the other half.

use areev_cal::AreevFacade;
use areev_core::types::{Grain, Tool, ToolKind, Workflow};
use areev_run::{
    CallerGrant, CodeExecutor, Credential, CredentialSource, EgressGrants, EgressHandle,
    EgressPolicy, ExecResult, HostToolExecutor, RunOptions, RunSession, Runner, ScriptedClock,
};
use areev_run_core::RunOutcome;
use areev_store::Areev;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
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

const TOKEN: &str = "tok-395-static-bearer";
const SESSION: &str = "sess-395-4f2a";

/// One request the fake server received: lowercased header map + JSON body.
#[derive(Debug, Clone)]
struct Seen {
    headers: HashMap<String, String>,
    body: Value,
}

fn read_request(s: &mut TcpStream) -> Seen {
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let mut headers = HashMap::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).unwrap();
    Seen { headers, body: serde_json::from_slice(&body).unwrap_or(Value::Null) }
}

fn answer(s: &mut TcpStream, status: u16, extra: &str, body: &str) {
    let _ = write!(
        s,
        "HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = s.flush();
}

/// A strict Streamable-HTTP MCP server for exactly `n` requests.
fn strict_server(n: usize) -> (String, std::thread::JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let t = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut initialized = false;
        for _ in 0..n {
            let (mut s, _) = listener.accept().unwrap();
            let req = read_request(&mut s);
            seen.push(req.clone());
            let h = |k: &str| req.headers.get(k).map(String::as_str).unwrap_or("");
            if h("authorization") != format!("Bearer {TOKEN}") {
                answer(&mut s, 401, "", "");
                continue;
            }
            let accept = h("accept");
            if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
                answer(&mut s, 406, "", "Not Acceptable: Client must accept both");
                continue;
            }
            let method = req.body["method"].as_str().unwrap_or("").to_string();
            if method == "initialize" {
                let body = json!({ "jsonrpc": "2.0", "id": req.body["id"],
                    "result": { "protocolVersion": "2025-06-18", "capabilities": { "tools": {} },
                                "serverInfo": { "name": "strict", "version": "1" } } });
                answer(
                    &mut s,
                    200,
                    &format!("Content-Type: application/json\r\nMcp-Session-Id: {SESSION}\r\n"),
                    &body.to_string(),
                );
                continue;
            }
            match h("mcp-session-id") {
                "" => {
                    answer(&mut s, 400, "", "Bad Request: No valid session ID provided");
                    continue;
                }
                id if id != SESSION => {
                    answer(&mut s, 404, "", "Session not found");
                    continue;
                }
                _ => {}
            }
            if h("mcp-protocol-version") != "2025-06-18" {
                answer(&mut s, 400, "", "Bad Request: unsupported protocol version");
                continue;
            }
            match method.as_str() {
                "notifications/initialized" => {
                    initialized = true;
                    answer(&mut s, 202, "", "");
                }
                "tools/call" if initialized => {
                    let id = req.body["id"].clone();
                    let progress = json!({ "jsonrpc": "2.0", "method": "notifications/progress",
                                           "params": { "progress": 1, "total": 2 } });
                    let result = json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [
                        { "type": "text",
                          "text": format!("{} paid", req.body["params"]["arguments"]["invoice"]) }
                    ] } });
                    let sse = format!(
                        "event: message\ndata: {progress}\n\nevent: message\ndata: {result}\n\n"
                    );
                    answer(&mut s, 200, "Content-Type: text/event-stream\r\n", &sse);
                }
                _ => answer(&mut s, 400, "", "Bad Request: not initialized"),
            }
        }
        seen
    });
    (origin, t)
}

/// A stateless JSON-RPC endpoint that knows only `tools/call` and answers
/// everything with JSON, for exactly `n` requests.
fn stateless_server(n: usize) -> (String, std::thread::JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let t = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..n {
            let (mut s, _) = listener.accept().unwrap();
            let req = read_request(&mut s);
            seen.push(req.clone());
            let id = req.body["id"].clone();
            let body = if req.body["method"] == "tools/call" {
                json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [
                    { "type": "text", "text": "stateless ok" } ] } })
            } else {
                json!({ "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32601, "message": "Method not found" } })
            };
            answer(&mut s, 200, "Content-Type: application/json\r\n", &body.to_string());
        }
        seen
    });
    (origin, t)
}

/// Install the blessed `mcp.call` as a Definition declaring `headers`, run
/// one node with `input` through the real sandbox and broker, and return the
/// result content plus every egress_call Observation journaled.
fn run_mcp_call(
    sandbox: &std::path::Path,
    origin: &str,
    headers: Value,
    input: Value,
) -> (Value, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let facade = Arc::new(AreevFacade::new(
        Areev::open(dir.path().join("m.db").to_str().unwrap()).unwrap(),
    ));
    let wasm = std::fs::read(root().join("areev-tools/dist/mcp.call.wasm")).unwrap();
    let uri = facade.with_store(|m| m.put_blob(&wasm)).unwrap();
    let def = Tool::new("mcp_lookup")
        .kind(ToolKind::Definition)
        .tool_description("the blessed mcp.call")
        .executor_uri(&uri)
        .runtime("wasm32-areev-io")
        .capabilities(json!([{ "http": {
            "hosts": [origin], "methods": ["POST"], "credentials": ["mcp"], "headers": headers } }]))
        .created_at(500)
        .namespace("ops");
    let dh = facade.with_store(|m| m.add(&def)).unwrap();
    let wf = Workflow::new(vec!["mcp_lookup".into()])
        .bind("mcp_lookup", &dh.to_hex())
        .created_at(600)
        .namespace("ops");
    let plan = facade.with_store(|m| m.add(&wf)).unwrap();

    let creds: BTreeMap<String, CredentialSource> =
        [("mcp".to_string(), Credential::Bearer(TOKEN.into()).into())].into();
    let broker = areev_run::Broker::start(
        EgressPolicy::from_config(Some(&json!({"int:allowed_outbound_hosts": [origin]}))).unwrap(),
        creds,
        EgressGrants::new()
            .grant("mcp_lookup", CallerGrant::new().method("POST").credential("mcp")),
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
    let session = runner
        .start(&plan, "r395", input, &RunOptions { workers: 1, ..Default::default() })
        .unwrap();
    let RunSession::Finished { outcome, .. } = session else { panic!("expected a terminal") };
    assert_eq!(outcome, RunOutcome::Completed, "{outcome:?}");
    let records = facade.with_store(|m| m.step_actions("ops", &plan, None, 10)).unwrap();
    let grain = facade.with_store(|m| m.get(&records[0].1)).unwrap();
    let content = grain.get_str("tool_content").expect("a result carries content").to_string();
    let result: Value = serde_json::from_str(&content).unwrap_or(Value::String(content));
    let calls = facade
        .with_store(|m| {
            m.recent_live_scoped(
                &[areev_core::authz::HARNESS_NS.to_string()],
                Some(areev_core::types::GrainType::Observation),
                50,
            )
        })
        .unwrap()
        .into_iter()
        .filter(|g| g.get_str("observation_kind") == Some("egress_call"))
        .map(|g| format!("{g:?}"))
        .collect();
    (result, calls)
}

const STREAMABLE_HEADERS: &[&str] = &["Content-Type", "Accept", "Mcp-Session-Id", "MCP-Protocol-Version"];

#[test]
fn blessed_mcp_call_speaks_streamable_http_to_a_strict_server() {
    let Some(sandbox) = sandbox() else {
        eprintln!("SKIPPED: no areev-sandbox binary — cargo build --manifest-path areev-sandbox/Cargo.toml");
        return;
    };
    let (origin, server) = strict_server(3);
    let (result, calls) = run_mcp_call(
        &sandbox,
        &origin,
        json!(STREAMABLE_HEADERS),
        json!({ "url": format!("{origin}/mcp"), "credential": "mcp", "tool": "paid_to",
                "arguments": { "invoice": "4471" }, "id": 7 }),
    );
    let seen = server.join().unwrap();

    assert_eq!(
        result,
        json!({ "status": 200,
                "result": { "content": [ { "type": "text", "text": "\"4471\" paid" } ] },
                "session": { "id": SESSION, "protocol_version": "2025-06-18" } }),
        "the SSE reply unwrapped to its JSON-RPC result, and the session on the journaled result"
    );
    let methods: Vec<&str> = seen.iter().map(|s| s.body["method"].as_str().unwrap()).collect();
    assert_eq!(methods, ["initialize", "notifications/initialized", "tools/call"]);
    for s in &seen {
        assert_eq!(s.headers["accept"], "application/json, text/event-stream");
        assert_eq!(s.headers["authorization"], format!("Bearer {TOKEN}"), "the broker attached it");
    }
    assert!(!seen[0].headers.contains_key("mcp-session-id"));
    assert_eq!(seen[2].headers["mcp-session-id"], SESSION);

    // Three brokered calls journaled; the initialize leg records the session
    // id that crossed back, never the credential's value.
    assert_eq!(calls.len(), 3, "{calls:#?}");
    assert!(calls.iter().any(|c| c.contains("response_headers") && c.contains(SESSION)), "{calls:#?}");
    assert!(calls.iter().all(|c| !c.contains(TOKEN)), "{calls:#?}");
}

#[test]
fn blessed_mcp_call_still_works_against_a_stateless_json_server() {
    let Some(sandbox) = sandbox() else {
        eprintln!("SKIPPED: no areev-sandbox binary — cargo build --manifest-path areev-sandbox/Cargo.toml");
        return;
    };
    // Default transport: `initialize` is refused (-32601), so no session and
    // no notification — the call follows and is unwrapped as before.
    let (origin, server) = stateless_server(2);
    let (result, _) = run_mcp_call(
        &sandbox,
        &origin,
        json!(STREAMABLE_HEADERS),
        json!({ "url": format!("{origin}/mcp"), "credential": "mcp", "tool": "paid_to",
                "arguments": {}, "id": 2 }),
    );
    let seen = server.join().unwrap();
    assert_eq!(
        result,
        json!({ "status": 200, "result": { "content": [ { "type": "text", "text": "stateless ok" } ] } })
    );
    assert_eq!(seen[1].body["method"], "tools/call");
    assert!(!seen[1].headers.contains_key("mcp-session-id"));
}

#[test]
fn transport_json_is_the_v1_single_post_under_a_v1_declaration() {
    let Some(sandbox) = sandbox() else {
        eprintln!("SKIPPED: no areev-sandbox binary — cargo build --manifest-path areev-sandbox/Cargo.toml");
        return;
    };
    // A Definition that declares only `Content-Type` (every pre-#395 one)
    // keeps one POST and one `max_calls` unit by naming the v1 transport.
    let (origin, server) = stateless_server(1);
    let (result, calls) = run_mcp_call(
        &sandbox,
        &origin,
        json!(["Content-Type"]),
        json!({ "url": format!("{origin}/mcp"), "credential": "mcp", "tool": "paid_to",
                "id": 2, "transport": "json" }),
    );
    let seen = server.join().unwrap();
    assert_eq!(seen.len(), 1);
    // No `Accept` of the blob's own (ureq's `*/*` default is what remains).
    assert!(!seen[0].headers.get("accept").is_some_and(|a| a.contains("event-stream")));
    assert_eq!(result["result"]["content"][0]["text"], "stateless ok");
    assert_eq!(calls.len(), 1);
    assert!(!calls[0].contains("response_headers"), "{calls:?}");
}

#[test]
fn the_strict_server_really_refuses_the_v1_request() {
    // Positive control: the strict server is strict. The pre-#395 single POST
    // (no `Accept`, no handshake, no session) is refused — so the streamable
    // test above passes because of the transport, not a lenient fixture.
    let Some(sandbox) = sandbox() else {
        eprintln!("SKIPPED: no areev-sandbox binary — cargo build --manifest-path areev-sandbox/Cargo.toml");
        return;
    };
    let (origin, server) = strict_server(1);
    let (result, _) = run_mcp_call(
        &sandbox,
        &origin,
        json!(["Content-Type"]),
        json!({ "url": format!("{origin}/mcp"), "credential": "mcp", "tool": "paid_to",
                "transport": "json" }),
    );
    server.join().unwrap();
    assert_eq!(result["status"], 406, "{result}");
    assert!(result.get("result").is_none(), "{result}");
}
