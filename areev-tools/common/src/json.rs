//! A JSON *slicer*: find a member of a top-level object, and hand back the
//! raw bytes of its value.
//!
//! Slicing rather than parsing is the whole point. A caller's `arguments`
//! object travels into the JSON-RPC envelope byte for byte, so nothing here
//! can reorder its keys, renormalize its numbers, or lose a character the
//! upstream cared about. What we build ourselves — the envelope, the request —
//! we build with the writer below.

use alloc::string::String;

/// The raw bytes of `key`'s value in a top-level JSON object, or `None` when
/// the input is not an object, the key is absent, or the object is malformed
/// from that point on. Trailing/leading whitespace is trimmed off the value.
pub fn member<'a>(obj: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let mut i = skip_ws(obj, 0);
    if obj.get(i)? != &b'{' {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(obj, i);
        match obj.get(i)? {
            b'}' => return None,
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {}
            // Not a key where a key must be: the object is malformed, and the
            // honest answer is "no such member" rather than a guess.
            _ => return None,
        }
        let (name, next) = string_at(obj, i)?;
        i = skip_ws(obj, next);
        if obj.get(i)? != &b':' {
            return None;
        }
        i = skip_ws(obj, i + 1);
        let start = i;
        let end = skip_value(obj, i)?;
        if name == key {
            return Some(&obj[start..end]);
        }
        i = end;
    }
}

/// A member whose value is a JSON string, unescaped. `None` when absent or
/// not a string — a caller that must distinguish "absent" from "not a string"
/// can slice with [`member`] and look at the first byte.
pub fn member_str(obj: &[u8], key: &str) -> Option<String> {
    let raw = member(obj, key)?;
    if raw.first()? != &b'"' {
        return None;
    }
    string_at(raw, 0).map(|(s, _)| s)
}

/// Is this raw value JSON `null` (or nothing at all)?
pub fn is_null(raw: Option<&[u8]>) -> bool {
    match raw {
        None => true,
        Some(v) => v == b"null",
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Read the JSON string starting at `b[i] == '"'`, returning it unescaped and
/// the index just past its closing quote.
fn string_at(b: &[u8], i: usize) -> Option<(String, usize)> {
    if b.get(i)? != &b'"' {
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
                        let hi = hex4(b, i + 1)?;
                        i += 4;
                        // A surrogate pair is two escapes; a lone surrogate is
                        // replaced rather than refused, because a tool that
                        // dropped a whole response over one bad character
                        // would be worse than one that reports what came back.
                        let ch = if (0xD800..0xDC00).contains(&hi) {
                            if b.get(i + 1) == Some(&b'\\') && b.get(i + 2) == Some(&b'u') {
                                let lo = hex4(b, i + 3)?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    i += 6;
                                    let c = 0x10000
                                        + ((u32::from(hi) - 0xD800) << 10)
                                        + (u32::from(lo) - 0xDC00);
                                    char::from_u32(c).unwrap_or('\u{FFFD}')
                                } else {
                                    '\u{FFFD}'
                                }
                            } else {
                                '\u{FFFD}'
                            }
                        } else {
                            char::from_u32(u32::from(hi)).unwrap_or('\u{FFFD}')
                        };
                        out.push(ch);
                    }
                    other => out.push(other as char),
                }
                i += 1;
            }
            _ => {
                // Copy the whole UTF-8 sequence: pushing byte-by-byte as
                // `char` would mojibake every non-ASCII character.
                let start = i;
                while i < b.len() && b[i] != b'"' && b[i] != b'\\' {
                    i += 1;
                }
                out.push_str(core::str::from_utf8(&b[start..i]).ok()?);
            }
        }
    }
}

fn hex4(b: &[u8], i: usize) -> Option<u16> {
    let mut v: u16 = 0;
    for k in 0..4 {
        let d = (*b.get(i + k)? as char).to_digit(16)? as u16;
        v = (v << 4) | d;
    }
    Some(v)
}

