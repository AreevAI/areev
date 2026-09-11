//! The blessed blobs, exercised as the bytes that ship (#179).
//!
//! These tests read `../areev-tools/dist/*.wasm` — the committed artifacts,
//! not a rebuild — and run them under the real engine against a loopback
//! broker stand-in. That is the only place in the tree where the shipped blob,
//! the frozen import set and the broker wire shape meet, and it is keyless: no
//! credential, no network, no `areev` binary.
//!
//! What each test defends is a property a pack depends on:
//!
//!   * the address in `dist/blessed.json` is the address of the file
//!     (a manifest that drifted from its blob would pin the wrong bytes);
//!   * `http.call` forwards its input to the broker **verbatim** — it makes no
//!     policy decision, which is what makes the Definition's `capabilities`
//!     the whole policy;
//!   * `mcp.call`/`a2a.call` build the JSON-RPC envelope the protocol wants and
//!     unwrap the answer, with the caller's own `arguments` copied byte for
//!     byte;
//!   * `mailbox.poll` (the trigger-connector example, #185) reads a filed feed
//!     by content address and pages it with a cursor;
//!   * `rest.poll` (#231) maps a paginated REST source with JSON pointers out
//!     of the Definition's `config` — so the four cursor rules live in one
//!     reviewed blob instead of one crate per provider;
//!   * and the import gate holds for all of them: a blob that declared no
//!     network does not get `areev::fetch` linked, and one that declared no
//!     blob read does not get `areev::blob_get`.

use areev_sandbox::{run, Limits, SandboxError};
use serde_json::{json, Value};

fn blob(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../areev-tools/dist")
        .join(format!("{name}.wasm"));
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("{}: {e} — run areev-tools/build.sh", path.display()))
}

fn manifest() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../areev-tools/dist/blessed.json");
    serde_json::from_slice(&std::fs::read(path).expect("dist/blessed.json")).unwrap()
}

/// Serialize the process-global broker handshake, exactly as the crate's own
/// tests do — `AREEV_EGRESS_URL` is read once per run.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

/// What one brokered call looked like from the broker's side.
struct Seen {
    path: String,
    token: String,
    body: String,
}

/// A one-shot loopback broker stand-in. `reply` is the body it answers with,
/// under `status`.
fn broker(status: u16, reply: Vec<u8>) -> (String, std::thread::JoinHandle<Seen>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
        let (mut token, mut len) = (String::new(), 0usize);
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap() == 0 || h.trim().is_empty() {
                break;
            }
            let lower = h.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("x-areev-egress-token:") {
                token = v.trim().to_string();
            }
            if let Some(v) = lower.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).unwrap();
        let mut stream = stream;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 {status} \r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                )
                .as_bytes(),
            )
            .unwrap();
        stream.write_all(&reply).unwrap();
        stream.flush().unwrap();
        Seen { path, token, body: String::from_utf8_lossy(&body).to_string() }
    });
    (url, handle)
}

/// Run one blessed blob against a stand-in broker, returning the guest's
/// result and what the broker saw.
fn call(name: &str, input: Value, limits: Limits, status: u16, reply: &str) -> (Value, Seen) {
    let (url, handle) = broker(status, reply.as_bytes().to_vec());
    let _guard = env_lock();
    std::env::set_var("AREEV_EGRESS_URL", &url);
    std::env::set_var("AREEV_EGRESS_TOKEN", "tok-blessed");
    let out = run(&blob(name), &input, &limits);
    std::env::remove_var("AREEV_EGRESS_URL");
    std::env::remove_var("AREEV_EGRESS_TOKEN");
    let seen = handle.join().unwrap();
    (out.expect("the module ran").output, seen)
}

fn io() -> Limits {
    Limits { allow_fetch: true, ..Default::default() }
}

#[test]
fn every_published_address_is_the_address_of_the_file() {
    use sha2_lite::sha256_hex;
    let m = manifest();
    let tools = m["tools"].as_object().expect("tools");
    assert_eq!(tools.len(), 5, "one entry per shipped blob: {:?}", tools.keys());
    for (name, meta) in tools {
        let bytes = blob(name);
        let hex = sha256_hex(&bytes);
        assert_eq!(
            meta["sha256"].as_str().unwrap(),
            hex,
            "{name}: dist/blessed.json pins an address the file does not have — \
             run areev-tools/build.sh and commit both"
        );
        assert_eq!(meta["address"].as_str().unwrap(), format!("cas://sha256:{hex}"));
        assert_eq!(meta["bytes"].as_u64().unwrap() as usize, bytes.len());
    }
}

