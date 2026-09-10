//! `a2a.call` — send a message to an A2A agent, through the broker.
//!
//! A2A is JSON-RPC 2.0 over HTTP like MCP, so this is the same envelope with
//! a different default method (`message/send`) and a different `params` shape.
//! They are separate blobs rather than one with a mode flag because a pack
//! pins an address and declares a capability per tool: an agent-to-agent
//! endpoint and an MCP server are different hosts with different credentials,
//! and one blob doing both would be one address to grant for both.
//!
//! ## Contract
//!
//! **Input**, in ascending order of control:
//!
//! ```json
//! { "url": "https://partner.example.com/a2a",
//!   "credential": "partner",
//!   "text": "Invoice 4471 is approved.",
//!   "message_id": "m-4471",
//!   "context_id": "ctx-ap-4471" }
//! ```
//!
//! `text` builds the A2A message (`role: "user"`, one text part). A caller
//! that needs attachments or metadata passes `message` verbatim instead; one
//! that needs another A2A method passes `method` and `params`.
//!
//! **Output**: `{"status": 200, "result": {…}}` — the Task or Message the
//! agent returned — or a JSON-RPC `error`, or the broker's refusal verbatim.
//!
//! **Not supported in v1**: `message/stream` (SSE) and push notifications.
//! One request, one response.
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
    let method = c.method.clone().unwrap_or_else(|| String::from("message/send"));
    let params = match c.params {
        Some(p) => String::from(core::str::from_utf8(p).unwrap_or("{}")),
        None => {
            let mut p = Obj::new();
            match json::member(input, "message").filter(|m| m.starts_with(b"{")) {
                // A caller that built the message owns it: A2A parts carry
                // files and structured data this blob has no business
                // inventing a spelling for.
                Some(m) => {
                    p.raw("message", m);
                }
                None => {
                    let Some(text) = json::member_str(input, "text") else {
                        return abi::error_json(
                            "input names none of \"text\", \"message\" or \"params\" — one of \
                             them says what to send",
                        );
                    };
                    let mut part = Obj::new();
                    part.string("kind", "text").string("text", &text);
                    let part = part.finish();
                    let mut msg = Obj::new();
                    msg.string("role", "user");
                    msg.raw("parts", alloc::format!("[{part}]").as_bytes());
                    msg.string("kind", "message");
                    if let Some(id) = json::member_str(input, "message_id") {
                        msg.string("messageId", &id);
                    }
                    if let Some(id) = json::member_str(input, "context_id") {
                        msg.string("contextId", &id);
                    }
                    if let Some(id) = json::member_str(input, "task_id") {
                        msg.string("taskId", &id);
                    }
                    p.raw("message", msg.finish().as_bytes());
                }
            }
            if let Some(cfg) = json::member(input, "configuration").filter(|v| v.starts_with(b"{")) {
                p.raw("configuration", cfg);
            }
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
