//! Server-Sent Events, just enough to find one JSON-RPC response in a reply
//! (#395).
//!
//! MCP's "Streamable HTTP" transport lets a server answer a POST with
//! `Content-Type: text/event-stream` instead of a JSON body: a stream of
//! events whose `data` fields carry JSON-RPC messages — possibly
//! notifications or server requests first, the response last. The broker
//! reads the whole bounded body before answering (one request, one answer),
//! so what arrives here is the complete stream, and the job is to pick out
//! the event that IS the response.
//!
//! Parsing follows the WHATWG event-stream rules that matter for that:
//! lines end in LF, CRLF or CR; a line starting with `:` is a comment; a
//! `data` field (one optional space after the colon stripped) appends its
//! value plus a newline to the event's data, so a JSON document split over
//! several `data:` lines is reassembled; a blank line dispatches the event
//! with its final newline removed. Other fields (`event`, `id`, `retry`) are
//! ignored — the JSON-RPC `id` inside the data is the correlation that
//! counts, not the SSE `id`. A final event the server did not terminate with
//! a blank line is still dispatched: the body is complete when it gets here,
//! and dropping the last message over a missing newline would turn a working
//! server into a silent failure.

use alloc::string::String;
use alloc::vec::Vec;

/// The `data` of every event in `body`, in order. Events with no `data`
/// field are skipped.
pub fn events(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut data = String::new();
    let mut has_data = false;
    let mut i = 0usize;
    // A leading UTF-8 BOM belongs to the format, not to the first line.
    if body.starts_with(b"\xEF\xBB\xBF") {
        i = 3;
    }
    loop {
        let start = i;
        while i < body.len() && body[i] != b'\n' && body[i] != b'\r' {
            i += 1;
        }
        let line = &body[start..i];
        let at_end = i >= body.len();
        // Consume the terminator: CRLF counts once.
        if !at_end {
            if body[i] == b'\r' && body.get(i + 1) == Some(&b'\n') {
                i += 2;
            } else {
                i += 1;
            }
        }
        if line.is_empty() {
            if has_data {
                dispatch(&mut out, &mut data);
                has_data = false;
            }
        } else if line[0] != b':' {
            let (field, value) = match line.iter().position(|&b| b == b':') {
                Some(c) => {
                    let v = &line[c + 1..];
                    (&line[..c], v.strip_prefix(b" ").unwrap_or(v))
                }
                None => (line, &b""[..]),
            };
            if field == b"data" {
                has_data = true;
                data.push_str(core::str::from_utf8(value).unwrap_or("\u{FFFD}"));
                data.push('\n');
            }
        }
        if at_end {
            break;
        }
    }
    if has_data {
        dispatch(&mut out, &mut data);
    }
    out
}

fn dispatch(out: &mut Vec<String>, data: &mut String) {
    if data.ends_with('\n') {
        data.pop();
    }
    out.push(core::mem::take(data));
}
