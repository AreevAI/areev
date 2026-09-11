//! JSON pointers, array elements, and object members — the slicing this tool
//! needs and `areev-tool-common` does not have.
//!
//! **Why it is here and not there.** A blessed blob's identity is its content
//! address, and adding even an unused function to the shared crate changes the
//! bytes of every other blob built from it — which would re-point every pack
//! that pinned `http.call`, `mcp.call`, `a2a.call` or `mailbox.poll`. So a new
//! tool's new helpers live in the new tool. When one of these is wanted by a
//! second blob, moving it into `common` is a deliberate re-blessing of all of
//! them, not a refactor.
//!
//! Slicing rather than parsing, like the rest of the tier: an item becomes an
//! Event's payload, and a value that survived a re-encode is not the value the
//! source recorded.

use alloc::string::String;
use alloc::vec::Vec;

use areev_tool_common::json;

/// Resolve a JSON pointer (RFC 6901) against `doc`, returning the raw bytes of
/// what it names.
///
/// `""` is the whole document; `/a/b` walks object members; `/0` indexes an
/// array; `~1` and `~0` are the escapes for `/` and `~`. One extension, and it
/// is the one a cursor mapping wants: **`-` selects an array's LAST element**.
/// RFC 6901 gives `-` no resolvable meaning (it is Patch's append position), so
/// nothing is overloaded — and "the id of the last message" becomes writable.
pub fn pointer<'a>(doc: &'a [u8], ptr: &str) -> Option<&'a [u8]> {
    if ptr.is_empty() {
        return Some(trim(doc));
    }
    if !ptr.starts_with('/') {
        return None;
    }
    let mut cur = trim(doc);
    for raw in ptr[1..].split('/') {
        let token = unescape(raw);
        cur = match cur.first()? {
            b'[' => {
                let items = elements(cur);
                if token == "-" {
                    *items.last()?
                } else {
                    *items.get(token.parse::<usize>().ok()?)?
                }
            }
            b'{' => json::member(cur, &token)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn unescape(t: &str) -> String {
    if !t.contains('~') {
        return String::from(t);
    }
    t.replace("~1", "/").replace("~0", "~")
}

/// A JSON scalar as the string an id or a cursor is made of: a string's own
/// content (unescaped), or a number/bool literal's text. `None` for an object,
/// an array, or nothing — none of those is an identity.
pub fn scalar(raw: Option<&[u8]>) -> Option<String> {
    let v = raw?;
    match v.first()? {
        b'"' => read_string(v, 0).map(|(s, _)| s),
        b'{' | b'[' => None,
        _ => core::str::from_utf8(v).ok().map(String::from),
    }
}

/// The top-level elements of a JSON array, as raw slices.
pub fn elements(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let Some(inner) = inside(b, b'[', b']') else { return out };
    let mut i = 0usize;
    while i < inner.len() {
        while i < inner.len() && matches!(inner[i], b' ' | b'\t' | b'\n' | b'\r' | b',') {
            i += 1;
        }
        if i >= inner.len() {
            break;
        }
        let start = i;
        let Some(end) = skip_value(inner, i) else { break };
        out.push(trim(&inner[start..end]));
        i = end;
    }
    out
}

/// The members of a JSON object, as (key, raw value) pairs, in the order the
/// object wrote them.
pub fn members(obj: &[u8]) -> Vec<(String, &[u8])> {
    let mut out = Vec::new();
    let Some(inner) = inside(obj, b'{', b'}') else { return out };
    let mut i = 0usize;
    while i < inner.len() {
        while i < inner.len() && matches!(inner[i], b' ' | b'\t' | b'\n' | b'\r' | b',') {
            i += 1;
        }
        if i >= inner.len() {
            break;
        }
        let Some((key, next)) = read_string(inner, i) else { break };
        i = skip_ws(inner, next);
        if inner.get(i) != Some(&b':') {
            break;
        }
        i = skip_ws(inner, i + 1);
        let start = i;
        let Some(end) = skip_value(inner, i) else { break };
        out.push((key, trim(&inner[start..end])));
        i = end;
    }
    out
}

/// What sits between the outermost `open`/`close` of a JSON container.
fn inside(b: &[u8], open: u8, close: u8) -> Option<&[u8]> {
    let s = skip_ws(b, 0);
    if *b.get(s)? != open {
        return None;
    }
    let end = skip_value(b, s)?;
    if b.get(end - 1) != Some(&close) || end - 1 <= s + 1 {
        return None;
    }
    Some(&b[s + 1..end - 1])
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
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

/// Read the JSON string at `b[i] == '"'`, unescaped, and the index past it.
///
/// Escapes are handled to the depth an identifier needs: the two-character
/// ones literally, and `\uXXXX` only for the BMP. A cursor or an id that
/// needed a surrogate pair would still round-trip as its escaped text, since
/// the pair reaches the output unchanged.
fn read_string(b: &[u8], i: usize) -> Option<(String, usize)> {
    if *b.get(i)? != b'"' {
        return None;
    }
    let mut out = String::new();
    let mut i = i + 1;
    loop {
        match *b.get(i)? {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                i += 1;
                match *b.get(i)? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let mut v: u32 = 0;
                        for k in 1..=4 {
                            v = (v << 4) | (*b.get(i + k)? as char).to_digit(16)?;
                        }
                        i += 4;
                        out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                    }
                    other => out.push(other as char),
                }
                i += 1;
            }
            _ => {
                // Copy the whole UTF-8 run: byte-by-byte as `char` mojibakes
                // everything non-ASCII.
                let start = i;
                while i < b.len() && b[i] != b'"' && b[i] != b'\\' {
                    i += 1;
                }
                out.push_str(core::str::from_utf8(&b[start..i]).ok()?);
            }
        }
    }
}

/// The index just past the JSON value starting at `i`.
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    let mut i = i;
    match *b.get(i)? {
        b'"' => read_string(b, i).map(|(_, n)| n),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut in_str = false;
            let mut escaped = false;
            loop {
                let c = *b.get(i)?;
                if in_str {
                    if escaped {
                        escaped = false;
                    } else if c == b'\\' {
                        escaped = true;
                    } else if c == b'"' {
                        in_str = false;
                    }
                } else {
                    match c {
                        b'"' => in_str = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(i + 1);
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
            }
        }
        _ => {
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                i += 1;
            }
            Some(i)
        }
    }
}