#[test]
fn http_call_forwards_the_request_verbatim_and_the_answer_verbatim() {
    let input = json!({
        "url": "https://api.example.com/v1/things?since=7",
        "method": "POST",
        "credential": "things",
        "headers": { "X-Api-Version": "2026-01-01" },
        "body": "{\"note\":\"quoted \\\"inner\\\" text\"}"
    });
    let (out, seen) = call(
        "http.call",
        input.clone(),
        io(),
        200,
        r#"{"status":201,"body":"{\"id\":\"t-1\"}"}"#,
    );
    assert_eq!(seen.path, "/", "a fetch goes to the broker's root path");
    assert_eq!(seen.token, "tok-blessed", "the capability token is presented");
    assert_eq!(
        serde_json::from_str::<Value>(&seen.body).unwrap(),
        input,
        "the request reaches the broker unchanged — this tool makes no policy decision, \
         which is what leaves the declaration as the whole policy"
    );
    assert_eq!(
        out,
        json!({ "status": 201, "body": "{\"id\":\"t-1\"}" }),
        "and the broker's answer reaches the caller unchanged, refusal code included"
    );
}

#[test]
fn http_call_hands_back_a_refusal_with_its_code() {
    // What a call to an undeclared host looks like from inside the guest: the
    // broker refuses, and the tool does not dress it up.
    let (out, _) = call(
        "http.call",
        json!({ "url": "https://elsewhere.example.com/", "method": "GET" }),
        io(),
        403,
        r#"{"error":"caller 'gateway' may not reach elsewhere.example.com","code":"RUN-E022"}"#,
    );
    assert_eq!(out["code"], "RUN-E022", "the code survives: {out}");
    assert!(out["error"].as_str().unwrap().contains("may not reach"));
}

#[test]
fn mcp_call_builds_tools_call_and_unwraps_the_result() {
    let (out, seen) = call(
        "mcp.call",
        json!({
            "url": "https://mcp.example.com/mcp",
            "credential": "mcp",
            "tool": "search_docs",
            "arguments": { "q": "invoice 4471", "limit": 3 },
            "id": 7
        }),
        io(),
        200,
        r#"{"status":200,"body":"{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"found\"}]}}"}"#,
    );
    let req: Value = serde_json::from_str(&seen.body).unwrap();
    assert_eq!(req["url"], "https://mcp.example.com/mcp");
    assert_eq!(req["method"], "POST");
    assert_eq!(req["credential"], "mcp");
    assert_eq!(
        req["headers"]["Content-Type"], "application/json",
        "a JSON-RPC endpoint receiving text/plain answers 415"
    );
    let envelope: Value = serde_json::from_str(req["body"].as_str().unwrap()).unwrap();
    assert_eq!(
        envelope,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "search_docs", "arguments": { "q": "invoice 4471", "limit": 3 } }
        })
    );
    assert_eq!(
        out,
        json!({ "status": 200, "result": { "content": [ { "type": "text", "text": "found" } ] } }),
        "one level of unwrap: the caller reads `result`, not a string containing a document"
    );
}

#[test]
fn mcp_call_surfaces_a_jsonrpc_error_as_an_error() {
    let (out, _) = call(
        "mcp.call",
        json!({ "url": "https://mcp.example.com/mcp", "tool": "nope" }),
        io(),
        200,
        r#"{"status":200,"body":"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"}"#,
    );
    assert_eq!(out["status"], 200);
    assert_eq!(out["error"]["code"], -32601);
    assert!(out.get("result").is_none(), "a JSON-RPC error is not a result: {out}");
}

#[test]
fn mcp_call_lets_a_caller_own_the_whole_envelope() {
    // `method` + `params` verbatim is how every MCP method beyond `tools/call`
    // is reachable without this blob carrying a table of them.
    let (_, seen) = call(
        "mcp.call",
        json!({ "url": "https://mcp.example.com/mcp", "method": "tools/list", "params": {} }),
        io(),
        200,
        r#"{"status":200,"body":"{\"result\":{}}"}"#,
    );
    let req: Value = serde_json::from_str(&seen.body).unwrap();
    let envelope: Value = serde_json::from_str(req["body"].as_str().unwrap()).unwrap();
    assert_eq!(envelope["method"], "tools/list");
    assert_eq!(envelope["params"], json!({}));
}