/// The index just past the JSON value starting at `i`.
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => string_at(b, i).map(|(_, n)| n),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = i;
            loop {
                match *b.get(i)? {
                    b'"' => {
                        i = string_at(b, i)?.1;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        _ => {
            let mut i = i;
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                i += 1;
            }
            Some(i)
        }
    }
}

/// Escape `bytes` as the CONTENT of a JSON string (no surrounding quotes).
///
/// Whole runs are copied at once, so a multi-byte UTF-8 sequence travels
/// intact — every byte that needs escaping is ASCII, so a run boundary can
/// never fall inside one. Non-ASCII is NOT re-encoded as `\u`: it is already
/// valid JSON, and escaping it would only make the result bigger and the
/// review harder.
pub fn escape_into(bytes: &[u8], out: &mut String) {
    let mut start = 0usize;
    for (i, &c) in bytes.iter().enumerate() {
        let repl: &str = match c {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            0x08 => "\\b",
            0x0c => "\\f",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x00..=0x1f => {
                copy_run(bytes, start, i, out);
                out.push_str("\\u00");
                out.push(char::from_digit(u32::from(c >> 4), 16).unwrap_or('0'));
                out.push(char::from_digit(u32::from(c & 0xf), 16).unwrap_or('0'));
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        copy_run(bytes, start, i, out);
        out.push_str(repl);
        start = i + 1;
    }
    copy_run(bytes, start, bytes.len(), out);
}

/// Append `b[s..e]`, replacing the run wholesale if it is not valid UTF-8 —
/// a tool that dropped a whole response over one bad byte would be worse than
/// one that reports what came back.
fn copy_run(b: &[u8], s: usize, e: usize, out: &mut String) {
    if s >= e {
        return;
    }
    match core::str::from_utf8(&b[s..e]) {
        Ok(t) => out.push_str(t),
        Err(_) => out.push('\u{FFFD}'),
    }
}

/// The same, for a `&str` the tool already holds.
pub fn escape_str_into(s: &str, out: &mut String) {
    escape_into(s.as_bytes(), out)
}

/// A JSON object under construction, written key by key.
///
/// Small on purpose: it knows how to put a raw value (a slice taken from the
/// input, which is already JSON) and a string value (escaped) into an object,
/// which is every shape a blessed tool builds.
pub struct Obj {
    buf: String,
    empty: bool,
}

impl Default for Obj {
    fn default() -> Self {
        Self::new()
    }
}

impl Obj {
    pub fn new() -> Obj {
        Obj { buf: String::from("{"), empty: true }
    }

    fn key(&mut self, k: &str) {
        if !self.empty {
            self.buf.push(',');
        }
        self.empty = false;
        self.buf.push('"');
        escape_str_into(k, &mut self.buf);
        self.buf.push_str("\":");
    }

    /// A value that is already JSON — a slice of the caller's own input.
    pub fn raw(&mut self, k: &str, v: &[u8]) -> &mut Self {
        self.key(k);
        self.buf.push_str(core::str::from_utf8(v).unwrap_or("null"));
        self
    }

    /// A string value, escaped.
    pub fn string(&mut self, k: &str, v: &str) -> &mut Self {
        self.key(k);
        self.buf.push('"');
        escape_str_into(v, &mut self.buf);
        self.buf.push('"');
        self
    }

    /// A string value whose content is itself a JSON document — escaped into
    /// the string, which is how a request body rides inside a request.
    pub fn string_bytes(&mut self, k: &str, v: &[u8]) -> &mut Self {
        self.key(k);
        self.buf.push('"');
        escape_into(v, &mut self.buf);
        self.buf.push('"');
        self
    }

    pub fn finish(&mut self) -> String {
        let mut out = core::mem::take(&mut self.buf);
        out.push('}');
        out
    }
}

/// Bytes of a value, or `null` when it is absent.
pub fn raw_or_null<'a>(obj: &'a [u8], key: &str) -> &'a [u8] {
    match member(obj, key) {
        Some(v) if !v.is_empty() => v,
        _ => b"null",
    }
}
