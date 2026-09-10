//! `mcp.call` — call a tool on an MCP server, through the broker.
//!
//! MCP is JSON-RPC 2.0 over HTTP, so what this adds over [`http.call`] is one
//! envelope and one unwrap: it builds `tools/call` around the caller's
//! `arguments` and returns the `result` rather than a string containing a
//! document containing the answer. The `arguments` object is **sliced out of
//! the input and copied verbatim** — never reparsed and reserialized — so a
//! server that cares about key order or number formatting sees exactly what
//! the caller wrote.
//!
//! Everything that decides *which* server may be reached, with which
//! credential, stays where it belongs: the Definition's `capabilities` and the
//! host's grant. This module cannot widen either.
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
//!   "headers": { "Content-Type": "application/json", "Accept": "application/json" } }
//! ```
//!
//! `tool` + `arguments` build `params`; a caller that needs another MCP method
//! may pass `method` (default `tools/call`) and `params` verbatim instead.
//! `headers` defaults to `{"Content-Type": "application/json"}` — a JSON-RPC
//! endpoint receiving `text/plain` answers 415 — and the pack must declare
//! every header name it sets.
//!
//! **Output**: `{"status": 200, "result": {…}}`, `{"status": 200, "error":
//! {…}}` for a JSON-RPC error, or the broker's refusal verbatim.
//!
//! **Not supported in v1**: the SSE/streamable transport. One request, one
//! response, synchronous — the same restriction the sandbox's single `fetch`
//! import makes for every tool here.
#![no_std]

extern crate alloc;

use alloc::string::String;

use areev_tool_common::abi;
use areev_tool_common::json::{self, Obj};
use areev_tool_common::jsonrpc::{self, Call};

areev_tool_common::guest_abi!();

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
    jsonrpc::dispatch(&Call {
        url: &c.url,
        method: &method,
        params,
        id: c.id,
        credential: c.credential,
        headers: c.headers,
    })
}