#[test]
fn a2a_call_builds_message_send_from_text() {
    let (out, seen) = call(
        "a2a.call",
        json!({
            "url": "https://partner.example.com/a2a",
            "credential": "partner",
            "text": "Invoice 4471 is approved.",
            "message_id": "m-4471",
            "context_id": "ctx-ap-4471"
        }),
        io(),
        200,
        r#"{"status":200,"body":"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"kind\":\"task\",\"id\":\"t-9\"}}"}"#,
    );
    let req: Value = serde_json::from_str(&seen.body).unwrap();
    let envelope: Value = serde_json::from_str(req["body"].as_str().unwrap()).unwrap();
    assert_eq!(envelope["method"], "message/send");
    assert_eq!(
        envelope["params"]["message"],
        json!({
            "role": "user",
            "parts": [ { "kind": "text", "text": "Invoice 4471 is approved." } ],
            "kind": "message",
            "messageId": "m-4471",
            "contextId": "ctx-ap-4471"
        })
    );
    assert_eq!(out["result"]["id"], "t-9");
}

#[test]
fn a2a_call_refuses_an_input_that_says_nothing_to_send() {
    // No broker is reached at all: the refusal is the tool's own, and it names
    // the three fields that would have worked.
    let _guard = env_lock();
    std::env::remove_var("AREEV_EGRESS_URL");
    let out = run(&blob("a2a.call"), &json!({ "url": "https://partner.example.com/a2a" }), &io())
        .unwrap();
    assert_eq!(out.fetches, 0, "nothing was sent: {}", out.output);
    let err = out.output["error"].as_str().unwrap_or_default();
    assert!(err.contains("text"), "got {}", out.output);
}

#[test]
fn mailbox_poll_reads_a_filed_feed_and_pages_it() {
    let feed = r#"[{"id":"msg-1","subject":"Invoice 4471"},
                   {"id":"msg-2","subject":"Invoice 4472"},
                   {"id":"msg-3","subject":"Invoice 4473"}]"#;
    let limits = Limits { allow_blob: true, ..Default::default() };
    let request = json!({
        "trigger": "abc",
        "connector": "mailbox",
        "max_items": 2,
        "config": { "int:feed_blob": "cas://sha256:0f".to_string() + &"0".repeat(62) }
    });

    let (out, seen) = call("mailbox.poll", request, limits.clone(), 200, feed);
    assert_eq!(seen.path, "/blob", "a blob read goes to the broker's blob path");
    let asked: Value = serde_json::from_str(&seen.body).unwrap();
    assert!(
        asked["uri"].as_str().unwrap().starts_with("cas://sha256:"),
        "it asks by address and cannot enumerate: {asked}"
    );
    assert_eq!(out["items"].as_array().unwrap().len(), 2, "max_items paged it: {out}");
    assert_eq!(out["items"][0]["id"], "msg-1");
    assert_eq!(
        out["items"][0]["payload"]["subject"], "Invoice 4471",
        "the feed's own object becomes the payload, sliced rather than re-encoded"
    );
    assert_eq!(out["cursor"], "msg-2");
    assert_eq!(out["more"], true, "there is a third: {out}");

    // Resume: the cursor is where it left off, and the page that finishes the
    // feed says so by omitting `more`.
    let resumed = json!({
        "trigger": "abc",
        "connector": "mailbox",
        "max_items": 2,
        "cursor": "msg-2",
        "config": { "int:feed_blob": "cas://sha256:0f".to_string() + &"0".repeat(62) }
    });
    let (out, _) = call("mailbox.poll", resumed, limits, 200, feed);
    assert_eq!(out["items"].as_array().unwrap().len(), 1);
    assert_eq!(out["items"][0]["id"], "msg-3");
    assert_eq!(out["cursor"], "msg-3");
    assert!(out.get("more").is_none(), "the feed is drained: {out}");
}

