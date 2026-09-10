//! `http.call` — one brokered HTTP request, and nothing else.
//!
//! ## Why this is the smallest tool in the tree, on purpose
//!
//! A gateway that decides *where a request may go* is code, and code that
//! makes policy decisions has to be read, versioned, and trusted by everyone
//! who installs it. This module makes none: it hands the request to the
//! broker and hands the answer back. Where it may go, which method it may
//! use, which credential it may spend and which headers it may set are the
//! pack's `capabilities` declaration and the host's grant — data, in the
//! memory, replicated with the tool, auditable without reading any code at
//! all. That is the whole point of the tier: the Tool Gateway becomes
//! configuration instead of code (#179).
//!
//! The consequence is that this blob has no opinion to get wrong, and its
//! behaviour cannot drift from its declaration — there is nothing between the
//! two.
//!
//! ## Contract
//!
//! **Input** is the broker's own request shape, forwarded verbatim:
//!
//! ```json
//! { "url": "https://api.example.com/v1/things",
//!   "method": "GET",
//!   "credential": "things",
//!   "headers": { "X-Api-Version": "2026-01-01" },
//!   "body": null }
//! ```
//!
//! **Output** is the broker's own answer, forwarded verbatim:
//! `{"status": 200, "body": "…"}`, or `{"error": "…", "code": "RUN-E022"}`
//! when policy said no. A caller has one shape to learn for every blessed
//! tool, and the refusal keeps the code a reader can look up.
#![no_std]

extern crate alloc;

use areev_tool_common::abi;

areev_tool_common::guest_abi!();

/// # Safety
/// The sandbox calls this with the pointer its own `alloc` returned and the
/// length it wrote there.
#[no_mangle]
pub unsafe extern "C" fn run(ptr: i32, len: i32) {
    let input = unsafe { abi::input(ptr, len) };
    // No parsing, no validation, no rewriting. An input that is not a request
    // is refused by the broker, which is the authority that was going to
    // decide anyway — and its refusal says why in the shape callers already
    // handle. Validating here would only add a second opinion that can drift.
    match abi::brokered_fetch(input) {
        Ok(reply) => abi::emit_bytes(&reply),
        Err(detail) => abi::emit_str(&abi::error_json(detail)),
    }
}
