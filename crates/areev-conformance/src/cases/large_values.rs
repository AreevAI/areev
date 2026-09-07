//! Dictionary values are not bounded by the index that keys them (#160).
//! Every subject, relation and object string is interned, so a value of any
//! size or entropy must store, dedupe, supersede, replicate and erase
//! identically on both backends. The Postgres backend once keyed `terms` by
//! the text itself, and a btree entry caps at ~2704 bytes after pglz
//! compression — a 2.7 KB base64 payload failed while 8 KB of `x` passed,
//! and `loop run`'s own recommendation ledger crossed the line at roughly
//! eight recommendations.

use crate::{fact, fact_at, Backend};

/// `len` characters over the base64 alphabet from a fixed xorshift seed:
/// deterministic, and at six bits of entropy per character it does not
/// compress — the shape that tripped the old text-keyed index.
pub fn incompressible(len: usize) -> String {
    const ALPHABET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ALPHABET[((x >> 32) % 64) as usize] as char
        })
        .collect()
}

/// A 20 KB JSON object shaped like the loop's persisted ledger
/// (`{"applied":{…},"audit_heads":{…}}`), which is written as a Fact object.
pub fn ledger_shaped_json() -> String {
    let heads: Vec<String> = (0..200u64)
        .map(|i| format!(r#""{:064x}":"{:064x}""#, i, i.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
        .collect();
    format!(r#"{{"applied":{{}},"audit_heads":{{{}}}}}"#, heads.join(","))
}

pub fn incompressible_values_of_any_size_are_stored(b: &dyn Backend) {
    let mut m = b.open_named("large_a");
    let big = incompressible(6000);
    let first = fact_at("ns", "s", "payload", &big, 1_000);
    let h = m.add(&first).unwrap();
    assert_eq!(
        m.latest("ns", "s", "payload").unwrap().unwrap().get_str("object"),
        Some(big.as_str())
    );
    // Re-adding the same grain (pinned created_at, so the same address)
    // finds the interned term and is the usual no-op.
    assert_eq!(m.add(&first).unwrap(), h);

    // The ledger shape, as a supersession.
    let ledger = ledger_shaped_json();
    assert!(ledger.len() > 20_000, "{}", ledger.len());
    let mut v2 = fact("ns", "s", "payload", &ledger);
    m.supersede(&h, &mut v2).unwrap();
    assert_eq!(
        m.latest("ns", "s", "payload").unwrap().unwrap().get_str("object"),
        Some(ledger.as_str())
    );

    // The same string as a SUBJECT: one dictionary, one rule.
    let hs = m.add(&fact("ns", &big, "is", "a subject")).unwrap();
    assert_eq!(m.recall("ns", &big, Some("is"), 4).unwrap().len(), 1);

    // Replication carries both.
    let path = b.scratch().join("large.mgb");
    m.bundle_since(0, path.to_str().unwrap()).unwrap();
    let mut peer = b.open_named("large_b");
    peer.import_bundle(path.to_str().unwrap()).unwrap();
    assert_eq!(
        peer.latest("ns", "s", "payload").unwrap().unwrap().get_str("object"),
        Some(ledger.as_str())
    );
    assert_eq!(peer.recall("ns", &big, Some("is"), 4).unwrap().len(), 1);

    // And erasure reaches it.
    m.forget(&hs).unwrap();
    assert!(m.recall("ns", &big, Some("is"), 4).unwrap().is_empty());
}
