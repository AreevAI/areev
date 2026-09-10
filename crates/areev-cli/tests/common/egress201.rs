//! The #201 fixture the Node and Python suites drive too: a `wasm32-areev-io`
//! tool whose "module" names its upstream, declared for gmail there and
//! sheets elsewhere, run under one set of grants. Included by `cli_smoke.rs`
//! and `mcp_smoke.rs` via `#[path]`.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const SECRETS: [(&str, &str); 2] =
    [("AREEV_TEST_GMAIL_201", "s3cret-gmail"), ("AREEV_TEST_SHEETS_201", "s3cret-sheets")];

/// A local upstream that says whether the broker attached a credential.
pub struct Upstream {
    pub url: String,
    stop: Arc<AtomicBool>,
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
pub fn upstream() -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            let Ok((mut s, _)) = listener.accept() else {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            };
            s.set_nonblocking(false).ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut len = 0usize;
            loop {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    for l in head.lines() {
                        if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    if buf.len() >= end + 4 + len {
                        let method = head.split_whitespace().next().unwrap_or("").to_string();
                        let auth = head.to_ascii_lowercase().contains("\nauthorization:");
                        let body = format!(
                            "{{\"ok\":true,\"auth\":{auth},\"method\":\"{method}\"}}"
                        );
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        );
                        break;
                    }
                }
            }
        }
    });
    Upstream { url: format!("http://127.0.0.1:{port}"), stop }
}

/// The fake sandbox: the Python suite's fixture, driven through `python3` —
/// five calls through the broker, reported as the result. One file for all
/// four surfaces, so "the same module" is literally the same module.
pub fn sandbox_cmd() -> String {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../areev-py/tests/egress_fetcher.py")
        .canonicalize()
        .unwrap();
    format!("python3 {}", fixture.display())
}

/// ONE declaration. Returns (workflow hex, executor address).
pub fn declare(db: &str, up: &str) -> (String, String) {
    use areev_core::types::{Grain, Tool, ToolKind, Workflow};
    let mut m = areev_store::Areev::open(db).unwrap();
    let uri = m.put_blob(format!("{{\"upstream\": \"{up}\"}}").as_bytes()).unwrap();
    let def = Tool::new("fetcher")
        .kind(ToolKind::Definition)
        .tool_description("a capability tool")
        .executor_uri(&uri)
        .runtime("wasm32-areev-io")
        .capabilities(serde_json::json!([
            {"http": {"hosts": [up], "methods": ["POST"], "credentials": ["gmail"]}},
            {"http": {"hosts": ["https://sheets.example.com"], "methods": ["POST"],
                      "credentials": ["sheets"]}}
        ]))
        .created_at(500)
        .namespace("ops");
    let dh = m.add(&def).unwrap();
    let wf = Workflow::new(vec!["fetcher".into()])
        .bind("fetcher", &dh.to_hex())
        .created_at(600)
        .namespace("ops");
    let wf = m.add(&wf).unwrap().to_hex();
    (wf, uri.trim_start_matches("cas://sha256:").to_string())
}

pub fn grants(up: &str) -> (String, String, String) {
    (
        "gmail=AREEV_TEST_GMAIL_201,sheets=AREEV_TEST_SHEETS_201".to_string(),
        format!("{up},https://sheets.example.com"),
        "fetcher:gmail+sheets:POST".to_string(),
    )
}

/// What the module reported, and every refusal the run journaled.
pub fn outcome(db: &str, run_id: &str) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut m = areev_store::Areev::open(db).unwrap();
    let trace = m.run_trace("ops", run_id, 100).unwrap();
    let exec = trace
        .iter()
        .find(|g| g.get_str("tool_name") == Some("fetcher") && g.get_str("tool_content").is_some())
        .unwrap_or_else(|| panic!("no execution record for the fetcher: {trace:?}"));
    let result: serde_json::Value =
        serde_json::from_str(exec.get_str("tool_content").unwrap()).unwrap();
    let refusals = m
        .run_trace("agent:harness", run_id, 100)
        .unwrap()
        .into_iter()
        .filter(|g| g.get_str("observation_kind") == Some("egress_refusal"))
        .map(|g| serde_json::to_value(&g.fields).unwrap())
        .collect();
    (result, refusals)
}

pub fn assert_outcome(result: &serde_json::Value, refusals: &[serde_json::Value], label: &str) {
    assert_eq!(result["leak"], "", "{label}: the secret never reaches the module");
    assert_eq!(result["allow_fetch"], true, "{label}: the capability gate is opened");
    // An admitted call answers 200 with {status, body}: the upstream's own reply, as a string.
    assert_eq!(result["admitted"]["status"], 200, "{label}: {}", result["admitted"]);
    assert_eq!(result["admitted"]["body"]["status"], 200, "{label}: {}", result["admitted"]);
    let upstream_saw: serde_json::Value =
        serde_json::from_str(result["admitted"]["body"]["body"].as_str().unwrap_or("null")).unwrap();
    assert_eq!(upstream_saw["auth"], true, "{label}: the broker attached the credential");
    assert_eq!(upstream_saw["method"], "POST");
    for k in ["wrong_host", "wrong_method", "undeclared_credential", "unpaired"] {
        assert_eq!(result[k]["status"], 403, "{label}: {k}: {}", result[k]);
        assert_eq!(result[k]["body"]["code"], "RUN-E022", "{label}: {k}: {}", result[k]);
    }
    assert!(
        result["unpaired"]["body"]["error"].as_str().unwrap_or("").contains("no single capability pairs"),
        "{label}: {}",
        result["unpaired"]
    );
    assert_eq!(refusals.len(), 4, "{label}: one Observation per refusal: {refusals:?}");
    assert!(refusals.iter().any(|r| r["destination"] == "https://evil.example.net/steal"));
    assert!(refusals.iter().all(|r| r["run_id"].is_string()), "{label}: refusals name the run");
}
