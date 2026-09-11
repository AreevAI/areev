//! `rest.poll` — a paginated REST connector as a **declaration** rather than
//! a crate (#231).
//!
//! `http.call` made this argument one level down and won it: a blob that
//! forwards a request has no opinion to get wrong, so the Definition's
//! `capabilities` block *is* the policy. A poll is one request plus three
//! decisions — where the items are, where the cursor is, and how the next
//! page is asked for — and in code those three are invisible to the pack, to
//! `areev tool provenance`, and to the loop's `code_revision` gate. Written
//! as JSON pointers in the Definition's `config`, they are data a reviewer
//! reads. Five providers become five declarations naming one blob, instead of
//! five crates each carrying the same three decisions and the same four
//! cursor rules to get wrong.
//!
//! ## The Definition
//!
//! ```json
//! { "tool_name": "gmail_poll", "kind": "definition",
//!   "executor_uri": "cas://sha256:<rest.poll>",
//!   "runtime": "wasm32-areev-io",
//!   "capabilities": [ { "http": { "hosts": ["https://gmail.googleapis.com"],
//!                                 "methods": ["GET"],
//!                                 "path_prefixes": ["/gmail/v1/users/me/"],
//!                                 "credentials": ["gmail"] } } ],
//!   "config": {
//!     "url": "https://gmail.googleapis.com/gmail/v1/users/me/messages",
//!     "query": { "q": "newer_than:1d" },
//!     "credential": "gmail",
//!     "page_size": "maxResults",
//!     "items": "/messages",
//!     "id": "/id",
//!     "order": "newest_first",
//!     "next_page": { "token_at": "/nextPageToken", "as_query": "pageToken" }
//!   } }
//! ```
//!
//! Reach is still bounded by the declaration, not by this file: a `url` the
//! `capabilities` block does not admit is refused by the broker, and the
//! refusal is journaled. This tool cannot widen anything it was not given.
//!
//! ## `config`
//!
//! | key | meaning |
//! |---|---|
//! | `url` | **required** — the endpoint to poll |
//! | `method` | default `GET` |
//! | `credential` | the credential NAME the broker spends; the tool never sees a value |
//! | `headers` | non-credential request headers, as an object |
//! | `query` | static query parameters, as an object of scalars |
//! | `body` | a request body, for a source that polls by POST |
//! | `page_size` | query parameter the request's `max_items` rides on |
//! | `cursor_param` | query parameter the stored watermark rides on |
//! | `items` | JSON pointer to the array of items in the response (`""` = the body IS the array) |
//! | `id` | pointer WITHIN an item to its id (default `/id`) |
//! | `cursor_from` | pointer into the RESPONSE to the new watermark; `-` selects an array's last element. Default: the item at the watermark end of the page |
//! | `order` | `oldest_first` (default) or `newest_first` — which end of a page is the newest item |
//! | `next_page` | `{ "token_at": <pointer>, "as_query": <parameter> }` |
//!
//! ## The four cursor rules, written once
//!
//! These are the ones every connector implementer gets wrong, which is the
//! argument for having one reviewed copy:
//!
//! 1. **An absent cursor means "leave it where it is", never a rewind.** A
//!    poll that found nothing emits no `cursor` key at all — not `null`, which
//!    the evaluator would have to guess about.
//! 2. **The watermark advances over everything LOOKED AT, not everything
//!    delivered.** This tool emits every item on the page it fetched, so the
//!    two coincide by construction — and that is deliberate: `max_items` is
//!    expressed to the source as a page size, never used to slice a page
//!    afterwards. Slicing a page you cannot resume mid-way is exactly how a
//!    poller either loses the tail or re-reads the head forever.
//! 3. **The first poll seeds.** Nothing to do here — the evaluator keeps the
//!    cursor and fires nothing (`docs/triggers.md`). What this tool must do is
//!    return a usable watermark on that poll, or the seed is empty and the
//!    next poll seeds again.
//! 4. **`more: true` drains without hammering.** A page token means the next
//!    invocation runs immediately instead of waiting out the interval. The
//!    token rides in the cursor, because the cursor is the only state a
//!    connector has between invocations.
//!
//! ## The cursor
//!
//! Opaque and connector-defined, as the contract says — but written to be
//! read by a person looking at `trigger status`:
//!
//! * `"msg-1802611"` — an ordinary watermark, and nothing else in flight.
//! * `{"w":"msg-1802611","p":"CAUQAA"}` — mid-drain: the watermark decided
//!   for this drain, plus the page token to ask for next.
//!
//! A cursor set by hand to a bare string is therefore always valid. While a
//! drain is in flight the watermark moves with each page under
//! `oldest_first` (a crash leaves it at the last page actually delivered) and
//! is held from the first page under `newest_first` (where the first page
//! holds the newest item, so a crash re-reads rather than skips). Both rules
//! are "never lose an item"; neither can rewind the source.
#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};

