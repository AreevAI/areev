//! `mailbox.poll` — the example connector, and the keyless proof that a
//! trigger can name its connector by content address (#185).
//!
//! It polls a **filed feed** rather than a mailbox API: the items live in one
//! CAS blob, and this module reads them with `areev::blob_get` through the
//! same broker on the same token a network connector uses. That makes the
//! example runnable in CI with no credential, no network and no host script —
//! the keyless-deterministic floor every example here holds to — while
//! exercising exactly the path a real connector takes: pinned code, a declared
//! capability, a brokered read, journaled.
//!
//! A production connector differs in one line: it declares
//! `{"http": {...}}` and calls `areev::fetch` instead. Everything around it —
//! the pin, the sandbox, the cursor, the dedup fence, the run start — is the
//! same, which is the point being made.
//!
//! ## Contract
//!
//! **Input** is the evaluator's `PollRequest`; the two fields it reads are
//! `cursor` (absent on the first poll) and `config`, which names the feed:
//!
//! ```json
//! { "trigger": "…", "connector": "mailbox", "max_items": 50,
//!   "config": { "int:feed_blob": "cas://sha256:<64 hex>" } }
//! ```
//!
//! The feed blob is a JSON array of items, each an object carrying an `id`:
//!
//! ```json
//! [ { "id": "msg-001", "subject": "Invoice 4471", "from": "…" }, … ]
//! ```
//!
//! **Output** is the `PollResponse` the evaluator expects — items after the
//! cursor, the new cursor, and whether more remain:
//!
//! ```json
//! { "items": [ { "id": "msg-002", "payload": { … } } ],
//!   "cursor": "msg-002", "more": false }
//! ```
//!
//! The cursor is the last emitted `id`, and items are returned in the feed's
//! own order: a cursor that names an id no longer in the feed replays from the
//! start, which is the honest reading of "I do not know where you were" for a
//! source that cannot answer it.
#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use areev_tool_common::abi;
use areev_tool_common::json::{self, Obj};

areev_tool_common::guest_abi!();

/// # Safety
/// The sandbox calls this with the pointer its own `alloc` returned.
#[no_mangle]
pub unsafe extern "C" fn run(ptr: i32, len: i32) {
    let input = unsafe { abi::input(ptr, len) };
    abi::emit_str(&poll(input));
}

fn poll(input: &[u8]) -> String {
    let config = json::member(input, "config").unwrap_or(b"{}");
    let Some(uri) = json::member_str(config, "int:feed_blob") else {
        return abi::error_json(
            "the trigger's config names no \"int:feed_blob\" — this connector reads its items \
             from one filed blob, by address",
        );
    };
    let mut req = Obj::new();
    req.string("uri", &uri);
    let feed = match abi::brokered_blob(req.finish().as_bytes()) {
        Ok(b) => b,
        Err(e) => return abi::error_json(e),
    };
    // A failed read comes back as `{"error": …}` rather than blob bytes; the
    // host tells the two apart by HTTP status, and so must we before treating
    // the payload as a feed.
    if let Some(err) = json::member(&feed, "error") {
        return abi::error_json(core::str::from_utf8(err).unwrap_or("blob read failed"));
    }

    let cursor = json::member_str(input, "cursor");
    let max = json::member(input, "max_items")
        .and_then(|v| core::str::from_utf8(v).ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(50)
        .max(1);

    let items = split_array(&feed);
    // Everything after the cursor. An unknown cursor replays from the start
    // rather than emitting nothing: a poll that silently returns no items is
    // indistinguishable from a healthy source, which is the failure this whole
    // tier exists to make loud.
    let start = match &cursor {
        None => 0,
        Some(c) => items
            .iter()
            .position(|it| json::member_str(it, "id").as_deref() == Some(c.as_str()))
            .map(|i| i + 1)
            .unwrap_or(0),
    };
    let remaining = &items[start.min(items.len())..];
    let page = &remaining[..remaining.len().min(max)];

    let mut out = String::from("{\"items\":[");
    let mut last: Option<String> = None;
    for (i, it) in page.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let id = json::member_str(it, "id").unwrap_or_default();
        let mut item = Obj::new();
        item.string("id", &id).raw("payload", it);
        out.push_str(&item.finish());
        last = Some(id);
    }
    out.push(']');
    // Absent cursor means "leave it alone" — the correct reading of an empty
    // page, and the reason this is conditional rather than always written.
    if let Some(c) = last {
        out.push_str(",\"cursor\":\"");
        json::escape_str_into(&c, &mut out);
        out.push('"');
    }
    if page.len() < remaining.len() {
        out.push_str(",\"more\":true");
    }
    out.push('}');
    out
}

/// The top-level elements of a JSON array, as raw slices.
///
/// Sliced rather than parsed for the same reason everything else here is: an
/// item becomes the Event's payload, and a value that survived a re-encode is
/// not the value the feed recorded.
fn split_array(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() && b[i] != b'[' {
        i += 1;
    }
    if i == b.len() {
        return out;
    }
    i += 1;
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    let mut in_str = false;
    let mut escaped = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                if depth == 0 && start.is_none() {
                    start = Some(i);
                }
            }
            b'{' | b'[' => {
                if depth == 0 && start.is_none() {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    // The array's own closing bracket.
                    if let Some(s) = start.take() {
                        out.push(trim(&b[s..i]));
                    }
                    return out;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start.take() {
                        out.push(trim(&b[s..=i]));
                    }
                }
            }
            b',' if depth == 0 => {
                if let Some(s) = start.take() {
                    out.push(trim(&b[s..i]));
                }
            }
            b' ' | b'\t' | b'\n' | b'\r' => {}
            _ => {
                if depth == 0 && start.is_none() {
                    start = Some(i);
                }
            }
        }
        i += 1;
    }
    out
}

fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0usize;
    let mut e = b.len();
    while s < e && matches!(b[s], b' ' | b'\t' | b'\n' | b'\r') {
        s += 1;
    }
    while e > s && matches!(b[e - 1], b' ' | b'\t' | b'\n' | b'\r') {
        e -= 1;
    }
    &b[s..e]
}
