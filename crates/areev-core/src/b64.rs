//! Standard base64 (RFC 4648) decoding — hand-rolled to keep the workspace
//! dependency-free (root CLAUDE.md policy). One implementation, two
//! callers: `areev-server` (HTTP Basic credentials) and `areev-trigger`
//! (connector blob payloads, #93). Encoding is deliberately absent — the
//! engine only ever *receives* base64, and a second direction would invite
//! a second alphabet.

/// Decode standard base64 (RFC 4648, `+`/`/` alphabet, padding optional).
/// Returns `None` on any invalid character or truncated input — callers
/// refuse loudly rather than salvage.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn decodes_rfc4648_vectors() {
        assert_eq!(decode("").unwrap(), b"");
        assert_eq!(decode("Zg==").unwrap(), b"f");
        assert_eq!(decode("Zm8=").unwrap(), b"fo");
        assert_eq!(decode("Zm9v").unwrap(), b"foo");
        assert_eq!(decode("Zm9vYmFy").unwrap(), b"foobar");
        // Padding is optional.
        assert_eq!(decode("Zm9vYg").unwrap(), b"foob");
    }

    #[test]
    fn refuses_invalid_input() {
        assert!(decode("not base64!").is_none());
        assert!(decode("Zm9v\u{202e}").is_none());
        // URL-safe alphabet is a different encoding, not this one.
        assert!(decode("a-b_").is_none());
    }

    #[test]
    fn binary_roundtrip_shape() {
        // 0x00 0xFF 0x10 — exercises the bit accumulator across byte edges.
        assert_eq!(decode("AP8Q").unwrap(), vec![0x00, 0xFF, 0x10]);
    }
}