use areev_tool_common::abi;
use areev_tool_common::json::{self, Obj};

mod ptr;

areev_tool_common::guest_abi!();

/// # Safety
/// The sandbox calls this with the pointer its own `alloc` returned.
#[no_mangle]
pub unsafe extern "C" fn run(ptr: i32, len: i32) {
    let input = unsafe { abi::input(ptr, len) };
    abi::emit_str(&poll(input));
}

/// What the stored cursor carried in: a watermark, a page token, or both.
#[derive(Default)]
struct Resume {
    watermark: Option<String>,
    page: Option<String>,
}

impl Resume {
    /// A bare string is a watermark; an object is a drain in flight. Parsed
    /// leniently on purpose — an operator may set a cursor by hand, and the
    /// bare form is the one they will write.
    fn parse(raw: Option<String>) -> Resume {
        let Some(c) = raw else { return Resume::default() };
        if !c.starts_with('{') {
            return Resume { watermark: Some(c), page: None };
        }
        let b = c.as_bytes();
        Resume { watermark: json::member_str(b, "w"), page: json::member_str(b, "p") }
    }
}

fn poll(input: &[u8]) -> String {
    let config = json::member(input, "config").unwrap_or(b"{}");
    let Some(url) = json::member_str(config, "url") else {
        return abi::error_json(
            "this connector's config names no \"url\" — a rest.poll Definition declares where \
             to poll, and the capabilities block declares where it may reach",
        );
    };
    let items_ptr = json::member_str(config, "items").unwrap_or_default();
    let id_ptr = json::member_str(config, "id").unwrap_or_else(|| "/id".to_string());
    let newest_first = json::member_str(config, "order").as_deref() == Some("newest_first");
    let max = ptr::scalar(json::member(input, "max_items"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(50)
        .max(1);
    let resume = Resume::parse(json::member_str(input, "cursor"));

    // ── the request ────────────────────────────────────────────────────
    let mut target = url;
    if let Some(q) = json::member(config, "query") {
        for (k, v) in ptr::members(q) {
            append_param(&mut target, &k, &ptr::scalar(Some(v)).unwrap_or_default());
        }
    }
    if let Some(p) = json::member_str(config, "page_size") {
        append_param(&mut target, &p, &max.to_string());
    }
    // The watermark and the page token ride TOGETHER while draining: the token
    // continues the same filtered query, so dropping the filter mid-drain
    // would widen it.
    if let (Some(p), Some(w)) = (json::member_str(config, "cursor_param"), &resume.watermark) {
        append_param(&mut target, &p, w);
    }
    let next_page = json::member(config, "next_page").unwrap_or(b"{}");
    if let (Some(p), Some(t)) = (json::member_str(next_page, "as_query"), &resume.page) {
        append_param(&mut target, &p, t);
    }

    let mut req = Obj::new();
    req.string("url", &target);
    req.string("method", &json::member_str(config, "method").unwrap_or_else(|| "GET".to_string()));
    if let Some(c) = json::member_str(config, "credential") {
        req.string("credential", &c);
    }
    if let Some(h) = json::member(config, "headers") {
        req.raw("headers", h);
    }
    if let Some(b) = json::member(config, "body") {
        req.raw("body", b);
    }
    let reply = match abi::brokered_fetch(req.finish().as_bytes()) {
        Ok(b) => b,
        Err(e) => return abi::error_json(e),
    };

    // ── the answer ─────────────────────────────────────────────────────
    // A refusal is forwarded with its code, exactly as `http.call` forwards
    // it: the broker is the authority that decided, and dressing its answer up
    // would only add a second opinion. The evaluator fails the poll on any
    // `error`, so a refused destination never reads as a quiet source.
    if json::member(&reply, "error").is_some() {
        return String::from_utf8_lossy(&reply).into_owned();
    }
    let status = ptr::scalar(json::member(&reply, "status"))
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(0);
    let body = json::member_str(&reply, "body").unwrap_or_default();
    if !(200..300).contains(&status) {
        let mut detail = String::from("upstream answered ");
        detail.push_str(&status.to_string());
        detail.push_str(": ");
        detail.push_str(&clip(&body));
        return abi::error_json(&detail);
    }
    let doc = body.as_bytes();
    let Some(array) = ptr::pointer(doc, &items_ptr) else {
        let mut detail = String::from("the response has nothing at items pointer ");
        detail.push_str(if items_ptr.is_empty() { "\"\" (the whole body)" } else { &items_ptr });
        detail.push_str(" — it answered: ");
        detail.push_str(&clip(&body));
        return abi::error_json(&detail);
    };
    if array.first() != Some(&b'[') {
        let mut detail = String::from("items pointer ");
        detail.push_str(&items_ptr);
        detail.push_str(" names a value that is not an array");
        return abi::error_json(&detail);
    }
    let items = ptr::elements(array);

    // ── the page ───────────────────────────────────────────────────────
    let mut out = String::from("{\"items\":[");
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let mut entry = Obj::new();
        // An id is what the evaluator dedups on when the declaration names no
        // `dedup_key`. Absent is not fatal: a declared pointer wins anyway, and
        // refusing the whole page over one unidentified item would lose the
        // rest of it.
        if let Some(id) = ptr::scalar(ptr::pointer(it, &id_ptr)) {
            entry.string("id", &id);
        }
        entry.raw("payload", it);
        out.push_str(&entry.finish());
    }
    out.push(']');

    // ── the cursor ─────────────────────────────────────────────────────
    let token = json::member_str(next_page, "token_at")
        .and_then(|p| ptr::scalar(ptr::pointer(doc, &p)))
        .filter(|t| !t.is_empty() && t != "null");
    // Rule 2: over everything looked at. The whole page was emitted, so the
    // watermark comes from the page — its newest end — and not from what
    // survived any filtering downstream.
    let edge = if newest_first { items.first() } else { items.last() };
    let fresh = match json::member_str(config, "cursor_from") {
        Some(p) => ptr::scalar(ptr::pointer(doc, &p)),
        None => edge.and_then(|it| ptr::scalar(ptr::pointer(it, &id_ptr))),
    };
    // Mid-drain under `newest_first`, the newest item was on the FIRST page:
    // later pages are older, so they must not move the watermark backwards.
    // Under `oldest_first` each page's last item IS the newest thing seen, so
    // it advances every page and a crash resumes from the last page actually
    // delivered.
    let draining_older = newest_first && resume.page.is_some();
    let advanced = if draining_older { None } else { fresh };

    match (&token, &advanced) {
        // Still draining: both halves must survive to the next invocation, and
        // only the compound form can carry them.
        (Some(t), w) => {
            let mut c = Obj::new();
            if let Some(w) = w.as_ref().or(resume.watermark.as_ref()) {
                c.string("w", w);
            }
            c.string("p", t);
            out.push_str(",\"cursor\":\"");
            json::escape_str_into(&c.finish(), &mut out);
            out.push_str("\",\"more\":true");
        }
        // The drain ended here. The cursor collapses back to the bare
        // watermark — the form an operator can read, and set.
        (None, Some(w)) => {
            out.push_str(",\"cursor\":\"");
            json::escape_str_into(w, &mut out);
            out.push('"');
        }
        // Nothing new on this page. Two situations that must not be conflated,
        // and only one of them writes anything:
        //
        // * A drain that ended on an empty page — the stored cursor still
        //   carries the exhausted page token, so leaving it alone would re-ask
        //   for that same page forever. Write the watermark back WITHOUT the
        //   token; the only case where restating an unchanged watermark is
        //   doing something.
        // * Otherwise rule 1: no cursor key at all, which is "leave it where it
        //   is". `null` would rewind the source — for a mailbox, re-processing
        //   everything it holds.
        (None, None) => {
            if let (Some(_), Some(w)) = (&resume.page, &resume.watermark) {
                out.push_str(",\"cursor\":\"");
                json::escape_str_into(w, &mut out);
                out.push('"');
            }
        }
    }
    out.push('}');
    out
}

/// Append `k=v` to a URL, percent-encoding everything that is not
/// unreserved. Conservative on purpose: a value that arrived from a
/// Definition is data, and a value that could carry a `&` could rewrite the
/// query it was placed in.
fn append_param(url: &mut String, k: &str, v: &str) {
    url.push(if url.contains('?') { '&' } else { '?' });
    percent_into(k, url);
    url.push('=');
    percent_into(v, url);
}

fn percent_into(s: &str, out: &mut String) {
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => {
                out.push('%');
                out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0').to_ascii_uppercase());
                out.push(char::from_digit(u32::from(b & 0xf), 16).unwrap_or('0').to_ascii_uppercase());
            }
        }
    }
}

/// Enough of a body to diagnose with, and not a whole page in an error
/// message an operator reads in a heartbeat log.
fn clip(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        return String::from(s);
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::from(&s[..end]);
    out.push('…');
    out
}