#[test]
fn mailbox_poll_refuses_a_declaration_that_names_no_feed() {
    let limits = Limits { allow_blob: true, ..Default::default() };
    let out = run(&blob("mailbox.poll"), &json!({ "trigger": "abc" }), &limits).unwrap();
    assert_eq!(out.blob_reads, 0);
    assert!(
        out.output["error"].as_str().unwrap_or_default().contains("int:feed_blob"),
        "got {}",
        out.output
    );
}

// ── rest.poll (#231) ───────────────────────────────────────────────────
//
// One blob, five providers. Everything provider-shaped is a pointer in the
// Definition's `config`, so these tests are the readable form of what a
// reviewer would otherwise have to read five crates to check.

/// The Gmail-shaped declaration from the issue, minus the credential (the
/// broker holds those; the tool only ever names one).
fn gmail_config() -> Value {
    json!({
        "url": "https://gmail.googleapis.com/gmail/v1/users/me/messages",
        "query": { "q": "newer_than:1d" },
        "credential": "gmail",
        "page_size": "maxResults",
        "items": "/messages",
        "id": "/id",
        "order": "newest_first",
        "next_page": { "token_at": "/nextPageToken", "as_query": "pageToken" }
    })
}

fn poll_request(config: Value, cursor: Option<&str>) -> Value {
    let mut req = json!({ "trigger": "t", "connector": "gmail", "max_items": 2, "config": config });
    if let Some(c) = cursor {
        req["cursor"] = json!(c);
    }
    req
}

#[test]
fn rest_poll_maps_a_response_with_pointers_from_the_definition() {
    let (out, seen) = call(
        "rest.poll",
        poll_request(gmail_config(), None),
        io(),
        200,
        r#"{"status":200,"body":"{\"messages\":[{\"id\":\"m-3\",\"snippet\":\"newest\"},{\"id\":\"m-2\"}]}"}"#,
    );
    let req: Value = serde_json::from_str(&seen.body).unwrap();
    let url = req["url"].as_str().unwrap();
    assert!(url.starts_with("https://gmail.googleapis.com/gmail/v1/users/me/messages?"), "{url}");
    assert!(url.contains("q=newer_than%3A1d"), "a static query parameter, encoded: {url}");
    assert!(
        url.contains("maxResults=2"),
        "max_items is asked of the SOURCE as a page size, never used to slice a page here: {url}"
    );
    assert_eq!(req["method"], "GET");
    assert_eq!(req["credential"], "gmail", "the tool names a credential and never holds one");

    assert_eq!(out["items"].as_array().unwrap().len(), 2);
    assert_eq!(out["items"][0]["id"], "m-3");
    assert_eq!(
        out["items"][0]["payload"]["snippet"], "newest",
        "the source's own object becomes the payload, sliced rather than re-encoded"
    );
    assert_eq!(
        out["cursor"], "m-3",
        "newest_first: the watermark is the newest end of the page, not its last element"
    );
    assert!(out.get("more").is_none(), "no page token, nothing in flight: {out}");
}

#[test]
fn rest_poll_carries_a_page_token_in_the_cursor_and_then_settles() {
    // Rule 4: a page token means "come back immediately". The cursor is the
    // only state a connector has between invocations, so the token rides in it.
    let (out, _) = call(
        "rest.poll",
        poll_request(gmail_config(), Some("m-0")),
        io(),
        200,
        r#"{"status":200,"body":"{\"messages\":[{\"id\":\"m-9\"}],\"nextPageToken\":\"CAUQAA\"}"}"#,
    );
    assert_eq!(out["more"], true, "there is a backlog: {out}");
    let carried: Value = serde_json::from_str(out["cursor"].as_str().unwrap()).unwrap();
    assert_eq!(carried["w"], "m-9", "the watermark this drain decided");
    assert_eq!(carried["p"], "CAUQAA", "and the page to ask for next");

    // Feed it back: the token becomes a query parameter, the watermark is held
    // (newest_first — later pages are OLDER and must not move it backwards),
    // and with no token in the reply the cursor collapses to the bare form.
    let (out, seen) = call(
        "rest.poll",
        poll_request(gmail_config(), Some(out["cursor"].as_str().unwrap())),
        io(),
        200,
        r#"{"status":200,"body":"{\"messages\":[{\"id\":\"m-8\"}]}"}"#,
    );
    let req: Value = serde_json::from_str(&seen.body).unwrap();
    assert!(req["url"].as_str().unwrap().contains("pageToken=CAUQAA"), "{}", req["url"]);
    assert_eq!(out["items"][0]["id"], "m-8", "the older page still delivers its items");
    assert_eq!(out["cursor"], "m-9", "and the watermark did not walk backwards into it");
    assert!(out.get("more").is_none(), "drained: {out}");
}

