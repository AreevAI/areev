//! JSON-RPC 2.0 over one brokered POST — the half `mcp.call` and `a2a.call`
//! share.
//!
//! Both protocols are JSON-RPC over HTTP; they differ in the method name and
//! in how `params` is shaped. Writing them as two copies of one envelope
//! builder would have produced two places for a quoting bug to live, in the
//! two tools that carry the most protocol surface.

use alloc::string::String;

use crate::abi;
use crate::json::{self, Obj};

/// What a caller asked for, after slicing the tool's input.
pub struct Call<'a> {
    pub url: &'a str,
    pub method: &'a str,
    /// Already-JSON params: either sliced verbatim from the input, or built
    /// by the tool.
    pub params: String,
    /// The JSON-RPC `id`, raw. A caller may pin one for correlation; the
    /// default is `1`, because one request per invocation is the whole
    /// concurrency model here.
    pub id: &'a [u8],
    /// The credential the BROKER attaches. The guest names it and never sees
    /// its value.
    pub credential: Option<String>,
    /// Extra request headers, raw JSON object bytes, or `None`.
    pub headers: Option<&'a [u8]>,
}

/// Build the JSON-RPC envelope, POST it through the broker, and unwrap the
/// answer.
pub fn dispatch(call: &Call) -> String {
    let mut env = Obj::new();
    env.string("jsonrpc", "2.0")
        .raw("id", call.id)
        .string("method", call.method)
        .raw("params", call.params.as_bytes());
    let body = env.finish();

    let mut req = Obj::new();
    req.string("url", call.url).string("method", "POST");
    if let Some(c) = &call.credential {
        req.string("credential", c);
    }
    // `Content-Type` is the guest's to set and the pack's to declare: a
    // JSON-RPC endpoint that receives `text/plain` answers 415, and the
    // broker owns only the credential-bearing header names. A caller that
    // passes its own `headers` object takes over completely — it may need
    // `Accept`, a protocol version, or a tenant header, and merging two
    // header sets in a blessed blob would be a policy decision made in the
    // wrong place.
    match call.headers {
        Some(h) => {
            req.raw("headers", h);
        }
        None => {
            req.raw("headers", br#"{"Content-Type":"application/json"}"#);
        }
    }
    req.string_bytes("body", body.as_bytes());
    let request = req.finish();

    let reply = match abi::brokered_fetch(request.as_bytes()) {
        Ok(r) => r,
        Err(e) => return abi::error_json(e),
    };
    unwrap_reply(&reply)
}

/// Turn the broker's answer into the tool's result.
///
/// A refusal or a transport failure is returned **verbatim** — it is already
/// the one error shape every seam here uses, and rewriting it would cost the
/// `RUN-E022` code a reader needs. A 2xx carrying a JSON-RPC envelope is
/// unwrapped one level, so a caller reads `result` rather than a string
/// containing a document containing the answer.
fn unwrap_reply(reply: &[u8]) -> String {
    let status = json::member(reply, "status");
    let Some(status) = status else {
        // No status: this is the broker's `{"error": ..., "code": ...}`.
        return lossy(reply);
    };
    let body = json::member_str(reply, "body").unwrap_or_default();
    let mut out = Obj::new();
    out.raw("status", status);
    let b = body.as_bytes();
    if let Some(result) = json::member(b, "result") {
        out.raw("result", result);
    } else if let Some(err) = json::member(b, "error") {
        out.raw("error", err);
    } else {
        // Not a JSON-RPC envelope — an HTML error page from a proxy, say.
        // Hand the body back as a string rather than inventing structure.
        out.string_bytes("body", b);
    }
    out.finish()
}

/// The broker's own answer, passed through. Not UTF-8 is not a thing the
/// broker produces, so it becomes an error rather than a lossy guess.
fn lossy(bytes: &[u8]) -> String {
    match core::str::from_utf8(bytes) {
        Ok(s) => String::from(s),
        Err(_) => abi::error_json("the broker's answer was not UTF-8"),
    }
}

/// Slice the fields every JSON-RPC tool takes, so the two tools differ only
/// in their defaults.
pub struct Common<'a> {
    pub url: String,
    pub credential: Option<String>,
    pub headers: Option<&'a [u8]>,
    pub id: &'a [u8],
    pub params: Option<&'a [u8]>,
    pub method: Option<String>,
}

pub fn common(input: &[u8]) -> Result<Common<'_>, &'static str> {
    let url = json::member_str(input, "url").ok_or("input names no \"url\"")?;
    Ok(Common {
        url,
        credential: json::member_str(input, "credential"),
        headers: json::member(input, "headers").filter(|h| h.starts_with(b"{")),
        id: match json::member(input, "id") {
            Some(v) if !v.is_empty() && v != b"null" => v,
            _ => b"1",
        },
        params: json::member(input, "params").filter(|p| p.starts_with(b"{")),
        method: json::member_str(input, "method"),
    })
}

/// The bytes of a raw value, for a tool assembling `params` from parts.
pub fn raw_or<'a>(input: &'a [u8], key: &str, default: &'a [u8]) -> &'a [u8] {
    match json::member(input, key) {
        Some(v) if !v.is_empty() && v != b"null" => v,
        _ => default,
    }
}

