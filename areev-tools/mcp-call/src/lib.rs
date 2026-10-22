//! `mcp.call` — call a tool on an MCP server, through the broker.
//!
//! MCP is JSON-RPC 2.0 over HTTP, so what this adds over [`http.call`] is the
//! transport's handshake, one envelope and one unwrap: it builds `tools/call`
//! around the caller's `arguments` and returns the `result` rather than a
//! string containing a document containing the answer. The `arguments` object
//! is **sliced out of the input and copied verbatim** — never reparsed and
//! reserialized — so a server that cares about key order or number formatting
//! sees exactly what the caller wrote.
//!
//! Everything that decides *which* server may be reached, with which
//! credential, stays where it belongs: the Definition's `capabilities` and the
//! host's grant. This module cannot widen either.
//!
//! ## Transport: MCP "Streamable HTTP" (#395)
//!
//! A server that follows the spec (rev 2025-06-18) strictly wants four things
//! a single bare POST does not give it, and this blob now does all four:
//!
//! 1. `Accept: application/json, text/event-stream` on every request;
//! 2. `initialize`, then the `notifications/initialized` notification, before
//!    any other request;
//! 3. the `Mcp-Session-Id` the server returned in the initialize RESPONSE
//!    HEADER, sent back on every later request (with `MCP-Protocol-Version`
//!    set to the version the server negotiated);
//! 4. a reply that may be `text/event-stream` (SSE) rather than JSON — the
//!    JSON-RPC response is the event whose `id` matches the request's.
//!
//! The session id is a response header, and response headers did not cross
//! the broker before #395. The broker now hands back a request-named header
//! from a closed list (`content-type`, `mcp-session-id`) and nothing else;
//! see `docs/security-model.md`.
//!
//! **Sessions across calls.** A wasm guest keeps nothing between invocations
//! (one input, one run, a bump allocator that never frees), and the broker
//! deliberately does not learn MCP. So a session is reused by the CALLER: the
//! output carries `session: {id, protocol_version}`, and passing that object
//! back as the input's `session` skips the handshake. A reused session the
//! server answers `404` for has expired: the blob re-initializes once and
//! retries. With no `session` in the input, every invocation handshakes.
//!
//! **Stateless servers.** An `initialize` that is not answered with a 2xx
//! JSON-RPC `result` (a plain JSON-RPC endpoint that only knows `tools/call`,
//! say) is not an error: the call proceeds without a session. A server that
//! answers `initialize` but issues no session id simply gets no
//! `Mcp-Session-Id`. `"transport": "json"` skips all of it and sends the one
//! pre-#395 POST, byte for byte.
//!
//! **Cost.** Every request is one brokered fetch and spends one `max_calls`
//! unit: 3 per invocation with a fresh handshake, 1 with a reused session, up
//! to 4 when a reused session has expired, 1 under `"transport": "json"`.
//!
//! ## Contract
//!
//! **Input**:
//!
//! ```json
//! { "url": "https://mcp.example.com/mcp",
//!   "credential": "mcp",
//!   "tool": "search_docs",
//!   "arguments": { "q": "invoice 4471" },
//!   "id": 1,
//!   "session": { "id": "…", "protocol_version": "2025-06-18" },
//!   "transport": "streamable",
//!   "headers": { "X-Tenant": "acme" } }
//! ```
//!
//! `tool` + `arguments` build `params`; a caller that needs another MCP method
//! may pass `method` (default `tools/call`) and `params` verbatim instead.
//! `headers` are the caller's own; this blob adds `Content-Type:
//! application/json` and the `Accept` above unless the caller set them, and
//! owns `Mcp-Session-Id` / `MCP-Protocol-Version` outright. The pack must
//! declare every header name that rides — under the default transport that is
//! `Content-Type`, `Accept`, `Mcp-Session-Id` and `MCP-Protocol-Version`.
//!
//! **Output**: `{"status": 200, "result": {…}, "session": {…}}`,
//! `{"status": 200, "error": {…}}` for a JSON-RPC error, `{"status": …,
//! "body": "…"}` for a reply that held no JSON-RPC response, or the broker's
//! refusal verbatim. `session` is present when the server negotiated a
//! version or issued a session id.
#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use areev_tool_common::abi;
use areev_tool_common::json::{self, Obj};
use areev_tool_common::jsonrpc::{self, Call, Common};
use areev_tool_common::sse;

areev_tool_common::guest_abi!();

/// The protocol revision this blob asks for in `initialize`. The server
/// answers with the one it will speak, and THAT is what later requests carry.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// What a Streamable HTTP client MUST accept.
const ACCEPT: &str = "application/json, text/event-stream";
/// The JSON-RPC id of the handshake's own request — distinct from any id a
/// caller is likely to pin, so an SSE stream mixing the two cannot confuse
/// one response for the other.
const INIT_ID: &[u8] = b"\"areev-init\"";

