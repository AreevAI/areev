//! A ~100-line HTTP/1.1 POST client for one destination: the engine's
//! credential broker, on loopback.
//!
//! ## Why this is hand-rolled and not a dependency
//!
//! This package's whole justification is its dependency tree. `Cargo.toml`
//! says it: five required dependencies, `no_std`-capable, and wasmi chosen over
//! wasmtime (50 direct deps, a higher MSRV) precisely so the part you *trust to
//! hold a line* is small enough to read. Pulling in an HTTP client to reach
//! `127.0.0.1` would trade that away for nothing.
//!
//! And the requirement really is that small. The destination is always
//! loopback, so **there is no TLS** — no certificates, no roots, no
//! negotiation. It is one request per connection, `Connection: close`, a
//! JSON body of known length, and a JSON response of known length. That is a
//! std-only `TcpStream` and about eighty lines, which is the same trade
//! `areev-server` makes for the console.
//!
//! Deliberately absent: connection reuse, chunked transfer-encoding, redirects
//! (the broker never issues one — following redirects is *its* job, and #99 is
//! about how carefully), and compression.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// How long one brokered call may take end to end. Generous — the broker's own
/// upstream ceilings are 15s connect + 60s response — and finite, because a
/// sandbox parked forever on a socket is a pool worker parked forever.
const TIMEOUT: Duration = Duration::from_secs(90);

/// Post `body` to `url` with the broker's capability token.
///
/// Returns the response body verbatim. The caller does not interpret it: the
/// guest's request shape and the broker's response shape are the SAME contract
/// on both sides of this hop, so this binary forwards rather than translates —
/// there is no place here for a translation bug, and the guest ABI stays
/// exactly the broker ABI.
pub fn post_json(url: &str, token: &str, body: &[u8], max_response_bytes: usize) -> Result<Vec<u8>, String> {
    let addr = authority(url)?;
    let stream = TcpStream::connect(&addr).map_err(|e| format!("connecting to the broker at {addr}: {e}"))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    // The token is a header value; a newline in one would let it author its
    // own headers. It comes from our own environment rather than from the
    // guest, so this is a belt not a brace — but a broker token that can forge
    // a request line is worth one `if`.
    if token.chars().any(|c| c.is_control()) {
        return Err("broker token contains a control character".into());
    }

    let head = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nX-Areev-Egress-Token: {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = stream;
    stream.write_all(head.as_bytes()).map_err(|e| format!("writing to the broker: {e}"))?;
    stream.write_all(body).map_err(|e| format!("writing to the broker: {e}"))?;
    stream.flush().map_err(|e| format!("writing to the broker: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("reading the broker's response: {e}"))?;
    if !line.starts_with("HTTP/1.") {
        return Err("the broker did not answer with HTTP".into());
    }

    let mut content_length: Option<usize> = None;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h).map_err(|e| format!("reading the broker's response: {e}"))?;
        if n == 0 || h.trim().is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().ok();
        }
    }

    // The cap is applied to the DECLARED length before allocating, then again
    // to what actually arrives — a `Content-Length` is a claim, and believing
    // it is how a cap gets applied too late.
    match content_length {
        Some(len) => {
            if len > max_response_bytes {
                return Err(format!(
                    "the broker's response of {len} bytes exceeds the {max_response_bytes}-byte \
                     ceiling — refused rather than truncated"
                ));
            }
            let mut buf = vec![0u8; len];
            reader
                .read_exact(&mut buf)
                .map_err(|e| format!("reading the broker's response body: {e}"))?;
            Ok(buf)
        }
        None => {
            // The broker always sends a Content-Length; read to EOF under the
            // cap rather than assume, and refuse an overrun instead of
            // silently keeping a prefix.
            let mut buf = Vec::new();
            let mut limited = reader.take(max_response_bytes as u64 + 1);
            limited
                .read_to_end(&mut buf)
                .map_err(|e| format!("reading the broker's response body: {e}"))?;
            if buf.len() > max_response_bytes {
                return Err(format!(
                    "the broker's response exceeds the {max_response_bytes}-byte ceiling — \
                     refused rather than truncated"
                ));
            }
            Ok(buf)
        }
    }
}

/// `http://127.0.0.1:1234` -> `127.0.0.1:1234`.
///
/// Refuses anything that is not loopback http. The broker binds
/// `127.0.0.1:0` and nothing else ever appears here, so an `AREEV_EGRESS_URL`
/// pointing somewhere else means the environment has been tampered with —
/// and this process holds a capability token worth stealing.
fn authority(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("AREEV_EGRESS_URL {url:?} is not an http:// URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("").trim_end_matches('/');
    let host = authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority);
    if host != "127.0.0.1" && host != "localhost" && host != "[::1]" {
        return Err(format!(
            "AREEV_EGRESS_URL {url:?} is not loopback — the broker is always local, and this \
             process holds a capability token"
        ));
    }
    Ok(authority.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_authorities_parse() {
        assert_eq!(authority("http://127.0.0.1:8080").unwrap(), "127.0.0.1:8080");
        assert_eq!(authority("http://127.0.0.1:8080/").unwrap(), "127.0.0.1:8080");
        assert_eq!(authority("http://localhost:1").unwrap(), "localhost:1");
    }

    #[test]
    fn a_non_loopback_broker_url_is_refused() {
        // The token this process holds spends real credentials; it does not
        // get posted off-box because a variable said so.
        assert!(authority("http://169.254.169.254/").is_err());
        assert!(authority("http://evil.example:80/").is_err());
        assert!(authority("https://127.0.0.1:8080").is_err());
        assert!(authority("127.0.0.1:8080").is_err());
    }
}
