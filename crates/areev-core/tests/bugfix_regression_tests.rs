//! Regression tests for serialization bugs found in the 2026-07-22 hunt.
use areev_core::{deserialize_blob, serialize_grain, Fact, Goal, Grain, State};

// #1 — a non-finite confidence is rejected on WRITE (was: serialized fine but
// unreadable, leaving an immutable grain stuck in the store forever).
#[test]
fn non_finite_confidence_rejected_on_write() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let f = Fact::new("a", "b", "c").confidence(bad).created_at(1_700_000_000_000);
        assert!(serialize_grain(&f).is_err(), "serialize must reject confidence={bad}");
    }
    // finite still works and round-trips
    let ok = Fact::new("a", "b", "c").confidence(0.5).created_at(1_700_000_000_000);
    let (blob, _h) = serialize_grain(&ok).unwrap();
    assert!(deserialize_blob(&blob).is_ok());
}

// #2 — trailing bytes after the payload are rejected (content-address canonicality).
#[test]
fn trailing_bytes_rejected() {
    let f = Fact::new("a", "b", "c").created_at(1_700_000_000_000);
    let (mut blob, _h) = serialize_grain(&f).unwrap();
    // canonical blob reads fine
    assert!(deserialize_blob(&blob).is_ok());
    blob.extend_from_slice(&[0xff, 0xff]);
    assert!(deserialize_blob(&blob).is_err(), "padded (non-canonical) blob must be rejected");
}

// #3 — nested user-JSON keys colliding with OMS short codes survive verbatim.
#[test]
fn state_context_short_keys_preserved() {
    let s = State::new(serde_json::json!({
        "o": "hello", "desc": "boot", "s": 1, "normal_key": true
    }))
    .created_at(1_700_000_000_000);
    let (blob, h1) = serialize_grain(&s).unwrap();
    let dg = deserialize_blob(&blob).unwrap();
    let ctx = dg.fields.get("context").unwrap().as_object().unwrap();
    for k in ["o", "desc", "s", "normal_key"] {
        assert!(ctx.contains_key(k), "context key {k:?} was rewritten; got {:?}", ctx.keys().collect::<Vec<_>>());
    }
    assert!(!ctx.contains_key("object") && !ctx.contains_key("description"));
    // round-trip is content-address stable now
    let dg2 = deserialize_blob(&blob).unwrap();
    let s2 = State::new(dg2.fields.get("context").unwrap().clone()).created_at(1_700_000_000_000);
    assert_eq!(serialize_grain(&s2).unwrap().1, h1, "re-serialize must reproduce the address");
}

// #3 — int:* profile keys inside context still round-trip (restricted expansion).
#[test]
fn context_int_profile_keys_still_expand() {
    let s = State::new(serde_json::json!({ "int:base_url": "https://x", "user_o": "keep" }))
        .created_at(1_700_000_000_000);
    let (blob, _h) = serialize_grain(&s).unwrap();
    let dg = deserialize_blob(&blob).unwrap();
    let ctx = dg.fields.get("context").unwrap().as_object().unwrap();
    assert!(ctx.contains_key("int:base_url"), "int:* profile key must round-trip: {:?}", ctx.keys().collect::<Vec<_>>());
}

// #12 — map keys are NFC-normalized (composition variants collapse to one hash).
#[test]
fn extra_field_keys_nfc_normalized() {
    let mk = |k: &str| {
        Fact::new("u", "likes", "x")
            .created_at(1_700_000_000_000)
            .extra_field(k, serde_json::json!(1))
    };
    let (_, h1) = serialize_grain(&mk("caf\u{00e9}")).unwrap(); // NFC é
    let (_, h2) = serialize_grain(&mk("cafe\u{0301}")).unwrap(); // NFD e + combining
    assert_eq!(h1, h2, "NFC and NFD key variants must produce one content address");
}

// #13 — a u64 above i64::MAX round-trips as an integer (was lossy f64).
#[test]
fn large_u64_extra_field_preserved() {
    let g = Goal::new("x")
        .created_at(1_700_000_000_000)
        .extra_field("id", serde_json::json!(18_446_744_073_709_551_615u64));
    let (blob, _h) = serialize_grain(&g).unwrap();
    let dg = deserialize_blob(&blob).unwrap();
    let v = dg.fields.get("id").unwrap();
    assert!(v.is_u64() || v.is_i64(), "large u64 became {v:?} (precision lost)");
    assert_eq!(v.as_u64(), Some(18_446_744_073_709_551_615));
}