/// # Safety
/// The sandbox calls this with the pointer its own `alloc` returned.
#[no_mangle]
pub unsafe extern "C" fn run(ptr: i32, len: i32) {
    let input = unsafe { abi::input(ptr, len) };
    abi::emit_str(&shape(input));
}

fn shape(input: &[u8]) -> String {
    let c = match jsonrpc::common(input) {
        Ok(c) => c,
        Err(e) => return abi::error_json(e),
    };
    let method = c.method.clone().unwrap_or_else(|| String::from("tools/call"));
    let params = match c.params {
        // A caller that spelled `params` itself owns the whole shape — that is
        // how any MCP method beyond `tools/call` is reachable without this
        // blob learning a table of them.
        Some(p) => String::from(core::str::from_utf8(p).unwrap_or("{}")),
        None => {
            let Some(tool) = json::member_str(input, "tool").or_else(|| json::member_str(input, "name"))
            else {
                return abi::error_json(
                    "input names neither \"tool\" nor \"params\" — one of them says what to call",
                );
            };
            let mut p = Obj::new();
            p.string("name", &tool)
                .raw("arguments", jsonrpc::raw_or(input, "arguments", b"{}"));
            p.finish()
        }
    };
    match json::member_str(input, "transport").as_deref() {
        // The pre-#395 single POST, unchanged — for a server, or a
        // declaration, that cannot take the handshake.
        Some("json") => {
            return jsonrpc::dispatch(&Call {
                url: &c.url,
                method: &method,
                params,
                id: c.id,
                credential: c.credential,
                headers: c.headers,
            })
        }
        None | Some("streamable") => {}
        Some(_) => {
            return abi::error_json("\"transport\" is \"streamable\" (the default) or \"json\"")
        }
    }
    match streamable(input, &c, &method, &params) {
        Ok(out) | Err(out) => out,
    }
}

/// What a request carries of the session: nothing, a negotiated version, an
/// id, or both.
#[derive(Default)]
struct Session {
    id: Option<String>,
    version: Option<String>,
}

/// One broker answer, read.
struct Reply {
    /// The upstream status, raw JSON (a number).
    status: Vec<u8>,
    body: String,
    content_type: Option<String>,
    session_id: Option<String>,
}

impl Reply {
    fn is_2xx(&self) -> bool {
        self.status.len() == 3 && self.status[0] == b'2'
    }
}

/// The handshake when needed, the call, one re-initialization when a reused
/// session turned out to be gone. `Err` is a finished output — a refusal the
/// broker gave, returned verbatim.
fn streamable(input: &[u8], c: &Common, method: &str, params: &str) -> Result<String, String> {
    let version = json::member_str(input, "protocol_version");
    let version = version.as_deref().unwrap_or(PROTOCOL_VERSION);
    let supplied = json::member(input, "session").filter(|s| s.starts_with(b"{"));
    let reused = supplied.is_some_and(|s| json::member_str(s, "id").is_some());
    let mut sess = match supplied {
        Some(s) => Session {
            id: json::member_str(s, "id"),
            version: json::member_str(s, "protocol_version"),
        },
        // A caller calling `initialize` itself is doing the handshake; doing
        // it for them first would initialize twice.
        None if method == "initialize" => Session::default(),
        None => handshake(c, version)?,
    };
    let body = envelope(c.id, method, params);
    let mut r = post(c, &sess, &body)?;
    // 404 on a request carrying a session id is the spec's "this session no
    // longer exists": start a new one, once. Never on a fresh session — a 404
    // there is the endpoint's answer, not an expiry.
    if reused && r.status == b"404" {
        sess = handshake(c, version)?;
        r = post(c, &sess, &body)?;
    }
    if sess.id.is_none() {
        sess.id = r.session_id.clone();
    }
    Ok(finish(&r, c.id, &sess))
}