#[test]
fn rest_poll_advances_over_an_oldest_first_page() {
    // The other ordering, and the other carry rule: each page's last item is
    // the newest thing looked at, so the watermark moves with every page and a
    // crash mid-drain resumes from the last page actually delivered.
    let config = json!({
        "url": "https://api.example.com/events",
        "items": "/data",
        "id": "/event_id",
        "cursor_param": "after"
    });
    let (out, seen) = call(
        "rest.poll",
        poll_request(config, Some("e-1")),
        io(),
        200,
        r#"{"status":200,"body":"{\"data\":[{\"event_id\":\"e-2\"},{\"event_id\":\"e-3\"}]}"}"#,
    );
    let req: Value = serde_json::from_str(&seen.body).unwrap();
    assert!(req["url"].as_str().unwrap().contains("after=e-1"), "the stored watermark is sent back");
    assert_eq!(out["cursor"], "e-3", "oldest_first: the last item looked at");
}

#[test]
fn rest_poll_takes_the_watermark_from_a_pointer_when_the_source_publishes_one() {
    // A server-side watermark (historyId, deltaLink) is always better than an
    // item id, and `cursor_from` is how a declaration says so. `-` is the last
    // element of an array along the way.
    let config = json!({
        "url": "https://api.example.com/sync",
        "items": "/changes",
        "id": "/id",
        "cursor_from": "/changes/-/seq"
    });
    let (out, _) = call(
        "rest.poll",
        poll_request(config, Some("7")),
        io(),
        200,
        r#"{"status":200,"body":"{\"changes\":[{\"id\":\"c-8\",\"seq\":8},{\"id\":\"c-9\",\"seq\":9}]}"}"#,
    );
    assert_eq!(out["cursor"], "9", "a number is a perfectly good watermark");
}

#[test]
fn rest_poll_leaves_the_cursor_alone_on_an_empty_page() {
    // Rule 1. Absent is "leave it where it is"; `null` would rewind the source,
    // which for a mailbox means re-processing everything.
    let (out, _) = call(
        "rest.poll",
        poll_request(gmail_config(), Some("m-9")),
        io(),
        200,
        r#"{"status":200,"body":"{\"messages\":[]}"}"#,
    );
    assert_eq!(out["items"].as_array().unwrap().len(), 0);
    assert!(out.get("cursor").is_none(), "no cursor key at all: {out}");
    assert!(out.get("more").is_none());
}

#[test]
fn rest_poll_ends_a_drain_that_finished_on_an_empty_page() {
    // The one case where restating an unchanged watermark is doing something:
    // the stored cursor still carries an exhausted page token, and leaving it
    // alone would re-ask for that same page on every tick, forever.
    let carried = r#"{"w":"m-9","p":"CAUQAA"}"#;
    let (out, _) = call(
        "rest.poll",
        poll_request(gmail_config(), Some(carried)),
        io(),
        200,
        r#"{"status":200,"body":"{\"messages\":[]}"}"#,
    );
    assert_eq!(out["cursor"], "m-9", "the token is dropped and the watermark kept: {out}");
    assert!(out.get("more").is_none());
}

#[test]
fn rest_poll_reports_an_upstream_failure_instead_of_an_empty_page() {
    // The failure this whole tier exists to make loud: a poll that answered
    // nothing must not look like a healthy source with nothing new.
    let (out, _) = call(
        "rest.poll",
        poll_request(gmail_config(), Some("m-9")),
        io(),
        200,
        r#"{"status":503,"body":"upstream is down"}"#,
    );
    let err = out["error"].as_str().unwrap_or_default();
    assert!(err.contains("503"), "got {out}");
    assert!(out.get("items").is_none(), "an error is not a page: {out}");
}