/// `initialize` → `notifications/initialized`. A server that does not answer
/// `initialize` with a 2xx JSON-RPC `result` is treated as stateless: no
/// session, no notification, and the caller's request goes out next anyway.
fn handshake(c: &Common, version: &str) -> Result<Session, String> {
    let mut p = Obj::new();
    p.string("protocolVersion", version)
        .raw("capabilities", b"{}")
        .raw("clientInfo", br#"{"name":"areev-mcp.call","version":"1"}"#);
    let init = envelope(INIT_ID, "initialize", &p.finish());
    let r = post(c, &Session::default(), &init)?;
    let msg = if r.is_2xx() { message(&r, INIT_ID) } else { None };
    let Some(result) = msg.as_deref().and_then(|m| json::member(m.as_bytes(), "result")) else {
        return Ok(Session::default());
    };
    let sess = Session {
        id: r.session_id.clone(),
        version: json::member_str(result, "protocolVersion"),
    };
    // A notification has no id and no response worth reading (202, empty).
    // Only a broker refusal stops the call: it would refuse the next one too.
    post(c, &sess, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)?;
    Ok(sess)
}

fn envelope(id: &[u8], method: &str, params: &str) -> String {
    let mut env = Obj::new();
    env.string("jsonrpc", "2.0")
        .raw("id", id)
        .string("method", method)
        .raw("params", params.as_bytes());
    env.finish()
}

/// One brokered POST, asking for the two response headers the transport
/// needs.
fn post(c: &Common, sess: &Session, body: &str) -> Result<Reply, String> {
    let mut req = Obj::new();
    req.string("url", &c.url).string("method", "POST");
    if let Some(cred) = &c.credential {
        req.string("credential", cred);
    }
    req.raw("headers", headers(c.headers, sess).as_bytes());
    req.string_bytes("body", body.as_bytes());
    req.raw("response_headers", br#"["Content-Type","Mcp-Session-Id"]"#);
    let request = req.finish();

    let reply = abi::brokered_fetch(request.as_bytes()).map_err(abi::error_json)?;
    let Some(status) = json::member(&reply, "status") else {
        // No status: the broker's own `{"error": ..., "code": ...}`, passed
        // through so the caller keeps the `RUN-E022` it can look up.
        return Err(jsonrpc::lossy(&reply));
    };
    // An older broker ignores `response_headers` and answers without
    // `headers`: no session id, and SSE is then recognised from the body.
    let h = json::member(&reply, "headers").unwrap_or(b"{}");
    Ok(Reply {
        status: status.to_vec(),
        body: json::member_str(&reply, "body").unwrap_or_default(),
        content_type: json::member_str(h, "content-type"),
        session_id: json::member_str(h, "mcp-session-id"),
    })
}

/// The caller's headers, plus what the transport needs. The caller's own
/// `Content-Type` / `Accept` win (it may know its server better); the session
/// pair is the transport's alone, so a caller's copy is dropped.
fn headers(caller: Option<&[u8]>, sess: &Session) -> String {
    let mut h = Obj::new();
    let (mut content_type, mut accept) = (false, false);
    for (name, value) in caller.map(json::members).unwrap_or_default() {
        if name.eq_ignore_ascii_case("mcp-session-id") || name.eq_ignore_ascii_case("mcp-protocol-version") {
            continue;
        }
        content_type |= name.eq_ignore_ascii_case("content-type");
        accept |= name.eq_ignore_ascii_case("accept");
        h.raw(&name, value);
    }
    if !content_type {
        h.string("Content-Type", "application/json");
    }
    if !accept {
        h.string("Accept", ACCEPT);
    }
    if let Some(id) = &sess.id {
        h.string("Mcp-Session-Id", id);
    }
    if let Some(v) = &sess.version {
        h.string("MCP-Protocol-Version", v);
    }
    h.finish()
}

/// The JSON-RPC response to request `id` inside a reply, as text.
///
/// A JSON body is the message itself. An SSE body is searched for the event
/// whose `id` is ours and that carries `result` or `error` — a server may
/// send notifications or its own requests first. Failing an exact match, an
/// `error` with a `null` id (a parse error the server could not correlate)
/// is the answer.
fn message(r: &Reply, id: &[u8]) -> Option<String> {
    let is_response = |m: &[u8]| json::member(m, "result").is_some() || json::member(m, "error").is_some();
    if !is_sse(r) {
        return is_response(r.body.as_bytes()).then(|| r.body.clone());
    }
    let events = sse::events(r.body.as_bytes());
    let found = events
        .iter()
        .find(|e| is_response(e.as_bytes()) && json::member(e.as_bytes(), "id") == Some(id))
        .or_else(|| {
            events.iter().find(|e| {
                json::member(e.as_bytes(), "error").is_some()
                    && json::is_null(json::member(e.as_bytes(), "id"))
            })
        });
    found.cloned()
}

/// Did the server answer with an event stream? By its media type when the
/// broker handed it back; by the body's first field name otherwise (an older
/// broker), where a JSON document cannot begin with a letter or a colon.
fn is_sse(r: &Reply) -> bool {
    if let Some(ct) = &r.content_type {
        let media = ct.split(';').next().unwrap_or("").trim();
        return media.eq_ignore_ascii_case("text/event-stream");
    }
    let b = r.body.trim_start();
    ["data:", "event:", "id:", "retry:", ":"].iter().any(|p| b.starts_with(p))
}

fn finish(r: &Reply, id: &[u8], sess: &Session) -> String {
    let mut out = Obj::new();
    out.raw("status", &r.status);
    match message(r, id) {
        Some(m) => {
            let m = m.as_bytes();
            match json::member(m, "result") {
                Some(result) => out.raw("result", result),
                None => out.raw("error", json::member(m, "error").unwrap_or(b"null")),
            };
        }
        // Not a JSON-RPC response — an HTML error page from a proxy, say.
        // Hand the body back as a string rather than inventing structure.
        None => {
            out.string_bytes("body", r.body.as_bytes());
        }
    }
    if sess.id.is_some() || sess.version.is_some() {
        let mut s = Obj::new();
        if let Some(id) = &sess.id {
            s.string("id", id);
        }
        if let Some(v) = &sess.version {
            s.string("protocol_version", v);
        }
        out.raw("session", s.finish().as_bytes());
    }
    out.finish()
}