#[test]
fn rest_poll_forwards_a_broker_refusal_with_its_code() {
    // Reach is the declaration's business, not this blob's: a URL outside the
    // capabilities block is refused by the broker, and the refusal arrives
    // unchanged so an operator can look the code up.
    let (out, _) = call(
        "rest.poll",
        poll_request(gmail_config(), None),
        io(),
        403,
        r#"{"error":"caller 'gmail' may not reach gmail.googleapis.com","code":"RUN-E022"}"#,
    );
    assert_eq!(out["code"], "RUN-E022", "{out}");
}

#[test]
fn rest_poll_refuses_a_declaration_that_names_no_url() {
    let out = run(&blob("rest.poll"), &json!({ "trigger": "t", "config": {} }), &io()).unwrap();
    assert_eq!(out.fetches, 0, "nothing was sent: {}", out.output);
    assert!(out.output["error"].as_str().unwrap_or_default().contains("url"), "{}", out.output);
}

#[test]
fn two_definitions_naming_one_blob_poll_two_apis() {
    // The point of the whole exercise: no per-provider code. Two `config`
    // blocks, one address, two different APIs read correctly.
    let (gmail, gmail_seen) = call(
        "rest.poll",
        poll_request(gmail_config(), Some("m-1")),
        io(),
        200,
        r#"{"status":200,"body":"{\"messages\":[{\"id\":\"m-2\"}]}"}"#,
    );
    let xero = json!({
        "url": "https://api.xero.com/api.xro/2.0/Invoices",
        "headers": { "Accept": "application/json" },
        "credential": "xero",
        "items": "/Invoices",
        "id": "/InvoiceID",
        "cursor_param": "If-Modified-Since"
    });
    let (xero_out, xero_seen) = call(
        "rest.poll",
        poll_request(xero, Some("2026-09-01")),
        io(),
        200,
        r#"{"status":200,"body":"{\"Invoices\":[{\"InvoiceID\":\"INV-88\",\"Total\":1240}]}"}"#,
    );

    let g: Value = serde_json::from_str(&gmail_seen.body).unwrap();
    let x: Value = serde_json::from_str(&xero_seen.body).unwrap();
    assert!(g["url"].as_str().unwrap().starts_with("https://gmail.googleapis.com/"));
    assert!(x["url"].as_str().unwrap().starts_with("https://api.xero.com/"));
    assert_eq!(x["headers"]["Accept"], "application/json");
    assert_eq!(gmail["items"][0]["id"], "m-2");
    assert_eq!(xero_out["items"][0]["id"], "INV-88");
    assert_eq!(xero_out["items"][0]["payload"]["Total"], 1240);
    assert_eq!(xero_out["cursor"], "INV-88");
}

#[test]
fn the_import_gate_holds_for_every_shipped_blob() {
    // The declaration decides which imports exist. A network tool on a host
    // that linked no `fetch` is refused BY NAME before one instruction — and a
    // blob reader is refused the same way, even on a host that allowed fetch,
    // because the two gates are independent.
    for name in ["http.call", "mcp.call", "a2a.call", "rest.poll"] {
        match run(&blob(name), &Value::Null, &Limits::default()).unwrap_err() {
            SandboxError::ForbiddenImport { module, name: import } => {
                assert_eq!((module.as_str(), import.as_str()), ("areev", "fetch"), "{name}");
            }
            other => panic!("{name}: expected a forbidden import, got {other}"),
        }
    }
    match run(&blob("mailbox.poll"), &Value::Null, &io()).unwrap_err() {
        SandboxError::ForbiddenImport { name, .. } => assert_eq!(name, "blob_get"),
        other => panic!("expected a forbidden import, got {other}"),
    }
}

/// SHA-256 in twenty lines, so a test that checks a published address does not
/// add a dependency to the package whose whole argument is its dependency
/// tree.
mod sha2_lite {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    pub fn sha256_hex(data: &[u8]) -> String {
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut msg = data.to_vec();
        let bits = (data.len() as u64) * 8;
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bits.to_be_bytes());
        for chunk in msg.chunks(64) {
            let mut w = [0u32; 64];
            for (i, word) in chunk.chunks(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
                (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (i, v) in [a, b, c, d, e, f, g, hh].into_iter().enumerate() {
                h[i] = h[i].wrapping_add(v);
            }
        }
        h.iter().map(|v| format!("{v:08x}")).collect()
    }
}
